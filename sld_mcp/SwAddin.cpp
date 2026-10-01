// SwAddin.cpp : Implementation of CSwAddin

#include "pch.h"
#include "framework.h"
#include "resource.h"
#include "sldmcp_i.h"
#include "SwAddin.h"
#include "RpcServer.h"
#include "SolidWorksService.h"
#include <objidl.h>
#include <cwchar>
#include <exception>
#include <string>

namespace
{
	// Must match DEFAULT_PORT / PORT_ENV in mcp_server/src/addin.rs.
	constexpr int kDefaultPort = 50551;
	const wchar_t kPortEnvVar[] = L"SLD_MCP_PORT";

	// From swconst.tlb (swMessageBoxIcon_e / swMessageBoxBtn_e), which this
	// project doesn't #import.
	constexpr long kSwMbWarning = 1;
	constexpr long kSwMbOk = 2;

	int GetPort()
	{
		wchar_t value[16] = {};
		DWORD len = GetEnvironmentVariableW(kPortEnvVar, value, static_cast<DWORD>(std::size(value)));
		if (len == 0 || len >= std::size(value))
			return kDefaultPort;

		wchar_t* end = nullptr;
		long port = std::wcstol(value, &end, 10);
		if (*end != L'\0' || port <= 0 || port > 65535)
			return kDefaultPort;
		return static_cast<int>(port);
	}

	// grpc++ keeps internal background threads (timers, its event engine)
	// alive after the server is destroyed, and SolidWorks unloads add-in
	// DLLs when they're disabled. Pin the DLL so those threads never end up
	// executing unmapped code.
	void PinThisModule()
	{
		HMODULE self = nullptr;
		GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
			reinterpret_cast<LPCWSTR>(&PinThisModule), &self);
	}
}

CSwAddin::~CSwAddin() = default;

STDMETHODIMP CSwAddin::ConnectToSW(IDispatch* ThisSW, long Cookie, VARIANT_BOOL* IsConnected)
{
	if (IsConnected == nullptr)
		return E_POINTER;

	*IsConnected = VARIANT_FALSE;

	if (ThisSW == nullptr)
		return E_INVALIDARG;

	m_addinCookie = Cookie;
	m_swApp.Release();
	HRESULT hr = ThisSW->QueryInterface(__uuidof(ISldWorks), reinterpret_cast<void**>(&m_swApp));
	if (FAILED(hr))
		return hr;

	hr = m_git.CoCreateInstance(CLSID_StdGlobalInterfaceTable, nullptr, CLSCTX_INPROC_SERVER);
	if (FAILED(hr))
	{
		ReleaseSolidWorks();
		return hr;
	}

	hr = m_git->RegisterInterfaceInGlobal(m_swApp, __uuidof(ISldWorks), &m_swAppGitCookie);
	if (FAILED(hr))
	{
		ReleaseSolidWorks();
		return hr;
	}

	PinThisModule();

	try
	{
		m_rpcServer = std::make_unique<RpcServer>(GetPort(),
			std::make_unique<SolidWorksService>(m_git, m_swAppGitCookie));
	}
	catch (const std::exception& ex)
	{
		std::wstring message = L"sld_mcp: the MCP bridge couldn't start, so it won't be available "
			L"in this SolidWorks session.\n\n";
		message += ATL::CA2W(ex.what(), CP_UTF8);
		long result = 0;
		m_swApp->SendMsgToUser2(ATL::CComBSTR(message.c_str()), kSwMbWarning, kSwMbOk, &result);

		ReleaseSolidWorks();
		return E_FAIL;
	}

	*IsConnected = VARIANT_TRUE;
	return S_OK;
}

STDMETHODIMP CSwAddin::DisconnectFromSW(VARIANT_BOOL* IsDisconnected)
{
	if (IsDisconnected == nullptr)
		return E_POINTER;

	// Stop the server (waiting out in-flight calls) before tearing down the
	// GIT registration its handlers depend on.
	if (m_rpcServer)
	{
		m_rpcServer->Shutdown();
		m_rpcServer.reset();
	}

	ReleaseSolidWorks();
	*IsDisconnected = VARIANT_TRUE;
	return S_OK;
}

void CSwAddin::ReleaseSolidWorks()
{
	if (m_git && m_swAppGitCookie != 0)
	{
		m_git->RevokeInterfaceFromGlobal(m_swAppGitCookie);
		m_swAppGitCookie = 0;
	}
	m_git.Release();
	m_swApp.Release();
}
