// SheetFormats.cpp : Sheet formats: DrawSheetFormat (lines and notes on a layer of a sheet's format:
// borders, title blocks; see docs/notes/drawings.md, "Borders and title blocks"). The properties the
// title block shows: Properties.cpp.
#include "pch.h"
#include "Common.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxFormatLines = 5000;
	constexpr int kMaxFormatNotes = 1000;

	std::string LayerOf(IAnnotation* annotation)
	{
		ATL::CComBSTR layer;
		annotation->get_Layer(&layer);
		return ToUtf8(layer);
	}

	std::vector<ATL::CComPtr<ISketchSegment>> FormatSegments(ISheet* sheet)
	{
		std::vector<ATL::CComPtr<ISketchSegment>> out;
		ATL::CComPtr<ISketch> sketch;
		ATL::CComVariant segments;
		if (SUCCEEDED(sheet->GetTemplateSketch(&sketch)) && sketch && SUCCEEDED(sketch->GetSketchSegments(&segments)))
			for (const auto& disp : comvariant::ToDispatches(segments))
				if (ATL::CComQIPtr<ISketchSegment> segment(disp.p); segment)
					out.push_back(ATL::CComPtr<ISketchSegment>(segment));
		return out;
	}

	std::vector<ATL::CComPtr<IAnnotation>> FormatAnnotations(const DrawingSheet& sheet)
	{
		std::vector<ATL::CComPtr<IAnnotation>> out;
		for (const auto& annotation : ViewAnnotationList(sheet.sheetView))
			if (AnnotationOwnerOf(annotation, nullptr) == sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT)
				out.push_back(annotation);
		return out;
	}

	// Selects everything given (segments with Select4, annotations with
	// Select3 / Select2) and deletes the selection.
	bool DeleteItems(IModelDoc2* doc, IModelDocExtension* ext, const std::vector<ATL::CComPtr<ISketchSegment>>& segments,
		const std::vector<ATL::CComPtr<IAnnotation>>& annotations)
	{
		if (segments.empty() && annotations.empty())
			return true;
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
		for (const auto& segment : segments)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (SUCCEEDED(segment->Select4(VARIANT_TRUE, nullptr, &selected)) && selected)
				any = true;
		}
		for (const auto& annotation : annotations)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if ((SUCCEEDED(annotation->Select3(VARIANT_TRUE, nullptr, &selected)) && selected) ||
				(SUCCEEDED(annotation->Select2(VARIANT_TRUE, 0, &selected)) && selected))
				any = true;
		}
		VARIANT_BOOL deleted = VARIANT_FALSE;
		const bool ok = any && SUCCEEDED(ext->DeleteSelection2(0, &deleted)) && deleted;
		doc->ClearSelection2(VARIANT_TRUE);
		return ok;
	}

	grpc::Status ValidateFormat(const sld_mcp::v1::DrawSheetFormatRequest& r)
	{
		if (r.layer().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which layer to draw on");
		if (r.lines_size() > kMaxFormatLines || r.notes_size() > kMaxFormatNotes)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "at most " + std::to_string(kMaxFormatLines) +
				" lines and " + std::to_string(kMaxFormatNotes) + " notes per call");
		if (r.layer_weight() < swLW_THIN || r.layer_weight() > swLW_THICK6)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown line weight " + std::to_string(r.layer_weight()));
		for (int i = 0; i < r.lines_size(); ++i)
		{
			const auto& l = r.lines(i);
			if (!IsFinite(l.start()) || !IsFinite(l.end()))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "line " + std::to_string(i + 1) + " must be finite");
			if (std::hypot(l.end().x() - l.start().x(), l.end().y() - l.start().y()) < 1e-6)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "line " + std::to_string(i + 1) + " has no length");
		}
		for (int i = 0; i < r.notes_size(); ++i)
		{
			const auto& n = r.notes(i);
			const std::string which = "note " + std::to_string(i + 1);
			if (n.text().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + " has no text");
			if (!IsFinite(n.position()) || !std::isfinite(n.angle()))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + "'s position must be finite");
			if (!(n.text_height() >= 0.0 && n.text_height() < 1.0))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + "'s text height must be meters above 0");
			if (n.justification() < 0 || n.justification() > swTextJustificationRight)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + ": unknown justification " +
					std::to_string(n.justification()));
		}
		return grpc::Status::OK;
	}

	// The note's look and place; false (with `problem`) at the first
	// thing SolidWorks doesn't take.
	bool StyleNote(INote* note, IAnnotation* annotation, const sld_mcp::v1::FormatNote& n, const std::string& layer,
		std::string& problem)
	{
		if (n.text_height() > 0.0 || n.bold() || !n.font().empty())
		{
			ATL::CComPtr<IDispatch> formatDisp;
			ATL::CComQIPtr<ITextFormat> format;
			if (SUCCEEDED(annotation->GetTextFormat(0, &formatDisp)) && formatDisp)
				format = formatDisp;
			if (!format)
			{
				problem = "SolidWorks gave no text format";
				return false;
			}
			if (n.text_height() > 0.0)
				format->put_CharHeight(n.text_height());
			if (n.bold())
				format->put_Bold(VARIANT_TRUE);
			if (!n.font().empty())
				format->put_TypeFaceName(FromUtf8(n.font()));
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(annotation->SetTextFormat(0, VARIANT_FALSE, format, &ok)) || !ok)
			{
				problem = "SolidWorks didn't take the text format";
				return false;
			}
		}
		if (n.justification() > 0)
			note->SetTextJustification(n.justification());
		if (n.angle() != 0.0)
			note->put_Angle(n.angle());
		annotation->put_Layer(FromUtf8(layer));
		VARIANT_BOOL ok = VARIANT_FALSE;
		if (FAILED(annotation->SetPosition2(n.position().x(), n.position().y(), 0.0, &ok)) || !ok)
		{
			problem = "SolidWorks didn't place it";
			return false;
		}
		return true;
	}
}

