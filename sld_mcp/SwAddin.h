// SwAddin.h : Declaration of CSwAddin.
//
// This is the entry point SolidWorks talks to: it implements ISwAddin,
// the interface SolidWorks calls to load/unload a COM add-in. Everything
// else (the gRPC server the Rust MCP server connects to) hangs off of this.
#pragma once

#include "resource.h"
#include <memory>

class RpcServer;

class ATL_NO_VTABLE CSwAddin :
	public ATL::CComObjectRootEx<ATL::CComSingleThreadModel>,
	public ATL::CComCoClass<CSwAddin, &CLSID_SwMcpAddin>,
	public ISwAddin
{
public:
	CSwAddin() = default;
	~CSwAddin();

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
	void ReleaseSolidWorks();

	ATL::CComPtr<ISldWorks> m_swApp;
	long m_addinCookie = 0;

	// m_swApp lives on SolidWorks' main STA thread, but gRPC calls arrive on
	// its own worker threads, which can't call m_swApp directly without
	// violating COM apartment-threading rules. Registering it in the Global
	// Interface Table lets those threads fetch a COM-marshaled proxy instead,
	// with calls forwarded to the main thread's message loop automatically.
	ATL::CComPtr<IGlobalInterfaceTable> m_git;
	DWORD m_swAppGitCookie = 0;

	std::unique_ptr<RpcServer> m_rpcServer;
};

OBJECT_ENTRY_AUTO(__uuidof(SwMcpAddin), CSwAddin)
