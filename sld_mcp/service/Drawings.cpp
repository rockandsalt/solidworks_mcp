// Drawings.cpp : Drawings: InspectDrawing, AddDrawingDimensions, InsertModelItems, RenderDrawing, SetupSheet,
// SetupDrawingView (see docs/notes/drawings.md).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxDrawingDimensions = 200;

	grpc::Status ValidateDrawingDimension(const sld_mcp::v1::DrawingDimension& dim, const std::string& where)
	{
		const int n = dim.refs_size();
		bool countOk = false;
		switch (dim.kind())
		{
		case sld_mcp::v1::DIMENSION_KIND_LENGTH:
		case sld_mcp::v1::DIMENSION_KIND_DIAMETER:
		case sld_mcp::v1::DIMENSION_KIND_RADIUS:
			countOk = n == 1;
			break;
		case sld_mcp::v1::DIMENSION_KIND_DISTANCE:
		case sld_mcp::v1::DIMENSION_KIND_ANGLE:
			countOk = n == 2;
			break;
		case sld_mcp::v1::DIMENSION_KIND_HORIZONTAL:
		case sld_mcp::v1::DIMENSION_KIND_VERTICAL:
			countOk = n == 1 || n == 2;
			break;
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, where + ": unknown dimension kind");
		}
		if (!countOk)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				where + ": wrong number of entities for a " + DimensionKindName(dim.kind()) + " dimension");
		for (const auto& ref : dim.refs())
		{
			if (ref.view_name() != dim.refs(0).view_name())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where + ": all entities of a dimension must be in the same view");
			if (ref.kind() == sld_mcp::v1::VIEW_ENTITY_KIND_UNSPECIFIED)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, where + ": entity kind missing");
		}
		if (dim.refs(0).view_name().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, where + ": view name missing");
		if (!std::isfinite(dim.text_x()) || !std::isfinite(dim.text_y()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, where + ": text position must be finite");
		return grpc::Status::OK;
	}

	// Identities and names of a view's annotations, to tell later which
	// ones are new.
	struct AnnotationSnapshot
	{
		std::set<IUnknown*> identities;
		std::vector<ATL::CComPtr<IUnknown>> alive;  // keeps the identities from being reused
		std::set<std::string> names;

		void Take(IView* view)
		{
			for (const auto& annotation : ViewAnnotationList(view))
			{
				ATL::CComPtr<IUnknown> identity;
				if (SUCCEEDED(annotation->QueryInterface(IID_PPV_ARGS(&identity))) && identity &&
					identities.insert(identity.p).second)
					alive.push_back(identity);
				ATL::CComBSTR name;
				if (SUCCEEDED(annotation->GetName(&name)) && name.Length() > 0)
					names.insert(ToUtf8(name));
			}
		}

		bool Contains(IAnnotation* annotation) const
		{
			ATL::CComPtr<IUnknown> identity;
			if (SUCCEEDED(annotation->QueryInterface(IID_PPV_ARGS(&identity))) && identities.count(identity.p))
				return true;
			ATL::CComBSTR name;
			return SUCCEEDED(annotation->GetName(&name)) && name.Length() > 0 && names.count(ToUtf8(name)) > 0;
		}
	};
}

