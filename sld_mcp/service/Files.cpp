// Files.cpp : Documents: GetActiveDocument, NewPart, NewAssembly, NewDrawing, SetDocumentUnits, SaveDocument, OpenDocument, CloseDocument, FileReferences (see docs/notes/files.md, units.md).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The units a document shows its lengths in, and its drafting standard.
	void ReadUnits(IModelDocExtension* ext, sld_mcp::v1::DocumentUnits* out)
	{
		auto read = [&](long pref) {
			long value = 0;
			ext->GetUserPreferenceInteger(pref, swDetailingNoOptionSpecified, &value);
			return static_cast<int32_t>(value);
		};
		out->set_unit_system(read(swUnitSystem));
		out->set_length_unit(read(swUnitsLinear));
		out->set_length_decimals(read(swUnitsLinearDecimalPlaces));
		out->set_drafting_standard(read(swDetailingDimensionStandard));
	}

	// Sets what `change` asks for, each value read back.
	grpc::Status ApplyUnits(IModelDoc2* doc, const sld_mcp::v1::UnitsChange& change)
	{
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		auto set = [&](long pref, long value, const char* what) {
			VARIANT_BOOL ok = VARIANT_FALSE;
			HRESULT hr = ext->SetUserPreferenceInteger(pref, swDetailingNoOptionSpecified, value, &ok);
			if (FAILED(hr))
				return ComError("IModelDocExtension::SetUserPreferenceInteger", hr);
			long now = -1;
			ext->GetUserPreferenceInteger(pref, swDetailingNoOptionSpecified, &now);
			if (!ok || now != value)
				return grpc::Status(grpc::StatusCode::INTERNAL, std::string("SolidWorks didn't take the ") + what +
					" " + std::to_string(value) + " (it reads " + std::to_string(now) + ")");
			return grpc::Status::OK;
		};
		// The drafting standard first: switching it may reset other options.
		if (change.drafting_standard() != 0)
			if (grpc::Status status = set(swDetailingDimensionStandard, change.drafting_standard(), "drafting standard"); !status.ok())
				return status;
		// The unit system sets the length unit; decimals stay as they were.
		if (change.unit_system() != 0)
			if (grpc::Status status = set(swUnitSystem, change.unit_system(), "unit system"); !status.ok())
				return status;
		if (change.has_length_decimals())
			if (grpc::Status status = set(swUnitsLinearDecimalPlaces, change.length_decimals(), "length decimal places"); !status.ok())
				return status;
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	}

	// Title, type, file and units of a document.
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
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ReadUnits(ext, out->mutable_units());
		return grpc::Status::OK;
	}

	// A new document from the user's default template of a kind (Tools >
	// Options > System Options > Default Templates), made the active one,
	// with `units` applied (a failure closes it again).
	grpc::Status NewFromDefaultTemplate(ISldWorks* swApp, swUserPreferenceStringValue_e templatePref,
		const std::string& kind, const sld_mcp::v1::UnitsChange& units, sld_mcp::v1::Document* out)
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

		if (grpc::Status status = ApplyUnits(doc, units); !status.ok())
		{
			ATL::CComBSTR title;
			doc->GetTitle(&title);
			swApp->CloseDoc(title);
			return status;
		}
		return DescribeDocument(doc, out);
	}

	// Every loaded document, those without a window of their own (a component
	// of an open assembly) too.
	std::vector<ATL::CComPtr<IModelDoc2>> LoadedDocuments(ISldWorks* swApp)
	{
		std::vector<ATL::CComPtr<IModelDoc2>> documents;
		ATL::CComVariant all;
		if (SUCCEEDED(swApp->GetDocuments(&all)))
			for (const auto& disp : comvariant::ToDispatches(all))
				if (ATL::CComQIPtr<IModelDoc2> doc(disp.p); doc)
					documents.push_back(ATL::CComPtr<IModelDoc2>(doc));
		return documents;
	}

	bool HasWindow(IModelDoc2* doc)
	{
		VARIANT_BOOL visible = VARIANT_FALSE;
		return SUCCEEDED(doc->get_Visible(&visible)) && visible;
	}

	std::string TitleOf(IModelDoc2* doc)
	{
		ATL::CComBSTR title;
		doc->GetTitle(&title);
		return ToUtf8(title);
	}

	// The open document going by a title, full path or file name, in any case.
	// (GetOpenDocumentByName only takes a full path, seen 2026-11-01.)
	grpc::Status FindOpenDocument(ISldWorks* swApp, const std::string& name, ATL::CComPtr<IModelDoc2>& out)
	{
		const ATL::CComBSTR wanted = FromUtf8(name);
		std::string open;
		for (const auto& doc : LoadedDocuments(swApp))
		{
			if (!HasWindow(doc))
				continue;
			ATL::CComBSTR title, path;
			doc->GetTitle(&title);
			doc->GetPathName(&path);
			const std::wstring file = path.Length() > 0 ? std::filesystem::path(path.m_str).filename().wstring() : L"";
			if (_wcsicmp(wanted, title) == 0 || (path.Length() > 0 && _wcsicmp(wanted, path) == 0) ||
				(!file.empty() && _wcsicmp(wanted, file.c_str()) == 0))
			{
				out = doc;
				return grpc::Status::OK;
			}
			open += (open.empty() ? "" : ", ") + ToUtf8(title);
		}
		return grpc::Status(grpc::StatusCode::NOT_FOUND, "no open document is called \"" + name + "\" -- " +
			(open.empty() ? "nothing is open" : "open: " + open));
	}

	// SolidWorks' working directory set to `folder` for a scope, then put
	// back. Imported assemblies' parts take their file names from it, and
	// Save As writes those never saved into it (seen 2026-11-03).
	class WorkingDirectory
	{
	public:
		WorkingDirectory(ISldWorks* app, const std::filesystem::path& folder) : m_app(app)
		{
			m_app->GetCurrentWorkingDirectory(&m_saved);
			VARIANT_BOOL set = VARIANT_FALSE;
			m_app->SetCurrentWorkingDirectory(ATL::CComBSTR((folder.wstring() + L"\\").c_str()), &set);
		}
		~WorkingDirectory()
		{
			VARIANT_BOOL set = VARIANT_FALSE;
			if (m_saved.Length() > 0)
				m_app->SetCurrentWorkingDirectory(m_saved, &set);
		}
		WorkingDirectory(const WorkingDirectory&) = delete;
		WorkingDirectory& operator=(const WorkingDirectory&) = delete;

	private:
		ISldWorks* m_app;
		ATL::CComBSTR m_saved;
	};

	// Files of an assembly's components, each once.
	std::vector<std::filesystem::path> ComponentFiles(IAssemblyDoc* assembly)
	{
		std::vector<std::filesystem::path> files;
		for (const auto& component : AllComponents(assembly))
		{
			ATL::CComBSTR path;
			if (SUCCEEDED(component->GetPathName(&path)) && path.Length() > 0 &&
				std::find(files.begin(), files.end(), std::filesystem::path(path.m_str)) == files.end())
				files.emplace_back(path.m_str);
		}
		return files;
	}

	// What GetDependencies2 / GetDocumentDependencies2 return: (name, path)
	// pairs, one per file used, or nothing (no references, no such file, not
	// a SolidWorks file). AddReadOnlyInfo isn't asked for: late-bound it adds
	// a "TRUE" / "FALSE" per file, early-bound it seemed not to (seen
	// 2026-11-09), so triples are recognised too.
	struct Dependency
	{
		std::wstring name, path;
	};

	std::vector<Dependency> ToDependencies(const VARIANT& v)
	{
		const auto strings = comvariant::ToBstrs(v);
		auto text = [&](size_t k) { return strings[k].Length() > 0 ? std::wstring(strings[k].m_str) : std::wstring(); };
		bool triples = !strings.empty() && strings.size() % 3 == 0;
		for (size_t i = 2; triples && i < strings.size(); i += 3)
			triples = _wcsicmp(text(i).c_str(), L"TRUE") == 0 || _wcsicmp(text(i).c_str(), L"FALSE") == 0;
		const size_t stride = triples ? 3 : 2;
		std::vector<Dependency> out;
		for (size_t i = 0; i + 1 < strings.size(); i += stride)
			out.push_back({ text(i), text(i + 1) });
		return out;
	}

	// `found` (search rules on) paired with `saved` (off) by name: both list
	// the same files in the same order (seen 2026-11-09).
	void AddReferences(const std::vector<Dependency>& found, const std::vector<Dependency>& saved,
		sld_mcp::v1::FileUses* out)
	{
		for (size_t i = 0; i < found.size(); ++i)
		{
			const Dependency* match = i < saved.size() && _wcsicmp(saved[i].name.c_str(), found[i].name.c_str()) == 0
				? &saved[i] : nullptr;
			for (size_t k = 0; !match && k < saved.size(); ++k)
				if (_wcsicmp(saved[k].name.c_str(), found[i].name.c_str()) == 0)
					match = &saved[k];
			auto* ref = out->add_references();
			ref->set_name(ToUtf8(ATL::CComBSTR(found[i].name.c_str())));
			ref->set_path(ToUtf8(ATL::CComBSTR(found[i].path.c_str())));
			ref->set_saved_path(ToUtf8(ATL::CComBSTR((match ? match->path : found[i].path).c_str())));
		}
	}

	// The files a loaded document uses directly, from memory.
	grpc::Status LoadedReferences(IModelDoc2* doc, sld_mcp::v1::FileUses* out)
	{
		ATL::CComVariant found, saved;
		HRESULT hr = doc->GetDependencies2(VARIANT_FALSE, VARIANT_TRUE, VARIANT_FALSE, &found);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetDependencies2", hr);
		hr = doc->GetDependencies2(VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, &saved);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetDependencies2", hr);
		VARIANT_BOOL dirty = VARIANT_FALSE;
		doc->GetSaveFlag(&dirty);
		out->set_loaded(true);
		out->set_unsaved_changes(dirty != VARIANT_FALSE);
		AddReferences(ToDependencies(found), ToDependencies(saved), out);
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
	const sld_mcp::v1::NewPartRequest* request,
	sld_mcp::v1::NewPartResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	return NewFromDefaultTemplate(swApp, swDefaultTemplatePart, "part", request->units(), response->mutable_document());
}

