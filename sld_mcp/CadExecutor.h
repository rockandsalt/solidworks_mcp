// The single admission / COM / lifetime boundary for every CAD RPC.
#pragma once
#include "GrpcIncludes.h"
#include "RequestControl.h"
#include <functional>

namespace sld_mcp::service
{
	class CadExecutor
	{
	public:
		CadExecutor(IGlobalInterfaceTable* git, DWORD cookie) : m_git(git), m_cookie(cookie) {}
		using Work = std::function<grpc::Status(RequestControl&, ISldWorks*)>;

		// Synchronous by design: neither context nor captured request/response data
		// can outlive the RPC. A client timeout never releases an active CAD lock.
		grpc::Status Execute(grpc::ServerContext* context, const char* method,
			ExecutionPolicy policy, const Work& work);

	private:
		ATL::CComPtr<IGlobalInterfaceTable> m_git;
		DWORD m_cookie;
		RequestGate m_gate;
	};
}