grpc::Status SolidWorksService::InspectDrawing(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectDrawingRequest* request,
	sld_mcp::v1::InspectDrawingResponse* response)
{
	// Read-only, but serialized with edits like InspectModel.
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComQIPtr<IDrawingDoc> drawing;
	if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;

	ATL::CComBSTR title;
	HRESULT hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);
	response->mutable_document()->set_title(ToUtf8(title));
	response->mutable_document()->set_type(sld_mcp::v1::DOCUMENT_TYPE_DRAWING);

	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	const std::set<std::string> entityViews(request->entity_views().begin(), request->entity_views().end());
	for (const auto& name : entityViews)
	{
		ATL::CComPtr<IView> view;
		std::string sheetName;
		if (grpc::Status status = FindDrawingView(sheets, name, view, sheetName); !status.ok())
			return status;
	}

	// A view's entities may only be listed while its sheet is showing, so
	// sheets with requested views are shown in turn; the one the user had
	// showing is put back after.
	SheetActivation sheetActivation(drawing);
	DrawingBudget budget;
	for (const auto& sheet : sheets)
	{
		const bool wanted = std::any_of(sheet.views.begin(), sheet.views.end(),
			[&](const ATL::CComPtr<IView>& view) { return entityViews.count(ViewName(view)) > 0; });
		if (wanted)
			sheetActivation.Show(sheet.name);
		DescribeSheet(sheet, sheetActivation.Original(), ext, math, entityViews, request->include_vertices(),
			request->include_annotations(), budget, response->add_sheets(), !request->skip_sheet_format());
	}
	response->set_truncated(budget.truncated);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddDrawingDimensions(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddDrawingDimensionsRequest* request,
	sld_mcp::v1::AddDrawingDimensionsResponse* response)
{
	const int count = request->dimensions_size();
	if (count == 0 || count > kMaxDrawingDimensions)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give between 1 and " + std::to_string(kMaxDrawingDimensions) + " dimensions");
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = ValidateDrawingDimension(request->dimensions(i), "dimension " + std::to_string(i + 1));
			!status.ok())
			return status;

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComQIPtr<IDrawingDoc> drawing;
	if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> drawingExt;
	if (grpc::Status status = GetExtension(doc, drawingExt); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;

	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	std::map<std::string, std::unique_ptr<DrawingViewContext>> contexts;
	auto contextFor = [&](const std::string& viewName, DrawingViewContext*& ctx) -> grpc::Status {
		if (auto it = contexts.find(viewName); it != contexts.end())
		{
			ctx = it->second.get();
			return grpc::Status::OK;
		}
		auto made = std::make_unique<DrawingViewContext>();
		if (grpc::Status status = FindDrawingView(sheets, viewName, made->view, made->sheetName); !status.ok())
			return status;
		made->model = ViewModelOf(made->view);
		made->ids = std::make_unique<PersistIds>(drawingExt);
		made->modelIds = std::make_unique<PersistIds>(made->model.ext);
		made->components = ViewComponents(made->view, made->model);
		if (made->model.type == swDocASSEMBLY)
			made->componentList = std::make_unique<ComponentList>(*made->modelIds, math,
				response->mutable_components(), ATL::CComQIPtr<IAssemblyDoc>(made->model.doc.p));
		ctx = made.get();
		contexts.emplace(viewName, std::move(made));
		return grpc::Status::OK;
	};
	// Every view is looked up before anything changes.
	for (const auto& dim : request->dimensions())
	{
		DrawingViewContext* ctx = nullptr;
		if (grpc::Status status = contextFor(dim.refs(0).view_name(), ctx); !status.ok())
			return status;
	}

	// No Modify dialog for a new dimension (drawing dimensions shouldn't
	// show one, but make sure nothing modal stalls the call).
	PreferenceToggles toggles(swApp);
	toggles.Set(swInputDimValOnCreate, false);
	// Restores the sheet the user had showing, after.
	SheetActivation sheetActivation(drawing);

	std::vector<ATL::CComPtr<IDisplayDimension>> created;
	grpc::Status failure = grpc::Status::OK;
	for (int i = 0; i < count && failure.ok(); ++i)
	{
		const auto& dim = request->dimensions(i);
		const std::string where = "dimension " + std::to_string(i + 1);
		DrawingViewContext* ctx = nullptr;
		if (failure = contextFor(dim.refs(0).view_name(), ctx); !failure.ok())
			break;

		// The view's sheet has to be showing for its entities to be listed
		// and selected.
		if (!sheetActivation.Show(ctx->sheetName))
		{
			failure = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				where + ": couldn't switch to sheet " + ctx->sheetName + ", which " + ViewName(ctx->view) + " is on");
			break;
		}
		VARIANT_BOOL ok = VARIANT_FALSE;
		drawing->ActivateView(FromUtf8(ViewName(ctx->view)), &ok);

		std::vector<ViewEntityItem> items(static_cast<size_t>(dim.refs_size()));
		for (int r = 0; r < dim.refs_size() && failure.ok(); ++r)
			failure = ResolveDrawingRef(*ctx, dim.refs(r), where, items[static_cast<size_t>(r)]);
		if (!failure.ok())
			break;

		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t r = 0; r < items.size() && failure.ok(); ++r)
			if (!SelectViewEntity(ctx->view, items[r]))
				failure = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where + ": SolidWorks wouldn't select entity " + std::to_string(r + 1) + " in " +
					ViewName(ctx->view));
		if (!failure.ok())
			break;

		const double at[3] = { dim.text_x(), dim.text_y(), 0.0 };
		ATL::CComPtr<IDispatch> createdDisp;
		const HRESULT hr = CreateDimension(doc, dim.kind(), at, createdDisp);
		ATL::CComQIPtr<IDisplayDimension> display(createdDisp.p);
		if (FAILED(hr) || !display)
		{
			failure = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				where + ": SolidWorks couldn't add a " + std::string(DimensionKindName(dim.kind())) +
				" dimension between those entities (do they suit that kind? e.g. diameter needs a circle "
				"or arc seen face-on, angle two straight edges)");
			break;
		}
		created.push_back(ATL::CComPtr<IDisplayDimension>(display));

		sld_mcp::v1::DrawingDimensionResult* result = response->add_dimensions();
		result->set_view_name(ViewName(ctx->view));
		ATL::CComPtr<IAnnotation> annotation;
		if (SUCCEEDED(display->IGetAnnotation(&annotation)) && annotation)
			DescribeAnnotation(annotation, *ctx->ids, ctx->componentList.get(), result->mutable_annotation());
		else
			DescribeDimension(display, result->mutable_annotation()->mutable_dimension()->mutable_dimension());
	}
	doc->ClearSelection2(VARIANT_TRUE);

	if (!failure.ok())
	{
		DeleteDimensions(doc, created);
		response->Clear();
		return failure;
	}
	doc->GraphicsRedraw2();
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::InsertModelItems(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InsertModelItemsRequest* request,
	sld_mcp::v1::InsertModelItemsResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComQIPtr<IDrawingDoc> drawing;
	if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;

	// The target views: the named ones (all on one sheet), or every view of
	// the active sheet.
	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	const bool allViews = request->view_names().empty();
	std::vector<ATL::CComPtr<IView>> targets;
	std::string sheetName;
	if (allViews)
	{
		sheetName = ActiveSheetName(drawing);
		for (const auto& sheet : sheets)
			if (sheet.name == sheetName)
				targets = sheet.views;
		if (targets.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active sheet (" + sheetName + ") has no drawing views -- add a view of the part first");
	}
	else
		for (const auto& name : request->view_names())
		{
			ATL::CComPtr<IView> view;
			std::string viewSheet;
			if (grpc::Status status = FindDrawingView(sheets, name, view, viewSheet); !status.ok())
				return status;
			if (!sheetName.empty() && viewSheet != sheetName)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the views must all be on one sheet (" + name + " is on " + viewSheet + ", not " + sheetName + ")");
			sheetName = viewSheet;
			targets.push_back(view);
		}

	SheetActivation sheetActivation(drawing);
	if (!sheetActivation.Show(sheetName))
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheetName);

	std::vector<AnnotationSnapshot> before(targets.size());
	for (size_t i = 0; i < targets.size(); ++i)
		before[i].Take(targets[i]);

	doc->ClearSelection2(VARIANT_TRUE);
	if (!allViews)
		for (const auto& view : targets)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(ext->SelectByID2(FromUtf8(ViewName(view)), ATL::CComBSTR(L"DRAWINGVIEW"), 0.0, 0.0, 0.0,
				VARIANT_TRUE, 0, nullptr, swSelectOptionDefault, &selected)) || !selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select view " + ViewName(view));
			}
		}

	const long types = request->types() != 0 ? request->types() : static_cast<long>(swInsertDimensionsMarkedForDrawing);
	ATL::CComVariant inserted;
	HRESULT hr = drawing->InsertModelAnnotations3(swImportModelItemsFromEntireModel, types,
		allViews ? VARIANT_TRUE : VARIANT_FALSE,
		request->include_duplicates() ? VARIANT_TRUE : VARIANT_FALSE,
		request->include_hidden_features() ? VARIANT_TRUE : VARIANT_FALSE,
		VARIANT_TRUE,  // place dimensions where they are in the model's sketches
		&inserted);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IDrawingDoc::InsertModelAnnotations3", hr);
	int insertedCount = 0;
	for (const auto& disp : comvariant::ToDispatches(inserted))
		if (disp)
			++insertedCount;
	response->set_inserted(insertedCount);

	for (size_t i = 0; i < targets.size(); ++i)
	{
		// Ids as DescribeView hands them out.
		const ViewModel model = ViewModelOf(targets[i]);
		PersistIds ids(ext);
		PersistIds modelIds(model.ext);
		sld_mcp::v1::ViewAnnotations* out = response->add_views();
		out->set_view_name(ViewName(targets[i]));
		std::optional<ComponentList> components;
		if (model.type == swDocASSEMBLY)
			components.emplace(modelIds, math, out->mutable_components(), ATL::CComQIPtr<IAssemblyDoc>(model.doc.p));
		size_t described = 0;
		for (const auto& annotation : ViewAnnotationList(targets[i]))
			if (!before[i].Contains(annotation) && described++ < kMaxAnnotations)
				DescribeAnnotation(annotation, ids, components ? &*components : nullptr, out->add_annotations());
	}
	doc->GraphicsRedraw2();
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::RenderDrawing(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::RenderDrawingRequest* request,
	sld_mcp::v1::RenderDrawingResponse* response)
{
	constexpr double kMetersPerInch = 0.0254;
	double dpi = request->dpi() == 0 ? 150.0 : request->dpi();
	const double maxSide = request->max_px() == 0 ? 8000.0 : request->max_px();  // pixels
	// SolidWorks' range: asked for less than 100 it wrote 100; for 433 it
	// silently used another setting.
	if (dpi < 100.0 || dpi > 300.0 || maxSide < 100.0 || maxSide > 8000.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "dpi must be 100..300 and max_px 100..8000");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComQIPtr<IDrawingDoc> drawing;
	if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	std::string wanted = request->sheet_name();
	if (wanted.empty() && !request->view_name().empty())
	{
		ATL::CComPtr<IView> view;
		if (grpc::Status status = FindDrawingView(sheets, request->view_name(), view, wanted); !status.ok())
			return status;
	}
	if (wanted.empty())
		wanted = ActiveSheetName(drawing);
	auto sheet = std::find_if(sheets.begin(), sheets.end(), [&](const DrawingSheet& s) { return s.name == wanted; });
	if (sheet == sheets.end())
	{
		std::string names;
		for (const auto& s : sheets)
			names += (names.empty() ? "" : ", ") + s.name;
		return grpc::Status(grpc::StatusCode::NOT_FOUND, "the drawing has no sheet \"" + wanted + "\" (it has " +
			names + ")");
	}

	// [paper size, template, scale 1, scale 2, first angle, width, height, ...]
	double width = 0.0, height = 0.0;
	ATL::CComVariant properties;
	if (sheet->sheet && SUCCEEDED(sheet->sheet->GetProperties2(&properties)))
		if (const std::vector<double> p = comvariant::ToDoubles(properties); p.size() >= 7)
		{
			width = p[5];
			height = p[6];
		}
	if (!(width > 0.0 && height > 0.0))
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't give sheet " + wanted + "'s size");
	dpi = std::max(100.0, std::floor(std::min(dpi, maxSide * kMetersPerInch / std::max(width, height))));

	SheetActivation sheetActivation(drawing);
	if (!sheetActivation.Show(sheet->name))
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheet->name);

	// File > Save As image uses the TIF/PSD/JPG/PNG export options: print
	// capture of the active sheet, in color, on a user-defined paper the
	// size of the sheet. (swTiffPrintUseSheetSize alone was ignored: the
	// image came out US Letter with the sheet scaled into it.)
	PreferenceToggles prefs(swApp);
	prefs.Set(swTiffPrintAllSheets, false);
	prefs.SetInteger(swTiffScreenOrPrintCapture, 1);
	prefs.SetInteger(swTiffPrintPaperSize, swDwgPapersUserDefined);
	prefs.SetDouble(swTiffPrintDrawingPaperWidth, width);
	prefs.SetDouble(swTiffPrintDrawingPaperHeight, height);
	prefs.SetInteger(swTiffImageType, swTiffImageRGB);
	prefs.SetInteger(swTiffPrintDPI, static_cast<long>(dpi));

	// A folder of its own, since SolidWorks may name the file after the sheet.
	std::error_code ec;
	const std::filesystem::path dir = std::filesystem::temp_directory_path(ec) /
		("sld_mcp_render_" + std::to_string(GetCurrentProcessId()) + "_" + std::to_string(GetTickCount64()));
	if (ec || !std::filesystem::create_directories(dir, ec))
		return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't make a temporary folder: " + ec.message());
	struct RemoveDir
	{
		std::filesystem::path path;
		~RemoveDir() { std::error_code ignored; std::filesystem::remove_all(path, ignored); }
	} removeDir{ dir };

	std::string failures;
	for (const char* format : { "png", "jpg" })
	{
		const std::filesystem::path file = dir / (std::string("sheet.") + format);
		long errors = 0, warnings = 0;
		VARIANT_BOOL ok = VARIANT_FALSE;
		const HRESULT hr = ext->SaveAs(ATL::CComBSTR(file.wstring().c_str()), swSaveAsCurrentVersion,
			swSaveAsOptions_Silent | swSaveAsOptions_Copy, nullptr, &errors, &warnings, &ok);
		// The file SolidWorks wrote, whatever it called it.
		std::filesystem::path written;
		for (const auto& entry : std::filesystem::directory_iterator(dir, ec))
			if (entry.is_regular_file() && entry.path().extension() == file.extension())
				written = entry.path();
		if (written.empty())
		{
			failures += std::string(failures.empty() ? "" : "; ") + format + ": SaveAs -> " +
				(FAILED(hr) ? "HRESULT failure" : std::to_string(ok != VARIANT_FALSE)) + ", errors " +
				std::to_string(errors);
			continue;
		}
		std::ifstream in(written, std::ios::binary);
		std::string bytes((std::istreambuf_iterator<char>(in)), std::istreambuf_iterator<char>());
		if (bytes.empty())
		{
			failures += std::string(failures.empty() ? "" : "; ") + format + ": empty file";
			continue;
		}
		response->set_image(std::move(bytes));
		response->set_format(format);
		break;
	}
	if (response->image().empty())
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't save sheet " + sheet->name + " as an image (" + failures + ")");

	ATL::CComBSTR title;
	if (SUCCEEDED(doc->GetTitle(&title)))
		response->set_document_title(ToUtf8(title));
	response->set_sheet_name(sheet->name);
	response->set_width(width);
	response->set_height(height);
	response->set_dpi(static_cast<int32_t>(dpi));
	for (const auto& view : sheet->views)
	{
		ATL::CComVariant outline;
		if (FAILED(view->GetOutline(&outline)))
			continue;
		const std::vector<double> o = comvariant::ToDoubles(outline);
		if (o.size() < 4)
			continue;
		sld_mcp::v1::SheetViewOutline* out = response->add_views();
		out->set_name(ViewName(view));
		const double lo[3] = { o[0], o[1], 0.0 }, hi[3] = { o[2], o[3], 0.0 };
		SetVec3(out->mutable_outline()->mutable_min(), lo);
		SetVec3(out->mutable_outline()->mutable_max(), hi);
	}
	return grpc::Status::OK;
}

