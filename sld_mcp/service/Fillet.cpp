// Fillet.cpp : FilletEdges and ChamferEdges (rounding and beveling edges).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "ReferenceHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection mark FeatureFillet3 / InsertFeatureChamfer read the
	// edges and faces from (InsertFeatureChamfer takes 0 or 1, seen
	// 2026-10-19).
	constexpr long kEdgeMark = 1;

	constexpr double kRightAngle = 3.14159265358979323846 / 2.0;

	// More references than anyone selects by hand.
	constexpr int kMaxEdges = 500;

	// The active part and its edges / faces, selected with `mark`, ready for
	// a feature that works on the selection. On error nothing is selected.
	grpc::Status SelectEdges(ISldWorks* swApp,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::GeometryRef>& refs, long mark,
		ATL::CComPtr<IModelDoc2>& doc, ATL::CComPtr<IFeatureManager>& featureManager)
	{
		if (refs.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one edge or face");
		if (refs.size() > kMaxEdges)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"at most " + std::to_string(kMaxEdges) + " edges and faces per call");

		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		HRESULT hr = doc->get_FeatureManager(&featureManager);
		if (FAILED(hr))
			return ComError("IModelDoc2::FeatureManager", hr);
		if (!featureManager)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

		Selection selection;
		for (int i = 0; i < refs.size(); ++i)
		{
			const std::string label = "edges entry " + std::to_string(i + 1);
			if (grpc::Status status = selection.Add(doc, ext, refs[i], mark, label); !status.ok())
				return status;
			const Pick& pick = selection.picks.back();
			if (pick.kind != Pick::Entity || pick.what == "vertex")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					label + ": " + pick.what + " isn't an edge or a face");
		}
		return selection.Select(doc, ext);
	}

	// What the feature call gave back: a feature that made no faces (an
	// equal-distance InsertFeatureChamfer does that, seen 2026-10-19) is
	// deleted and counts as failed, like no feature.
	grpc::Status CheckMadeFaces(IModelDoc2* doc, IFeature* feature, const std::string& failure)
	{
		long faces = 0;
		if (feature && SUCCEEDED(feature->GetFaceCount(&faces)) && faces > 0)
			return grpc::Status::OK;
		if (feature)
			DeleteFeature(doc, feature);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, failure);
	}
}

grpc::Status SolidWorksService::FilletEdges(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::FilletEdgesRequest* request,
	sld_mcp::v1::EdgeFeatureResponse* response)
{
	const double radius = request->radius();
	if (!(std::isfinite(radius) && radius > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "radius must be positive");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComPtr<IFeatureManager> featureManager;
	if (grpc::Status status = SelectEdges(swApp, request->edges(), kEdgeMark, doc, featureManager);
		!status.ok())
		return status;

	// A constant-radius fillet, as the UI makes one: uniform radius, keep
	// features, attach edges, plus tangent propagation if asked.
	const long options = swFeatureFilletUniformRadius | swFeatureFilletAttachEdges |
		swFeatureFilletKeepFeatures | (request->tangent_propagation() ? swFeatureFilletPropagate : 0);
	ATL::CComVariant none;
	ATL::CComPtr<IDispatch> filletObj;
	HRESULT hr = featureManager->FeatureFillet3(
		options,
		radius,                        // R1
		0.0,                           // R2 (asymmetric fillets)
		0.0,                           // Rho (conic fillets)
		swFeatureFilletType_Simple,    // Ftyp: constant radius
		swFilletOverFlowType_Default,  // OverflowType
		swFeatureFilletCircular,       // ConicRhoType: circular profile
		none,                          // Radii (per-edge radii)
		none,                          // Dist2Arr
		none,                          // RhoArr
		none,                          // SetBackDistances
		none,                          // PointRadiusArray
		none,                          // PointDist2Array
		none,                          // PointRhoArray
		&filletObj);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IFeatureManager::FeatureFillet3", hr);
	ATL::CComQIPtr<IFeature> fillet(filletObj);
	if (grpc::Status status = CheckMadeFaces(doc, fillet,
			"SolidWorks couldn't fillet those edges -- the radius is probably too big for them (it must "
			"fit within the faces on both sides of each edge, and next to other fillets); try a smaller one");
		!status.ok())
		return status;

	response->set_feature_name(FeatureName(fillet));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::ChamferEdges(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::ChamferEdgesRequest* request,
	sld_mcp::v1::EdgeFeatureResponse* response)
{
	const double distance = request->distance();
	const double angle = request->angle();
	const double otherDistance = request->other_distance();
	if (!(std::isfinite(distance) && distance > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "distance must be positive");
	if (!(std::isfinite(angle) && angle >= 0.0 && angle < kRightAngle))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "angle must be more than 0 and less than 90 degrees");
	if (!(std::isfinite(otherDistance) && otherDistance >= 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "other distance must be positive");
	if (angle > 0.0 && otherDistance > 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give an angle or a second distance, not both");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComPtr<IFeatureManager> featureManager;
	if (grpc::Status status = SelectEdges(swApp, request->edges(), kEdgeMark, doc, featureManager);
		!status.ok())
		return status;

	// Equal distance = angle-distance at 45 degrees: swChamferEqualDistance
	// makes an empty feature through this call. Distance-distance ignores
	// swFeatureChamferFlipDirection, so flipping it swaps the distances
	// (both seen 2026-10-19).
	const bool equal = angle == 0.0 && otherDistance == 0.0;
	const bool twoDistances = otherDistance > 0.0;
	const bool swap = twoDistances && request->flip();
	const long type = twoDistances ? swChamferDistanceDistance : swChamferAngleDistance;
	const long options = (request->flip() && !equal && !twoDistances ? swFeatureChamferFlipDirection : 0) |
		(request->tangent_propagation() ? swFeatureChamferTangentPropagation : 0);
	ATL::CComPtr<IFeature> chamfer;
	HRESULT hr = featureManager->InsertFeatureChamfer(
		options,
		type,                        // ChamferType
		swap ? otherDistance : distance, // Width
		equal ? kRightAngle / 2.0 : angle, // Angle (angle-distance)
		swap ? distance : otherDistance, // OtherDist (distance-distance)
		0.0,                         // VertexChamDist1..3 (vertex chamfers)
		0.0,
		0.0,
		&chamfer);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IFeatureManager::InsertFeatureChamfer", hr);
	if (grpc::Status status = CheckMadeFaces(doc, chamfer,
			"SolidWorks couldn't chamfer those edges -- the chamfer is probably too big for them (it must "
			"fit within the faces on both sides of each edge); try a smaller distance");
		!status.ok())
		return status;

	response->set_feature_name(FeatureName(chamfer));
	return grpc::Status::OK;
}
