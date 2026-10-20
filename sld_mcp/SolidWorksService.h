// SolidWorksService.h : the gRPC service the MCP server talks to.
//
// Implements the RPCs declared in proto/sld_mcp.proto. Adding a tool means:
// add the rpc + messages to the .proto, override the generated method here,
/// and implement it in the matching service/<Area>.cpp (helpers that more than
// one area needs go in service/*Helpers.h or service/Common.h).
#pragma once

#include "GrpcIncludes.h"
#include <mutex>

class SolidWorksService final : public sld_mcp::v1::SolidWorks::Service
{
public:
	// `git` / `swAppCookie` identify the ISldWorks registered in the Global
	// Interface Table by CSwAddin. Both must outlive this service.
	SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie);

	grpc::Status GetActiveDocument(grpc::ServerContext* context,
		const sld_mcp::v1::GetActiveDocumentRequest* request,
		sld_mcp::v1::GetActiveDocumentResponse* response) override;

	grpc::Status NewPart(grpc::ServerContext* context,
		const sld_mcp::v1::NewPartRequest* request,
		sld_mcp::v1::NewPartResponse* response) override;

	grpc::Status CreateRectangleSketch(grpc::ServerContext* context,
		const sld_mcp::v1::CreateRectangleSketchRequest* request,
		sld_mcp::v1::CreateRectangleSketchResponse* response) override;

	grpc::Status AddSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::AddSketchEntitiesRequest* request,
		sld_mcp::v1::AddSketchEntitiesResponse* response) override;

	grpc::Status ExtrudeSketch(grpc::ServerContext* context,
		const sld_mcp::v1::ExtrudeSketchRequest* request,
		sld_mcp::v1::ExtrudeSketchResponse* response) override;

	grpc::Status RevolveSketch(grpc::ServerContext* context,
		const sld_mcp::v1::RevolveSketchRequest* request,
		sld_mcp::v1::RevolveSketchResponse* response) override;

	grpc::Status InspectModel(grpc::ServerContext* context,
		const sld_mcp::v1::InspectModelRequest* request,
		sld_mcp::v1::InspectModelResponse* response) override;

	grpc::Status GetMesh(grpc::ServerContext* context,
		const sld_mcp::v1::GetMeshRequest* request,
		sld_mcp::v1::GetMeshResponse* response) override;

	grpc::Status InspectSketch(grpc::ServerContext* context,
		const sld_mcp::v1::InspectSketchRequest* request,
		sld_mcp::v1::InspectSketchResponse* response) override;

	grpc::Status AddSketchDimensions(grpc::ServerContext* context,
		const sld_mcp::v1::AddSketchDimensionsRequest* request,
		sld_mcp::v1::AddSketchDimensionsResponse* response) override;

	grpc::Status AddSketchRelations(grpc::ServerContext* context,
		const sld_mcp::v1::AddSketchRelationsRequest* request,
		sld_mcp::v1::AddSketchRelationsResponse* response) override;

	grpc::Status FullyDefineSketch(grpc::ServerContext* context,
		const sld_mcp::v1::FullyDefineSketchRequest* request,
		sld_mcp::v1::FullyDefineSketchResponse* response) override;

	grpc::Status SetDimensionValue(grpc::ServerContext* context,
		const sld_mcp::v1::SetDimensionValueRequest* request,
		sld_mcp::v1::SetDimensionValueResponse* response) override;

	grpc::Status SetDimensionTolerance(grpc::ServerContext* context,
		const sld_mcp::v1::SetDimensionToleranceRequest* request,
		sld_mcp::v1::SetDimensionToleranceResponse* response) override;

	grpc::Status HoleWizard(grpc::ServerContext* context,
		const sld_mcp::v1::HoleWizardRequest* request,
		sld_mcp::v1::HoleWizardResponse* response) override;

	grpc::Status AddMate(grpc::ServerContext* context,
		const sld_mcp::v1::AddMateRequest* request,
		sld_mcp::v1::AddMateResponse* response) override;

	grpc::Status InspectAnnotations(grpc::ServerContext* context,
		const sld_mcp::v1::InspectAnnotationsRequest* request,
		sld_mcp::v1::InspectAnnotationsResponse* response) override;

	grpc::Status InspectDrawing(grpc::ServerContext* context,
		const sld_mcp::v1::InspectDrawingRequest* request,
		sld_mcp::v1::InspectDrawingResponse* response) override;

	grpc::Status AddDrawingDimensions(grpc::ServerContext* context,
		const sld_mcp::v1::AddDrawingDimensionsRequest* request,
		sld_mcp::v1::AddDrawingDimensionsResponse* response) override;

	grpc::Status InsertModelItems(grpc::ServerContext* context,
		const sld_mcp::v1::InsertModelItemsRequest* request,
		sld_mcp::v1::InsertModelItemsResponse* response) override;

	grpc::Status RenderDrawing(grpc::ServerContext* context,
		const sld_mcp::v1::RenderDrawingRequest* request,
		sld_mcp::v1::RenderDrawingResponse* response) override;

	grpc::Status DeleteFeatures(grpc::ServerContext* context,
		const sld_mcp::v1::DeleteFeaturesRequest* request,
		sld_mcp::v1::DeleteFeaturesResponse* response) override;

	grpc::Status DeleteSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::DeleteSketchEntitiesRequest* request,
		sld_mcp::v1::DeleteSketchEntitiesResponse* response) override;

	grpc::Status DiagnoseModel(grpc::ServerContext* context,
		const sld_mcp::v1::DiagnoseModelRequest* request,
		sld_mcp::v1::DiagnoseModelResponse* response) override;

	grpc::Status EditExtrude(grpc::ServerContext* context,
		const sld_mcp::v1::EditExtrudeRequest* request,
		sld_mcp::v1::EditExtrudeResponse* response) override;

	grpc::Status EditMate(grpc::ServerContext* context,
		const sld_mcp::v1::EditMateRequest* request,
		sld_mcp::v1::EditMateResponse* response) override;

	grpc::Status SaveDocument(grpc::ServerContext* context,
		const sld_mcp::v1::SaveDocumentRequest* request,
		sld_mcp::v1::SaveDocumentResponse* response) override;

	grpc::Status OpenDocument(grpc::ServerContext* context,
		const sld_mcp::v1::OpenDocumentRequest* request,
		sld_mcp::v1::OpenDocumentResponse* response) override;

	grpc::Status CreateReferenceGeometry(grpc::ServerContext* context,
		const sld_mcp::v1::CreateReferenceGeometryRequest* request,
		sld_mcp::v1::CreateReferenceGeometryResponse* response) override;

private:
	// gRPC runs handlers on its own thread pool, not on SolidWorks' main
	// STA thread, so ISldWorks can't be used directly. This fetches a
	// marshaled proxy from the GIT; COM then forwards every call made on it
	// to the main thread. Call only inside a ComApartment scope (see .cpp).
	grpc::Status GetSwApp(ATL::CComPtr<ISldWorks>& swApp) const;

	ATL::CComPtr<IGlobalInterfaceTable> m_git;
	DWORD m_swAppCookie;

	// Serializes RPCs that change the model. Each COM call is marshaled to
	// SolidWorks' main thread on its own, so two concurrent multi-step RPCs
	// (select plane, open sketch, ...) could otherwise interleave their
	// steps and clobber each other's selection or open sketch.
	std::mutex m_editMutex;
};
