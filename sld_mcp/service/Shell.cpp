// Shell.cpp : ShellBody (hollowing out a solid).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "ReferenceHelpers.h"
#include "SeedHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection marks InsertFeatureShell reads the removed faces and
	// InsertFeatureShellAddThickness the walls of their own thickness from:
	// without them every selected face is removed and the thicknesses are
	// ignored (seen 2026-10-31).
	constexpr long kOpenFaceMark = 1;
	constexpr long kThickFaceMark = 2;

	// More references than anyone selects by hand.
	constexpr int kMaxFaces = 500;

	// Resolves one face into `selection`, refusing anything else.
	grpc::Status AddFace(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref, long mark,
		const std::string& label, Selection& selection)
	{
		if (grpc::Status status = selection.Add(doc, ext, ref, mark, label); !status.ok())
			return status;
		const Pick& pick = selection.picks.back();
		if (!ATL::CComQIPtr<IFace2>(pick.obj.p))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + ": " + pick.what + " isn't a face");
		return grpc::Status::OK;
	}

	// All the faces must belong to one body: one shell per body.
	grpc::Status CheckOneBody(const Selection& open, const Selection& thick)
	{
		ATL::CComPtr<IBody2> first;
		for (const Selection* selection : { &open, &thick })
			for (const Pick& pick : selection->picks)
			{
				ATL::CComQIPtr<IFace2> face(pick.obj.p);
				ATL::CComPtr<IBody2> body;
				if (!face || FAILED(face->IGetBody(&body)) || !body)
					continue;
				if (!first)
					first = body;
				else if (!SameObject(first, body))
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						"those faces belong to more than one body: shell one body per call");
			}
		return grpc::Status::OK;
	}

	// The selection the shell calls read: each wall of its own thickness is
	// appended and given its thickness in a call of its own (one call for two
	// faces, or clearing the selection between them, loses thicknesses; seen
	// 2026-10-31), then the openings or the body are added. On error nothing
	// is selected.
	grpc::Status SelectForShell(IModelDoc2* doc, IModelDocExtension* ext,
		const sld_mcp::v1::ShellBodyRequest& request, const Selection& thick, const Selection& open,
		const std::vector<ATL::CComPtr<IBody2>>& bodies)
	{
		ATL::CComPtr<ISelectionMgr> selectionManager;
		HRESULT hr = doc->get_ISelectionManager(&selectionManager);
		if (FAILED(hr) || !selectionManager)
			return ComError("IModelDoc2::ISelectionManager", hr);
		ATL::CComPtr<ISelectData> data;
		hr = selectionManager->CreateSelectData(&data);
		if (FAILED(hr) || !data)
			return ComError("ISelectionMgr::CreateSelectData", hr);

		const auto wouldntSelect = [&](const std::string& what) {
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks wouldn't select " + what + " -- is it hidden or suppressed?");
		};
		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t i = 0; i < thick.picks.size(); ++i)
		{
			if (!SelectPick(ext, data, thick.picks[i], kThickFaceMark))
				return wouldntSelect("face_thicknesses entry " + std::to_string(i + 1));
			hr = doc->InsertFeatureShellAddThickness(request.face_thicknesses(static_cast<int>(i)).thickness());
			if (FAILED(hr))
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return ComError("IModelDoc2::InsertFeatureShellAddThickness", hr);
			}
		}
		for (size_t i = 0; i < open.picks.size(); ++i)
			if (!SelectPick(ext, data, open.picks[i], kOpenFaceMark))
				return wouldntSelect("open_faces entry " + std::to_string(i + 1));
		return SelectSeedBodies(doc, bodies, kOpenFaceMark);
	}
}

