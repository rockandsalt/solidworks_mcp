// Pattern.cpp : PatternFeatures (linear and circular feature / body patterns) and
// PatternSketchEntities (linear and circular sketch patterns).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SeedHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr double kPi = 3.14159265358979323846;

	// The selection marks FeatureLinearPattern5 / FeatureCircularPattern5 read
	// their inputs from (the circular pattern's axis is direction 1).
	constexpr long kDirection1Mark = 1;
	constexpr long kDirection2Mark = 2;
	constexpr long kSeedFeatureMark = 4;
	constexpr long kSeedBodyMark = 256;

	// More instances than anyone patterns in one go (count, or count x count2).
	constexpr long long kMaxInstances = 10000;
	constexpr int kMaxPatternSketchItems = 1000;

	// Where a circular sketch pattern's copies may be off from where they
	// should be, meters.
	constexpr double kPositionTolerance = 1e-6;

	bool IsStraight(IEdge* edge)
	{
		ATL::CComPtr<ICurve> curve;
		VARIANT_BOOL line = VARIANT_FALSE;
		return SUCCEEDED(edge->IGetCurve(&curve)) && curve && SUCCEEDED(curve->IsLine(&line)) && line;
	}

	bool IsLine(const Pick& pick)
	{
		ATL::CComQIPtr<ISketchSegment> segment(pick.obj.p);
		long type = -1;
		return segment && SUCCEEDED(segment->GetType(&type)) && type == swSketchLINE;
	}

	// Whether a resolved reference can be a linear pattern's direction: a
	// straight edge or sketch line, a reference axis, or a planar face or
	// plane (along its normal).
	grpc::Status CheckDirection(const Pick& pick, const std::string& label)
	{
		const auto notADirection = [&](const std::string& why) {
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				label + ": " + pick.what + " " + why +
				" -- give a straight edge, a sketch line, a reference axis, or a planar face or plane (to pattern "
				"along its normal)");
		};
		switch (pick.kind)
		{
		case Pick::SketchSegment:
			return IsLine(pick) ? grpc::Status::OK : notADirection("isn't a line");
		case Pick::Entity:
		{
			if (ATL::CComQIPtr<IEdge> edge(pick.obj.p); edge)
				return IsStraight(edge) ? grpc::Status::OK : notADirection("isn't straight");
			ATL::CComQIPtr<IFace2> face(pick.obj.p);
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL planar = VARIANT_FALSE;
			if (!face)
				return notADirection("isn't a direction");
			if (SUCCEEDED(face->IGetSurface(&surface)) && surface && SUCCEEDED(surface->IsPlane(&planar)) && planar)
				return grpc::Status::OK;
			return notADirection("isn't flat");
		}
		case Pick::Feature:
		{
			ATL::CComQIPtr<IFeature> feature(pick.obj.p);
			const std::string type = feature ? FeatureTypeName(feature) : std::string();
			if (type == "RefAxis" || type == "RefPlane")
				return grpc::Status::OK;
			return notADirection("isn't an axis or a plane");
		}
		default:
			return notADirection("is a point");
		}
	}

	// Whether a resolved reference can be a circular pattern's axis: a
	// straight or circular edge, a sketch line, a reference axis, or a
	// cylindrical or conical face.
	grpc::Status CheckPatternAxis(const Pick& pick)
	{
		const auto notAnAxis = [&](const std::string& why) {
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"axis: " + pick.what + " " + why +
				" -- give a reference axis, a straight or circular edge, a sketch line, or a cylindrical face (its "
				"axis)");
		};
		switch (pick.kind)
		{
		case Pick::SketchSegment:
			return IsLine(pick) ? grpc::Status::OK : notAnAxis("isn't a line");
		case Pick::Entity:
		{
			if (ATL::CComQIPtr<IEdge> edge(pick.obj.p); edge)
			{
				ATL::CComPtr<ICurve> curve;
				VARIANT_BOOL line = VARIANT_FALSE, circle = VARIANT_FALSE;
				if (SUCCEEDED(edge->IGetCurve(&curve)) && curve &&
					((SUCCEEDED(curve->IsLine(&line)) && line) || (SUCCEEDED(curve->IsCircle(&circle)) && circle)))
					return grpc::Status::OK;
				return notAnAxis("is neither straight nor circular");
			}
			ATL::CComQIPtr<IFace2> face(pick.obj.p);
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL cylinder = VARIANT_FALSE, cone = VARIANT_FALSE;
			if (face && SUCCEEDED(face->IGetSurface(&surface)) && surface &&
				((SUCCEEDED(surface->IsCylinder(&cylinder)) && cylinder) || (SUCCEEDED(surface->IsCone(&cone)) && cone)))
				return grpc::Status::OK;
			return notAnAxis(face ? "isn't cylindrical or conical" : "isn't an axis");
		}
		case Pick::Feature:
		{
			ATL::CComQIPtr<IFeature> feature(pick.obj.p);
			if (feature && FeatureTypeName(feature) == "RefAxis")
				return grpc::Status::OK;
			return notAnAxis("isn't a reference axis");
		}
		default:
			return notAnAxis("is a point");
		}
	}

	grpc::Status CheckSpacing(const sld_mcp::v1::PatternDirection& d, const std::string& label)
	{
		if (d.direction().target_case() == sld_mcp::v1::GeometryRef::TARGET_NOT_SET)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give " + label);
		if (d.count() < 2 || d.count() > kMaxInstances)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				label + ": count must be 2 to " + std::to_string(kMaxInstances) + " (the original included)");
		if (!(std::isfinite(d.spacing()) && d.spacing() > 0.0))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + ": spacing must be more than 0");
		return grpc::Status::OK;
	}

	// 2D points in sketch coordinates, meters.
	struct Point2
	{
		double x = 0.0;
		double y = 0.0;
	};

	Point2 At(const sld_mcp::v1::Vec3& v)
	{
		return {v.x(), v.y()};
	}

	// Whether `angle` lies on the arc that starts at `from` and turns
	// `sweep` radians (negative: clockwise).
	bool OnSweep(double from, double sweep, double angle)
	{
		double d = angle - from;
		if (sweep < 0)
			d = -d;
		d = std::fmod(d, 2 * kPi);
		if (d < 0)
			d += 2 * kPi;
		return d <= std::abs(sweep) + 1e-12;
	}

	// Grows `lo`/`hi` to take in an entity of a sketch, by what SolidWorks
	// says of it: points and line ends, circles whole, arcs from end to end
	// (with the quadrant points they pass). Other segments by their end and
	// center points only.
	void AddToBox(IDispatch* entity, const sld_mcp::v1::SketchItem& item, Point2& lo, Point2& hi, bool& any)
	{
		auto add = [&](Point2 p) {
			if (!any)
				lo = hi = p;
			lo = {std::min(lo.x, p.x), std::min(lo.y, p.y)};
			hi = {std::max(hi.x, p.x), std::max(hi.y, p.y)};
			any = true;
		};
		if (item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_POINT)
		{
			add(At(item.position()));
			return;
		}
		if (item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE || item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_ARC)
		{
			const Point2 c = At(item.center());
			const double r = item.radius();
			const Point2 quadrants[] = {{c.x + r, c.y}, {c.x, c.y + r}, {c.x - r, c.y}, {c.x, c.y - r}};
			if (item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE)
			{
				for (const Point2& q : quadrants)
					add(q);
				return;
			}
			const Point2 s = At(item.start()), e = At(item.end());
			add(s);
			add(e);
			int direction = 1;  // 1 counterclockwise, -1 clockwise
			if (ATL::CComQIPtr<ISketchArc> arc(entity); arc)
				arc->GetRotationDir(&direction);
			const double from = std::atan2(s.y - c.y, s.x - c.x);
			double sweep = std::atan2(e.y - c.y, e.x - c.x) - from;
			if (direction < 0 && sweep > 0)
				sweep -= 2 * kPi;
			if (direction >= 0 && sweep < 0)
				sweep += 2 * kPi;
			for (int i = 0; i < 4; ++i)
				if (OnSweep(from, sweep, i * kPi / 2))
					add(quadrants[i]);
			return;
		}
		for (const auto* v : {&item.start(), &item.end(), &item.center()})
			if (v->x() != 0.0 || v->y() != 0.0 || v->z() != 0.0)
				add(At(*v));
	}

	// One point of the first item, to check its copies by: a point itself, a
	// segment's start, a circle's center.
	bool ProbePoint(const sld_mcp::v1::SketchItem& item, Point2& p)
	{
		if (item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_POINT)
			p = At(item.position());
		else if (item.start_point().point())
			p = At(item.start());
		else if (item.center_point().point())
			p = At(item.center());
		else
			return false;
		return true;
	}

	Point2 Rotate(Point2 p, Point2 c, double angle)
	{
		const double dx = p.x - c.x, dy = p.y - c.y;
		return {c.x + dx * std::cos(angle) - dy * std::sin(angle), c.y + dx * std::sin(angle) + dy * std::cos(angle)};
	}

	// ---- EditPattern -------------------------------------------------------

	// One direction (or the axis) of a feature pattern.
	struct PatternSideSettings
	{
		long count = 1;
		double spacing = 0.0;
		bool reverse = false;
		std::string what;       // for messages
		std::string persistId;  // the direction / axis entity's, to put it back
		bool entity = false;    // it's a face or an edge
	};

	// A linear or circular pattern's settings, as its *PatternFeatureData has
	// them.
	struct PatternSettings
	{
		bool circular = false;
		bool bodyPattern = false;
		bool equal = false;     // circular
		bool hasSide2 = false;  // linear
		std::vector<ATL::CComPtr<IFeature>> seeds;
		long bodies = 0;
		PatternSideSettings side[2];
	};

	// The definition of a linear or a circular feature pattern.
	struct PatternData
	{
		ATL::CComQIPtr<ILinearPatternFeatureData> linear;
		ATL::CComQIPtr<ICircularPatternFeatureData> circular;

		IDispatch* Get() const { return linear ? static_cast<IDispatch*>(linear.p) : circular.p; }

		HRESULT Access(IModelDoc2* doc) const
		{
			VARIANT_BOOL access = VARIANT_FALSE;
			return linear ? linear->AccessSelections(doc, nullptr, &access)
						  : circular->AccessSelections(doc, nullptr, &access);
		}

		void Release() const
		{
			if (linear)
				linear->ReleaseSelectionAccess();
			else
				circular->ReleaseSelectionAccess();
		}
	};

	grpc::Status PatternDataOf(IFeature* feature, PatternData& data)
	{
		ATL::CComPtr<IDispatch> definition;
		HRESULT hr = feature->GetDefinition(&definition);
		if (FAILED(hr))
			return ComError("IFeature::GetDefinition", hr);
		data.linear = definition.p;
		data.circular = definition.p;
		if (!data.linear && !data.circular)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + FeatureName(feature) + "\" is a " + FeatureTypeName(feature) + " feature, not a linear or "
				"circular feature pattern");
		return grpc::Status::OK;
	}

	void DescribeSide(PersistIds& ids, IDispatch* axis, PatternSideSettings& side)
	{
		side.persistId = ids.Get(axis);
		side.entity = ATL::CComQIPtr<IFace2>(axis) || ATL::CComQIPtr<IEdge>(axis);
		side.what = ReferenceWhat(axis);
	}

	// Reads the settings; the selections must be accessed.
	PatternSettings ReadPattern(IModelDocExtension* ext, const PatternData& d)
	{
		PatternSettings s;
		PersistIds ids(ext);
		VARIANT_BOOL flag = VARIANT_FALSE;
		ATL::CComVariant seeds;
		ATL::CComPtr<IDispatch> axis1, axis2;
		if (d.linear)
		{
			ILinearPatternFeatureData* l = d.linear;
			l->get_D1TotalInstances(&s.side[0].count);
			l->get_D1Spacing(&s.side[0].spacing);
			l->get_D1ReverseDirection(&flag);
			s.side[0].reverse = flag != VARIANT_FALSE;
			l->get_D1Axis(&axis1);
			flag = VARIANT_FALSE;
			l->IsDirection2Specified(&flag);
			const bool specified = flag != VARIANT_FALSE;
			l->get_D2TotalInstances(&s.side[1].count);
			l->get_D2Spacing(&s.side[1].spacing);
			flag = VARIANT_FALSE;
			l->get_D2ReverseDirection(&flag);
			s.side[1].reverse = flag != VARIANT_FALSE;
			l->get_D2Axis(&axis2);
			s.hasSide2 = specified && s.side[1].count > 1;
			flag = VARIANT_FALSE;
			l->get_BodyPattern(&flag);
			s.bodyPattern = flag != VARIANT_FALSE;
			l->get_PatternFeatureArray(&seeds);
			l->GetPatternBodyCount(&s.bodies);
		}
		else
		{
			ICircularPatternFeatureData* c = d.circular;
			s.circular = true;
			c->get_TotalInstances(&s.side[0].count);
			c->get_Spacing(&s.side[0].spacing);
			c->get_ReverseDirection(&flag);
			s.side[0].reverse = flag != VARIANT_FALSE;
			flag = VARIANT_FALSE;
			c->get_EqualSpacing(&flag);
			s.equal = flag != VARIANT_FALSE;
			c->get_Axis(&axis1);
			flag = VARIANT_FALSE;
			c->get_BodyPattern(&flag);
			s.bodyPattern = flag != VARIANT_FALSE;
			c->get_PatternFeatureArray(&seeds);
			c->GetPatternBodyCount(&s.bodies);
		}
		DescribeSide(ids, axis1, s.side[0]);
		if (s.hasSide2)
			DescribeSide(ids, axis2, s.side[1]);
		if (!s.bodyPattern)
			s.seeds = FeatureList(seeds);
		return s;
	}

	// Reads the pattern with its selections accessed, then releases them.
	grpc::Status ReadPatternNow(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature, PatternSettings& s)
	{
		PatternData data;
		if (grpc::Status status = PatternDataOf(feature, data); !status.ok())
			return status;
		if (HRESULT hr = data.Access(doc); FAILED(hr))
			return ComError("AccessSelections (pattern)", hr);
		s = ReadPattern(ext, data);
		data.Release();
		return grpc::Status::OK;
	}

	void DescribePatternSide(const PatternSideSettings& s, sld_mcp::v1::PatternSide* out)
	{
		out->set_what(s.what);
		if (s.entity)
			out->set_persist_id(s.persistId);
		out->set_count(static_cast<int32_t>(s.count));
		out->set_spacing(s.spacing);
		out->set_reverse(s.reverse);
	}

	grpc::Status DescribePattern(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature,
		sld_mcp::v1::PatternDefinition* out, PatternSettings* settings)
	{
		PatternSettings s;
		if (grpc::Status status = ReadPatternNow(doc, ext, feature, s); !status.ok())
			return status;
		out->set_name(FeatureName(feature));
		out->set_type_name(FeatureTypeName(feature));
		out->set_circular(s.circular);
		for (const auto& seed : s.seeds)
			out->add_features(FeatureName(seed));
		if (s.bodyPattern)
			out->set_bodies(static_cast<int32_t>(s.bodies));
		DescribePatternSide(s.side[0], out->mutable_direction1());
		if (s.hasSide2)
			DescribePatternSide(s.side[1], out->mutable_direction2());
		out->set_equal_spacing(s.circular && s.equal);
		for (const auto& display : FeatureDimensions(feature))
			DescribeDimension(display, out->add_dimensions());
		int32_t code = 0;
		bool warning = false;
		SetFeatureError(feature, code, warning);
		out->set_error_code(code);
		out->set_error_is_warning(warning);
		if (settings)
			*settings = s;
		return grpc::Status::OK;
	}

	// A new direction / axis for one side: a reference from the request, or
	// (putting one back) a persist id. Neither = keep.
	struct AxisChange
	{
		const sld_mcp::v1::GeometryRef* ref = nullptr;
		std::string persistId;
	};

	// Resolves an axis change where the pattern is built (the selections
	// accessed).
	grpc::Status ResolveAxis(IModelDoc2* doc, IModelDocExtension* ext, const AxisChange& change, bool circular,
		const std::string& label, ATL::CComPtr<IDispatch>& obj)
	{
		if (change.ref)
		{
			Pick pick;
			if (grpc::Status status = ResolvePick(doc, ext, *change.ref, pick); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message() +
					" (it must exist where the pattern is built)");
			if (grpc::Status status = circular ? CheckPatternAxis(pick) : CheckDirection(pick, label); !status.ok())
				return status;
			if (!pick.obj)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + ": " + pick.what + " can't be used here");
			obj = pick.obj;
			return grpc::Status::OK;
		}
		if (grpc::Status status = ResolvePersistId(ext, change.persistId, obj); !status.ok())
			return grpc::Status(status.error_code(), label + ": " + status.error_message());
		return grpc::Status::OK;
	}

	// Gives the pattern `target`'s counts, spacings and directions, `axes`'
	// new direction / axis entities and `seeds` (empty: keep) with
	// IFeature::ModifyDefinition.
	grpc::Status ApplyPattern(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature,
		const PatternSettings& target, const AxisChange (&axes)[2], const std::vector<ATL::CComPtr<IFeature>>& seeds,
		bool& modified)
	{
		modified = false;
		PatternData data;
		if (grpc::Status status = PatternDataOf(feature, data); !status.ok())
			return status;
		if (HRESULT hr = data.Access(doc); FAILED(hr))
			return ComError("AccessSelections (pattern)", hr);
		const PatternSettings current = ReadPattern(ext, data);
		auto fail = [&](grpc::Status status) {
			data.Release();
			return status;
		};

		ATL::CComPtr<IDispatch> axis[2];
		for (int i = 0; i < 2; ++i)
			if (axes[i].ref || !axes[i].persistId.empty())
				if (grpc::Status status = ResolveAxis(doc, ext, axes[i], target.circular,
						target.circular ? "axis" : i == 0 ? "direction" : "direction2", axis[i]);
					!status.ok())
					return fail(status);

		const auto bit = [](bool b) { return b ? VARIANT_TRUE : VARIANT_FALSE; };
		const auto differs = [](double a, double b) { return std::abs(a - b) > 1e-12; };
		const PatternSideSettings& want1 = target.side[0];
		const PatternSideSettings& have1 = current.side[0];
		if (data.linear)
		{
			ILinearPatternFeatureData* l = data.linear;
			if (axis[0])
				l->put_D1Axis(axis[0]);
			if (want1.count != have1.count)
				l->put_D1TotalInstances(want1.count);
			if (differs(want1.spacing, have1.spacing))
				l->put_D1Spacing(want1.spacing);
			if (want1.reverse != have1.reverse)
				l->put_D1ReverseDirection(bit(want1.reverse));
			const PatternSideSettings& want2 = target.side[1];
			const PatternSideSettings& have2 = current.side[1];
			if (target.hasSide2)
			{
				if (axis[1])
					l->put_D2Axis(axis[1]);
				if (want2.count != have2.count || !current.hasSide2)
					l->put_D2TotalInstances(want2.count);
				if (differs(want2.spacing, have2.spacing))
					l->put_D2Spacing(want2.spacing);
				if (want2.reverse != have2.reverse)
					l->put_D2ReverseDirection(bit(want2.reverse));
			}
			else if (current.hasSide2)
				l->put_D2TotalInstances(1);
		}
		else
		{
			ICircularPatternFeatureData* c = data.circular;
			if (axis[0])
				c->put_Axis(axis[0]);
			if (target.equal != current.equal)
				c->put_EqualSpacing(bit(target.equal));
			if (want1.count != have1.count)
				c->put_TotalInstances(want1.count);
			if (differs(want1.spacing, have1.spacing) || target.equal != current.equal)
				c->put_Spacing(want1.spacing);
			if (want1.reverse != have1.reverse)
				c->put_ReverseDirection(bit(want1.reverse));
		}
		if (!seeds.empty())
		{
			std::vector<IDispatch*> list;
			for (const auto& seed : seeds)
				list.push_back(seed);
			const ATL::CComVariant array = comvariant::FromDispatches(list);
			if (data.linear)
				data.linear->put_PatternFeatureArray(array);
			else
				data.circular->put_PatternFeatureArray(array);
		}

		VARIANT_BOOL ok = VARIANT_FALSE;
		HRESULT hr = feature->ModifyDefinition(data.Get(), doc, nullptr, &ok);
		if (FAILED(hr) || !ok)
			data.Release();
		if (FAILED(hr))
			return ComError("IFeature::ModifyDefinition", hr);
		modified = ok != VARIANT_FALSE;
		return grpc::Status::OK;
	}

	// What differs between the pattern as read and as asked; empty if nothing.
	std::string PatternMismatch(const PatternSettings& now, const PatternSettings& want, size_t seedCount)
	{
		const auto side = [&](int i) -> std::string {
			const PatternSideSettings& a = now.side[i];
			const PatternSideSettings& b = want.side[i];
			const std::string which = i == 0 ? "" : " in the second direction";
			if (a.count != b.count)
				return std::to_string(a.count) + " instances" + which;
			if (std::abs(a.spacing - b.spacing) > 1e-9)
				return "spacing " + std::to_string(a.spacing) + which;
			if (a.reverse != b.reverse)
				return std::string(a.reverse ? "reversed" : "not reversed") + which;
			return {};
		};
		if (std::string s = side(0); !s.empty())
			return s;
		if (now.hasSide2 != want.hasSide2)
			return now.hasSide2 ? "a second direction" : "no second direction";
		if (want.hasSide2)
			if (std::string s = side(1); !s.empty())
				return s;
		if (now.circular && now.equal != want.equal)
			return now.equal ? "equal spacing" : "no equal spacing";
		if (seedCount && now.seeds.size() != seedCount)
			return std::to_string(now.seeds.size()) + " patterned features";
		return {};
	}
}

