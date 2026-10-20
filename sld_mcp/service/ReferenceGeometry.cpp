// ReferenceGeometry.cpp : CreateReferenceGeometry: reference planes, axes, points and
// coordinate systems, from model geometry, sketch entities, the origin or the default planes.
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxPlaneRefs = 3;
	constexpr int kMaxAxisRefs = 2;
	constexpr int kMaxPointRefs = 2;
	// Most points one along-curve request may make.
	constexpr int kMaxRefPoints = 100;

	// A GeometryRef resolved to what gets selected.
	struct Pick
	{
		enum Kind { Entity, Feature, SketchSegment, SketchPoint, ByName } kind = Entity;
		ATL::CComPtr<IDispatch> obj;
		// ByName: SelectByID2 name and type (the origin's point when
		// SolidWorks won't hand out the object).
		std::string selectName;
		// For messages: "face", "sketch line", "Plane1", "the origin", ...
		std::string what;
	};

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
			ATL::CComQIPtr<IPartDoc> part(doc);
			ATL::CComPtr<IFeature> feature;
			if (!part || FAILED(part->IFeatureByName(FromUtf8(ref.feature_name()), &feature)) || !feature)
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					"the active part has no feature named \"" + ref.feature_name() + "\"");
			return PickFeature(feature, out);
		}
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a reference is empty");
		}
	}

	// Adds a pick to the selection with `mark` (the role SolidWorks reads it in).
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

	// The references of one request, resolved, with the mark each is
	// selected with.
	struct Selection
	{
		std::vector<Pick> picks;
		std::vector<long> marks;

		grpc::Status Add(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref,
			long mark, const std::string& label)
		{
			Pick pick;
			if (grpc::Status status = ResolvePick(doc, ext, ref, pick); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
			picks.push_back(pick);
			marks.push_back(mark);
			return grpc::Status::OK;
		}

		// "face, sketch line of Sketch2" for messages.
		std::string Describe() const
		{
			std::string text;
			for (const Pick& p : picks)
				text += (text.empty() ? "" : ", ") + p.what;
			return text;
		}

		grpc::Status Select(IModelDoc2* doc, IModelDocExtension* ext) const
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
	};

	long ToSwPlaneConstraint(sld_mcp::v1::PlaneConstraint c)
	{
		switch (c)
		{
		case sld_mcp::v1::PLANE_CONSTRAINT_COINCIDENT:    return swRefPlaneReferenceConstraint_Coincident;
		case sld_mcp::v1::PLANE_CONSTRAINT_PARALLEL:      return swRefPlaneReferenceConstraint_Parallel;
		case sld_mcp::v1::PLANE_CONSTRAINT_PERPENDICULAR: return swRefPlaneReferenceConstraint_Perpendicular;
		case sld_mcp::v1::PLANE_CONSTRAINT_DISTANCE:      return swRefPlaneReferenceConstraint_Distance;
		case sld_mcp::v1::PLANE_CONSTRAINT_ANGLE:         return swRefPlaneReferenceConstraint_Angle;
		case sld_mcp::v1::PLANE_CONSTRAINT_TANGENT:       return swRefPlaneReferenceConstraint_Tangent;
		case sld_mcp::v1::PLANE_CONSTRAINT_MID_PLANE:     return swRefPlaneReferenceConstraint_MidPlane;
		default:                                          return 0;
		}
	}

	long ToSwRefPointType(sld_mcp::v1::ReferencePointKind kind)
	{
		switch (kind)
		{
		case sld_mcp::v1::REFERENCE_POINT_KIND_ARC_CENTER:   return swRefPointCenterEdge;
		case sld_mcp::v1::REFERENCE_POINT_KIND_FACE_CENTER:  return swRefPointFaceCenter;
		case sld_mcp::v1::REFERENCE_POINT_KIND_PROJECTION:   return swRefPointFaceVertexProjection;
		case sld_mcp::v1::REFERENCE_POINT_KIND_INTERSECTION: return swRefPointIntersection;
		case sld_mcp::v1::REFERENCE_POINT_KIND_ALONG_CURVE:  return swRefPointAlongCurve;
		case sld_mcp::v1::REFERENCE_POINT_KIND_AT_POINT:     return swRefPointSketchPoint;
		default:                                             return swRefPointInvalid;
		}
	}

	// Top-level feature names, to tell the new features apart afterwards.
	std::set<std::string> TopLevelNames(IModelDoc2* doc)
	{
		std::set<std::string> names;
		ATL::CComPtr<IFeature> feature;
		doc->IFirstFeature(&feature);
		while (feature)
		{
			names.insert(FeatureName(feature));
			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				break;
			feature = next;
		}
		return names;
	}

	// The reference features that weren't in `before`, in tree order.
	std::vector<ATL::CComPtr<IFeature>> NewReferenceFeatures(IModelDoc2* doc, const std::set<std::string>& before)
	{
		std::vector<ATL::CComPtr<IFeature>> created;
		ATL::CComPtr<IFeature> feature;
		doc->IFirstFeature(&feature);
		while (feature)
		{
			if (!before.count(FeatureName(feature)) && IsReferenceType(FeatureTypeName(feature)))
				created.push_back(feature);
			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				break;
			feature = next;
		}
		return created;
	}

	void DeleteCreated(IModelDoc2* doc, IModelDocExtension* ext, const std::vector<ATL::CComPtr<IFeature>>& features)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
		for (const auto& feature : features)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			any |= SUCCEEDED(feature->Select2(VARIANT_TRUE, 0, &selected)) && selected;
		}
		VARIANT_BOOL deleted = VARIANT_FALSE;
		if (any)
			ext->DeleteSelection2(swDelete_Absorbed, &deleted);
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
	}

	// SolidWorks makes no reference point at a bare vertex (no
	// swRefPointType_e takes one, seen 2026-10-19), so this puts one 0 % or
	// 100 % along one of its edges, whichever end the vertex is. False if
	// neither lands on it; nothing is left behind then.
	bool PointAtVertex(IModelDoc2* doc, IModelDocExtension* ext, IFeatureManager* featureManager, IVertex* vertex)
	{
		ATL::CComVariant edges, at;
		std::vector<double> target;
		if (FAILED(vertex->GetEdges(&edges)) || FAILED(vertex->GetPoint(&at)) ||
			(target = comvariant::ToDoubles(at)).size() < 3)
			return false;
		const auto list = comvariant::ToDispatches(edges);
		if (list.empty())
			return false;
		Selection edge;
		edge.picks.push_back(Pick{Pick::Entity, list.front(), {}, "edge"});
		edge.marks.push_back(0);
		for (const double percent : {0.0, 100.0})
		{
			const std::set<std::string> before = TopLevelNames(doc);
			if (!edge.Select(doc, ext).ok())
				return false;
			ATL::CComVariant features;
			featureManager->InsertReferencePoint(swRefPointAlongCurve, swRefPointAlongCurvePercentage, percent, 1,
				&features);
			doc->ClearSelection2(VARIANT_TRUE);
			const std::vector<ATL::CComPtr<IFeature>> created = NewReferenceFeatures(doc, before);
			sld_mcp::v1::ReferenceGeometry where;
			if (created.size() == 1 && DescribeReferenceGeometry(ext, created.front(), "RefPoint", &where) &&
				(Eigen::Vector3d(where.origin().x(), where.origin().y(), where.origin().z()) -
					Eigen::Vector3d(target[0], target[1], target[2])).norm() < 1e-9)
				return true;
			if (!created.empty())
				DeleteCreated(doc, ext, created);
		}
		return false;
	}

	// Shape checks that need only the request.
	grpc::Status Validate(const sld_mcp::v1::CreateReferenceGeometryRequest& request)
	{
		using R = sld_mcp::v1::CreateReferenceGeometryRequest;
		const auto bad = [](const std::string& message) {
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, message);
		};
		switch (request.geometry_case())
		{
		case R::kPlane:
		{
			const auto& refs = request.plane().references();
			if (refs.empty() || refs.size() > kMaxPlaneRefs)
				return bad("a plane takes 1 to 3 references");
			for (const auto& r : refs)
			{
				if (!ToSwPlaneConstraint(r.constraint()))
					return bad("every plane reference needs a constraint");
				if (r.constraint() == sld_mcp::v1::PLANE_CONSTRAINT_DISTANCE &&
					!(std::isfinite(r.distance()) && r.distance() > 0.0))
					return bad("a distance must be more than 0");
				if (r.constraint() == sld_mcp::v1::PLANE_CONSTRAINT_ANGLE &&
					!(std::isfinite(r.angle()) && r.angle() > 0.0))
					return bad("an angle must be more than 0");
			}
			return grpc::Status::OK;
		}
		case R::kAxis:
			if (request.axis().references().empty() || request.axis().references().size() > kMaxAxisRefs)
				return bad("an axis takes 1 or 2 references");
			return grpc::Status::OK;
		case R::kPoint:
		{
			const auto& point = request.point();
			if (ToSwRefPointType(point.kind()) == swRefPointInvalid)
				return bad("give the kind of reference point");
			if (point.references().empty() || point.references().size() > kMaxPointRefs)
				return bad("a reference point takes 1 or 2 references");
			if (point.kind() != sld_mcp::v1::REFERENCE_POINT_KIND_ALONG_CURVE)
				return grpc::Status::OK;
			if (point.count() < 0 || point.count() > kMaxRefPoints)
				return bad("count must be 1 to " + std::to_string(kMaxRefPoints));
			switch (point.along())
			{
			case sld_mcp::v1::ALONG_CURVE_DISTANCE:
				if (!(std::isfinite(point.distance()) && point.distance() > 0.0))
					return bad("a distance along the curve must be more than 0");
				return grpc::Status::OK;
			case sld_mcp::v1::ALONG_CURVE_PERCENT:
				if (!(std::isfinite(point.percent()) && point.percent() >= 0.0 && point.percent() <= 100.0))
					return bad("a percentage must be 0 to 100");
				return grpc::Status::OK;
			case sld_mcp::v1::ALONG_CURVE_EVENLY:
				return grpc::Status::OK;
			default:
				return bad("a point along a curve needs a distance, a percentage or a count");
			}
		}
		case R::kCoordinateSystem:
			return grpc::Status::OK;
		default:
			return bad("give a plane, axis, point or coordinate system");
		}
	}
}

