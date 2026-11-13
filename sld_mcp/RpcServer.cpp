// RpcServer.cpp : grpc::Server lifecycle.
#include "pch.h"
#include "RpcServer.h"
#include "SolidWorksService.h"
#include <chrono>
#include <stdexcept>
#include <string>
#include <thread>
#include <utility>

namespace
{
	constexpr auto kShutdownGracePeriod = std::chrono::seconds(2);

	// Must match TOKEN_HEADER in mcp_server/src/addin.rs.
	constexpr char kTokenHeader[] = "x-sld-mcp-token";

	// Refuses calls that don't carry the token from the instance file, so
	// only someone who can read the user's %LOCALAPPDATA% drives SolidWorks,
	// and a client reading a stale file can't end up talking to whatever
	// took its port. Runs before any handler.
	class TokenCheck final : public grpc::AuthMetadataProcessor
	{
	public:
		explicit TokenCheck(std::string token) : m_token(std::move(token)) {}

		// A string comparison: no need for gRPC's blocking thread pool.
		bool IsBlocking() const override { return false; }

		grpc::Status Process(const InputMetadata& authMetadata, grpc::AuthContext* /*context*/,
			OutputMetadata* consumedAuthMetadata, OutputMetadata* /*responseMetadata*/) override
		{
			auto it = authMetadata.find(kTokenHeader);
			if (it == authMetadata.end())
				return grpc::Status(grpc::StatusCode::UNAUTHENTICATED,
					"no connection token (x-sld-mcp-token); read it from the sld_mcp instance file");

			const std::string token(it->second.data(), it->second.size());
			if (!SameToken(token))
				return grpc::Status(grpc::StatusCode::UNAUTHENTICATED,
					"wrong connection token: the sld_mcp instance file it came from is out of date");

			consumedAuthMetadata->emplace(kTokenHeader, token);
			return grpc::Status::OK;
		}

	private:
		// Without an early exit, so the time taken says nothing about the token.
		bool SameToken(const std::string& token) const
		{
			if (token.size() != m_token.size())
				return false;
			unsigned char diff = 0;
			for (size_t i = 0; i < token.size(); ++i)
				diff |= static_cast<unsigned char>(token[i] ^ m_token[i]);
			return diff == 0;
		}

		const std::string m_token;
	};
}

RpcServer::RpcServer(int port, const std::string& token, IGlobalInterfaceTable* git, DWORD swAppCookie,
	MainThread* mainThread)
	: m_service(std::make_unique<SolidWorksService>(git, swAppCookie, mainThread))
{
	const std::string address = "127.0.0.1:" + std::to_string(port);

	grpc::ServerBuilder builder;
	// Never share the port with another listener (e.g. a second SolidWorks
	// instance): the client should reach exactly one add-in or fail.
	builder.AddChannelArgument(GRPC_ARG_ALLOW_REUSEPORT, 0);

	// Local credentials: plaintext HTTP/2 like insecure ones (no handshake
	// bytes, so tonic's plain channel connects), but they refuse non-loopback
	// peers and, unlike insecure ones, accept an AuthMetadataProcessor
	// (InsecureServerCredentials crashes on SetAuthMetadataProcessor).
	std::shared_ptr<grpc::ServerCredentials> credentials = grpc::experimental::LocalServerCredentials(LOCAL_TCP);
	credentials->SetAuthMetadataProcessor(std::make_shared<TokenCheck>(token));

	builder.AddListeningPort(address, credentials, &m_port);
	builder.RegisterService(m_service.get());

	m_server = builder.BuildAndStart();
	if (!m_server || m_port == 0)
		throw std::runtime_error("couldn't listen on " + address + " (port already in use or reserved?)");
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