grpc::Status SolidWorksService::NewAssembly(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::NewAssemblyRequest* request,
	sld_mcp::v1::NewAssemblyResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	return NewFromDefaultTemplate(swApp, swDefaultTemplateAssembly, "assembly", request->units(), response->mutable_document());
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
	// a fallback: A3 unless the request names one).
	const long paperSize = request->has_paper_size() ? request->paper_size()
		: request->has_fallback_paper_size() ? request->fallback_paper_size() : swDwgPaperA3size;
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

	// Templates with a sheet of their own ignore NewDocument's paper size,
	// so an explicit one is set up afterwards.
	SheetChange change;
	change.format = formatPath;
	if (request->has_paper_size())
	{
		change.paperSize = paperSize;
		change.width = request->width();
		change.height = request->height();
	}
	SheetSetup setup;
	if (grpc::Status status = ChangeActiveSheet(drawing, change, setup); !status.ok())
		return fail(status);
	if (!(setup.width > 0.0 && setup.height > 0.0))
		return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"the sheet came out without a size -- give a paper size"));
	ATL::CComPtr<ISheet> sheet;
	if (grpc::Status status = ActiveSheet(drawing, sheet); !status.ok())
		return fail(status);

	if (grpc::Status status = ApplyUnits(doc, request->units()); !status.ok())
		return fail(status);

	sld_mcp::v1::SheetInfo* info = response->mutable_sheet();
	info->set_name(SheetName(sheet));
	info->set_active(true);
	SetSheetInfo(setup, info);
	ATL::CComBSTR formatName;
	if (SUCCEEDED(sheet->GetSheetFormatName(&formatName)))
		info->set_sheet_format(ToUtf8(formatName));
	if (setup.HasFormat())
		response->set_sheet_format_path(ToUtf8(setup.format));
	response->set_template_(ToUtf8(ATL::CComBSTR(templatePath.c_str())));

	if (grpc::Status status = DescribeDocument(doc, response->mutable_document()); !status.ok())
		return fail(status);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::SetDocumentUnits(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetDocumentUnitsRequest* request,
	sld_mcp::v1::SetDocumentUnitsResponse* response)
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

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ReadUnits(ext, response->mutable_before());
	if (grpc::Status status = ApplyUnits(doc, request->units()); !status.ok())
		return status;
	return DescribeDocument(doc, response->mutable_document());
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
	HRESULT hr = S_OK;
	if (!request->document().empty())
	{
		if (grpc::Status status = FindOpenDocument(swApp, request->document(), doc); !status.ok())
			return status;
	}
	else if (FAILED(hr = swApp->get_IActiveDoc2(&doc)))
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
			" (" + (request->document().empty() ? "the active document, " : "") + before.title() + ")");

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

	// A drawing's PDF: which sheets (all of them unless named, rather than
	// whatever the user last picked), and color for this export only.
	ATL::CComPtr<IDispatch> exportData;
	PreferenceToggles prefs(swApp);
	if (!inPlace && request->has_pdf())
	{
		ATL::CComQIPtr<IDrawingDoc> drawing(doc.p);
		if (!drawing)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "only a drawing has sheets to export to PDF");
		const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
		std::vector<std::string> wanted;
		for (const std::string& name : request->pdf().sheets())
		{
			const DrawingSheet* sheet = FindSheet(sheets, name);
			if (!sheet)
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					"the drawing has no sheet \"" + name + "\" -- its sheets: " + SheetList(sheets));
			wanted.push_back(sheet->name);
		}
		std::vector<ATL::CComBSTR> names;
		for (const DrawingSheet& sheet : sheets)
			if (wanted.empty() || std::find(wanted.begin(), wanted.end(), sheet.name) != wanted.end())
			{
				names.emplace_back(FromUtf8(sheet.name));
				response->add_pdf_sheets(sheet.name);
			}

		hr = swApp->GetExportFileData(swExportPdfData, &exportData);
		ATL::CComQIPtr<IExportPdfData> pdf(exportData.p);
		if (FAILED(hr) || !pdf)
			return ComError("ISldWorks::GetExportFileData(swExportPdfData)", FAILED(hr) ? hr : E_NOINTERFACE);
		// Every sheet by name too: SetSheets returns false for
		// swExportData_ExportAllSheets / ExportCurrentSheet even though it
		// takes them (GetWhichSheets reads back), true only for a list.
		VARIANT_BOOL set = VARIANT_FALSE;
		hr = pdf->SetSheets(swExportData_ExportSpecifiedSheets, comvariant::FromBstrs(names), &set);
		if (FAILED(hr))
			return ComError("IExportPdfData::SetSheets", hr);
		if (!set)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't take the sheets to export to PDF");
		pdf->put_ViewPdfAfterSaving(VARIANT_FALSE);
		if (request->pdf().has_color())
			prefs.Set(swPDFExportInColor, request->pdf().color());
	}

	// An imported assembly's parts have never been saved (there's no file
	// at their path yet): Save As writes them into the working directory
	// (seen 2026-11-03), so for the call that's the assembly's folder, and
	// a file of one's name there is refused rather than overwritten.
	std::optional<WorkingDirectory> partsFolder;
	if (!inPlace && !request->copy())
		if (ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p); assembly)
		{
			const std::filesystem::path folder = std::filesystem::path(target.m_str).parent_path();
			std::string clash;
			bool unsaved = false;
			for (const auto& part : ComponentFiles(assembly))
			{
				std::error_code ec;
				if (std::filesystem::exists(part, ec))
					continue;
				unsaved = true;
				if (const std::filesystem::path there = folder / part.filename(); std::filesystem::exists(there, ec))
					clash += (clash.empty() ? "" : ", ") + ToUtf8(ATL::CComBSTR(there.c_str()));
			}
			if (!clash.empty())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "the assembly has parts that were never "
					"saved (it was imported), which are saved next to it, and there are files of their names "
					"already: " + clash + " -- save it to another folder");
			if (unsaved)
				partsFolder.emplace(swApp, folder);
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
		hr = ext->SaveAs(target, swSaveAsCurrentVersion, options, exportData, &errors, &warnings, &ok);
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
		if (!request->import())
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
	long errors = 0, warnings = 0;
	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = S_OK;
	if (request->import())
	{
		// The new document is named after the file ("bracket.STEP" ->
		// "bracket.SLDPRT"), even next to an open document of that name,
		// which close_document and ActivateDoc3 then can't tell apart.
		const std::filesystem::path file(path.m_str);
		for (const auto& open : LoadedDocuments(swApp))
		{
			ATL::CComBSTR title, openPath;
			open->GetTitle(&title);
			open->GetPathName(&openPath);
			const std::wstring name = title.Length() > 0 ? title.m_str : L"";
			if (_wcsicmp(std::filesystem::path(name).stem().c_str(), file.stem().c_str()) == 0)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "a document called " + ToUtf8(title) +
					" is open (" + (openPath.Length() > 0 ? ToUtf8(openPath) : std::string("never saved")) +
					"), and the import would be called that too -- close it first, or import a copy of the file "
					"under another name");
		}

		// OpenDoc6 refuses a STEP file (swFileRequiresRepairError, seen
		// 2026-11-03). With 3D Interconnect on, STEP and IGES would come in
		// as a linked MBimport feature; off, they're an Imported1 body like
		// Parasolid, which features can be built on (files.md). An
		// assembly's parts get their file names in the working directory,
		// fixed then: the import's own folder rather than wherever that was
		// left (an earlier session's folder, seen 2026-11-03).
		{
			WorkingDirectory folder(swApp, file.parent_path());
			PreferenceToggles prefs(swApp);
			prefs.Set(swMultiCAD_Enable3DInterconnect, false);
			hr = swApp->LoadFile4(path, ATL::CComBSTR(L"r"), nullptr, &errors, &doc);
		}
		if (FAILED(hr))
			return ComError("ISldWorks::LoadFile4", hr);

		// A part named after a file that's already there (the user's own
		// part, or an earlier import saved) would be saved over it: as far as
		// SolidWorks knows, it is that file.
		if (ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p); assembly)
		{
			std::string existing;
			for (const auto& part : ComponentFiles(assembly))
			{
				std::error_code ec;
				if (std::filesystem::exists(part, ec))
					existing += (existing.empty() ? "" : ", ") + ToUtf8(ATL::CComBSTR(part.c_str()));
			}
			if (!existing.empty())
			{
				ATL::CComBSTR title;
				doc->GetTitle(&title);
				doc.Release();
				swApp->CloseDoc(title);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "the assembly's parts would be saved "
					"next to " + request->path() + ", but there are files of their names already: " + existing +
					" -- import a copy of it from a folder of its own (or open the SolidWorks files if they're an "
					"earlier import of it)");
			}
		}
	}
	else
	{
		ATL::CComPtr<IDispatch> open;
		response->set_already_open(SUCCEEDED(swApp->GetOpenDocumentByName(path, &open)) && open);
		const long options = swOpenDocOptions_Silent | (request->read_only() ? swOpenDocOptions_ReadOnly : 0);
		hr = swApp->OpenDoc6(path, type, options, ATL::CComBSTR(L""), &errors, &warnings, &doc);
		if (FAILED(hr))
			return ComError("ISldWorks::OpenDoc6", hr);
	}
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

