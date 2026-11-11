// Extrude.cpp : ExtrudeSketch (boss and cut extrudes).
#include "pch.h"
#include "Common.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

grpc::Status SolidWorksService::ExtrudeSketch(grpc::ServerContext* context,
	const sld_mcp::v1::ExtrudeSketchRequest* request,
	sld_mcp::v1::ExtrudeSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool cut = request->cut();
	const bool throughAll = request->through_all();
	if (throughAll && !cut)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "through_all only applies to cuts");
	const double depth = throughAll ? 0.0 : request->depth();
	if (!throughAll && !(std::isfinite(depth) && depth > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "depth must be positive");

	return m_executor.Execute(context, "ExtrudeSketch", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;

		// Resolve the sketch: by name if given, else the latest unused one.
		ATL::CComPtr<IFeature> sketch;
		HRESULT hr = S_OK;
		if (request->sketch_name().empty())
		{
			if (grpc::Status status = FindLatestUnusedSketch(doc, sketch); !status.ok())
				return status;
		}
		else
		{
			if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketch); !status.ok())
				return status;
		}

		ATL::CComBSTR sketchName;
		hr = sketch->get_Name(&sketchName);
		if (FAILED(hr))
			return ComError("IFeature::Name", hr);

		// Informational: whether the profile is fully defined.
		if (ATL::CComPtr<ISketch> sketchData; SketchOf(sketch, sketchData).ok())
			response->set_sketch_status(SketchStatus(sketchData));

		ATL::CComPtr<IFeatureManager> featureManager;
		hr = doc->get_FeatureManager(&featureManager);
		if (FAILED(hr))
			return ComError("IModelDoc2::FeatureManager", hr);
		if (!featureManager)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

		// Mid-plane takes the total depth in D1 and splits it across both sides;
		// it has no direction, so `reverse_direction` doesn't apply. Through-all
		// overrides both.
		const bool midPlane = !throughAll && request->mid_plane();
		const VARIANT_BOOL reverse = (!midPlane && request->reverse_direction()) ? VARIANT_TRUE : VARIANT_FALSE;
		const long endCondition = throughAll ? swEndCondThroughAll : midPlane ? swEndCondMidPlane : swEndCondBlind;

		// FeatureExtrusion3 / FeatureCut4 extrude whatever profile is selected.
		ATL::CComPtr<IFeature> extrusion;
		bool notSelected = false;
		auto create = [&](VARIANT_BOOL dir) -> HRESULT {
			HRESULT selectHr = doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(selectHr))
				return selectHr;
			VARIANT_BOOL selected = VARIANT_FALSE;
			selectHr = sketch->Select2(VARIANT_FALSE, 0, &selected);
			if (FAILED(selectHr))
				return selectHr;
			notSelected = !selected;
			if (notSelected)
				return S_OK;
			extrusion.Release();
			if (cut)
				return featureManager->FeatureCut4(
					VARIANT_TRUE,       // Sd: one direction only
					VARIANT_FALSE,      // Flip: keep the material outside the profile
					dir,                // Dir: reverse direction 1
					endCondition,       // T1
					swEndCondBlind,     // T2 (unused, single direction)
					depth,              // D1
					0.0,                // D2
					VARIANT_FALSE,      // Dchk1: no draft
					VARIANT_FALSE,      // Dchk2
					VARIANT_FALSE,      // Ddir1
					VARIANT_FALSE,      // Ddir2
					0.0,                // Dang1
					0.0,                // Dang2
					VARIANT_FALSE,      // OffsetReverse1
					VARIANT_FALSE,      // OffsetReverse2
					VARIANT_FALSE,      // TranslateSurface1
					VARIANT_FALSE,      // TranslateSurface2
					VARIANT_FALSE,      // NormalCut (sheet metal only)
					VARIANT_TRUE,       // UseFeatScope
					VARIANT_TRUE,       // UseAutoSelect: all bodies in scope
					VARIANT_FALSE,      // AssemblyFeatureScope (assemblies only)
					VARIANT_FALSE,      // AutoSelectComponents
					VARIANT_FALSE,      // PropagateFeatureToParts
					swStartSketchPlane, // T0: start at the sketch plane
					0.0,                // StartOffset
					VARIANT_FALSE,      // FlipStartOffset
					VARIANT_FALSE,      // OptimizeGeometry
					&extrusion);
			else
				return featureManager->FeatureExtrusion3(
					VARIANT_TRUE,       // Sd: one direction only
					VARIANT_FALSE,      // Flip: side to cut (cuts only)
					dir,                // Dir: reverse direction 1
					endCondition,       // T1
					swEndCondBlind,     // T2 (unused, single direction)
					depth,              // D1
					0.0,                // D2
					VARIANT_FALSE,      // Dchk1: no draft
					VARIANT_FALSE,      // Dchk2
					VARIANT_FALSE,      // Ddir1
					VARIANT_FALSE,      // Ddir2
					0.0,                // Dang1
					0.0,                // Dang2
					VARIANT_FALSE,      // OffsetReverse1
					VARIANT_FALSE,      // OffsetReverse2
					VARIANT_FALSE,      // TranslateSurface1
					VARIANT_FALSE,      // TranslateSurface2
					VARIANT_TRUE,       // Merge with existing bodies
					VARIANT_TRUE,       // UseFeatScope
					VARIANT_TRUE,       // UseAutoSelect: all bodies in scope
					swStartSketchPlane, // T0: start at the sketch plane
					0.0,                // StartOffset
					VARIANT_FALSE,      // FlipStartOffset
					&extrusion);
		};
		hr = create(reverse);
		// A cut goes against the sketch normal by default: into the material for
		// a sketch on a face, but into nothing for a sketch on a plane with the
		// material on its normal side. Unless a direction was asked for, try the
		// other way before giving up (a failed attempt creates nothing).
		bool flipped = false;
		if (SUCCEEDED(hr) && !notSelected && !extrusion && cut && !midPlane && !reverse)
		{
			hr = create(VARIANT_TRUE);
			flipped = SUCCEEDED(hr) && extrusion;
		}
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError(cut ? "IFeatureManager::FeatureCut4" : "IFeatureManager::FeatureExtrusion3", hr);
		if (notSelected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch");
		if (!extrusion)
		{
			if (cut)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks couldn't cut with " + ToUtf8(sketchName) +
					" -- the sketch must contain a closed, non-self-intersecting profile, and the cut "
					"must pass through material" +
					std::string(midPlane || reverse ? "" : " (tried both directions)"));
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't extrude " + ToUtf8(sketchName) +
				" -- the sketch must contain a closed, non-self-intersecting profile");
		}

		ATL::CComBSTR featureName;
		hr = extrusion->get_Name(&featureName);
		if (FAILED(hr))
			return ComError("IFeature::Name", hr);

		response->set_feature_name(ToUtf8(featureName));
		response->set_sketch_name(ToUtf8(sketchName));
		response->set_direction_flipped(flipped);
		return grpc::Status::OK;
	});
}