namespace
{
	// Deletes a sheet (and everything on it) the way the Delete key does.
	bool DeleteSheet(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE, deleted = VARIANT_FALSE;
		if (FAILED(ext->SelectByID2(FromUtf8(name), ATL::CComBSTR(L"SHEET"), 0.0, 0.0, 0.0, VARIANT_FALSE, 0, nullptr,
			swSelectOptionDefault, &selected)) || !selected)
			return false;
		const bool ok = SUCCEEDED(ext->DeleteSelection2(0, &deleted)) && deleted;
		doc->ClearSelection2(VARIANT_TRUE);
		return ok;
	}

	// After the sheet's projection changed, its projected views report the
	// new direction (ModelToViewTransform) but keep drawing the old side,
	// through rebuilds and even a save and reopen; switching a view's
	// display mode away and back makes SolidWorks draw it again.
	void RedrawProjectedViews(const DrawingSheet& sheet)
	{
		for (const auto& view : sheet.views)
		{
			if (ViewType(view) != swDrawingProjectedView)
				continue;
			long mode = swHIDDEN;
			VARIANT_BOOL useParent = VARIANT_FALSE, faceted = VARIANT_FALSE, edges = VARIANT_FALSE, ok = VARIANT_FALSE;
			if (FAILED(view->GetDisplayMode2(&mode)) || FAILED(view->GetUseParentDisplayMode(&useParent)))
				continue;
			view->GetFacettedHlrDisplay(&faceted);
			view->GetDisplayEdgesInShadedMode(&edges);
			view->SetDisplayMode3(VARIANT_FALSE, mode == swWIREFRAME ? swHIDDEN : swWIREFRAME, faceted, edges, &ok);
			view->SetDisplayMode3(useParent, mode, faceted, edges, &ok);
		}
	}

