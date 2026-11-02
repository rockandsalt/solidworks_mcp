// DrawingHelpers.h : Drawings: sheets, views and the annotations in a view.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"

namespace sld_mcp::service
{
	std::string ViewName(IView* view);

	long ViewType(IView* view);

	// swDisplayMode_e as the user sees it: SolidWorks reports "shaded with
	// edges" as swSHADED plus GetDisplayEdgesInShadedMode, this makes it
	// swSHADED_EDGES. swHIDDEN if SolidWorks doesn't say.
	long ViewDisplayMode(IView* view);

	std::string SheetName(ISheet* sheet);

	// One sheet of a drawing: its drawing views, and the sheet's own "view"
	// (which holds the annotations placed on the sheet itself).
	struct DrawingSheet
	{
		std::string name;
		ATL::CComPtr<ISheet> sheet;
		ATL::CComPtr<IView> sheetView;
		std::vector<ATL::CComPtr<IView>> views;
	};

	// Every sheet with its views, in the drawing's sheet order.
	std::vector<DrawingSheet> DrawingSheets(IDrawingDoc* drawing);

	std::vector<ATL::CComPtr<IAnnotation>> ViewAnnotationList(IView* view);

	// `name` as an absolute path, or as a file in one of the folders the
	// file-location preference `foldersPref` lists (";"-separated), with or
	// without `extension` (".drwdot"). NOT_FOUND names the files there are.
	grpc::Status FindTemplateFile(ISldWorks* swApp, swUserPreferenceStringValue_e foldersPref,
		const std::string& name, const std::wstring& extension, const std::string& kind, std::wstring& path);

	// A sheet's setup: ISheet::GetProperties2 ([paper size, template,
	// scale 1, scale 2, first angle, width, height, ...]), its sheet format
	// file and custom property view.
	struct SheetSetup
	{
		long paperSize = -1;  // swDwgPaperSizes_e; -1 = not reported
		long templateType = swDwgTemplateNone;
		double scaleNumerator = 1.0;
		double scaleDenominator = 1.0;
		bool firstAngle = false;
		double width = 0.0;  // meters
		double height = 0.0;
		ATL::CComBSTR format;  // ISheet::GetTemplateName
		ATL::CComBSTR propertyView;

		// GetTemplateName is empty or "*.drt" without a sheet format.
		bool HasFormat() const { return format.Length() > 0 && format[0] != L'*'; }
	};

	grpc::Status ReadSheetSetup(ISheet* sheet, SheetSetup& setup);

	// Paper size, scale, projection and width / height into `info`.
	void SetSheetInfo(const SheetSetup& setup, sld_mcp::v1::SheetInfo* info);

	grpc::Status ActiveSheet(IDrawingDoc* drawing, ATL::CComPtr<ISheet>& sheet);

	// What to change on a sheet; unset / empty = keep.
	struct SheetChange
	{
		std::optional<long> paperSize;
		double width = 0.0;  // meters, user-defined paper size only
		double height = 0.0;
		std::wstring format;  // a sheet format file
		std::optional<std::pair<double, double>> scale;  // numerator, denominator
		std::optional<bool> firstAngle;
	};

	// Applies `change` to the active sheet with IDrawingDoc::SetupSheet5,
	// keeping its name, the rest of its setup and its custom property view.
	// SetupSheet5 only ever changes the active sheet (and renames it to its
	// Name argument), so show the sheet first. A new sheet format brings its
	// own size, even over a paper size given in the same call, so the
	// format goes on first and the size after. Fails if SolidWorks refuses
	// or the sheet doesn't read back as asked; `setup` is the sheet's setup
	// after (also on failure, as far as it got).
	grpc::Status ChangeActiveSheet(IDrawingDoc* drawing, const SheetChange& change, SheetSetup& setup);

	// Visible entities listed per InspectDrawing call, over all views. A big
	// view of a detailed part has a few thousand edges.
	constexpr size_t kMaxViewEntities = 20000;

	// The active document, if it's a drawing.
	grpc::Status GetActiveDrawing(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc,
		ATL::CComQIPtr<IDrawingDoc>& drawing);

	std::string ActiveSheetName(IDrawingDoc* drawing);

