// Loft.cpp : LoftSketch (boss and cut lofts).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection marks InsertProtrusionBlend2 / InsertCutBlend read the
	// profiles (in selection order) and the guide curves from.
	constexpr long kProfileMark = 1;
	constexpr long kGuideMark = 2;

	// More than any loft needs.
	constexpr int kMaxProfiles = 100;
	constexpr int kMaxGuides = 20;

	// A guide curve: any feature by name. SolidWorks decides whether it can
	// guide the loft (a sketch, 3D sketch or other curve).
	grpc::Status FindGuide(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& guide)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		HRESULT hr = part->IFeatureByName(FromUtf8(name), &guide);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!guide)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"guide curve: the active part has no feature named \"" + name + "\"");
		return grpc::Status::OK;
	}

	// A profile sketch's points (their mean) and its normal, in model space.
	bool ProfileFrame(SketchSpace& space, IFeature* feature, Eigen::Vector3d& center, Eigen::Vector3d& normal)
	{
		ATL::CComPtr<ISketch> sketch;
		if (!SketchOf(feature, sketch).ok())
			return false;
		space.Attach(sketch);
		ATL::CComVariant points;
		if (FAILED(sketch->GetSketchPoints2(&points)))
			return false;
		Eigen::Vector3d sum = Eigen::Vector3d::Zero();
		int count = 0;
		for (const auto& disp : comvariant::ToDispatches(points))
		{
			ATL::CComQIPtr<ISketchPoint> point(disp.p);
			double s[3], m[3];
			if (!point || FAILED(point->get_X(&s[0])) || FAILED(point->get_Y(&s[1])) || FAILED(point->get_Z(&s[2])) ||
				!space.ToModel(s, m))
				return false;
			sum += Eigen::Vector3d(m[0], m[1], m[2]);
			++count;
		}
		const double origin[3] = {0, 0, 0}, z[3] = {0, 0, 1};
		double modelOrigin[3], modelZ[3];
		if (count == 0 || !space.ToModel(origin, modelOrigin) || !space.ToModel(z, modelZ))
			return false;
		center = sum / count;
		normal = Eigen::Vector3d(modelZ[0], modelZ[1], modelZ[2]) - Eigen::Vector3d(modelOrigin[0], modelOrigin[1], modelOrigin[2]);
		return true;
	}

	// InsertProtrusionBlend2's Start/EndTangentDir for normal tangency. The
	// tangent runs along the end profile's sketch normal when the flag is
	// true, against it when false, whichever way the loft goes (seen
	// 2026-10-29), so the flag is whether that normal points the way the loft
	// travels there: from `from` to `to` (the first profile to the second at
	// the start, the second-to-last to the last at the end). True if the
	// geometry can't be read.
	bool NormalAlongLoft(SketchSpace& space, IFeature* end, IFeature* from, IFeature* to)
	{
		Eigen::Vector3d endCenter, endNormal, fromCenter, toCenter, unused;
		if (!ProfileFrame(space, end, endCenter, endNormal) || !ProfileFrame(space, from, fromCenter, unused) ||
			!ProfileFrame(space, to, toCenter, unused))
			return true;
		return endNormal.dot(toCenter - fromCenter) >= 0.0;
	}

	// InsertCutBlend has no tangent-direction arguments: its tangents run along
	// the end profiles' sketch normals, as InsertProtrusionBlend2's do with
	// true. Sets them through ILoftFeatureData, whose ReverseStart/
	// EndTangentDirection are the same flags despite their names (a boss made
	// with true reads back true; a cut set to false turns, seen 2026-10-29).
	grpc::Status SetTangentDirections(IModelDoc2* doc, IFeature* loft, bool startAlong, bool endAlong)
	{
		ATL::CComPtr<IDispatch> definition;
		HRESULT hr = loft->GetDefinition(&definition);
		if (FAILED(hr))
			return ComError("IFeature::GetDefinition", hr);
		ATL::CComQIPtr<ILoftFeatureData> data(definition);
		if (!data)
			return grpc::Status(grpc::StatusCode::INTERNAL, "the cut-loft has no ILoftFeatureData");
		VARIANT_BOOL access = VARIANT_FALSE;
		hr = data->AccessSelections(doc, nullptr, &access);
		if (FAILED(hr))
			return ComError("ILoftFeatureData::AccessSelections", hr);
		data->put_ReverseStartTangentDirection(startAlong ? VARIANT_TRUE : VARIANT_FALSE);
		data->put_ReverseEndTangentDirection(endAlong ? VARIANT_TRUE : VARIANT_FALSE);
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = loft->ModifyDefinition(data, doc, nullptr, &ok);
		if (FAILED(hr) || !ok)
			data->ReleaseSelectionAccess();
		if (FAILED(hr))
			return ComError("IFeature::ModifyDefinition", hr);
		if (!ok)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't make the cut-loft normal to its profiles in the direction it goes");
		return grpc::Status::OK;
	}

	// Appends `feature` to the selection with `mark`.
	grpc::Status SelectWithMark(IFeature* feature, long mark, const std::string& name)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		HRESULT hr = feature->Select2(VARIANT_TRUE, mark, &selected);
		if (FAILED(hr))
			return ComError("IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select " + name);
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::LoftSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::LoftSketchRequest* request,
	sld_mcp::v1::LoftSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool cut = request->cut();
	const bool closed = request->closed();
	const int profileCount = request->profiles_size();
	if (profileCount < 2)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a loft needs at least 2 profiles");
	if (profileCount > kMaxProfiles)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxProfiles) + " profiles");
	if (request->guide_curves_size() > kMaxGuides)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxGuides) + " guide curves");
	if (closed && profileCount < 3)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a closed loft needs at least 3 profiles");
	if (closed && (request->start_normal() || request->end_normal()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"a closed loft has no start or end to set the tangency of");

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

	// Resolve everything first; each feature may be used once.
	std::vector<ATL::CComPtr<IFeature>> features;
	std::vector<std::string> names;
	auto addUnique = [&](ATL::CComPtr<IFeature> feature) -> grpc::Status {
		const std::string name = FeatureName(feature);
		for (auto& seen : features)
			if (seen.IsEqualObject(feature))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					name + " is given more than once -- each profile and guide curve must be its own sketch");
		features.push_back(feature);
		names.push_back(name);
		return grpc::Status::OK;
	};
	for (const std::string& name : request->profiles())
	{
		ATL::CComPtr<IFeature> sketch;
		if (grpc::Status status = FindSketchByName(doc, name, sketch); !status.ok())
			return grpc::Status(status.error_code(), "profile: " + status.error_message());
		if (grpc::Status status = addUnique(sketch); !status.ok())
			return status;

		// Informational: whether the profile is fully defined.
		ATL::CComPtr<ISketch> sketchData;
		response->add_profile_status(SketchOf(sketch, sketchData).ok()
			? SketchStatus(sketchData) : sld_mcp::v1::DEFINITION_STATUS_UNSPECIFIED);
	}
	for (const std::string& name : request->guide_curves())
	{
		ATL::CComPtr<IFeature> guide;
		if (grpc::Status status = FindGuide(doc, name, guide); !status.ok())
			return status;
		if (grpc::Status status = addUnique(guide); !status.ok())
			return status;
	}

	// Normal tangency runs the way the loft goes at each end.
	bool startAlong = true, endAlong = true;
	if (request->start_normal() || request->end_normal())
	{
		SketchSpace space;
		if (grpc::Status status = space.Init(swApp); !status.ok())
			return status;
		startAlong = NormalAlongLoft(space, features[0], features[0], features[1]);
		endAlong = NormalAlongLoft(space, features[profileCount - 1], features[profileCount - 2],
			features[profileCount - 1]);
	}

	ATL::CComPtr<IFeatureManager> featureManager;
	HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IFeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	// Select the profiles in order (kProfileMark), then the guide curves
	// (kGuideMark).
	hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	for (size_t i = 0; i < features.size(); ++i)
	{
		const long mark = static_cast<int>(i) < profileCount ? kProfileMark : kGuideMark;
		if (grpc::Status status = SelectWithMark(features[i], mark, names[i]); !status.ok())
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return status;
		}
	}

	const short startMatching = static_cast<short>(request->start_normal() ? swTangencyNormalToProfile : swTangencyNone);
	const short endMatching = static_cast<short>(request->end_normal() ? swTangencyNormalToProfile : swTangencyNone);
	ATL::CComPtr<IFeature> loft;
	if (cut)
		hr = featureManager->InsertCutBlend(
			closed ? VARIANT_TRUE : VARIANT_FALSE, // Closed
			VARIANT_TRUE,      // KeepTangency (merge tangent faces)
			VARIANT_FALSE,     // ForceNonRational
			1.0,               // TessToleranceFactor
			startMatching,     // StartMatchingType
			endMatching,       // EndMatchingType
			VARIANT_FALSE,     // IsThinBody
			0.0,               // Thickness1
			0.0,               // Thickness2
			0,                 // ThinType (not thin)
			VARIANT_TRUE,      // UseFeatScope
			VARIANT_TRUE,      // UseAutoSelect: all bodies in scope
			&loft);
	else
		hr = featureManager->InsertProtrusionBlend2(
			closed ? VARIANT_TRUE : VARIANT_FALSE, // Closed
			VARIANT_TRUE,      // KeepTangency (merge tangent faces)
			VARIANT_FALSE,     // ForceNonRational
			1.0,               // TessToleranceFactor
			startMatching,     // StartMatchingType
			endMatching,       // EndMatchingType
			1.0,               // StartTangentLength (the UI's default weight)
			1.0,               // EndTangentLength
			startAlong ? VARIANT_TRUE : VARIANT_FALSE, // StartTangentDir
			endAlong ? VARIANT_TRUE : VARIANT_FALSE,   // EndTangentDir
			VARIANT_FALSE,     // IsThinBody
			0.0,               // Thickness1
			0.0,               // Thickness2
			0,                 // ThinType (not thin)
			VARIANT_TRUE,      // Merge with existing bodies
			VARIANT_TRUE,      // UseFeatScope
			VARIANT_TRUE,      // UseAutoSelect: all bodies in scope
			swGuideCurveInfluenceNextGuide, // GuideCurveInfluence (the UI's default)
			&loft);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError(cut ? "IFeatureManager::InsertCutBlend" : "IFeatureManager::InsertProtrusionBlend2", hr);
	if (!loft)
	{
		std::string profiles;
		for (int i = 0; i < profileCount; ++i)
			profiles += (i ? ", " : "") + names[i];
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't " + std::string(cut ? "cut-loft" : "loft") + " through " + profiles +
			" -- each profile must be one closed, non-self-intersecting contour (or, first or last, a "
			"single point) on its own plane, the profiles must not cross each other, and every guide curve "
			"must touch every profile" +
			std::string(closed ? "; a closed loft must not cross itself on its way back to the first profile" : "") +
			std::string(cut ? "; the cut must pass through material" : ""));
	}

	if (cut && (!startAlong || !endAlong))
	{
		if (grpc::Status status = SetTangentDirections(doc, loft, startAlong, endAlong); !status.ok())
		{
			DeleteFeature(doc, loft);
			return status;
		}
	}

	response->set_feature_name(FeatureName(loft));
	return grpc::Status::OK;
}
