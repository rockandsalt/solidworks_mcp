// Sweep.cpp : SweepSketch (boss and cut sweeps), EditSweep.
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
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

	// A sweep's settings, as ISweepFeatureData has them.
	struct SweepSettings
	{
		bool cut = false;
		bool circular = false;
		double diameter = 0.0;
		ATL::CComPtr<IFeature> profile;  // null for a circular profile
		ATL::CComPtr<IFeature> path;
		short twistControl = swTwistControlFollowPath;
		double twist = 0.0;
		short guides = 0;
	};

	grpc::Status SweepDataOf(IFeature* feature, ATL::CComQIPtr<ISweepFeatureData>& data)
	{
		ATL::CComPtr<IDispatch> definition;
		HRESULT hr = feature->GetDefinition(&definition);
		if (FAILED(hr))
			return ComError("IFeature::GetDefinition", hr);
		data = definition.p;
		if (!data)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + FeatureName(feature) + "\" is a " + FeatureTypeName(feature) + " feature, not a sweep");
		return grpc::Status::OK;
	}

	// Reads the settings; the selections must be accessed.
	SweepSettings ReadSweep(IFeature* feature, ISweepFeatureData* data)
	{
		SweepSettings s;
		s.cut = FeatureTypeName(feature).find("Cut") != std::string::npos;
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_CircularProfile(&flag);
		s.circular = flag != VARIANT_FALSE;
		if (s.circular)
			data->get_CircularProfileDiameter(&s.diameter);
		ATL::CComPtr<IDispatch> profile, path;
		if (!s.circular && SUCCEEDED(data->get_Profile(&profile)))
			s.profile = ATL::CComQIPtr<IFeature>(profile);
		if (SUCCEEDED(data->get_Path(&path)))
			s.path = ATL::CComQIPtr<IFeature>(path);
		data->get_TwistControlType(&s.twistControl);
		data->GetTwistAngle(&s.twist);
		data->GetGuideCurvesCount(&s.guides);
		return s;
	}

	grpc::Status ReadSweepNow(IModelDoc2* doc, IFeature* feature, SweepSettings& s)
	{
		ATL::CComQIPtr<ISweepFeatureData> data;
		if (grpc::Status status = SweepDataOf(feature, data); !status.ok())
			return status;
		VARIANT_BOOL access = VARIANT_FALSE;
		HRESULT hr = data->AccessSelections(doc, nullptr, &access);
		if (FAILED(hr))
			return ComError("ISweepFeatureData::AccessSelections", hr);
		s = ReadSweep(feature, data);
		data->ReleaseSelectionAccess();
		return grpc::Status::OK;
	}

	std::string NameOr(IFeature* feature, const char* none)
	{
		return feature ? FeatureName(feature) : std::string(none);
	}

	grpc::Status DescribeSweep(IModelDoc2* doc, IFeature* feature, sld_mcp::v1::SweepDefinition* out,
		SweepSettings* settings)
	{
		SweepSettings s;
		if (grpc::Status status = ReadSweepNow(doc, feature, s); !status.ok())
			return status;
		out->set_name(FeatureName(feature));
		out->set_type_name(FeatureTypeName(feature));
		out->set_cut(s.cut);
		if (s.circular)
			out->set_circle_diameter(s.diameter);
		else
			out->set_profile(NameOr(s.profile, "?"));
		out->set_path(NameOr(s.path, "?"));
		out->set_twist_control(s.twistControl);
		if (s.twistControl == swTwistControlConstantTwistAlongPath)
			out->set_twist_angle(s.twist);
		out->set_guide_curves(s.guides);
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

	// Puts a sketch / curve feature as the profile or path: the feature
	// itself, or else the object behind it (an ISketch), whichever SolidWorks
	// keeps (read back through `get`).
	template <typename Put, typename Get>
	void PutFeature(IFeature* feature, Put put, Get get)
	{
		put(feature);
		ATL::CComPtr<IDispatch> now;
		if (SUCCEEDED(get(&now)) && ATL::CComPtr<IFeature>(feature).IsEqualObject(now))
			return;
		ATL::CComPtr<IDispatch> specific;
		if (SUCCEEDED(feature->GetSpecificFeature2(&specific)) && specific)
			put(specific);
	}

	// Gives the sweep `target`'s path, profile and twist with
	// IFeature::ModifyDefinition.
	grpc::Status ApplySweep(IModelDoc2* doc, IFeature* feature, const SweepSettings& target, bool& modified)
	{
		modified = false;
		ATL::CComQIPtr<ISweepFeatureData> data;
		if (grpc::Status status = SweepDataOf(feature, data); !status.ok())
			return status;
		VARIANT_BOOL access = VARIANT_FALSE;
		HRESULT hr = data->AccessSelections(doc, nullptr, &access);
		if (FAILED(hr))
			return ComError("ISweepFeatureData::AccessSelections", hr);
		const SweepSettings current = ReadSweep(feature, data);

		auto same = [](IFeature* a, IFeature* b) { return a && b && ATL::CComPtr<IFeature>(a).IsEqualObject(b); };
		if (target.path && !same(target.path, current.path))
			PutFeature(target.path, [&](IDispatch* p) { return data->put_Path(p); },
				[&](IDispatch** p) { return data->get_Path(p); });
		if (target.circular)
		{
			if (!current.circular)
				data->put_CircularProfile(VARIANT_TRUE);
			if (std::abs(target.diameter - current.diameter) > 1e-12)
				data->put_CircularProfileDiameter(target.diameter);
		}
		else
		{
			if (current.circular)
				data->put_CircularProfile(VARIANT_FALSE);
			if (target.profile && !same(target.profile, current.profile))
				PutFeature(target.profile, [&](IDispatch* p) { return data->put_Profile(p); },
					[&](IDispatch** p) { return data->get_Profile(p); });
		}
		if (target.twistControl != current.twistControl)
			data->put_TwistControlType(target.twistControl);
		if (target.twistControl == swTwistControlConstantTwistAlongPath && std::abs(target.twist - current.twist) > 1e-12)
			data->SetTwistAngle(target.twist);

		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = feature->ModifyDefinition(data, doc, nullptr, &ok);
		if (FAILED(hr) || !ok)
			data->ReleaseSelectionAccess();
		if (FAILED(hr))
			return ComError("IFeature::ModifyDefinition", hr);
		modified = ok != VARIANT_FALSE;
		return grpc::Status::OK;
	}
}

