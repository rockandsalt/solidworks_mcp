// sld_mcp.cpp : Implementation of DLL Exports.


#include "pch.h"
#include "framework.h"
#include "resource.h"
#include "sldmcp_i.h"
#include "dllmain.h"


using namespace ATL;

namespace
{
	// CLSID of CSwMcpAddin, as a registry subkey name.
	const wchar_t kAddinClsidKey[] = L"SOFTWARE\\SolidWorks\\AddIns\\{c191de66-c755-4b06-928b-c39f69d1afc7}";

	// Registers/unregisters the add-in with SolidWorks, in addition to the
	// normal COM registration ATL already handles for the CLSID itself.
	void RegisterSwAddin()
	{
		CRegKey machineKey;
		if (machineKey.Create(HKEY_LOCAL_MACHINE, kAddinClsidKey) == ERROR_SUCCESS)
		{
			machineKey.SetStringValue(L"Description", L"sld_mcp SolidWorks Add-in");
			machineKey.SetStringValue(L"Title", L"sld_mcp");
		}

		CRegKey userKey;
		if (userKey.Create(HKEY_CURRENT_USER, kAddinClsidKey) == ERROR_SUCCESS)
		{
			userKey.SetDWORDValue(L"", 1);
		}
	}

	void UnregisterSwAddin()
	{
		CRegKey(HKEY_CURRENT_USER).DeleteSubKey(kAddinClsidKey);
		CRegKey(HKEY_LOCAL_MACHINE).DeleteSubKey(kAddinClsidKey);
	}
}

// Used to determine whether the DLL can be unloaded by OLE.
_Use_decl_annotations_
STDAPI DllCanUnloadNow(void)
{
	return _AtlModule.DllCanUnloadNow();
}

// Returns a class factory to create an object of the requested type.
_Use_decl_annotations_
STDAPI DllGetClassObject(_In_ REFCLSID rclsid, _In_ REFIID riid, _Outptr_ LPVOID* ppv)
{
	return _AtlModule.DllGetClassObject(rclsid, riid, ppv);
}

// DllRegisterServer - Adds entries to the system registry.
_Use_decl_annotations_
STDAPI DllRegisterServer(void)
{
	// registers object, typelib and all interfaces in typelib
	HRESULT hr = _AtlModule.DllRegisterServer();
	if (SUCCEEDED(hr))
		RegisterSwAddin();
	return hr;
}

// DllUnregisterServer - Removes entries from the system registry.
_Use_decl_annotations_
STDAPI DllUnregisterServer(void)
{
	UnregisterSwAddin();
	HRESULT hr = _AtlModule.DllUnregisterServer();
	return hr;
}

// DllInstall - Adds/Removes entries to the system registry per user per machine.
STDAPI DllInstall(BOOL bInstall, _In_opt_  LPCWSTR pszCmdLine)
{
	HRESULT hr = E_FAIL;
	static const wchar_t szUserSwitch[] = L"user";

	if (pszCmdLine != nullptr)
	{
		if (_wcsnicmp(pszCmdLine, szUserSwitch, _countof(szUserSwitch)) == 0)
		{
			ATL::AtlSetPerUserRegistration(true);
		}
	}

	if (bInstall)
	{
		hr = DllRegisterServer();
		if (FAILED(hr))
		{
			DllUnregisterServer();
		}
	}
	else
	{
		hr = DllUnregisterServer();
	}

	return hr;
}


