// Exercises the production GetMesh handler over gRPC, without a running CAD app.
#include "pch.h"
#include "SolidWorksService.h"
#include "MainThread.h"
#include "service/Common.h"
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <future>
#include <iostream>
#include <mutex>
#include <stdexcept>
#include <thread>
#include <vector>

using namespace std::chrono_literals;

namespace
{
	using namespace sld_mcp::service;

	bool TestGateLifecycle(ExecutionPolicy policy)
	{
		RequestGate gate;
		std::atomic<bool> cancelled{ false };
		RequestControl control([&] { return cancelled.load(); }, policy);
		std::promise<void> entered, release;
		auto ready = entered.get_future();
		auto resume = release.get_future();
		bool cleaned = false, finished = false;
		struct Cleanup { bool& done; ~Cleanup() { done = true; } };
		auto running = std::async(std::launch::async, [&] {
			try
			{
				gate.Execute(control, [&] {
					Cleanup cleanup{ cleaned };
					entered.set_value();
					resume.wait(); // stand-in for an uninterruptible COM call
					control.Checkpoint();
					finished = true;
					return 7;
				});
				return false;
			}
			catch (const RequestCancelled&) { return true; }
		});
		const bool started = ready.wait_for(2s) == std::future_status::ready;
		cancelled = true;
		RequestControl fresh([] { return false; }, ExecutionPolicy::Cancellable);
		auto next = std::async(std::launch::async, [&] {
			return gate.Execute(fresh, [&] { return cleaned; });
		});
		// Expiry must not release a running operation's lock before its cleanup.
		const bool held = next.wait_for(50ms) != std::future_status::ready;
		release.set_value();
		const bool stopped = running.get();
		const bool recovered = next.get();
		const bool read = policy == ExecutionPolicy::Cancellable;
		return started && held && recovered && stopped == read && finished != read;
	}

	bool TestGateAdmissionAndExceptions()
	{
		RequestGate gate;
		// An already-expired edit cannot start, even though an admitted edit
		// would be allowed to finish. Both policy types share admission rules.
		RequestControl expired([] { return true; }, ExecutionPolicy::CompleteOperation);
		bool ran = false, rejected = false;
		try { gate.Execute(expired, [&] { ran = true; return 0; }); }
		catch (const RequestCancelled&) { rejected = true; }
		if (!rejected || ran)
			return false;
		RequestControl active([] { return false; }, ExecutionPolicy::CompleteOperation);
		bool cleaned = false;
		struct Cleanup { bool& done; ~Cleanup() { done = true; } };
		try
		{
			gate.Execute(active, [&]() -> int {
				Cleanup cleanup{ cleaned };
				throw std::runtime_error("fake CAD failure");
			});
		}
		catch (const std::runtime_error&) {}
		return gate.Execute(active, [&] { return cleaned; });
	}

	// RPCs that read unless asked to change: only the plain read may stop early.
	bool TestReadOrChangePolicies()
	{
		constexpr auto read = ExecutionPolicy::Cancellable;
		constexpr auto change = ExecutionPolicy::CompleteOperation;

		sld_mcp::v1::SelectionRequest selection;
		bool passed = PolicyFor(selection) == read;
		selection.set_append(true);
		passed = passed && PolicyFor(selection) == read;
		selection.add_select()->set_feature_name("Boss-Extrude1");
		passed = passed && PolicyFor(selection) == change;
		sld_mcp::v1::SelectionRequest clear;
		clear.set_clear(true);
		passed = passed && PolicyFor(clear) == change;

		sld_mcp::v1::ConfigurationsRequest configurations;
		passed = passed && PolicyFor(configurations) == read;
		configurations.set_activate("Default");
		passed = passed && PolicyFor(configurations) == change;
		configurations.set_delete_("Other");
		passed = passed && PolicyFor(configurations) == change;
		configurations.mutable_create();
		passed = passed && PolicyFor(configurations) == change;

		sld_mcp::v1::EquationsRequest equations;
		passed = passed && PolicyFor(equations) == read;
		equations.add_delete_("Width");
		passed = passed && PolicyFor(equations) == change;
		sld_mcp::v1::EquationsRequest set;
		set.add_set()->set_name("Width");
		passed = passed && PolicyFor(set) == change;
		return passed;
	}

