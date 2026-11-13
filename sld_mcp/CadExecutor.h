// The single admission / COM / lifetime boundary for every CAD RPC.
#pragma once
#include "GrpcIncludes.h"
#include "RequestControl.h"
#include "MainThread.h"
#include <functional>

namespace sld_mcp::service
{
	class CadExecutor
	{
	public:
		// With `mainThread` the work runs on SolidWorks' main thread (direct COM
		// calls); without it, on the gRPC worker (each call marshaled there).
		CadExecutor(IGlobalInterfaceTable* git, DWORD cookie, MainThread* mainThread = nullptr)
			: m_git(git), m_cookie(cookie), m_mainThread(mainThread) {}
		using Work = std::function<grpc::Status(RequestControl&, ISldWorks*)>;

		// Synchronous by design: neither context nor captured request/response data
		// can outlive the RPC. A client timeout never releases an active CAD lock.
		grpc::Status Execute(grpc::ServerContext* context, const char* method,
			ExecutionPolicy policy, const Work& work);

	private:
		ATL::CComPtr<IGlobalInterfaceTable> m_git;
		DWORD m_cookie;
		MainThread* m_mainThread;
		RequestGate m_gate;
	};
}
