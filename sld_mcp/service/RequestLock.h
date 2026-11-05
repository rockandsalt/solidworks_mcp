// RequestLock.h : cancellable waiting for the shared model lock, without COM.
#pragma once

#include <chrono>
#include <mutex>
#include <thread>

namespace sld_mcp::service
{
	// Cancellation after acquisition still leaves ownership with the caller's
	// RAII lock, which releases it when the handler returns.
	template <typename IsCancelled>
	bool LockUnlessCancelled(std::unique_lock<std::mutex>& lock, IsCancelled cancelled)
	{
		while (!cancelled())
		{
			if (lock.try_lock())
				return !cancelled();
			std::this_thread::sleep_for(std::chrono::milliseconds(10));
		}
		return false;
	}
}
