// SketchEditing.cpp : Changing existing entities of a sketch: SketchCorners (fillets, chamfers),
// TrimSketchEntity (trim, extend).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// More than anyone rounds in one go.
	constexpr int kMaxCorners = 200;

	// Geometry closer than this (meters) hasn't changed.
	constexpr double kEditTolerance = 1e-9;

	constexpr double kPi = 3.14159265358979323846;

	double Length(const sld_mcp::v1::Vec3& a, const sld_mcp::v1::Vec3& b)
	{
		return std::hypot(a.x() - b.x(), a.y() - b.y());
	}

	bool Moved(bool hadA, const sld_mcp::v1::Vec3& a, bool hadB, const sld_mcp::v1::Vec3& b)
	{
		return hadA != hadB || (hadA && Length(a, b) > kEditTolerance);
	}

	bool SameGeometry(const sld_mcp::v1::SketchItem& a, const sld_mcp::v1::SketchItem& b)
	{
		return a.type() == b.type() && std::abs(a.length() - b.length()) <= kEditTolerance &&
			!Moved(a.has_start(), a.start(), b.has_start(), b.start()) &&
			!Moved(a.has_end(), a.end(), b.has_end(), b.end()) &&
			!Moved(a.has_center(), a.center(), b.has_center(), b.center());
	}

	// The segments of a sketch as they are now.
	std::map<SketchItemKey, sld_mcp::v1::SketchItem> SegmentsOf(const SketchItems& items)
	{
		std::map<SketchItemKey, sld_mcp::v1::SketchItem> segments;
		for (const auto& e : items.All())
		{
			sld_mcp::v1::SketchItem item;
			if (!e.point && DescribeSketchItem(e.obj, std::string(), &item))
				segments.emplace(SketchKeyOf(e), std::move(item));
		}
		return segments;
	}

	// What's in the open sketch while an edit runs.
	struct OpenEdit
	{
		IModelDoc2* doc;
		ISketchManager* sketchManager;
		ISketch* sketch;
		SketchItems items;  // as the edit started
		std::map<SketchItemKey, sld_mcp::v1::SketchItem> before;

		// Whether any segment changed, appeared or went since the edit started.
		bool Changed() const
		{
			SketchItems now;
			now.Load(sketch);
			const auto after = SegmentsOf(now);
			if (after.size() != before.size())
				return true;
			for (const auto& [key, item] : after)
				if (auto old = before.find(key); old == before.end() || !SameGeometry(old->second, item))
					return true;
			return false;
		}

		// How many new segments of a kind there are.
		int CountNew(long type) const
		{
			SketchItems now;
			now.Load(sketch);
			int count = 0;
			for (const auto& e : now.All())
				if (!e.point && e.type == type && !before.count(SketchKeyOf(e)))
					++count;
			return count;
		}

		// A segment as it was when the edit started (null if it wasn't there).
		const sld_mcp::v1::SketchItem* Old(const sld_mcp::v1::SketchItemRef& ref) const
		{
			auto it = before.find(SketchKeyOf(ref));
			return it == before.end() ? nullptr : &it->second;
		}

		// Takes back everything since the edit started (seen 2026-10-31: one step,
		// whatever a call made, restores the segments, relations and dimensions).
		void Undo() const
		{
			doc->ClearSelection2(VARIANT_TRUE);
			doc->EditUndo2(1);
		}
	};

	// Opens the sketch the refs are in, checks them, runs `edit` on it, closes it
	// and reports. Closed without the exit rebuild, as MirrorSketchEntities does: a
	// change that breaks the profile of the sketch's feature would open the Repair
	// Sketch dialog with InsertSketch(TRUE).
	template <class Edit>
	grpc::Status RunSketchEdit(ISldWorks* swApp, const std::string& requestedSketch,
		const std::vector<sld_mcp::v1::SketchItemRef>& refs, sld_mcp::v1::SketchEditResponse* response, Edit edit)
	{
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		// Check everything against the closed sketch before opening it.
		ATL::CComPtr<IFeature> sketchFeature;
		if (grpc::Status status = ResolveItemsSketch(doc, ext, requestedSketch, refs.front(), sketchFeature); !status.ok())
			return status;
		const std::string sketchName = FeatureName(sketchFeature);
		const std::string pid = PersistIdOf(ext, sketchFeature);
		{
			ATL::CComPtr<ISketch> closed;
			if (grpc::Status status = SketchOf(sketchFeature, closed); !status.ok())
				return status;
			SketchItems items;
			items.Load(closed);
			for (size_t i = 0; i < refs.size(); ++i)
			{
				const std::string where = "entity " + std::to_string(i + 1);
				google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> one;
				*one.Add()->mutable_item() = refs[i];
				if (grpc::Status status = CheckRefsInSketch(ext, one, pid, sketchName, where); !status.ok())
					return status;
				if (!items.Find(refs[i]))
					return grpc::Status(grpc::StatusCode::NOT_FOUND,
						where + " isn't in sketch \"" + sketchName + "\" any more (deleted or merged?) -- inspect the "
						"sketch again for current IDs");
			}
		}

		std::set<std::string> dimensionsBefore;
		for (const auto& display : FeatureDimensions(sketchFeature))
		{
			sld_mcp::v1::DimensionInfo info;
			if (DescribeDimension(display, &info))
				dimensionsBefore.insert(info.name());
		}
		const auto errorsBefore = FeatureErrors(FeatureTree(doc));

		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);

		ATL::CComPtr<ISketchManager> sketchManager;
		if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
			return status;
		ATL::CComPtr<ISketch> openSketch;
		if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, openSketch); !status.ok())
			return status;

		OpenEdit open{doc, sketchManager, openSketch};
		open.items.Load(openSketch);
		open.before = SegmentsOf(open.items);
		const std::set<SketchItemKey> keysBefore = SketchKeys(open.items);

		doc->ClearSelection2(VARIANT_TRUE);
		const grpc::Status failure = edit(open);
		doc->ClearSelection2(VARIANT_TRUE);

		std::set<SketchItemKey> added;
		std::vector<sld_mcp::v1::SketchItemRef> gone;
		if (failure.ok())
		{
			SketchItems now;
			now.Load(openSketch);
			for (const auto& e : now.All())
				if (!keysBefore.count(SketchKeyOf(e)))
					added.insert(SketchKeyOf(e));
			const std::set<SketchItemKey> keysNow = SketchKeys(now);
			for (const auto& [key, item] : open.before)
				if (!keysNow.count(key))
					gone.push_back(item.ref());
		}

		const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_FALSE);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		if (!failure.ok())
			return failure;
		if (FAILED(closeHr))
			return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"the sketch was changed, but SolidWorks left \"" + sketchName + "\" open for editing");

		ATL::CComPtr<ISketch> sketch;
		if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
			return status;
		const std::string sketchPid = PersistIdOf(ext, sketchFeature);
		response->set_sketch_name(sketchName);
		SketchItems after;
		after.Load(sketch);
		DescribeAddedItems(after, added, sketchPid, true, response->mutable_items());
		for (const auto& e : after.All())
		{
			sld_mcp::v1::SketchItem item;
			if (e.point || added.count(SketchKeyOf(e)) || !DescribeSketchItem(e.obj, sketchPid, &item))
				continue;
			if (auto old = open.before.find(SketchKeyOf(e)); old != open.before.end() && !SameGeometry(old->second, item))
				*response->add_changed() = item;
		}
		for (auto ref : gone)
		{
			ref.set_sketch_persist_id(sketchPid);
			*response->add_deleted() = ref;
		}
		for (const auto& display : FeatureDimensions(sketchFeature))
		{
			sld_mcp::v1::DimensionInfo info;
			if (DescribeDimension(display, &info) && !dimensionsBefore.count(info.name()))
				*response->add_dimensions() = info;
		}
		sld_mcp::v1::SketchContours contours;
		if (CountContours(sketch, contours))
			*response->mutable_contours() = contours;
		ReadSketchState(sketch, sketchPid, response->mutable_state());
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		return grpc::Status::OK;
	}

	grpc::Status ValidateCorners(const sld_mcp::v1::SketchCornersRequest& request)
	{
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const int count = request.items_size();
		if (count == 0)
			return invalid("give the corners: sketch points, or two segments");
		if (count > kMaxCorners)
			return invalid("too many corners in one call (max " + std::to_string(kMaxCorners) + ")");
		int points = 0;
		for (const auto& item : request.items())
			points += item.point() ? 1 : 0;
		if (points != 0 && points != count)
			return invalid("give sketch points or two segments, not both");
		if (points == 0 && count != 2)
			return invalid("two segments make one corner -- give exactly two, or the corners' sketch points");
		switch (request.kind_case())
		{
		case sld_mcp::v1::SketchCornersRequest::kFilletRadius:
			if (!(std::isfinite(request.fillet_radius()) && request.fillet_radius() > 0.0))
				return invalid("a fillet's radius must be more than 0");
			return grpc::Status::OK;
		case sld_mcp::v1::SketchCornersRequest::kChamfer:
		{
			const auto& chamfer = request.chamfer();
			if (!(std::isfinite(chamfer.distance()) && chamfer.distance() > 0.0))
				return invalid("a chamfer's distance must be more than 0");
			if (chamfer.kind() == sld_mcp::v1::SKETCH_CHAMFER_KIND_EQUAL)
				return grpc::Status::OK;
			if (chamfer.kind() != sld_mcp::v1::SKETCH_CHAMFER_KIND_DISTANCES &&
				chamfer.kind() != sld_mcp::v1::SKETCH_CHAMFER_KIND_ANGLE)
				return invalid("a chamfer's kind must be EQUAL, DISTANCES or ANGLE");
			if (points > 0)
				return invalid("a chamfer with two different sides needs the two segments, to say which is which");
			if (!(std::isfinite(chamfer.second()) && chamfer.second() > 0.0))
				return invalid("a chamfer's second distance or angle must be more than 0");
			if (chamfer.kind() == sld_mcp::v1::SKETCH_CHAMFER_KIND_ANGLE && chamfer.second() >= kPi)
				return invalid("a chamfer's angle must be under 180 degrees");
			return grpc::Status::OK;
		}
		default:
			return invalid("say what to make: a fillet or a chamfer");
		}
	}
}

