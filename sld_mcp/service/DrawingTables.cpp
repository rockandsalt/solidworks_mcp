// DrawingTables.cpp : Drawing tables and balloons: SetupDrawingTable (add a BOM, hole table, general or
// revision table, change a table), AddBalloons (see docs/notes/drawings.md, "Tables and balloons").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxTableRows = 500;
	constexpr int kMaxTableColumns = 50;
	constexpr int kMaxTableEdits = 5000;
	constexpr int kMaxHoles = 500;
	constexpr int kMaxBalloons = 200;

	// Where a new table goes when no position is given: the sheet's upper
	// left, inside a 20 mm margin. The MCP server moves it to a free spot.
	constexpr double kDefaultTableMargin = 0.020;

	std::string AnnotationName(IAnnotation* annotation)
	{
		ATL::CComBSTR name;
		if (annotation)
			annotation->GetName(&name);
		return ToUtf8(name);
	}

	std::string TableName(ITableAnnotation* table)
	{
		ATL::CComPtr<IAnnotation> annotation;
		if (FAILED(table->GetAnnotation(&annotation)))
			return {};
		return AnnotationName(annotation);
	}

	const char* KindWord(sld_mcp::v1::TableKind kind)
	{
		switch (kind)
		{
		case sld_mcp::v1::TABLE_KIND_BOM:
			return "bill of materials";
		case sld_mcp::v1::TABLE_KIND_HOLE:
			return "hole table";
		case sld_mcp::v1::TABLE_KIND_REVISION:
			return "revision table";
		default:
			return "table";
		}
	}

	struct FoundTable
	{
		ATL::CComPtr<ITableAnnotation> table;
		std::string sheetName;
	};

	grpc::Status FindTable(const std::vector<DrawingSheet>& sheets, const std::string& name, const std::string& sheetName,
		FoundTable& out)
	{
		std::vector<FoundTable> hits;
		for (const auto& sheet : sheets)
		{
			if (!sheetName.empty() && !EqualsIgnoringCase(sheet.name, sheetName))
				continue;
			for (const auto& table : SheetTables(sheet))
				if (TableName(table) == name)
					hits.push_back({ table, sheet.name });
		}
		if (hits.empty())
			return grpc::Status(grpc::StatusCode::NOT_FOUND, "the drawing has no table named \"" + name + "\"" +
				(sheetName.empty() ? "" : " on " + sheetName) + " -- inspect_drawing_tables lists them");
		if (hits.size() > 1)
		{
			std::string where;
			for (const auto& hit : hits)
				where += (where.empty() ? "" : ", ") + hit.sheetName;
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "tables on several sheets are named \"" + name +
				"\" (" + where + ") -- say which sheet");
		}
		out = hits[0];
		return grpc::Status::OK;
	}

	bool DeleteTable(IModelDoc2* doc, IModelDocExtension* ext, ITableAnnotation* table)
	{
		ATL::CComPtr<IAnnotation> annotation;
		if (FAILED(table->GetAnnotation(&annotation)) || !annotation)
			return false;
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE, deleted = VARIANT_FALSE;
		const bool ok = SUCCEEDED(annotation->Select2(VARIANT_FALSE, 0, &selected)) && selected &&
			SUCCEEDED(ext->DeleteSelection2(0, &deleted)) && deleted;
		doc->ClearSelection2(VARIANT_TRUE);
		return ok;
	}

	bool DeleteNotes(IModelDoc2* doc, IModelDocExtension* ext, const std::vector<ATL::CComPtr<IAnnotation>>& annotations)
	{
		if (annotations.empty())
			return true;
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
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

	std::string CellText(ITableAnnotation* table, long row, long column)
	{
		ATL::CComBSTR text;
		table->get_Text2(row, column, VARIANT_TRUE, &text);
		return ToUtf8(text);
	}

	// Annotations of a view that weren't in `before` (by COM identity).
	std::vector<ATL::CComPtr<IAnnotation>> NewAnnotations(IView* view, const std::vector<ATL::CComPtr<IAnnotation>>& before)
	{
		std::vector<ATL::CComPtr<IAnnotation>> out;
		for (const auto& annotation : ViewAnnotationList(view))
			if (std::none_of(before.begin(), before.end(), [&](const ATL::CComPtr<IAnnotation>& b) {
				return SameObject(b, annotation); }))
				out.push_back(annotation);
		return out;
	}

	grpc::Status ValidateTableRequest(const sld_mcp::v1::SetupDrawingTableRequest& r)
	{
		const bool create = r.name().empty();
		if (create && r.kind() == sld_mcp::v1::TABLE_KIND_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which kind of table to add (bom, hole, general)");
		if (!create && r.kind() != sld_mcp::v1::TABLE_KIND_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a table's kind can't be changed");
		const bool ofView = r.kind() == sld_mcp::v1::TABLE_KIND_BOM || r.kind() == sld_mcp::v1::TABLE_KIND_HOLE;
		if (create && ofView && r.view_name().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, std::string("say which view the ") +
				KindWord(r.kind()) + " is of");
		if (create && r.kind() == sld_mcp::v1::TABLE_KIND_GENERAL &&
			(r.rows() < 1 || r.rows() > kMaxTableRows || r.columns() < 1 || r.columns() > kMaxTableColumns))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a general table needs 1.." +
				std::to_string(kMaxTableRows) + " rows and 1.." + std::to_string(kMaxTableColumns) + " columns");
		if (create && r.kind() == sld_mcp::v1::TABLE_KIND_HOLE &&
			(r.hole_edges_size() == 0 || r.hole_edges_size() > kMaxHoles || r.hole_origin().view_name().empty()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a hole table needs an origin vertex and 1.." + std::to_string(kMaxHoles) + " hole edges");
		if (create && r.kind() == sld_mcp::v1::TABLE_KIND_HOLE)
		{
			if (r.hole_origin().view_name() != r.view_name())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the hole table's origin must be in " + r.view_name());
			for (const auto& ref : r.hole_edges())
				if (ref.view_name() != r.view_name())
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the hole edges must all be in " + r.view_name());
		}
		if (r.kind() == sld_mcp::v1::TABLE_KIND_BOM && (r.bom_type() < 0 || r.bom_type() > swBomType_Indented))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown BOM type " + std::to_string(r.bom_type()));
		if (create && r.kind() != sld_mcp::v1::TABLE_KIND_REVISION && r.revisions_size() > 0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "revisions go in a revision table");
		if (r.revision_symbol() < 0 || r.revision_symbol() > swRevisionTable_HexagonSymbol)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown revision symbol " + std::to_string(r.revision_symbol()));
		if (r.anchor() < 0 || r.anchor() > swBOMConfigurationAnchor_BottomRight)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown anchor " + std::to_string(r.anchor()));
		if (r.has_position() && (!std::isfinite(r.position().x()) || !std::isfinite(r.position().y())))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the position must be finite");
		if (r.cells_size() > kMaxTableEdits || r.add_rows_size() > kMaxTableRows || r.delete_rows_size() > kMaxTableRows ||
			r.revisions_size() > kMaxTableRows)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many cells or rows in one call");
		for (const auto& w : r.column_widths())
			if (!std::isfinite(w) || w < 0.0 || w > 1.0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "column widths are meters above 0 (0 = keep)");
		return grpc::Status::OK;
	}

	// What a change may need to put back.
	struct TableState
	{
		std::map<std::pair<long, long>, std::string> cells;
		std::vector<double> widths;
		std::string title;
		bool titleVisible = false;
		std::string layer;
		std::vector<double> position;
		long anchor = 0;
	};

	TableState ReadTableState(ITableAnnotation* table, IAnnotation* annotation, const sld_mcp::v1::SetupDrawingTableRequest& r)
	{
		TableState s;
		const long t = TitleRows(table);
		for (const auto& cell : r.cells())
			s.cells.emplace(std::make_pair(cell.row(), cell.column()), CellText(table, cell.row() + t, cell.column()));
		long columns = 0;
		table->get_TotalColumnCount(&columns);
		for (long c = 0; c < columns; ++c)
		{
			double w = 0.0;
			table->GetColumnWidth(c, &w);
			s.widths.push_back(w);
		}
		ATL::CComBSTR title;
		VARIANT_BOOL visible = VARIANT_FALSE;
		table->get_Title(&title);
		table->get_TitleVisible(&visible);
		s.title = ToUtf8(title);
		s.titleVisible = visible != VARIANT_FALSE;
		ATL::CComBSTR layer;
		annotation->get_Layer(&layer);
		s.layer = ToUtf8(layer);
		ATL::CComVariant position;
		if (SUCCEEDED(annotation->GetPosition(&position)))
			s.position = comvariant::ToDoubles(position);
		table->get_AnchorType(&s.anchor);
		return s;
	}

	void RestoreTableState(ITableAnnotation* table, IAnnotation* annotation, const TableState& s)
	{
		const long t = TitleRows(table);
		for (const auto& [at, text] : s.cells)
			table->put_Text2(at.first + t, at.second, VARIANT_TRUE, FromUtf8(text));
		for (size_t c = 0; c < s.widths.size(); ++c)
		{
			double result = 0.0;
			table->SetColumnWidth(static_cast<long>(c), s.widths[c], swTableRowColChange_TableSizeCanChange, &result);
		}
		table->put_Title(FromUtf8(s.title));
		table->put_TitleVisible(s.titleVisible ? VARIANT_TRUE : VARIANT_FALSE);
		annotation->put_Layer(FromUtf8(s.layer));
		table->put_AnchorType(s.anchor);
		if (s.position.size() >= 3)
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			annotation->SetPosition2(s.position[0], s.position[1], s.position[2], &ok);
		}
	}

	// The first column of a revision table of swTableColumnTypes_e `type`, -1 if none.
	long RevisionColumn(ITableAnnotation* table, long type)
	{
		long columns = 0;
		table->get_TotalColumnCount(&columns);
		for (long c = 0; c < columns; ++c)
		{
			ATL::CComVariant propertyData;
			long status = 0, now = 0;
			if (SUCCEEDED(table->GetColumnType3(c, VARIANT_TRUE, &propertyData, &status, &now)) && now == type)
				return c;
		}
		return -1;
	}

	// Adds a revision at the bottom, its cells and its symbols; `added` gets
	// its id as soon as there is one, to take it back on a later failure.
	grpc::Status AddRevisionRow(IDrawingDoc* drawing, ITableAnnotation* table, IRevisionTableAnnotation* revisions,
		const sld_mcp::v1::RevisionEntry& entry, std::vector<long>& added)
	{
		const std::string name = TableName(table);
		long id = -1;
		// Typed in: the row shows it, but SolidWorks' count goes on by its own
		// letters (seen 2026-11-01).
		if (FAILED(revisions->AddRevision(FromUtf8(entry.revision()), &id)) || id <= 0)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add a revision to " + name);
		added.push_back(id);
		long row = -1;
		if (FAILED(revisions->GetRowNumberForId(id, &row)) || row < 0)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add the revision's row to " + name);
		const std::pair<const std::string*, long> texts[] = {
			{ &entry.zone(), swRevisionTableColumnType_Zone },
			{ &entry.description(), swRevisionTableColumnType_Description },
			{ &entry.date(), swRevisionTableColumnType_Date },
			{ &entry.approved(), swRevisionTableColumnType_Approved },
		};
		for (const auto& [text, type] : texts)
		{
			if (text->empty())
				continue;
			const long column = RevisionColumn(table, type);
			if (column < 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, name + " has no column for \"" + *text +
					"\" (its template leaves that column out)");
			table->put_Text2(row, column, VARIANT_TRUE, FromUtf8(*text));
			if (CellText(table, row, column) != *text)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't take \"" + *text + "\" in " + name);
		}
		// Each shows the latest revision, this one.
		for (const auto& at : entry.symbols())
		{
			ATL::CComPtr<INote> symbol;
			if (FAILED(drawing->InsertRevisionSymbol(at.x(), at.y(), &symbol)) || !symbol)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't place the revision's symbol");
		}
		return grpc::Status::OK;
	}

	// Applies the edits of `r` to a table, each read back; stops at the
	// first that fails. Rows and revisions are added, rows deleted, then
	// cells set, widths, title, layer, anchor, position.
	grpc::Status ApplyTableChange(ITableAnnotation* table, IAnnotation* annotation, IModelDoc2* doc, IDrawingDoc* drawing,
		const sld_mcp::v1::SetupDrawingTableRequest& r, bool create, std::vector<long>& addedRevisions)
	{
		long type = 0;
		table->get_Type(&type);
		const bool generated = type == swTableAnnotation_BillOfMaterials || type == swTableAnnotation_HoleChart;
		long rows = 0, columns = 0;
		table->get_TotalRowCount(&rows);
		table->get_TotalColumnCount(&columns);
		const std::string name = TableName(table);
		// Indices on the wire leave a shown title's row out.
		const long t = TitleRows(table);
		rows -= t;
		ATL::CComQIPtr<IRevisionTableAnnotation> revisions(table);

		if (generated && (r.add_rows_size() > 0 || r.delete_rows_size() > 0))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, name +
				"'s rows come from the model (a BOM's components, a hole table's holes): they can't be added or deleted");
		if (revisions && r.add_rows_size() > 0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, name + " is a revision table: add revisions, not plain rows");
		if (!revisions && r.revisions_size() > 0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, name + " isn't a revision table");
		// Row indices are checked against the table as it is before
		// anything changes; cells against the table after the rows.
		std::set<long> doomed;
		for (const auto& row : r.delete_rows())
		{
			if (row < 0 || row >= rows)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "row " + std::to_string(row) + " isn't in " + name +
					" (rows 0.." + std::to_string(rows - 1) + ")");
			doomed.insert(row);
		}
		// DeleteRevision of a header row's id, 0, deletes the title row (seen
		// 2026-11-01), so the ids are looked up and checked first.
		std::map<long, long> revisionIds;
		if (revisions)
			for (const long row : doomed)
			{
				long id = 0;
				revisions->GetIdForRowNumber(row + t, &id);
				if (id <= 0)
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "row " + std::to_string(row) + " of " + name +
						" is its header, not a revision");
				revisionIds[row] = id;
			}
		if (!doomed.empty() && static_cast<long>(doomed.size()) >= rows + r.add_rows_size() + r.revisions_size())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a table keeps at least one row: delete the table instead");
		for (const auto& row : r.add_rows())
			if (row.cells_size() > columns)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a new row has " + std::to_string(row.cells_size()) +
					" cells; " + name + " has " + std::to_string(columns) + " columns");
		const long rowsAfter = rows + r.add_rows_size() + r.revisions_size() - static_cast<long>(doomed.size());
		for (const auto& cell : r.cells())
			if (cell.row() < 0 || cell.row() >= rowsAfter || cell.column() < 0 || cell.column() >= columns)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "cell [" + std::to_string(cell.row()) + ", " +
					std::to_string(cell.column()) + "] isn't in " + name + " (rows 0.." + std::to_string(rowsAfter - 1) +
					", columns 0.." + std::to_string(columns - 1) + ")");
		if (r.column_widths_size() > columns)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, std::to_string(r.column_widths_size()) +
				" column widths for " + std::to_string(columns) + " columns");
		// Cells SolidWorks fills itself (a BOM's item numbers, quantities,
		// part numbers; a hole table's locations) are refused up front where
		// it says so.
		if (r.add_rows_size() == 0 && r.delete_rows_size() == 0 && r.revisions_size() == 0)
			for (const auto& cell : r.cells())
			{
				VARIANT_BOOL editable = VARIANT_TRUE;
				if (SUCCEEDED(table->IsCellTextEditable(cell.row() + t, cell.column(), &editable)) && !editable)
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "cell [" + std::to_string(cell.row()) + ", " +
						std::to_string(cell.column()) + "] of " + name + " is filled by SolidWorks (\"" +
						CellText(table, cell.row() + t, cell.column()) + "\") and can't be typed over");
			}

		// Rows: added at the bottom, then the doomed ones deleted bottom up
		// (their indices are below the new ones).
		for (int i = 0; i < r.add_rows_size(); ++i)
		{
			long count = 0;
			table->get_TotalRowCount(&count);
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(table->InsertRow(swTableItemInsertPosition_Last, count - 1, &ok)) || !ok)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add a row to " + name);
			const auto& row = r.add_rows(i);
			for (int c = 0; c < row.cells_size(); ++c)
				table->put_Text2(count, c, VARIANT_TRUE, FromUtf8(row.cells(c)));
		}
		for (const auto& entry : r.revisions())
			if (grpc::Status status = AddRevisionRow(drawing, table, revisions, entry, addedRevisions); !status.ok())
				return status;
		for (auto it = doomed.rbegin(); it != doomed.rend(); ++it)
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (revisions)
			{
				if (FAILED(revisions->DeleteRevision(revisionIds[*it], VARIANT_TRUE, &ok)) || !ok)
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't delete row " +
						std::to_string(*it) + " of " + name);
			}
			else if (FAILED(table->DeleteRow2(*it + t, VARIANT_TRUE, &ok)) || !ok)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't delete row " +
					std::to_string(*it) + " of " + name);
		}
		for (const auto& cell : r.cells())
		{
			table->put_Text2(cell.row() + t, cell.column(), VARIANT_TRUE, FromUtf8(cell.text()));
			if (CellText(table, cell.row() + t, cell.column()) != cell.text())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks didn't take the text of cell [" +
					std::to_string(cell.row()) + ", " + std::to_string(cell.column()) + "] of " + name + " (it shows \"" +
					CellText(table, cell.row() + t, cell.column()) + "\")");
		}
		for (int c = 0; c < r.column_widths_size(); ++c)
		{
			if (r.column_widths(c) <= 0.0)
				continue;
			double result = 0.0;
			table->SetColumnWidth(c, r.column_widths(c), swTableRowColChange_TableSizeCanChange, &result);
			double now = 0.0;
			table->GetColumnWidth(c, &now);
			if (std::abs(now - r.column_widths(c)) > 1e-5)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks made column " + std::to_string(c) +
					" of " + name + " " + std::to_string(now * 1000.0) + " mm wide instead of " +
					std::to_string(r.column_widths(c) * 1000.0) + " (too narrow for its text?)");
		}
		if (r.has_title())
		{
			if (!r.title().empty())
				table->put_Title(FromUtf8(r.title()));
			table->put_TitleVisible(r.title().empty() ? VARIANT_FALSE : VARIANT_TRUE);
		}
		if (r.has_layer())
		{
			annotation->put_Layer(FromUtf8(r.layer()));
			ATL::CComBSTR now;
			annotation->get_Layer(&now);
			if (ToUtf8(now) != r.layer())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the drawing has no layer \"" + r.layer() + "\"");
		}
		// (InsertRevisionTable2 keeps its template's corner, top right, whatever
		// it's told; seen 2026-11-01.)
		if (r.anchor() > 0 && (!create || revisions))
			table->put_AnchorType(r.anchor());
		if (r.has_position())
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(annotation->SetPosition2(r.position().x(), r.position().y(), 0.0, &ok)) || !ok)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't move " + name);
		}
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::SetupDrawingTable(grpc::ServerContext* context,
	const sld_mcp::v1::SetupDrawingTableRequest* request,
	sld_mcp::v1::SetupDrawingTableResponse* response)
{
	if (grpc::Status status = ValidateTableRequest(*request); !status.ok())
		return status;
	const bool create = request->name().empty();

	return m_executor.Execute(context, "SetupDrawingTable", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
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
		std::string sheetName;
		ATL::CComPtr<IView> view;
		FoundTable found;
		std::wstring templatePath;
		const auto kind = request->kind();
		if (create)
		{
			if (kind == sld_mcp::v1::TABLE_KIND_GENERAL || kind == sld_mcp::v1::TABLE_KIND_REVISION)
			{
				const std::string& wanted = request->sheet_name().empty() ? original : request->sheet_name();
				const DrawingSheet* sheet = FindSheet(sheets, wanted);
				if (!sheet)
					return grpc::Status(grpc::StatusCode::NOT_FOUND,
						"the drawing has no sheet named \"" + wanted + "\" (its sheets: " + SheetList(sheets) + ")");
				sheetName = sheet->name;
			}
			else
			{
				if (grpc::Status status = FindDrawingView(sheets, request->view_name(), view, sheetName); !status.ok())
					return status;
				if (!request->sheet_name().empty() && !EqualsIgnoringCase(request->sheet_name(), sheetName))
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, request->view_name() + " is on " + sheetName +
						", not " + request->sheet_name());
			}
			if (!request->template_file().empty() || kind != sld_mcp::v1::TABLE_KIND_GENERAL)
			{
				const auto pref = kind == sld_mcp::v1::TABLE_KIND_BOM ? swFileLocationsBOMTemplates
					: kind == sld_mcp::v1::TABLE_KIND_HOLE ? swFileLocationsHoleTableTemplates
					: kind == sld_mcp::v1::TABLE_KIND_REVISION ? swFileLocationsRevisionTableTemplates : swFileLocationsGeneralTablesTemplate;
				const std::wstring extension = kind == sld_mcp::v1::TABLE_KIND_BOM ? L".sldbomtbt"
					: kind == sld_mcp::v1::TABLE_KIND_HOLE ? L".sldholtbt"
					: kind == sld_mcp::v1::TABLE_KIND_REVISION ? L".sldrevtbt" : L".sldtbt";
				const std::string name = !request->template_file().empty() ? request->template_file()
					: kind == sld_mcp::v1::TABLE_KIND_BOM ? "bom-standard"
					: kind == sld_mcp::v1::TABLE_KIND_REVISION ? "standard revision block" : "standard hole table--letters";
				if (grpc::Status status = FindTemplateFile(swApp, pref, name, extension, std::string(KindWord(kind)) + " template",
					templatePath); !status.ok())
					return status;
			}
		}
		else
		{
			if (grpc::Status status = FindTable(sheets, request->name(), request->sheet_name(), found); !status.ok())
				return status;
			sheetName = found.sheetName;
		}

		// Unless each sheet's table stands alone, a later sheet's says "See
		// Sheet1" and AddRevision adds to the first sheet's, or fails without
		// one (seen 2026-11-01).
		if (request->revisions_size() > 0 && !sheets.empty() && sheetName != sheets[0].name)
		{
			long style = swRevisionTable_SeeSheet1;
			ext->GetUserPreferenceInteger(swRevisionTableMultipleSheetStyle, swDetailingNoOptionSpecified, &style);
			if (style != swRevisionTable_Independent)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "this drawing keeps its revisions on the first sheet, " +
					sheets[0].name + " (a later sheet's table only says \"See Sheet1\"): add them to that sheet's table");
		}

		std::unique_ptr<DrawingViewContext> ctx;
		google::protobuf::RepeatedPtrField<sld_mcp::v1::ComponentInfo> holeComponents;
		std::vector<ATL::CComPtr<IAnnotation>> viewBefore;
		ATL::CComPtr<ITableAnnotation> table;
		ATL::CComPtr<IAnnotation> annotation;
		{
			SheetActivation activation(drawing);
			if (!activation.Show(sheetName))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheetName);
			// Tables go on the sheet, never into its format.
			EditModeScope editMode(drawing);
			editMode.Set(false);

			if (create)
			{
				SheetSetup setup;
				if (const DrawingSheet* sheet = FindSheet(sheets, sheetName); sheet)
					ReadSheetSetup(sheet->sheet, setup);
				const long anchor = request->anchor() > 0 ? request->anchor()
					: kind == sld_mcp::v1::TABLE_KIND_REVISION ? swBOMConfigurationAnchor_TopRight : swBOMConfigurationAnchor_TopLeft;
				const double x = request->has_position() ? request->position().x()
					: kind == sld_mcp::v1::TABLE_KIND_REVISION ? std::max(setup.width - kDefaultTableMargin, kDefaultTableMargin) : kDefaultTableMargin;
				const double y = request->has_position() ? request->position().y()
					: std::max(setup.height - kDefaultTableMargin, kDefaultTableMargin);
				const ATL::CComBSTR templateBstr(templatePath.c_str());
				doc->ClearSelection2(VARIANT_TRUE);
				if (kind == sld_mcp::v1::TABLE_KIND_BOM)
				{
					const ViewModel model = ViewModelOf(view);
					if (model.type != swDocASSEMBLY)
						return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, request->view_name() +
							" doesn't show an assembly: a bill of materials lists an assembly's components");
					// An empty configuration gives a BOM of only its header
					// (seen 2026-10-22): the view's own is named.
					ATL::CComBSTR configuration(FromUtf8(request->configuration()));
					if (configuration.Length() == 0)
						view->get_ReferencedConfiguration(&configuration);
					VARIANT_BOOL ok = VARIANT_FALSE;
					drawing->ActivateView(FromUtf8(request->view_name()), &ok);
					const long bomType = request->bom_type() > 0 ? request->bom_type() : swBomType_TopLevelOnly;
					ATL::CComPtr<IDispatch> made;
					HRESULT hr = view->InsertBomTable6(VARIANT_FALSE, x, y, anchor, bomType, configuration, templateBstr,
						VARIANT_FALSE, bomType == swBomType_Indented ? swNumberingType_Detailed : swNumberingType_None,
						VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, &made);
					if (FAILED(hr))
						return ComError("IView::InsertBomTable6", hr);
					table = ATL::CComQIPtr<ITableAnnotation>(made.p);
					if (!table)
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add a bill of materials for " +
							request->view_name() + " (configuration \"" + ToUtf8(configuration) + "\")");
					long rows = 0, header = 0;
					table->get_TotalRowCount(&rows);
					table->GetHeaderCount(&header);
					if (rows <= header)
					{
						DeleteTable(doc, ext, table);
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks made an empty bill of materials for " +
							request->view_name() + " (configuration \"" + ToUtf8(configuration) + "\"); it was deleted again");
					}
				}
				else if (kind == sld_mcp::v1::TABLE_KIND_HOLE)
				{
					ctx = std::make_unique<DrawingViewContext>();
					ctx->view = view;
					ctx->sheetName = sheetName;
					ctx->model = ViewModelOf(view);
					ctx->ids = std::make_unique<PersistIds>(ext);
					ctx->modelIds = std::make_unique<PersistIds>(ctx->model.ext);
					ctx->components = ViewComponents(view, ctx->model);
					if (ctx->model.type == swDocASSEMBLY)
						ctx->componentList = std::make_unique<ComponentList>(*ctx->modelIds, math, &holeComponents,
							ATL::CComQIPtr<IAssemblyDoc>(ctx->model.doc.p));
					VARIANT_BOOL ok = VARIANT_FALSE;
					drawing->ActivateView(FromUtf8(request->view_name()), &ok);
					// Origin with mark 1, the holes' edges with mark 2 (as the
					// Hole Table PropertyManager's two boxes).
					ViewEntityItem origin;
					if (grpc::Status status = ResolveDrawingRef(*ctx, request->hole_origin(), "the origin", origin); !status.ok())
						return status;
					if (origin.kind != sld_mcp::v1::VIEW_ENTITY_KIND_VERTEX)
						return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the hole table's origin must be a vertex");
					std::vector<ViewEntityItem> holes;
					for (int i = 0; i < request->hole_edges_size(); ++i)
					{
						ViewEntityItem item;
						if (grpc::Status status = ResolveDrawingRef(*ctx, request->hole_edges(i), "hole edge " + std::to_string(i + 1),
							item); !status.ok())
							return status;
						if (item.kind != sld_mcp::v1::VIEW_ENTITY_KIND_EDGE)
							return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "hole " + std::to_string(i + 1) +
								" must be an edge (the hole's circle)");
						holes.push_back(item);
					}
					viewBefore = ViewAnnotationList(view);
					doc->ClearSelection2(VARIANT_TRUE);
					if (!SelectViewEntityAt(doc, view, origin, std::nullopt, 1))
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select the origin vertex");
					for (size_t i = 0; i < holes.size(); ++i)
						if (!SelectViewEntityAt(doc, view, holes[i], std::nullopt, 2))
						{
							doc->ClearSelection2(VARIANT_TRUE);
							return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select hole edge " +
								std::to_string(i + 1));
						}
					const std::string start = request->start_tag().empty() ? "A" : request->start_tag();
					const long tagType = std::isdigit(static_cast<unsigned char>(start[0])) ? swHoleTable_NumericTags
						: swHoleTable_AlphaNumericTags;
					ATL::CComPtr<IDispatch> made;
					HRESULT hr = view->InsertHoleTable3(VARIANT_FALSE, x, y, anchor, FromUtf8(start), templateBstr,
						swHoleTableTagOrder_XY, tagType, ATL::CComVariant(), &made);
					doc->ClearSelection2(VARIANT_TRUE);
					if (FAILED(hr))
						return ComError("IView::InsertHoleTable3", hr);
					table = ATL::CComQIPtr<ITableAnnotation>(made.p);
					if (!table)
					{
						DeleteNotes(doc, ext, NewAnnotations(view, viewBefore));
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add the hole table (are the "
							"edges holes' circles seen face-on, and the origin a vertex of the same view?)");
					}
				}
				else if (kind == sld_mcp::v1::TABLE_KIND_REVISION)
				{
					const DrawingSheet* sheet = FindSheet(sheets, sheetName);
					if (!sheet || !sheet->sheet)
						return grpc::Status(grpc::StatusCode::NOT_FOUND, "the drawing has no sheet named \"" + sheetName + "\"");
					ATL::CComPtr<IRevisionTableAnnotation> existing;
					if (SUCCEEDED(sheet->sheet->get_RevisionTable(&existing)) && existing)
						if (ATL::CComQIPtr<ITableAnnotation> other(existing.p); other)
							return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, sheetName + " has a revision table already (" +
								TableName(other) + "): add revisions to it with edit_drawing_table");
					VARIANT_BOOL ok = VARIANT_FALSE;
					drawing->ActivateView(ATL::CComBSTR(L""), &ok);
					ATL::CComPtr<IRevisionTableAnnotation> made;
					HRESULT hr = sheet->sheet->InsertRevisionTable2(VARIANT_FALSE, x, y, anchor, templateBstr,
						request->revision_symbol() > 0 ? request->revision_symbol() : static_cast<long>(swRevisionTable_TriangleSymbol),
						VARIANT_FALSE, &made);
					if (FAILED(hr))
						return ComError("ISheet::InsertRevisionTable2", hr);
					table = ATL::CComQIPtr<ITableAnnotation>(made.p);
					if (!table)
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add the revision table");
				}
				else
				{
					VARIANT_BOOL ok = VARIANT_FALSE;
					drawing->ActivateView(ATL::CComBSTR(L""), &ok);
					HRESULT hr = drawing->InsertTableAnnotation2(VARIANT_FALSE, x, y, anchor, templateBstr, request->rows(),
						request->columns(), &table);
					if (FAILED(hr))
						return ComError("IDrawingDoc::InsertTableAnnotation2", hr);
					if (!table)
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add the table");
				}
			}
			else
				table = found.table;

			if (FAILED(table->GetAnnotation(&annotation)) || !annotation)
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks gave the table no annotation");
			TableState before;
			if (!create)
				before = ReadTableState(table, annotation, *request);
			std::vector<long> addedRevisions;
			grpc::Status status = ApplyTableChange(table, annotation, doc, drawing, *request, create, addedRevisions);
			if (!status.ok())
			{
				if (ATL::CComQIPtr<IRevisionTableAnnotation> revisions(table.p); revisions && !create)
					for (const long id : addedRevisions)
					{
						VARIANT_BOOL ok = VARIANT_FALSE;
						revisions->DeleteRevision(id, VARIANT_TRUE, &ok);
					}
				if (create)
				{
					DeleteTable(doc, ext, table);
					if (view && kind == sld_mcp::v1::TABLE_KIND_HOLE)
						DeleteNotes(doc, ext, NewAnnotations(view, viewBefore));
				}
				else
					RestoreTableState(table, annotation, before);
				doc->GraphicsRedraw2();
				return status;
			}
		}

		DescribeTable(table, response->mutable_table());
		if (view && kind == sld_mcp::v1::TABLE_KIND_HOLE)
		{
			PersistIds ids(ext);
			for (const auto& tag : NewAnnotations(view, viewBefore))
				if (long type = 0; SUCCEEDED(tag->GetType(&type)) && type == swNote)
					DescribeAnnotation(tag, ids, nullptr, response->add_hole_tags());
		}
		for (const auto& s : DrawingSheets(drawing))
			if (s.name == sheetName)
			{
				DrawingBudget budget;
				DescribeSheet(s, original, ext, math, {}, false, false, budget, response->mutable_sheet());
				DescribeSheetLayout(s, response->mutable_sheet());
			}
		response->set_created(create);
		response->set_active_sheet(original);
		return grpc::Status::OK;
	});
}