	// IDrawingDoc::SetupSheet5 with `setup` on the active sheet, named `name`.
	bool RestoreActiveSheet(IDrawingDoc* drawing, const std::string& name, const SheetSetup& setup)
	{
		VARIANT_BOOL ok = VARIANT_FALSE;
		return SUCCEEDED(drawing->SetupSheet5(FromUtf8(name),
			setup.paperSize >= 0 ? setup.paperSize : static_cast<long>(swDwgPapersUserDefined), setup.templateType,
			setup.scaleNumerator, setup.scaleDenominator, setup.firstAngle ? VARIANT_TRUE : VARIANT_FALSE,
			setup.format, setup.width, setup.height, setup.propertyView, VARIANT_TRUE, &ok)) && ok;
	}
}

grpc::Status SolidWorksService::SetupSheet(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetupSheetRequest* request,
	sld_mcp::v1::SetupSheetResponse* response)
{
	if (request->has_paper_size() && (request->paper_size() < swDwgPaperAsize || request->paper_size() > swDwgPapersUserDefined))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown paper size " + std::to_string(request->paper_size()));
	if (request->has_paper_size() && request->paper_size() == swDwgPapersUserDefined &&
		!(request->width() > 0.0 && request->height() > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a user-defined paper size needs a width and height above 0");
	if (request->has_scale_numerator() && !(request->scale_numerator() > 0.0 && request->scale_denominator() > 0.0 &&
		std::isfinite(request->scale_numerator()) && std::isfinite(request->scale_denominator())))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a sheet scale needs a numerator and denominator above 0");
	if (request->add() && !request->new_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a new sheet is named by sheet_name, not new_name");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComQIPtr<IDrawingDoc> drawing;
	if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;

	SheetChange change;
	if (!request->sheet_format().empty())
		if (grpc::Status status = FindTemplateFile(swApp, swFileLocationsSheetFormat, request->sheet_format(),
			L".slddrt", "sheet format", change.format); !status.ok())
			return status;
	if (request->has_paper_size())
	{
		change.paperSize = request->paper_size();
		change.width = request->width();
		change.height = request->height();
	}
	if (request->has_scale_numerator())
		change.scale = std::make_pair(request->scale_numerator(), request->scale_denominator());
	if (request->has_first_angle())
		change.firstAngle = request->first_angle();

	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	const std::string original = ActiveSheetName(drawing);
	std::string target;
	if (request->add())
	{
		target = request->sheet_name();
		if (!target.empty())
		{
			if (const DrawingSheet* taken = FindSheet(sheets, target))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the drawing already has a sheet named \"" + taken->name + "\" (its sheets: " + SheetList(sheets) + ")");
		}
		else
			for (size_t n = sheets.size() + 1; target.empty() || FindSheet(sheets, target); ++n)
				target = "Sheet" + std::to_string(n);
	}
	else
	{
		const std::string& wanted = request->sheet_name().empty() ? original : request->sheet_name();
		const DrawingSheet* found = FindSheet(sheets, wanted);
		if (!found)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the drawing has no sheet named \"" + wanted + "\" (its sheets: " + SheetList(sheets) + ")");
		target = found->name;
		if (!request->new_name().empty() && request->new_name() != target)
			for (const auto& s : sheets)
				if (s.name != target && SameSheetName(s.name, request->new_name()))
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						"the drawing already has a sheet named \"" + s.name + "\"");
	}

	std::string finalName = target;
	std::string activeAfter = original;
	{
		// SetupSheet5 only changes the active sheet: a changed sheet is
		// shown for the call and the user's put back (unless `activate`).
		SheetActivation activation(drawing);
		SheetSetup before;
		if (request->add())
		{
			// A copy of the active sheet's setup, as SolidWorks' Add Sheet
			// makes; NewSheet3 makes the new sheet the active one.
			ATL::CComPtr<ISheet> current;
			if (grpc::Status status = ActiveSheet(drawing, current); !status.ok())
				return status;
			if (grpc::Status status = ReadSheetSetup(current, before); !status.ok())
				return status;
			VARIANT_BOOL ok = VARIANT_FALSE;
			HRESULT hr = drawing->NewSheet3(FromUtf8(target),
				before.paperSize >= 0 ? before.paperSize : static_cast<long>(swDwgPapersUserDefined),
				before.HasFormat() ? before.templateType : static_cast<long>(swDwgTemplateNone),
				before.scaleNumerator, before.scaleDenominator, before.firstAngle ? VARIANT_TRUE : VARIANT_FALSE,
				before.HasFormat() ? before.format : ATL::CComBSTR(L""), before.width, before.height,
				ATL::CComBSTR(L""), &ok);
			if (FAILED(hr))
				return ComError("IDrawingDoc::NewSheet3", hr);
			if (!ok)
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't add the sheet " + target);
			if (ActiveSheetName(drawing) != target)
				activation.Show(target);
		}
		else
		{
			if (!activation.Show(target))
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't show the sheet " + target);
			ATL::CComPtr<ISheet> current;
			if (grpc::Status status = ActiveSheet(drawing, current); !status.ok())
				return status;
			if (grpc::Status status = ReadSheetSetup(current, before); !status.ok())
				return status;
		}

		// The sheet is the active one from here on. On failure a new sheet
		// is deleted again, a changed one set back.
		auto undo = [&](grpc::Status failure) {
			if (request->add())
			{
				// NewSheet3 showed the new sheet behind `activation`'s back.
				VARIANT_BOOL shown = VARIANT_FALSE;
				drawing->ActivateSheet(FromUtf8(original), &shown);
				DeleteSheet(doc, ext, target);
			}
			else
			{
				RestoreActiveSheet(drawing, target, before);
				VARIANT_BOOL rebuilt = VARIANT_FALSE;
				doc->EditRebuild3(&rebuilt);
			}
			return failure;
		};
		if (ActiveSheetName(drawing) != target)
			return undo(grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't show the sheet " + target));

		SheetSetup after;
		if (grpc::Status status = ChangeActiveSheet(drawing, change, after); !status.ok())
			return undo(status);
		if (!request->new_name().empty() && request->new_name() != target)
		{
			ATL::CComPtr<ISheet> current;
			if (grpc::Status status = ActiveSheet(drawing, current); !status.ok())
				return undo(status);
			// SetName ignores a name it doesn't take (empty, with "@", taken).
			current->SetName(FromUtf8(request->new_name()));
			if (SheetName(current) != request->new_name())
				return undo(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"SolidWorks didn't take \"" + request->new_name() + "\" as the sheet's name"));
			finalName = request->new_name();
		}

		if (after.firstAngle != before.firstAngle)
			for (const auto& s : DrawingSheets(drawing))
				if (s.name == finalName)
					RedrawProjectedViews(s);
		// Views follow the sheet's scale on a rebuild.
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		if (request->add() || request->activate())
		{
			activation.Keep();
			activeAfter = finalName;
		}
		else if (target == original)
			activeAfter = finalName;
		response->set_sheet_format_path(after.HasFormat() ? ToUtf8(after.format) : std::string());
	}

	for (const auto& s : DrawingSheets(drawing))
	{
		response->add_sheet_names(s.name);
		if (s.name == finalName)
		{
			DrawingBudget budget;
			DescribeSheet(s, activeAfter, ext, math, {}, false, false, budget, response->mutable_sheet());
		}
	}
	response->set_active_sheet(activeAfter);
	response->set_added(request->add());
	return grpc::Status::OK;
}

