// Files.cpp : Documents: GetActiveDocument, NewPart, SaveDocument, OpenDocument (see docs/notes/files.md).
#include "pch.h"
#include "Common.h"

using namespace sld_mcp::service;

namespace
{
	// Title, type and file of a document.
	grpc::Status DescribeDocument(IModelDoc2* doc, sld_mcp::v1::Document* out)
	{
		ATL::CComBSTR title, path;
		HRESULT hr = doc->GetTitle(&title);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetTitle", hr);
		long type = swDocNONE;
		hr = doc->GetType(&type);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetType", hr);
		hr = doc->GetPathName(&path);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetPathName", hr);
		out->set_title(ToUtf8(title));
		out->set_type(ToDocumentType(type));
		out->set_path(ToUtf8(path));
		return grpc::Status::OK;
	}

	// This SolidWorks process: its id and start time (process ids get reused).
	std::string ProcessSession()
	{
		FILETIME created{}, exited{}, kernel{}, user{};
		GetProcessTimes(GetCurrentProcess(), &created, &exited, &kernel, &user);
		char text[48];
		std::snprintf(text, sizeof(text), "%lu-%08lx%08lx", GetCurrentProcessId(),
			created.dwHighDateTime, created.dwLowDateTime);
		return text;
	}
}

grpc::Status SolidWorksService::GetActiveDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::GetActiveDocumentRequest* /*request*/,
	sld_mcp::v1::GetActiveDocumentResponse* response)
{
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	static const std::string session = ProcessSession();
	response->set_session(session);

	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = swApp->get_IActiveDoc2(&doc);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	if (!doc)
		return grpc::Status::OK; // nothing open: leave `document` unset

	return DescribeDocument(doc, response->mutable_document());
}

grpc::Status SolidWorksService::NewPart(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::NewPartRequest* /*request*/,
	sld_mcp::v1::NewPartResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	// Tools > Options > System Options > Default Templates > Parts
	ATL::CComBSTR templatePath;
	HRESULT hr = swApp->GetUserPreferenceStringValue(swDefaultTemplatePart, &templatePath);
	if (FAILED(hr))
		return ComError("ISldWorks::GetUserPreferenceStringValue", hr);
	if (templatePath.Length() == 0)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks has no default part template set -- set one under Tools > Options > "
			"System Options > Default Templates");

	// Paper size and dimensions only apply to drawings.
	ATL::CComPtr<IDispatch> created;
	hr = swApp->NewDocument(templatePath, 0, 0.0, 0.0, &created);
	if (FAILED(hr))
		return ComError("ISldWorks::NewDocument", hr);
	ATL::CComQIPtr<IModelDoc2> doc(created.p);
	if (!doc)
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"SolidWorks couldn't create a part from the default template " + ToUtf8(templatePath));

	ATL::CComBSTR title;
	hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);

	response->mutable_document()->set_title(ToUtf8(title));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::SaveDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SaveDocumentRequest* request,
	sld_mcp::v1::SaveDocumentResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = swApp->get_IActiveDoc2(&doc);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	if (!doc)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
	sld_mcp::v1::Document before;
	if (grpc::Status status = DescribeDocument(doc, &before); !status.ok())
		return status;

	const bool inPlace = request->path().empty();
	if (inPlace && before.path().empty())
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			before.title() + " has never been saved -- give a path to save it to");
	if (!request->allowed_types().empty() &&
		std::find(request->allowed_types().begin(), request->allowed_types().end(), before.type()) ==
		request->allowed_types().end())
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"that file format doesn't hold a " + std::string(before.type() == sld_mcp::v1::DOCUMENT_TYPE_PART ? "part"
				: before.type() == sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY ? "assembly" : "drawing") +
			" (the active document, " + before.title() + ")");

	const ATL::CComBSTR target = inPlace ? FromUtf8(before.path()) : FromUtf8(request->path());
	if (!inPlace)
	{
		// SaveAs over a file another open document is fails (error 1).
		ATL::CComPtr<IDispatch> open;
		if (SUCCEEDED(swApp->GetOpenDocumentByName(target, &open)) && open && !SameObject(open, doc))
		{
			ATL::CComBSTR openTitle;
			if (ATL::CComQIPtr<IModelDoc2> openDoc(open.p); openDoc)
				openDoc->GetTitle(&openTitle);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, request->path() +
				" is open in SolidWorks as another document (" + ToUtf8(openTitle) + ") -- close it or pick another path");
		}
	}

	// Whether the file was written, whatever SolidWorks says: a part with
	// rebuild errors is saved but reported as failed (diagnosing.md).
	const std::filesystem::path file(target.m_str);
	const auto started = std::filesystem::file_time_type::clock::now();
	long errors = 0, warnings = 0;
	VARIANT_BOOL ok = VARIANT_FALSE;
	if (inPlace)
		hr = doc->Save3(swSaveAsOptions_Silent | swSaveAsOptions_SaveReferenced, &errors, &warnings, &ok);
	else
	{
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		const long options = swSaveAsOptions_Silent |
			(request->copy() ? swSaveAsOptions_Copy : swSaveAsOptions_SaveReferenced);
		hr = ext->SaveAs(target, swSaveAsCurrentVersion, options, nullptr, &errors, &warnings, &ok);
	}
	if (FAILED(hr))
		return ComError(inPlace ? "IModelDoc2::Save3" : "IModelDocExtension::SaveAs", hr);

	std::error_code ec;
	const bool written = std::filesystem::is_regular_file(file, ec) &&
		std::filesystem::last_write_time(file, ec) >= started && !ec;
	response->set_saved(ok != VARIANT_FALSE || written);
	response->set_saved_path(ToUtf8(target));
	response->set_errors(static_cast<int32_t>(errors));
	response->set_warnings(static_cast<int32_t>(warnings));
	return DescribeDocument(doc, response->mutable_document());
}

