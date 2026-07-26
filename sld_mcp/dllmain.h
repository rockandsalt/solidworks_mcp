// dllmain.h : Declaration of module class.

class CsldmcpModule : public ATL::CAtlDllModuleT< CsldmcpModule >
{
public :
	DECLARE_LIBID(LIBID_sldmcpLib)
	DECLARE_REGISTRY_APPID_RESOURCEID(IDR_SLDMCP, "{f846a1ae-dce6-483c-ac89-3ff0afd723b6}")
};

extern class CsldmcpModule _AtlModule;