grpc::Status sld_mcp::service::DescribePatternDefinition(IModelDoc2* doc, IModelDocExtension* ext,
	IFeature* feature, sld_mcp::v1::PatternDefinition* out)
{
	return DescribePattern(doc, ext, feature, out, nullptr);
}

grpc::Status SolidWorksService::PatternFeatures(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::PatternFeaturesRequest* request,
	sld_mcp::v1::PatternFeaturesResponse* response)
{
	const int featureCount = request->features_size();
	const int bodyCount = request->bodies_size();
	if (featureCount == 0 && bodyCount == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the features or the bodies to pattern");
	if (featureCount > 0 && bodyCount > 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give features or bodies to pattern, not both");
	if (featureCount + bodyCount > kMaxSeeds)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxSeeds) + " features or bodies per call");

	const bool circular = request->has_circular();
	const auto& linear = request->linear();
	const bool twoWays = !circular && linear.has_direction2() && linear.direction2().count() > 1;
	if (circular)
	{
		const auto& c = request->circular();
		if (c.axis().target_case() == sld_mcp::v1::GeometryRef::TARGET_NOT_SET)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the axis to pattern round");
		if (c.count() < 2 || c.count() > kMaxInstances)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"count must be 2 to " + std::to_string(kMaxInstances) + " (the original included)");
		if (!(std::isfinite(c.angle()) && c.angle() > 0.0 && c.angle() <= 2 * kPi + 1e-9))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the angle must be more than 0 and at most 360 degrees");
	}
	else
	{
		if (!request->has_linear())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say whether the pattern is linear or circular");
		if (grpc::Status status = CheckSpacing(linear.direction1(), "direction"); !status.ok())
			return status;
		if (twoWays)
		{
			if (grpc::Status status = CheckSpacing(linear.direction2(), "direction2"); !status.ok())
				return status;
			if (static_cast<long long>(linear.direction1().count()) * linear.direction2().count() > kMaxInstances)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"at most " + std::to_string(kMaxInstances) + " instances in all");
		}
	}

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
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	// The direction(s) or the axis.
	Selection directions;
	if (circular)
	{
		if (grpc::Status status = directions.Add(doc, ext, request->circular().axis(), kDirection1Mark, "axis");
			!status.ok())
			return status;
		if (grpc::Status status = CheckPatternAxis(directions.picks.front()); !status.ok())
			return status;
	}
	else
	{
		if (grpc::Status status =
				directions.Add(doc, ext, linear.direction1().direction(), kDirection1Mark, "direction");
			!status.ok())
			return status;
		if (grpc::Status status = CheckDirection(directions.picks.front(), "direction"); !status.ok())
			return status;
		if (twoWays)
		{
			if (grpc::Status status =
					directions.Add(doc, ext, linear.direction2().direction(), kDirection2Mark, "direction2");
				!status.ok())
				return status;
			if (grpc::Status status = CheckDirection(directions.picks.back(), "direction2"); !status.ok())
				return status;
		}
	}

	std::vector<ATL::CComPtr<IFeature>> features;
	std::vector<ATL::CComPtr<IBody2>> bodies;
	std::string patterned;
	if (grpc::Status status = ResolveSeedFeatures(doc, ext, request->features(), "patterned",
			"pattern the feature made from it, or pattern the sketch's entities inside it with "
			"linear_pattern_sketch_entities / circular_pattern_sketch_entities",
			features, patterned);
		!status.ok())
		return status;
	if (grpc::Status status = ResolveSeedBodies(ext, request->bodies(), bodies, patterned); !status.ok())
		return status;

	if (grpc::Status status = directions.Select(doc, ext); !status.ok())
		return status;
	if (grpc::Status status = SelectSeedFeatures(doc, features, kSeedFeatureMark); !status.ok())
		return status;
	if (grpc::Status status = SelectSeedBodies(doc, bodies, kSeedBodyMark); !status.ok())
		return status;

	ATL::CComPtr<IFeature> pattern;
	const ATL::CComBSTR noDimension(L"");
	if (circular)
	{
		const auto& c = request->circular();
		hr = featureManager->FeatureCircularPattern5(
			c.count(),                                    // Number
			c.angle(),                                    // Spacing (the total with EqualSpacing)
			c.reverse() ? VARIANT_TRUE : VARIANT_FALSE,   // FlipDirection
			noDimension,                                  // DName
			VARIANT_FALSE,                                // GeometryPattern
			c.equal_spacing() ? VARIANT_TRUE : VARIANT_FALSE, // EqualSpacing
			VARIANT_FALSE,                                // VaryInstance
			VARIANT_FALSE,                                // SyncSubAssemblies
			VARIANT_FALSE,                                // BDir2
			VARIANT_FALSE,                                // BSymmetric
			1,                                            // Number2
			0.0,                                          // Spacing2
			noDimension,                                  // DName2
			VARIANT_FALSE,                                // EqualSpacing2
			&pattern);
	}
	else
	{
		const auto& d1 = linear.direction1();
		const auto& d2 = linear.direction2();
		hr = featureManager->FeatureLinearPattern5(
			d1.count(),                                   // Num1
			d1.spacing(),                                 // Spacing1
			twoWays ? d2.count() : 1,                     // Num2
			twoWays ? d2.spacing() : d1.spacing(),        // Spacing2 (unused with one)
			d1.reverse() ? VARIANT_TRUE : VARIANT_FALSE,  // FlipDir1
			twoWays && d2.reverse() ? VARIANT_TRUE : VARIANT_FALSE, // FlipDir2
			noDimension,                                  // DName1
			noDimension,                                  // DName2
			VARIANT_FALSE,                                // GeometryPattern
			VARIANT_FALSE,                                // VaryInstance
			VARIANT_FALSE, VARIANT_FALSE,                 // HasOffset1/2
			VARIANT_FALSE, VARIANT_FALSE,                 // CtrlByNum1/2 (spacing, not up to a reference)
			VARIANT_FALSE, VARIANT_FALSE,                 // FromCentroid1/2
			VARIANT_FALSE, VARIANT_FALSE,                 // RevOffset1/2
			0.0, 0.0,                                     // Offset1/2
			VARIANT_FALSE,                                // D2PatternSeedOnly
			VARIANT_FALSE,                                // SyncSubAssemblies
			&pattern);
	}
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError(circular ? "IFeatureManager::FeatureCircularPattern5" : "IFeatureManager::FeatureLinearPattern5",
			hr);
	const std::string along = circular ? "round " + directions.Describe() : "along " + directions.Describe();
	if (!pattern)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't pattern " + patterned + " " + along +
			" -- e.g. copies of a cut or fillet that find no material, or copies of a boss that don't touch the "
			"body; check the direction, spacing and count");
	if (grpc::Status status = KeepIfBuilt(doc, pattern,
			"the copies of " + patterned + " " + along +
				" probably find no material there (a cut or fillet beyond the body) -- check the direction, "
				"spacing and count");
		!status.ok())
		return status;

	response->set_feature_name(FeatureName(pattern));
	response->set_patterned(patterned);
	response->set_direction1(directions.picks.front().what);
	if (twoWays)
		response->set_direction2(directions.picks.back().what);
	response->set_body_count(SolidBodyCount(doc));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::PatternSketchEntities(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::PatternSketchEntitiesRequest* request,
	sld_mcp::v1::PatternSketchEntitiesResponse* response)
{
	const int count = request->items_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one sketch entity to pattern");
	if (count > kMaxPatternSketchItems)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"too many sketch entities in one call (max " + std::to_string(kMaxPatternSketchItems) + ")");
	const bool circular = request->has_circular();
	const auto& linear = request->linear();
	const auto& round = request->circular();
	if (circular)
	{
		if (round.count() < 2 || round.count() > kMaxInstances)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"count must be 2 to " + std::to_string(kMaxInstances) + " (the originals included)");
		if (!(std::isfinite(round.spacing()) && round.spacing() != 0.0 && std::abs(round.spacing()) <= 2 * kPi))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the angle between instances must not be 0, nor more than 360 degrees");
	}
	else
	{
		if (!request->has_linear())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say whether the pattern is linear or circular");
		const long long instances = static_cast<long long>(std::max(linear.count(), 1)) * std::max(linear.count2(), 1);
		if (linear.count() < 1 || linear.count2() < 0 || instances < 2 || instances > kMaxInstances)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the pattern needs 2 to " + std::to_string(kMaxInstances) + " instances (the originals included)");
		if (!(std::isfinite(linear.spacing()) && linear.spacing() > 0.0) && linear.count() > 1)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "spacing must be more than 0");
		if (!(std::isfinite(linear.spacing2()) && linear.spacing2() > 0.0) && linear.count2() > 1)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "spacing2 must be more than 0");
		if (!std::isfinite(linear.angle()) || !std::isfinite(linear.angle2()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the directions' angles must be numbers");
	}
	auto where = [](int i) { return "entity " + std::to_string(i + 1); };

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

	ATL::CComPtr<IFeature> sketchFeature;
	if (grpc::Status status = ResolveItemsSketch(doc, ext, request->sketch_name(), request->items(0), sketchFeature);
		!status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	response->set_sketch_name(sketchName);

	for (int i = 0; i < count; ++i)
	{
		google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> one;
		*one.Add()->mutable_item() = request->items(i);
		if (grpc::Status status = CheckRefsInSketch(ext, one, sketchPid, sketchName, where(i)); !status.ok())
			return status;
	}

	// Check the entities against the closed sketch before opening it; for a
	// circular pattern, also find their box and a point to check the copies
	// by.
	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	bool anyPoints = false;
	Point2 lo, hi, probe;
	bool anyBox = false, haveProbe = false;
	{
		SketchItems items;
		items.Load(sketch);
		for (int i = 0; i < count; ++i)
		{
			const auto& ref = request->items(i);
			ATL::CComPtr<IDispatch> entity = items.Find(ref);
			if (!entity)
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					where(i) + " isn't in sketch \"" + sketchName + "\" any more (deleted or merged?) -- inspect "
					"the sketch again for current IDs");
			anyPoints = anyPoints || ref.point();
			sld_mcp::v1::SketchItem item;
			if (!circular || !DescribeSketchItem(entity, sketchPid, &item))
				continue;
			AddToBox(entity, item, lo, hi, anyBox);
			if (!haveProbe)
				haveProbe = ProbePoint(item, probe);
		}
	}
	// The circular pattern's center as SolidWorks takes it: ArcRadius and
	// ArcAngle from the middle of the entities' box (seen 2026-10-20). It
	// refuses a negative ArcAngle, so that is kept in [0, 2 pi).
	const Point2 center = circular ? At(round.center()) : Point2{};
	const Point2 boxMiddle{(lo.x + hi.x) / 2, (lo.y + hi.y) / 2};
	const double arcRadius = std::hypot(center.x - boxMiddle.x, center.y - boxMiddle.y);
	double arcAngle = std::atan2(center.y - boxMiddle.y, center.x - boxMiddle.x);
	if (arcAngle < 0)
		arcAngle += 2 * kPi;
	if (arcAngle >= 2 * kPi)
		arcAngle = 0.0;

	const auto errorsBefore = FeatureErrors(FeatureTree(doc));

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;
	ATL::CComPtr<ISketch> openSketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, openSketch); !status.ok())
		return status;

	std::set<SketchItemKey> added;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems open;
		open.Load(openSketch);
		const std::set<SketchItemKey> before = SketchKeys(open);
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		const std::vector<sld_mcp::v1::SketchItemRef> refs(request->items().begin(), request->items().end());
		if (size_t failed = 0; !SelectSketchItems(doc, open, refs, failed))
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks wouldn't select " + where(static_cast<int>(failed)) +
				" in the open sketch -- nothing was patterned");

		VARIANT_BOOL made = VARIANT_FALSE;
		const ATL::CComBSTR keepAll(L"");
		if (circular)
			// PatternSpacing turns clockwise (seen 2026-10-20). PatternRotate
			// off moved the originals themselves, so copies are always
			// rotated.
			hr = sketchManager->CreateCircularSketchStepAndRepeat(arcRadius, arcAngle, round.count(), -round.spacing(),
				VARIANT_TRUE, keepAll, VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, &made);
		else
			hr = sketchManager->CreateLinearSketchStepAndRepeat(std::max(linear.count(), 1),
				std::max(linear.count2(), 1), linear.spacing(), linear.count2() > 1 ? linear.spacing2() : 0.0,
				linear.angle(), linear.angle2(), keepAll, VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE,
				VARIANT_FALSE, &made);
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError(circular ? "ISketchManager::CreateCircularSketchStepAndRepeat"
									 : "ISketchManager::CreateLinearSketchStepAndRepeat",
				hr);
		open.Load(openSketch);
		for (const SketchItemKey& key : SketchKeys(open))
			if (!before.count(key))
				added.insert(key);
		if (!made || added.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't pattern anything in \"" + sketchName + "\"" +
				(made ? std::string() : " (it refused the pattern)"));

		// SolidWorks picks its own reference point on the entities; check
		// that the copies of one of them are where they belong, else take
		// the copies out again.
		if (!circular || !haveProbe)
			return grpc::Status::OK;
		std::vector<Point2> addedPoints;
		for (const auto& e : open.All())
		{
			sld_mcp::v1::SketchItem item;
			if (e.point && added.count(SketchKeyOf(e)) && DescribeSketchItem(e.obj, sketchPid, &item))
				addedPoints.push_back(At(item.position()));
		}
		for (int k = 1; k < round.count(); ++k)
		{
			const Point2 expected = Rotate(probe, center, k * round.spacing());
			if (std::any_of(addedPoints.begin(), addedPoints.end(), [&](const Point2& p) {
					return std::hypot(p.x - expected.x, p.y - expected.y) <= kPositionTolerance;
				}))
				continue;
			for (const auto& e : open.All())
				if (added.count(SketchKeyOf(e)))
				{
					VARIANT_BOOL selected = VARIANT_FALSE;
					if (ATL::CComQIPtr<ISketchSegment> segment(e.obj.p); segment)
						segment->Select4(VARIANT_TRUE, nullptr, &selected);
					else if (ATL::CComQIPtr<ISketchPoint> point(e.obj.p); point)
						point->Select4(VARIANT_TRUE, nullptr, &selected);
				}
			doc->EditDelete();
			doc->ClearSelection2(VARIANT_TRUE);
			added.clear();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks put the circular pattern's center somewhere other than asked (copy " + std::to_string(k) +
				" isn't where it belongs), so the copies were deleted again -- this happens with entity types whose "
				"extent the add-in can't work out (splines, ellipses); pattern lines, arcs, circles and points");
		}
		return grpc::Status::OK;
	}();

	// Closed without the exit rebuild, as DeleteSketchEntities does: when
	// the new entities break the profile of the sketch's feature,
	// InsertSketch(TRUE) would open the Repair Sketch dialog.
	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_FALSE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"the entities were patterned, but SolidWorks left \"" + sketchName + "\" open for editing");
	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);

	sketch.Release();
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	SketchItems after;
	after.Load(sketch);
	DescribeAddedItems(after, added, sketchPid, anyPoints, response->mutable_items());

	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;
	ReadSketchState(sketch, sketchPid, response->mutable_state());
	AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::EditPattern(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::EditPatternRequest* request,
	sld_mcp::v1::EditPatternResponse* response)
{
	// Validate before touching SolidWorks.
	using Ref = sld_mcp::v1::GeometryRef;
	const auto countOk = [](int32_t n) { return n >= 2 && n <= kMaxInstances; };
	const auto spacingOk = [](double s) { return std::isfinite(s) && s > 0.0; };
	if (request->has_count() && !countOk(request->count()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"count must be 2 to " + std::to_string(kMaxInstances) + " (the original included)");
	if (request->has_count2() && !(request->count2() == 1 || countOk(request->count2())))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"count2 must be 2 to " + std::to_string(kMaxInstances) + ", or 1 to remove the second direction");
	if ((request->has_spacing() && !spacingOk(request->spacing())) ||
		(request->has_spacing2() && !spacingOk(request->spacing2())))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a spacing must be more than 0");
	const bool newAxis = request->direction().target_case() != Ref::TARGET_NOT_SET;
	const bool newAxis2 = request->direction2().target_case() != Ref::TARGET_NOT_SET;
	const bool side2Change = request->has_count2() || request->has_spacing2() || request->has_reverse2() || newAxis2;
	if (request->features_size() > kMaxSeeds)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxSeeds) + " features per call");
	const bool anyChange = request->has_count() || request->has_spacing() || request->has_reverse() ||
		request->has_equal_spacing() || newAxis || side2Change || request->features_size() > 0;

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
	PersistIds ids(ext);

	const std::vector<TreeFeature> tree = FeatureTree(doc);
	size_t index = 0;
	if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, request->feature(), index); !status.ok())
		return status;
	ATL::CComPtr<IFeature> feature = tree[index].feature;
	const std::string name = tree[index].name;

	PatternSettings before;
	if (grpc::Status status = DescribePattern(doc, ext, feature, response->mutable_before(), &before); !status.ok())
		return status;
	response->set_body_count(SolidBodyCount(doc));
	if (!anyChange)
		return grpc::Status::OK;

	// What it should become: the current settings with the request's changes.
	PatternSettings target = before;
	PatternSideSettings& side1 = target.side[0];
	PatternSideSettings& side2 = target.side[1];
	if (before.circular)
	{
		if (side2Change)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" is a circular pattern: it has no second direction");
		// An angle comes with whether it's the total or the step; a spacing
		// without (a linear one, in meters) is a mistake.
		if (request->has_spacing() && !request->has_equal_spacing())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" is a circular pattern: give its spacing as an angle (angle_deg for the total, "
				"spacing_deg between neighbours)");
		if (request->has_spacing() && request->spacing() > 2 * kPi + 1e-9)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the angle must be at most 360 degrees");
		if (request->has_equal_spacing())
			target.equal = request->equal_spacing();
	}
	else if (request->has_equal_spacing())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"\"" + name + "\" is a linear pattern: give its spacing as a distance (spacing_mm); angles are for "
			"circular ones");
	if (request->has_count())
		side1.count = request->count();
	if (request->has_spacing())
		side1.spacing = request->spacing();
	if (request->has_reverse())
		side1.reverse = request->reverse();
	if (side2Change)
	{
		if (request->has_count2())
			target.hasSide2 = request->count2() > 1;
		else
			target.hasSide2 = true;
		if (!target.hasSide2 && (request->has_spacing2() || request->has_reverse2() || newAxis2))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"count2 = 1 removes the second direction; give nothing else for it");
		if (target.hasSide2 && !before.hasSide2)
		{
			if (!newAxis2 || !request->has_spacing2() || !request->has_count2())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" has no second direction yet -- give direction2, count2 and spacing2 to add one");
			side2.reverse = false;
		}
		if (request->has_count2() && target.hasSide2)
			side2.count = request->count2();
		if (request->has_spacing2())
			side2.spacing = request->spacing2();
		if (request->has_reverse2())
			side2.reverse = request->reverse2();
	}
	if (static_cast<long long>(side1.count) * (target.hasSide2 ? side2.count : 1) > kMaxInstances)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxInstances) + " instances in all");
	if (target.circular && !target.equal && side1.spacing * (side1.count - 1) >= 2 * kPi - 1e-9)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			std::to_string(side1.count) + " instances " + std::to_string(side1.spacing * 180.0 / kPi) +
			" degrees apart don't fit in one turn");

	std::vector<ATL::CComPtr<IFeature>> seeds;
	if (request->features_size() > 0)
	{
		if (before.bodyPattern)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" patterns bodies, not features -- delete it and pattern the features instead");
		std::string names;
		if (grpc::Status status = ResolveSeedFeatures(doc, ext, request->features(), "patterned",
				"pattern the feature made from it", seeds, names);
			!status.ok())
			return status;
	}

	AxisChange axes[2];
	if (newAxis)
		axes[0].ref = &request->direction();
	if (newAxis2)
		axes[1].ref = &request->direction2();
	// Putting it back: the old direction / axis entities where they changed.
	AxisChange oldAxes[2];
	if (newAxis)
		oldAxes[0].persistId = before.side[0].persistId;
	if (newAxis2 && before.hasSide2)
		oldAxes[1].persistId = before.side[1].persistId;

	DefinitionEdit edit;
	edit.apply = [&](bool& modified) {
		return ApplyPattern(doc, ext, feature, target, axes, seeds, modified);
	};
	edit.restore = [&]() {
		bool ignored = false;
		ApplyPattern(doc, ext, feature, before, oldAxes, seeds.empty() ? seeds : before.seeds, ignored);
	};
	edit.check = [&]() -> std::string {
		PatternSettings now;
		if (!ReadPatternNow(doc, ext, feature, now).ok())
			return "no definition SolidWorks would hand out";
		return PatternMismatch(now, target, seeds.size());
	};
	edit.rejected = "e.g. copies of a cut or fillet that find no material, copies of a boss that don't touch the "
		"body, or a direction that doesn't suit";
	if (grpc::Status status = EditDefinition(swApp, doc, feature, tree, edit, response->mutable_new_errors());
		!status.ok())
		return status;
	response->set_body_count(SolidBodyCount(doc));
	return DescribePattern(doc, ext, feature, response->mutable_after(), nullptr);
}
