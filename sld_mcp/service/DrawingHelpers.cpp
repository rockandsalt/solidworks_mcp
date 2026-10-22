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

	grpc::Status ReadSheetSetup(ISheet* sheet, SheetSetup& setup)
	{
		ATL::CComVariant properties;
		std::vector<double> p;
		if (!sheet || FAILED(sheet->GetProperties2(&properties)) || (p = comvariant::ToDoubles(properties)).size() < 7)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't describe the sheet " + SheetName(sheet));
		setup.paperSize = std::isfinite(p[0]) ? static_cast<long>(p[0]) : -1;
		setup.templateType = static_cast<long>(p[1]);
		setup.scaleNumerator = p[2];
		setup.scaleDenominator = p[3];
		setup.firstAngle = p[4] != 0.0;
		setup.width = p[5];
		setup.height = p[6];
		setup.format.Empty();
		setup.propertyView.Empty();
		sheet->GetTemplateName(&setup.format);
		sheet->get_CustomPropertyView(&setup.propertyView);
		return grpc::Status::OK;
	}

	void SetSheetInfo(const SheetSetup& setup, sld_mcp::v1::SheetInfo* info)
	{
		info->set_paper_size(static_cast<int32_t>(setup.paperSize));
		info->set_scale_numerator(setup.scaleNumerator);
		info->set_scale_denominator(setup.scaleDenominator);
		info->set_first_angle(setup.firstAngle);
		info->set_width(setup.width);
		info->set_height(setup.height);
	}

	grpc::Status ActiveSheet(IDrawingDoc* drawing, ATL::CComPtr<ISheet>& sheet)
	{
		sheet.Release();
		ATL::CComPtr<IDispatch> disp;
		HRESULT hr = drawing->GetCurrentSheet(&disp);
		if (FAILED(hr))
			return ComError("IDrawingDoc::GetCurrentSheet", hr);
		sheet = ATL::CComQIPtr<ISheet>(disp.p);
		if (!sheet)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't give the drawing's active sheet");
		return grpc::Status::OK;
	}

	grpc::Status ChangeActiveSheet(IDrawingDoc* drawing, const SheetChange& change, SheetSetup& setup)
	{
		ATL::CComPtr<ISheet> sheet;
		if (grpc::Status status = ActiveSheet(drawing, sheet); !status.ok())
			return status;
		if (grpc::Status status = ReadSheetSetup(sheet, setup); !status.ok())
			return status;
		ATL::CComBSTR name;
		sheet->GetName(&name);

		const auto [numerator, denominator] = change.scale.value_or(std::make_pair(setup.scaleNumerator, setup.scaleDenominator));
		const bool firstAngle = change.firstAngle.value_or(setup.firstAngle);
		// SetupSheet5 with another size and/or sheet format (empty = keep
		// the sheet's), the scale and projection asked for, then read back.
		auto apply = [&](long paper, double width, double height, const std::wstring& format,
			const std::string& failure) -> grpc::Status {
			VARIANT_BOOL ok = VARIANT_FALSE;
			HRESULT hr = drawing->SetupSheet5(name, paper, format.empty() ? setup.templateType : swDwgTemplateCustom,
				numerator, denominator, firstAngle ? VARIANT_TRUE : VARIANT_FALSE,
				format.empty() ? setup.format : ATL::CComBSTR(format.c_str()), width, height, setup.propertyView,
				VARIANT_TRUE, &ok);
			if (FAILED(hr))
				return ComError("IDrawingDoc::SetupSheet5", hr);
			if (!ok)
				return grpc::Status(grpc::StatusCode::INTERNAL, failure);
			if (grpc::Status status = ActiveSheet(drawing, sheet); !status.ok())
				return status;
			return ReadSheetSetup(sheet, setup);
		};
		auto sizeDiffers = [&] {
			return change.paperSize &&
				(setup.paperSize != *change.paperSize ||
					(*change.paperSize == swDwgPapersUserDefined &&
						(std::abs(setup.width - change.width) > 1e-6 || std::abs(setup.height - change.height) > 1e-6)));
		};
		auto close = [](double a, double b) { return std::abs(a - b) <= 1e-9 * std::max(std::abs(a), std::abs(b)); };
		auto scaleDiffers = [&] {
			return !close(setup.scaleNumerator, numerator) || !close(setup.scaleDenominator, denominator) ||
				setup.firstAngle != firstAngle;
		};

		if (!change.format.empty())
			if (grpc::Status status = apply(swDwgPapersUserDefined, 0.0, 0.0, change.format,
				"SolidWorks couldn't put the sheet format " + ToUtf8(ATL::CComBSTR(change.format.c_str())) +
				" on the sheet"); !status.ok())
				return status;
		if (sizeDiffers())
		{
			if (grpc::Status status = apply(*change.paperSize, change.width, change.height, L"",
				"SolidWorks couldn't change the sheet's size"); !status.ok())
				return status;
			if (sizeDiffers())
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't change the sheet's size");
		}
		if (scaleDiffers())
		{
			// A size SolidWorks didn't report is kept as user defined.
			const long paper = setup.paperSize >= 0 ? setup.paperSize : static_cast<long>(swDwgPapersUserDefined);
			if (grpc::Status status = apply(paper, setup.width, setup.height, L"",
				"SolidWorks couldn't change the sheet's scale or projection"); !status.ok())
				return status;
			if (scaleDiffers())
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't change the sheet's scale or projection");
		}
		return grpc::Status::OK;
	}
}