grpc::Status SolidWorksService::OpenDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::OpenDocumentRequest* request,
	sld_mcp::v1::OpenDocumentResponse* response)
{
	long type = swDocNONE;
	switch (request->type())
	{
	case sld_mcp::v1::DOCUMENT_TYPE_PART: type = swDocPART; break;
	case sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY: type = swDocASSEMBLY; break;
	case sld_mcp::v1::DOCUMENT_TYPE_DRAWING: type = swDocDRAWING; break;
	default:
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the document type");
	}
	if (request->path().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the file to open");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	const ATL::CComBSTR path = FromUtf8(request->path());
	ATL::CComPtr<IDispatch> open;
	response->set_already_open(SUCCEEDED(swApp->GetOpenDocumentByName(path, &open)) && open);

	const long options = swOpenDocOptions_Silent | (request->read_only() ? swOpenDocOptions_ReadOnly : 0);
	long errors = 0, warnings = 0;
	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = swApp->OpenDoc6(path, type, options, ATL::CComBSTR(L""), &errors, &warnings, &doc);
	if (FAILED(hr))
		return ComError("ISldWorks::OpenDoc6", hr);
	response->set_errors(static_cast<int32_t>(errors));
	response->set_warnings(static_cast<int32_t>(warnings));
	response->set_opened(doc != nullptr);
	if (!doc)
		return grpc::Status::OK;
	if (grpc::Status status = DescribeDocument(doc, response->mutable_document()); !status.ok())
		return status;

	// OpenDoc6 shows a newly opened document, but leaves another one active
	// when this one was already open. ActivateDoc3 takes the title; the path
	// is the fallback.
	auto isActive = [&] {
		ATL::CComPtr<IModelDoc2> active;
		return SUCCEEDED(swApp->get_IActiveDoc2(&active)) && SameObject(active, doc);
	};
	for (const ATL::CComBSTR& name : { FromUtf8(response->document().title()), path })
	{
		if (isActive())
			break;
		long activateErrors = 0;
		ATL::CComPtr<IDispatch> activated;
		swApp->ActivateDoc3(name, VARIANT_FALSE, swDontRebuildActiveDoc, &activateErrors, &activated);
	}
	if (!isActive())
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "opened " + response->document().title() +
			" but SolidWorks wouldn't make it the active document");
	return grpc::Status::OK;
}
