// SwAddin.h : Declaration of CSwAddin.
//
// This is the entry point SolidWorks talks to: it implements ISwAddin,
// the interface SolidWorks calls to load/unload a COM add-in. Everything
// else (the IPC bridge to Rust, the MCP server) hangs off of this.
#pragma once

#include "resource.h"

class ATL_NO_VTABLE CSwAddin :
	public ATL::CComObjectRootEx<ATL::CComSingleThreadModel>,
	public ATL::CComCoClass<CSwAddin, &CLSID_SwMcpAddin>,
	public ISwAddin
{
public:
	CSwAddin() = default;

	DECLARE_REGISTRY_RESOURCEID(IDR_SLDMCP)
	DECLARE_NOT_AGGREGATABLE(CSwAddin)
	DECLARE_PROTECT_FINAL_CONSTRUCT()

	BEGIN_COM_MAP(CSwAddin)
		COM_INTERFACE_ENTRY(ISwAddin)
	END_COM_MAP()

	// ISwAddin
	STDMETHOD(ConnectToSW)(IDispatch* ThisSW, long Cookie, VARIANT_BOOL* IsConnected);
	STDMETHOD(DisconnectFromSW)(VARIANT_BOOL* IsDisconnected);

private:
	ATL::CComPtr<ISldWorks> m_swApp;
	long m_addinCookie = 0;
};

OBJECT_ENTRY_AUTO(__uuidof(SwMcpAddin), CSwAddin)