	// Blocks the first connection lookup while GetMesh owns the model mutex.
	// Failing the lookup keeps every request away from any real SolidWorks API.
	class BlockingGit final : public IGlobalInterfaceTable
	{
	public:
		HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** out) override
		{
			*out = nullptr;
			if (iid != IID_IUnknown && iid != IID_IGlobalInterfaceTable)
				return E_NOINTERFACE;
			*out = static_cast<IGlobalInterfaceTable*>(this);
			AddRef();
			return S_OK;
		}
		ULONG STDMETHODCALLTYPE AddRef() override { return ++m_refs; }
		ULONG STDMETHODCALLTYPE Release() override { return --m_refs; }
		HRESULT STDMETHODCALLTYPE RegisterInterfaceInGlobal(IUnknown*, REFIID, DWORD*) override { return E_NOTIMPL; }
		HRESULT STDMETHODCALLTYPE RevokeInterfaceFromGlobal(DWORD) override { return E_NOTIMPL; }
		HRESULT STDMETHODCALLTYPE GetInterfaceFromGlobal(DWORD, REFIID, void** out) override
		{
			*out = nullptr;
			if (++calls == 1)
			{
				entered.set_value();
				std::unique_lock<std::mutex> lock(m_mutex);
				m_release.wait_for(lock, 5s, [&] { return m_released; });
			}
			return E_FAIL;
		}
		void Unblock()
		{
			std::lock_guard<std::mutex> lock(m_mutex);
			m_released = true;
			m_release.notify_all();
		}

		std::atomic<int> calls{ 0 };
		std::promise<void> entered;

