// DrawingHelpers.h : Drawings: sheets, views and the annotations in a view.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	std::string ViewName(IView* view);

	long ViewType(IView* view);

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
}
