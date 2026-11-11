// Draft.cpp : DraftFaces (tapering faces about a neutral plane).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SeedHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection marks InsertMultiFaceDraft reads its inputs from.
	constexpr long kNeutralPlaneMark = 1;
	constexpr long kFaceMark = 2;

	// More references than anyone selects by hand.
	constexpr int kMaxFaces = 500;

	grpc::Status CheckNeutralPlane(const Pick& pick)
	{
		const std::string hint = " -- give a planar face, a reference plane or a default plane";
		if (pick.kind == Pick::Entity)
		{
			ATL::CComQIPtr<IFace2> face(pick.obj.p);
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL planar = VARIANT_FALSE;
			if (face && SUCCEEDED(face->IGetSurface(&surface)) && surface && SUCCEEDED(surface->IsPlane(&planar)) &&
				planar)
				return grpc::Status::OK;
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"neutral_plane: " + std::string(face ? "the face isn't flat" : "a " + pick.what + " isn't a plane") + hint);
		}
		if (pick.kind == Pick::Feature)
		{
			ATL::CComQIPtr<IFeature> feature(pick.obj.p);
			if (feature && FeatureTypeName(feature) == "RefPlane")
				return grpc::Status::OK;
		}
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"neutral_plane: " + pick.what + " isn't a plane" + hint);
	}
}

grpc::Status SolidWorksService::DraftFaces(grpc::ServerContext* context,
	const sld_mcp::v1::DraftFacesRequest* request,
	sld_mcp::v1::DraftFacesResponse* response)
{
	const double angle = request->angle();
	if (!(std::isfinite(angle) && angle > 0.0 && angle < M_PI_2))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "angle must be more than 0 and less than 90 degrees");
	if (request->faces().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one face to draft");
	if (request->faces_size() > kMaxFaces)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxFaces) + " faces per call");
	if (request->propagation() < swFacePropNone || request->propagation() > swFacePropOuterLoops)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown propagation");
	if (request->neutral_plane().target_case() == sld_mcp::v1::GeometryRef::TARGET_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the neutral plane");

	return m_executor.Execute(context, "DraftFaces", ExecutionPolicy::CompleteOperation,
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

		Selection selection;
		if (grpc::Status status = selection.Add(doc, ext, request->neutral_plane(), kNeutralPlaneMark, "neutral_plane");
			!status.ok())
			return status;
		if (grpc::Status status = CheckNeutralPlane(selection.picks.back()); !status.ok())
			return status;
		for (int i = 0; i < request->faces_size(); ++i)
		{
			const std::string label = "faces entry " + std::to_string(i + 1);
			if (grpc::Status status = selection.Add(doc, ext, request->faces(i), kFaceMark, label); !status.ok())
				return status;
			const Pick& pick = selection.picks.back();
			if (!ATL::CComQIPtr<IFace2>(pick.obj.p))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + ": " + pick.what + " isn't a face");
		}

		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);

		if (grpc::Status status = selection.Select(doc, ext); !status.ok())
			return status;

		const double volumeBefore = SolidVolume(doc);
		ATL::CComPtr<IFeature> draft;
		hr = featureManager->InsertMultiFaceDraft(
			angle,                                       // Angle
			request->flip() ? VARIANT_TRUE : VARIANT_FALSE, // FlipDir
			VARIANT_FALSE,                               // EdgeDraft: not parting line
			request->propagation(),                      // PropType
			VARIANT_FALSE,                               // IsStepDraft
			VARIANT_FALSE,                               // IsBodyDraft
			&draft);
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IFeatureManager::InsertMultiFaceDraft", hr);

		const std::string why =
			"each face must meet the neutral plane's direction at an angle the draft can take: faces flat to the "
			"neutral plane can't be drafted, and neighbors must have room to follow; try a smaller angle, or flip";
		if (!draft)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks couldn't draft those faces -- " + why);
		// Not KeepIfBuilt: a draft that worked has GetFaceCount 0, and no volume
		// check either: about a plane through the middle of a wall the volume stays
		// the same (both seen 2026-11-01).
		VARIANT_BOOL warning = VARIANT_FALSE;
		long error = swFeatureErrorNone;
		if (SUCCEEDED(draft->GetErrorCode2(&warning, &error)) && error != swFeatureErrorNone && !warning)
		{
			const std::string name = FeatureName(draft);
			DeleteFeature(doc, draft);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks made " + name + " but it failed to rebuild (swFeatureError_e " + std::to_string(error) +
				"), so it was deleted again -- " + why);
		}

		response->set_feature_name(FeatureName(draft));
		response->set_volume_change(SolidVolume(doc) - volumeBefore);
		return grpc::Status::OK;
	});
}