grpc::Status SolidWorksService::CloseDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CloseDocumentRequest* request,
	sld_mcp::v1::CloseDocumentResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> active;
	HRESULT hr = swApp->get_IActiveDoc2(&active);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	ATL::CComPtr<IModelDoc2> doc;
	if (!request->document().empty())
	{
		if (grpc::Status status = FindOpenDocument(swApp, request->document(), doc); !status.ok())
			return status;
	}
	else if (!active)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
	else
		doc = active;
	if (grpc::Status status = DescribeDocument(doc, response->mutable_closed()); !status.ok())
		return status;
	const std::string title = response->closed().title();

	// CloseDoc never asks: it throws unsaved changes away (seen 2026-11-01).
	VARIANT_BOOL dirty = VARIANT_FALSE;
	hr = doc->GetSaveFlag(&dirty);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetSaveFlag", hr);
	response->set_had_unsaved_changes(dirty != VARIANT_FALSE);
	if (dirty && !request->discard_changes())
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, title + " has unsaved changes -- save_document "
			"first, or close it with discard_changes to lose them");

	// Closing a document in the background activates another one (seen
	// 2026-11-01), so the one that was active is put back afterwards.
	const std::string keepActive = active && !SameObject(active, doc) ? TitleOf(active) : std::string();
	active.Release();
	doc.Release();
	hr = swApp->CloseDoc(FromUtf8(title));
	if (FAILED(hr))
		return ComError("ISldWorks::CloseDoc", hr);

	for (const auto& loaded : LoadedDocuments(swApp))
		if (TitleOf(loaded) == title)
		{
			if (HasWindow(loaded))
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't close " + title);
			response->set_still_loaded(true);
		}

	if (!keepActive.empty())
	{
		long errors = 0;
		ATL::CComPtr<IDispatch> activated;
		swApp->ActivateDoc3(FromUtf8(keepActive), VARIANT_FALSE, swDontRebuildActiveDoc, &errors, &activated);
	}
	hr = swApp->get_IActiveDoc2(&active);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	return active ? DescribeDocument(active, response->mutable_active()) : grpc::Status::OK;
}

