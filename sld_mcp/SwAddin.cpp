// SwAddin.cpp : Implementation of CSwAddin

#include "pch.h"
#include "framework.h"
#include "resource.h"
#include "sldmcp_i.h"
#include "SwAddin.h"

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

	ATL::CComBSTR docName(L"(no active document)");
	ATL::CComPtr<IModelDoc2> activeDoc;
	if (SUCCEEDED(m_swApp->get_IActiveDoc2(&activeDoc)) && activeDoc)
	{
		ATL::CComBSTR title;
		if (SUCCEEDED(activeDoc->GetTitle(&title)) && title.Length() > 0)
			docName = title;
	}

	// First entry point: prove the add-in loads and can reach the SolidWorks
	// object model. This will be replaced by the Rust IPC bridge.
	::MessageBoxW(nullptr, docName, L"sld_mcp add-in", MB_OK | MB_ICONINFORMATION);

	*IsConnected = VARIANT_TRUE;
	return S_OK;
}

STDMETHODIMP CSwAddin::DisconnectFromSW(VARIANT_BOOL* IsDisconnected)
{
	if (IsDisconnected == nullptr)
		return E_POINTER;

	m_swApp.Release();
	*IsDisconnected = VARIANT_TRUE;
	return S_OK;
}
