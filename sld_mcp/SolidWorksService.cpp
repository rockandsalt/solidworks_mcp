// SolidWorksService.cpp : RPC implementations, each a thin wrapper over the
// SolidWorks API.
#include "pch.h"
#include "SolidWorksService.h"
#include <cstdio>
#include <string>

namespace
{
	// Joins the calling gRPC worker thread to the MTA for the duration of one
	// RPC. COM objects (including the proxy from the GIT) must be released
	// before this goes out of scope, so declare it first in each handler.
	class ComApartment
	{
	public:
		ComApartment() : m_hr(CoInitializeEx(nullptr, COINIT_MULTITHREADED)) {}
		~ComApartment()
		{
			if (SUCCEEDED(m_hr))
				CoUninitialize();
		}
		ComApartment(const ComApartment&) = delete;
		ComApartment& operator=(const ComApartment&) = delete;

		// RPC_E_CHANGED_MODE: the thread is already in an STA, which works too.
		bool Ok() const { return SUCCEEDED(m_hr) || m_hr == RPC_E_CHANGED_MODE; }

	private:
		HRESULT m_hr;
	};

	grpc::Status ComError(const char* what, HRESULT hr)
	{
		char message[128];
		std::snprintf(message, sizeof(message), "%s failed (HRESULT 0x%08lX)",
			what, static_cast<unsigned long>(hr));
		return grpc::Status(grpc::StatusCode::INTERNAL, message);
	}

	// protobuf strings are UTF-8; SolidWorks hands back UTF-16 BSTRs.
	std::string ToUtf8(const ATL::CComBSTR& s)
	{
		const int wideLen = static_cast<int>(s.Length());
		if (wideLen == 0)
			return {};
		const int len = WideCharToMultiByte(CP_UTF8, 0, s.m_str, wideLen, nullptr, 0, nullptr, nullptr);
		std::string out(static_cast<size_t>(len), '\0');
		WideCharToMultiByte(CP_UTF8, 0, s.m_str, wideLen, out.data(), len, nullptr, nullptr);
		return out;
	}
}

SolidWorksService::SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie)
	: m_git(git), m_swAppCookie(swAppCookie)
{
}

grpc::Status SolidWorksService::GetSwApp(ATL::CComPtr<ISldWorks>& swApp) const
{
	HRESULT hr = m_git->GetInterfaceFromGlobal(m_swAppCookie, __uuidof(ISldWorks),
		reinterpret_cast<void**>(&swApp));
	if (FAILED(hr))
		return grpc::Status(grpc::StatusCode::UNAVAILABLE, "lost connection to SolidWorks");
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::GetActiveDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::GetActiveDocumentRequest* /*request*/,
	sld_mcp::v1::GetActiveDocumentResponse* response)
{
	ComApartment com;
	if (!com.Ok())
		return grpc::Status(grpc::StatusCode::INTERNAL, "CoInitializeEx failed on gRPC worker thread");

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = swApp->get_IActiveDoc2(&doc);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	if (!doc)
		return grpc::Status::OK; // nothing open: leave `document` unset

	ATL::CComBSTR title;
	hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);

	response->mutable_document()->set_title(ToUtf8(title));
	return grpc::Status::OK;
}
