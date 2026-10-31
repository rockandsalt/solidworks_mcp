// SketchConstraints.cpp : Sketch dimensions and relations: AddSketchDimensions, AddSketchRelations,
// FullyDefineSketch, SetDimensionValue.
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The active part or assembly, with no sketch open for editing.
	grpc::Status GetEditableModel(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		return CheckNoOpenSketch(doc);
	}

	// Requests with more dimensions or relations than this are refused.
	constexpr int kMaxSketchConstraints = 200;

	// Most refs a many-entity relation (equal, parallel, ...) can have. Must
	// match MAX_RELATION_ENTITIES in mcp_server/src/main.rs.
	constexpr int kMaxRelationRefs = 32;

	const char* DefinitionStatusText(sld_mcp::v1::DefinitionStatus status)
	{
		switch (status)
		{
		case sld_mcp::v1::DEFINITION_STATUS_UNDER:       return "under defined";
		case sld_mcp::v1::DEFINITION_STATUS_FULLY:       return "fully defined";
		case sld_mcp::v1::DEFINITION_STATUS_OVER:        return "over defined";
		case sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION: return "unsolvable";
		case sld_mcp::v1::DEFINITION_STATUS_INVALID:     return "solved into invalid geometry";
		case sld_mcp::v1::DEFINITION_STATUS_NOT_SOLVED:  return "not solved (automatic solve is off)";
		default:                                         return "of unknown status";
		}
	}

	// No dialogs while adding dimensions: don't ask for a value on creation,
	// and make a dimension that would over-define the sketch driven instead
	// of asking what to do (the RPC then reports it as a conflict).
	void SilenceDimensionPrompts(PreferenceToggles& toggles)
	{
		toggles.Set(swInputDimValOnCreate, false);
		toggles.Set(swSketchOverdefiningDimsPromptToSetState, false);
		toggles.Set(swSketchOverdefiningDimsSetDrivenByDefault, true);
	}

	// Where a ref is, in sketch coordinates, for placing dimension text.
	struct RefShape
	{
		enum Kind { Point, Line, Round } kind = Point;
		double a[2] = {};  // the point; a line's start; a circle's center
		double b[2] = {};  // a line's end
		double radius = 0.0;
	};

	// A SketchRef resolved against the open sketch.
	struct ResolvedRef
	{
		ATL::CComPtr<IDispatch> obj;  // null for the origin
		bool origin = false;
		sld_mcp::v1::SketchItemType type = sld_mcp::v1::SKETCH_ITEM_TYPE_UNSPECIFIED;  // sketch items
		bool isPoint = false;  // a sketch point or the origin
		bool isLine = false;   // a sketch line or straight model edge
		bool isRound = false;  // a sketch arc/circle or circular model edge
		RefShape shape;
	};

	// Maps a model-space point into the sketch (z dropped).
	bool ModelToSketch2D(const SketchSpace& space, const double model[3], double out[2])
	{
		double p[3];
		if (!space.ToSketch(model, p))
			return false;
		out[0] = p[0];
		out[1] = p[1];
		return true;
	}

	// Context for resolving refs: the open sketch and how to reach the rest
	// of the model.
	struct RefContext
	{
		IModelDocExtension* ext;
		const SketchItems& items;
		const SketchSpace& space;
		const std::string& sketchPid;
		const std::string& sketchName;
	};

	grpc::Status ResolveRef(const RefContext& ctx, const sld_mcp::v1::SketchRef& ref, ResolvedRef& out)
	{
		using Ref = sld_mcp::v1::SketchRef;
		switch (ref.target_case())
		{
		case Ref::kOrigin:
		{
			out.origin = true;
			out.isPoint = true;
			const double zero[3] = {0.0, 0.0, 0.0};
			out.shape.kind = RefShape::Point;
			ModelToSketch2D(ctx.space, zero, out.shape.a);
			return grpc::Status::OK;
		}
		case Ref::kItem:
		{
			out.obj = ctx.items.Find(ref.item());
			if (!out.obj)
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					"no such entity in sketch \"" + ctx.sketchName +
					"\" (deleted or merged?) -- inspect the sketch again for current IDs");
			sld_mcp::v1::SketchItem item;
			if (!DescribeSketchItem(out.obj, ctx.sketchPid, &item))
				return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't read a sketch entity's geometry");
			out.type = item.type();
			switch (item.type())
			{
			case sld_mcp::v1::SKETCH_ITEM_TYPE_POINT:
				out.isPoint = true;
				out.shape.kind = RefShape::Point;
				out.shape.a[0] = item.position().x();
				out.shape.a[1] = item.position().y();
				break;
			case sld_mcp::v1::SKETCH_ITEM_TYPE_LINE:
				out.isLine = true;
				out.shape.kind = RefShape::Line;
				out.shape.a[0] = item.start().x();
				out.shape.a[1] = item.start().y();
				out.shape.b[0] = item.end().x();
				out.shape.b[1] = item.end().y();
				break;
			case sld_mcp::v1::SKETCH_ITEM_TYPE_ARC:
			case sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE:
				out.isRound = true;
				out.shape.kind = RefShape::Round;
				out.shape.a[0] = item.center().x();
				out.shape.a[1] = item.center().y();
				out.shape.radius = item.radius();
				break;
			default:
				// Splines etc.: aim at the middle of the end points.
				out.shape.kind = RefShape::Point;
				out.shape.a[0] = (item.start().x() + item.end().x()) / 2.0;
				out.shape.a[1] = (item.start().y() + item.end().y()) / 2.0;
				break;
			}
			return grpc::Status::OK;
		}
		case Ref::kModelPersistId:
		{
			if (grpc::Status status = ResolvePersistId(ctx.ext, ref.model_persist_id(), out.obj); !status.ok())
				return status;
			if (ATL::CComQIPtr<IEdge> edge(out.obj.p); edge)
			{
				ATL::CComVariant params;
				std::vector<double> c;
				if (SUCCEEDED(edge->GetCurveParams2(&params)) && (c = comvariant::ToDoubles(params)).size() >= 6)
				{
					ModelToSketch2D(ctx.space, &c[0], out.shape.a);
					ModelToSketch2D(ctx.space, &c[3], out.shape.b);
				}
				ATL::CComPtr<ICurve> curve;
				long identity = 0;
				if (SUCCEEDED(edge->IGetCurve(&curve)) && curve && SUCCEEDED(curve->Identity(&identity)))
				{
					if (identity == LINE_TYPE)
					{
						out.isLine = true;
						out.shape.kind = RefShape::Line;
					}
					else if (identity == CIRCLE_TYPE)
					{
						ATL::CComVariant circle;
						std::vector<double> p;
						if (SUCCEEDED(curve->get_CircleParams(&circle)) && (p = comvariant::ToDoubles(circle)).size() >= 7)
						{
							out.isRound = true;
							out.shape.kind = RefShape::Round;
							ModelToSketch2D(ctx.space, &p[0], out.shape.a);
							out.shape.radius = p[6];
						}
					}
				}
				if (out.shape.kind == RefShape::Point)
					for (int i = 0; i < 2; ++i)
						out.shape.a[i] = (out.shape.a[i] + out.shape.b[i]) / 2.0;
				return grpc::Status::OK;
			}
			if (ATL::CComQIPtr<IFace2> face(out.obj.p); face)
			{
				ATL::CComVariant box;
				std::vector<double> b;
				if (SUCCEEDED(face->GetBox(&box)) && (b = comvariant::ToDoubles(box)).size() >= 6)
				{
					const double center[3] = {(b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2};
					ModelToSketch2D(ctx.space, center, out.shape.a);
				}
				return grpc::Status::OK;
			}
			if (ATL::CComQIPtr<IVertex> vertex(out.obj.p); vertex)
			{
				ATL::CComVariant point;
				std::vector<double> p;
				if (SUCCEEDED(vertex->GetPoint(&point)) && (p = comvariant::ToDoubles(point)).size() >= 3)
					ModelToSketch2D(ctx.space, p.data(), out.shape.a);
				out.isPoint = true;
				return grpc::Status::OK;
			}
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"only sketch entities, the origin, and model edges, faces or vertices can be referenced");
		}
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a reference is empty");
		}
	}

	// Adds a resolved ref to the selection.
	bool SelectRef(IModelDocExtension* ext, const ResolvedRef& ref, ISketchPoint* originPoint,
		const std::string& originName)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (ref.origin)
		{
			if (originPoint && SUCCEEDED(originPoint->Select4(VARIANT_TRUE, nullptr, &selected)) && selected)
				return true;
			if (originName.empty())
				return false;
			// The origin's point as the UI names it ("Point1@Origin").
			return SUCCEEDED(ext->SelectByID2(FromUtf8("Point1@" + originName), ATL::CComBSTR(L"EXTSKETCHPOINT"),
				0.0, 0.0, 0.0, VARIANT_TRUE, 0, nullptr, swSelectOptionDefault, &selected)) && selected;
		}
		if (ATL::CComQIPtr<ISketchSegment> segment(ref.obj.p); segment)
			return SUCCEEDED(segment->Select4(VARIANT_TRUE, nullptr, &selected)) && selected;
		if (ATL::CComQIPtr<ISketchPoint> point(ref.obj.p); point)
			return SUCCEEDED(point->Select4(VARIANT_TRUE, nullptr, &selected)) && selected;
		if (ATL::CComQIPtr<IEntity> entity(ref.obj.p); entity)
			return SUCCEEDED(entity->Select4(VARIANT_TRUE, nullptr, &selected)) && selected;
		return false;
	}

	// Picks the sketch a dimension/relation request is about: by name, or the
	// sketch its first sketch item ref belongs to.
	template <typename Constraint>
	grpc::Status ResolveConstraintSketch(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name,
		const google::protobuf::RepeatedPtrField<Constraint>& constraints, ATL::CComPtr<IFeature>& sketch)
	{
		if (!name.empty())
			return FindSketchByName(doc, name, sketch, true);
		for (const auto& constraint : constraints)
			for (const auto& ref : constraint.refs())
				if (ref.has_item())
				{
					ATL::CComPtr<IDispatch> obj;
					if (grpc::Status status = ResolvePersistId(ext, ref.item().sketch_persist_id(), obj); !status.ok())
						return grpc::Status(status.error_code(), "the referenced sketch " + status.error_message());
					sketch = ATL::CComQIPtr<IFeature>(obj.p);
					if (!sketch)
						return grpc::Status(grpc::StatusCode::INTERNAL, "a sketch reference didn't resolve to a feature");
					return grpc::Status::OK;
				}
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"name the sketch, or reference at least one of its entities");
	}

	// Shape checks that need only the request.
	grpc::Status ValidateSketchDimension(const sld_mcp::v1::SketchDimension& dim)
	{
		using K = sld_mcp::v1::DimensionKind;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const int n = dim.refs_size();
		switch (dim.kind())
		{
		case K::DIMENSION_KIND_LENGTH:
		case K::DIMENSION_KIND_DIAMETER:
		case K::DIMENSION_KIND_RADIUS:
			if (n != 1)
				return invalid(std::string("a ") + DimensionKindName(dim.kind()) + " dimension takes 1 entity");
			break;
		case K::DIMENSION_KIND_DISTANCE:
		case K::DIMENSION_KIND_ANGLE:
			if (n != 2)
				return invalid(std::string("a ") + DimensionKindName(dim.kind()) + " dimension takes 2 entities");
			break;
		case K::DIMENSION_KIND_HORIZONTAL:
		case K::DIMENSION_KIND_VERTICAL:
			if (n != 1 && n != 2)
				return invalid(std::string("a ") + DimensionKindName(dim.kind()) +
					" dimension takes 1 line or 2 entities");
			break;
		default:
			return invalid("unknown dimension kind");
		}
		bool anyItem = false;
		for (const auto& ref : dim.refs())
		{
			if (ref.target_case() == sld_mcp::v1::SketchRef::TARGET_NOT_SET)
				return invalid("a reference is empty");
			anyItem = anyItem || ref.has_item();
		}
		if (!anyItem)
			return invalid("a dimension must involve at least one entity of the sketch");
		if (dim.has_value())
		{
			const double v = dim.value();
			if (!std::isfinite(v) || v <= 0.0)
				return invalid("a dimension value must be positive");
			if (dim.kind() == K::DIMENSION_KIND_ANGLE && v >= 2.0 * 3.14159265358979323846)
				return invalid("an angle must be less than 360 degrees");
		}
		return grpc::Status::OK;
	}

	// Shape checks once the refs are resolved.
	grpc::Status CheckDimensionRefs(sld_mcp::v1::DimensionKind kind, const std::vector<ResolvedRef>& refs)
	{
		using K = sld_mcp::v1::DimensionKind;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		switch (kind)
		{
		case K::DIMENSION_KIND_LENGTH:
			if (!refs[0].isLine)
				return invalid("a length dimension needs a line (use distance for other entities)");
			break;
		case K::DIMENSION_KIND_DIAMETER:
		case K::DIMENSION_KIND_RADIUS:
			if (!refs[0].isRound)
				return invalid(std::string("a ") + DimensionKindName(kind) + " dimension needs a circle or an arc");
			break;
		case K::DIMENSION_KIND_ANGLE:
			if (!refs[0].isLine || !refs[1].isLine)
				return invalid("an angle dimension needs 2 lines");
			break;
		case K::DIMENSION_KIND_HORIZONTAL:
		case K::DIMENSION_KIND_VERTICAL:
			if (refs.size() == 1 && !refs[0].isLine)
				return invalid(std::string("a ") + DimensionKindName(kind) +
					" dimension of a single entity needs a line; give 2 entities to measure between them");
			break;
		default:
			break;
		}
		return grpc::Status::OK;
	}

	// The point a ref is measured from: a point, a line's middle, a circle's center.
	void RefAnchor(const RefShape& s, double out[2])
	{
		if (s.kind == RefShape::Line)
		{
			out[0] = (s.a[0] + s.b[0]) / 2.0;
			out[1] = (s.a[1] + s.b[1]) / 2.0;
		}
		else
		{
			out[0] = s.a[0];
			out[1] = s.a[1];
		}
	}

	// Where to put a new dimension's text, in sketch coordinates: off to the
	// side of what it measures. For distances and angles the spot also tells
	// SolidWorks which dimension is meant (aligned, which angle sector).
	grpc::Status PlaceDimension(sld_mcp::v1::DimensionKind kind, const std::vector<ResolvedRef>& refs, double out[2])
	{
		using K = sld_mcp::v1::DimensionKind;
		// Bounding box of everything involved sets the scale of the offset.
		double lo[2] = {HUGE_VAL, HUGE_VAL}, hi[2] = {-HUGE_VAL, -HUGE_VAL};
		auto grow = [&](const double p[2], double r) {
			for (int i = 0; i < 2; ++i)
			{
				lo[i] = std::min(lo[i], p[i] - r);
				hi[i] = std::max(hi[i], p[i] + r);
			}
		};
		for (const auto& ref : refs)
		{
			grow(ref.shape.a, ref.shape.kind == RefShape::Round ? ref.shape.radius : 0.0);
			if (ref.shape.kind == RefShape::Line)
				grow(ref.shape.b, 0.0);
		}
		const double size = std::hypot(hi[0] - lo[0], hi[1] - lo[1]);
		const double offset = std::max(0.2 * size, 0.002);

		switch (kind)
		{
		case K::DIMENSION_KIND_HORIZONTAL:
			out[0] = (lo[0] + hi[0]) / 2.0;
			out[1] = hi[1] + offset;
			return grpc::Status::OK;
		case K::DIMENSION_KIND_VERTICAL:
			out[0] = hi[0] + offset;
			out[1] = (lo[1] + hi[1]) / 2.0;
			return grpc::Status::OK;
		case K::DIMENSION_KIND_DIAMETER:
		case K::DIMENSION_KIND_RADIUS:
		{
			const double d = (refs[0].shape.radius + offset) * std::sqrt(0.5);
			out[0] = refs[0].shape.a[0] + d;
			out[1] = refs[0].shape.a[1] + d;
			return grpc::Status::OK;
		}
		case K::DIMENSION_KIND_ANGLE:
		{
			const RefShape& l1 = refs[0].shape;
			const RefShape& l2 = refs[1].shape;
			const double d1[2] = {l1.b[0] - l1.a[0], l1.b[1] - l1.a[1]};
			const double d2[2] = {l2.b[0] - l2.a[0], l2.b[1] - l2.a[1]};
			const double cross = d1[0] * d2[1] - d1[1] * d2[0];
			const double len1 = std::hypot(d1[0], d1[1]), len2 = std::hypot(d2[0], d2[1]);
			if (len1 <= 0.0 || len2 <= 0.0 || std::abs(cross) < 1e-9 * len1 * len2)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the two lines are parallel -- there's no angle between them (use distance)");
			// Where the (extended) lines meet.
			const double t = ((l2.a[0] - l1.a[0]) * d2[1] - (l2.a[1] - l1.a[1]) * d2[0]) / cross;
			const double x[2] = {l1.a[0] + t * d1[0], l1.a[1] + t * d1[1]};
			// Toward the far end of each line from there: the sector the lines are in.
			auto away = [&](const RefShape& l, double u[2]) {
				const double da = std::hypot(l.a[0] - x[0], l.a[1] - x[1]);
				const double db = std::hypot(l.b[0] - x[0], l.b[1] - x[1]);
				const double* tip = db >= da ? l.b : l.a;
				const double len = std::max(da, db);
				u[0] = (tip[0] - x[0]) / len;
				u[1] = (tip[1] - x[1]) / len;
				return len;
			};
			double u1[2], u2[2];
			const double reach = std::min(away(l1, u1), away(l2, u2));
			double bis[2] = {u1[0] + u2[0], u1[1] + u2[1]};
			double bl = std::hypot(bis[0], bis[1]);
			if (bl < 1e-9)
			{
				bis[0] = -u1[1];
				bis[1] = u1[0];
				bl = 1.0;
			}
			const double dist = std::max(0.5 * reach, offset);
			out[0] = x[0] + bis[0] / bl * dist;
			out[1] = x[1] + bis[1] / bl * dist;
			return grpc::Status::OK;
		}
		case K::DIMENSION_KIND_LENGTH:
		case K::DIMENSION_KIND_DISTANCE:
		default:
		{
			double a[2], b[2];
			if (kind == K::DIMENSION_KIND_LENGTH)
			{
				std::copy(refs[0].shape.a, refs[0].shape.a + 2, a);
				std::copy(refs[0].shape.b, refs[0].shape.b + 2, b);
			}
			else
			{
				RefAnchor(refs[0].shape, a);
				RefAnchor(refs[1].shape, b);
			}
			const double mid[2] = {(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0};
			const double d[2] = {b[0] - a[0], b[1] - a[1]};
			const double len = std::hypot(d[0], d[1]);
			if (len < 1e-12)
			{
				out[0] = mid[0] + offset;
				out[1] = mid[1] + offset;
			}
			else
			{
				// Beside the segment between them, not past either end, so
				// SolidWorks makes an aligned dimension.
				out[0] = mid[0] - d[1] / len * offset;
				out[1] = mid[1] + d[0] / len * offset;
			}
			return grpc::Status::OK;
		}
		}
	}

	const char* RelationKindName(sld_mcp::v1::RelationKind kind)
	{
		switch (kind)
		{
		case sld_mcp::v1::RELATION_KIND_HORIZONTAL:    return "horizontal";
		case sld_mcp::v1::RELATION_KIND_VERTICAL:      return "vertical";
		case sld_mcp::v1::RELATION_KIND_COINCIDENT:    return "coincident";
		case sld_mcp::v1::RELATION_KIND_MIDPOINT:      return "midpoint";
		case sld_mcp::v1::RELATION_KIND_COLLINEAR:     return "collinear";
		case sld_mcp::v1::RELATION_KIND_PARALLEL:      return "parallel";
		case sld_mcp::v1::RELATION_KIND_PERPENDICULAR: return "perpendicular";
		case sld_mcp::v1::RELATION_KIND_TANGENT:       return "tangent";
		case sld_mcp::v1::RELATION_KIND_CONCENTRIC:    return "concentric";
		case sld_mcp::v1::RELATION_KIND_CORADIAL:      return "coradial";
		case sld_mcp::v1::RELATION_KIND_EQUAL:         return "equal";
		case sld_mcp::v1::RELATION_KIND_FIXED:         return "fixed";
		case sld_mcp::v1::RELATION_KIND_SYMMETRIC:     return "symmetric";
		case sld_mcp::v1::RELATION_KIND_MERGE:         return "merge";
		case sld_mcp::v1::RELATION_KIND_ALONG_X:       return "along_x";
		case sld_mcp::v1::RELATION_KIND_ALONG_Y:       return "along_y";
		case sld_mcp::v1::RELATION_KIND_ALONG_Z:       return "along_z";
		default:                                        return "unknown";
		}
	}

	grpc::Status ValidateSketchRelation(const sld_mcp::v1::SketchRelation& relation)
	{
		using K = sld_mcp::v1::RelationKind;
		const int n = relation.refs_size();
		int lo = 2, hi = 2;
		switch (relation.kind())
		{
		case K::RELATION_KIND_HORIZONTAL:
		case K::RELATION_KIND_VERTICAL:
		case K::RELATION_KIND_FIXED:
		case K::RELATION_KIND_ALONG_X:
		case K::RELATION_KIND_ALONG_Y:
		case K::RELATION_KIND_ALONG_Z:
			lo = 1;
			hi = kMaxRelationRefs;
			break;
		case K::RELATION_KIND_COLLINEAR:
		case K::RELATION_KIND_PARALLEL:
		case K::RELATION_KIND_CONCENTRIC:
		case K::RELATION_KIND_EQUAL:
			hi = kMaxRelationRefs;
			break;
		case K::RELATION_KIND_SYMMETRIC:
			lo = hi = 3;
			break;
		case K::RELATION_KIND_COINCIDENT:
		case K::RELATION_KIND_MIDPOINT:
		case K::RELATION_KIND_PERPENDICULAR:
		case K::RELATION_KIND_TANGENT:
		case K::RELATION_KIND_CORADIAL:
		case K::RELATION_KIND_MERGE:
			break;
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown relation kind");
		}
		if (n < lo || n > hi)
		{
			std::string want = lo == hi ? std::to_string(lo) : std::to_string(lo) + " to " + std::to_string(hi);
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				std::string("a ") + RelationKindName(relation.kind()) + " relation takes " + want + " entities");
		}
		bool anyItem = false;
		for (const auto& ref : relation.refs())
		{
			if (ref.target_case() == sld_mcp::v1::SketchRef::TARGET_NOT_SET)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a reference is empty");
			anyItem = anyItem || ref.has_item();
		}
		if (!anyItem)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a relation must involve at least one entity of the sketch");
		return grpc::Status::OK;
	}

	// The IModelDoc2::SketchAddConstraints command for a relation.
	const wchar_t* RelationCommand(sld_mcp::v1::RelationKind kind, bool allPoints)
	{
		switch (kind)
		{
		case sld_mcp::v1::RELATION_KIND_HORIZONTAL:    return allPoints ? L"sgHORIZPOINTS2D" : L"sgHORIZONTAL2D";
		case sld_mcp::v1::RELATION_KIND_VERTICAL:      return allPoints ? L"sgVERTPOINTS2D" : L"sgVERTICAL2D";
		case sld_mcp::v1::RELATION_KIND_COINCIDENT:    return L"sgCOINCIDENT";
		case sld_mcp::v1::RELATION_KIND_MIDPOINT:      return L"sgATMIDDLE";
		case sld_mcp::v1::RELATION_KIND_COLLINEAR:     return L"sgCOLINEAR";
		case sld_mcp::v1::RELATION_KIND_PARALLEL:      return L"sgPARALLEL";
		case sld_mcp::v1::RELATION_KIND_PERPENDICULAR: return L"sgPERPENDICULAR";
		case sld_mcp::v1::RELATION_KIND_TANGENT:       return L"sgTANGENT";
		case sld_mcp::v1::RELATION_KIND_CONCENTRIC:    return L"sgCONCENTRIC";
		case sld_mcp::v1::RELATION_KIND_CORADIAL:      return L"sgCORADIAL";
		case sld_mcp::v1::RELATION_KIND_EQUAL:         return L"sgSAMELENGTH";
		case sld_mcp::v1::RELATION_KIND_FIXED:         return L"sgFIXED";
		case sld_mcp::v1::RELATION_KIND_SYMMETRIC:     return L"sgSYMMETRIC";
		case sld_mcp::v1::RELATION_KIND_MERGE:         return L"sgMERGEPOINTS";
		case sld_mcp::v1::RELATION_KIND_ALONG_X:       return L"sgALONGX3D";
		case sld_mcp::v1::RELATION_KIND_ALONG_Y:       return L"sgALONGY3D";
		case sld_mcp::v1::RELATION_KIND_ALONG_Z:       return L"sgALONGZ3D";
		default:                                        return nullptr;
		}
	}

	bool IsAlongAxis(sld_mcp::v1::RelationKind kind)
	{
		return kind == sld_mcp::v1::RELATION_KIND_ALONG_X || kind == sld_mcp::v1::RELATION_KIND_ALONG_Y ||
			kind == sld_mcp::v1::RELATION_KIND_ALONG_Z;
	}

	// Deletes the relations past the first `keep`, newest first. Best effort:
	// relies on SolidWorks listing relations in the order they were made.
	void DeleteRelationsAfter(ISketch* sketch, size_t keep)
	{
		ATL::CComPtr<ISketchRelationManager> manager;
		if (FAILED(sketch->get_RelationManager(&manager)) || !manager)
			return;
		const auto relations = SketchRelations(sketch);
		for (size_t i = relations.size(); i > keep; --i)
		{
			VARIANT_BOOL deleted = VARIANT_FALSE;
			manager->DeleteRelation(relations[i - 1], &deleted);
		}
	}

	// What FullyDefineSketch's error codes mean (swAutodimStatus_e).
	std::string AutodimError(long code)
	{
		switch (code)
		{
		case swAutodimStatusSketchIsEmpty:           return "the sketch is empty";
		case swAutodimStatusSketchIsOverDefined:     return "the sketch is over defined -- remove the conflicting dimension or relation first";
		case swAutodimStatusNoEntities:              return "there's nothing left to define";
		case swAutodimStatusEntitiesNotValid:        return "the sketch has entities SolidWorks can't auto-dimension";
		case swAutodimStatusDatumNotSupplied:        return "no datum was given";
		case swAutodimStatusDatumNotUnique:          return "the datum is ambiguous";
		case swAutodimStatusDatumNotValidType:       return "a datum must be a point, a line or the origin";
		case swAutodimStatusDatumLineNotCenterline:  return "a datum line must be a construction line";
		case swAutodimStatusDatumLineNotVertical:    return "the horizontal-dimension datum line must be vertical";
		case swAutodimStatusDatumLineNotHorizontal:  return "the vertical-dimension datum line must be horizontal";
		case swAutodimStatusAlgorithmFailed:         return "SolidWorks' algorithm failed on this sketch";
		case swAutodimStatusSketchNoSolutionFound:   return "the sketch has no solution";
		case swAutodimStatusCenterlineNotAllowed:    return "a centerline isn't allowed here";
		default:                                     return "error code " + std::to_string(code);
		}
	}
}

