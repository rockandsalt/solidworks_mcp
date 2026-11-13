#include "pch.h"
#include "CadExecutor.h"
#include <atomic>
#include <chrono>
#include <cstdio>

using namespace sld_mcp::service;

namespace
{
	class ComApartment
	{
	public:
		ComApartment() : m_hr(CoInitializeEx(nullptr, COINIT_MULTITHREADED)) {}
		~ComApartment() { if (SUCCEEDED(m_hr)) CoUninitialize(); }
		bool Ok() const { return SUCCEEDED(m_hr) || m_hr == RPC_E_CHANGED_MODE; }
	private:
		HRESULT m_hr;
	};

	// Debugger/DebugView diagnostics contain no CAD contents or connection token.
	class RequestTrace
	{
	public:
		explicit RequestTrace(const char* method) : m_method(method), m_id(++s_next) {}
		void Write(const char* phase, int code = 0, bool expired = false) const
		{
			const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
				std::chrono::steady_clock::now() - m_started).count();
			char text[384];
			std::snprintf(text, sizeof(text),
				"sld_mcp pid=%lu request=%llu method=%s phase=%s elapsed_ms=%lld status=%d expired=%d\n",
				GetCurrentProcessId(), m_id, m_method, phase, elapsed, code, expired);
			OutputDebugStringA(text);
		}
	private:
		const char* m_method;
		unsigned long long m_id;
		std::chrono::steady_clock::time_point m_started = std::chrono::steady_clock::now();
		inline static std::atomic<unsigned long long> s_next{ 0 };
	};
}

grpc::Status CadExecutor::Execute(grpc::ServerContext* context, const char* method,
	ExecutionPolicy policy, const Work& work)
{
	RequestControl control([context] {
		return context->IsCancelled() || std::chrono::system_clock::now() >= context->deadline();
	}, policy);
	RequestTrace trace(method);
	trace.Write("waiting");
	bool started = false;

	// Keep the lock through stack unwinding, COM releases and apartment teardown.
	// Never cancel an editing command by abandoning its worker or unlocking early.
	// On the main thread the GIT hands out SolidWorks' own ISldWorks, so every
	// COM object the work touches is created and released there, with direct calls.
	auto connectAndWork = [&]() -> grpc::Status {
		trace.Write("connecting");
		ATL::CComPtr<ISldWorks> swApp;
		const HRESULT hr = m_git->GetInterfaceFromGlobal(m_cookie, __uuidof(ISldWorks),
			reinterpret_cast<void**>(&swApp));
		if (FAILED(hr) || !swApp)
			return grpc::Status(grpc::StatusCode::UNAVAILABLE, "lost connection to SolidWorks");
		if (control.IsCancelled())
			return grpc::Status(grpc::StatusCode::CANCELLED, "request expired before CAD work started");
		trace.Write("executing");
		started = true;
		grpc::Status result = work(control, swApp);
		// A partial read must never be published as successful after expiry.
		control.Checkpoint();
		return result;
	};
	auto execute = [&]() -> grpc::Status {
		if (!m_mainThread)
		{
			ComApartment com;
			if (!com.Ok())
				return grpc::Status(grpc::StatusCode::INTERNAL, "CoInitializeEx failed on gRPC worker thread");
			if (control.IsCancelled())
				return grpc::Status(grpc::StatusCode::CANCELLED, "request expired before CAD work started");
			return connectAndWork();
		}
		if (control.IsCancelled())
			return grpc::Status(grpc::StatusCode::CANCELLED, "request expired before CAD work started");
		trace.Write("queued");
		grpc::Status status;
		switch (m_mainThread->Run([&] { status = connectAndWork(); }, [&] { return control.IsCancelled(); }))
		{
		case MainThread::Outcome::Done:
			return status;
		case MainThread::Outcome::NotStarted:
			return grpc::Status(grpc::StatusCode::CANCELLED, "request expired before CAD work started");
		default:
			return grpc::Status(grpc::StatusCode::UNAVAILABLE, "the SolidWorks add-in is shutting down");
		}
	};
	grpc::Status result;
	try { result = m_gate.Execute(control, execute); }
	catch (const RequestCancelled&)
	{
		result = grpc::Status(grpc::StatusCode::CANCELLED, started
			? "request cancelled or deadline expired; stopped at a safe CAD checkpoint"
			: "request cancelled or deadline expired while waiting for CAD access; work not started");
	}
	catch (const std::exception&)
	{
		result = grpc::Status(grpc::StatusCode::INTERNAL, "CAD request failed with an unexpected exception");
	}
	trace.Write("finished", result.error_code(), control.IsCancelled());
	return result;
}
