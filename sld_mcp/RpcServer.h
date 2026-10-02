// RpcServer.h : owns the grpc::Server hosted inside SolidWorks.
//
// Deliberately free of grpc++ headers so SwAddin.cpp doesn't have to include
// them (see GrpcIncludes.h for why that matters).
#pragma once

#include <memory>

namespace grpc
{
	class Server;
	class Service;
}

class RpcServer
{
public:
	// Starts serving the SolidWorks service on 127.0.0.1:`port` (loopback
	// only). `git` / `swAppCookie` identify the ISldWorks the RPC handlers
	// use; both must outlive this object. Throws std::runtime_error if the
	// port can't be bound -- typically because another SolidWorks instance
	// already has it.
	RpcServer(int port, IGlobalInterfaceTable* git, DWORD swAppCookie);
	~RpcServer();

	RpcServer(const RpcServer&) = delete;
	RpcServer& operator=(const RpcServer&) = delete;

	// Must be called on SolidWorks' main (STA) thread. Waits for in-flight
	// RPCs to finish, cancelling any still running after a short grace
	// period. Safe to call more than once; the destructor calls it too.
	void Shutdown();

private:
	std::unique_ptr<grpc::Service> m_service;
	std::unique_ptr<grpc::Server> m_server;
};
