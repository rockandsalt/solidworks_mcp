// SketchHelpers.cpp : see SketchHelpers.h.
#include "pch.h"
#include "SketchHelpers.h"
#include "Common.h"

namespace sld_mcp::service
{
	grpc::Status FindStandardPlane(IModelDoc2* doc, int index, ATL::CComPtr<IFeature>& plane)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		int planesSeen = 0;
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"RefPlane" && planesSeen++ == index)
			{
				plane = feature;
				return grpc::Status::OK;
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"couldn't find the part's default Front/Top/Right planes");
	}

	bool StandardPlaneIndex(sld_mcp::v1::Plane plane, int& index)
	{
		switch (plane)
		{
		case sld_mcp::v1::PLANE_FRONT: index = 0; return true;
		case sld_mcp::v1::PLANE_TOP:   index = 1; return true;
		case sld_mcp::v1::PLANE_RIGHT: index = 2; return true;
		default:                       return false;
		}
	}

	void FindOrigin(IModelDoc2* doc, ATL::CComPtr<ISketchPoint>& point, std::string& featureName)
	{
		ATL::CComPtr<IFeature> feature;
		if (FAILED(doc->IFirstFeature(&feature)))
			return;
		while (feature)
		{
			ATL::CComBSTR typeName;
			if (SUCCEEDED(feature->GetTypeName2(&typeName)) && typeName == L"OriginProfileFeature")
			{
				featureName = FeatureName(feature);
				ATL::CComPtr<ISketch> sketch;
				ATL::CComVariant points;
				if (SketchOf(feature, sketch).ok() && SUCCEEDED(sketch->GetSketchPoints2(&points)))
				{
					const auto list = comvariant::ToDispatches(points);
					if (!list.empty())
						point = ATL::CComQIPtr<ISketchPoint>(list.front().p);
				}
				return;
			}
			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				return;
			feature = next;
		}
	}

	grpc::Status FindSketchByName(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& sketch)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		HRESULT hr = part->IFeatureByName(FromUtf8(name), &sketch);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!sketch)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active part has no feature named \"" + name + "\"");

		ATL::CComBSTR typeName;
		hr = sketch->GetTypeName2(&typeName);
		if (FAILED(hr))
			return ComError("IFeature::GetTypeName2", hr);
		if (typeName != L"ProfileFeature")
		{
			sketch.Release();
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + name + "\" isn't a 2D sketch");
		}
		return grpc::Status::OK;
	}

	bool CountContours(ISketch* sketch, sld_mcp::v1::SketchContours& contours)
	{
		ATL::CComVariant list;
		if (FAILED(sketch->GetSketchContours(&list)))
			return false;
		// VT_EMPTY / VT_NULL: no contours at all (e.g. only points).
		for (const auto& disp : comvariant::ToDispatches(list))
		{
			if (!disp)
				continue;
			ATL::CComQIPtr<ISketchContour> contour(disp.p);
			VARIANT_BOOL closed = VARIANT_FALSE;
			if (!contour || FAILED(contour->IsClosed(&closed)))
				return false;
			if (closed)
				contours.set_closed(contours.closed() + 1);
			else
				contours.set_open(contours.open() + 1);
		}
		return true;
	}

	grpc::Status FindLatestSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		sketch.Release();
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"ProfileFeature")
				sketch = feature;

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		if (!sketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active part has no 2D sketch -- create one first");
		return grpc::Status::OK;
	}

	grpc::Status FindLatestUnusedSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch, IFeature* except)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		sketch.Release();
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"ProfileFeature")
			{
				long children = 0;
				hr = feature->IGetChildCount(&children);
				if (FAILED(hr))
					return ComError("IFeature::IGetChildCount", hr);
				if (children == 0 && !(except && feature.IsEqualObject(except)))
					sketch = feature;
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		if (!sketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active part has no sketch that isn't already used by a feature -- "
				"create a sketch first");
		return grpc::Status::OK;
	}

	grpc::Status ResolveSketchFeature(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& sketch)
	{
		return name.empty() ? FindLatestSketch(doc, sketch) : FindSketchByName(doc, name, sketch);
	}

	grpc::Status SketchOf(IFeature* feature, ATL::CComPtr<ISketch>& sketch)
	{
		ATL::CComPtr<IDispatch> specific;
		HRESULT hr = feature->GetSpecificFeature2(&specific);
		if (FAILED(hr))
			return ComError("IFeature::GetSpecificFeature2", hr);
		sketch.Release();
		if (specific)
			specific->QueryInterface(IID_PPV_ARGS(&sketch));
		if (!sketch)
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks returned no sketch data for \"" + FeatureName(feature) + "\"");
		return grpc::Status::OK;
	}

	grpc::Status OpenExistingSketch(IModelDoc2* doc, IFeature* sketchFeature, ATL::CComPtr<ISketch>& sketch)
	{
		const std::string name = FeatureName(sketchFeature);
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		VARIANT_BOOL selected = VARIANT_FALSE;
		hr = sketchFeature->Select2(VARIANT_FALSE, 0, &selected);
		if (FAILED(hr))
			return ComError("IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select sketch \"" + name + "\"");
		hr = doc->EditSketch();
		if (FAILED(hr))
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return ComError("IModelDoc2::EditSketch", hr);
		}
		hr = doc->IGetActiveSketch2(&sketch);
		if (FAILED(hr) || !sketch)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks didn't open sketch \"" + name + "\" for editing");
		}
		return grpc::Status::OK;
	}

	grpc::Status GetSketchManager(IModelDoc2* doc, ATL::CComPtr<ISketchManager>& sketchManager)
	{
		HRESULT hr = doc->get_SketchManager(&sketchManager);
		if (FAILED(hr))
			return ComError("IModelDoc2::SketchManager", hr);
		if (!sketchManager)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");
		return grpc::Status::OK;
	}

	std::string PersistIdOf(IModelDocExtension* ext, IUnknown* obj)
	{
		ATL::CComQIPtr<IDispatch> disp(obj);
		ATL::CComVariant id;
		if (!disp || FAILED(ext->GetPersistReference3(disp, &id)))
			return {};
		return comvariant::ToBytes(id);
	}

	sld_mcp::v1::DefinitionStatus ToDefinitionStatus(long status)
	{
		switch (status)
		{
		case swUnderConstrained: return sld_mcp::v1::DEFINITION_STATUS_UNDER;
		case swFullyConstrained: return sld_mcp::v1::DEFINITION_STATUS_FULLY;
		case swOverConstrained:  return sld_mcp::v1::DEFINITION_STATUS_OVER;
		case swNoSolution:       return sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION;
		case swInvalidSolution:  return sld_mcp::v1::DEFINITION_STATUS_INVALID;
		case swAutosolveOff:     return sld_mcp::v1::DEFINITION_STATUS_NOT_SOLVED;
		default:                 return sld_mcp::v1::DEFINITION_STATUS_UNSPECIFIED;
		}
	}

	sld_mcp::v1::DefinitionStatus SketchStatus(ISketch* sketch)
	{
		long status = 0;
		if (FAILED(sketch->GetConstrainedStatus(&status)))
			return sld_mcp::v1::DEFINITION_STATUS_UNSPECIFIED;
		return ToDefinitionStatus(status);
	}

	bool SketchEntityId(IDispatch* entity, bool& isPoint, long long& id0, long long& id1)
	{
		if (!entity)
			return false;
		ATL::CComVariant id;
		if (ATL::CComQIPtr<ISketchSegment> segment(entity); segment)
		{
			isPoint = false;
			if (FAILED(segment->GetID(&id)))
				return false;
		}
		else if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
		{
			isPoint = true;
			if (FAILED(point->GetID(&id)))
				return false;
		}
		else
			return false;
		const std::vector<long long> v = comvariant::ToInt64s(id);
		if (v.size() < 2)
			return false;
		id0 = v[0];
		id1 = v[1];
		return true;
	}

	bool SetItemRef(IDispatch* entity, const std::string& sketchPid, sld_mcp::v1::SketchItemRef* ref)
	{
		bool isPoint = false;
		long long id0 = 0, id1 = 0;
		if (!SketchEntityId(entity, isPoint, id0, id1))
			return false;
		ref->set_sketch_persist_id(sketchPid);
		ref->set_point(isPoint);
		ref->set_id0(id0);
		ref->set_id1(id1);
		return true;
	}

	long ItemStatus(IDispatch* entity)
	{
		long status = 0;
		if (ATL::CComQIPtr<ISketchSegment> segment(entity); segment)
			segment->get_Status(&status);
		else if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
			point->get_Status(&status);
		return status;
	}

	bool PointPosition(IDispatch* pointDisp, double out[3])
	{
		ATL::CComQIPtr<ISketchPoint> point(pointDisp);
		if (!point || FAILED(point->get_X(&out[0])) || FAILED(point->get_Y(&out[1])))
			return false;
		out[2] = 0.0;
		return true;
	}

	void SetItemPoint(IDispatch* pointDisp, const std::string& sketchPid, sld_mcp::v1::Vec3* position,
		sld_mcp::v1::SketchItemRef* ref)
	{
		double p[3];
		sld_mcp::v1::SketchItemRef r;
		if (!PointPosition(pointDisp, p) || !SetItemRef(pointDisp, sketchPid, &r))
			return;
		SetVec3(position, p);
		*ref = r;
	}

	bool DescribeSketchItem(IDispatch* entity, const std::string& sketchPid, sld_mcp::v1::SketchItem* item)
	{
		if (!SetItemRef(entity, sketchPid, item->mutable_ref()))
			return false;
		item->set_status(ToDefinitionStatus(ItemStatus(entity)));

		if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
		{
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_POINT);
			double p[3];
			if (PointPosition(point, p))
				SetVec3(item->mutable_position(), p);
			return true;
		}
		ATL::CComQIPtr<ISketchSegment> segment(entity);
		if (!segment)
			return false;

		ATL::CComBSTR name;
		if (SUCCEEDED(segment->GetName(&name)))
			item->set_name(ToUtf8(name));
		VARIANT_BOOL construction = VARIANT_FALSE;
		if (SUCCEEDED(segment->get_ConstructionGeometry(&construction)))
			item->set_construction(construction != VARIANT_FALSE);
		double length = 0.0;
		if (SUCCEEDED(segment->GetLength(&length)))
			item->set_length(length);

		auto startEnd = [&](IDispatch* start, IDispatch* end) {
			sld_mcp::v1::Vec3 position;
			sld_mcp::v1::SketchItemRef ref;
			SetItemPoint(start, sketchPid, &position, &ref);
			if (ref.id0() || ref.id1() || ref.point())
			{
				*item->mutable_start() = position;
				*item->mutable_start_point() = ref;
			}
			position.Clear();
			ref.Clear();
			SetItemPoint(end, sketchPid, &position, &ref);
			if (ref.id0() || ref.id1() || ref.point())
			{
				*item->mutable_end() = position;
				*item->mutable_end_point() = ref;
			}
		};

		long type = -1;
		segment->GetType(&type);
		switch (type)
		{
		case swSketchLINE:
		{
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_LINE);
			ATL::CComQIPtr<ISketchLine> line(segment);
			ATL::CComPtr<IDispatch> start, end;
			if (line && SUCCEEDED(line->GetStartPoint2(&start)) && SUCCEEDED(line->GetEndPoint2(&end)))
				startEnd(start, end);
			break;
		}
		case swSketchARC:
		{
			ATL::CComQIPtr<ISketchArc> arc(segment);
			int circle = 0;
			if (arc)
				arc->IsCircle(&circle);
			item->set_type(circle ? sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE : sld_mcp::v1::SKETCH_ITEM_TYPE_ARC);
			if (!arc)
				break;
			ATL::CComPtr<IDispatch> center;
			if (SUCCEEDED(arc->GetCenterPoint2(&center)))
				SetItemPoint(center, sketchPid, item->mutable_center(), item->mutable_center_point());
			double radius = 0.0;
			if (SUCCEEDED(arc->GetRadius(&radius)))
				item->set_radius(radius);
			ATL::CComPtr<IDispatch> start, end;
			if (!circle && SUCCEEDED(arc->GetStartPoint2(&start)) && SUCCEEDED(arc->GetEndPoint2(&end)))
				startEnd(start, end);
			break;
		}
		case swSketchSPLINE:
		{
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_SPLINE);
			ATL::CComQIPtr<ISketchSpline> spline(segment);
			ATL::CComVariant points;
			if (spline && SUCCEEDED(spline->GetPoints2(&points)))
			{
				const auto list = comvariant::ToDispatches(points);
				if (list.size() >= 2)
					startEnd(list.front(), list.back());
			}
			break;
		}
		case swSketchELLIPSE:
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_ELLIPSE);
			break;
		default:
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_OTHER);
			break;
		}
		return true;
	}

	void ReadSketchState(ISketch* sketch, const std::string& sketchPid, sld_mcp::v1::SketchState* state)
	{
		state->set_status(SketchStatus(sketch));
		SketchItems items;
		items.Load(sketch);
		int count = 0;
		for (const auto& e : items.All())
		{
			const long status = ItemStatus(e.obj);
			if (status == swFullyConstrained || status == 0 || status == swUnknownConstraint)
				continue;
			++count;
			if (state->not_fully_defined_size() < kMaxListedUndefined)
			{
				sld_mcp::v1::SketchItem item;
				if (DescribeSketchItem(e.obj, sketchPid, &item))
					state->add_not_fully_defined()->Swap(&item);
			}
		}
		state->set_not_fully_defined_count(count);
	}

	grpc::Status CheckRefsInSketch(IModelDocExtension* ext,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef>& refs,
		const std::string& sketchPid, const std::string& sketchName, const std::string& where)
	{
		for (int i = 0; i < refs.size(); ++i)
		{
			if (!refs[i].has_item() || refs[i].item().sketch_persist_id() == sketchPid)
				continue;
			// Same sketch, different bytes? Compare by name.
			ATL::CComPtr<IDispatch> obj;
			grpc::Status status = ResolvePersistId(ext, refs[i].item().sketch_persist_id(), obj);
			ATL::CComQIPtr<IFeature> other(obj.p);
			const std::string otherName = other ? FeatureName(other) : std::string();
			if (status.ok() && otherName == sketchName)
				continue;
			const std::string prefix = where + ", reference " + std::to_string(i + 1) + ": ";
			if (!status.ok())
				return grpc::Status(status.error_code(), prefix + "its sketch " + status.error_message());
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				prefix + "that entity is in sketch \"" + otherName + "\", not \"" + sketchName + "\"");
		}
		return grpc::Status::OK;
	}

	std::vector<ATL::CComPtr<ISketchRelation>> SketchRelations(ISketch* sketch, long filter)
	{
		std::vector<ATL::CComPtr<ISketchRelation>> out;
		ATL::CComPtr<ISketchRelationManager> manager;
		ATL::CComVariant list;
		if (FAILED(sketch->get_RelationManager(&manager)) || !manager || FAILED(manager->GetRelations(filter, &list)))
			return out;
		for (const auto& disp : comvariant::ToDispatches(list))
			if (ATL::CComQIPtr<ISketchRelation> relation(disp.p); relation)
				out.push_back(relation);
		return out;
	}
}
