// SketchOffset.cpp : OffsetSketchEntities (sketch entities, or model edges and faces, offset
// into a sketch).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// More than anyone offsets in one go.
	constexpr int kMaxOffsetItems = 1000;

	// Radii and centers closer than this (meters) count as the same.
	constexpr double kOffsetTolerance = 1e-7;

	long CapEndsOf(sld_mcp::v1::OffsetCaps caps)
	{
		switch (caps)
		{
		case sld_mcp::v1::OFFSET_CAPS_ARCS: return swSkOffsetArcCaps;
		case sld_mcp::v1::OFFSET_CAPS_LINES: return swSkOffsetLineCaps;
		default: return swSkOffsetNoCaps;
		}
	}

	long ConstructionOf(const sld_mcp::v1::OffsetSketchEntitiesRequest& request)
	{
		if (request.base_construction())
			return request.offset_construction() ? swSkOffsetMakeBothConstruction : swSkOffsetMakeOrigConstruction;
		return request.offset_construction() ? swSkOffsetMakeOffsConstruction : swSkOffsetDontMakeConstruction;
	}

	// An edge or a face behind a persist id, as its IEntity.
	grpc::Status ModelEntity(IModelDocExtension* ext, const std::string& persistId, const std::string& where,
		ATL::CComPtr<IEntity>& entity)
	{
		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, persistId, obj); !status.ok())
			return grpc::Status(status.error_code(), where + ": " + status.error_message());
		if (!ATL::CComQIPtr<IEdge>(obj.p) && !ATL::CComQIPtr<IFace2>(obj.p))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				where + " isn't an edge or a face -- offset sketch entities, model edges or faces");
		if (FAILED(obj->QueryInterface(IID_PPV_ARGS(&entity))) || !entity)
			return grpc::Status(grpc::StatusCode::INTERNAL, where + " has no IEntity interface");
		return grpc::Status::OK;
	}

	// SketchOffset2 takes an inward distance beyond an arc's radius without
	// complaint, and draws an arc of radius (distance - radius) round the same
	// center, with the lines joining it swapped (seen 2026-10-31: a circle of
	// radius 10 offset inward by 12 makes one of radius 2).
	bool TurnedInsideOut(const SketchItems& open, const std::set<SketchItemKey>& added, double distance)
	{
		std::vector<sld_mcp::v1::SketchItem> before, made;
		for (const auto& e : open.All())
		{
			sld_mcp::v1::SketchItem item;
			if (!e.point && DescribeSketchItem(e.obj, std::string(), &item) &&
				(item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_ARC || item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE))
				(added.count(SketchKeyOf(e)) ? made : before).push_back(item);
		}
		for (const auto& m : made)
			for (const auto& b : before)
				if (b.radius() < distance && std::abs(m.radius() - (distance - b.radius())) < kOffsetTolerance &&
					std::abs(m.center().x() - b.center().x()) < kOffsetTolerance &&
					std::abs(m.center().y() - b.center().y()) < kOffsetTolerance)
					return true;
		return false;
	}
}