grpc::Status SolidWorksService::SketchCorners(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SketchCornersRequest* request,
	sld_mcp::v1::SketchEditResponse* response)
{
	if (grpc::Status status = ValidateCorners(*request); !status.ok())
		return status;
	const bool fillet = request->has_fillet_radius();
	std::vector<sld_mcp::v1::SketchItemRef> refs(request->items().begin(), request->items().end());
	const int corners = refs.front().point() ? static_cast<int>(refs.size()) : 1;

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();
	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	return RunSketchEdit(swApp, request->sketch_name(), refs, response, [&](OpenEdit& open) -> grpc::Status {
		// SolidWorks puts the first distance on the segment selected last.
		std::vector<sld_mcp::v1::SketchItemRef> order = refs;
		if (!fillet && !refs.front().point())
			std::reverse(order.begin(), order.end());
		if (size_t failed = 0; !SelectSketchItems(open.doc, open.items, order, failed))
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select an entity -- nothing was changed");

		ATL::CComPtr<ISketchSegment> made;
		HRESULT hr = S_OK;
		if (fillet)
			// Whatever a dimension or relation holds onto at the corner stays, on a
			// point there (the fillet command's default; 0 would ask in a dialog).
			hr = open.sketchManager->CreateFillet(request->fillet_radius(), swConstrainedCornerKeepGeometry, &made);
		else
		{
			const auto& chamfer = request->chamfer();
			long type = swSketchChamfer_DistanceEqual;
			double second = chamfer.distance();
			if (chamfer.kind() != sld_mcp::v1::SKETCH_CHAMFER_KIND_EQUAL)
			{
				type = chamfer.kind() == sld_mcp::v1::SKETCH_CHAMFER_KIND_ANGLE ? swSketchChamfer_DistanceAngle
				                                                                 : swSketchChamfer_DistanceDistance;
				second = chamfer.second();
			}
			hr = open.sketchManager->CreateChamfer(type, chamfer.distance(), second, &made);
		}
		if (FAILED(hr))
			return ComError(fillet ? "ISketchManager::CreateFillet" : "ISketchManager::CreateChamfer", hr);

		// Of two corners with no room for both it makes one and returns as usual
		// (seen 2026-10-31), so count what came out.
		const int count = open.CountNew(fillet ? swSketchARC : swSketchLINE);
		if (count != corners)
		{
			if (count > 0)
				open.Undo();
			const std::string what = fillet ? "fillet" : "chamfer";
			if (count > 0)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"only " + std::to_string(count) + " of the " + std::to_string(corners) + " corners had room for this " +
					what + "; nothing was changed -- make it smaller");
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, fillet
				? "SolidWorks couldn't fillet that: the radius may be too big for the entities, or they're parallel, "
				  "tangent or cross each other (trim_sketch_entity first); nothing was changed"
				: "SolidWorks couldn't chamfer that: the sizes may be too big for the entities, or they don't touch "
				  "(extend or trim them first); nothing was changed");
		}
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::TrimSketchEntity(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::TrimSketchEntityRequest* request,
	sld_mcp::v1::SketchEditResponse* response)
{
	auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
	const auto kind = request->kind();
	const bool corner = kind == sld_mcp::v1::SKETCH_TRIM_KIND_CORNER;
	if (kind != sld_mcp::v1::SKETCH_TRIM_KIND_PIECE && !corner && kind != sld_mcp::v1::SKETCH_TRIM_KIND_EXTEND)
		return invalid("kind must be PIECE, CORNER or EXTEND");
	if (!request->has_item() || request->item().point())
		return invalid("give a line, arc, circle, ellipse or spline, not a point");
	if (corner)
	{
		if (!request->has_other() || request->other().point())
			return invalid("a corner needs a second line, arc, ellipse or spline");
		if (SketchKeyOf(request->item()) == SketchKeyOf(request->other()))
			return invalid("a corner needs two different entities");
	}
	else if (!request->has_at() || !IsFinite(request->at()))
		return invalid("say where on the entity: a finite point");

	std::vector<sld_mcp::v1::SketchItemRef> refs{request->item()};
	if (corner)
		refs.push_back(request->other());

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();
	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	return RunSketchEdit(swApp, request->sketch_name(), refs, response, [&](OpenEdit& open) -> grpc::Status {
		// SketchTrim and SketchExtend take sketch coordinates.
		double at[3] = {request->at().x(), request->at().y(), 0.0};
		if (request->model_space())
		{
			SketchSpace space;
			if (grpc::Status status = space.Init(swApp); !status.ok())
				return status;
			space.Attach(open.sketch);
			const double model[3] = {request->at().x(), request->at().y(), request->at().z()};
			double local[3];
			if (!space.ToSketch(model, local))
				return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't map the point into the sketch's coordinates");
			at[0] = local[0];
			at[1] = local[1];
		}

		if (size_t failed = 0; !SelectSketchItems(open.doc, open.items, refs, failed))
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select an entity -- nothing was changed");

		// SketchTrim's result says nothing: false for a piece it did remove, so
		// what counts is what changed (seen 2026-10-31).
		VARIANT_BOOL ok = VARIANT_FALSE;
		const HRESULT hr = kind == sld_mcp::v1::SKETCH_TRIM_KIND_EXTEND
			? open.sketchManager->SketchExtend(at[0], at[1], at[2], &ok)
			: open.sketchManager->SketchTrim(corner ? swSketchTrimCorner : swSketchTrimClosest, at[0], at[1], at[2], &ok);
		if (FAILED(hr))
			return ComError(kind == sld_mcp::v1::SKETCH_TRIM_KIND_EXTEND ? "ISketchManager::SketchExtend"
			                                                              : "ISketchManager::SketchTrim", hr);
		if (!open.Changed())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				corner ? "those two can't meet in a corner (parallel, or they already do?); nothing was changed"
				: kind == sld_mcp::v1::SKETCH_TRIM_KIND_EXTEND ? "nothing is in the way of that entity to extend it to; nothing was changed"
				: "SolidWorks couldn't trim that; nothing was changed");

		SketchItems now;
		now.Load(open.sketch);
		const auto after = SegmentsOf(now);
		if (corner)
		{
			// With entities that cross it keeps the part of each it chooses, whichever
			// parts were picked (seen 2026-10-31), so only growing is left to it.
			for (const auto& ref : refs)
			{
				auto then = after.find(SketchKeyOf(ref));
				const auto* old = open.Old(ref);
				if (old && (then == after.end() || then->second.length() < old->length() - kEditTolerance))
				{
					open.Undo();
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"those two cross, and a corner would keep the wrong parts as likely as the right ones; nothing "
						"was changed -- give `at` to remove the pieces you don't want, one by one");
				}
			}
		}
		else if (kind == sld_mcp::v1::SKETCH_TRIM_KIND_EXTEND)
		{
			// With nothing beyond the end asked for it grows the other one.
			const auto* old = open.Old(refs.front());
			auto then = after.find(SketchKeyOf(refs.front()));
			if (old && then != after.end() && old->has_start() && old->has_end() && then->second.has_start() &&
				then->second.has_end())
			{
				sld_mcp::v1::Vec3 pick;
				pick.set_x(at[0]);
				pick.set_y(at[1]);
				const bool startAsked = Length(old->start(), pick) <= Length(old->end(), pick);
				const auto& was = startAsked ? old->start() : old->end();
				const auto& is = startAsked ? then->second.start() : then->second.end();
				if (Length(was, is) <= kEditTolerance)
				{
					open.Undo();
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"nothing is in the way of that end to extend it to (the other end would grow instead); nothing "
						"was changed -- give `at` near the other end if that's what you want");
				}
			}
		}
		return grpc::Status::OK;
	});
}
