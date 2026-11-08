// Rib.cpp : RibSketch (a thin wall grown from a sketch until it meets the body).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "SeedHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// One try: the sketch selected, InsertRib, which returns nothing: the new
	// feature is the new last one. A rib that failed to rebuild or added no
	// volume is deleted again, which leaves the sketch unused. OK with `rib`
	// null if SolidWorks made nothing.
	grpc::Status TryRib(IModelDoc2* doc, IFeatureManager* featureManager, IFeature* sketch,
		const sld_mcp::v1::RibSketchRequest& request, bool reverse, const std::string& why,
		ATL::CComPtr<IFeature>& rib, double& added)
	{
		rib.Release();
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE;
		HRESULT hr = sketch->Select2(VARIANT_FALSE, 0, &selected);
		if (FAILED(hr))
			return ComError("IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select " + FeatureName(sketch));

		const double volumeBefore = SolidVolume(doc);
		ATL::CComPtr<IFeature> lastBefore;
		doc->IFeatureByPositionReverse(0, &lastBefore);
		hr = featureManager->InsertRib(
			request.one_sided() ? VARIANT_FALSE : VARIANT_TRUE,      // Is2Sided
			request.flip_side() ? VARIANT_TRUE : VARIANT_FALSE,      // ReverseThicknessDir
			request.thickness(),                                     // Thickness
			0,                                                       // ReferenceEdgeIndex
			reverse ? VARIANT_TRUE : VARIANT_FALSE,                  // ReverseMaterialDir
			request.draft_angle() > 0.0 ? VARIANT_TRUE : VARIANT_FALSE, // IsDrafted
			request.draft_outward() ? VARIANT_TRUE : VARIANT_FALSE,  // DraftOutward
			request.draft_angle(),                                   // DraftAngle
			request.normal_to_sketch() ? VARIANT_TRUE : VARIANT_FALSE, // IsNormToSketch
			VARIANT_FALSE);                                          // IsDraftedFromWall
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IFeatureManager::InsertRib", hr);

		ATL::CComPtr<IFeature> last;
		doc->IFeatureByPositionReverse(0, &last);
		if (!last || SameObject(last, lastBefore))
			return grpc::Status::OK;
		if (!KeepIfBuilt(doc, last, why).ok())
			return grpc::Status::OK;
		added = SolidVolume(doc) - volumeBefore;
		if (added < kVolumeTolerance)
		{
			DeleteFeature(doc, last);
			return grpc::Status::OK;
		}
		rib = last;
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::RibSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::RibSketchRequest* request,
	sld_mcp::v1::RibSketchResponse* response)
{
	const double thickness = request->thickness();
	const double draft = request->draft_angle();
	if (!(std::isfinite(thickness) && thickness > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "thickness must be positive");
	if (!(std::isfinite(draft) && draft >= 0.0 && draft < M_PI_2))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "draft angle must be 0 or more and less than 90 degrees");

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

	ATL::CComPtr<IFeature> sketch;
	if (grpc::Status status = request->sketch_name().empty()
			? FindLatestUnusedSketch(doc, sketch)
			: FindSketchByName(doc, request->sketch_name(), sketch);
		!status.ok())
		return status;
	const std::string sketchName = FeatureName(sketch);

	if (SolidBodyCount(doc) == 0)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"the active part has no solid body for a rib to grow to: extrude something first");

	// Informational: whether the sketch is fully defined.
	if (ATL::CComPtr<ISketch> sketchData; SketchOf(sketch, sketchData).ok())
		response->set_sketch_status(SketchStatus(sketchData));

	ATL::CComPtr<IFeatureManager> featureManager;
	HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	const std::string why =
		"the rib must end up closed in by the body: it needs material in the way it grows (within the sketch "
		"plane, or along its normal with normal_to_sketch), and the sketch, carried on past its ends, must run "
		"into material. A line floating over a plain plate has nothing to stop it: draw the chain down to the "
		"plate";
	const bool reverse = request->reverse_direction();
	ATL::CComPtr<IFeature> rib;
	double added = 0.0;
	if (grpc::Status status = TryRib(doc, featureManager, sketch, *request, reverse, why, rib, added); !status.ok())
		return status;
	// Which way has the material differs by sketch, and a caller can't see
	// it: unless a way was asked for, try the other before
	// giving up. Each failed try uses up a name (Rib1, then Rib2 is made).
	bool flipped = false;
	if (!rib && !reverse)
	{
		if (grpc::Status status = TryRib(doc, featureManager, sketch, *request, true, why, rib, added); !status.ok())
			return status;
		flipped = rib;
	}
	if (!rib)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't make a rib from " + sketchName + (reverse ? "" : " (tried both ways)") + " -- " + why);

	response->set_feature_name(FeatureName(rib));
	response->set_sketch_name(sketchName);
	response->set_direction_flipped(flipped);
	response->set_volume_change(added);
	return grpc::Status::OK;
}
