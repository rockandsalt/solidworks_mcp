// sld_mcp.cpp : Implementation of DLL Exports.


#include "pch.h"
#include "framework.h"
#include "resource.h"
#include "sldmcp_i.h"
#include "dllmain.h"


using namespace ATL;

namespace
{
	// SolidWorks lists add-ins found under HKLM\SOFTWARE\SolidWorks\Addins\{CLSID}
	// (so registering needs admin rights, like the COM registration itself),
	// and loads one at startup if HKCU\Software\SolidWorks\AddInsStartup\{CLSID}
	// is 1.
	const wchar_t kAddinKey[] = L"SOFTWARE\\SolidWorks\\Addins\\{c191de66-c755-4b06-928b-c39f69d1afc7}";
	const wchar_t kAddinStartupKey[] = L"Software\\SolidWorks\\AddInsStartup\\{c191de66-c755-4b06-928b-c39f69d1afc7}";

	// Written by earlier versions of this DLL in place of kAddinStartupKey,
	// which is where SolidWorks actually reads the startup flag. Cleaned up
	// on unregister.
	const wchar_t kLegacyUserKey[] = L"SOFTWARE\\SolidWorks\\AddIns\\{c191de66-c755-4b06-928b-c39f69d1afc7}";

	HRESULT RegisterSwAddin()
	{
		CRegKey machineKey;
		LONG rc = machineKey.Create(HKEY_LOCAL_MACHINE, kAddinKey);
		if (rc == ERROR_SUCCESS)
			rc = machineKey.SetDWORDValue(nullptr, 0);
		if (rc == ERROR_SUCCESS)
			rc = machineKey.SetStringValue(L"Description", L"sld_mcp SolidWorks Add-in");
		if (rc == ERROR_SUCCESS)
			rc = machineKey.SetStringValue(L"Title", L"sld_mcp");
		if (rc != ERROR_SUCCESS)
			return HRESULT_FROM_WIN32(rc);

		CRegKey userKey;
		rc = userKey.Create(HKEY_CURRENT_USER, kAddinStartupKey);
		if (rc == ERROR_SUCCESS)
			rc = userKey.SetDWORDValue(nullptr, 1);
		return HRESULT_FROM_WIN32(rc);
	}

	void UnregisterSwAddin()
	{
		RegDeleteKeyW(HKEY_CURRENT_USER, kAddinStartupKey);
		RegDeleteKeyW(HKEY_CURRENT_USER, kLegacyUserKey);
		RegDeleteKeyW(HKEY_LOCAL_MACHINE, kAddinKey);
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
		hr = RegisterSwAddin();
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