grpc::Status SolidWorksService::OffsetSketchEntities(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::OffsetSketchEntitiesRequest* request,
	sld_mcp::v1::OffsetSketchEntitiesResponse* response)
{
	const int itemCount = request->items_size();
	const int modelCount = request->model_persist_ids_size();
	const bool fromModel = modelCount > 0;
	const bool existing = !request->sketch_name().empty();
	const bool onEntity = !request->on_persist_id().empty();
	const double distance = request->distance();
	const auto side = request->side();
	auto where = [](int i) { return "entity " + std::to_string(i + 1); };

	if (!std::isfinite(distance) || distance <= 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the offset distance must be more than 0");
	if (side != sld_mcp::v1::OFFSET_SIDE_OUTWARD && side != sld_mcp::v1::OFFSET_SIDE_INWARD &&
		side != sld_mcp::v1::OFFSET_SIDE_BOTH)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "side must be OUTWARD, INWARD or BOTH");
	if (itemCount > 0 && fromModel)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give sketch entities or model edges and faces, not both");
	if (itemCount == 0 && !fromModel && !existing)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give the entities to offset, or the sketch to offset as a whole");
	if (itemCount + modelCount > kMaxOffsetItems)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"too many entities in one call (max " + std::to_string(kMaxOffsetItems) + ")");
	if (request->caps() != sld_mcp::v1::OFFSET_CAPS_NONE && side != sld_mcp::v1::OFFSET_SIDE_BOTH)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "end caps only go with an offset to both sides");
	if (fromModel && (request->caps() != sld_mcp::v1::OFFSET_CAPS_NONE || request->base_construction() ||
		request->offset_construction() || request->add_dimension()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"end caps, construction and a dimension only go with sketch entities, not model edges or faces");
	int planeIndex = 0;
	if (existing || !fromModel)
	{
		if (onEntity || request->plane() != sld_mcp::v1::PLANE_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a new sketch only goes with model edges or faces, and not with an existing sketch_name");
	}
	else if (!onEntity && !StandardPlaneIndex(request->plane(), planeIndex))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");

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

	// Check everything against the closed sketch before opening one.
	ATL::CComPtr<IFeature> sketchFeature;
	SketchTarget target;
	if (itemCount > 0)
	{
		if (grpc::Status status = ResolveItemsSketch(doc, ext, request->sketch_name(), request->items(0), sketchFeature);
			!status.ok())
			return status;
		const std::string sketchName = FeatureName(sketchFeature);
		const std::string pid = PersistIdOf(ext, sketchFeature);
		ATL::CComPtr<ISketch> closed;
		if (grpc::Status status = SketchOf(sketchFeature, closed); !status.ok())
			return status;
		SketchItems items;
		items.Load(closed);
		for (int i = 0; i < itemCount; ++i)
		{
			google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> one;
			*one.Add()->mutable_item() = request->items(i);
			if (grpc::Status status = CheckRefsInSketch(ext, one, pid, sketchName, where(i)); !status.ok())
				return status;
			if (request->items(i).point())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + " is a point -- only lines, arcs, circles and splines can be offset");
			if (!items.Find(request->items(i)))
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					where(i) + " isn't in sketch \"" + sketchName + "\" any more (deleted or merged?) -- inspect the "
					"sketch again for current IDs");
		}
	}
	else if (existing)
	{
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
			return status;
	}
	else if (grpc::Status status = ResolveSketchTarget(doc, request->on_persist_id(), planeIndex, target); !status.ok())
		return status;
	for (int i = 0; i < modelCount; ++i)
	{
		ATL::CComPtr<IEntity> entity;
		if (grpc::Status status = ModelEntity(ext, request->model_persist_ids(i), where(i), entity); !status.ok())
			return status;
	}

	const auto errorsBefore = FeatureErrors(FeatureTree(doc));

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;
	ATL::CComPtr<ISketch> openSketch;
	if (sketchFeature)
	{
		if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, openSketch); !status.ok())
			return status;
	}
	else
	{
		if (grpc::Status status = OpenNewSketch(doc, sketchManager, target, openSketch); !status.ok())
			return status;
		sketchFeature = ATL::CComQIPtr<IFeature>(openSketch.p);
	}
	const std::string sketchName = FeatureName(sketchFeature);

	std::set<SketchItemKey> added;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems open;
		open.Load(openSketch);
		const std::set<SketchItemKey> before = SketchKeys(open);
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);

		// What the offset of an open chain starts from is the first entity
		// selected, so they're selected in the order given.
		if (fromModel)
			for (int i = 0; i < modelCount; ++i)
			{
				// Again: opening an existing sketch rolls back the features
				// after it, and with them the edges they made.
				ATL::CComPtr<IEntity> entity;
				if (!ModelEntity(ext, request->model_persist_ids(i), where(i), entity).ok())
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						where(i) + " doesn't exist while \"" + sketchName + "\" is being edited: it comes from a "
						"feature after the sketch, or built from it -- offset it into a later sketch");
				VARIANT_BOOL selected = VARIANT_FALSE;
				hr = entity->Select4(VARIANT_TRUE, nullptr, &selected);
				if (FAILED(hr))
					return ComError("IEntity::Select4", hr);
				if (!selected)
					return grpc::Status(grpc::StatusCode::INTERNAL,
						"SolidWorks wouldn't select " + where(i) + " -- nothing was offset");
			}
		else
		{
			std::vector<sld_mcp::v1::SketchItemRef> refs(request->items().begin(), request->items().end());
			// No entities given: everything in the sketch that a feature would use.
			if (refs.empty())
				for (const auto& e : open.All())
				{
					sld_mcp::v1::SketchItem item;
					if (!e.point && DescribeSketchItem(e.obj, std::string(), &item) && !item.construction())
						refs.push_back(item.ref());
				}
			if (refs.empty())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"\"" + sketchName + "\" has nothing to offset: no segments other than construction ones");
			if (size_t failed = 0; !SelectSketchItems(doc, open, refs, failed))
				return grpc::Status(grpc::StatusCode::INTERNAL,
					"SolidWorks wouldn't select " + where(static_cast<int>(failed)) +
					" in the open sketch -- nothing was offset");
		}

		const double signedDistance = side == sld_mcp::v1::OFFSET_SIDE_INWARD ? -distance : distance;
		const VARIANT_BOOL both = side == sld_mcp::v1::OFFSET_SIDE_BOTH ? VARIANT_TRUE : VARIANT_FALSE;
		const VARIANT_BOOL chain = request->chain() ? VARIANT_TRUE : VARIANT_FALSE;
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = fromModel
			? doc->SketchOffsetEntities2(signedDistance, both, chain, &ok)
			: sketchManager->SketchOffset2(signedDistance, both, chain, CapEndsOf(request->caps()),
				ConstructionOf(*request), request->add_dimension() ? VARIANT_TRUE : VARIANT_FALSE, &ok);
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError(fromModel ? "IModelDoc2::SketchOffsetEntities2" : "ISketchManager::SketchOffset2", hr);

		open.Load(openSketch);
		std::vector<ATL::CComPtr<IDispatch>> made;
		for (const auto& e : open.All())
			if (!before.count(SketchKeyOf(e)))
			{
				added.insert(SketchKeyOf(e));
				made.push_back(e.obj);
			}
		// False and nothing new: an inward distance that leaves no shape.
		if (!ok || added.empty())
		{
			DeleteDrawnEntities(doc, made);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't offset that by this distance -- e.g. inward by more than the shape has room "
				"for; nothing was changed");
		}
		if (TurnedInsideOut(open, added, distance))
		{
			DeleteDrawnEntities(doc, made);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"this distance is more than the radius of an arc or circle on that side, which would turn the shape "
				"inside out; nothing was changed");
		}
		return grpc::Status::OK;
	}();

	// Closed without the exit rebuild, as MirrorSketchEntities does: when the
	// new entities break the profile of the sketch's feature, InsertSketch(TRUE)
	// would open the Repair Sketch dialog.
	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_FALSE);
	if (!failure.ok())
	{
		if (SUCCEEDED(closeHr) && fromModel && !existing)
			DeleteSketchFeature(doc, sketchFeature);
		VARIANT_BOOL restored = VARIANT_FALSE;
		doc->EditRebuild3(&restored);
		return failure;
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"the entities were offset, but SolidWorks left \"" + sketchName + "\" open for editing");
	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	response->set_sketch_name(sketchName);
	response->set_created_sketch(fromModel && !existing);
	SketchItems after;
	after.Load(sketch);
	DescribeAddedItems(after, added, sketchPid, false, response->mutable_items());
	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;
	ReadSketchState(sketch, sketchPid, response->mutable_state());
	AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
	return grpc::Status::OK;
}
