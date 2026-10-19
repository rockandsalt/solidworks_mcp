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
}
