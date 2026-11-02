// DrawingHelpers.cpp : see DrawingHelpers.h.
#include "pch.h"
#include "DrawingHelpers.h"
#include "Common.h"
#include "AnnotationHelpers.h"

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

	long ViewDisplayMode(IView* view)
	{
		long mode = swHIDDEN;
		if (!view || FAILED(view->GetDisplayMode2(&mode)))
			return swHIDDEN;
		VARIANT_BOOL edges = VARIANT_FALSE;
		if (mode == swSHADED && SUCCEEDED(view->GetDisplayEdgesInShadedMode(&edges)) && edges)
			return swSHADED_EDGES;
		return mode;
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

	// The active document, if it's a drawing.
	grpc::Status GetActiveDrawing(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc,
		ATL::CComQIPtr<IDrawingDoc>& drawing)
	{
		HRESULT hr = swApp->get_IActiveDoc2(&doc);
		if (FAILED(hr))
			return ComError("ISldWorks::IActiveDoc2", hr);
		if (!doc)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"no document is open in SolidWorks -- open a drawing first");
		long docType = swDocNONE;
		hr = doc->GetType(&docType);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetType", hr);
		if (docType != swDocDRAWING)
		{
			doc.Release();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active document isn't a drawing -- open (or switch to) the drawing first");
		}
		drawing = doc.p;
		if (!drawing)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active drawing has no IDrawingDoc interface");
		return grpc::Status::OK;
	}

	std::string ActiveSheetName(IDrawingDoc* drawing)
	{
		ATL::CComPtr<IDispatch> disp;
		if (FAILED(drawing->GetCurrentSheet(&disp)) || !disp)
			return {};
		ATL::CComQIPtr<ISheet> sheet(disp.p);
		return SheetName(sheet);
	}

	// Finds a view by name; `sheetName` is the sheet it's on.
	grpc::Status FindDrawingView(const std::vector<DrawingSheet>& sheets, const std::string& name,
		ATL::CComPtr<IView>& view, std::string& sheetName)
	{
		std::string known;
		for (const auto& s : sheets)
			for (const auto& v : s.views)
			{
				const std::string viewName = ViewName(v);
				if (viewName == name)
				{
					view = v;
					sheetName = s.name;
					return grpc::Status::OK;
				}
				known += (known.empty() ? "" : ", ") + viewName;
			}
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			"the drawing has no view named \"" + name + "\"" +
			(known.empty() ? std::string(" (it has no views)") : " (its views: " + known + ")"));
	}

	bool SelectDrawingView(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE;
		return SUCCEEDED(ext->SelectByID2(FromUtf8(name), ATL::CComBSTR(L"DRAWINGVIEW"), 0.0, 0.0, 0.0, VARIANT_FALSE,
			0, nullptr, swSelectOptionDefault, &selected)) && selected;
	}

	bool DeleteDrawingView(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name)
	{
		VARIANT_BOOL deleted = VARIANT_FALSE;
		const bool ok = SelectDrawingView(doc, ext, name) && SUCCEEDED(ext->DeleteSelection2(0, &deleted)) && deleted;
		doc->ClearSelection2(VARIANT_TRUE);
		return ok;
	}

	ViewModel ViewModelOf(IView* view)
	{
		ViewModel model;
		if (FAILED(view->get_ReferencedDocument(&model.doc)) || !model.doc)
		{
			model.doc.Release();
			return model;
		}
		if (FAILED(model.doc->GetType(&model.type)))
			model.type = swDocNONE;
		if (FAILED(model.doc->get_Extension(&model.ext)))
			model.ext.Release();
		return model;
	}

	namespace
	{
		long SwViewEntityType(sld_mcp::v1::ViewEntityKind kind)
		{
			switch (kind)
			{
			case sld_mcp::v1::VIEW_ENTITY_KIND_VERTEX:
				return swViewEntityType_Vertex;
			case sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE:
				return swViewEntityType_SilhouetteEdge;
			default:
				return swViewEntityType_Edge;
			}
		}
	}

	// What GetVisibleEntities2 is asked about: an assembly view's visible
	// components one by one, or (part views) a null component.
	std::vector<ATL::CComPtr<IComponent2>> ViewComponents(IView* view, const ViewModel& model)
	{
		std::vector<ATL::CComPtr<IComponent2>> out;
		if (model.type == swDocASSEMBLY)
		{
			ATL::CComVariant components;
			if (SUCCEEDED(view->GetVisibleComponents(&components)))
				for (const auto& disp : comvariant::ToDispatches(components))
					if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
						out.push_back(ATL::CComPtr<IComponent2>(component));
		}
		if (out.empty())
			out.emplace_back();
		return out;
	}

	// Appends a view's visible entities of one kind (of one component, known
	// by `componentPid`) to `out`. `ids` must be the drawing's: the view's
	// model gives all of a view's entities the same persist id. False if
	// `budget` ran out first.
	bool ListViewEntities(IView* view, IComponent2* component, const std::string& componentPid,
		sld_mcp::v1::ViewEntityKind kind, PersistIds& ids, size_t& budget, std::vector<ViewEntityItem>& out)
	{
		ATL::CComVariant entities;
		// GetVisibleEntities2's silhouettes in a part view can have no face
		// and no end points (after the view's scale changed, until the next
		// GetVisibleEntities call); GetVisibleEntities' are whole.
		const HRESULT hr = kind == sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE
			? view->GetVisibleEntities(component, SwViewEntityType(kind), &entities)
			: view->GetVisibleEntities2(component, SwViewEntityType(kind), &entities);
		if (FAILED(hr))
			return true;
		std::map<std::string, int> seen;
		for (const auto& disp : comvariant::ToDispatches(entities))
		{
			if (!disp)
				continue;
			if (budget == 0)
				return false;
			--budget;
			ViewEntityItem item;
			item.kind = kind;
			item.obj = disp;
			item.component = component;
			item.componentPid = componentPid;
			if (kind == sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE)
			{
				ATL::CComQIPtr<ISilhouetteEdge> silhouette(disp.p);
				ATL::CComPtr<IFace2> face;
				if (silhouette && SUCCEEDED(silhouette->GetFace(&face)) && face)
					item.persistId = ids.Get(face);
			}
			else
				item.persistId = ids.Get(disp);
			item.ordinal = seen[item.persistId]++;
			out.push_back(std::move(item));
		}
		return true;
	}

	namespace
	{
		bool MathPointCoordinates(IMathPoint* point, sld_mcp::v1::Vec3* out)
		{
			ATL::CComVariant data;
			if (!point || FAILED(point->get_ArrayData(&data)))
				return false;
			const std::vector<double> p = comvariant::ToDoubles(data);
			if (p.size() < 3 || !std::isfinite(p[0]) || !std::isfinite(p[1]) || !std::isfinite(p[2]))
				return false;
			SetVec3(out, p.data());
			return true;
		}

		// A view vertex's point in its model. IVertex::GetPoint on the vertices
		// GetVisibleEntities2 hands out comes back shifted by the view's
		// position on the sheet (model x + view x, model y + view y; SOLIDWORKS
		// 2026), while the end vertices of its edges are in model space: the
		// one nearest the unshifted point is this vertex.
		bool ViewVertexPoint(IVertex* vertex, const Eigen::Vector2d& viewPosition, Eigen::Vector3d& out)
		{
			ATL::CComVariant pointVar;
			if (FAILED(vertex->GetPoint(&pointVar)))
				return false;
			const std::vector<double> p = comvariant::ToDoubles(pointVar);
			if (p.size() < 3)
				return false;
			const Eigen::Vector3d unshifted(p[0] - viewPosition.x(), p[1] - viewPosition.y(), p[2]);
			out = unshifted;
			double best = std::numeric_limits<double>::infinity();
			ATL::CComVariant edges;
			if (SUCCEEDED(vertex->GetEdges(&edges)))
				for (const auto& disp : comvariant::ToDispatches(edges))
					if (ATL::CComQIPtr<IEdge> edge(disp.p); edge)
						for (const bool start : { true, false })
						{
							ATL::CComPtr<IDispatch> endDisp;
							if (FAILED(start ? edge->GetStartVertex(&endDisp) : edge->GetEndVertex(&endDisp)) || !endDisp)
								continue;
							ATL::CComQIPtr<IVertex> end(endDisp.p);
							ATL::CComVariant endVar;
							if (!end || FAILED(end->GetPoint(&endVar)))
								continue;
							const std::vector<double> e = comvariant::ToDoubles(endVar);
							if (e.size() < 3)
								continue;
							const Eigen::Vector3d candidate(e[0], e[1], e[2]);
							if (const double d = (candidate - unshifted).norm(); d < best)
							{
								best = d;
								out = candidate;
							}
						}
			return out.allFinite();
		}

		void DescribeViewEntity(const ViewEntityItem& item, const Eigen::Vector2d& viewPosition, PersistIds& ids,
			sld_mcp::v1::ViewEntity* out)
		{
			out->set_kind(item.kind);
			out->set_persist_id(item.persistId);
			out->set_component_persist_id(item.componentPid);
			out->set_ordinal(item.ordinal);
			switch (item.kind)
			{
			case sld_mcp::v1::VIEW_ENTITY_KIND_EDGE:
				if (ATL::CComQIPtr<IEdge> edge(item.obj.p); edge)
				{
					InspectEdge(edge, ids, out->mutable_edge());
					out->mutable_edge()->set_component_persist_id(item.componentPid);
				}
				break;
			case sld_mcp::v1::VIEW_ENTITY_KIND_VERTEX:
				if (ATL::CComQIPtr<IVertex> vertex(item.obj.p); vertex)
				{
					Eigen::Vector3d point;
					if (ViewVertexPoint(vertex, viewPosition, point))
						SetVec3(out->mutable_start(), point);
				}
				break;
			case sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE:
				if (ATL::CComQIPtr<ISilhouetteEdge> silhouette(item.obj.p); silhouette)
				{
					ATL::CComPtr<IMathPoint> start, end;
					sld_mcp::v1::Vec3 p;
					if (SUCCEEDED(silhouette->GetStartPoint(&start)) && MathPointCoordinates(start, &p))
						*out->mutable_start() = p;
					if (SUCCEEDED(silhouette->GetEndPoint(&end)) && MathPointCoordinates(end, &p))
						*out->mutable_end() = p;
				}
				break;
			default:
				break;
			}
		}

		// IView::ModelToViewTransform as where the model origin and unit axes
		// land on the sheet (as DescribePlacement does for components), so its
		// internal layout never matters.
		void DescribeViewTransform(IView* view, IMathUtility* math, sld_mcp::v1::ViewInfo* info)
		{
			ATL::CComPtr<IMathTransform> xform;
			if (FAILED(view->get_ModelToViewTransform(&xform)) || !xform)
				return;
			const Eigen::Vector3d unit[4] = { Eigen::Vector3d::Zero(), Eigen::Vector3d::UnitX(),
				Eigen::Vector3d::UnitY(), Eigen::Vector3d::UnitZ() };
			Eigen::Vector3d placed[4];
			for (int i = 0; i < 4; ++i)
				if (!TransformPoint(math, xform, unit[i], placed[i]))
					return;
			SetVec3(info->mutable_model_origin(), placed[0]);
			SetVec3(info->mutable_model_x(), Eigen::Vector3d(placed[1] - placed[0]));
			SetVec3(info->mutable_model_y(), Eigen::Vector3d(placed[2] - placed[0]));
			SetVec3(info->mutable_model_z(), Eigen::Vector3d(placed[3] - placed[0]));
		}

		// Types of all of a view's annotations, and with `describe` each one.
		template <typename Info>
		void DescribeViewAnnotations(IView* view, bool describe, PersistIds& ids, ComponentList* components,
			DrawingBudget& budget, Info* info)
		{
			for (const auto& annotation : ViewAnnotationList(view))
			{
				long type = 0;
				annotation->GetType(&type);
				info->add_annotation_types(static_cast<int32_t>(type));
				if (!describe)
					continue;
				if (budget.annotations == 0)
				{
					budget.truncated = true;
					continue;
				}
				--budget.annotations;
				DescribeAnnotation(annotation, ids, components, info->add_annotations());
			}
		}

		// The letter of a section, detail or auxiliary view, its crop and its breaks.
		void DescribeViewShape(IView* view, const Eigen::Vector2d& viewPosition, sld_mcp::v1::ViewInfo* info)
		{
			ATL::CComBSTR label;
			const long type = ViewType(view);
			ATL::CComPtr<IDispatch> disp;
			if (type == swDrawingSectionView && SUCCEEDED(view->GetSection(&disp)) && disp)
			{
				if (ATL::CComQIPtr<IDrSection> section(disp.p); section)
					section->GetLabel(&label);
			}
			else if (type == swDrawingDetailView && SUCCEEDED(view->GetDetail(&disp)) && disp)
			{
				if (ATL::CComQIPtr<IDetailCircle> detail(disp.p); detail)
					detail->GetLabel(&label);
			}
			else if (type == swDrawingAuxiliaryView && SUCCEEDED(view->GetProjectionArrow(&disp)) && disp)
			{
				if (ATL::CComQIPtr<IProjectionArrow> arrow(disp.p); arrow)
					arrow->GetLabel(&label);
			}
			info->set_label(ToUtf8(label));

			VARIANT_BOOL flag = VARIANT_FALSE;
			if (SUCCEEDED(view->IsCropped(&flag)))
				info->set_cropped(flag != VARIANT_FALSE);
			flag = VARIANT_FALSE;
			if (SUCCEEDED(view->IsBroken(&flag)))
				info->set_broken(flag != VARIANT_FALSE);
			ATL::CComVariant lines;
			if (FAILED(view->GetBreakLines(&lines)))
				return;
			for (const auto& line : comvariant::ToDispatches(lines))
			{
				ATL::CComQIPtr<IBreakLine> breakLine(line.p);
				if (!breakLine)
					continue;
				long orientation = 0, style = 0;
				double first = 0.0, second = 0.0;
				breakLine->get_Orientation(&orientation);
				breakLine->get_Style(&style);
				breakLine->GetPosition(0, &first);
				breakLine->GetPosition(1, &second);
				auto* out = info->add_breaks();
				const bool alongX = orientation == swBreakLineVertical;
				const double origin = alongX ? viewPosition.x() : viewPosition.y();
				out->set_along_x(alongX);
				out->set_from(origin + first);
				out->set_to(origin + second);
				out->set_style(static_cast<int32_t>(style));
			}
		}

		void DescribeView(IView* view, IModelDocExtension* drawingExt, IMathUtility* math, bool listEntities,
			bool includeVertices, bool includeAnnotations, DrawingBudget& budget, sld_mcp::v1::ViewInfo* info)
		{
			info->set_name(ViewName(view));
			info->set_type(static_cast<int32_t>(ViewType(view)));
			ATL::CComBSTR text;
			if (SUCCEEDED(view->GetOrientationName(&text)))
				info->set_orientation(ToUtf8(text));
			text.Empty();
			if (SUCCEEDED(view->GetReferencedModelName(&text)))
				info->set_model_file(ToUtf8(text));
			text.Empty();
			if (SUCCEEDED(view->get_ReferencedConfiguration(&text)))
				info->set_configuration(ToUtf8(text));
			VARIANT_BOOL loaded = VARIANT_FALSE;
			if (SUCCEEDED(view->IsModelLoaded(&loaded)))
				info->set_model_loaded(loaded != VARIANT_FALSE);

			ATL::CComVariant ratio;
			if (SUCCEEDED(view->get_ScaleRatio(&ratio)))
				if (const std::vector<double> r = comvariant::ToDoubles(ratio); r.size() >= 2)
				{
					info->set_scale_numerator(r[0]);
					info->set_scale_denominator(r[1]);
				}
			long useSheetScale = 0;
			if (SUCCEEDED(view->get_UseSheetScale(&useSheetScale)))
				info->set_uses_sheet_scale(useSheetScale != 0);
			VARIANT_BOOL useParent = VARIANT_FALSE;
			if (SUCCEEDED(view->get_UseParentScale(&useParent)))
				info->set_uses_parent_scale(useParent != VARIANT_FALSE);
			useParent = VARIANT_FALSE;
			if (SUCCEEDED(view->GetUseParentDisplayMode(&useParent)))
				info->set_uses_parent_display(useParent != VARIANT_FALSE);

			// [xmin, ymin, xmax, ymax] and [x, y], sheet coordinates.
			ATL::CComVariant outline;
			if (SUCCEEDED(view->GetOutline(&outline)))
				if (const std::vector<double> o = comvariant::ToDoubles(outline); o.size() >= 4)
				{
					const double lo[3] = { o[0], o[1], 0.0 }, hi[3] = { o[2], o[3], 0.0 };
					SetVec3(info->mutable_outline()->mutable_min(), lo);
					SetVec3(info->mutable_outline()->mutable_max(), hi);
				}
			ATL::CComVariant position;
			Eigen::Vector2d viewPosition = Eigen::Vector2d::Zero();
			if (SUCCEEDED(view->get_Position(&position)))
				if (const std::vector<double> p = comvariant::ToDoubles(position); p.size() >= 2)
				{
					const double at[3] = { p[0], p[1], 0.0 };
					SetVec3(info->mutable_position(), at);
					viewPosition = Eigen::Vector2d(p[0], p[1]);
				}
			double angle = 0.0;
			if (SUCCEEDED(view->get_Angle(&angle)) && std::isfinite(angle))
				info->set_angle(angle);
			info->set_display_mode(static_cast<int32_t>(ViewDisplayMode(view)));
			ATL::CComPtr<IDispatch> baseDisp;
			if (SUCCEEDED(view->GetBaseView(&baseDisp)) && baseDisp)
				if (ATL::CComQIPtr<IView> base(baseDisp.p); base)
					info->set_base_view(ViewName(base));
			DescribeViewTransform(view, math, info);
			DescribeViewShape(view, viewPosition, info);

			const ViewModel model = ViewModelOf(view);
			if (model.doc)
				info->set_model_type(ToDocumentType(model.type));
			// View entities (and what annotations are attached to) by the
			// drawing's persist ids, components by the assembly's.
			PersistIds ids(drawingExt);
			PersistIds modelIds(model.ext);
			std::optional<ComponentList> components;
			if (model.type == swDocASSEMBLY)
				components.emplace(modelIds, math, info->mutable_components(),
					ATL::CComQIPtr<IAssemblyDoc>(model.doc.p));

			if (listEntities)
			{
				info->set_entities_listed(true);
				std::vector<sld_mcp::v1::ViewEntityKind> kinds = { sld_mcp::v1::VIEW_ENTITY_KIND_EDGE,
					sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE };
				if (includeVertices)
					kinds.push_back(sld_mcp::v1::VIEW_ENTITY_KIND_VERTEX);
				std::vector<ViewEntityItem> items;
				for (const auto& component : ViewComponents(view, model))
				{
					const std::string componentPid = components ? components->Add(component) : std::string();
					for (const auto kind : kinds)
						if (!ListViewEntities(view, component, componentPid, kind, ids, budget.entities, items))
							budget.truncated = true;
				}
				for (const auto& item : items)
					DescribeViewEntity(item, viewPosition, ids, info->add_entities());
			}
			DescribeViewAnnotations(view, includeAnnotations, ids, components ? &*components : nullptr, budget, info);
		}
	}

	void DescribeSheet(const DrawingSheet& sheet, const std::string& activeName, IModelDocExtension* drawingExt,
		IMathUtility* math, const std::set<std::string>& entityViews, bool includeVertices, bool includeAnnotations,
		DrawingBudget& budget, sld_mcp::v1::SheetInfo* info, bool describeSheetFormat)
	{
		info->set_name(sheet.name);
		info->set_active(sheet.name == activeName);
		info->set_paper_size(-1);
		if (sheet.sheet)
		{
			if (SheetSetup setup; ReadSheetSetup(sheet.sheet, setup).ok())
				SetSheetInfo(setup, info);
			ATL::CComBSTR format;
			if (SUCCEEDED(sheet.sheet->GetSheetFormatName(&format)))
				info->set_sheet_format(ToUtf8(format));
		}
		for (const auto& view : sheet.views)
			DescribeView(view, drawingExt, math, entityViews.count(ViewName(view)) > 0, includeVertices,
				includeAnnotations, budget, info->add_views());
		if (!sheet.sheetView)
			return;
		// The sheet's own list holds its sheet format's annotations too
		// (the title block's notes); counted apart.
		PersistIds ids(drawingExt);
		for (const auto& annotation : ViewAnnotationList(sheet.sheetView))
		{
			long type = 0;
			annotation->GetType(&type);
			const bool format = AnnotationOwnerOf(annotation, nullptr) == sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT;
			if (format)
				info->add_sheet_format_annotation_types(static_cast<int32_t>(type));
			else
				info->add_annotation_types(static_cast<int32_t>(type));
			if (!includeAnnotations || (format && !describeSheetFormat))
				continue;
			if (budget.annotations == 0)
			{
				budget.truncated = true;
				continue;
			}
			--budget.annotations;
			DescribeAnnotation(annotation, ids, nullptr, info->add_annotations());
		}
	}

	grpc::Status ResolveDrawingRef(DrawingViewContext& ctx, const sld_mcp::v1::DrawingEntityRef& ref,
		const std::string& where, ViewEntityItem& out)
	{
		ATL::CComPtr<IComponent2> component;
		if (!ref.component_persist_id().empty())
		{
			if (ctx.componentList)
				for (const auto& c : ctx.components)
					if (c && ctx.componentList->Add(c) == ref.component_persist_id())
					{
						component = c;
						break;
					}
			if (!component)
				return grpc::Status(grpc::StatusCode::NOT_FOUND, where +
					": its component isn't visible in " + ViewName(ctx.view) + " any more -- inspect the drawing again");
		}
		auto key = std::make_pair(static_cast<int>(ref.kind()), ref.component_persist_id());
		auto it = ctx.entities.find(key);
		if (it == ctx.entities.end())
		{
			std::vector<ViewEntityItem> items;
			size_t unlimited = std::numeric_limits<size_t>::max();
			ListViewEntities(ctx.view, component, ref.component_persist_id(), ref.kind(), *ctx.ids, unlimited, items);
			it = ctx.entities.emplace(key, std::move(items)).first;
		}
		for (const auto& item : it->second)
			if (item.persistId == ref.persist_id() && item.ordinal == ref.ordinal())
			{
				out = item;
				return grpc::Status::OK;
			}
		return grpc::Status(grpc::StatusCode::NOT_FOUND, where + ": that entity isn't visible in " +
			ViewName(ctx.view) + " any more (was the drawing or the model changed?) -- inspect the drawing again "
			"for current ids");
	}

	bool SelectViewEntity(IView* view, const ViewEntityItem& item)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (SUCCEEDED(view->SelectEntity(item.obj, VARIANT_TRUE, &selected)) && selected)
			return true;
		if (ATL::CComQIPtr<ISilhouetteEdge> silhouette(item.obj.p); silhouette)
		{
			selected = VARIANT_FALSE;
			return SUCCEEDED(silhouette->Select2(VARIANT_TRUE, nullptr, &selected)) && selected;
		}
		return false;
	}

	bool SelectViewEntityAt(IModelDoc2* doc, IView* view, const ViewEntityItem& item,
		const std::optional<Eigen::Vector2d>& at, long mark)
	{
		ATL::CComPtr<ISelectionMgr> selections;
		ATL::CComPtr<ISelectData> data;
		if (FAILED(doc->get_ISelectionManager(&selections)) || !selections ||
			FAILED(selections->CreateSelectData(&data)) || !data)
			return false;
		data->put_Mark(mark);
		data->put_View(view);
		if (at)
		{
			data->put_X(at->x());
			data->put_Y(at->y());
			data->put_Z(0.0);
		}
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (ATL::CComQIPtr<ISilhouetteEdge> silhouette(item.obj.p); silhouette)
			silhouette->Select2(VARIANT_TRUE, data, &selected);
		else if (ATL::CComQIPtr<IEntity> entity(item.obj.p); entity)
			entity->Select4(VARIANT_TRUE, data, &selected);
		return selected != VARIANT_FALSE;
	}

	bool EqualsIgnoringCase(const std::string& a, const std::string& b)
	{
		return _wcsicmp(FromUtf8(a), FromUtf8(b)) == 0;
	}

	// SolidWorks refuses a sheet name another sheet has in any case.
	bool SameSheetName(const std::string& a, const std::string& b)
	{
		return EqualsIgnoringCase(a, b);
	}

	const DrawingSheet* FindSheet(const std::vector<DrawingSheet>& sheets, const std::string& name)
	{
		for (const auto& s : sheets)
			if (s.name == name)
				return &s;
		for (const auto& s : sheets)
			if (SameSheetName(s.name, name))
				return &s;
		return nullptr;
	}

	std::string SheetList(const std::vector<DrawingSheet>& sheets)
	{
		std::string list;
		for (const auto& s : sheets)
			list += (list.empty() ? "" : ", ") + s.name;
		return list;
	}

	grpc::Status ToSheetPoint(const std::vector<DrawingSheet>& sheets, IMathUtility* math,
		const sld_mcp::v1::DrawingPoint& point, const std::string& what, Eigen::Vector2d& out)
	{
		if (!std::isfinite(point.x()) || !std::isfinite(point.y()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, what + " must be finite");
		if (point.view_name().empty())
		{
			out = Eigen::Vector2d(point.x(), point.y());
			return grpc::Status::OK;
		}
		ATL::CComPtr<IView> view;
		std::string sheetName;
		if (grpc::Status status = FindDrawingView(sheets, point.view_name(), view, sheetName); !status.ok())
			return status;
		ATL::CComPtr<IMathTransform> xform;
		Eigen::Vector3d origin;
		if (FAILED(view->get_ModelToViewTransform(&xform)) || !xform ||
			!TransformPoint(math, xform, Eigen::Vector3d::Zero(), origin))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't say where " + point.view_name() + "'s model origin is: give sheet coordinates");
		double scale = 1.0;
		ATL::CComVariant ratio;
		if (SUCCEEDED(view->get_ScaleRatio(&ratio)))
			if (const std::vector<double> r = comvariant::ToDoubles(ratio); r.size() >= 2 && r[1] > 0.0)
				scale = r[0] / r[1];
		double angle = 0.0;
		if (FAILED(view->get_Angle(&angle)) || !std::isfinite(angle))
			angle = 0.0;
		out = origin.head<2>() + scale * (Eigen::Rotation2Dd(angle) * Eigen::Vector2d(point.x(), point.y()));
		return grpc::Status::OK;
	}

	long TitleRows(ITableAnnotation* table)
	{
		VARIANT_BOOL visible = VARIANT_FALSE;
		return SUCCEEDED(table->get_TitleVisible(&visible)) && visible ? 1 : 0;
	}

	ATL::CComPtr<ITableAnnotation> TableOf(IAnnotation* annotation)
	{
		long type = 0;
		ATL::CComPtr<IDispatch> specific;
		if (!annotation || FAILED(annotation->GetType(&type)) || type != swTableAnnotation ||
			FAILED(annotation->GetSpecificAnnotation(&specific)) || !specific)
			return {};
		ATL::CComQIPtr<ITableAnnotation> table(specific.p);
		return ATL::CComPtr<ITableAnnotation>(table);
	}

	void DescribeTable(ITableAnnotation* table, sld_mcp::v1::TableInfo* info)
	{
		ATL::CComPtr<IAnnotation> annotation;
		if (SUCCEEDED(table->GetAnnotation(&annotation)) && annotation)
		{
			ATL::CComBSTR name;
			if (SUCCEEDED(annotation->GetName(&name)))
				info->set_name(ToUtf8(name));
			std::string ownerName;
			info->set_owner(AnnotationOwnerOf(annotation, &ownerName));
			info->set_owner_name(ownerName);
			ATL::CComVariant positionVar;
			if (SUCCEEDED(annotation->GetPosition(&positionVar)))
				if (const std::vector<double> p = comvariant::ToDoubles(positionVar); p.size() >= 3)
					SetVec3(info->mutable_position(), p.data());
			ATL::CComBSTR layer;
			if (SUCCEEDED(annotation->get_Layer(&layer)))
				info->set_layer(ToUtf8(layer));
		}
		long value = 0;
		if (SUCCEEDED(table->get_Type(&value)))
			info->set_type(static_cast<int32_t>(value));
		value = 0;
		if (SUCCEEDED(table->get_AnchorType(&value)))
			info->set_anchor(static_cast<int32_t>(value));
		VARIANT_BOOL flag = VARIANT_FALSE;
		ATL::CComBSTR title;
		const long titleRows = TitleRows(table);
		value = 0;
		// A revision table's count takes in its shown title (2 with the column
		// titles, seen 2026-11-01), which `rows` leaves out.
		if (SUCCEEDED(table->GetHeaderCount(&value)))
			info->set_header_rows(static_cast<int32_t>(std::max(0L, value - titleRows)));
		if (ATL::CComQIPtr<IRevisionTableAnnotation> revisions(table); revisions)
		{
			ATL::CComBSTR current;
			if (SUCCEEDED(revisions->get_CurrentRevision(&current)))
				info->set_current_revision(ToUtf8(current));
		}
		if (titleRows > 0 && SUCCEEDED(table->get_Title(&title)))
		{
			info->set_title(ToUtf8(title));
			double height = 0.0;
			table->GetRowHeight(0, &height);
			info->set_title_height(height);
		}

		long rows = 0, columns = 0;
		table->get_TotalRowCount(&rows);
		table->get_TotalColumnCount(&columns);
		ATL::CComQIPtr<IBomTableAnnotation> bom(table);
		for (long c = 0; c < columns; ++c)
		{
			double width = 0.0;
			table->GetColumnWidth(c, &width);
			info->add_column_widths(width);
			flag = VARIANT_FALSE;
			table->get_ColumnHidden(c, &flag);
			info->add_column_hidden(flag != VARIANT_FALSE);
			ATL::CComVariant propertyData;
			long status = 0, type = 0;
			if (FAILED(table->GetColumnType3(c, VARIANT_TRUE, &propertyData, &status, &type)))
				type = 0;
			info->add_column_types(static_cast<int32_t>(type));
			ATL::CComBSTR property;
			if (bom && type == swBomTableColumnType_CustomProperty)
				bom->GetColumnCustomProperty(c, &property);
			info->add_column_properties(ToUtf8(property));
		}

		ATL::CComBSTR configuration;
		if (bom)
		{
			ATL::CComPtr<IBomFeature> feature;
			if (SUCCEEDED(bom->get_BomFeature(&feature)) && feature)
			{
				long type = 0;
				if (SUCCEEDED(feature->get_TableType(&type)))
					info->set_bom_type(static_cast<int32_t>(type));
				ATL::CComBSTR model, featureName;
				if (SUCCEEDED(feature->GetReferencedModelName(&model)))
					info->set_bom_model_file(ToUtf8(model));
				if (SUCCEEDED(feature->get_Name(&featureName)))
					info->set_bom_feature(ToUtf8(featureName));
				// The configuration listed: the first of the BOM's (a BOM can
				// list several, one quantity column each).
				ATL::CComVariant visible, names;
				if (SUCCEEDED(feature->GetConfigurations(VARIANT_TRUE, &visible, &names)))
					if (const std::vector<ATL::CComBSTR> list = comvariant::ToBstrs(names); !list.empty())
						configuration = list[0];
				info->set_bom_configuration(ToUtf8(configuration));
			}
		}

		long cells = 0;
		for (long r = titleRows; r < rows; ++r)
		{
			if (cells + columns > kMaxTableCells)
			{
				info->set_truncated(true);
				break;
			}
			auto* row = info->add_rows();
			double height = 0.0;
			table->GetRowHeight(r, &height);
			info->add_row_heights(height);
			flag = VARIANT_FALSE;
			table->get_RowHidden(r, &flag);
			row->set_hidden(flag != VARIANT_FALSE);
			for (long c = 0; c < columns; ++c, ++cells)
			{
				ATL::CComBSTR text;
				table->get_DisplayedText2(r, c, VARIANT_TRUE, &text);
				row->add_cells(ToUtf8(text));
			}
			ATL::CComVariant components;
			if (bom && r - titleRows >= info->header_rows() && SUCCEEDED(bom->GetComponents2(r, configuration, &components)))
				for (const auto& disp : comvariant::ToDispatches(components))
					if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
					{
						ATL::CComBSTR name;
						if (SUCCEEDED(component->get_Name2(&name)))
							row->add_components(ToUtf8(name));
					}
		}
	}

	std::vector<ATL::CComPtr<ITableAnnotation>> SheetTables(const DrawingSheet& sheet)
	{
		std::vector<ATL::CComPtr<ITableAnnotation>> out;
		std::vector<ATL::CComPtr<IUnknown>> seen;
		auto consider = [&](IView* view) {
			for (const auto& annotation : ViewAnnotationList(view))
				if (ATL::CComPtr<ITableAnnotation> table = TableOf(annotation))
				{
					if (std::any_of(seen.begin(), seen.end(), [&](const ATL::CComPtr<IUnknown>& s) {
						return SameObject(s, table); }))
						continue;
					seen.push_back(ATL::CComPtr<IUnknown>(table));
					out.push_back(table);
				}
		};
		if (sheet.sheetView)
			consider(sheet.sheetView);
		for (const auto& view : sheet.views)
			consider(view);
		return out;
	}

	void DescribeSheetLayout(const DrawingSheet& sheet, sld_mcp::v1::SheetInfo* info)
	{
		for (const auto& table : SheetTables(sheet))
			DescribeTable(table, info->add_tables());
		if (!sheet.sheet)
			return;
		// The format's sketch is in sheet coordinates, like its notes.
		ATL::CComPtr<ISketch> sketch;
		ATL::CComVariant segments;
		if (FAILED(sheet.sheet->GetTemplateSketch(&sketch)) || !sketch || FAILED(sketch->GetSketchSegments(&segments)))
			return;
		for (const auto& disp : comvariant::ToDispatches(segments))
		{
			ATL::CComQIPtr<ISketchSegment> segment(disp.p);
			long type = -1;
			ATL::CComQIPtr<ISketchLine> line(disp.p);
			if (!segment || !line || FAILED(segment->GetType(&type)) || type != swSketchLINE)
				continue;
			ATL::CComPtr<IDispatch> startDisp, endDisp;
			line->GetStartPoint2(&startDisp);
			line->GetEndPoint2(&endDisp);
			ATL::CComQIPtr<ISketchPoint> start(startDisp.p), end(endDisp.p);
			if (!start || !end)
				continue;
			double p[6] = {};
			start->get_X(&p[0]);
			start->get_Y(&p[1]);
			end->get_X(&p[3]);
			end->get_Y(&p[4]);
			auto* out = info->add_format_lines();
			SetVec3(out->mutable_start(), p);
			SetVec3(out->mutable_end(), p + 3);
			ATL::CComBSTR layer;
			if (SUCCEEDED(segment->get_Layer(&layer)))
				out->set_layer(ToUtf8(layer));
		}
	}
}
