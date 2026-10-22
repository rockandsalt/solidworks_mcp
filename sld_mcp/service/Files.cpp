// Files.cpp : Documents: GetActiveDocument, NewPart, NewAssembly, NewDrawing, SaveDocument, OpenDocument (see docs/notes/files.md).
#include "pch.h"
#include "Common.h"
#include "DrawingHelpers.h"

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

	// A new document from the user's default template of a kind (Tools >
	// Options > System Options > Default Templates), made the active one.
	grpc::Status NewFromDefaultTemplate(ISldWorks* swApp, swUserPreferenceStringValue_e templatePref,
		const std::string& kind, sld_mcp::v1::Document* out)
	{
		ATL::CComBSTR templatePath;
		HRESULT hr = swApp->GetUserPreferenceStringValue(templatePref, &templatePath);
		if (FAILED(hr))
			return ComError("ISldWorks::GetUserPreferenceStringValue", hr);
		if (templatePath.Length() == 0)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks has no default " + kind + " template set -- set one under Tools > Options > "
				"System Options > Default Templates");

		// Paper size and dimensions only apply to drawings.
		ATL::CComPtr<IDispatch> created;
		hr = swApp->NewDocument(templatePath, 0, 0.0, 0.0, &created);
		if (FAILED(hr))
			return ComError("ISldWorks::NewDocument", hr);
		ATL::CComQIPtr<IModelDoc2> doc(created.p);
		if (!doc)
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks couldn't create a " + kind + " from the default template " + ToUtf8(templatePath));

		return DescribeDocument(doc, out);
	}

	// `name` as an absolute path, or as a file in one of the folders the
	// file-location preference `foldersPref` lists (";"-separated), with or
	// without `extension` (".drwdot"). NOT_FOUND names the files there are.
	grpc::Status FindTemplateFile(ISldWorks* swApp, swUserPreferenceStringValue_e foldersPref,
		const std::string& name, const std::wstring& extension, const std::string& kind, std::wstring& path)
	{
		namespace fs = std::filesystem;
		const fs::path given(std::wstring(FromUtf8(name)));
		if (given.is_absolute())
		{
			std::error_code ec;
			if (!fs::is_regular_file(given, ec))
				return grpc::Status(grpc::StatusCode::NOT_FOUND, "there's no " + kind + " " + name);
			path = given.wstring();
			return grpc::Status::OK;
		}

		ATL::CComBSTR folderList;
		HRESULT hr = swApp->GetUserPreferenceStringValue(foldersPref, &folderList);
		if (FAILED(hr))
			return ComError("ISldWorks::GetUserPreferenceStringValue", hr);
		std::vector<fs::path> folders;
		std::wstring list = folderList.m_str ? folderList.m_str : L"";
		for (size_t start = 0; start <= list.size();)
		{
			const size_t end = std::min(list.find(L';', start), list.size());
			if (end > start)
				folders.emplace_back(list.substr(start, end - start));
			start = end + 1;
		}

		std::vector<std::string> available;
		for (const fs::path& folder : folders)
		{
			for (const fs::path& candidate : { folder / given, folder / (given.wstring() + extension) })
			{
				std::error_code ec;
				if (fs::is_regular_file(candidate, ec))
				{
					path = candidate.wstring();
					return grpc::Status::OK;
				}
			}
			std::error_code ec;
			for (fs::directory_iterator it(folder, ec), end; !ec && it != end; it.increment(ec))
				if (_wcsicmp(it->path().extension().c_str(), extension.c_str()) == 0)
					available.push_back(ToUtf8(ATL::CComBSTR(it->path().stem().c_str())));
		}

		std::string folderNames, there;
		for (const fs::path& folder : folders)
			folderNames += (folderNames.empty() ? "" : ", ") + ToUtf8(ATL::CComBSTR(folder.c_str()));
		for (const std::string& file : available)
			there += (there.empty() ? "" : ", ") + file;
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			"there's no " + kind + " \"" + name + "\" in " + (folderNames.empty() ? "SolidWorks' folders" : folderNames) +
			(there.empty() ? std::string(" (it holds none)") : "; there are: " + there));
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

	return NewFromDefaultTemplate(swApp, swDefaultTemplatePart, "part", response->mutable_document());
}

