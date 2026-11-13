// MainThread.cpp : see MainThread.h.
#include "pch.h"
#include "MainThread.h"
#include <exception>
#include <stdexcept>

namespace
{
	const wchar_t kWindowClass[] = L"sld_mcp.MainThread";
	constexpr UINT kWake = WM_APP + 1;
	// How often a waiting Run checks for cancellation and Close.
	constexpr DWORD kPollMs = 50;

	enum class State { Queued, Running, Done, Abandoned };
}

struct MainThread::Job
{
	explicit Job(const std::function<void()>& w) : work(&w), done(CreateEventW(nullptr, TRUE, FALSE, nullptr)) {}
	~Job() { if (done) CloseHandle(done); }

	// Only called after a successful Queued -> Running, i.e. while Run still
	// waits, so the function it points to is alive.
	const std::function<void()>* work;
	HANDLE done;
	std::atomic<State> state{ State::Queued };
	std::exception_ptr error;
};

MainThread::MainThread()
{
	HMODULE module = nullptr;
	GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
		reinterpret_cast<LPCWSTR>(&WindowProc), &module);

	WNDCLASSEXW windowClass = { sizeof(windowClass) };
	windowClass.lpfnWndProc = &WindowProc;
	windowClass.hInstance = module;
	windowClass.lpszClassName = kWindowClass;
	// Already registered: the add-in was connected before (its DLL is pinned).
	if (!RegisterClassExW(&windowClass) && GetLastError() != ERROR_CLASS_ALREADY_EXISTS)
		throw std::runtime_error("couldn't register the main-thread window class");

	m_window = CreateWindowExW(0, kWindowClass, L"", 0, 0, 0, 0, 0, HWND_MESSAGE, nullptr, module, nullptr);
	if (!m_window)
		throw std::runtime_error("couldn't create the main-thread window");
	SetWindowLongPtrW(m_window, GWLP_USERDATA, reinterpret_cast<LONG_PTR>(this));
}

MainThread::~MainThread()
{
	Close();
	SetWindowLongPtrW(m_window, GWLP_USERDATA, 0);
	DestroyWindow(m_window);
}

void MainThread::Close()
{
	m_closed = true;
}

MainThread::Outcome MainThread::Run(const std::function<void()>& work, const std::function<bool()>& cancelled)
{
	auto job = std::make_shared<Job>(work);
	if (!job->done)
		throw std::runtime_error("couldn't create an event for main-thread work");
	{
		std::lock_guard<std::mutex> lock(m_mutex);
		if (m_closed)
			return Outcome::Closed;
		m_queue.push_back(job);
	}
	if (!PostMessageW(m_window, kWake, 0, 0))
	{
		// Left in the queue, where Drain skips it -- unless another wake already
		// started it, and then it's waited for as usual.
		State queued = State::Queued;
		if (job->state.compare_exchange_strong(queued, State::Abandoned))
			return Outcome::Closed;
	}

	while (WaitForSingleObject(job->done, kPollMs) != WAIT_OBJECT_0)
	{
		const bool closed = m_closed;
		if (!closed && !cancelled())
			continue;
		State queued = State::Queued;
		if (job->state.compare_exchange_strong(queued, State::Abandoned))
			return closed ? Outcome::Closed : Outcome::NotStarted;
		// Already running: it finishes.
	}
	if (job->state == State::Abandoned)
		return Outcome::Closed;
	if (job->error)
		std::rethrow_exception(job->error);
	return Outcome::Done;
}

void MainThread::Drain()
{
	// Woken again while running work (SolidWorks pumping messages inside one
	// of its calls): the outer loop picks the new work up when that's done.
	if (m_draining)
		return;
	m_draining = true;
	for (;;)
	{
		std::shared_ptr<Job> job;
		{
			std::lock_guard<std::mutex> lock(m_mutex);
			if (m_queue.empty())
				break;
			job = std::move(m_queue.front());
			m_queue.pop_front();
		}
		State queued = State::Queued;
		if (m_closed)
		{
			if (job->state.compare_exchange_strong(queued, State::Abandoned))
				SetEvent(job->done);
			continue;
		}
		if (!job->state.compare_exchange_strong(queued, State::Running))
			continue;
		// No exception may leave a window procedure into SolidWorks' code.
		try { (*job->work)(); }
		catch (...) { job->error = std::current_exception(); }
		job->state = State::Done;
		SetEvent(job->done);
	}
	m_draining = false;
}

LRESULT CALLBACK MainThread::WindowProc(HWND window, UINT message, WPARAM wParam, LPARAM lParam)
{
	if (message == kWake)
	{
		if (auto* self = reinterpret_cast<MainThread*>(GetWindowLongPtrW(window, GWLP_USERDATA)))
			self->Drain();
		return 0;
	}
	return DefWindowProcW(window, message, wParam, lParam);
}
