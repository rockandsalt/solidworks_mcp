// DrawingHelpers.cpp : see DrawingHelpers.h.
#include "pch.h"
#include "DrawingHelpers.h"
#include "Common.h"

namespace sld_mcp::service
{
	std::string ViewName(IView* view)
	{
		ATL::CComBSTR name;
		if (!view)
			return {};
		if (FAILED(view->GetName2(&name)) || name.Length() == 0)
		{
			name.Empty();
			view->get_Name(&name);
		}
		return ToUtf8(name);
	}

	long ViewType(IView* view)
	{
		long type = 0;
		if (view)
			view->get_Type(&type);
		return type;
	}

	std::string SheetName(ISheet* sheet)
	{
		ATL::CComBSTR name;
		if (sheet)
			sheet->GetName(&name);
		return ToUtf8(name);
	}

	std::vector<DrawingSheet> DrawingSheets(IDrawingDoc* drawing)
	{
		std::vector<DrawingSheet> out;
		ATL::CComVariant namesVar;
		if (SUCCEEDED(drawing->GetSheetNames(&namesVar)))
			for (const auto& name : comvariant::ToBstrs(namesVar))
			{
				DrawingSheet s;
				s.name = ToUtf8(name);
				if (FAILED(drawing->get_Sheet(name, &s.sheet)))
					s.sheet.Release();
				ATL::CComVariant views;
				if (s.sheet && SUCCEEDED(s.sheet->GetViews(&views)))
					for (const auto& disp : comvariant::ToDispatches(views))
					{
						ATL::CComQIPtr<IView> view(disp.p);
						if (!view)
							continue;
						if (ViewType(view) == swDrawingSheet)
						{
							if (!s.sheetView)
								s.sheetView = view;
						}
						else
							s.views.push_back(ATL::CComPtr<IView>(view));
					}
				out.push_back(std::move(s));
			}

		// IDrawingDoc::GetViews: one array per sheet, the sheet's own view
		// first. The source for the sheet views, and for a sheet's views if
		// ISheet::GetViews gave none.
		ATL::CComVariant all;
		if (FAILED(drawing->GetViews(&all)))
			return out;
		for (const auto& perSheet : comvariant::ToVariants(all))
		{
			ATL::CComPtr<IView> sheetView;
			std::vector<ATL::CComPtr<IView>> views;
			for (const auto& disp : comvariant::ToDispatches(perSheet))
			{
				ATL::CComQIPtr<IView> view(disp.p);
				if (!view)
					continue;
				if (ViewType(view) == swDrawingSheet && !sheetView)
					sheetView = view;
				else if (ViewType(view) != swDrawingSheet)
					views.push_back(ATL::CComPtr<IView>(view));
			}
			if (!sheetView)
				continue;
			ATL::CComPtr<ISheet> sheet;
			std::string name = SUCCEEDED(sheetView->get_Sheet(&sheet)) && sheet ? SheetName(sheet) : ViewName(sheetView);
			for (auto& s : out)
				if (s.name == name)
				{
					if (!s.sheetView)
						s.sheetView = sheetView;
					if (s.views.empty())
						s.views = std::move(views);
					break;
				}
		}
		return out;
	}

	std::vector<ATL::CComPtr<IAnnotation>> ViewAnnotationList(IView* view)
	{
		std::vector<ATL::CComPtr<IAnnotation>> out;
		ATL::CComVariant annotations;
		if (view && SUCCEEDED(view->GetAnnotations(&annotations)))
			for (const auto& disp : comvariant::ToDispatches(annotations))
				if (ATL::CComQIPtr<IAnnotation> annotation(disp.p); annotation)
					out.push_back(ATL::CComPtr<IAnnotation>(annotation));
		return out;
	}
}