grpc::Status SolidWorksService::CreateReferenceGeometry(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CreateReferenceGeometryRequest* request,
	sld_mcp::v1::CreateReferenceGeometryResponse* response)
{
	if (grpc::Status status = Validate(*request); !status.ok())
		return status;

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComPtr<IFeatureManager> featureManager;
	HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr) || !featureManager)
		return ComError("IModelDoc2::FeatureManager", hr);

	// No What's Wrong dialog if the new feature fails to rebuild.
	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	// Resolve every reference before changing anything.
	using R = sld_mcp::v1::CreateReferenceGeometryRequest;
	Selection selection;
	const char* what = "a feature";
	switch (request->geometry_case())
	{
	case R::kPlane:
		what = "a plane";
		for (int i = 0; i < request->plane().references_size(); ++i)
			if (grpc::Status status = selection.Add(doc, ext, request->plane().references(i).ref(), i,
				"reference " + std::to_string(i + 1)); !status.ok())
				return status;
		break;
	case R::kAxis:
		what = "an axis";
		for (int i = 0; i < request->axis().references_size(); ++i)
			if (grpc::Status status = selection.Add(doc, ext, request->axis().references(i), 0,
				"reference " + std::to_string(i + 1)); !status.ok())
				return status;
		break;
	case R::kPoint:
		what = "a point";
		for (int i = 0; i < request->point().references_size(); ++i)
			if (grpc::Status status = selection.Add(doc, ext, request->point().references(i), 0,
				"reference " + std::to_string(i + 1)); !status.ok())
				return status;
		break;
	case R::kCoordinateSystem:
	{
		what = "a coordinate system";
		// InsertCoordinateSystem reads the origin with mark 1 and the X / Y /
		// Z axis references with marks 2 / 4 / 8.
		const auto& cs = request->coordinate_system();
		const std::pair<const sld_mcp::v1::GeometryRef*, std::pair<long, const char*>> roles[] = {
			{cs.has_origin() ? &cs.origin() : nullptr, {1, "origin"}},
			{cs.has_x_axis() ? &cs.x_axis() : nullptr, {2, "x axis"}},
			{cs.has_y_axis() ? &cs.y_axis() : nullptr, {4, "y axis"}},
			{cs.has_z_axis() ? &cs.z_axis() : nullptr, {8, "z axis"}},
		};
		for (const auto& [ref, role] : roles)
			if (ref)
				if (grpc::Status status = selection.Add(doc, ext, *ref, role.first, role.second); !status.ok())
					return status;
		break;
	}
	default:
		break;
	}

	// A point at a vertex is made another way (PointAtVertex), from an edge.
	ATL::CComQIPtr<IVertex> pointVertex;
	if (request->geometry_case() == R::kPoint &&
		request->point().kind() == sld_mcp::v1::REFERENCE_POINT_KIND_AT_POINT && selection.picks.size() == 1)
		pointVertex = selection.picks.front().obj;

	const std::set<std::string> before = TopLevelNames(doc);
	if (!pointVertex)
		if (grpc::Status status = selection.Select(doc, ext); !status.ok())
			return status;

	bool made = false;
	switch (request->geometry_case())
	{
	case R::kPlane:
	{
		long constraints[kMaxPlaneRefs] = {};
		double values[kMaxPlaneRefs] = {};
		for (int i = 0; i < request->plane().references_size(); ++i)
		{
			const auto& r = request->plane().references(i);
			constraints[i] = ToSwPlaneConstraint(r.constraint());
			if (r.flip())
				constraints[i] |= swRefPlaneReferenceConstraint_OptionFlip;
			if (r.constraint() == sld_mcp::v1::PLANE_CONSTRAINT_DISTANCE)
				values[i] = r.distance();
			else if (r.constraint() == sld_mcp::v1::PLANE_CONSTRAINT_ANGLE)
				values[i] = r.angle();
		}
		ATL::CComPtr<IDispatch> plane;
		hr = featureManager->InsertRefPlane(constraints[0], values[0], constraints[1], values[1], constraints[2],
			values[2], &plane);
		made = SUCCEEDED(hr) && plane;
		break;
	}
	case R::kAxis:
	{
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = doc->InsertAxis2(VARIANT_TRUE, &ok);
		made = SUCCEEDED(hr) && ok;
		break;
	}
	case R::kPoint:
	{
		if (pointVertex)
		{
			made = PointAtVertex(doc, ext, featureManager, pointVertex);
			break;
		}
		const auto& point = request->point();
		long along = swRefPointAlongCurveDistance;
		double value = 0.0;
		if (point.kind() == sld_mcp::v1::REFERENCE_POINT_KIND_ALONG_CURVE)
			switch (point.along())
			{
			case sld_mcp::v1::ALONG_CURVE_PERCENT:
				along = swRefPointAlongCurvePercentage;
				value = point.percent();  // 0-100, seen 2026-10-19 (0.5 made a point at 0.5 %)
				break;
			case sld_mcp::v1::ALONG_CURVE_EVENLY:
				along = swRefPointAlongCurveEvenlyDistributed;
				break;
			default:
				value = point.distance();
				break;
			}
		ATL::CComVariant features;
		hr = featureManager->InsertReferencePoint(ToSwRefPointType(point.kind()), along, value,
			std::max(1, point.count()), &features);
		made = SUCCEEDED(hr) && !comvariant::ToDispatches(features).empty();
		break;
	}
	case R::kCoordinateSystem:
	{
		const auto& cs = request->coordinate_system();
		ATL::CComPtr<IFeature> feature;
		hr = featureManager->InsertCoordinateSystem(cs.flip_x() ? VARIANT_TRUE : VARIANT_FALSE,
			cs.flip_y() ? VARIANT_TRUE : VARIANT_FALSE, cs.flip_z() ? VARIANT_TRUE : VARIANT_FALSE, &feature);
		made = SUCCEEDED(hr) && feature;
		break;
	}
	default:
		break;
	}
	doc->ClearSelection2(VARIANT_TRUE);

	// Whatever the call returned, the new features are what's in the tree now.
	std::vector<ATL::CComPtr<IFeature>> created = NewReferenceFeatures(doc, before);
	const std::string refs = selection.Describe();
	if (!made || created.empty())
	{
		if (!created.empty())
			DeleteCreated(doc, ext, created);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			std::string("SolidWorks couldn't make ") + what + " from " + (refs.empty() ? "no references" : refs) +
			" -- check the references and how they're combined");
	}

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);
	for (const auto& feature : created)
	{
		VARIANT_BOOL warning = VARIANT_FALSE;
		long code = swFeatureErrorNone;
		if (SUCCEEDED(feature->GetErrorCode2(&warning, &code)) && code != swFeatureErrorNone && !warning)
		{
			const std::string name = FeatureName(feature);
			DeleteCreated(doc, ext, created);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				name + " (" + what + " made from " + refs + ") fails to rebuild (error " +
				std::to_string(code) + "), so it was deleted again");
		}
	}

	if (!request->name().empty())
		for (size_t i = 0; i < created.size(); ++i)
		{
			const std::string name = request->name() + (i == 0 ? "" : std::to_string(i + 1));
			created[i]->put_Name(FromUtf8(name));
		}

	PersistIds ids(ext);
	for (const auto& feature : created)
	{
		sld_mcp::v1::FeatureInfo* info = response->add_features();
		const std::string type = FeatureTypeName(feature);
		info->set_persist_id(ids.Get(feature));
		info->set_name(FeatureName(feature));
		info->set_type_name(type);
		ATL::CComVariant parents;
		if (SUCCEEDED(feature->GetParents(&parents)))
			for (const auto& parent : FeatureList(parents))
				info->add_parents(FeatureName(parent));
		VARIANT_BOOL warning = VARIANT_FALSE;
		long code = swFeatureErrorNone;
		if (SUCCEEDED(feature->GetErrorCode2(&warning, &code)) && code != swFeatureErrorNone)
		{
			info->set_error_code(static_cast<int32_t>(code));
			info->set_error_is_warning(warning != VARIANT_FALSE);
		}
		DescribeReferenceGeometry(ext, feature, type, info->mutable_reference());
	}
	return grpc::Status::OK;
}