namespace
{
	std::string JoinNames(const std::vector<std::string>& names)
	{
		std::string list;
		for (const auto& name : names)
			list += (list.empty() ? "" : ", ") + name;
		return list;
	}

	std::string FileNameOf(const std::string& path)
	{
		const size_t slash = path.find_last_of("\\/");
		return slash == std::string::npos ? path : path.substr(slash + 1);
	}

	std::string ViewModelPath(IView* view)
	{
		ATL::CComBSTR path;
		view->GetReferencedModelName(&path);
		return ToUtf8(path);
	}

	ATL::CComPtr<IModelDoc2> OpenModel(ISldWorks* swApp, const std::wstring& path)
	{
		ATL::CComPtr<IDispatch> disp;
		if (FAILED(swApp->GetOpenDocumentByName(ATL::CComBSTR(path.c_str()), &disp)) || !disp)
			return {};
		ATL::CComQIPtr<IModelDoc2> doc(disp.p);
		return ATL::CComPtr<IModelDoc2>(doc);
	}

	std::vector<std::string> ModelViewNames(IModelDoc2* model)
	{
		std::vector<std::string> names;
		ATL::CComVariant namesVar;
		if (model && SUCCEEDED(model->GetModelViewNames(&namesVar)))
			for (const auto& name : comvariant::ToBstrs(namesVar))
				names.push_back(ToUtf8(name));
		return names;
	}

	// A named view of the model as SolidWorks spells it ("*front" ->
	// "*Front"); `wanted` itself if the model isn't open or has no such view.
	std::string NamedViewSpelling(IModelDoc2* model, const std::string& wanted)
	{
		for (const auto& name : ModelViewNames(model))
			if (EqualsIgnoringCase(name, wanted))
				return name;
		return wanted;
	}

	grpc::Status NoNamedView(IModelDoc2* model, const std::string& modelPath, const std::string& wanted)
	{
		const std::vector<std::string> names = ModelViewNames(model);
		return grpc::Status(grpc::StatusCode::NOT_FOUND, FileNameOf(modelPath) + " has no named view \"" + wanted + "\"" +
			(names.empty() ? std::string() : " (its named views: " + JoinNames(names) + ")"));
	}