grpc::Status SolidWorksService::FileReferences(grpc::ServerContext* context,
	const sld_mcp::v1::FileReferencesRequest* request,
	sld_mcp::v1::FileReferencesResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	if (request->files().empty())
	{
		ATL::CComPtr<IModelDoc2> doc;
		if (!request->document().empty())
		{
			if (grpc::Status status = FindOpenDocument(swApp, request->document(), doc); !status.ok())
				return status;
		}
		else
		{
			HRESULT hr = swApp->get_IActiveDoc2(&doc);
			if (FAILED(hr))
				return ComError("ISldWorks::IActiveDoc2", hr);
			if (!doc)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
		}
		if (grpc::Status status = DescribeDocument(doc, response->mutable_document()); !status.ok())
			return status;
		return LoadedReferences(doc, response->add_uses());
	}

	// A loaded file is read from memory: GetDocumentDependencies2 on its path
	// would read the file on disk, without the unsaved changes.
	const auto loaded = LoadedDocuments(swApp);
	for (const auto& file : request->files())
	{
		if (context->IsCancelled())
			return grpc::Status::CANCELLED;
		const ATL::CComBSTR path = FromUtf8(file);
		auto* uses = response->add_uses();
		const auto open = std::find_if(loaded.begin(), loaded.end(), [&](const ATL::CComPtr<IModelDoc2>& doc) {
			ATL::CComBSTR docPath;
			return SUCCEEDED(doc->GetPathName(&docPath)) && docPath.Length() > 0 && _wcsicmp(docPath, path) == 0;
		});
		if (open != loaded.end())
		{
			if (grpc::Status status = LoadedReferences(*open, uses); !status.ok())
				return status;
			continue;
		}
		ATL::CComVariant found, saved;
		HRESULT hr = swApp->GetDocumentDependencies2(path, VARIANT_FALSE, VARIANT_TRUE, VARIANT_FALSE, &found);
		if (FAILED(hr))
			return ComError("ISldWorks::GetDocumentDependencies2", hr);
		hr = swApp->GetDocumentDependencies2(path, VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, &saved);
		if (FAILED(hr))
			return ComError("ISldWorks::GetDocumentDependencies2", hr);
		AddReferences(ToDependencies(found), ToDependencies(saved), uses);
	}
	return grpc::Status::OK;
}
