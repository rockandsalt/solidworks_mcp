// Sketches.cpp : Drawing sketches: CreateRectangleSketch, AddSketchEntities, InspectSketch.
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// What a new sketch goes on: a planar face (`face` + `faceEntity`) or a
	// reference plane feature (`plane`); exactly one is set once resolved.
	struct SketchTarget
	{
		ATL::CComPtr<IFace2> face;
		ATL::CComPtr<IEntity> faceEntity;
		ATL::CComPtr<IFeature> plane;
	};

	// Resolves `onPersistId` (a planar face or a reference plane feature), or
	// the standard plane `planeIndex` when it's empty.
	grpc::Status ResolveSketchTarget(IModelDoc2* doc, const std::string& onPersistId, int planeIndex,
		SketchTarget& target)
	{
		if (onPersistId.empty())
			return FindStandardPlane(doc, planeIndex, target.plane);

		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, onPersistId, obj); !status.ok())
			return status;

		if (ATL::CComQIPtr<IFace2> face(obj.p); face)
		{
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL planar = VARIANT_FALSE;
			if (FAILED(face->IGetSurface(&surface)) || !surface ||
				FAILED(surface->IsPlane(&planar)) || !planar)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"that face isn't planar -- sketches need a planar face or a reference plane");
			if (FAILED(face->QueryInterface(IID_PPV_ARGS(&target.faceEntity))) || !target.faceEntity)
				return grpc::Status(grpc::StatusCode::INTERNAL, "face has no IEntity interface");
			target.face = face;
			return grpc::Status::OK;
		}
		if (ATL::CComQIPtr<IFeature> feature(obj.p); feature)
		{
			ATL::CComBSTR typeName;
			HRESULT hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName != L"RefPlane")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + FeatureName(feature) + "\" isn't a reference plane -- sketches need a "
					"planar face or a reference plane");
			target.plane = feature;
			return grpc::Status::OK;
		}
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"that ID isn't a face or a reference plane -- sketches need a planar face or a reference plane");
	}

	// Selects the target and opens a new sketch on it. On failure nothing is
	// left open.
	grpc::Status OpenNewSketch(IModelDoc2* doc, ISketchManager* sketchManager, const SketchTarget& target,
		ATL::CComPtr<ISketch>& sketch)
	{
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		VARIANT_BOOL selected = VARIANT_FALSE;
		hr = target.faceEntity ? target.faceEntity->Select4(VARIANT_FALSE, nullptr, &selected)
		                       : target.plane->Select2(VARIANT_FALSE, 0, &selected);
		if (FAILED(hr))
			return ComError(target.faceEntity ? "IEntity::Select4" : "IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch plane");

		// Opens a new sketch on the selected plane or face.
		hr = sketchManager->InsertSketch(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("ISketchManager::InsertSketch", hr);
		hr = doc->IGetActiveSketch2(&sketch);
		if (FAILED(hr) || !sketch)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't open a sketch on the plane");
		}
		return grpc::Status::OK;
	}

	// Deletes a (closed) sketch feature, e.g. one left empty by a failed
	// RPC. Best effort.
	void DeleteSketchFeature(IModelDoc2* doc, IFeature* sketchFeature)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (sketchFeature && SUCCEEDED(sketchFeature->Select2(VARIANT_FALSE, 0, &selected)) && selected)
		{
			doc->EditDelete();
			doc->ClearSelection2(VARIANT_TRUE);
		}
	}

	// Requests with more entities (or spline points) than this are refused.
	constexpr int kMaxSketchEntities = 1000;

	constexpr int kMaxSplinePoints = 1000;

	// Points closer than this (meters) count as the same point.
	constexpr double kSketchTolerance = 1e-9;

	// End points closer than this (meters) are merged after drawing, so
	// chains of entities connect into contours.
	constexpr double kMergeDistance = 1e-8;

	// Checks an entity's shape before SolidWorks is touched: the right
	// points are present and every number is finite. Geometric degeneracy is
	// checked when drawing, once points are in sketch coordinates.
	grpc::Status ValidateSketchEntity(const sld_mcp::v1::SketchEntity& entity)
	{
		using Entity = sld_mcp::v1::SketchEntity;
		auto invalid = [](const char* why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		auto point = [](bool has, const sld_mcp::v1::Vec3& p) { return has && IsFinite(p); };
		switch (entity.geometry_case())
		{
		case Entity::kLine:
			if (!point(entity.line().has_start(), entity.line().start()) ||
				!point(entity.line().has_end(), entity.line().end()))
				return invalid("a line needs a finite start and end");
			return grpc::Status::OK;
		case Entity::kCircle:
		{
			const double r = entity.circle().radius();
			if (!point(entity.circle().has_center(), entity.circle().center()))
				return invalid("a circle needs a finite center");
			if (!(std::isfinite(r) && r > kSketchTolerance))
				return invalid("a circle's radius must be positive");
			return grpc::Status::OK;
		}
		case Entity::kArc:
		{
			const auto& arc = entity.arc();
			if (!point(arc.has_start(), arc.start()) || !point(arc.has_end(), arc.end()))
				return invalid("an arc needs a finite start and end");
			if (arc.has_center() == arc.has_through())
				return invalid("an arc needs exactly one of center or through");
			if (!IsFinite(arc.has_center() ? arc.center() : arc.through()))
				return invalid("an arc's center/through point must be finite");
			if (arc.clockwise() && !arc.has_center())
				return invalid("clockwise only applies to arcs given by their center");
			return grpc::Status::OK;
		}
		case Entity::kSpline:
		{
			const auto& points = entity.spline().points();
			const int minPoints = entity.spline().closed() ? 3 : 2;
			if (points.size() < minPoints)
				return invalid(entity.spline().closed() ? "a closed spline needs at least 3 points"
				                                        : "a spline needs at least 2 points");
			if (points.size() > kMaxSplinePoints)
				return invalid("a spline can have at most 1000 points");
			for (const auto& p : points)
				if (!IsFinite(p))
					return invalid("spline points must be finite");
			return grpc::Status::OK;
		}
		case Entity::kPoint:
			if (!point(entity.point().has_position(), entity.point().position()))
				return invalid("a point needs a finite position");
			return grpc::Status::OK;
		default:
			return invalid("an entity has no geometry (line, circle, arc, spline or point)");
		}
	}

	// Maps request points into the open sketch's 2D coordinates (z = 0).
	struct SketchPointMapper
	{
		const SketchSpace& space;
		bool modelSpace;

		bool operator()(const sld_mcp::v1::Vec3& p, double out[3]) const
		{
			if (modelSpace)
			{
				const double in[3] = {p.x(), p.y(), p.z()};
				if (!space.ToSketch(in, out))
					return false;
			}
			else
			{
				out[0] = p.x();
				out[1] = p.y();
			}
			// Dropping the sketch z projects the point onto the sketch plane.
			out[2] = 0.0;
			return true;
		}
	};

	double Distance2D(const double a[3], const double b[3])
	{
		return std::hypot(a[0] - b[0], a[1] - b[1]);
	}

	// Draws one (validated) entity into the open sketch and returns what was
	// created: an ISketchSegment, or an ISketchPoint for points.
	grpc::Status DrawSketchEntity(ISketchManager* sketchManager, const SketchPointMapper& map,
		const sld_mcp::v1::SketchEntity& entity, ATL::CComPtr<IDispatch>& created)
	{
		using Entity = sld_mcp::v1::SketchEntity;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const grpc::Status mapFailed(grpc::StatusCode::INTERNAL,
			"couldn't map a point into the sketch's coordinates");

		ATL::CComPtr<ISketchSegment> segment;
		HRESULT hr = S_OK;
		std::string what;
		switch (entity.geometry_case())
		{
		case Entity::kLine:
		{
			what = "line";
			double s[3], e[3];
			if (!map(entity.line().start(), s) || !map(entity.line().end(), e))
				return mapFailed;
			if (Distance2D(s, e) < kSketchTolerance)
				return invalid("a line's start and end are the same point in the sketch plane");
			hr = sketchManager->CreateLine(s[0], s[1], 0.0, e[0], e[1], 0.0, &segment);
			break;
		}
		case Entity::kCircle:
		{
			what = "circle";
			double c[3];
			if (!map(entity.circle().center(), c))
				return mapFailed;
			hr = sketchManager->CreateCircleByRadius(c[0], c[1], 0.0, entity.circle().radius(), &segment);
			break;
		}
		case Entity::kArc:
		{
			what = "arc";
			const auto& arc = entity.arc();
			double s[3], e[3];
			if (!map(arc.start(), s) || !map(arc.end(), e))
				return mapFailed;
			if (arc.has_center())
			{
				double c[3];
				if (!map(arc.center(), c))
					return mapFailed;
				const double radius = Distance2D(s, c);
				const double toEnd = Distance2D(e, c);
				if (radius < kSketchTolerance || toEnd < kSketchTolerance)
					return invalid("an arc's start and end must both be away from its center");
				// The radius comes from the start; the end only gives the angle.
				for (int i = 0; i < 2; ++i)
					e[i] = c[i] + (e[i] - c[i]) * radius / toEnd;
				if (Distance2D(s, e) < kSketchTolerance)
					return invalid("an arc's start and end are at the same angle around its center -- "
					               "draw a circle for a full turn");
				// Direction: +1 counterclockwise, -1 clockwise.
				hr = sketchManager->CreateArc(c[0], c[1], 0.0, s[0], s[1], 0.0, e[0], e[1], 0.0,
					static_cast<short>(arc.clockwise() ? -1 : 1), &segment);
			}
			else
			{
				double t[3];
				if (!map(arc.through(), t))
					return mapFailed;
				const double chord = Distance2D(s, e);
				if (chord < kSketchTolerance)
					return invalid("an arc's start and end are the same point -- draw a circle instead");
				// Twice the triangle's area over its base = the through point's
				// distance from the chord.
				const double cross = (t[0] - s[0]) * (e[1] - s[1]) - (t[1] - s[1]) * (e[0] - s[0]);
				if (std::abs(cross) / chord < kSketchTolerance)
					return invalid("an arc's three points are in a straight line -- draw a line instead");
				// Start, end, then a point on the arc.
				hr = sketchManager->Create3PointArc(s[0], s[1], 0.0, e[0], e[1], 0.0, t[0], t[1], 0.0, &segment);
			}
			break;
		}
		case Entity::kSpline:
		{
			what = "spline";
			const auto& spline = entity.spline();
			std::vector<double> data;
			data.reserve(static_cast<size_t>(spline.points_size() + 1) * 3);
			double first[3] = {};
			double previous[3] = {};
			for (int i = 0; i < spline.points_size(); ++i)
			{
				double p[3];
				if (!map(spline.points(i), p))
					return mapFailed;
				if (i > 0 && Distance2D(p, previous) < kSketchTolerance)
					return invalid("a spline has two consecutive points at the same place in the sketch plane");
				if (i == 0)
					std::copy(p, p + 3, first);
				std::copy(p, p + 3, previous);
				data.insert(data.end(), p, p + 3);
			}
			// A spline whose last point is its first is closed.
			if (spline.closed() && Distance2D(previous, first) >= kSketchTolerance)
				data.insert(data.end(), first, first + 3);
			ATL::CComVariant points = comvariant::FromDoubles(data.data(), data.size());
			hr = sketchManager->CreateSpline(points, &segment);
			break;
		}
		case Entity::kPoint:
		{
			double p[3];
			if (!map(entity.point().position(), p))
				return mapFailed;
			ATL::CComPtr<ISketchPoint> point;
			hr = sketchManager->CreatePoint(p[0], p[1], 0.0, &point);
			if (FAILED(hr))
				return ComError("ISketchManager::CreatePoint", hr);
			if (!point)
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the point");
			created = point.p;
			return grpc::Status::OK;
		}
		default:
			return invalid("an entity has no geometry");
		}

		if (FAILED(hr))
			return ComError(("ISketchManager creating a " + what).c_str(), hr);
		if (!segment)
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks didn't create the " + what + " (degenerate geometry?)");
		if (entity.construction())
			segment->put_ConstructionGeometry(VARIANT_TRUE);
		created = segment.p;
		return grpc::Status::OK;
	}

	// Deletes entities drawn into the still-open sketch. Best effort.
	void DeleteDrawnEntities(IModelDoc2* doc, const std::vector<ATL::CComPtr<IDispatch>>& entities)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
		for (const auto& entity : entities)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (ATL::CComQIPtr<ISketchSegment> segment(entity.p); segment)
				segment->Select4(VARIANT_TRUE, nullptr, &selected);
			else if (ATL::CComQIPtr<ISketchPoint> point(entity.p); point)
				point->Select4(VARIANT_TRUE, nullptr, &selected);
			any = any || selected;
		}
		if (any)
			doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
	}

	// Sets the sketch persist ID on every ref in an item, for items described
	// before the sketch's persist ID was known.
	void StampSketchId(sld_mcp::v1::SketchItem* item, const std::string& sketchPid)
	{
		item->mutable_ref()->set_sketch_persist_id(sketchPid);
		if (item->has_start_point())
			item->mutable_start_point()->set_sketch_persist_id(sketchPid);
		if (item->has_end_point())
			item->mutable_end_point()->set_sketch_persist_id(sketchPid);
		if (item->has_center_point())
			item->mutable_center_point()->set_sketch_persist_id(sketchPid);
	}

	void DescribeRelation(ISketchRelation* relation, const std::string& sketchPid, const SketchItems& items,
		sld_mcp::v1::RelationInfo* info)
	{
		long type = 0;
		if (SUCCEEDED(relation->GetRelationType(&type)))
			info->set_constraint_type(static_cast<int32_t>(type));
		ATL::CComVariant entities;
		if (FAILED(relation->GetEntities(&entities)))
			return;
		for (const auto& entity : comvariant::ToDispatches(entities))
		{
			sld_mcp::v1::SketchItemRef ref;
			if (entity && items.Contains(entity) && SetItemRef(entity, sketchPid, &ref))
				*info->add_items() = ref;
			else
				info->set_external_count(info->external_count() + 1);
		}
	}
}

