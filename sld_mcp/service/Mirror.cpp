// Mirror.cpp : MirrorFeatures (features or bodies about a plane) and MirrorSketchEntities
// (sketch entities about a line of their sketch).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection marks InsertMirrorFeature2 reads its inputs from.
	constexpr long kFeatureMark = 1;
	constexpr long kPlaneMark = 2;
	constexpr long kBodyMark = 256;

	// More than anyone mirrors in one go.
	constexpr int kMaxMirrorFeatures = 500;
	constexpr int kMaxMirrorSketchItems = 1000;

	grpc::Status CheckMirrorPlane(const Pick& pick)
	{
		if (pick.kind == Pick::Entity)
		{
			ATL::CComQIPtr<IFace2> face(pick.obj.p);
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL planar = VARIANT_FALSE;
			if (face && SUCCEEDED(face->IGetSurface(&surface)) && surface && SUCCEEDED(surface->IsPlane(&planar)) &&
				planar)
				return grpc::Status::OK;
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"plane: " + std::string(face ? "the face isn't flat" : "a " + pick.what + " isn't a plane") +
				" -- mirror about a planar face, a reference plane or a default plane");
		}
		if (pick.kind == Pick::Feature)
		{
			ATL::CComQIPtr<IFeature> feature(pick.obj.p);
			if (feature && FeatureTypeName(feature) == "RefPlane")
				return grpc::Status::OK;
		}
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"plane: " + pick.what + " isn't a plane -- mirror about a planar face, a reference plane or a default "
			"plane");
	}

	int SolidBodyCount(IModelDoc2* doc)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		ATL::CComVariant bodies;
		if (!part || FAILED(part->GetBodies2(swSolidBody, VARIANT_FALSE, &bodies)))
			return 0;
		return static_cast<int>(comvariant::ToDispatches(bodies).size());
	}

	// Segment type (-1 for points: segments are numbered per type) and ID.
	using ItemKey = std::tuple<long, long long, long long>;

	ItemKey KeyOf(const SketchItems::Entry& e)
	{
		return {e.point ? -1L : e.type, e.id0, e.id1};
	}

	ItemKey KeyOf(const sld_mcp::v1::SketchItemRef& ref)
	{
		return {ref.point() ? -1L : static_cast<long>(ref.segment_type()), ref.id0(), ref.id1()};
	}

	std::set<ItemKey> ItemKeys(const SketchItems& items)
	{
		std::set<ItemKey> keys;
		for (const auto& e : items.All())
			keys.insert(KeyOf(e));
		return keys;
	}
}