grpc::Status SolidWorksService::AddSketchDimensions(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddSketchDimensionsRequest* request,
	sld_mcp::v1::AddSketchDimensionsResponse* response)
{
	// Validate before touching SolidWorks.
	const int count = request->dimensions_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one dimension");
	if (count > kMaxSketchConstraints)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many dimensions in one call (max 200)");
	auto where = [](int i) { return "dimension " + std::to_string(i + 1); };
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = ValidateSketchDimension(request->dimensions(i)); !status.ok())
			return grpc::Status(status.error_code(), where(i) + ": " + status.error_message());

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

	// Resolve everything that can fail before the sketch is opened.
	ATL::CComPtr<IFeature> sketchFeature;
	if (grpc::Status status = ResolveConstraintSketch(doc, ext, request->sketch_name(), request->dimensions(),
		sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = CheckRefsInSketch(ext, request->dimensions(i).refs(), sketchPid, sketchName,
			where(i)); !status.ok())
			return status;

	ATL::CComPtr<ISketchPoint> originPoint;
	std::string originName;
	FindOrigin(doc, originPoint, originName);

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;
	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;

	PreferenceToggles toggles(swApp);
	SilenceDimensionPrompts(toggles);

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
		return status;

	// The sketch is open: the work below returns its status instead of
	// returning from the RPC, so the sketch is always closed after.
	space.Attach(sketch);
	std::vector<ATL::CComPtr<IDisplayDimension>> created;
	std::vector<double> originalValues;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems items;
		items.Load(sketch);
		const RefContext ctx{ext, items, space, sketchPid, sketchName};

		// Rolls back: values first (moving the geometry back), then the
		// dimensions themselves.
		auto rollback = [&](const grpc::Status& why) {
			for (size_t i = created.size(); i-- > 0;)
			{
				ATL::CComPtr<IDimension> dim;
				long result = 0;
				if (i < originalValues.size() && SUCCEEDED(created[i]->GetDimension2(0, &dim)) && dim)
					dim->SetSystemValue3(originalValues[i], swSetValue_UseCurrentSetting, ATL::CComVariant(), &result);
			}
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			DeleteDimensions(doc, created);
			return why;
		};

		// Create every dimension at the geometry's current value first, so
		// setting values afterwards solves with all of them in place.
		for (int i = 0; i < count; ++i)
		{
			const auto& dimension = request->dimensions(i);
			if (IsSketch3D(sketch) && (dimension.kind() == sld_mcp::v1::DIMENSION_KIND_HORIZONTAL ||
				dimension.kind() == sld_mcp::v1::DIMENSION_KIND_VERTICAL))
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": a 3D sketch has no horizontal or vertical -- use length or distance"));
			std::vector<ResolvedRef> refs(dimension.refs_size());
			for (int r = 0; r < dimension.refs_size(); ++r)
				if (grpc::Status status = ResolveRef(ctx, dimension.refs(r), refs[r]); !status.ok())
					return rollback(grpc::Status(status.error_code(),
						where(i) + ", reference " + std::to_string(r + 1) + ": " + status.error_message()));
			if (grpc::Status status = CheckDimensionRefs(dimension.kind(), refs); !status.ok())
				return rollback(grpc::Status(status.error_code(), where(i) + ": " + status.error_message()));

			double text2d[2];
			if (grpc::Status status = PlaceDimension(dimension.kind(), refs, text2d); !status.ok())
				return rollback(grpc::Status(status.error_code(), where(i) + ": " + status.error_message()));
			const double textSketch[3] = {text2d[0], text2d[1], 0.0};
			double text[3];
			if (!space.ToModel(textSketch, text))
				return rollback(grpc::Status(grpc::StatusCode::INTERNAL,
					"couldn't map a point out of the sketch's coordinates"));

			doc->ClearSelection2(VARIANT_TRUE);
			for (int r = 0; r < static_cast<int>(refs.size()); ++r)
				if (!SelectRef(ext, refs[r], originPoint, originName))
					return rollback(grpc::Status(grpc::StatusCode::INTERNAL,
						where(i) + ": SolidWorks wouldn't select reference " + std::to_string(r + 1)));

			ATL::CComPtr<IDispatch> dimDisp;
			const HRESULT hr = CreateDimension(doc, dimension.kind(), text, dimDisp);
			doc->ClearSelection2(VARIANT_TRUE);
			ATL::CComQIPtr<IDisplayDimension> display(dimDisp.p);
			if (FAILED(hr))
				return rollback(ComError("IModelDoc2::Add*Dimension2", hr));
			if (!display)
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": SolidWorks couldn't make a " + DimensionKindName(dimension.kind()) +
					" dimension from those entities"));
			created.push_back(display);

			ATL::CComPtr<IDimension> dim;
			if (FAILED(display->GetDimension2(0, &dim)) || !dim)
				return rollback(grpc::Status(grpc::StatusCode::INTERNAL, where(i) + ": the new dimension has no value"));
			double value = 0.0;
			dim->get_SystemValue(&value);
			originalValues.push_back(value);
			long driven = swDimensionDrivenUnknown;
			if (SUCCEEDED(dim->get_DrivenState(&driven)) && driven == swDimensionDriven)
				return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where(i) + ": that would over-define the sketch -- what it measures is already fixed by "
					"other dimensions or relations"));
		}

		// Now drive them to the requested values.
		bool anyValue = false;
		for (int i = 0; i < count; ++i)
		{
			if (!request->dimensions(i).has_value())
				continue;
			anyValue = true;
			ATL::CComPtr<IDimension> dim;
			long result = swSetValue_Failure;
			if (FAILED(created[i]->GetDimension2(0, &dim)) || !dim ||
				FAILED(dim->SetSystemValue3(request->dimensions(i).value(), swSetValue_UseCurrentSetting,
					ATL::CComVariant(), &result)) || result != swSetValue_Successful)
				return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where(i) + ": SolidWorks wouldn't take that value (error " + std::to_string(result) + ")"));
		}
		if (anyValue)
		{
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
		}

		const sld_mcp::v1::DefinitionStatus status = SketchStatus(sketch);
		if (status == sld_mcp::v1::DEFINITION_STATUS_OVER || status == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION ||
			status == sld_mcp::v1::DEFINITION_STATUS_INVALID)
			return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("with those values the sketch is ") + DefinitionStatusText(status) +
				" -- nothing was changed; check the values against the sketch's other dimensions"));

		for (const auto& display : created)
			DescribeDimension(display, response->add_dimensions());
		ReadSketchState(sketch, sketchPid, response->mutable_state());
		return grpc::Status::OK;
	}();

	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	response->set_sketch_name(sketchName);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddSketchRelations(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddSketchRelationsRequest* request,
	sld_mcp::v1::AddSketchRelationsResponse* response)
{
	const int count = request->relations_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one relation");
	if (count > kMaxSketchConstraints)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many relations in one call (max 200)");
	auto where = [](int i) { return "relation " + std::to_string(i + 1); };
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = ValidateSketchRelation(request->relations(i)); !status.ok())
			return grpc::Status(status.error_code(), where(i) + ": " + status.error_message());

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
	if (grpc::Status status = ResolveConstraintSketch(doc, ext, request->sketch_name(), request->relations(),
		sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = CheckRefsInSketch(ext, request->relations(i).refs(), sketchPid, sketchName,
			where(i)); !status.ok())
			return status;

	ATL::CComPtr<ISketchPoint> originPoint;
	std::string originName;
	FindOrigin(doc, originPoint, originName);

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;
	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
		return status;

	space.Attach(sketch);
	int added = 0;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems items;
		items.Load(sketch);
		const RefContext ctx{ext, items, space, sketchPid, sketchName};
		const size_t before = SketchRelations(sketch).size();
		auto rollback = [&](const grpc::Status& why) {
			DeleteRelationsAfter(sketch, before);
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			return why;
		};

		for (int i = 0; i < count; ++i)
		{
			const auto& relation = request->relations(i);
			const bool is3D = IsSketch3D(sketch);
			if (IsAlongAxis(relation.kind()) && !is3D)
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": along_x / along_y / along_z are for 3D sketches -- use horizontal or vertical"));
			if (is3D && (relation.kind() == sld_mcp::v1::RELATION_KIND_HORIZONTAL ||
				relation.kind() == sld_mcp::v1::RELATION_KIND_VERTICAL))
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": a 3D sketch has no horizontal or vertical -- use along_x, along_y or along_z"));
			std::vector<ResolvedRef> refs(relation.refs_size());
			bool allPoints = true;
			for (int r = 0; r < relation.refs_size(); ++r)
			{
				if (grpc::Status status = ResolveRef(ctx, relation.refs(r), refs[r]); !status.ok())
					return rollback(grpc::Status(status.error_code(),
						where(i) + ", reference " + std::to_string(r + 1) + ": " + status.error_message()));
				allPoints = allPoints && refs[r].isPoint;
			}
			if (relation.kind() == sld_mcp::v1::RELATION_KIND_SYMMETRIC && !refs.back().isLine)
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": the last entity of a symmetric relation must be the line to mirror about"));
			// Horizontal/vertical between points needs exactly two of them.
			if (allPoints && refs.size() != 2 &&
				(relation.kind() == sld_mcp::v1::RELATION_KIND_HORIZONTAL ||
				 relation.kind() == sld_mcp::v1::RELATION_KIND_VERTICAL))
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": horizontal/vertical relations take lines, or exactly 2 points"));

			doc->ClearSelection2(VARIANT_TRUE);
			for (int r = 0; r < static_cast<int>(refs.size()); ++r)
				if (!SelectRef(ext, refs[r], originPoint, originName))
					return rollback(grpc::Status(grpc::StatusCode::INTERNAL,
						where(i) + ": SolidWorks wouldn't select reference " + std::to_string(r + 1)));

			const size_t relationsBefore = SketchRelations(sketch).size();
			const HRESULT hr = doc->SketchAddConstraints(ATL::CComBSTR(RelationCommand(relation.kind(), allPoints)));
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return rollback(ComError("IModelDoc2::SketchAddConstraints", hr));
			if (SketchRelations(sketch).size() <= relationsBefore)
				return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where(i) + ": SolidWorks didn't add the " + RelationKindName(relation.kind()) +
					" relation -- the entities may not suit it, already have it, or it would over-define "
					"the sketch"));
			++added;
		}

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		const sld_mcp::v1::DefinitionStatus status = SketchStatus(sketch);
		if (status == sld_mcp::v1::DEFINITION_STATUS_OVER || status == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION ||
			status == sld_mcp::v1::DEFINITION_STATUS_INVALID)
			return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("with those relations the sketch is ") + DefinitionStatusText(status) +
				" -- they were removed again"));

		ReadSketchState(sketch, sketchPid, response->mutable_state());
		return grpc::Status::OK;
	}();

	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	response->set_sketch_name(sketchName);
	response->set_added(added);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::FullyDefineSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::FullyDefineSketchRequest* request,
	sld_mcp::v1::FullyDefineSketchResponse* response)
{
	if (request->skip_relations() && request->skip_dimensions())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"skipping both relations and dimensions leaves nothing to add");
	long scheme = swAutodimSchemeBaseline;
	switch (request->scheme())
	{
	case sld_mcp::v1::DIMENSION_SCHEME_UNSPECIFIED:
	case sld_mcp::v1::DIMENSION_SCHEME_BASELINE: scheme = swAutodimSchemeBaseline; break;
	case sld_mcp::v1::DIMENSION_SCHEME_CHAIN:    scheme = swAutodimSchemeChain; break;
	case sld_mcp::v1::DIMENSION_SCHEME_ORDINATE: scheme = swAutodimSchemeOrdinate; break;
	default:
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown dimension scheme");
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

	ATL::CComPtr<IFeature> sketchFeature;
	if (grpc::Status status = ResolveSketchFeature(doc, request->sketch_name(), sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);

	// Datums, as refs (checked like a dimension's).
	google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> datums;
	if (request->has_horizontal_datum())
		*datums.Add() = request->horizontal_datum();
	if (request->has_vertical_datum())
		*datums.Add() = request->vertical_datum();
	if (grpc::Status status = CheckRefsInSketch(ext, datums, sketchPid, sketchName, "datum"); !status.ok())
		return status;

	ATL::CComPtr<ISketchPoint> originPoint;
	std::string originName;
	FindOrigin(doc, originPoint, originName);

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;
	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;

	const std::set<std::string> dimensionsBefore = DimensionNames(sketchFeature);

	PreferenceToggles toggles(swApp);
	SilenceDimensionPrompts(toggles);

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
		return status;

	space.Attach(sketch);
	size_t relationsBefore = 0;
	const grpc::Status failure = [&]() -> grpc::Status {
		relationsBefore = SketchRelations(sketch).size();
		const sld_mcp::v1::DefinitionStatus initial = SketchStatus(sketch);
		if (initial == sld_mcp::v1::DEFINITION_STATUS_FULLY)
			return grpc::Status::OK;  // nothing to do
		if (initial == sld_mcp::v1::DEFINITION_STATUS_OVER || initial == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("the sketch is ") + DefinitionStatusText(initial) +
				" -- fix the conflicting dimensions or relations first");

		SketchItems items;
		items.Load(sketch);
		const RefContext ctx{ext, items, space, sketchPid, sketchName};
		ATL::CComPtr<IDispatch> datum[2];  // horizontal, vertical; null = sketch origin
		const sld_mcp::v1::SketchRef* datumRefs[2] = {
			request->has_horizontal_datum() ? &request->horizontal_datum() : nullptr,
			request->has_vertical_datum() ? &request->vertical_datum() : nullptr};
		for (int d = 0; d < 2; ++d)
		{
			if (!datumRefs[d] || datumRefs[d]->has_origin())
				continue;
			ResolvedRef ref;
			if (grpc::Status status = ResolveRef(ctx, *datumRefs[d], ref); !status.ok())
				return grpc::Status(status.error_code(),
					std::string(d == 0 ? "horizontal" : "vertical") + " datum: " + status.error_message());
			datum[d] = ref.obj;
		}

		const VARIANT_BOOL relations = request->skip_relations() ? VARIANT_FALSE : VARIANT_TRUE;
		const VARIANT_BOOL dimensions = request->skip_dimensions() ? VARIANT_FALSE : VARIANT_TRUE;
		const long allRelations = swSketchFullyDefineRelationType_Equal | swSketchFullyDefineRelationType_Horizontal |
			swSketchFullyDefineRelationType_Vertical | swSketchFullyDefineRelationType_Tangent |
			swSketchFullyDefineRelationType_Perpendicular | swSketchFullyDefineRelationType_Colinear |
			swSketchFullyDefineRelationType_Concentric | swSketchFullyDefineRelationType_Parallel |
			swSketchFullyDefineRelationType_Midpoint | swSketchFullyDefineRelationType_Coincident;
		auto run = [&](VARIANT_BOOL allEntities, long& code) {
			doc->ClearSelection2(VARIANT_TRUE);
			if (allEntities == VARIANT_FALSE)
				for (const auto& entry : items.All())
				{
					VARIANT_BOOL selected = VARIANT_FALSE;
					if (ATL::CComQIPtr<ISketchSegment> segment(entry.obj.p); segment)
						segment->Select4(VARIANT_TRUE, nullptr, &selected);
				}
			code = swAutodimStatusSuccess;
			const HRESULT hr = sketchManager->FullyDefineSketch(allEntities, relations, allRelations, dimensions,
				scheme, datum[0], scheme, datum[1], swAutodimHorizontalPlacementBelow,
				swAutodimVerticalPlacementLeft, &code);
			doc->ClearSelection2(VARIANT_TRUE);
			return hr;
		};

		long code = swAutodimStatusSuccess;
		HRESULT hr = run(VARIANT_TRUE, code);
		// If SolidWorks wants the entities selected after all, select them all.
		if (SUCCEEDED(hr) && code == swAutodimStatusNoEntities)
			hr = run(VARIANT_FALSE, code);
		// If it insists on explicit datums, use the origin for the missing ones.
		if (SUCCEEDED(hr) && code == swAutodimStatusDatumNotSupplied && originPoint)
		{
			for (auto& d : datum)
				if (!d)
					d = originPoint.p;
			hr = run(VARIANT_TRUE, code);
		}
		if (FAILED(hr))
			return ComError("ISketchManager::FullyDefineSketch", hr);
		if (code != swAutodimStatusSuccess)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't fully define the sketch: " + AutodimError(code));

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		ReadSketchState(sketch, sketchPid, response->mutable_state());
		return grpc::Status::OK;
	}();

	if (failure.ok() && !response->has_state())
		ReadSketchState(sketch, sketchPid, response->mutable_state());
	const size_t relationsAfter = SketchRelations(sketch).size();

	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	// What it added, read from the closed sketch.
	for (const auto& display : FeatureDimensions(sketchFeature))
	{
		sld_mcp::v1::DimensionInfo info;
		if (DescribeDimension(display, &info) && !dimensionsBefore.count(info.name()))
			response->add_added_dimensions()->Swap(&info);
	}
	response->set_added_relations(static_cast<int32_t>(relationsAfter > relationsBefore ? relationsAfter - relationsBefore : 0));
	response->set_sketch_name(sketchName);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::SetDimensionValue(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetDimensionValueRequest* request,
	sld_mcp::v1::SetDimensionValueResponse* response)
{
	const bool angle = request->value_case() == sld_mcp::v1::SetDimensionValueRequest::kAngle;
	if (request->value_case() == sld_mcp::v1::SetDimensionValueRequest::VALUE_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a length or an angle");
	const double value = angle ? request->angle() : request->length();
	if (!std::isfinite(value) || value <= 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the value must be positive");
	if (request->name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the dimension's name, e.g. D1@Sketch1");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditableModel(swApp, doc); !status.ok())
		return status;

	ATL::CComPtr<IDispatch> param;
	HRESULT hr = doc->Parameter(FromUtf8(request->name()), &param);
	if (FAILED(hr))
		return ComError("IModelDoc2::Parameter", hr);
	ATL::CComQIPtr<IDimension> dim(param.p);
	if (!dim)
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			"the active document has no dimension named \"" + request->name() + "\" -- names look like "
			"D1@Sketch1");

	long paramType = swDimensionParamTypeUnknown;
	dim->GetType(&paramType);
	if (paramType == swDimensionParamTypeDoubleAngular && !angle)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + request->name() + "\" is an angle; give degrees");
	if (paramType == swDimensionParamTypeDoubleLinear && angle)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + request->name() + "\" is a length; give millimeters");

	long result = swSetValue_Failure;
	hr = dim->SetSystemValue3(value, swSetValue_UseCurrentSetting, ATL::CComVariant(), &result);
	if (FAILED(hr))
		return ComError("IDimension::SetSystemValue3", hr);
	switch (result)
	{
	case swSetValue_Successful:
		break;
	case swSetValue_DrivenDimension:
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"\"" + request->name() + "\" is a driven (reference) dimension; it only measures");
	case swSetValue_InvalidValue:
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks rejected that value");
	default:
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't set \"" + request->name() + "\" (error " + std::to_string(result) + ")");
	}

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	hr = doc->EditRebuild3(&rebuilt);
	if (FAILED(hr))
		return ComError("IModelDoc2::EditRebuild3", hr);

	sld_mcp::v1::DimensionInfo* info = response->mutable_dimension();
	DescribeDimensionValue(dim, info);
	info->set_type(paramType == swDimensionParamTypeDoubleAngular ? sld_mcp::v1::DIMENSION_TYPE_ANGULAR
	                                                              : sld_mcp::v1::DIMENSION_TYPE_LINEAR);
	return grpc::Status::OK;
}