grpc::Status SolidWorksService::CreateRectangleSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CreateRectangleSketchRequest* request,
	sld_mcp::v1::CreateRectangleSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool onEntity = !request->on_persist_id().empty();
	int planeIndex = 0;
	if (!onEntity && !StandardPlaneIndex(request->plane(), planeIndex))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
	const double halfWidth = request->width() / 2.0;
	const double halfHeight = request->height() / 2.0;
	if (!(std::isfinite(halfWidth) && halfWidth > 0.0 && std::isfinite(halfHeight) && halfHeight > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "width and height must be positive");

	// The center is either 2D sketch coordinates or a model-space point that
	// gets projected onto the sketch plane once the sketch exists.
	double cx = request->center_x();
	double cy = request->center_y();
	bool haveModelCenter = request->has_center_model();
	double modelCenter[3] = {};
	if (haveModelCenter)
	{
		modelCenter[0] = request->center_model().x();
		modelCenter[1] = request->center_model().y();
		modelCenter[2] = request->center_model().z();
		if (!(std::isfinite(modelCenter[0]) && std::isfinite(modelCenter[1]) && std::isfinite(modelCenter[2])))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "center must be finite");
	}
	else if (!(std::isfinite(cx) && std::isfinite(cy)))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "center must be finite");

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

	// What to sketch on: a planar face, a reference plane feature, or one of
	// the three default planes.
	SketchTarget target;
	if (grpc::Status status = ResolveSketchTarget(doc, request->on_persist_id(), planeIndex, target); !status.ok())
		return status;

	// On a face, default to the middle of the face (well, of its bounding box).
	HRESULT hr = S_OK;
	if (target.face && !haveModelCenter)
	{
		ATL::CComVariant box;
		hr = target.face->GetBox(&box);
		if (FAILED(hr))
			return ComError("IFace2::GetBox", hr);
		const std::vector<double> b = comvariant::ToDoubles(box);
		if (b.size() < 6)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no box for the face");
		for (int i = 0; i < 3; ++i)
			modelCenter[i] = (b[i] + b[i + 3]) / 2.0;
		haveModelCenter = true;
	}

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<ISketchManager> sketchManager;
	hr = doc->get_SketchManager(&sketchManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::SketchManager", hr);
	if (!sketchManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenNewSketch(doc, sketchManager, target, sketch); !status.ok())
		return status;

	// The sketch is now open: everything below must fall through to the
	// InsertSketch call that closes it, so no early returns until then.
	grpc::Status failure = grpc::Status::OK;
	space.Attach(sketch);

	if (haveModelCenter)
	{
		// Dropping the sketch Z (the distance from the plane) projects the
		// point onto the sketch plane.
		double sketchCenter[3];
		if (space.ToSketch(modelCenter, sketchCenter))
		{
			cx = sketchCenter[0];
			cy = sketchCenter[1];
		}
		else
			failure = grpc::Status(grpc::StatusCode::INTERNAL,
				"couldn't map the center point into the sketch's coordinates");
	}

	// AddToDB skips snapping and inferencing, so the rectangle lands exactly
	// at the requested coordinates instead of snapping to nearby geometry.
	VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
	sketchManager->get_AddToDB(&previousAddToDB);
	sketchManager->put_AddToDB(VARIANT_TRUE);

	// Center point, then one corner; sketch coordinates in meters. Returns
	// the created segments, or nothing if SolidWorks rejected the geometry.
	ATL::CComVariant segments;
	HRESULT createHr = S_OK;
	bool created = false;
	if (failure.ok())
	{
		createHr = sketchManager->CreateCenterRectangle(
			cx, cy, 0.0, cx + halfWidth, cy + halfHeight, 0.0, &segments);
		created = SUCCEEDED(createHr) && segments.vt != VT_EMPTY && segments.vt != VT_NULL;
		if (FAILED(createHr))
			failure = ComError("ISketchManager::CreateCenterRectangle", createHr);
		else if (!created)
			failure = grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the rectangle");
	}

	sketchManager->put_AddToDB(previousAddToDB);

	// Report the frame and where the center actually landed. Purely
	// informational, so a failure here just leaves them out.
	if (created)
	{
		// The sides (and any construction geometry), with IDs for
		// dimensioning; stamped with the sketch's persist ID once it's closed.
		for (const auto& segment : comvariant::ToDispatches(segments))
		{
			sld_mcp::v1::SketchItem item;
			if (segment && DescribeSketchItem(segment, std::string(), &item))
				response->add_items()->Swap(&item);
		}

		sld_mcp::v1::SketchFrame frame;
		const double sketchCenter[3] = {cx, cy, 0.0};
		double center[3];
		if (space.Frame(frame) && space.ToModel(sketchCenter, center))
		{
			*response->mutable_frame() = frame;
			SetVec3(response->mutable_center_model(), center);
		}
	}

	// An ISketch is also its IFeature in the tree; read the name while it's
	// still the active sketch.
	ATL::CComQIPtr<IFeature> sketchFeature(sketch.p);
	ATL::CComBSTR sketchName;
	if (sketchFeature)
		sketchFeature->get_Name(&sketchName);

	// Close the sketch (and rebuild).
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	doc->ClearSelection2(VARIANT_TRUE);

	if (!created)
	{
		// Don't leave an empty sketch behind.
		if (SUCCEEDED(closeHr))
			DeleteSketchFeature(doc, sketchFeature);
		return failure;
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (auto& item : *response->mutable_items())
		StampSketchId(&item, sketchPid);
	response->set_sketch_name(ToUtf8(sketchName));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddSketchEntities(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddSketchEntitiesRequest* request,
	sld_mcp::v1::AddSketchEntitiesResponse* response)
{
	// Validate before touching SolidWorks.
	const bool existing = !request->sketch_name().empty();
	const bool onEntity = !request->on_persist_id().empty();
	int planeIndex = 0;
	if (existing)
	{
		if (onEntity || request->plane() != sld_mcp::v1::PLANE_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"give either an existing sketch_name or where to put a new sketch, not both");
	}
	else if (!onEntity && !StandardPlaneIndex(request->plane(), planeIndex))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
	if (request->entities_size() == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "nothing to draw -- give at least one entity");
	if (request->entities_size() > kMaxSketchEntities)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many entities in one call (max 1000)");
	for (int i = 0; i < request->entities_size(); ++i)
		if (grpc::Status status = ValidateSketchEntity(request->entities(i)); !status.ok())
			return grpc::Status(status.error_code(),
				"entity " + std::to_string(i + 1) + ": " + status.error_message());

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

	// Resolve everything that can fail before a sketch is opened.
	ATL::CComPtr<IFeature> sketchFeature;
	SketchTarget target;
	if (existing)
	{
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
			return status;
	}
	else if (grpc::Status status = ResolveSketchTarget(doc, request->on_persist_id(), planeIndex, target);
		!status.ok())
		return status;

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;

	ATL::CComPtr<ISketchManager> sketchManager;
	HRESULT hr = doc->get_SketchManager(&sketchManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::SketchManager", hr);
	if (!sketchManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	// Open the sketch: re-enter the existing one, or create one.
	ATL::CComPtr<ISketch> sketch;
	if (existing)
	{
		if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
			return status;
	}
	else
	{
		if (grpc::Status status = OpenNewSketch(doc, sketchManager, target, sketch); !status.ok())
			return status;
		sketchFeature = ATL::CComQIPtr<IFeature>(sketch.p);
	}

	// The sketch is now open: everything below must fall through to the
	// InsertSketch call that closes it, so no early returns until then.
	space.Attach(sketch);
	const SketchPointMapper map{space, request->model_space()};

	// AddToDB skips snapping and inferencing, so entities land exactly at the
	// requested coordinates instead of snapping to nearby geometry.
	VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
	sketchManager->get_AddToDB(&previousAddToDB);
	sketchManager->put_AddToDB(VARIANT_TRUE);

	grpc::Status failure = grpc::Status::OK;
	std::vector<ATL::CComPtr<IDispatch>> created;
	for (int i = 0; i < request->entities_size() && failure.ok(); ++i)
	{
		ATL::CComPtr<IDispatch> entity;
		failure = DrawSketchEntity(sketchManager, map, request->entities(i), entity);
		if (failure.ok())
			created.push_back(entity);
		else
			failure = grpc::Status(failure.error_code(),
				"entity " + std::to_string(i + 1) + ": " + failure.error_message());
	}

	sketchManager->put_AddToDB(previousAddToDB);

	if (failure.ok())
	{
		// With AddToDB on, entities that meet end to end each keep their own
		// end point; merging them is what connects them into contours.
		VARIANT_BOOL merged = VARIANT_FALSE;
		sketch->MergePoints(kMergeDistance, &merged);

		// IDs for later dimensioning. Merging can delete a drawn point (it's
		// folded into another), so describe after it, skipping what's gone.
		for (const auto& entity : created)
		{
			sld_mcp::v1::SketchItem item;
			if (DescribeSketchItem(entity, std::string(), &item))
				response->add_items()->Swap(&item);
		}

		sld_mcp::v1::SketchFrame frame;
		if (space.Frame(frame))
			*response->mutable_frame() = frame;
	}
	else if (existing)
	{
		// All or nothing: take back what this call drew.
		DeleteDrawnEntities(doc, created);
	}

	ATL::CComBSTR sketchName;
	if (sketchFeature)
		sketchFeature->get_Name(&sketchName);

	// Close the sketch (and rebuild).
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	doc->ClearSelection2(VARIANT_TRUE);

	if (!failure.ok())
	{
		// Don't leave a half-drawn new sketch behind.
		if (!existing && SUCCEEDED(closeHr))
			DeleteSketchFeature(doc, sketchFeature);
		return failure;
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	// After the rebuild, so contours reflect the merged points.
	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;

	// Read once the sketch is closed, so it's the ID later calls compare to.
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (auto& item : *response->mutable_items())
		StampSketchId(&item, sketchPid);

	response->set_sketch_name(ToUtf8(sketchName));
	response->set_created_sketch(!existing);
	response->set_entity_count(static_cast<int32_t>(created.size()));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::InspectSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectSketchRequest* request,
	sld_mcp::v1::InspectSketchResponse* response)
{
	// Read-only, but must not see a half-finished edit (see InspectModel).
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetActivePart(swApp, doc); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<IFeature> feature;
	if (grpc::Status status = ResolveSketchFeature(doc, request->sketch_name(), feature); !status.ok())
		return status;
	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = SketchOf(feature, sketch); !status.ok())
		return status;
	const std::string sketchName = FeatureName(feature);
	const std::string sketchPid = PersistIdOf(ext, feature);
	response->set_sketch_name(sketchName);

	SketchSpace space;
	if (space.Init(swApp).ok())
	{
		space.Attach(sketch);
		sld_mcp::v1::SketchFrame frame;
		if (space.Frame(frame))
			*response->mutable_frame() = frame;
	}

	response->set_status(SketchStatus(sketch));

	SketchItems items;
	items.Load(sketch);
	for (const auto& entry : items.All())
	{
		if (static_cast<size_t>(response->items_size()) >= kMaxSketchItems)
		{
			response->set_truncated(true);
			break;
		}
		sld_mcp::v1::SketchItem item;
		if (DescribeSketchItem(entry.obj, sketchPid, &item))
			response->add_items()->Swap(&item);
	}

	for (const auto& display : FeatureDimensions(feature))
	{
		sld_mcp::v1::DimensionInfo info;
		if (DescribeDimension(display, &info))
			response->add_dimensions()->Swap(&info);
	}

	for (const auto& relation : SketchRelations(sketch))
	{
		long type = 0;
		// Dimensions show up as relations too; they're listed above.
		if (SUCCEEDED(relation->GetRelationType(&type)) &&
			(type == swConstraintType_DISTANCE || type == swConstraintType_ANGLE ||
			 type == swConstraintType_RADIUS || type == swConstraintType_DIAMETER))
			continue;
		DescribeRelation(relation, sketchPid, items, response->add_relations());
	}

	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;
	return grpc::Status::OK;
}
