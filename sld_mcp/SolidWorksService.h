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
