// RpcServer.h : owns the grpc::Server hosted inside SolidWorks.
//
// Deliberately free of grpc++ headers so SwAddin.cpp doesn't have to include
// them (see GrpcIncludes.h for why that matters).
#pragma once

#include <memory>
#include <string>

namespace grpc
{
	class Server;
	class Service;
}

class RpcServer
{
public:
	// Starts serving the SolidWorks service on 127.0.0.1:`port` (loopback
	// only; 0 = any free port, see Port()). Calls without `token` in their
	// x-sld-mcp-token metadata are refused. `git` / `swAppCookie` identify
	// the ISldWorks the RPC handlers use; both must outlive this object.
	// Throws std::runtime_error if the port can't be bound.
	RpcServer(int port, const std::string& token, IGlobalInterfaceTable* git, DWORD swAppCookie);
	~RpcServer();

	RpcServer(const RpcServer&) = delete;
	RpcServer& operator=(const RpcServer&) = delete;

	// Must be called on SolidWorks' main (STA) thread. Waits for in-flight
	// RPCs to finish, cancelling any still running after a short grace
	// period. Safe to call more than once; the destructor calls it too.
	void Shutdown();

	// The port it listens on.
	int Port() const { return m_port; }

private:
	int m_port = 0;
	std::unique_ptr<grpc::Service> m_service;
	std::unique_ptr<grpc::Server> m_server;
};