	// Finds a view by name; `sheetName` is the sheet it's on.
	grpc::Status FindDrawingView(const std::vector<DrawingSheet>& sheets, const std::string& name,
		ATL::CComPtr<IView>& view, std::string& sheetName);

	// Selects the view by name, on the active sheet, and nothing else.
	bool SelectDrawingView(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name);

	bool DeleteDrawingView(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name);

	bool EqualsIgnoringCase(const std::string& a, const std::string& b);

	// SolidWorks refuses a sheet name another sheet has in any case.
	bool SameSheetName(const std::string& a, const std::string& b);

	// A sheet by name: exactly, else ignoring case. Null if there's none.
	const DrawingSheet* FindSheet(const std::vector<DrawingSheet>& sheets, const std::string& name);

	// "Sheet1, Sheet2", for messages.
	std::string SheetList(const std::vector<DrawingSheet>& sheets);

	// Shows `sheetName` if it isn't the sheet SolidWorks has active; puts
	// the sheet that was active back when it goes out of scope. Entities of
	// a view can only be selected while its sheet is showing.
	class SheetActivation
	{
	public:
		explicit SheetActivation(IDrawingDoc* drawing) : m_drawing(drawing), m_original(ActiveSheetName(drawing)),
			m_current(m_original) {}
		~SheetActivation()
		{
			if (m_current != m_original && !m_original.empty())
			{
				VARIANT_BOOL ok = VARIANT_FALSE;
				m_drawing->ActivateSheet(FromUtf8(m_original), &ok);
			}
		}
		SheetActivation(const SheetActivation&) = delete;
		SheetActivation& operator=(const SheetActivation&) = delete;

		bool Show(const std::string& sheetName)
		{
			if (sheetName == m_current)
				return true;
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(m_drawing->ActivateSheet(FromUtf8(sheetName), &ok)) || !ok)
				return false;
			m_current = sheetName;
			return true;
		}

		const std::string& Original() const { return m_original; }

		// Leaves the sheet showing now as the active one.
		void Keep() { m_original = m_current; }

