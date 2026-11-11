// Sketches.cpp : Drawing sketches: CreateRectangleSketch, AddSketchEntities, InspectSketch.
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// Requests with more entities (or spline points) than this are refused.
	constexpr int kMaxSketchEntities = 1000;

	constexpr int kMaxSplinePoints = 1000;

	// Points closer than this (meters) count as the same point.
	constexpr double kSketchTolerance = 1e-9;

	// End points closer than this (meters) are merged after drawing, so
	// chains of entities connect into contours.
	constexpr double kMergeDistance = 1e-8;

	// Width and height dimensions on a new center rectangle's top and right
	// sides (CreateCenterRectangle makes none), placed outside it, in the
	// open sketch. A side that won't take one is left without.
	void DimensionRectangle(ISldWorks* swApp, IModelDoc2* doc, const SketchSpace& space, const ATL::CComVariant& segments,
		double cx, double cy, double halfWidth, double halfHeight, sld_mcp::v1::CreateRectangleSketchResponse* response)
	{
		PreferenceToggles toggles(swApp);
		toggles.Set(swInputDimValOnCreate, false);
		toggles.Set(swSketchOverdefiningDimsPromptToSetState, false);
		toggles.Set(swSketchOverdefiningDimsSetDrivenByDefault, true);
		const double gap = std::max(0.005, 0.3 * std::min(halfWidth, halfHeight));
		struct Side { double x, y; bool horizontal; };
		for (const Side side : { Side{ cx, cy + halfHeight, true }, Side{ cx + halfWidth, cy, false } })
			for (const auto& disp : comvariant::ToDispatches(segments))
			{
				ATL::CComQIPtr<ISketchSegment> segment(disp.p);
				ATL::CComQIPtr<ISketchLine> line(disp.p);
				VARIANT_BOOL construction = VARIANT_TRUE;
				if (!segment || !line || FAILED(segment->get_ConstructionGeometry(&construction)) || construction)
					continue;
				ATL::CComPtr<IDispatch> startDisp, endDisp;
				line->GetStartPoint2(&startDisp);
				line->GetEndPoint2(&endDisp);
				ATL::CComQIPtr<ISketchPoint> start(startDisp.p), end(endDisp.p);
				double sx = 0, sy = 0, ex = 0, ey = 0;
				if (!start || !end || FAILED(start->get_X(&sx)) || FAILED(start->get_Y(&sy)) ||
					FAILED(end->get_X(&ex)) || FAILED(end->get_Y(&ey)) ||
					std::abs((sx + ex) / 2 - side.x) > 1e-6 || std::abs((sy + ey) / 2 - side.y) > 1e-6)
					continue;
				const double textSketch[3] = { side.x + (side.horizontal ? 0.0 : gap), side.y + (side.horizontal ? gap : 0.0), 0.0 };
				double text[3];
				VARIANT_BOOL selected = VARIANT_FALSE;
				ATL::CComPtr<IDispatch> created;
				doc->ClearSelection2(VARIANT_TRUE);
				if (space.ToModel(textSketch, text) && SUCCEEDED(segment->Select4(VARIANT_FALSE, nullptr, &selected)) &&
					selected && SUCCEEDED(doc->AddDimension2(text[0], text[1], text[2], &created)))
					if (ATL::CComQIPtr<IDisplayDimension> display(created.p); display)
						DescribeDimension(display, response->add_dimensions());
				doc->ClearSelection2(VARIANT_TRUE);
				break;
			}
	}

	constexpr int kMinPolygonSides = 3;

	// What SolidWorks' polygon tool goes up to.
	constexpr int kMaxPolygonSides = 40;

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
		case Entity::kSlot:
		{
			const auto& slot = entity.slot();
			if (!point(slot.has_start(), slot.start()) || !point(slot.has_end(), slot.end()))
				return invalid("a slot needs a finite start and end");
			if (slot.has_center() && slot.has_through())
				return invalid("a slot along an arc needs one of center or through, not both");
			if ((slot.has_center() && !IsFinite(slot.center())) || (slot.has_through() && !IsFinite(slot.through())))
				return invalid("a slot's center/through point must be finite");
			if (slot.clockwise() && !slot.has_center())
				return invalid("clockwise only applies to slots along an arc given by its center");
			if (!(std::isfinite(slot.width()) && slot.width() > kSketchTolerance))
				return invalid("a slot's width must be positive");
			return grpc::Status::OK;
		}
		case Entity::kPolygon:
		{
			const auto& polygon = entity.polygon();
			if (!point(polygon.has_center(), polygon.center()))
				return invalid("a polygon needs a finite center");
			if (polygon.sides() < kMinPolygonSides || polygon.sides() > kMaxPolygonSides)
				return invalid("a polygon needs 3 to 40 sides");
			if (!(std::isfinite(polygon.corner_radius()) && polygon.corner_radius() > kSketchTolerance))
				return invalid("a polygon's radius must be positive");
			if (!std::isfinite(polygon.angle()))
				return invalid("a polygon's angle must be finite");
			return grpc::Status::OK;
		}
		case Entity::kEllipse:
		{
			const auto& ellipse = entity.ellipse();
			if (!point(ellipse.has_center(), ellipse.center()))
				return invalid("an ellipse needs a finite center");
			if (!(std::isfinite(ellipse.major_radius()) && std::isfinite(ellipse.minor_radius()) &&
				ellipse.minor_radius() > kSketchTolerance && ellipse.major_radius() >= ellipse.minor_radius()))
				return invalid("an ellipse needs a positive minor radius, not more than its major radius");
			if (!std::isfinite(ellipse.angle()))
				return invalid("an ellipse's angle must be finite");
			return grpc::Status::OK;
		}
		default:
			return invalid("an entity has no geometry (line, circle, arc, spline, point, slot, polygon or ellipse)");
		}
	}

	// Maps request points into the open sketch's coordinates: 2D (z = 0), or
	// in a 3D sketch the model coordinates they came in.
	struct SketchPointMapper
	{
		const SketchSpace& space;
		bool modelSpace;
		bool is3D;

		bool operator()(const sld_mcp::v1::Vec3& p, double out[3]) const
		{
			if (is3D)
			{
				out[0] = p.x();
				out[1] = p.y();
				out[2] = p.z();
				return true;
			}
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

	// Between two mapped points: 3D sketches use all of it, in 2D z is 0.
	double Distance(const double a[3], const double b[3])
	{
		return (Eigen::Vector3d(a[0], a[1], a[2]) - Eigen::Vector3d(b[0], b[1], b[2])).norm();
	}

	// Draws a slot. SolidWorks makes its segments, a construction center line or
	// arc, and a point in the middle.
	grpc::Status DrawSlot(ISketchManager* sketchManager, const SketchPointMapper& map,
		const sld_mcp::v1::SketchSlot& slot)
	{
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const grpc::Status mapFailed(grpc::StatusCode::INTERNAL,
			"couldn't map a point into the sketch's coordinates");
		double s[3], e[3];
		if (!map(slot.start(), s) || !map(slot.end(), e))
			return mapFailed;
		if (Distance(s, e) < kSketchTolerance)
			return invalid("a slot's start and end are the same point in the sketch plane");
		long type = swSketchSlotCreationType_line;
		long direction = 1;
		// What the three points are depends on the type: start, end, unused;
		// center, start, end; or start, end, a point between them.
		double p1[3] = {s[0], s[1], 0.0}, p2[3] = {e[0], e[1], 0.0}, p3[3] = {};
		if (slot.has_center())
		{
			double c[3];
			if (!map(slot.center(), c))
				return mapFailed;
			const double radius = Distance(s, c);
			const double toEnd = Distance(e, c);
			if (radius < kSketchTolerance || toEnd < kSketchTolerance)
				return invalid("a slot's start and end must both be away from the center of its arc");
			if (radius <= slot.width() / 2.0)
				return invalid("a slot along an arc must be narrower than twice the arc's radius");
			// As for arcs: the radius comes from the start.
			for (int i = 0; i < 2; ++i)
				e[i] = c[i] + (e[i] - c[i]) * radius / toEnd;
			if (Distance(s, e) < kSketchTolerance)
				return invalid("a slot's start and end are at the same angle around its center");
			type = swSketchSlotCreationType_arc;
			direction = slot.clockwise() ? -1 : 1;
			std::copy(c, c + 3, p1);
			std::copy(s, s + 3, p2);
			std::copy(e, e + 3, p3);
		}
		else if (slot.has_through())
		{
			if (!map(slot.through(), p3))
				return mapFailed;
			const Eigen::Vector3d start(s[0], s[1], s[2]);
			const double cross = (Eigen::Vector3d(p3[0], p3[1], p3[2]) - start).cross(Eigen::Vector3d(e[0], e[1], e[2]) - start).norm();
			if (cross / Distance(s, e) < kSketchTolerance)
				return invalid("a slot's three points are in a straight line -- leave out through for a straight slot");
			type = swSketchSlotCreationType_3pointarc;
		}
		ATL::CComPtr<ISketchSlot> created;
		const HRESULT hr = sketchManager->CreateSketchSlot(type, swSketchSlotLengthType_CenterCenter, slot.width(),
			p1[0], p1[1], 0.0, p2[0], p2[1], 0.0, p3[0], p3[1], 0.0, direction,
			slot.add_dimensions() ? VARIANT_TRUE : VARIANT_FALSE, &created);
		if (FAILED(hr))
			return ComError("ISketchManager::CreateSketchSlot", hr);
		if (!created)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't create the slot -- e.g. it's wider than the arc it follows can take");
		return grpc::Status::OK;
	}

	grpc::Status DrawPolygon(ISketchManager* sketchManager, const SketchPointMapper& map,
		const sld_mcp::v1::SketchPolygon& polygon)
	{
		double c[3];
		if (!map(polygon.center(), c))
			return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't map a point into the sketch's coordinates");
		// The point is a corner whichever construction circle it gets (seen
		// 2026-10-31).
		ATL::CComVariant segments;
		const HRESULT hr = sketchManager->CreatePolygon(c[0], c[1], 0.0,
			c[0] + polygon.corner_radius() * std::cos(polygon.angle()),
			c[1] + polygon.corner_radius() * std::sin(polygon.angle()), 0.0,
			polygon.sides(), polygon.inscribed() ? VARIANT_TRUE : VARIANT_FALSE, &segments);
		if (FAILED(hr))
			return ComError("ISketchManager::CreatePolygon", hr);
		if (comvariant::ToDispatches(segments).empty())
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the polygon");
		return grpc::Status::OK;
	}

	// Draws one (validated) entity into the open sketch and appends what was
	// created: ISketchSegments, or an ISketchPoint for points.
	grpc::Status DrawSketchEntity(ISketch* sketch, ISketchManager* sketchManager, const SketchPointMapper& map,
		const sld_mcp::v1::SketchEntity& entity, std::vector<ATL::CComPtr<IDispatch>>& created)
	{
		using Entity = sld_mcp::v1::SketchEntity;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const grpc::Status mapFailed(grpc::StatusCode::INTERNAL,
			"couldn't map a point into the sketch's coordinates");

		// One entity, many segments: what's new is what counts, also what these
		// calls don't return (a slot gives an ISketchSlot).
		if (entity.geometry_case() == Entity::kSlot || entity.geometry_case() == Entity::kPolygon)
		{
			if (map.is3D)
				return invalid(entity.has_slot() ? "a 3D sketch can't take a slot (it wouldn't know which way to face)"
				                                 : "a 3D sketch can't take a polygon (it wouldn't know which way to face)");
			SketchItems items;
			items.Load(sketch);
			const std::set<SketchItemKey> before = SketchKeys(items);
			if (grpc::Status status = entity.has_slot() ? DrawSlot(sketchManager, map, entity.slot())
			                                            : DrawPolygon(sketchManager, map, entity.polygon());
				!status.ok())
				return status;
			items.Load(sketch);
			std::set<SketchItemKey> onSegment;
			for (const auto& e : items.All())
			{
				sld_mcp::v1::SketchItem item;
				if (e.point || before.count(SketchKeyOf(e)) || !DescribeSketchItem(e.obj, std::string(), &item))
					continue;
				for (const sld_mcp::v1::SketchItemRef* p : {&item.start_point(), &item.end_point(), &item.center_point()})
					if (p->point())
						onSegment.insert(SketchKeyOf(*p));
				if (ATL::CComQIPtr<ISketchSegment> segment(e.obj.p); segment && entity.construction())
					segment->put_ConstructionGeometry(VARIANT_TRUE);
				created.push_back(e.obj);
			}
			// A slot's middle, to dimension it by.
			for (const auto& e : items.All())
				if (e.point && !before.count(SketchKeyOf(e)) && !onSegment.count(SketchKeyOf(e)))
					created.push_back(e.obj);
			return grpc::Status::OK;
		}

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
			if (Distance(s, e) < kSketchTolerance)
				return invalid(map.is3D ? "a line's start and end are the same point"
				                        : "a line's start and end are the same point in the sketch plane");
			hr = sketchManager->CreateLine(s[0], s[1], s[2], e[0], e[1], e[2], &segment);
			break;
		}
		case Entity::kCircle:
		{
			what = "circle";
			// CreateCircleByRadius has no normal: it would lie level with model
			// XY, whatever was meant (seen 2026-10-31).
			if (map.is3D)
				return invalid("a 3D sketch can't take a circle (it wouldn't know which way to face) -- "
				               "draw it in a 2D sketch on a plane, or two 3-point arcs");
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
				if (map.is3D)
					return invalid("in a 3D sketch an arc is given by a point it passes `through`, which "
					               "settles its plane and direction, not by its center");
				double c[3];
				if (!map(arc.center(), c))
					return mapFailed;
				const double radius = Distance(s, c);
				const double toEnd = Distance(e, c);
				if (radius < kSketchTolerance || toEnd < kSketchTolerance)
					return invalid("an arc's start and end must both be away from its center");
				// The radius comes from the start; the end only gives the angle.
				for (int i = 0; i < 2; ++i)
					e[i] = c[i] + (e[i] - c[i]) * radius / toEnd;
				if (Distance(s, e) < kSketchTolerance)
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
				const double chord = Distance(s, e);
				if (chord < kSketchTolerance)
					return invalid("an arc's start and end are the same point -- draw a circle instead");
				// Twice the triangle's area over its base = the through point's
				// distance from the chord.
				const Eigen::Vector3d start(s[0], s[1], s[2]);
				const double cross = (Eigen::Vector3d(t[0], t[1], t[2]) - start).cross(Eigen::Vector3d(e[0], e[1], e[2]) - start).norm();
				if (cross / chord < kSketchTolerance)
					return invalid("an arc's three points are in a straight line -- draw a line instead");
				// Start, end, then a point on the arc.
				hr = sketchManager->Create3PointArc(s[0], s[1], s[2], e[0], e[1], e[2], t[0], t[1], t[2], &segment);
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
				if (i > 0 && Distance(p, previous) < kSketchTolerance)
					return invalid(map.is3D ? "a spline has two consecutive points at the same place"
					                        : "a spline has two consecutive points at the same place in the sketch plane");
				if (i == 0)
					std::copy(p, p + 3, first);
				std::copy(p, p + 3, previous);
				data.insert(data.end(), p, p + 3);
			}
			// A spline whose last point is its first is closed.
			if (spline.closed() && Distance(previous, first) >= kSketchTolerance)
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
			hr = sketchManager->CreatePoint(p[0], p[1], p[2], &point);
			if (FAILED(hr))
				return ComError("ISketchManager::CreatePoint", hr);
			if (!point)
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the point");
			created.push_back(point.p);
			return grpc::Status::OK;
		}
		case Entity::kEllipse:
		{
			what = "ellipse";
			if (map.is3D)
				return invalid("a 3D sketch can't take an ellipse (it wouldn't know which way to face)");
			const auto& ellipse = entity.ellipse();
			double c[3];
			if (!map(ellipse.center(), c))
				return mapFailed;
			const double ca = std::cos(ellipse.angle());
			const double sa = std::sin(ellipse.angle());
			hr = sketchManager->CreateEllipse(c[0], c[1], 0.0,
				c[0] + ellipse.major_radius() * ca, c[1] + ellipse.major_radius() * sa, 0.0,
				c[0] - ellipse.minor_radius() * sa, c[1] + ellipse.minor_radius() * ca, 0.0, &segment);
			break;
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
		created.push_back(segment.p);
		return grpc::Status::OK;
	}

	// Deletes entities drawn into the still-open sketch. Best effort.
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

grpc::Status SolidWorksService::CreateRectangleSketch(grpc::ServerContext* context,
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

	return m_executor.Execute(context, "CreateRectangleSketch", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
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

		if (created)
			DimensionRectangle(swApp, doc, space, segments, cx, cy, halfWidth, halfHeight, response);

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
	});
}