	private:
		std::atomic<ULONG> m_refs{ 1 };
		std::mutex m_mutex;
		std::condition_variable m_release;
		bool m_released = false;
	};

	// This thread stands in for SolidWorks' main thread: it dispatches its
	// window messages until `done` is ready or `limit` passes.
	template <typename Future>
	bool PumpUntil(Future& done, std::chrono::milliseconds limit)
	{
		const auto end = std::chrono::steady_clock::now() + limit;
		while (done.wait_for(0ms) != std::future_status::ready)
		{
			if (std::chrono::steady_clock::now() >= end)
				return false;
			MsgWaitForMultipleObjects(0, nullptr, FALSE, 10, QS_ALLINPUT);
			MSG message;
			while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE))
				DispatchMessageW(&message);
		}
		return true;
	}

	void Pump(std::chrono::milliseconds duration)
	{
		std::promise<void> never;
		auto future = never.get_future();
		PumpUntil(future, duration);
	}

	bool TestMainThread()
	{
		using Outcome = MainThread::Outcome;
		MainThread main;
		const DWORD owner = GetCurrentThreadId();
		auto never = [] { return false; };

		// Work runs on the owning thread; an exception it throws reaches the caller.
		auto ran = std::async(std::launch::async, [&] {
			DWORD ranOn = 0;
			const bool done = main.Run([&] { ranOn = GetCurrentThreadId(); }, never) == Outcome::Done;
			bool threw = false;
			try { main.Run([] { throw std::runtime_error("fake CAD failure"); }, never); }
			catch (const std::runtime_error&) { threw = true; }
			return done && ranOn == owner && threw;
		});
		if (!PumpUntil(ran, 2s) || !ran.get())
			return false;

		// Cancelled while queued (the owner isn't pumping): it never runs, not
		// even once the owner pumps again.
		bool late = false;
		std::atomic<bool> cancel{ false };
		auto queued = std::async(std::launch::async, [&] {
			return main.Run([&] { late = true; }, [&] { return cancel.load(); });
		});
		std::this_thread::sleep_for(100ms);
		cancel = true;
		if (queued.wait_for(1s) != std::future_status::ready || queued.get() != Outcome::NotStarted)
			return false;
		Pump(100ms);
		if (late)
			return false;

		// Work queued while other work runs and pumps messages (as SolidWorks does
		// inside some calls) waits for it instead of running nested.
		std::vector<int> order;
		std::future<Outcome> inner;
		auto outer = std::async(std::launch::async, [&] {
			return main.Run([&] {
				order.push_back(1);
				inner = std::async(std::launch::async, [&] { return main.Run([&] { order.push_back(3); }, never); });
				Pump(200ms);
				order.push_back(2);
			}, never);
		});
		if (!PumpUntil(outer, 2s) || outer.get() != Outcome::Done || !inner.valid() ||
			!PumpUntil(inner, 2s) || inner.get() != Outcome::Done || order != std::vector<int>{ 1, 2, 3 })
			return false;

		// Close: queued work is let go without running, new work is refused.
		bool closedRan = false;
		auto pending = std::async(std::launch::async, [&] { return main.Run([&] { closedRan = true; }, never); });
		std::this_thread::sleep_for(100ms);
		main.Close();
		if (pending.wait_for(1s) != std::future_status::ready || pending.get() != Outcome::Closed)
			return false;
		auto refused = std::async(std::launch::async, [&] { return main.Run([] {}, never); });
		if (refused.wait_for(1s) != std::future_status::ready || refused.get() != Outcome::Closed)
			return false;
		Pump(100ms);
		return !closedRan;
	}

	// Records which thread looks the connection up; failing it keeps the
	// handler away from any real SolidWorks API.
	class ThreadGit final : public IGlobalInterfaceTable
	{
	public:
		HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** out) override
		{
			*out = nullptr;
			if (iid != IID_IUnknown && iid != IID_IGlobalInterfaceTable)
				return E_NOINTERFACE;
			*out = static_cast<IGlobalInterfaceTable*>(this);
			return S_OK;
		}
		ULONG STDMETHODCALLTYPE AddRef() override { return 2; }
		ULONG STDMETHODCALLTYPE Release() override { return 1; }
		HRESULT STDMETHODCALLTYPE RegisterInterfaceInGlobal(IUnknown*, REFIID, DWORD*) override { return E_NOTIMPL; }
		HRESULT STDMETHODCALLTYPE RevokeInterfaceFromGlobal(DWORD) override { return E_NOTIMPL; }
		HRESULT STDMETHODCALLTYPE GetInterfaceFromGlobal(DWORD, REFIID, void** out) override
		{
			*out = nullptr;
			thread = GetCurrentThreadId();
			++calls;
			return E_FAIL;
		}

		std::atomic<int> calls{ 0 };
		std::atomic<DWORD> thread{ 0 };
	};

	// The production executor with a main thread: an RPC that expires while
	// queued never connects; a fresh one connects on the main thread.
	bool TestExecutorOnMainThread()
	{
		MainThread main;
		ThreadGit git;
		SolidWorksService service(&git, 1, &main);
		grpc::ServerBuilder builder;
		int port = 0;
		builder.AddListeningPort("127.0.0.1:0", grpc::InsecureServerCredentials(), &port);
		builder.RegisterService(&service);
		auto server = builder.BuildAndStart();
		if (!server || !port)
			return false;
		auto channel = grpc::CreateChannel("127.0.0.1:" + std::to_string(port), grpc::InsecureChannelCredentials());
		auto call = [channel](std::chrono::milliseconds timeout) {
			return std::async(std::launch::async, [channel, timeout] {
				auto stub = sld_mcp::v1::SolidWorks::NewStub(channel);
				grpc::ClientContext context;
				context.set_deadline(std::chrono::system_clock::now() + timeout);
				sld_mcp::v1::GetActiveDocumentRequest request;
				sld_mcp::v1::GetActiveDocumentResponse response;
				return stub->GetActiveDocument(&context, request, &response).error_code();
			});
		};

		auto expired = call(300ms);
		bool passed = expired.wait_for(2s) == std::future_status::ready;
		if (passed)
		{
			const auto code = expired.get();
			passed = code == grpc::StatusCode::DEADLINE_EXCEEDED || code == grpc::StatusCode::CANCELLED;
		}
		// Let the server side give up on it (it polls every 50 ms), then pump:
		// the stale wake must not start it.
		std::this_thread::sleep_for(200ms);
		Pump(100ms);
		passed = passed && git.calls == 0;

		auto fresh = call(5s);
		passed = PumpUntil(fresh, 5s) && fresh.get() == grpc::StatusCode::UNAVAILABLE && passed;
		passed = passed && git.calls == 1 && git.thread == GetCurrentThreadId();
		server->Shutdown();
		server->Wait();
		return passed;
	}

	class ObservedService final : public sld_mcp::v1::SolidWorks::Service
	{
	public:
		explicit ObservedService(IGlobalInterfaceTable* git) : m_inner(git, 1) {}

		grpc::Status GetMesh(grpc::ServerContext* context, const sld_mcp::v1::GetMeshRequest* request,
			sld_mcp::v1::GetMeshResponse* response) override
		{
			const bool queued = request->cached_key() == 2;
			if (queued)
				queuedEntered.set_value();
			grpc::Status status = m_inner.GetMesh(context, request, response);
			if (queued)
				queuedReturned.set_value(status.error_code());
			return status;
		}

		std::promise<void> queuedEntered;
		std::promise<grpc::StatusCode> queuedReturned;
		std::promise<grpc::StatusCode> macroReturned, documentReturned;

		grpc::Status RunMacro(grpc::ServerContext* context, const sld_mcp::v1::RunMacroRequest* request,
			sld_mcp::v1::RunMacroResponse* response) override
		{
			auto status = m_inner.RunMacro(context, request, response);
			macroReturned.set_value(status.error_code());
			return status;
		}
		grpc::Status GetActiveDocument(grpc::ServerContext* context,
			const sld_mcp::v1::GetActiveDocumentRequest* request, sld_mcp::v1::GetActiveDocumentResponse* response) override
		{
			auto status = m_inner.GetActiveDocument(context, request, response);
			documentReturned.set_value(status.error_code());
			return status;
		}

	private:
		SolidWorksService m_inner;
	};
}