	// A configuration of the model as SolidWorks spells it, from the open
	// document if there is one (it may have configurations not saved yet),
	// else from the file. An unknown name given to a view leaves it with no
	// configuration at all, so it's checked first.
	grpc::Status FindConfiguration(ISldWorks* swApp, const std::wstring& modelPath, const std::string& wanted,
		std::string& exact)
	{
		ATL::CComVariant namesVar;
		HRESULT hr = S_OK;
		if (ATL::CComPtr<IModelDoc2> model = OpenModel(swApp, modelPath))
			hr = model->GetConfigurationNames(&namesVar);
		else
			hr = swApp->GetConfigurationNames(ATL::CComBSTR(modelPath.c_str()), &namesVar);
		if (FAILED(hr))
			return ComError("GetConfigurationNames", hr);
		std::vector<std::string> names;
		for (const auto& name : comvariant::ToBstrs(namesVar))
			names.push_back(ToUtf8(name));
		for (const bool ignoreCase : { false, true })
			for (const auto& name : names)
				if (ignoreCase ? EqualsIgnoringCase(name, wanted) : name == wanted)
				{
					exact = name;
					return grpc::Status::OK;
				}
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			FileNameOf(ToUtf8(ATL::CComBSTR(modelPath.c_str()))) + " has no configuration \"" + wanted + "\"" +
			(names.empty() ? std::string() : " (its configurations: " + JoinNames(names) + ")"));
	}

	// A part or assembly a view could show: one the drawing's views show, or
	// one open in SolidWorks (`path` empty if it was never saved).
	struct ModelCandidate
	{
		std::string path;
		std::string title;
		bool shown = false;  // by a view of the drawing
	};

	std::vector<ModelCandidate> ModelCandidates(ISldWorks* swApp, const std::vector<DrawingSheet>& sheets)
	{
		std::vector<ModelCandidate> out;
		auto add = [&](const std::string& path, const std::string& title, bool shown) {
			for (auto& c : out)
				if (path.empty() ? c.path.empty() && c.title == title : EqualsIgnoringCase(c.path, path))
				{
					c.shown = c.shown || shown;
					return;
				}
			out.push_back({ path, title, shown });
		};
		for (const auto& sheet : sheets)
			for (const auto& view : sheet.views)
				if (const std::string path = ViewModelPath(view); !path.empty())
					add(path, FileNameOf(path), true);
		ATL::CComVariant docs;
		if (SUCCEEDED(swApp->GetDocuments(&docs)))
			for (const auto& disp : comvariant::ToDispatches(docs))
			{
				ATL::CComQIPtr<IModelDoc2> doc(disp.p);
				long type = swDocNONE;
				if (!doc || FAILED(doc->GetType(&type)) || (type != swDocPART && type != swDocASSEMBLY))
					continue;
				ATL::CComBSTR path, title;
				doc->GetPathName(&path);
				doc->GetTitle(&title);
				add(ToUtf8(path), ToUtf8(title), false);
			}
		return out;
	}

	std::string CandidateList(const std::vector<ModelCandidate>& candidates)
	{
		std::vector<std::string> names;
		for (const auto& c : candidates)
			names.push_back(c.path.empty() ? c.title + " (never saved)" : FileNameOf(c.path));
		return JoinNames(names);
	}

	grpc::Status CandidatePath(const ModelCandidate& c, std::wstring& path)
	{
		if (c.path.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, c.title +
				" was never saved -- save it first (save_document): a drawing view shows a model file");
		path = std::wstring(FromUtf8(c.path));
		return grpc::Status::OK;
	}

	// The model file `wanted` names: an absolute .SLDPRT / .SLDASM path, or
	// the file name (extension optional) or title of a model the drawing
	// shows or SolidWorks has open. Empty: the drawing's one model, else the
	// one part or assembly open.
	grpc::Status ResolveViewModel(ISldWorks* swApp, const std::vector<DrawingSheet>& sheets, const std::string& wanted,
		std::wstring& path)
	{
		namespace fs = std::filesystem;
		const fs::path given(std::wstring(FromUtf8(wanted)));
		if (!wanted.empty() && given.is_absolute())
		{
			std::error_code ec;
			if (!fs::is_regular_file(given, ec))
				return grpc::Status(grpc::StatusCode::NOT_FOUND, "there's no file " + wanted);
			const std::wstring ext = given.extension().wstring();
			if (_wcsicmp(ext.c_str(), L".sldprt") != 0 && _wcsicmp(ext.c_str(), L".sldasm") != 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"a drawing view shows a part (.SLDPRT) or an assembly (.SLDASM), not " + wanted);
			path = given.wstring();
			return grpc::Status::OK;
		}

		const std::vector<ModelCandidate> candidates = ModelCandidates(swApp, sheets);
		if (wanted.empty())
		{
			std::vector<const ModelCandidate*> shown;
			for (const auto& c : candidates)
				if (c.shown)
					shown.push_back(&c);
			if (shown.size() == 1)
				return CandidatePath(*shown[0], path);
			if (shown.empty() && candidates.size() == 1)
				return CandidatePath(candidates[0], path);
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, candidates.empty()
				? "no part or assembly is open -- give the model's full path"
				: "say which model the view shows (an absolute path, or one of: " + CandidateList(candidates) + ")");
		}

		auto stem = [](const std::string& name) {
			const size_t dot = name.find_last_of('.');
			return dot == std::string::npos ? name : name.substr(0, dot);
		};
		std::vector<const ModelCandidate*> matches;
		for (const auto& c : candidates)
		{
			const std::string file = FileNameOf(c.path);
			if ((!file.empty() && (EqualsIgnoringCase(file, wanted) || EqualsIgnoringCase(stem(file), wanted))) ||
				EqualsIgnoringCase(c.title, wanted) || EqualsIgnoringCase(stem(c.title), wanted))
				matches.push_back(&c);
		}
		if (matches.size() == 1)
			return CandidatePath(*matches[0], path);
		if (matches.size() > 1)
		{
			std::vector<std::string> paths;
			for (const auto* c : matches)
				paths.push_back(c->path.empty() ? c->title : c->path);
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"several open models are called \"" + wanted + "\": " + JoinNames(paths) + " -- give the full path");
		}
		return grpc::Status(grpc::StatusCode::NOT_FOUND, "no open part or assembly is called \"" + wanted + "\"" +
			(candidates.empty() ? std::string(" (none is open)") : " (open: " + CandidateList(candidates) + ")") +
			" -- give the model's full path");
	}

	// How a drawing view is set up, to compare with or to put back.
	struct ViewState
	{
		std::string name;
		std::string modelPath;
		std::string configuration;
		std::string orientation;
		double x = 0.0;  // the view's center, sheet meters
		double y = 0.0;
		bool sheetScale = false;
		bool parentScale = false;
		double numerator = 1.0;
		double denominator = 1.0;
		long mode = swHIDDEN;  // ViewDisplayMode
		bool parentDisplay = false;
		bool faceted = false;
	};

	ViewState ReadViewState(IView* view)
	{
		ViewState s;
		s.name = ViewName(view);
		s.modelPath = ViewModelPath(view);
		ATL::CComBSTR text;
		if (SUCCEEDED(view->get_ReferencedConfiguration(&text)))
			s.configuration = ToUtf8(text);
		text.Empty();
		if (SUCCEEDED(view->GetOrientationName(&text)))
			s.orientation = ToUtf8(text);
		ATL::CComVariant position;
		if (SUCCEEDED(view->get_Position(&position)))
			if (const std::vector<double> p = comvariant::ToDoubles(position); p.size() >= 2)
			{
				s.x = p[0];
				s.y = p[1];
			}
		long useSheetScale = 0;
		if (SUCCEEDED(view->get_UseSheetScale(&useSheetScale)))
			s.sheetScale = useSheetScale != 0;
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(view->get_UseParentScale(&flag)))
			s.parentScale = flag != VARIANT_FALSE;
		ATL::CComVariant ratio;
		if (SUCCEEDED(view->get_ScaleRatio(&ratio)))
			if (const std::vector<double> r = comvariant::ToDoubles(ratio); r.size() >= 2)
			{
				s.numerator = r[0];
				s.denominator = r[1];
			}
		s.mode = ViewDisplayMode(view);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(view->GetUseParentDisplayMode(&flag)))
			s.parentDisplay = flag != VARIANT_FALSE;
		flag = VARIANT_FALSE;
		if (SUCCEEDED(view->GetFacettedHlrDisplay(&flag)))
			s.faceted = flag != VARIANT_FALSE;
		return s;
	}

	ATL::CComPtr<IView> BaseViewOf(IView* view)
	{
		ATL::CComPtr<IDispatch> disp;
		if (FAILED(view->GetBaseView(&disp)) || !disp)
			return {};
		ATL::CComQIPtr<IView> base(disp.p);
		return ATL::CComPtr<IView>(base);
	}

	// Views whose orientation is a named view of their model; the others
	// (projected, auxiliary, section, ...) follow the view they come from.
	bool HasOwnOrientation(IView* view)
	{
		const long type = ViewType(view);
		return type == swDrawingNamedView || type == swDrawingStandardView;
	}

	bool CloseRatio(double a, double b)
	{
		return std::abs(a - b) <= 1e-9 * std::max(std::abs(a), std::abs(b));
	}

	// What to change on a view; empty / unset = keep. Steps already as asked
	// are skipped.
	struct ViewChange
	{
		std::wstring modelPath;
		std::string configuration;  // as SolidWorks spells it
		std::string orientation;
		std::optional<std::pair<double, double>> position;
		enum class Scale { Keep, Ratio, Sheet, Parent } scale = Scale::Keep;
		double numerator = 1.0;
		double denominator = 1.0;
		std::optional<long> mode;  // swDisplayMode_e, swSHADED_EDGES included
		bool parentDisplay = false;
		std::string name;

		bool ScaleIs(const ViewState& s) const
		{
			switch (scale)
			{
			case Scale::Parent:
				return s.parentScale;
			case Scale::Sheet:
				return s.sheetScale && !s.parentScale;
			case Scale::Ratio:
				return !s.sheetScale && !s.parentScale && CloseRatio(s.numerator, numerator) &&
					CloseRatio(s.denominator, denominator);
			default:
				return true;
			}
		}

		// The change that puts a view back the way `s` describes it.
		static ViewChange Back(const ViewState& s, bool ownOrientation)
		{
			ViewChange c;
			c.modelPath = std::wstring(FromUtf8(s.modelPath));
			c.configuration = s.configuration;
			if (ownOrientation)
				c.orientation = s.orientation;
			c.position = std::make_pair(s.x, s.y);
			c.scale = s.parentScale ? Scale::Parent : s.sheetScale ? Scale::Sheet : Scale::Ratio;
			c.numerator = s.numerator;
			c.denominator = s.denominator;
			if (s.parentDisplay)
				c.parentDisplay = true;
			else
				c.mode = s.mode;
			c.name = s.name;
			return c;
		}
	};

	bool SelectDrawingView(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE;
		return SUCCEEDED(ext->SelectByID2(FromUtf8(name), ATL::CComBSTR(L"DRAWINGVIEW"), 0.0, 0.0, 0.0, VARIANT_FALSE,
			0, nullptr, swSelectOptionDefault, &selected)) && selected;
	}

	// Applies `c` to `view` step by step, each read back: model,
	// configuration, orientation, scale, display, position, name. Stops at
	// the first step that fails, unless `bestEffort` (putting a view back).
	grpc::Status ApplyViewChange(IModelDoc2* doc, IDrawingDoc* drawing, IModelDocExtension* ext, IView* view,
		const ViewChange& c, bool bestEffort)
	{
		grpc::Status first = grpc::Status::OK;
		// False when the caller should stop.
		auto step = [&](grpc::Status status) {
			if (!status.ok() && first.ok())
				first = status;
			return status.ok() || bestEffort;
		};
		const std::string viewName = ViewName(view);

		const std::string modelPath = ToUtf8(ATL::CComBSTR(c.modelPath.c_str()));
		if (!c.modelPath.empty() && !EqualsIgnoringCase(ViewModelPath(view), modelPath))
		{
			ATL::CComVariant views = comvariant::FromDispatches({ view });
			// The instances to replace are the view's root drawing component:
			// with none given SolidWorks throws (RPC_E_SERVERFAULT), even for a
			// part view.
			ATL::CComPtr<IDrawingComponent> root;
			ATL::CComVariant instances;
			if (SUCCEEDED(view->get_RootDrawingComponent(&root)) && root)
				instances = comvariant::FromDispatches({ root.p });
			VARIANT_BOOL ok = VARIANT_FALSE;
			HRESULT hr = drawing->ReplaceViewModel(ATL::CComBSTR(c.modelPath.c_str()), views, instances, &ok);
			if (!step(FAILED(hr) ? ComError("IDrawingDoc::ReplaceViewModel", hr)
				: !ok || !EqualsIgnoringCase(ViewModelPath(view), modelPath)
				? grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't replace " + viewName + "'s model with " + FileNameOf(modelPath))
				: grpc::Status::OK))
				return first;
		}

		if (!c.configuration.empty())
		{
			ATL::CComBSTR current;
			view->get_ReferencedConfiguration(&current);
			if (ToUtf8(current) != c.configuration)
			{
				view->put_ReferencedConfiguration(FromUtf8(c.configuration));
				current.Empty();
				view->get_ReferencedConfiguration(&current);
				if (!step(ToUtf8(current) == c.configuration ? grpc::Status::OK
					: grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"SolidWorks didn't switch " + viewName + " to configuration " + c.configuration)))
					return first;
			}
		}

		if (!c.orientation.empty())
		{
			ATL::CComBSTR current;
			view->GetOrientationName(&current);
			if (!EqualsIgnoringCase(ToUtf8(current), c.orientation))
			{
				// ShowNamedView2 in a drawing turns the selected view (else the
				// one made last), whatever ActivateView says.
				grpc::Status status = grpc::Status::OK;
				if (!SelectDrawingView(doc, ext, viewName))
					status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select " + viewName);
				else
				{
					doc->ShowNamedView2(FromUtf8(c.orientation), -1);
					doc->ClearSelection2(VARIANT_TRUE);
					current.Empty();
					view->GetOrientationName(&current);
					if (!EqualsIgnoringCase(ToUtf8(current), c.orientation))
					{
						ATL::CComPtr<IModelDoc2> model;
						view->get_ReferencedDocument(&model);
						status = NoNamedView(model, ViewModelPath(view), c.orientation);
					}
				}
				if (!step(status))
					return first;
			}
		}

		if (c.scale != ViewChange::Scale::Keep)
		{
			const ViewState s = ReadViewState(view);
			if (!c.ScaleIs(s))
			{
				if (c.scale == ViewChange::Scale::Parent)
					view->put_UseParentScale(VARIANT_TRUE);
				else
				{
					if (s.parentScale)
						view->put_UseParentScale(VARIANT_FALSE);
					if (c.scale == ViewChange::Scale::Sheet)
						view->put_UseSheetScale(1);
					else
					{
						// Turns the view's UseSheetScale off.
						const double ratio[2] = { c.numerator, c.denominator };
						view->put_ScaleRatio(comvariant::FromDoubles(ratio, 2));
					}
				}
				if (!step(c.ScaleIs(ReadViewState(view)) ? grpc::Status::OK
					: grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't change " + viewName + "'s scale")))
					return first;
			}
		}

		if (c.mode || c.parentDisplay)
		{
			const ViewState s = ReadViewState(view);
			long mode = c.mode.value_or(s.mode);
			if (c.parentDisplay)
				if (ATL::CComPtr<IView> base = BaseViewOf(view))
					mode = ViewDisplayMode(base);
			if (s.mode != mode || s.parentDisplay != c.parentDisplay)
			{
				// "Shaded with edges" is shaded plus the edges flag; mode 7
				// itself SolidWorks ignores (the view stayed hidden lines removed).
				const bool edges = mode == swSHADED_EDGES;
				VARIANT_BOOL ok = VARIANT_FALSE;
				view->SetDisplayMode3(c.parentDisplay ? VARIANT_TRUE : VARIANT_FALSE, edges ? swSHADED : mode,
					s.faceted ? VARIANT_TRUE : VARIANT_FALSE, edges ? VARIANT_TRUE : VARIANT_FALSE, &ok);
				const ViewState after = ReadViewState(view);
				if (!step(after.parentDisplay == c.parentDisplay && (c.parentDisplay || after.mode == mode)
					? grpc::Status::OK
					: grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"SolidWorks didn't change " + viewName + "'s display mode")))
					return first;
			}
		}

		// Views aligned with another one move only along the alignment;
		// what SolidWorks made of it is in the reply.
		if (c.position)
		{
			const double at[2] = { c.position->first, c.position->second };
			HRESULT hr = view->put_Position(comvariant::FromDoubles(at, 2));
			if (!step(FAILED(hr) ? ComError("IView::Position", hr) : grpc::Status::OK))
				return first;
		}

		if (!c.name.empty() && ViewName(view) != c.name)
		{
			// SetName2 refuses names it doesn't take ("a@b").
			VARIANT_BOOL ok = VARIANT_FALSE;
			view->SetName2(FromUtf8(c.name), &ok);
			if (!step(ViewName(view) == c.name ? grpc::Status::OK
				: grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"SolidWorks didn't take \"" + c.name + "\" as the view's name")))
				return first;
		}
		return first;
	}

	bool DeleteDrawingView(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name)
	{
		VARIANT_BOOL deleted = VARIANT_FALSE;
		const bool ok = SelectDrawingView(doc, ext, name) && SUCCEEDED(ext->DeleteSelection2(0, &deleted)) && deleted;
		doc->ClearSelection2(VARIANT_TRUE);
		return ok;
	}
}