	private:
		ATL::CComPtr<IDrawingDoc> m_drawing;
		std::string m_original;
		std::string m_current;
	};

	// The model a drawing view shows. Entities in the view belong to it, so
	// their persist ids come from its extension. All null if the model isn't
	// loaded (lightweight or detached views).
	struct ViewModel
	{
		ATL::CComPtr<IModelDoc2> doc;
		ATL::CComPtr<IModelDocExtension> ext;
		long type = swDocNONE;
	};

	ViewModel ViewModelOf(IView* view);

	// What GetVisibleEntities2 is asked about: an assembly view's visible
	// components one by one, or (part views) a null component.
	std::vector<ATL::CComPtr<IComponent2>> ViewComponents(IView* view, const ViewModel& model);

	// One visible entity of a view, with the key a DrawingEntityRef names it
	// by: persist id (silhouettes: their face's), component, and ordinal
	// among the view's entities of that kind with the same persist id and
	// component.
	struct ViewEntityItem
	{
		sld_mcp::v1::ViewEntityKind kind = sld_mcp::v1::VIEW_ENTITY_KIND_UNSPECIFIED;
		ATL::CComPtr<IDispatch> obj;
		ATL::CComPtr<IComponent2> component;
		std::string persistId;
		std::string componentPid;
		int ordinal = 0;
	};

	// Appends a view's visible entities of one kind (of one component, known
	// by `componentPid`) to `out`. `ids` must be the drawing's: the view's
	// model gives all of a view's entities the same persist id. False if
	// `budget` ran out first.
	bool ListViewEntities(IView* view, IComponent2* component, const std::string& componentPid,
		sld_mcp::v1::ViewEntityKind kind, PersistIds& ids, size_t& budget, std::vector<ViewEntityItem>& out);

	// Caps shared by everything one InspectDrawing call lists.
	struct DrawingBudget
	{
		size_t entities = kMaxViewEntities;
		size_t annotations = kMaxAnnotations;
		bool truncated = false;
	};

	// A sheet as InspectDrawing reports it: setup, views (with the entities
	// of those in `entityViews`), annotations (the sheet format's only
	// counted unless `describeSheetFormat`).
	void DescribeSheet(const DrawingSheet& sheet, const std::string& activeName, IModelDocExtension* drawingExt,
		IMathUtility* math, const std::set<std::string>& entityViews, bool includeVertices, bool includeAnnotations,
		DrawingBudget& budget, sld_mcp::v1::SheetInfo* info, bool describeSheetFormat = true);

	// Which of the sheet and its format is being edited; SolidWorks puts a
	// new note in the format while that's being edited, whatever view is
	// active. Puts back the format mode the user had, if the sheet showing
	// at the end is the one they had.
	class EditModeScope
	{
	public:
		explicit EditModeScope(IDrawingDoc* drawing) : m_drawing(drawing), m_sheet(ActiveSheetName(drawing))
		{
			VARIANT_BOOL editingSheet = VARIANT_TRUE;
			if (SUCCEEDED(drawing->GetEditSheet(&editingSheet)))
				m_userFormat = m_format = editingSheet == VARIANT_FALSE;
		}
		~EditModeScope()
		{
			const bool wanted = m_userFormat && ActiveSheetName(m_drawing) == m_sheet;
			if (wanted != m_format)
				Set(wanted);
		}
		EditModeScope(const EditModeScope&) = delete;
		EditModeScope& operator=(const EditModeScope&) = delete;

		// EditSheet also deactivates the active view.
		void Set(bool format)
		{
			if (format)
				m_drawing->EditTemplate();
			else
				m_drawing->EditSheet();
			m_format = format;
		}

	private:
		ATL::CComPtr<IDrawingDoc> m_drawing;
		std::string m_sheet;
		bool m_userFormat = false;
		bool m_format = false;
	};

	// A DrawingPoint on the sheet: as given, or from a view's own
	// coordinates (the model's size, from where the model origin lands on
	// the sheet, along the sheet's axes turned by the view's angle).
	grpc::Status ToSheetPoint(const std::vector<DrawingSheet>& sheets, IMathUtility* math,
		const sld_mcp::v1::DrawingPoint& point, const std::string& what, Eigen::Vector2d& out);

	// Cells listed per table (rows x columns); the rest is cut off
	// (TableInfo.truncated).
	constexpr long kMaxTableCells = 5000;

	ATL::CComPtr<ITableAnnotation> TableOf(IAnnotation* annotation);

	// A shown title is a row of its own above the table (SolidWorks' row 0):
	// 1 then, else 0. Row indices on the wire leave it out.
	long TitleRows(ITableAnnotation* table);

	// A table's anchor, sizes, cells and, for a BOM, what it lists and which
	// components each row counts.
	void DescribeTable(ITableAnnotation* table, sld_mcp::v1::TableInfo* info);

	// The sheet's tables (whether SolidWorks lists them under the sheet or
	// a view), each once.
	std::vector<ATL::CComPtr<ITableAnnotation>> SheetTables(const DrawingSheet& sheet);

	// The sheet's tables and its format's straight sketch lines into `info`.
	void DescribeSheetLayout(const DrawingSheet& sheet, sld_mcp::v1::SheetInfo* info);

	// What finding and selecting entities of one view needs, built once per
	// view per call.
	struct DrawingViewContext
	{
		ATL::CComPtr<IView> view;
		std::string sheetName;
		ViewModel model;
		std::unique_ptr<PersistIds> ids;  // the drawing's, as DescribeSheet's
		std::unique_ptr<PersistIds> modelIds;
		std::vector<ATL::CComPtr<IComponent2>> components;
		std::unique_ptr<ComponentList> componentList;  // assemblies
		// Visible entities by kind and component persist id.
		std::map<std::pair<int, std::string>, std::vector<ViewEntityItem>> entities;
	};

	// The entity `ref` names, listed again; NOT_FOUND (prefixed with
	// `where`) if it isn't visible in the view any more.
	grpc::Status ResolveDrawingRef(DrawingViewContext& ctx, const sld_mcp::v1::DrawingEntityRef& ref,
		const std::string& where, ViewEntityItem& out);

	// Adds the entity to the selection (silhouettes through their own Select2).
	bool SelectViewEntity(IView* view, const ViewEntityItem& item);

	// Adds the entity to the selection with `mark`, picked at `at` (sheet
	// meters) when given, through an ISelectData for the view.
	bool SelectViewEntityAt(IModelDoc2* doc, IView* view, const ViewEntityItem& item,
		const std::optional<Eigen::Vector2d>& at, long mark);
}
