// DrawingViews.cpp : Views made from other views and views' shapes: DeriveDrawingView, ShapeDrawingView
// (see docs/notes/drawings.md, "Derived views").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// swCommands_Tools_Crop_Delete: there's no other way to take a crop off.
	constexpr long kRemoveCropCommand = 1389;
	// What the UI starts with for jagged outlines and break lines.
	constexpr long kShapeIntensity = 5;

	struct ViewCall
	{
		ATL::CComPtr<ISldWorks> swApp;
		ATL::CComPtr<IModelDoc2> doc;
		ATL::CComQIPtr<IDrawingDoc> drawing;
		ATL::CComPtr<IModelDocExtension> ext;
		ATL::CComQIPtr<IMathUtility> math;
		ATL::CComPtr<IView> view;
		std::string sheetName;
		std::string original;  // the sheet showing before the call
	};

	// `call.swApp` is set.
	grpc::Status OpenViewCall(const std::string& viewName, ViewCall& call)
	{
		if (grpc::Status status = GetActiveDrawing(call.swApp, call.doc, call.drawing); !status.ok())
			return status;
		if (grpc::Status status = GetExtension(call.doc, call.ext); !status.ok())
			return status;
		if (grpc::Status status = GetMathUtility(call.swApp, call.math); !status.ok())
			return status;
		call.original = ActiveSheetName(call.drawing);
		return FindDrawingView(DrawingSheets(call.drawing), viewName, call.view, call.sheetName);
	}

	Eigen::Vector2d ViewPosition(IView* view)
	{
		ATL::CComVariant position;
		if (SUCCEEDED(view->get_Position(&position)))
			if (const std::vector<double> p = comvariant::ToDoubles(position); p.size() >= 2)
				return Eigen::Vector2d(p[0], p[1]);
		return Eigen::Vector2d::Zero();
	}

	// A view's sketch has its origin at IView::Position, the model's size
	// and the sheet's axes (seen 2026-11-01, also where Position isn't the
	// model origin).
	struct ViewSketch
	{
		Eigen::Vector2d position = Eigen::Vector2d::Zero();
		double scale = 1.0;

		Eigen::Vector2d Point(const sld_mcp::v1::Vec3& sheet) const
		{
			return (Eigen::Vector2d(sheet.x(), sheet.y()) - position) / scale;
		}
	};

	grpc::Status ReadViewSketch(IView* view, ViewSketch& sketch)
	{
		double angle = 0.0;
		if (SUCCEEDED(view->get_Angle(&angle)) && std::abs(angle) > 1e-9)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				ViewName(view) + " is rotated on the sheet: this only works on a view that isn't");
		sketch.position = ViewPosition(view);
		ATL::CComVariant ratio;
		if (SUCCEEDED(view->get_ScaleRatio(&ratio)))
			if (const std::vector<double> r = comvariant::ToDoubles(ratio); r.size() >= 2 && r[0] > 0.0 && r[1] > 0.0)
				sketch.scale = r[0] / r[1];
		return grpc::Status::OK;
	}

	// Draws in a view's sketch, at exactly the coordinates given: the view is
	// active and what's drawn selected until the view is made from it.
	class ViewSketchScope
	{
	public:
		ViewSketchScope(ViewCall& call) : m_call(call)
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			call.doc->ClearSelection2(VARIANT_TRUE);
			call.drawing->ActivateView(FromUtf8(ViewName(call.view)), &ok);
			if (SUCCEEDED(call.doc->get_SketchManager(&m_sketches)) && m_sketches)
			{
				m_sketches->get_AddToDB(&m_addToDB);
				m_sketches->put_AddToDB(VARIANT_TRUE);
			}
		}
		~ViewSketchScope()
		{
			if (m_sketches)
				m_sketches->put_AddToDB(m_addToDB);
		}
		ViewSketchScope(const ViewSketchScope&) = delete;
		ViewSketchScope& operator=(const ViewSketchScope&) = delete;

		ISketchManager* Sketches() const { return m_sketches; }

		// Deletes what was drawn, when nothing was made from it.
		void DeleteDrawn(const std::vector<ATL::CComPtr<ISketchSegment>>& segments)
		{
			m_call.doc->ClearSelection2(VARIANT_TRUE);
			bool any = false;
			for (const auto& segment : segments)
			{
				VARIANT_BOOL selected = VARIANT_FALSE;
				if (segment && SUCCEEDED(segment->Select4(VARIANT_TRUE, nullptr, &selected)) && selected)
					any = true;
			}
			VARIANT_BOOL deleted = VARIANT_FALSE;
			if (any)
				m_call.ext->DeleteSelection2(0, &deleted);
			m_call.doc->ClearSelection2(VARIANT_TRUE);
		}

	private:
		ViewCall& m_call;
		ATL::CComPtr<ISketchManager> m_sketches;
		VARIANT_BOOL m_addToDB = VARIANT_FALSE;
	};

	grpc::Status Refused(const std::string& message)
	{
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, message);
	}

	grpc::Status MakeSection(ViewCall& call, const sld_mcp::v1::DeriveDrawingViewRequest& request, ATL::CComPtr<IView>& made)
	{
		const auto& spec = request.section();
		ViewSketch sketch;
		if (grpc::Status status = ReadViewSketch(call.view, sketch); !status.ok())
			return status;
		const Eigen::Vector2d start = sketch.Point(spec.start()), end = sketch.Point(spec.end());
		ViewSketchScope scope(call);
		ATL::CComPtr<ISketchSegment> line;
		if (!scope.Sketches() ||
			FAILED(scope.Sketches()->CreateLine(start.x(), start.y(), 0.0, end.x(), end.y(), 0.0, &line)) || !line)
			return Refused("SolidWorks wouldn't draw the section line in " + ViewName(call.view));
		const long options = spec.surface_only() ? swCreateSectionView_DisplaySurfaceCut : 0;
		// No components left out; an empty VARIANT here is not what it takes.
		ATL::CComVariant excluded(static_cast<IDispatch*>(nullptr));
		HRESULT hr = call.drawing->CreateSectionViewAt5(request.position().x(), request.position().y(), 0.0,
			FromUtf8(request.label()), options, excluded, spec.depth(), &made);
		if (FAILED(hr) || !made)
		{
			scope.DeleteDrawn({ line });
			return FAILED(hr) ? ComError("IDrawingDoc::CreateSectionViewAt5", hr)
				: Refused("SolidWorks didn't make a section view from that line");
		}
		return grpc::Status::OK;
	}

	grpc::Status MakeDetail(ViewCall& call, const sld_mcp::v1::DeriveDrawingViewRequest& request, ATL::CComPtr<IView>& made)
	{
		const auto& spec = request.detail();
		ViewSketch sketch;
		if (grpc::Status status = ReadViewSketch(call.view, sketch); !status.ok())
			return status;
		const Eigen::Vector2d center = sketch.Point(spec.center());
		ViewSketchScope scope(call);
		ATL::CComPtr<ISketchSegment> circle;
		if (!scope.Sketches() ||
			FAILED(scope.Sketches()->CreateCircleByRadius(center.x(), center.y(), 0.0, spec.radius() / sketch.scale, &circle)) ||
			!circle)
			return Refused("SolidWorks wouldn't draw the detail circle in " + ViewName(call.view));
		ATL::CComPtr<IDispatch> disp;
		HRESULT hr = call.drawing->CreateDetailViewAt4(request.position().x(), request.position().y(), 0.0, spec.style(),
			spec.scale().numerator(), spec.scale().denominator(), FromUtf8(request.label()), swDetCircleCIRCLE,
			spec.full_outline() ? VARIANT_TRUE : VARIANT_FALSE, spec.jagged_outline() ? VARIANT_TRUE : VARIANT_FALSE,
			spec.no_outline() ? VARIANT_TRUE : VARIANT_FALSE, kShapeIntensity, &disp);
		ATL::CComQIPtr<IView> view(disp.p);
		if (FAILED(hr) || !view)
		{
			scope.DeleteDrawn({ circle });
			return FAILED(hr) ? ComError("IDrawingDoc::CreateDetailViewAt4", hr)
				: Refused("SolidWorks didn't make a detail view from that circle");
		}
		made = view;
		return grpc::Status::OK;
	}

	grpc::Status MakeAuxiliary(ViewCall& call, const sld_mcp::v1::DeriveDrawingViewRequest& request, ATL::CComPtr<IView>& made)
	{
		const auto& spec = request.auxiliary();
		if (spec.edge().view_name() != request.view_name())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the edge must be in " + request.view_name());
		DrawingViewContext ctx;
		ctx.view = call.view;
		ctx.sheetName = call.sheetName;
		ctx.model = ViewModelOf(call.view);
		ctx.ids = std::make_unique<PersistIds>(call.ext);
		ctx.modelIds = std::make_unique<PersistIds>(ctx.model.ext);
		ctx.components = ViewComponents(call.view, ctx.model);
		sld_mcp::v1::ViewInfo components;
		if (ctx.model.type == swDocASSEMBLY)
			ctx.componentList = std::make_unique<ComponentList>(*ctx.modelIds, call.math, components.mutable_components(),
				ATL::CComQIPtr<IAssemblyDoc>(ctx.model.doc.p));
		ViewEntityItem edge;
		if (grpc::Status status = ResolveDrawingRef(ctx, spec.edge(), "the edge", edge); !status.ok())
			return status;
		if (edge.kind != sld_mcp::v1::VIEW_ENTITY_KIND_EDGE)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "an auxiliary view needs a model edge of the view");

		VARIANT_BOOL ok = VARIANT_FALSE;
		call.doc->ClearSelection2(VARIANT_TRUE);
		call.drawing->ActivateView(FromUtf8(request.view_name()), &ok);
		if (!SelectViewEntity(call.view, edge))
			return Refused("SolidWorks wouldn't select that edge");
		ATL::CComPtr<IDispatch> disp;
		HRESULT hr = call.drawing->CreateAuxiliaryViewAt2(request.position().x(), request.position().y(), 0.0, VARIANT_FALSE,
			FromUtf8(request.label()), VARIANT_TRUE, spec.flip() ? VARIANT_TRUE : VARIANT_FALSE, &disp);
		if (FAILED(hr))
			return ComError("IDrawingDoc::CreateAuxiliaryViewAt2", hr);
		ATL::CComQIPtr<IView> view(disp.p);
		if (!view)
			return Refused("SolidWorks didn't make an auxiliary view from that edge: it takes a straight edge at an "
				"angle the view can look square onto");
		made = view;
		return grpc::Status::OK;
	}

	grpc::Status MakeProjected(ViewCall& call, const sld_mcp::v1::DeriveDrawingViewRequest& request, ATL::CComPtr<IView>& made)
	{
		if (!SelectDrawingView(call.doc, call.ext, request.view_name()))
			return Refused("SolidWorks wouldn't select " + request.view_name());
		HRESULT hr = call.drawing->CreateUnfoldedViewAt3(request.position().x(), request.position().y(), 0.0, VARIANT_FALSE, &made);
		if (FAILED(hr))
			return ComError("IDrawingDoc::CreateUnfoldedViewAt3", hr);
		if (!made)
			return Refused("SolidWorks didn't project a view from " + request.view_name());
		return grpc::Status::OK;
	}

	// The view's sheet with all its views into the reply.
	void FinishViewCall(ViewCall& call, const std::string& viewName, bool created, sld_mcp::v1::SetupDrawingViewResponse* response)
	{
		for (const auto& s : DrawingSheets(call.drawing))
			if (s.name == call.sheetName)
			{
				DrawingBudget budget;
				DescribeSheet(s, call.original, call.ext, call.math, {}, false, false, budget, response->mutable_sheet());
			}
		response->set_view_name(viewName);
		response->set_created(created);
		response->set_active_sheet(call.original);
	}

	void EndViewEdit(ViewCall& call)
	{
		call.doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL ok = VARIANT_FALSE;
		call.drawing->ActivateView(ATL::CComBSTR(L""), &ok);
		call.doc->EditRebuild3(&ok);
		call.doc->GraphicsRedraw2();
	}

	bool ViewIs(IView* view, HRESULT(__stdcall IView::* query)(VARIANT_BOOL*))
	{
		VARIANT_BOOL flag = VARIANT_FALSE;
		return SUCCEEDED((view->*query)(&flag)) && flag != VARIANT_FALSE;
	}

	bool IsCropped(IView* view) { return ViewIs(view, &IView::IsCropped); }
	bool IsBroken(IView* view) { return ViewIs(view, &IView::IsBroken); }

	// The segments of a view's own sketch by type and id (ids are per type).
	std::map<std::pair<long, std::vector<long long>>, ATL::CComPtr<ISketchSegment>> ViewSketchSegments(IView* view)
	{
		std::map<std::pair<long, std::vector<long long>>, ATL::CComPtr<ISketchSegment>> out;
		ATL::CComPtr<ISketch> sketch;
		ATL::CComVariant segments;
		if (FAILED(view->IGetSketch(&sketch)) || !sketch || FAILED(sketch->GetSketchSegments(&segments)))
			return out;
		for (const auto& item : comvariant::ToDispatches(segments))
		{
			ATL::CComQIPtr<ISketchSegment> segment(item.p);
			if (!segment)
				continue;
			long type = 0;
			ATL::CComVariant id;
			segment->GetType(&type);
			segment->GetID(&id);
			out.emplace(std::make_pair(type, comvariant::ToInt64s(id)), ATL::CComPtr<ISketchSegment>(segment));
		}
		return out;
	}

	grpc::Status RemoveCrop(ViewCall& call)
	{
		const std::string name = ViewName(call.view);
		// The profile of a crop isn't among the view's sketch segments, and
		// comes back as loose lines when the crop goes (seen 2026-11-01): what's
		// new after is the profile, whatever else the user drew in the view.
		const auto before = ViewSketchSegments(call.view);
		VARIANT_BOOL ran = VARIANT_FALSE;
		if (!SelectDrawingView(call.doc, call.ext, name) ||
			FAILED(call.ext->RunCommand(kRemoveCropCommand, ATL::CComBSTR(L""), &ran)) || IsCropped(call.view))
			return Refused("SolidWorks didn't take the crop off " + name);
		std::vector<ATL::CComPtr<ISketchSegment>> profile;
		for (const auto& [key, segment] : ViewSketchSegments(call.view))
			if (!before.count(key))
				profile.push_back(segment);
		ViewSketchScope scope(call);
		scope.DeleteDrawn(profile);
		return grpc::Status::OK;
	}

	grpc::Status CropView(ViewCall& call, const sld_mcp::v1::CropViewSpec& spec)
	{
		const std::string name = ViewName(call.view);
		// Crop2 fails on a cropped view (returns 0, seen 2026-11-01), so an
		// old crop goes first: refusals that can come after that are ruled
		// out here.
		if (ViewType(call.view) == swDrawingDetailView || IsBroken(call.view))
			return Refused(name + " is a detail or broken view: SolidWorks can't crop those (take the breaks off first)");
		ViewSketch sketch;
		if (grpc::Status status = ReadViewSketch(call.view, sketch); !status.ok())
			return status;
		if (IsCropped(call.view))
			if (grpc::Status status = RemoveCrop(call); !status.ok())
				return status;

		ViewSketchScope scope(call);
		if (!scope.Sketches())
			return Refused("no sketch manager");
		std::vector<ATL::CComPtr<ISketchSegment>> drawn;
		if (spec.radius() > 0.0)
		{
			const Eigen::Vector2d center = sketch.Point(spec.center());
			ATL::CComPtr<ISketchSegment> circle;
			if (FAILED(scope.Sketches()->CreateCircleByRadius(center.x(), center.y(), 0.0, spec.radius() / sketch.scale, &circle)) ||
				!circle)
				return Refused("SolidWorks wouldn't draw the crop circle in " + name);
			drawn.push_back(circle);
		}
		else
		{
			const Eigen::Vector2d a = sketch.Point(spec.corner1()), b = sketch.Point(spec.corner2());
			ATL::CComVariant lines;
			if (FAILED(scope.Sketches()->CreateCornerRectangle(a.x(), a.y(), 0.0, b.x(), b.y(), 0.0, &lines)))
				return Refused("SolidWorks wouldn't draw the crop rectangle in " + name);
			for (const auto& line : comvariant::ToDispatches(lines))
				if (ATL::CComQIPtr<ISketchSegment> segment(line.p); segment)
					drawn.push_back(ATL::CComPtr<ISketchSegment>(segment));
			if (drawn.empty())
				return Refused("SolidWorks wouldn't draw the crop rectangle in " + name);
		}
		long result = swCropViewErrors_Unknown;
		HRESULT hr = call.view->Crop2(spec.jagged_outline() ? VARIANT_TRUE : VARIANT_FALSE,
			spec.no_outline() ? VARIANT_TRUE : VARIANT_FALSE, kShapeIntensity, &result);
		if (FAILED(hr) || result != swCropViewErrors_NoError || !IsCropped(call.view))
		{
			scope.DeleteDrawn(drawn);
			if (FAILED(hr))
				return ComError("IView::Crop2", hr);
			return Refused("SolidWorks didn't crop " + name + (result == swCropViewErrors_IncorrectProfile
				? ": it doesn't take that profile" : " (error " + std::to_string(result) + ")"));
		}
		return grpc::Status::OK;
	}

	void DeleteBreakLines(ViewCall& call, const std::vector<ATL::CComQIPtr<IBreakLine>>& lines)
	{
		for (const auto& line : lines)
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (line && SUCCEEDED(line->Select(VARIANT_FALSE, nullptr, &ok)) && ok)
				call.ext->DeleteSelection2(0, &ok);
		}
		call.doc->ClearSelection2(VARIANT_TRUE);
	}

	grpc::Status BreakView(ViewCall& call, const sld_mcp::v1::BreakViewSpec& spec)
	{
		const std::string name = ViewName(call.view);
		if (IsCropped(call.view))
			return Refused(name + " is cropped: SolidWorks can't break a cropped view (take the crop off first)");
		ViewSketch sketch;
		if (grpc::Status status = ReadViewSketch(call.view, sketch); !status.ok())
			return status;
		call.doc->ClearSelection2(VARIANT_TRUE);
		// Unlike sketch coordinates, these are sheet lengths from the view's
		// position, whatever the view's scale (seen 2026-11-01).
		std::vector<ATL::CComQIPtr<IBreakLine>> inserted;
		for (const auto& b : spec.breaks())
		{
			const double origin = b.along_x() ? sketch.position.x() : sketch.position.y();
			ATL::CComPtr<IDispatch> disp;
			HRESULT hr = call.view->InsertBreak3(b.along_x() ? swBreakLineVertical : swBreakLineHorizontal, b.from() - origin,
				b.to() - origin, spec.style(), kShapeIntensity, VARIANT_FALSE, &disp);
			ATL::CComQIPtr<IBreakLine> line(disp.p);
			if (FAILED(hr) || !line)
			{
				DeleteBreakLines(call, inserted);
				return FAILED(hr) ? ComError("IView::InsertBreak3", hr)
					: Refused("SolidWorks didn't take that break in " + name);
			}
			inserted.push_back(line);
		}
		if (!SelectDrawingView(call.doc, call.ext, name))
		{
			DeleteBreakLines(call, inserted);
			return Refused("SolidWorks wouldn't select " + name);
		}
		HRESULT hr = call.drawing->BreakView();
		if (FAILED(hr) || !IsBroken(call.view))
		{
			DeleteBreakLines(call, inserted);
			return FAILED(hr) ? ComError("IDrawingDoc::BreakView", hr) : Refused("SolidWorks didn't break " + name);
		}
		return grpc::Status::OK;
	}

	grpc::Status RemoveBreaks(ViewCall& call)
	{
		const std::string name = ViewName(call.view);
		ATL::CComVariant lines;
		call.view->GetBreakLines(&lines);
		std::vector<ATL::CComQIPtr<IBreakLine>> all;
		for (const auto& line : comvariant::ToDispatches(lines))
			all.emplace_back(line.p);
		if (all.empty() && !IsBroken(call.view))
			return Refused(name + " has no breaks");
		// Break lines stay after UnBreakView, ready to break it again.
		if (!SelectDrawingView(call.doc, call.ext, name))
			return Refused("SolidWorks wouldn't select " + name);
		HRESULT hr = call.drawing->UnBreakView();
		if (FAILED(hr))
			return ComError("IDrawingDoc::UnBreakView", hr);
		if (IsBroken(call.view))
			return Refused("SolidWorks didn't unbreak " + name);
		DeleteBreakLines(call, all);
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::DeriveDrawingView(grpc::ServerContext* context,
	const sld_mcp::v1::DeriveDrawingViewRequest* request,
	sld_mcp::v1::SetupDrawingViewResponse* response)
{
	using Request = sld_mcp::v1::DeriveDrawingViewRequest;
	if (request->kind_case() == Request::KIND_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which kind of view to make");
	if (!request->has_position())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say where the view goes");

	return m_executor.Execute(context, "DeriveDrawingView", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ViewCall call;
		call.swApp = swApp;
		if (grpc::Status status = OpenViewCall(request->view_name(), call); !status.ok())
			return status;
		if (!request->new_name().empty())
		{
			ATL::CComPtr<IView> taken;
			std::string takenSheet;
			if (FindDrawingView(DrawingSheets(call.drawing), request->new_name(), taken, takenSheet).ok())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the drawing already has a view named \"" + request->new_name() + "\" (on " + takenSheet + ")");
		}

		std::string finalName;
		{
			SheetActivation activation(call.drawing);
			if (!activation.Show(call.sheetName))
				return Refused("couldn't switch to sheet " + call.sheetName);
			ATL::CComPtr<IView> made;
			grpc::Status status = grpc::Status::OK;
			switch (request->kind_case())
			{
			case Request::kProjected:
				status = MakeProjected(call, *request, made);
				break;
			case Request::kSection:
				status = MakeSection(call, *request, made);
				break;
			case Request::kDetail:
				status = MakeDetail(call, *request, made);
				break;
			default:
				status = MakeAuxiliary(call, *request, made);
				break;
			}
			if (!status.ok())
			{
				EndViewEdit(call);
				return status;
			}
			if (!request->new_name().empty())
			{
				VARIANT_BOOL ok = VARIANT_FALSE;
				made->SetName2(FromUtf8(request->new_name()), &ok);
				if (ViewName(made) != request->new_name())
				{
					DeleteDrawingView(call.doc, call.ext, ViewName(made));
					EndViewEdit(call);
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						"SolidWorks didn't take \"" + request->new_name() + "\" as the view's name");
				}
			}
			finalName = ViewName(made);
			EndViewEdit(call);
		}
		FinishViewCall(call, finalName, true, response);
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::ShapeDrawingView(grpc::ServerContext* context,
	const sld_mcp::v1::ShapeDrawingViewRequest* request,
	sld_mcp::v1::SetupDrawingViewResponse* response)
{
	using Request = sld_mcp::v1::ShapeDrawingViewRequest;
	if (request->change_case() == Request::CHANGE_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say what to do to the view");

	return m_executor.Execute(context, "ShapeDrawingView", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ViewCall call;
		call.swApp = swApp;
		if (grpc::Status status = OpenViewCall(request->view_name(), call); !status.ok())
			return status;

		{
			SheetActivation activation(call.drawing);
			if (!activation.Show(call.sheetName))
				return Refused("couldn't switch to sheet " + call.sheetName);
			grpc::Status status = grpc::Status::OK;
			switch (request->change_case())
			{
			case Request::kCrop:
				status = CropView(call, request->crop());
				break;
			case Request::kBreaks:
				status = BreakView(call, request->breaks());
				break;
			case Request::kRemoveCrop:
				status = IsCropped(call.view) ? RemoveCrop(call) : Refused(request->view_name() + " isn't cropped");
				break;
			default:
				status = RemoveBreaks(call);
				break;
			}
			EndViewEdit(call);
			if (!status.ok())
				return status;
		}
		FinishViewCall(call, request->view_name(), false, response);
		return grpc::Status::OK;
	});
}