grpc::Status SolidWorksService::MirrorFeatures(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::MirrorFeaturesRequest* request,
	sld_mcp::v1::MirrorFeaturesResponse* response)
{
	const int featureCount = request->features_size();
	const int bodyCount = request->bodies_size();
	if (featureCount == 0 && bodyCount == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the features or the bodies to mirror");
	if (featureCount > 0 && bodyCount > 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give features or bodies to mirror, not both");
	if (featureCount + bodyCount > kMaxMirrorFeatures)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxMirrorFeatures) + " features or bodies per call");
	if (request->plane().target_case() == sld_mcp::v1::GeometryRef::TARGET_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the plane to mirror about");

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
	PersistIds ids(ext);

	Selection plane;
	if (grpc::Status status = plane.Add(doc, ext, request->plane(), kPlaneMark, "plane"); !status.ok())
		return status;
	if (grpc::Status status = CheckMirrorPlane(plane.picks.front()); !status.ok())
		return status;

	// The features, in request order, once each.
	std::vector<ATL::CComPtr<IFeature>> features;
	std::string mirrored;
	{
		const std::vector<TreeFeature> tree = featureCount ? FeatureTree(doc) : std::vector<TreeFeature>{};
		std::set<std::string> seen;
		for (int i = 0; i < featureCount; ++i)
		{
			const std::string label = "feature " + std::to_string(i + 1);
			size_t index = 0;
			if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, request->features(i), index); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
			const TreeFeature& f = tree[index];
			if (f.fixed)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					label + ": \"" + f.name + "\" can't be mirrored: it " + f.fixed);
			if (IsSketchType(f.type))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					label + ": \"" + f.name + "\" is a sketch -- mirror the feature made from it, or mirror the "
					"sketch's entities inside it with mirror_sketch_entities");
			if (!seen.insert(f.name).second)
				continue;
			features.push_back(f.feature);
			mirrored += (mirrored.empty() ? "" : ", ") + f.name;
		}
	}

	// The bodies, once each.
	std::vector<ATL::CComPtr<IBody2>> bodies;
	for (int i = 0; i < bodyCount; ++i)
	{
		const std::string label = "body " + std::to_string(i + 1);
		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, request->bodies(i), obj); !status.ok())
			return grpc::Status(status.error_code(), label + " " + status.error_message());
		ATL::CComQIPtr<IBody2> body(obj.p);
		if (!body)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + " isn't a body");
		if (std::none_of(bodies.begin(), bodies.end(), [&](const auto& b) { return SameObject(b, body); }))
			bodies.push_back(body);
	}
	if (!bodies.empty())
		mirrored = bodies.size() == 1 ? "1 body" : std::to_string(bodies.size()) + " bodies";

	if (grpc::Status status = plane.Select(doc, ext); !status.ok())
		return status;
	for (size_t i = 0; i < features.size(); ++i)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (FAILED(features[i]->Select2(VARIANT_TRUE, kFeatureMark, &selected)) || !selected)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks wouldn't select \"" + FeatureName(features[i]) + "\" -- is it suppressed?");
		}
	}
	if (!bodies.empty())
	{
		ATL::CComPtr<ISelectionMgr> selectionManager;
		ATL::CComPtr<ISelectData> data;
		hr = doc->get_ISelectionManager(&selectionManager);
		if (FAILED(hr) || !selectionManager || FAILED(selectionManager->CreateSelectData(&data)) || !data)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return ComError("ISelectionMgr::CreateSelectData", hr);
		}
		data->put_Mark(kBodyMark);
		for (size_t i = 0; i < bodies.size(); ++i)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(bodies[i]->Select2(VARIANT_TRUE, data, &selected)) || !selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select body " + std::to_string(i + 1) + " -- is it hidden?");
			}
		}
	}

	ATL::CComPtr<IFeature> mirror;
	hr = featureManager->InsertMirrorFeature2(
		bodies.empty() ? VARIANT_FALSE : VARIANT_TRUE,  // BMirrorBody
		VARIANT_FALSE,                                  // BGeometryPattern
		request->merge() ? VARIANT_TRUE : VARIANT_FALSE, // BMerge (bodies)
		VARIANT_FALSE,                                  // BKnit (surface bodies)
		swFeatureScope_AllBodies,                       // ScopeOptions
		&mirror);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IFeatureManager::InsertMirrorFeature2", hr);
	if (!mirror)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't mirror " + mirrored + " about " + plane.Describe() +
			" -- e.g. a mirrored cut or fillet that finds no material on the other side, or a mirrored boss "
			"that doesn't touch the body");

	// A mirror that fails to rebuild, or changes nothing (a mirrored cut
	// beyond the body builds without an error and has no faces, seen
	// 2026-10-20), is deleted again.
	VARIANT_BOOL warning = VARIANT_FALSE;
	long error = swFeatureErrorNone;
	long faces = 0;
	const bool failed =
		SUCCEEDED(mirror->GetErrorCode2(&warning, &error)) && error != swFeatureErrorNone && !warning;
	if (failed || (SUCCEEDED(mirror->GetFaceCount(&faces)) && faces == 0))
	{
		const std::string name = FeatureName(mirror);
		DeleteFeature(doc, mirror);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks made " + name + " but it " +
			(failed ? "failed to rebuild (swFeatureError_e " + std::to_string(error) + ")" : "changed nothing") +
			", so it was deleted again -- the mirrored copy of " + mirrored + " about " + plane.Describe() +
			" probably finds no material there (a cut or fillet beyond the body)");
	}

	response->set_feature_name(FeatureName(mirror));
	response->set_mirrored(mirrored);
	response->set_plane(plane.Describe());
	response->set_body_count(SolidBodyCount(doc));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::MirrorSketchEntities(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::MirrorSketchEntitiesRequest* request,
	sld_mcp::v1::MirrorSketchEntitiesResponse* response)
{
	const int count = request->items_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one sketch entity to mirror");
	if (count > kMaxMirrorSketchItems)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"too many sketch entities in one call (max " + std::to_string(kMaxMirrorSketchItems) + ")");
	if (!request->has_axis())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the line to mirror about");
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

	// The sketch: by name, or the one the first entity is in.
	ATL::CComPtr<IFeature> sketchFeature;
	if (!request->sketch_name().empty())
	{
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
			return status;
	}
	else
	{
		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, request->items(0).sketch_persist_id(), obj); !status.ok())
			return grpc::Status(status.error_code(), "the entities' sketch " + status.error_message());
		sketchFeature = ATL::CComQIPtr<IFeature>(obj.p);
		if (!sketchFeature)
			return grpc::Status(grpc::StatusCode::INTERNAL, "a sketch reference didn't resolve to a feature");
	}
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	response->set_sketch_name(sketchName);

	// The entities and the axis must be in that sketch.
	for (int i = 0; i <= count; ++i)
	{
		google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> one;
		*one.Add()->mutable_item() = i < count ? request->items(i) : request->axis();
		if (grpc::Status status = CheckRefsInSketch(ext, one, sketchPid, sketchName, i < count ? where(i) : "axis");
			!status.ok())
			return status;
	}

	// Check everything against the closed sketch before opening it.
	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	// The requested segments' lengths, to tell which SolidWorks extended.
	std::map<ItemKey, double> lengths;
	bool anyPoints = false;
	{
		SketchItems items;
		items.Load(sketch);
		ATL::CComQIPtr<ISketchSegment> axis(items.Find(request->axis()).p);
		long axisType = -1;
		if (request->axis().point() || !axis || FAILED(axis->GetType(&axisType)) || axisType != swSketchLINE)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"axis: " + std::string(items.Find(request->axis()) ? "that isn't a line" : "it isn't in \"" +
					sketchName + "\" any more") + " -- mirror about a line or centerline of the sketch");
		for (int i = 0; i < count; ++i)
		{
			const auto& ref = request->items(i);
			if (!items.Find(ref))
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					where(i) + " isn't in sketch \"" + sketchName + "\" any more (deleted or merged?) -- inspect "
					"the sketch again for current IDs");
			if (KeyOf(ref) == KeyOf(request->axis()))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + " is the axis itself -- the line to mirror about isn't mirrored");
			anyPoints = anyPoints || ref.point();
			sld_mcp::v1::SketchItem item;
			if (!ref.point() && DescribeSketchItem(items.Find(ref), sketchPid, &item))
				lengths[KeyOf(ref)] = item.length();
		}
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

	std::set<ItemKey> added;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems open;
		open.Load(openSketch);
		const std::set<ItemKey> before = ItemKeys(open);
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		// The entities first, the line to mirror about last.
		for (int i = 0; i <= count; ++i)
		{
			ATL::CComPtr<IDispatch> obj = open.Find(i < count ? request->items(i) : request->axis());
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (ATL::CComQIPtr<ISketchSegment> segment(obj.p); segment)
				segment->Select4(VARIANT_TRUE, nullptr, &selected);
			else if (ATL::CComQIPtr<ISketchPoint> point(obj.p); point)
				point->Select4(VARIANT_TRUE, nullptr, &selected);
			if (!selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::INTERNAL,
					"SolidWorks wouldn't select " + (i < count ? where(i) : std::string("the axis")) +
					" in the open sketch -- nothing was mirrored");
			}
		}
		hr = doc->SketchMirror();
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::SketchMirror", hr);
		open.Load(openSketch);
		for (const ItemKey& key : ItemKeys(open))
			if (!before.count(key))
				added.insert(key);
		if (added.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't mirror anything in \"" + sketchName + "\"");
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
			"the entities were mirrored, but SolidWorks left \"" + sketchName + "\" open for editing");
	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);

	// What changed, read from the closed sketch: the new segments; the new
	// points that are no segment's end or center (only when points were
	// mirrored: SolidWorks also adds a midpoint on the axis to each line it
	// extends); the requested lines it extended across the axis instead of
	// copying them (seen 2026-10-20).
	sketch.Release();
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	SketchItems after;
	after.Load(sketch);
	std::set<ItemKey> segmentPoints;
	std::vector<sld_mcp::v1::SketchItem> points;
	for (const auto& e : after.All())
	{
		sld_mcp::v1::SketchItem item;
		if (!DescribeSketchItem(e.obj, sketchPid, &item))
			continue;
		const ItemKey key = KeyOf(e);
		if (!e.point)
			for (const sld_mcp::v1::SketchItemRef* p : {&item.start_point(), &item.end_point(), &item.center_point()})
				if (p->point())
					segmentPoints.insert(KeyOf(*p));
		if (auto length = lengths.find(key); length != lengths.end() &&
			std::abs(item.length() - length->second) > 1e-9)
			*response->add_extended() = item;
		if (!added.count(key))
			continue;
		if (e.point)
			points.push_back(item);
		else
			*response->add_items() = item;
	}
	if (anyPoints)
		for (const auto& item : points)
			if (!segmentPoints.count(KeyOf(item.ref())))
				*response->add_items() = item;

	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;
	ReadSketchState(sketch, sketchPid, response->mutable_state());
	AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
	return grpc::Status::OK;
}
