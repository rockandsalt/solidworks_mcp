// SolidWorksService.cpp : construction and the ISldWorks proxy. The RPCs are
// implemented in service/, one file per area; shared helpers are in
// service/*Helpers.h and service/Common.h.
#include "pch.h"
#include "SolidWorksService.h"

SolidWorksService::SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie)
	: m_git(git), m_swAppCookie(swAppCookie)
{
}

grpc::Status SolidWorksService::GetSwApp(ATL::CComPtr<ISldWorks>& swApp) const
{
	HRESULT hr = m_git->GetInterfaceFromGlobal(m_swAppCookie, __uuidof(ISldWorks),
		reinterpret_cast<void**>(&swApp));
	if (FAILED(hr))
		return grpc::Status(grpc::StatusCode::UNAVAILABLE, "lost connection to SolidWorks");
	return grpc::Status::OK;
}