grpc::Status SolidWorksService::ShellBody(grpc::ServerContext* context,
	const sld_mcp::v1::ShellBodyRequest* request,
	sld_mcp::v1::EdgeFeatureResponse* response)
{
	const double thickness = request->thickness();
	if (!(std::isfinite(thickness) && thickness > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "thickness must be positive");
	if (request->open_faces_size() + request->face_thicknesses_size() > kMaxFaces)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxFaces) + " faces per call");
	if (!request->body().empty() && (request->open_faces_size() > 0 || request->face_thicknesses_size() > 0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give a body only for a closed shell of one even thickness: open_faces already say which body");
	for (const auto& entry : request->face_thicknesses())
		if (!(std::isfinite(entry.thickness()) && entry.thickness() > 0.0))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "face thicknesses must be positive");

	return m_executor.Execute(context, "ShellBody", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		Selection open;
		for (int i = 0; i < request->open_faces_size(); ++i)
			if (grpc::Status status = AddFace(doc, ext, request->open_faces(i), kOpenFaceMark,
					"open_faces entry " + std::to_string(i + 1), open);
				!status.ok())
				return status;
		Selection thick;
		for (int i = 0; i < request->face_thicknesses_size(); ++i)
			if (grpc::Status status = AddFace(doc, ext, request->face_thicknesses(i).face(), kThickFaceMark,
					"face_thicknesses entry " + std::to_string(i + 1), thick);
				!status.ok())
				return status;
		std::vector<ATL::CComPtr<IBody2>> bodies;
		if (!request->body().empty())
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, request->body(), obj); !status.ok())
				return grpc::Status(status.error_code(), "body " + status.error_message());
			ATL::CComQIPtr<IBody2> body(obj.p);
			if (!body)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "body: that isn't a body");
			bodies.push_back(body);
		}

		// With nothing selected SolidWorks shells a lone body, and with several
		// bodies makes nothing, without saying why. A closed shell with walls of
		// their own thickness in such a part makes nothing either, and with the
		// body selected too builds the wrong walls (6272 mm^3 for 4832). Faces of
		// two bodies make nothing. (All seen 2026-10-31.)
		const int bodyCount = SolidBodyCount(doc);
		if (bodyCount == 0)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "the active part has no solid body to shell");
		if (bodyCount > 1 && open.picks.empty())
		{
			if (!thick.picks.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"SolidWorks can't make a closed shell with face_thicknesses in a part with several solid bodies "
					"(this one has " + std::to_string(bodyCount) + "): give open_faces too, or use one even thickness");
			if (bodies.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"this part has " + std::to_string(bodyCount) +
					" solid bodies: say which to shell with `body` (a body id from inspect_model), or give open_faces");
		}
		if (grpc::Status status = CheckOneBody(open, thick); !status.ok())
			return status;

		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);

		if (grpc::Status status = SelectForShell(doc, ext, *request, thick, open, bodies); !status.ok())
			return status;

		// InsertFeatureShell returns nothing: the new feature is the new last one.
		const double volumeBefore = SolidVolume(doc);
		ATL::CComPtr<IFeature> lastBefore;
		doc->IFeatureByPositionReverse(0, &lastBefore);
		HRESULT hr = doc->InsertFeatureShell(thickness, request->outward() ? VARIANT_TRUE : VARIANT_FALSE);
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::InsertFeatureShell", hr);
		ATL::CComPtr<IFeature> shell;
		doc->IFeatureByPositionReverse(0, &shell);

		const std::string tooThick =
			"the walls are probably too thick for the body (inward walls must leave room inside: less than half "
			"its thinnest part when closed, and no more than its smallest curve radius); try thinner ones, or outward";
		if (!shell || SameObject(shell, lastBefore))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks couldn't shell that body -- " + tooThick);
		if (grpc::Status status = KeepIfBuilt(doc, shell, tooThick); !status.ok())
			return status;
		// Walls that fill a closed body exactly make a feature with faces and no
		// error that leaves the body as it was (seen 2026-10-31).
		if (std::abs(SolidVolume(doc) - volumeBefore) < kVolumeTolerance)
		{
			const std::string name = FeatureName(shell);
			DeleteFeature(doc, shell);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks made " + name + " but it changed nothing, so it was deleted again -- " + tooThick);
		}

		response->set_feature_name(FeatureName(shell));
		return grpc::Status::OK;
	});
}
