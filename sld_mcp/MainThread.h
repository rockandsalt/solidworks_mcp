// MainThread.h : runs work on SolidWorks' main (STA) thread.
//
// COM calls made from a gRPC worker are marshaled to the main thread one by
// one, about 1-12 ms each (architecture.md, "COM marshaling cost"). Posting
// the whole piece of work there once makes its calls direct. Free of grpc++
// and of the SolidWorks API, so it can be tested on its own.
#pragma once

#include <atomic>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>

class MainThread
{
public:
	// Must be constructed and destroyed on the thread that runs the work, which
	// must dispatch window messages (SolidWorks' main thread does). Throws
	// std::runtime_error if its window can't be created.
	MainThread();
	~MainThread();

	MainThread(const MainThread&) = delete;
	MainThread& operator=(const MainThread&) = delete;

	enum class Outcome
	{
		Done,       // `work` ran; an exception it threw is rethrown by Run
		NotStarted, // `cancelled` returned true while `work` was still queued
		Closed,     // Close() was called before `work` started
	};

	// From any other thread: queues `work`, then waits until it has run, unless
	// `cancelled` (polled while waiting) becomes true or Close() is called
	// before it starts. Once started it always runs to the end. Work queued
	// while other work is running (the main thread can pump messages during a
	// SolidWorks call) waits for it to finish instead of running nested.
	Outcome Run(const std::function<void()>& work, const std::function<bool()>& cancelled);

	// On the owning thread, before the server stops: work that hasn't started
	// never will, and Run refuses new work.
	void Close();

private:
	struct Job;

	static LRESULT CALLBACK WindowProc(HWND window, UINT message, WPARAM wParam, LPARAM lParam);
	void Drain();

	HWND m_window = nullptr;
	std::atomic<bool> m_closed{ false };
	std::mutex m_mutex;
	std::deque<std::shared_ptr<Job>> m_queue;
	bool m_draining = false; // owning thread only
};
