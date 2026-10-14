// SolidWorksService.h : the gRPC service the MCP server talks to.
//
// Implements the RPCs declared in proto/sld_mcp.proto. Adding a tool means:
// add the rpc + messages to the .proto, override the generated method here,
// and implement it in SolidWorksService.cpp.
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

	grpc::Status HoleWizard(grpc::ServerContext* context,
		const sld_mcp::v1::HoleWizardRequest* request,
		sld_mcp::v1::HoleWizardResponse* response) override;

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