grpc::Status SolidWorksService::DrawSheetFormat(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::DrawSheetFormatRequest* request,
	sld_mcp::v1::DrawSheetFormatResponse* response)
{
	if (grpc::Status status = ValidateFormat(*request); !status.ok())
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
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;

	const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
	const std::string original = ActiveSheetName(drawing);
	const std::string& wanted = request->sheet_name().empty() ? original : request->sheet_name();
	const DrawingSheet* sheet = FindSheet(sheets, wanted);
	if (!sheet)
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			"the drawing has no sheet named \"" + wanted + "\" (its sheets: " + SheetList(sheets) + ")");
	const std::string sheetName = sheet->name;
	const std::string& layer = request->layer();

	ATL::CComPtr<IDispatch> layerMgrDisp;
	ATL::CComQIPtr<ILayerMgr> layers;
	if (SUCCEEDED(doc->GetLayerManager(&layerMgrDisp)) && layerMgrDisp)
		layers = layerMgrDisp;
	if (!layers)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave no layer manager");

	std::vector<ATL::CComPtr<ISketchSegment>> addedLines;
	std::vector<ATL::CComPtr<IAnnotation>> addedNotes;
	{
		SheetActivation activation(drawing);
		if (!activation.Show(sheetName))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheetName);

		// The layer, made if missing; AddLayer makes the new layer the
		// current one, which is put back after.
		ATL::CComBSTR current;
		layers->GetCurrentLayer(&current);
		ATL::CComPtr<IDispatch> existing;
		if (FAILED(layers->GetLayer(FromUtf8(layer), &existing)) || !existing)
		{
			int added = 0;
			if (FAILED(layers->AddLayer(FromUtf8(layer), ATL::CComBSTR(L""), request->layer_color(), swLineCONTINUOUS,
				request->layer_weight(), &added)) || !added)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks wouldn't make a layer \"" + layer + "\"");
			response->set_layer_created(true);
			int ok = 0;
			layers->SetCurrentLayer(current, &ok);
		}

		EditModeScope editMode(drawing);
		editMode.Set(true);

		if (request->clear())
		{
			std::vector<ATL::CComPtr<ISketchSegment>> segments;
			for (const auto& segment : FormatSegments(sheet->sheet))
			{
				ATL::CComBSTR l;
				if (SUCCEEDED(segment->get_Layer(&l)) && ToUtf8(l) == layer)
					segments.push_back(segment);
			}
			std::vector<ATL::CComPtr<IAnnotation>> notes;
			for (const auto& annotation : FormatAnnotations(*sheet))
				if (LayerOf(annotation) == layer)
					notes.push_back(annotation);
			if (!DeleteItems(doc, ext, segments, notes))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't delete what layer " + layer +
					" had in " + sheetName + "'s format");
			response->set_lines_deleted(static_cast<int32_t>(segments.size()));
			response->set_notes_deleted(static_cast<int32_t>(notes.size()));
		}

		grpc::Status status = grpc::Status::OK;
		ATL::CComPtr<ISketchManager> sketches;
		if (FAILED(doc->get_SketchManager(&sketches)) || !sketches)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave no sketch manager");
		// Straight into the database: no snapping to (or relations with) what
		// is already drawn.
		sketches->put_AddToDB(VARIANT_TRUE);
		sketches->put_DisplayWhenAdded(VARIANT_FALSE);
		doc->ClearSelection2(VARIANT_TRUE);
		for (int i = 0; i < request->lines_size() && status.ok(); ++i)
		{
			const auto& l = request->lines(i);
			ATL::CComPtr<ISketchSegment> segment;
			if (FAILED(sketches->CreateLine(l.start().x(), l.start().y(), 0.0, l.end().x(), l.end().y(), 0.0, &segment)) || !segment)
			{
				status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't draw line " + std::to_string(i + 1));
				break;
			}
			addedLines.push_back(segment);
			segment->put_Layer(FromUtf8(layer));
		}
		sketches->put_AddToDB(VARIANT_FALSE);
		sketches->put_DisplayWhenAdded(VARIANT_TRUE);
		doc->ClearSelection2(VARIANT_TRUE);

		std::vector<IDispatch*> fields;
		std::vector<ATL::CComPtr<INote>> keep;
		for (int i = 0; i < request->notes_size() && status.ok(); ++i)
		{
			const auto& n = request->notes(i);
			const std::string which = "note " + std::to_string(i + 1);
			// While the format is being edited, a new note goes into it
			// (nothing selected, or it'd get a leader).
			doc->ClearSelection2(VARIANT_TRUE);
			ATL::CComPtr<IDispatch> noteDisp;
			ATL::CComQIPtr<INote> note;
			if (SUCCEEDED(doc->InsertNote(FromUtf8(n.text()), &noteDisp)) && noteDisp)
				note = noteDisp;
			ATL::CComPtr<IDispatch> annotationDisp;
			ATL::CComQIPtr<IAnnotation> annotation;
			if (note && SUCCEEDED(note->GetAnnotation(&annotationDisp)) && annotationDisp)
				annotation = annotationDisp;
			if (!annotation)
			{
				status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add " + which);
				break;
			}
			addedNotes.push_back(ATL::CComPtr<IAnnotation>(annotation));
			if (AnnotationOwnerOf(annotation, nullptr) != sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT)
			{
				status = grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks put " + which + " outside the sheet format");
				break;
			}
			std::string problem;
			if (!StyleNote(note, annotation, n, layer, problem))
			{
				status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, which + ": " + problem);
				break;
			}
			keep.push_back(ATL::CComPtr<INote>(note));
			if (n.field())
				fields.push_back(noteDisp);
		}
		doc->ClearSelection2(VARIANT_TRUE);
		if (!status.ok())
		{
			DeleteItems(doc, ext, addedLines, addedNotes);
			{ VARIANT_BOOL rebuilt = VARIANT_FALSE; doc->ForceRebuild3(VARIANT_FALSE, &rebuilt); }
			return status;
		}
		response->set_lines_added(static_cast<int32_t>(addedLines.size()));

		// Title block fields: best effort, the drawing is fine without them.
		if (!fields.empty())
		{
			ATL::CComPtr<ITitleBlock> block;
			if (SUCCEEDED(sheet->sheet->get_TitleBlock(&block)) && block)
				response->set_fields_problem("the sheet format already has title block fields (SolidWorks keeps one set; "
					"define them again in Edit Sheet Format > Title Block Fields)");
			else
			{
				block.Release();
				HRESULT hr = sheet->sheet->InsertTitleBlock(comvariant::FromDispatches(fields), &block);
				long count = 0;
				if (SUCCEEDED(hr) && block && SUCCEEDED(block->GetNoteCount(&count)) && count > 0)
					response->set_fields_defined(static_cast<int32_t>(count));
				else
					response->set_fields_problem("SolidWorks didn't make them title block fields (ISheet::InsertTitleBlock " +
						std::string(SUCCEEDED(hr) ? "returned nothing" : "failed") + ")");
			}
		}
	}
	// After editing the format, SolidWorks leaves the views out of the
	// printed sheet until a rebuild (seen 2026-10-22).
	{ VARIANT_BOOL rebuilt = VARIANT_FALSE; doc->ForceRebuild3(VARIANT_FALSE, &rebuilt); }
	doc->GraphicsRedraw2();

	PersistIds ids(ext);
	for (const auto& annotation : addedNotes)
		DescribeAnnotation(annotation, ids, nullptr, response->add_notes());
	for (const auto& s : DrawingSheets(drawing))
		if (s.name == sheetName)
		{
			DrawingBudget budget;
			DescribeSheet(s, original, ext, math, {}, false, false, budget, response->mutable_sheet());
			DescribeSheetLayout(s, response->mutable_sheet());
		}
	response->set_active_sheet(original);
	return grpc::Status::OK;
}