bool TestPmiPreferences();

int main()
{
	if (!TestPmiPreferences())
		return 1;
	if (!TestGateAdmissionAndExceptions() || !TestGateLifecycle(ExecutionPolicy::Cancellable) ||
		!TestGateLifecycle(ExecutionPolicy::CompleteOperation))
	{
		std::cerr << "FAIL: shared request admission, cancellation, edit completion or cleanup\n";
		return 1;
	}
	std::cout << "PASS: shared admission, running inspection cancellation, edit completion and exception cleanup\n";
	if (!TestReadOrChangePolicies())
	{
		std::cerr << "FAIL: a selection, configurations or equations request that changes something could stop early, or a read couldn't\n";
		return 1;
	}
	std::cout << "PASS: selection, configurations and equations reads are cancellable, their changes complete\n";
	if (!TestMainThread())
	{
		std::cerr << "FAIL: main-thread work: owning thread, exceptions, cancelled while queued, no nesting or Close\n";
		return 1;
	}
	if (!TestExecutorOnMainThread())
	{
		std::cerr << "FAIL: executor on the main thread: an expired queued RPC connected, or a fresh one didn't on the main thread\n";
		return 1;
	}
	std::cout << "PASS: main-thread work runs on its thread, never after expiring in the queue, never nested\n";
	BlockingGit git;
	ObservedService service(&git);
	grpc::ServerBuilder builder;
	int port = 0;
	builder.AddListeningPort("127.0.0.1:0", grpc::InsecureServerCredentials(), &port);
	builder.RegisterService(&service);
	auto server = builder.BuildAndStart();
	if (!server || !port)
		return 1;
	auto channel = grpc::CreateChannel("127.0.0.1:" + std::to_string(port), grpc::InsecureChannelCredentials());
	auto call = [&](uint64_t key, std::chrono::milliseconds timeout) {
		auto stub = sld_mcp::v1::SolidWorks::NewStub(channel);
		grpc::ClientContext context;
		context.set_deadline(std::chrono::system_clock::now() + timeout);
		sld_mcp::v1::GetMeshRequest request;
		request.set_cached_key(key);
		sld_mcp::v1::GetMeshResponse response;
		return stub->GetMesh(&context, request, &response);
	};

	auto holderEntered = git.entered.get_future();
	auto queuedEntered = service.queuedEntered.get_future();
	auto queuedReturned = service.queuedReturned.get_future();
	auto macroReturned = service.macroReturned.get_future();
	auto documentReturned = service.documentReturned.get_future();
	auto holder = std::async(std::launch::async, [&] { return call(1, 10s); });
	bool passed = holderEntered.wait_for(2s) == std::future_status::ready;
	if (passed)
	{
		const auto queued = call(2, 300ms);
		passed = queued.error_code() == grpc::StatusCode::DEADLINE_EXCEEDED ||
			queued.error_code() == grpc::StatusCode::CANCELLED;
		passed = passed && queuedEntered.wait_for(1s) == std::future_status::ready;
		// Observe SERVER completion while the holder still owns the mutex.
		// A client-side timeout by itself would pass even with the old bug.
		const bool returned = queuedReturned.wait_for(1s) == std::future_status::ready;
		passed = passed && returned;
		if (returned)
			passed = passed && queuedReturned.get() == grpc::StatusCode::CANCELLED;
		passed = passed && git.calls == 1;

		// Commands and basic document reads must use the same admission gate.
		// These must return on the server while the original call still owns it.
		auto stub = sld_mcp::v1::SolidWorks::NewStub(channel);
		grpc::ClientContext macroContext;
		macroContext.set_deadline(std::chrono::system_clock::now() + 300ms);
		sld_mcp::v1::RunMacroRequest macroRequest;
		macroRequest.set_path("must-not-run.swb");
		sld_mcp::v1::RunMacroResponse macroResponse;
		stub->RunMacro(&macroContext, macroRequest, &macroResponse);
		const bool macroStopped = macroReturned.wait_for(1s) == std::future_status::ready;
		passed = passed && macroStopped;
		if (macroStopped)
			passed = passed && macroReturned.get() == grpc::StatusCode::CANCELLED;
		grpc::ClientContext documentContext;
		documentContext.set_deadline(std::chrono::system_clock::now() + 300ms);
		sld_mcp::v1::GetActiveDocumentRequest documentRequest;
		sld_mcp::v1::GetActiveDocumentResponse documentResponse;
		stub->GetActiveDocument(&documentContext, documentRequest, &documentResponse);
		const bool documentStopped = documentReturned.wait_for(1s) == std::future_status::ready;
		passed = passed && documentStopped;
		if (documentStopped)
			passed = passed && documentReturned.get() == grpc::StatusCode::CANCELLED;
		passed = passed && git.calls == 1;
	}
	git.Unblock();
	passed = holder.get().error_code() == grpc::StatusCode::UNAVAILABLE && passed;
	// Positive control: a fresh request acquires the released mutex and reaches
	// the fake GIT. No expired request may perform a late connection lookup.
	passed = call(3, 2s).error_code() == grpc::StatusCode::UNAVAILABLE && passed;
	server->Shutdown();
	server->Wait();
	passed = passed && git.calls == 2;
	std::cout << (passed ? "PASS" : "FAIL")
		<< ": expired mesh, macro and document RPCs exit before lock release; fresh request proceeds; connection lookups="
		<< git.calls << '\n';
	return passed ? 0 : 1;
}
