// Macro.cpp : RunMacro (the run_macro tool's plain-text VBA macro file).
#include "pch.h"
#include "Common.h"

using namespace sld_mcp::service;

grpc::Status SolidWorksService::RunMacro(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::RunMacroRequest* request,
	sld_mcp::v1::RunMacroResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	// A .swb always starts at main, whatever the names passed here. Its false
	// result comes with error 0 for a macro that doesn't compile, a missing file
	// and a macro stopped by End (run a second time first!) alike (seen
	// 2026-11-01), so the error isn't reported. Unloading lets the caller delete
	// the file.
	long error = 0;
	VARIANT_BOOL ran = VARIANT_FALSE;
	const HRESULT hr = swApp->RunMacro2(FromUtf8(request->path()), ATL::CComBSTR(L"macro"), ATL::CComBSTR(L"main"),
		swRunMacroUnloadAfterRun, &error, &ran);
	if (FAILED(hr))
		return ComError("ISldWorks::RunMacro2", hr);
	response->set_ran(ran != VARIANT_FALSE);
	return grpc::Status::OK;
}