grpc::Status SolidWorksService::SetupDrawingView(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetupDrawingViewRequest* request,
	sld_mcp::v1::SetupDrawingViewResponse* response)
{
	using Request = sld_mcp::v1::SetupDrawingViewRequest;
	const bool create = request->create();
	if (!create && request->view_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "name the view to change");
	if (create && !request->new_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a new view is named by view_name, not new_name");
	if (request->scale_case() == Request::kRatio &&
		!(request->ratio().numerator() > 0.0 && request->ratio().denominator() > 0.0 &&
			std::isfinite(request->ratio().numerator()) && std::isfinite(request->ratio().denominator())))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a view scale needs a numerator and denominator above 0");
	if (request->has_display_mode())
	{
		const long mode = request->display_mode();
		if (mode != swWIREFRAME && mode != swHIDDEN_GREYED && mode != swHIDDEN && mode != swSHADED && mode != swSHADED_EDGES)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown display mode " + std::to_string(mode));
		if (request->parent_display())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give display_mode or parent_display, not both");
	}
	if (create && (request->parent_display() || request->scale_case() == Request::kFollowParent))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"a new view is made from the model, not derived from another view: it has no parent scale or display");
	if (request->has_position() && !(std::isfinite(request->position().x()) && std::isfinite(request->position().y())))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the view's position must be finite");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComQIPtr<IDrawingDoc> drawing;
	if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;

	// Everything is looked up before anything changes.
	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	const std::string original = ActiveSheetName(drawing);
	ATL::CComPtr<IView> view;
	std::string sheetName;
	if (create)
	{
		const std::string& wanted = request->sheet_name().empty() ? original : request->sheet_name();
		const DrawingSheet* sheet = FindSheet(sheets, wanted);
		if (!sheet)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the drawing has no sheet named \"" + wanted + "\" (its sheets: " + SheetList(sheets) + ")");
		sheetName = sheet->name;
	}
	else if (grpc::Status status = FindDrawingView(sheets, request->view_name(), view, sheetName); !status.ok())
		return status;
	const std::string& wantedName = create ? request->view_name() : request->new_name();
	if (!wantedName.empty() && (create || wantedName != request->view_name()))
	{
		ATL::CComPtr<IView> taken;
		std::string takenSheet;
		if (FindDrawingView(sheets, wantedName, taken, takenSheet).ok())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the drawing already has a view named \"" + wantedName + "\" (on " + takenSheet + ")");
	}
	if (view)
	{
		if (!request->orientation().empty() && !HasOwnOrientation(view))
		{
			ATL::CComPtr<IView> base = BaseViewOf(view);
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, request->view_name() + " is derived from " +
				(base ? ViewName(base) : std::string("another view")) +
				": it looks the way that view's orientation and the sheet's projection say -- change that view's "
				"orientation instead");
		}
		if ((request->parent_display() || request->scale_case() == Request::kFollowParent) && !BaseViewOf(view))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				request->view_name() + " isn't derived from another view: it has no parent scale or display");
	}

	ViewChange change;
	if (create || !request->model().empty())
		if (grpc::Status status = ResolveViewModel(swApp, sheets, request->model(), change.modelPath); !status.ok())
			return status;
	const std::wstring modelPath = !change.modelPath.empty() ? change.modelPath
		: std::wstring(FromUtf8(ViewModelPath(view)));
	if (!request->configuration().empty())
		if (grpc::Status status = FindConfiguration(swApp, modelPath, request->configuration(), change.configuration);
			!status.ok())
			return status;
	if (!request->orientation().empty())
		change.orientation = NamedViewSpelling(OpenModel(swApp, modelPath), request->orientation());
	if (request->has_position())
		change.position = std::make_pair(request->position().x(), request->position().y());
	switch (request->scale_case())
	{
	case Request::kRatio:
		change.scale = ViewChange::Scale::Ratio;
		change.numerator = request->ratio().numerator();
		change.denominator = request->ratio().denominator();
		break;
	case Request::kFollowSheet:
		change.scale = ViewChange::Scale::Sheet;
		break;
	case Request::kFollowParent:
		change.scale = ViewChange::Scale::Parent;
		break;
	default:
		break;
	}
	if (request->has_display_mode())
		change.mode = request->display_mode();
	change.parentDisplay = request->parent_display();
	change.name = wantedName;

	std::string finalName;
	{
		// The view's sheet is shown for the call (views are selected by name
		// on the active sheet); the user's is put back after.
		SheetActivation activation(drawing);
		if (!activation.Show(sheetName))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheetName);
		ATL::CComPtr<ISheet> sheet;
		if (grpc::Status status = ActiveSheet(drawing, sheet); !status.ok())
			return status;
		SheetSetup sheetBefore;
		if (grpc::Status status = ReadSheetSetup(sheet, sheetBefore); !status.ok())
			return status;
		response->mutable_sheet_scale_before()->set_numerator(sheetBefore.scaleNumerator);
		response->mutable_sheet_scale_before()->set_denominator(sheetBefore.scaleDenominator);

		// SolidWorks rescales a sheet to fit its first view ("automatically
		// scale new drawing views"); not when the caller gave the scale.
		PreferenceToggles prefs(swApp);
		if (create && request->scale_case() != Request::SCALE_NOT_SET)
			prefs.Set(swAutomaticScaling3ViewDrawings, false);

		ViewState before;
		if (create)
		{
			const std::string orientation = change.orientation.empty() ? "*Front" : change.orientation;
			const auto [x, y] = change.position.value_or(std::make_pair(sheetBefore.width / 2, sheetBefore.height / 2));
			doc->ClearSelection2(VARIANT_TRUE);
			HRESULT hr = drawing->CreateDrawViewFromModelView3(ATL::CComBSTR(change.modelPath.c_str()),
				FromUtf8(orientation), x, y, 0.0, &view);
			if (FAILED(hr))
				return ComError("IDrawingDoc::CreateDrawViewFromModelView3", hr);
			// Null for a named view the model doesn't have.
			if (!view)
				return NoNamedView(OpenModel(swApp, change.modelPath), ToUtf8(ATL::CComBSTR(change.modelPath.c_str())),
					orientation);
			// Made as asked; the rest is the change.
			change.modelPath.clear();
			change.orientation.clear();
			change.position.reset();
		}
		else
			before = ReadViewState(view);

		// On failure a new view is deleted again (and the sheet's scale, if
		// SolidWorks changed it for the view, put back); a changed one is
		// set back as it was.
		auto undo = [&](grpc::Status failure) {
			if (create)
			{
				DeleteDrawingView(doc, ext, ViewName(view));
				SheetSetup now;
				ATL::CComPtr<ISheet> current;
				if (ActiveSheet(drawing, current).ok() && ReadSheetSetup(current, now).ok() &&
					(!CloseRatio(now.scaleNumerator, sheetBefore.scaleNumerator) ||
						!CloseRatio(now.scaleDenominator, sheetBefore.scaleDenominator)))
				{
					SheetChange back;
					back.scale = std::make_pair(sheetBefore.scaleNumerator, sheetBefore.scaleDenominator);
					ChangeActiveSheet(drawing, back, now);
				}
			}
			else
				ApplyViewChange(doc, drawing, ext, view, ViewChange::Back(before, HasOwnOrientation(view)), true);
			doc->ClearSelection2(VARIANT_TRUE);
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			return failure;
		};
		if (grpc::Status status = ApplyViewChange(doc, drawing, ext, view, change, false); !status.ok())
			return undo(status);
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		doc->GraphicsRedraw2();
		finalName = ViewName(view);
	}

	for (const auto& s : DrawingSheets(drawing))
		if (s.name == sheetName)
		{
			DrawingBudget budget;
			DescribeSheet(s, original, ext, math, {}, false, false, budget, response->mutable_sheet());
		}
	response->set_view_name(finalName);
	response->set_created(create);
	response->set_active_sheet(original);
	return grpc::Status::OK;
}
