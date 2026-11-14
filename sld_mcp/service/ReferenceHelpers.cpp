// ReferenceHelpers.cpp : see ReferenceHelpers.h.
#include "pch.h"
#include "ReferenceHelpers.h"
#include "FeatureTreeHelpers.h"
#include "SketchHelpers.h"

namespace sld_mcp::service
{
	namespace
	{
		// The origin and unit axes of a transform's frame, in model space.
		// Read from ArrayData ([X axis, Y axis, Z axis, origin, scale, ...]) in
		// one call: mapping four IMathPoints through it (as TransformPoint
		// does) took ~0.2 s per plane, which made listing a part's features
		// several times slower. The layout was checked against planes and
		// coordinate systems of known orientation (devloop/scenarios/reference.py).
		bool FrameOf(IMathTransform* xform, Eigen::Vector3d& origin, Eigen::Vector3d axes[3])
		{
			ATL::CComVariant data;
			std::vector<double> m;
			if (FAILED(xform->get_ArrayData(&data)) || (m = comvariant::ToDoubles(data)).size() < 12)
				return false;
			for (int a = 0; a < 3; ++a)
				axes[a] = Eigen::Vector3d(m[3 * a], m[3 * a + 1], m[3 * a + 2]).normalized();
			origin = Eigen::Vector3d(m[9], m[10], m[11]);
			return true;
		}

		// Reference features (planes, axes, ...) can be references; other
		// features can't.
		grpc::Status PickFeature(IFeature* feature, Pick& out)
		{
			const std::string type = FeatureTypeName(feature);
			const std::string name = FeatureName(feature);
			if (!IsReferenceType(type))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" (" + type + ") isn't reference geometry -- reference faces, edges, vertices, "
					"sketch entities, the origin, or reference planes / axes / points / coordinate systems");
			out.kind = Pick::Feature;
			out.obj = feature;
			out.what = name;
			return grpc::Status::OK;
		}