grpc::Status SolidWorksService::AddSketchEntities(grpc::ServerContext* context,
	const sld_mcp::v1::AddSketchEntitiesRequest* request,
	sld_mcp::v1::AddSketchEntitiesResponse* response)
{
	// Validate before touching SolidWorks.
	const bool existing = !request->sketch_name().empty();
	const bool onEntity = !request->on_persist_id().empty();
	const bool new3D = request->sketch_3d();
	int planeIndex = 0;
	if (existing)
	{
		if (onEntity || new3D || request->plane() != sld_mcp::v1::PLANE_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"give either an existing sketch_name or where to put a new sketch, not both");
	}
	else if (new3D)
	{
		if (onEntity || request->plane() != sld_mcp::v1::PLANE_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a 3D sketch isn't on a plane or face");
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

	return m_executor.Execute(context, "AddSketchEntities", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;

		// Resolve everything that can fail before a sketch is opened.
		ATL::CComPtr<IFeature> sketchFeature;
		SketchTarget target;
		if (existing)
		{
			if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature, true); !status.ok())
				return status;
		}
		else if (!new3D)
		{
			if (grpc::Status status = ResolveSketchTarget(doc, request->on_persist_id(), planeIndex, target);
				!status.ok())
				return status;
		}

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
			if (grpc::Status status = new3D ? OpenNew3DSketch(doc, sketchManager, sketch)
			                                : OpenNewSketch(doc, sketchManager, target, sketch);
				!status.ok())
				return status;
			sketchFeature = ATL::CComQIPtr<IFeature>(sketch.p);
		}

		// The sketch is now open: everything below must fall through to the
		// InsertSketch call that closes it, so no early returns until then.
		space.Attach(sketch);
		const bool is3D = new3D || IsSketch3D(sketch);
		const SketchPointMapper map{space, request->model_space(), is3D};

		// AddToDB skips snapping and inferencing, so entities land exactly at the
		// requested coordinates instead of snapping to nearby geometry.
		VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
		sketchManager->get_AddToDB(&previousAddToDB);
		sketchManager->put_AddToDB(VARIANT_TRUE);

		grpc::Status failure = grpc::Status::OK;
		if (is3D && !request->model_space())
			failure = grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a 3D sketch has no 2D coordinates -- give points as [x, y, z] in model coordinates");
		std::vector<ATL::CComPtr<IDispatch>> created;
		for (int i = 0; i < request->entities_size() && failure.ok(); ++i)
		{
			failure = DrawSketchEntity(sketch, sketchManager, map, request->entities(i), created);
			if (!failure.ok())
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
			if (!is3D && space.Frame(frame))
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
		const HRESULT closeHr = is3D ? sketchManager->Insert3DSketch(VARIANT_TRUE)
		                             : sketchManager->InsertSketch(VARIANT_TRUE);
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
		response->set_is_3d(is3D);
		response->set_entity_count(request->entities_size());
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::InspectSketch(grpc::ServerContext* context,
	const sld_mcp::v1::InspectSketchRequest* request,
	sld_mcp::v1::InspectSketchResponse* response)
{
	// Read-only, but must not see a half-finished edit (see InspectModel).
	return m_executor.Execute(context, "InspectSketch", ExecutionPolicy::Cancellable,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetActivePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		ATL::CComPtr<IFeature> feature;
		if (grpc::Status status = ResolveSketchFeature(doc, request->sketch_name(), feature, true); !status.ok())
			return status;
		ATL::CComPtr<ISketch> sketch;
		if (grpc::Status status = SketchOf(feature, sketch); !status.ok())
			return status;
		const std::string sketchName = FeatureName(feature);
		const std::string sketchPid = PersistIdOf(ext, feature);
		response->set_sketch_name(sketchName);

		const bool is3D = IsSketch3D(sketch);
		response->set_is_3d(is3D);
		SketchSpace space;
		if (!is3D && space.Init(swApp).ok())
		{
			space.Attach(sketch);
			sld_mcp::v1::SketchFrame frame;
			if (space.Frame(frame))
				*response->mutable_frame() = frame;
		}

		response->set_status(SketchStatus(sketch));

		SketchItems items;
		items.Load(sketch);
		const ArcAxisPoints hidden(sketch, items);
		for (const auto& entry : items.All())
		{
			if (static_cast<size_t>(response->items_size()) >= kMaxSketchItems)
			{
				response->set_truncated(true);
				break;
			}
			if (entry.point && hidden.Contains(entry.obj))
				continue;
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
	});
}
