// Mirror.cpp : MirrorFeatures (features or bodies about a plane) and MirrorSketchEntities
// (sketch entities about a line of their sketch).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SeedHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection marks InsertMirrorFeature2 reads its inputs from.
	constexpr long kFeatureMark = 1;
	constexpr long kPlaneMark = 2;
	constexpr long kBodyMark = 256;

	// More than anyone mirrors in one go.
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
}

grpc::Status SolidWorksService::MirrorFeatures(grpc::ServerContext* context,
	const sld_mcp::v1::MirrorFeaturesRequest* request,
	sld_mcp::v1::MirrorFeaturesResponse* response)
{
	const int featureCount = request->features_size();
	const int bodyCount = request->bodies_size();
	if (featureCount == 0 && bodyCount == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the features or the bodies to mirror");
	if (featureCount > 0 && bodyCount > 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give features or bodies to mirror, not both");
	if (featureCount + bodyCount > kMaxSeeds)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxSeeds) + " features or bodies per call");
	if (request->plane().target_case() == sld_mcp::v1::GeometryRef::TARGET_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the plane to mirror about");

	return m_executor.Execute(context, "MirrorFeatures", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
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

		Selection plane;
		if (grpc::Status status = plane.Add(doc, ext, request->plane(), kPlaneMark, "plane"); !status.ok())
			return status;
		if (grpc::Status status = CheckMirrorPlane(plane.picks.front()); !status.ok())
			return status;

		std::vector<ATL::CComPtr<IFeature>> features;
		std::vector<ATL::CComPtr<IBody2>> bodies;
		std::string mirrored;
		if (grpc::Status status = ResolveSeedFeatures(doc, ext, request->features(), "mirrored",
				"mirror the feature made from it, or mirror the sketch's entities inside it with "
				"mirror_sketch_entities",
				features, mirrored);
			!status.ok())
			return status;
		if (grpc::Status status = ResolveSeedBodies(ext, request->bodies(), bodies, mirrored); !status.ok())
			return status;

		if (grpc::Status status = plane.Select(doc, ext); !status.ok())
			return status;
		if (grpc::Status status = SelectSeedFeatures(doc, features, kFeatureMark); !status.ok())
			return status;
		if (grpc::Status status = SelectSeedBodies(doc, bodies, kBodyMark); !status.ok())
			return status;

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

		// A mirrored cut beyond the body builds without an error and has no
		// faces (seen 2026-10-20).
		if (grpc::Status status = KeepIfBuilt(doc, mirror,
				"the mirrored copy of " + mirrored + " about " + plane.Describe() +
					" probably finds no material there (a cut or fillet beyond the body)");
			!status.ok())
			return status;

		response->set_feature_name(FeatureName(mirror));
		response->set_mirrored(mirrored);
		response->set_plane(plane.Describe());
		response->set_body_count(SolidBodyCount(doc));
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::MirrorSketchEntities(grpc::ServerContext* context,
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

	return m_executor.Execute(context, "MirrorSketchEntities", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
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
		std::map<SketchItemKey, double> lengths;
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
				if (SketchKeyOf(ref) == SketchKeyOf(request->axis()))
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						where(i) + " is the axis itself -- the line to mirror about isn't mirrored");
				anyPoints = anyPoints || ref.point();
				sld_mcp::v1::SketchItem item;
				if (!ref.point() && DescribeSketchItem(items.Find(ref), sketchPid, &item))
					lengths[SketchKeyOf(ref)] = item.length();
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

		std::set<SketchItemKey> added;
		const grpc::Status failure = [&]() -> grpc::Status {
			SketchItems open;
			open.Load(openSketch);
			const std::set<SketchItemKey> before = SketchKeys(open);
			HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IModelDoc2::ClearSelection2", hr);
			// The entities first, the line to mirror about last.
			std::vector<sld_mcp::v1::SketchItemRef> refs(request->items().begin(), request->items().end());
			refs.push_back(request->axis());
			if (size_t failed = 0; !SelectSketchItems(doc, open, refs, failed))
				return grpc::Status(grpc::StatusCode::INTERNAL,
					"SolidWorks wouldn't select " +
					(failed < static_cast<size_t>(count) ? where(static_cast<int>(failed)) : std::string("the axis")) +
					" in the open sketch -- nothing was mirrored");
			hr = doc->SketchMirror();
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IModelDoc2::SketchMirror", hr);
			open.Load(openSketch);
			for (const SketchItemKey& key : SketchKeys(open))
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
		DescribeAddedItems(after, added, sketchPid, anyPoints, response->mutable_items());
		for (const auto& e : after.All())
		{
			sld_mcp::v1::SketchItem item;
			if (auto length = lengths.find(SketchKeyOf(e));
				length != lengths.end() && DescribeSketchItem(e.obj, sketchPid, &item) &&
				std::abs(item.length() - length->second) > 1e-9)
				*response->add_extended() = item;
		}

		sld_mcp::v1::SketchContours contours;
		if (CountContours(sketch, contours))
			*response->mutable_contours() = contours;
		ReadSketchState(sketch, sketchPid, response->mutable_state());
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		return grpc::Status::OK;
	});
}