namespace
{
	grpc::Status ValidateBalloons(const sld_mcp::v1::AddBalloonsRequest& r)
	{
		if (r.view_name().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which view to balloon");
		if (r.targets_size() > kMaxBalloons)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "at most " + std::to_string(kMaxBalloons) + " balloons per call");
		for (int i = 0; i < r.targets_size(); ++i)
		{
			const auto& t = r.targets(i);
			const std::string which = "balloon " + std::to_string(i + 1);
			if (t.component().empty() == t.entity().view_name().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + ": give a component or an entity");
			if (!t.entity().view_name().empty() && t.entity().view_name() != r.view_name())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + ": its entity is in " + t.entity().view_name() +
					", not " + r.view_name());
			if (t.has_arrow_point() && t.entity().view_name().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, which + ": an arrow point goes with an entity");
		}
		if (r.targets_size() > 0 && (r.layout() != 0 || r.every_instance()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "layout / every_instance are for ballooning the whole view");
		if (r.layout() < 0 || r.layout() > swDetailingBalloonLayout_Left)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown layout " + std::to_string(r.layout()));
		if (r.has_style() && (r.style() < swBS_None || r.style() > swBS_Verbose))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown balloon style " + std::to_string(r.style()));
		if (r.has_size() && (r.size() < swBF_Tightest || r.size() > swBF_5Chars))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown balloon size " + std::to_string(r.size()));
		for (const auto content : { r.has_upper_content() ? r.upper_content() : 1, r.has_lower_content() ? r.lower_content() : 2 })
			if (content < swBalloonTextCustom || content > swBalloonTextViewViewLetter)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown balloon text " + std::to_string(content));
		return grpc::Status::OK;
	}