grpc::Status sld_mcp::service::DescribeSweepDefinition(IModelDoc2* doc, IFeature* feature,
	sld_mcp::v1::SweepDefinition* out)
{
	return DescribeSweep(doc, feature, out, nullptr);
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

grpc::Status SolidWorksService::EditSweep(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::EditSweepRequest* request,
	sld_mcp::v1::EditSweepResponse* response)
{
	// Validate before touching SolidWorks.
	if (request->has_circle_diameter() &&
		!(std::isfinite(request->circle_diameter()) && request->circle_diameter() > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "circle_diameter must be positive");
	if (request->has_circle_diameter() && !request->sketch_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give a profile sketch or a circle diameter, not both");
	if (request->has_twist_angle() && !std::isfinite(request->twist_angle()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "twist angle must be a number");
	const bool anyChange = !request->path().empty() || !request->sketch_name().empty() ||
		request->has_circle_diameter() || request->has_twist_angle();

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

	SweepSettings before;
	if (grpc::Status status = DescribeSweep(doc, feature, response->mutable_before(), &before); !status.ok())
		return status;
	if (!anyChange)
		return grpc::Status::OK;

	// What it should become: the current settings with the request's changes.
	SweepSettings target = before;
	// (Found into fresh pointers: ATL asserts on taking the address of a set one.)
	if (!request->path().empty())
	{
		ATL::CComPtr<IFeature> path;
		if (grpc::Status status = FindPath(doc, request->path(), path); !status.ok())
			return status;
		target.path = path;
	}
	if (!request->sketch_name().empty())
	{
		ATL::CComPtr<IFeature> sketch;
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketch); !status.ok())
			return status;
		target.profile = sketch;
		target.circular = false;
	}
	if (request->has_circle_diameter())
	{
		target.circular = true;
		target.diameter = request->circle_diameter();
		target.profile.Release();
	}
	if (!target.circular && !target.profile)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't hand out \"" + name + "\"'s profile -- give sketch_name or circle_diameter");
	if (!target.path)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't hand out \"" + name + "\"'s path -- give the path");
	if (target.profile && target.profile.IsEqualObject(target.path))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"the profile and the path must be different sketches -- " + FeatureName(target.path) + " is both");
	if (request->has_twist_angle())
	{
		if (request->twist_angle() != 0.0)
		{
			target.twistControl = swTwistControlConstantTwistAlongPath;
			target.twist = request->twist_angle();
		}
		else if (target.twistControl == swTwistControlConstantTwistAlongPath)
			target.twistControl = swTwistControlFollowPath;
	}
	if (target.circular && target.twistControl == swTwistControlConstantTwistAlongPath && target.twist != 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"a circular profile can't be twisted -- give twist 0 with it");

	DefinitionEdit edit;
	edit.apply = [&](bool& modified) { return ApplySweep(doc, feature, target, modified); };
	edit.restore = [&]() {
		bool ignored = false;
		ApplySweep(doc, feature, before, ignored);
	};
	edit.check = [&]() -> std::string {
		SweepSettings now;
		if (!ReadSweepNow(doc, feature, now).ok())
			return "no definition SolidWorks would hand out";
		if (!now.path || !now.path.IsEqualObject(target.path))
			return "path " + NameOr(now.path, "(none)");
		if (now.circular != target.circular)
			return now.circular ? "a circular profile" : "a profile sketch";
		if (target.circular && std::abs(now.diameter - target.diameter) > 1e-9)
			return "a " + std::to_string(now.diameter * 1000.0) + " mm circle";
		if (!target.circular && (!now.profile || !now.profile.IsEqualObject(target.profile)))
			return "profile " + NameOr(now.profile, "(none)");
		if (now.twistControl != target.twistControl ||
			(target.twistControl == swTwistControlConstantTwistAlongPath && std::abs(now.twist - target.twist) > 1e-9))
			return "twist control " + std::to_string(now.twistControl) + ", " +
				std::to_string(Degrees(now.twist)) + " degrees";
		return {};
	};
	edit.rejected = "e.g. a profile that isn't one closed contour across the path, a path that isn't one chain of "
		"curves, bends tighter than the profile, or a cut that no longer passes through material";
	if (grpc::Status status = EditDefinition(swApp, doc, feature, tree, edit, response->mutable_new_errors());
		!status.ok())
		return status;
	return DescribeSweep(doc, feature, response->mutable_after(), nullptr);
}
