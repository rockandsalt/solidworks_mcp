// RpcServer.cpp : grpc::Server lifecycle.
#include "pch.h"
#include "RpcServer.h"
#include "SolidWorksService.h"
#include <chrono>
#include <stdexcept>
#include <string>
#include <thread>

namespace
{
	constexpr auto kShutdownGracePeriod = std::chrono::seconds(2);
}

RpcServer::RpcServer(int port, IGlobalInterfaceTable* git, DWORD swAppCookie)
	: m_service(std::make_unique<SolidWorksService>(git, swAppCookie))
{
	const std::string address = "127.0.0.1:" + std::to_string(port);

	grpc::ServerBuilder builder;
	// Never share the port with another listener (e.g. a second SolidWorks
	// instance): the client should reach exactly one add-in or fail.
	builder.AddChannelArgument(GRPC_ARG_ALLOW_REUSEPORT, 0);

	int boundPort = 0;
	builder.AddListeningPort(address, grpc::InsecureServerCredentials(), &boundPort);
	builder.RegisterService(m_service.get());

	m_server = builder.BuildAndStart();
	if (!m_server || boundPort == 0)
		throw std::runtime_error("couldn't listen on " + address + " (port already in use?)");
}

RpcServer::~RpcServer()
{
	Shutdown();
}

void RpcServer::Shutdown()
{
	if (!m_server)
		return;

	// An in-flight RPC can be blocked inside a COM call that is waiting for
	// this very thread (SolidWorks' main STA) to dispatch it. Server::Shutdown
	// blocks until handlers return, so calling it directly here would
	// deadlock. Instead, run it on a helper thread and keep dispatching COM
	// calls on this one until it's done.
	HANDLE done = CreateEventW(nullptr, TRUE, FALSE, nullptr);
	grpc::Server* server = m_server.get();
	std::thread shutdown([server, done]
		{
			server->Shutdown(std::chrono::system_clock::now() + kShutdownGracePeriod);
			SetEvent(done);
		});

	DWORD signaled = 0;
	CoWaitForMultipleHandles(0, INFINITE, 1, &done, &signaled);

	shutdown.join();
	CloseHandle(done);
	m_server.reset();
	m_service.reset();
}