grpc::Status SolidWorksService::NewAssembly(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::NewAssemblyRequest* /*request*/,
	sld_mcp::v1::NewAssemblyResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	return NewFromDefaultTemplate(swApp, swDefaultTemplateAssembly, "assembly", response->mutable_document());
}

grpc::Status SolidWorksService::NewDrawing(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::NewDrawingRequest* request,
	sld_mcp::v1::NewDrawingResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	std::wstring templatePath;
	if (request->template_().empty())
	{
		ATL::CComBSTR defaultTemplate;
		HRESULT hr = swApp->GetUserPreferenceStringValue(swDefaultTemplateDrawing, &defaultTemplate);
		if (FAILED(hr))
			return ComError("ISldWorks::GetUserPreferenceStringValue", hr);
		if (defaultTemplate.Length() == 0)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks has no default drawing template set -- set one under Tools > Options > "
				"System Options > Default Templates, or give a template");
		templatePath = defaultTemplate.m_str;
	}
	else if (grpc::Status status = FindTemplateFile(swApp, swFileLocationsDocumentTemplates, request->template_(),
		L".drwdot", "drawing template", templatePath); !status.ok())
		return status;

	std::wstring formatPath;
	if (!request->sheet_format().empty())
		if (grpc::Status status = FindTemplateFile(swApp, swFileLocationsSheetFormat, request->sheet_format(),
			L".slddrt", "sheet format", formatPath); !status.ok())
			return status;

	// A template that has a sheet of its own ignores these; one without
	// makes its sheet this size (user defined 0 x 0 = an empty sheet, hence
	// A3 by default).
	const long paperSize = request->has_paper_size() ? request->paper_size() : swDwgPaperA3size;
	ATL::CComPtr<IDispatch> created;
	HRESULT hr = swApp->NewDocument(ATL::CComBSTR(templatePath.c_str()), paperSize, request->width(), request->height(), &created);
	if (FAILED(hr))
		return ComError("ISldWorks::NewDocument", hr);
	ATL::CComQIPtr<IModelDoc2> doc(created.p);
	if (!doc)
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"SolidWorks couldn't create a drawing from the template " + ToUtf8(ATL::CComBSTR(templatePath.c_str())));

	// From here on a failure closes the new document again.
	ATL::CComBSTR title;
	doc->GetTitle(&title);
	auto fail = [&](grpc::Status status) {
		swApp->CloseDoc(title);
		return status;
	};

	ATL::CComQIPtr<IDrawingDoc> drawing(doc.p);
	if (!drawing)
		return fail(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			ToUtf8(ATL::CComBSTR(templatePath.c_str())) + " isn't a drawing template"));

	// [paper size, template, scale 1, scale 2, first angle, width, height, ...]
	auto sheetProperties = [&](ATL::CComPtr<ISheet>& sheet, std::vector<double>& p) -> grpc::Status {
		sheet.Release();
		ATL::CComPtr<IDispatch> sheetDisp;
		HRESULT hr = drawing->GetCurrentSheet(&sheetDisp);
		if (FAILED(hr))
			return ComError("IDrawingDoc::GetCurrentSheet", hr);
		sheet = ATL::CComQIPtr<ISheet>(sheetDisp.p);
		ATL::CComVariant properties;
		if (!sheet || FAILED(sheet->GetProperties2(&properties)) ||
			(p = comvariant::ToDoubles(properties)).size() < 7)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't describe the new drawing's sheet");
		return grpc::Status::OK;
	};
	ATL::CComPtr<ISheet> sheet;
	std::vector<double> p;
	if (grpc::Status status = sheetProperties(sheet, p); !status.ok())
		return fail(status);

	// The sheet with another size and/or sheet format (empty = keep its own),
	// keeping its name, scale, projection and custom property view.
	auto setupSheet = [&](long paper, double width, double height, const std::wstring& format,
		const std::string& failure) -> grpc::Status {
		ATL::CComBSTR sheetName, currentFormat, propertyView;
		sheet->GetName(&sheetName);
		sheet->GetTemplateName(&currentFormat);
		sheet->get_CustomPropertyView(&propertyView);
		VARIANT_BOOL ok = VARIANT_FALSE;
		HRESULT hr = drawing->SetupSheet5(sheetName, paper, format.empty() ? static_cast<long>(p[1]) : swDwgTemplateCustom,
			p[2], p[3], p[4] != 0.0 ? VARIANT_TRUE : VARIANT_FALSE,
			format.empty() ? currentFormat : ATL::CComBSTR(format.c_str()), width, height, propertyView, VARIANT_TRUE, &ok);
		if (FAILED(hr))
			return ComError("IDrawingDoc::SetupSheet5", hr);
		if (!ok)
			return grpc::Status(grpc::StatusCode::INTERNAL, failure);
		return sheetProperties(sheet, p);
	};
	// A new sheet format brings its own size, even over a paper size given
	// in the same call; so the size, if asked for, is set after it.
	if (!formatPath.empty())
		if (grpc::Status status = setupSheet(swDwgPapersUserDefined, 0.0, 0.0, formatPath,
			"SolidWorks couldn't put the sheet format " + request->sheet_format() + " on the sheet"); !status.ok())
			return fail(status);
	auto sizeDiffers = [&] {
		return request->has_paper_size() &&
			(static_cast<long>(p[0]) != paperSize ||
				(paperSize == swDwgPapersUserDefined &&
					(std::abs(p[5] - request->width()) > 1e-6 || std::abs(p[6] - request->height()) > 1e-6)));
	};
	// Templates with a sheet of their own ignore NewDocument's paper size.
	if (sizeDiffers())
	{
		if (grpc::Status status = setupSheet(paperSize, request->width(), request->height(), L"",
			"SolidWorks couldn't change the sheet's size"); !status.ok())
			return fail(status);
		if (sizeDiffers())
			return fail(grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't change the sheet's size"));
	}
	if (!(p[5] > 0.0 && p[6] > 0.0))
		return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"the sheet came out without a size -- give a paper size"));

	ATL::CComPtr<IModelDocExtension> ext;
	hr = doc->get_Extension(&ext);
	if (FAILED(hr))
		return fail(ComError("IModelDoc2::Extension", hr));
	if (request->unit_system() != 0)
	{
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = ext->SetUserPreferenceInteger(swUnitSystem, swDetailingNoOptionSpecified, request->unit_system(), &ok);
		if (FAILED(hr))
			return fail(ComError("IModelDocExtension::SetUserPreferenceInteger", hr));
		if (!ok)
			return fail(grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't take the unit system"));
	}
	long unitSystem = 0;
	ext->GetUserPreferenceInteger(swUnitSystem, swDetailingNoOptionSpecified, &unitSystem);
	response->set_unit_system(static_cast<int32_t>(unitSystem));

	sld_mcp::v1::SheetInfo* info = response->mutable_sheet();
	info->set_name(SheetName(sheet));
	info->set_active(true);
	if (std::isfinite(p[0]))
		info->set_paper_size(static_cast<int32_t>(p[0]));
	info->set_scale_numerator(p[2]);
	info->set_scale_denominator(p[3]);
	info->set_first_angle(p[4] != 0.0);
	info->set_width(p[5]);
	info->set_height(p[6]);
	ATL::CComBSTR formatName, formatFile;
	if (SUCCEEDED(sheet->GetSheetFormatName(&formatName)))
		info->set_sheet_format(ToUtf8(formatName));
	// "*.drt" = no sheet format.
	if (SUCCEEDED(sheet->GetTemplateName(&formatFile)) && formatFile.Length() > 0 && formatFile[0] != L'*')
		response->set_sheet_format_path(ToUtf8(formatFile));
	response->set_template_(ToUtf8(ATL::CComBSTR(templatePath.c_str())));

	if (grpc::Status status = DescribeDocument(doc, response->mutable_document()); !status.ok())
		return fail(status);
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
