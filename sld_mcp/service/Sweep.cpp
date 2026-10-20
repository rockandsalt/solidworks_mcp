// Sweep.cpp : SweepSketch (boss and cut sweeps).
#include "pch.h"
#include "Common.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection marks InsertProtrusionSwept4 / InsertCutSwept5 read the
	// profile and the path from.
	constexpr long kProfileMark = 1;
	constexpr long kPathMark = 4;

	// The path: any feature by name. SolidWorks decides whether it can be
	// swept along (a sketch, 3D sketch, helix or other curve).
	grpc::Status FindPath(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& path)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		HRESULT hr = part->IFeatureByName(FromUtf8(name), &path);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!path)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"path: the active part has no feature named \"" + name + "\"");
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::SweepSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SweepSketchRequest* request,
	sld_mcp::v1::SweepSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool cut = request->cut();
	const double diameter = request->circle_diameter();
	const double twist = request->twist_angle();
	const bool circular = diameter != 0.0;
	if (request->path().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the path to sweep along");
	if (circular && !(std::isfinite(diameter) && diameter > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "circle_diameter must be positive");
	if (circular && !request->sketch_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give a profile sketch or a circle diameter, not both");
	if (!std::isfinite(twist))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "twist angle must be a number");
	if (circular && twist != 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a circular profile can't be twisted");

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

	ATL::CComPtr<IFeature> path;
	if (grpc::Status status = FindPath(doc, request->path(), path); !status.ok())
		return status;
	const std::string pathName = FeatureName(path);

	// The profile: none for a circle, else by name if given, else the latest
	// unused sketch that isn't the path.
	ATL::CComPtr<IFeature> sketch;
	std::string sketchName;
	if (!circular)
	{
		if (request->sketch_name().empty())
		{
			if (grpc::Status status = FindLatestUnusedSketch(doc, sketch, path); !status.ok())
				return grpc::Status(status.error_code(), status.error_message() +
					" (other than the path " + pathName + "), or give circle_diameter");
		}
		else
		{
			if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketch); !status.ok())
				return status;
		}
		if (sketch.IsEqualObject(path))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the profile and the path must be different sketches -- " + pathName + " is both");
		sketchName = FeatureName(sketch);

		// Informational: whether the profile is fully defined.
		if (ATL::CComPtr<ISketch> sketchData; SketchOf(sketch, sketchData).ok())
			response->set_sketch_status(SketchStatus(sketchData));
	}

	ATL::CComPtr<IFeatureManager> featureManager;
	HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	// Select the profile (kProfileMark) and the path (kPathMark).
	hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	VARIANT_BOOL selected = VARIANT_FALSE;
	if (sketch)
	{
		hr = sketch->Select2(VARIANT_TRUE, kProfileMark, &selected);
		if (FAILED(hr))
			return ComError("IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select " + sketchName);
	}
	hr = path->Select2(VARIANT_TRUE, kPathMark, &selected);
	if (FAILED(hr))
	{
		doc->ClearSelection2(VARIANT_TRUE);
		return ComError("IFeature::Select2", hr);
	}
	if (!selected)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select " + pathName);
	}

	// Constant twist along the path, else the profile follows the path.
	const long twistControl = twist != 0.0 ? swTwistControlConstantTwistAlongPath : swTwistControlFollowPath;
	ATL::CComPtr<IFeature> sweep;
	if (cut)
		hr = featureManager->InsertCutSwept5(
			VARIANT_FALSE,     // Propagate along tangent edges (edge paths only)
			VARIANT_FALSE,     // Alignment (obsolete)
			twistControl,      // TwistCtrlOption
			VARIANT_FALSE,     // KeepTangency
			VARIANT_FALSE,     // BAdvancedSmoothing
			swTangencyNone,    // StartMatchingType
			swTangencyNone,    // EndMatchingType
			VARIANT_FALSE,     // IsThinBody
			0.0,               // Thickness1
			0.0,               // Thickness2
			0,                 // ThinType (not thin)
			swTangencyNone,    // PathAlign
			VARIANT_TRUE,      // UseFeatScope
			VARIANT_TRUE,      // UseAutoSelect: all bodies in scope
			twist,             // TwistAngle
			VARIANT_TRUE,      // BMergeSmoothFaces
			VARIANT_FALSE,     // AssemblyFeatureScope
			VARIANT_FALSE,     // AutoSelectComponents
			VARIANT_FALSE,     // PropagateFeatureToParts
			circular ? VARIANT_TRUE : VARIANT_FALSE, // CircularProfile
			diameter,          // CircularProfileDiameter
			swSweepDirection1, // Direction (circular profile)
			&sweep);
	else
		hr = featureManager->InsertProtrusionSwept4(
			VARIANT_FALSE,     // Propagate along tangent edges (edge paths only)
			VARIANT_FALSE,     // Alignment (obsolete)
			twistControl,      // TwistCtrlOption
			VARIANT_FALSE,     // KeepTangency
			VARIANT_FALSE,     // BAdvancedSmoothing
			swTangencyNone,    // StartMatchingType
			swTangencyNone,    // EndMatchingType
			VARIANT_FALSE,     // IsThinBody
			0.0,               // Thickness1
			0.0,               // Thickness2
			0,                 // ThinType (not thin)
			swTangencyNone,    // PathAlign
			VARIANT_TRUE,      // Merge with existing bodies
			VARIANT_TRUE,      // UseFeatScope
			VARIANT_TRUE,      // UseAutoSelect: all bodies in scope
			twist,             // TwistAngle
			VARIANT_TRUE,      // BMergeSmoothFaces
			circular ? VARIANT_TRUE : VARIANT_FALSE, // CircularProfile
			diameter,          // CircularProfileDiameter
			swSweepDirection1, // Direction (circular profile)
			&sweep);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError(cut ? "IFeatureManager::InsertCutSwept5" : "IFeatureManager::InsertProtrusionSwept4", hr);
	if (!sweep)
	{
		const std::string what = circular ? "a circle" : sketchName;
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't " + std::string(cut ? "cut-sweep " : "sweep ") + what + " along " + pathName +
			" -- " + std::string(circular ? "" :
				"the profile must be one closed, non-self-intersecting contour, usually on a plane "
				"across the start of the path, and ") +
			"the path must be one chain of curves (a sketch, 3D sketch, helix or curve) whose bends are "
			"wider than the profile, so the result doesn't cross itself" +
			std::string(cut ? "; the cut must pass through material" : ""));
	}

	response->set_feature_name(FeatureName(sweep));
	response->set_sketch_name(sketchName);
	response->set_path(pathName);
	return grpc::Status::OK;
}