	// The balloon options both Auto Balloon and InsertBOMBalloon2 take.
	template <typename Options>
	void SetBalloonOptions(Options* o, const sld_mcp::v1::AddBalloonsRequest& r)
	{
		if (r.has_style())
			o->put_Style(r.style());
		if (r.has_size())
			o->put_Size(r.size());
		if (r.has_upper_content())
			o->put_UpperTextContent(r.upper_content());
		if (!r.upper_text().empty())
			o->put_UpperText(FromUtf8(r.upper_text()));
		if (r.has_lower_content())
			o->put_LowerTextContent(r.lower_content());
		if (!r.lower_text().empty())
			o->put_LowerText(FromUtf8(r.lower_text()));
		if (r.has_layer())
			o->put_Layername(FromUtf8(r.layer()));
	}

	// The view's stand-in for a component of its assembly, by the
	// assembly's name for it ("bracket-1", "sub-1/bracket-1") or the
	// stand-in's own ("pair-1/bracket-1").
	ATL::CComPtr<IComponent2> ViewComponentNamed(const DrawingViewContext& ctx, const std::string& name,
		std::vector<std::string>& names)
	{
		ATL::CComQIPtr<IAssemblyDoc> assembly(ctx.model.doc.p);
		for (const auto& component : ctx.components)
		{
			if (!component)
				continue;
			ATL::CComBSTR own;
			if (ATL::CComPtr<IComponent2> c = assembly ? AssemblyComponentOf(assembly, component) : component)
				c->get_Name2(&own);
			ATL::CComBSTR standIn;
			component->get_Name2(&standIn);
			names.push_back(ToUtf8(own.Length() > 0 ? own : standIn));
			if (ToUtf8(own) == name || ToUtf8(standIn) == name)
				return component;
		}
		return {};
	}
}

