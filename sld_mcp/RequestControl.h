// Cancellation policy independent of gRPC, COM and the SolidWorks API.
#pragma once
#include "service/RequestLock.h"
#include <exception>
#include <functional>
#include <utility>

namespace sld_mcp::service
{
	enum class ExecutionPolicy { Cancellable, CompleteOperation };

	class RequestCancelled final : public std::exception
	{
	public:
		const char* what() const noexcept override { return "request cancelled or deadline expired"; }
	};

	class RequestControl
	{
	public:
		RequestControl(std::function<bool()> cancelled, ExecutionPolicy policy)
			: m_cancelled(std::move(cancelled)), m_policy(policy) {}

		// Always used for admission, including commands that must finish once started.
		bool IsCancelled() const { return m_cancelled(); }

		// Only inspections may unwind at a checkpoint. Editing commands keep their
		// lock and existing cleanup semantics until the complete operation returns.
		void Checkpoint() const
		{
			if (m_policy == ExecutionPolicy::Cancellable && IsCancelled())
				throw RequestCancelled();
		}

	private:
		std::function<bool()> m_cancelled;
		ExecutionPolicy m_policy;
	};

	// This is the serialized lifetime of actual work, independent of a client's
	// wait. It can be exercised with fake work without COM or a CAD application.
	class RequestGate
	{
	public:
		template <typename Work>
		auto Execute(RequestControl& control, Work work)
		{
			std::unique_lock<std::mutex> lock(m_mutex, std::defer_lock);
			if (!LockUnlessCancelled(lock, [&] { return control.IsCancelled(); }))
				throw RequestCancelled();
			auto result = work();
			control.Checkpoint();
			return result;
		}
	private:
		std::mutex m_mutex;
	};
}