		const char* SketchSegmentName(ISketchSegment* segment)
		{
			long type = -1;
			segment->GetType(&type);
			switch (type)
			{
			case swSketchLINE:    return "sketch line";
			case swSketchARC:     return "sketch arc";
			case swSketchSPLINE:  return "sketch spline";
			case swSketchELLIPSE: return "sketch ellipse";
			default:              return "sketch segment";
			}
		}
	}

	std::string ReferenceWhat(IDispatch* obj)
	{
		if (!obj)
			return "nothing";
		if (ATL::CComQIPtr<IFeature> feature(obj); feature)
			return FeatureName(feature);
		if (ATL::CComQIPtr<IFace2>(obj))
			return "face";
		if (ATL::CComQIPtr<IEdge>(obj))
			return "edge";
		if (ATL::CComQIPtr<ISketchSegment>(obj))
			return "sketch segment";
		if (ATL::CComQIPtr<IRefAxis>(obj))
			return "reference axis";
		if (ATL::CComQIPtr<IRefPlane>(obj))
			return "plane";
		return "reference";
	}

	bool IsReferenceType(const std::string& type)
	{
		return type == "RefPlane" || type == "RefAxis" || type == "RefPoint" || type == "CoordSys";
	}

	bool DescribeReferenceGeometry(IModelDocExtension* ext, IFeature* feature, const std::string& type,
		sld_mcp::v1::ReferenceGeometry* out)
	{
		if (type == "CoordSys")
		{
			ATL::CComPtr<IMathTransform> xform;
			Eigen::Vector3d origin, axes[3];
			if (!ext || FAILED(ext->GetCoordinateSystemTransformByName(FromUtf8(FeatureName(feature)), &xform)) ||
				!xform || !FrameOf(xform, origin, axes))
				return false;
			SetVec3(out->mutable_origin(), origin);
			SetVec3(out->mutable_x_axis(), axes[0]);
			SetVec3(out->mutable_y_axis(), axes[1]);
			SetVec3(out->mutable_z_axis(), axes[2]);
			return true;
		}

		ATL::CComPtr<IDispatch> specific;
		if (FAILED(feature->GetSpecificFeature2(&specific)) || !specific)
			return false;
		if (type == "RefPlane")
		{
			ATL::CComQIPtr<IRefPlane> plane(specific.p);
			ATL::CComPtr<IMathTransform> xform;
			Eigen::Vector3d origin, axes[3];
			// The plane is the XY plane of its transform's frame.
			if (!plane || FAILED(plane->get_Transform(&xform)) || !xform || !FrameOf(xform, origin, axes))
				return false;
			SetVec3(out->mutable_origin(), origin);
			SetVec3(out->mutable_direction(), axes[2]);
			SetVec3(out->mutable_x_axis(), axes[0]);
			SetVec3(out->mutable_y_axis(), axes[1]);
			return true;
		}
		if (type == "RefAxis")
		{
			ATL::CComQIPtr<IRefAxis> axis(specific.p);
			ATL::CComVariant params;
			std::vector<double> p;  // [start xyz, end xyz]
			if (!axis || FAILED(axis->GetRefAxisParams(&params)) || (p = comvariant::ToDoubles(params)).size() < 6)
				return false;
			const Eigen::Vector3d start(p[0], p[1], p[2]), end(p[3], p[4], p[5]);
			SetVec3(out->mutable_origin(), start);
			SetVec3(out->mutable_direction(), (end - start).normalized());
			out->set_length((end - start).norm());
			return true;
		}
		if (type == "RefPoint")
		{
			ATL::CComQIPtr<IRefPoint> point(specific.p);
			ATL::CComPtr<IMathPoint> at;
			ATL::CComVariant data;
			std::vector<double> p;
			if (!point || FAILED(point->GetRefPoint(&at)) || !at || FAILED(at->get_ArrayData(&data)) ||
				(p = comvariant::ToDoubles(data)).size() < 3)
				return false;
			SetVec3(out->mutable_origin(), &p[0]);
			return true;
		}
		return false;
	}

	grpc::Status ResolvePick(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref,
		Pick& out)
	{
		using Ref = sld_mcp::v1::GeometryRef;
		switch (ref.target_case())
		{
		case Ref::kModelPersistId:
		{
			if (grpc::Status status = ResolvePersistId(ext, ref.model_persist_id(), out.obj); !status.ok())
				return status;
			if (ATL::CComQIPtr<IFeature> feature(out.obj.p); feature)
				return PickFeature(feature, out);
			out.kind = Pick::Entity;
			if (ATL::CComQIPtr<IFace2>(out.obj.p))
				out.what = "face";
			else if (ATL::CComQIPtr<IEdge>(out.obj.p))
				out.what = "edge";
			else if (ATL::CComQIPtr<IVertex>(out.obj.p))
				out.what = "vertex";
			else
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"only faces, edges, vertices and reference features can be referenced by id");
			return grpc::Status::OK;
		}
		case Ref::kSketchItem:
		{
			ATL::CComPtr<IDispatch> sketchObj;
			if (grpc::Status status = ResolvePersistId(ext, ref.sketch_item().sketch_persist_id(), sketchObj);
				!status.ok())
				return grpc::Status(status.error_code(), "the entity's sketch " + status.error_message());
			ATL::CComQIPtr<IFeature> sketchFeature(sketchObj.p);
			ATL::CComPtr<ISketch> sketch;
			if (!sketchFeature)
				return grpc::Status(grpc::StatusCode::INTERNAL, "a sketch reference didn't resolve to a feature");
			if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
				return status;
			SketchItems items;
			items.Load(sketch);
			out.obj = items.Find(ref.sketch_item());
			if (!out.obj)
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					"no such entity in sketch \"" + FeatureName(sketchFeature) +
					"\" (deleted or merged?) -- inspect the sketch again for current IDs");
			if (ATL::CComQIPtr<ISketchSegment> segment(out.obj.p); segment)
			{
				out.kind = Pick::SketchSegment;
				out.what = SketchSegmentName(segment);
			}
			else
			{
				out.kind = Pick::SketchPoint;
				out.what = "sketch point";
			}
			out.what += " of " + FeatureName(sketchFeature);
			return grpc::Status::OK;
		}
		case Ref::kOrigin:
		{
			ATL::CComPtr<ISketchPoint> point;
			std::string originName;
			FindOrigin(doc, point, originName);
			out.what = "the origin";
			if (point)
			{
				out.kind = Pick::SketchPoint;
				out.obj = point;
				return grpc::Status::OK;
			}
			if (originName.empty())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't find the part's origin");
			// The origin's point as the UI names it ("Point1@Origin").
			out.kind = Pick::ByName;
			out.selectName = "Point1@" + originName;
			return grpc::Status::OK;
		}
		case Ref::kStandardPlane:
		{
			int index = 0;
			if (!StandardPlaneIndex(ref.standard_plane(), index))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
			ATL::CComPtr<IFeature> plane;
			if (grpc::Status status = FindStandardPlane(doc, index, plane); !status.ok())
				return status;
			return PickFeature(plane, out);
		}
		case Ref::kFeatureName:
		{
			ATL::CComPtr<IFeature> feature;
			if (grpc::Status status = FeatureByName(doc, ref.feature_name(), feature); !status.ok())
				return status;
			return PickFeature(feature, out);
		}
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a reference is empty");
		}
	}

	bool SelectPick(IModelDocExtension* ext, ISelectData* data, const Pick& pick, long mark)
	{
		data->put_Mark(mark);
		VARIANT_BOOL selected = VARIANT_FALSE;
		switch (pick.kind)
		{
		case Pick::Entity:
		{
			ATL::CComQIPtr<IEntity> entity(pick.obj.p);
			return entity && SUCCEEDED(entity->Select4(VARIANT_TRUE, data, &selected)) && selected;
		}
		case Pick::Feature:
		{
			ATL::CComQIPtr<IFeature> feature(pick.obj.p);
			return feature && SUCCEEDED(feature->Select2(VARIANT_TRUE, mark, &selected)) && selected;
		}
		case Pick::SketchSegment:
		{
			ATL::CComQIPtr<ISketchSegment> segment(pick.obj.p);
			return segment && SUCCEEDED(segment->Select4(VARIANT_TRUE, data, &selected)) && selected;
		}
		case Pick::SketchPoint:
		{
			ATL::CComQIPtr<ISketchPoint> point(pick.obj.p);
			return point && SUCCEEDED(point->Select4(VARIANT_TRUE, data, &selected)) && selected;
		}
		case Pick::ByName:
			return SUCCEEDED(ext->SelectByID2(FromUtf8(pick.selectName), ATL::CComBSTR(L"EXTSKETCHPOINT"),
				0.0, 0.0, 0.0, VARIANT_TRUE, mark, nullptr, swSelectOptionDefault, &selected)) && selected;
		}
		return false;
	}

	grpc::Status Selection::Add(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref,
		long mark, const std::string& label)
	{
		Pick pick;
		if (grpc::Status status = ResolvePick(doc, ext, ref, pick); !status.ok())
			return grpc::Status(status.error_code(), label + ": " + status.error_message());
		picks.push_back(pick);
		marks.push_back(mark);
		return grpc::Status::OK;
	}

	std::string Selection::Describe() const
	{
		std::string text;
		for (const Pick& p : picks)
			text += (text.empty() ? "" : ", ") + p.what;
		return text;
	}

	grpc::Status Selection::Select(IModelDoc2* doc, IModelDocExtension* ext) const
	{
		ATL::CComPtr<ISelectionMgr> selectionManager;
		HRESULT hr = doc->get_ISelectionManager(&selectionManager);
		if (FAILED(hr) || !selectionManager)
			return ComError("IModelDoc2::ISelectionManager", hr);
		ATL::CComPtr<ISelectData> data;
		hr = selectionManager->CreateSelectData(&data);
		if (FAILED(hr) || !data)
			return ComError("ISelectionMgr::CreateSelectData", hr);

		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t i = 0; i < picks.size(); ++i)
			if (!SelectPick(ext, data, picks[i], marks[i]))
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select reference " + std::to_string(i + 1) + " (" + picks[i].what +
					") -- is it hidden or suppressed?");
			}
		return grpc::Status::OK;
	}
}