grpc::Status SolidWorksService::AddBalloons(grpc::ServerContext* context,
	const sld_mcp::v1::AddBalloonsRequest* request,
	sld_mcp::v1::AddBalloonsResponse* response)
{
	if (grpc::Status status = ValidateBalloons(*request); !status.ok())
		return status;

	return m_executor.Execute(context, "AddBalloons", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
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
		DrawingViewContext ctx;
		if (grpc::Status status = FindDrawingView(sheets, request->view_name(), ctx.view, ctx.sheetName); !status.ok())
			return status;
		ctx.model = ViewModelOf(ctx.view);
		if (ctx.model.type != swDocASSEMBLY)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, request->view_name() +
				(ctx.model.doc ? " shows a part" : "'s model isn't loaded") + ": BOM balloons label an assembly's components");
		ctx.ids = std::make_unique<PersistIds>(ext);
		ctx.modelIds = std::make_unique<PersistIds>(ctx.model.ext);
		ctx.components = ViewComponents(ctx.view, ctx.model);
		ctx.componentList = std::make_unique<ComponentList>(*ctx.modelIds, math, response->mutable_components(),
			ATL::CComQIPtr<IAssemblyDoc>(ctx.model.doc.p));

		// Positions and the targets' entities, before anything changes.
		std::vector<std::optional<Eigen::Vector2d>> positions, arrows;
		for (int i = 0; i < request->targets_size(); ++i)
		{
			const auto& t = request->targets(i);
			const std::string which = "balloon " + std::to_string(i + 1);
			std::optional<Eigen::Vector2d> at, arrow;
			Eigen::Vector2d p;
			if (t.has_position())
			{
				if (grpc::Status status = ToSheetPoint(sheets, math, t.position(), which + "'s position", p); !status.ok())
					return status;
				at = p;
			}
			if (t.has_arrow_point())
			{
				if (grpc::Status status = ToSheetPoint(sheets, math, t.arrow_point(), which + "'s arrow point", p); !status.ok())
					return status;
				arrow = p;
			}
			positions.push_back(at);
			arrows.push_back(arrow);
		}

		std::vector<ATL::CComPtr<IAnnotation>> made;
		{
			SheetActivation activation(drawing);
			if (!activation.Show(ctx.sheetName))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + ctx.sheetName);
			EditModeScope editMode(drawing);
			editMode.Set(false);
			VARIANT_BOOL ok = VARIANT_FALSE;
			drawing->ActivateView(FromUtf8(request->view_name()), &ok);

			// Targets resolved to the entity each balloon's leader goes to.
			std::vector<ViewEntityItem> items;
			for (int i = 0; i < request->targets_size(); ++i)
			{
				const auto& t = request->targets(i);
				const std::string which = "balloon " + std::to_string(i + 1);
				ViewEntityItem item;
				if (!t.component().empty())
				{
					std::vector<std::string> names;
					ATL::CComPtr<IComponent2> component = ViewComponentNamed(ctx, t.component(), names);
					if (!component)
					{
						std::string list;
						for (const auto& n : names)
							list += (list.empty() ? "" : ", ") + n;
						return grpc::Status(grpc::StatusCode::NOT_FOUND, which + ": " + request->view_name() +
							" shows no component named \"" + t.component() + "\" (it shows: " + list + ")");
					}
					// Its first visible edge; SolidWorks puts the arrow on it.
					const std::string pid = ctx.componentList->Add(component);
					std::vector<ViewEntityItem> edges;
					size_t budget = 1;
					ListViewEntities(ctx.view, component, pid, sld_mcp::v1::VIEW_ENTITY_KIND_EDGE, *ctx.ids, budget, edges);
					if (edges.empty())
					{
						budget = 1;
						ListViewEntities(ctx.view, component, pid, sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE, *ctx.ids, budget, edges);
					}
					if (edges.empty())
						return grpc::Status(grpc::StatusCode::NOT_FOUND, which + ": none of " + t.component() + "'s edges are visible in " +
							request->view_name());
					item = edges[0];
				}
				else if (grpc::Status status = ResolveDrawingRef(ctx, t.entity(), which, item); !status.ok())
					return status;
				items.push_back(item);
			}

			grpc::Status status = grpc::Status::OK;
			if (request->targets_size() == 0)
			{
				// Auto Balloon works on the selected view.
				doc->ClearSelection2(VARIANT_TRUE);
				VARIANT_BOOL selected = VARIANT_FALSE;
				if (FAILED(ext->SelectByID2(FromUtf8(request->view_name()), ATL::CComBSTR(L"DRAWINGVIEW"), 0.0, 0.0, 0.0,
					VARIANT_FALSE, 0, nullptr, swSelectOptionDefault, &selected)) || !selected)
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select " + request->view_name());
				ATL::CComPtr<IAutoBalloonOptions> options;
				if (FAILED(drawing->CreateAutoBalloonOptions(&options)) || !options)
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave no auto balloon options");
				if (request->layout() > 0)
					options->put_Layout(request->layout());
				options->put_IgnoreMultiple(request->every_instance() ? VARIANT_FALSE : VARIANT_TRUE);
				SetBalloonOptions(options.p, *request);
				ATL::CComVariant notes;
				HRESULT hr = drawing->AutoBalloon5(options, &notes);
				doc->ClearSelection2(VARIANT_TRUE);
				if (FAILED(hr))
					return ComError("IDrawingDoc::AutoBalloon5", hr);
				for (const auto& disp : comvariant::ToDispatches(notes))
					if (ATL::CComQIPtr<INote> note(disp.p); note)
					{
						ATL::CComPtr<IDispatch> a;
						if (SUCCEEDED(note->GetAnnotation(&a)) && a)
							made.push_back(ATL::CComPtr<IAnnotation>(ATL::CComQIPtr<IAnnotation>(a.p)));
					}
				if (made.empty())
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks added no balloons to " +
						request->view_name() + " (are its components already ballooned?)");
			}
			else
				for (size_t i = 0; i < items.size() && status.ok(); ++i)
				{
					const std::string which = "balloon " + std::to_string(i + 1);
					doc->ClearSelection2(VARIANT_TRUE);
					if (!SelectViewEntityAt(doc, ctx.view, items[i], arrows[i], 0) &&
						!(arrows[i] && SelectViewEntityAt(doc, ctx.view, items[i], std::nullopt, 0)) &&
						!SelectViewEntity(ctx.view, items[i]))
					{
						status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select " + which + "'s entity");
						break;
					}
					ATL::CComPtr<IBalloonOptions> options;
					if (FAILED(ext->CreateBalloonOptions(&options)) || !options)
					{
						status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave no balloon options");
						break;
					}
					SetBalloonOptions(options.p, *request);
					ATL::CComPtr<INote> note;
					HRESULT hr = ext->InsertBOMBalloon2(options, &note);
					doc->ClearSelection2(VARIANT_TRUE);
					ATL::CComPtr<IDispatch> a;
					if (FAILED(hr) || !note || FAILED(note->GetAnnotation(&a)) || !a)
					{
						status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add " + which);
						break;
					}
					ATL::CComQIPtr<IAnnotation> annotation(a.p);
					made.push_back(ATL::CComPtr<IAnnotation>(annotation));
					if (positions[i])
					{
						VARIANT_BOOL moved = VARIANT_FALSE;
						if (FAILED(annotation->SetPosition2(positions[i]->x(), positions[i]->y(), 0.0, &moved)) || !moved)
							status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't move " + which);
					}
				}
			if (!status.ok())
			{
				DeleteNotes(doc, ext, made);
				doc->GraphicsRedraw2();
				return status;
			}
			doc->GraphicsRedraw2();
		}

		PersistIds ids(ext);
		for (const auto& annotation : made)
			DescribeAnnotation(annotation, ids, ctx.componentList.get(), response->add_balloons());
		// The BOMs balloons take their numbers from: tables listing this model.
		ATL::CComBSTR modelFile;
		ctx.view->GetReferencedModelName(&modelFile);
		for (const auto& s : DrawingSheets(drawing))
		{
			for (const auto& table : SheetTables(s))
				if (ATL::CComQIPtr<IBomTableAnnotation> bom(table.p); bom)
				{
					ATL::CComPtr<IBomFeature> feature;
					ATL::CComBSTR listed;
					if (SUCCEEDED(bom->get_BomFeature(&feature)) && feature && SUCCEEDED(feature->GetReferencedModelName(&listed)) &&
						_wcsicmp(listed.m_str ? listed.m_str : L"", modelFile.m_str ? modelFile.m_str : L"") == 0)
						response->add_boms(TableName(table));
				}
			if (s.name == ctx.sheetName)
			{
				DrawingBudget budget;
				DescribeSheet(s, original, ext, math, {}, false, false, budget, response->mutable_sheet());
				DescribeSheetLayout(s, response->mutable_sheet());
			}
		}
		response->set_active_sheet(original);
		return grpc::Status::OK;
	});
}
