// DrawingCenterMarks.cpp : Center marks and centerlines of a drawing view: AddCenterMarks, AddCenterlines
// (see docs/notes/drawings.md, "Center marks and centerlines").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxCenterEntities = 200;

	// AutoInsertCenterMarks2's size and gap, which it ignores with the
	// document's settings asked for.
	constexpr double kAutoMarkSize = 0.0025;
	constexpr double kAutoMarkGap = 0.0005;

	std::string AnnotationName(IAnnotation* annotation)
	{
		ATL::CComBSTR name;
		annotation->GetName(&name);
		return ToUtf8(name);
	}

	// A view's annotations of one swAnnotationType_e, each with the persist
	// ids of what it's attached to (silhouettes have none).
	struct AttachedAnnotation
	{
		ATL::CComPtr<IAnnotation> annotation;
		std::set<std::string> on;
		size_t attachedCount = 0;
	};

	AttachedAnnotation WithAttached(IAnnotation* annotation, PersistIds& ids)
	{
		AttachedAnnotation out;
		out.annotation = annotation;
		ATL::CComVariant attached;
		if (SUCCEEDED(annotation->GetAttachedEntities3(&attached)))
			for (const auto& disp : comvariant::ToDispatches(attached))
			{
				++out.attachedCount;
				sld_mcp::v1::AttachedEntity entity;
				DescribeAttached(disp, ids, nullptr, &entity);
				if (!entity.persist_id().empty())
					out.on.insert(entity.persist_id());
			}
		return out;
	}

	std::vector<AttachedAnnotation> AnnotationsOfType(IView* view, long type, PersistIds& ids)
	{
		std::vector<AttachedAnnotation> out;
		for (const auto& annotation : ViewAnnotationList(view))
			if (long now = 0; SUCCEEDED(annotation->GetType(&now)) && now == type)
				out.push_back(WithAttached(annotation, ids));
		return out;
	}

	// The view's annotations that weren't in `before`.
	std::vector<ATL::CComPtr<IAnnotation>> NewAnnotations(IView* view, const std::vector<ATL::CComPtr<IAnnotation>>& before)
	{
		std::vector<ATL::CComPtr<IAnnotation>> out;
		for (const auto& annotation : ViewAnnotationList(view))
			if (std::none_of(before.begin(), before.end(), [&](const ATL::CComPtr<IAnnotation>& b) {
				return SameObject(b, annotation); }))
				out.push_back(annotation);
		return out;
	}

	// The reply's common part: what was added and the sheet as it is now.
	void FinishReply(IDrawingDoc* drawing, IModelDocExtension* ext, IMathUtility* math, DrawingViewContext& ctx,
		const std::string& original, const std::vector<ATL::CComPtr<IAnnotation>>& added,
		sld_mcp::v1::AddCenterMarksResponse* response)
	{
		for (const auto& annotation : added)
			DescribeAnnotation(annotation, *ctx.ids, ctx.componentList.get(), response->add_added());
		for (const auto& s : DrawingSheets(drawing))
			if (s.name == ctx.sheetName)
			{
				DrawingBudget budget;
				DescribeSheet(s, original, ext, math, {}, false, false, budget, response->mutable_sheet());
			}
		response->set_active_sheet(original);
	}

	grpc::Status MakeViewContext(const std::vector<DrawingSheet>& sheets, IModelDocExtension* ext, IMathUtility* math,
		const std::string& viewName, sld_mcp::v1::AddCenterMarksResponse* response, DrawingViewContext& ctx)
	{
		if (grpc::Status status = FindDrawingView(sheets, viewName, ctx.view, ctx.sheetName); !status.ok())
			return status;
		ctx.model = ViewModelOf(ctx.view);
		ctx.ids = std::make_unique<PersistIds>(ext);
		ctx.modelIds = std::make_unique<PersistIds>(ctx.model.ext);
		ctx.components = ViewComponents(ctx.view, ctx.model);
		if (ctx.model.type == swDocASSEMBLY)
			ctx.componentList = std::make_unique<ComponentList>(*ctx.modelIds, math, response->mutable_components(),
				ATL::CComQIPtr<IAssemblyDoc>(ctx.model.doc.p));
		return grpc::Status::OK;
	}

	// All of the view's holes, fillets or slots. SolidWorks marks every one
	// each time it's asked (seen 2026-11-01), so a new mark on a circle that
	// had one is deleted again.
	grpc::Status AutoMarks(IModelDoc2* doc, IModelDocExtension* ext, DrawingViewContext& ctx, long types,
		std::vector<ATL::CComPtr<IAnnotation>>& added, sld_mcp::v1::AddCenterMarksResponse* response)
	{
		const std::vector<ATL::CComPtr<IAnnotation>> before = ViewAnnotationList(ctx.view);
		std::set<std::string> marked;
		for (const auto& mark : AnnotationsOfType(ctx.view, swCenterMarkSym, *ctx.ids))
			marked.insert(mark.on.begin(), mark.on.end());
		const VARIANT_BOOL slots = (types & swAutoInsertCenterMarkType_Slots) ? VARIANT_TRUE : VARIANT_FALSE;
		VARIANT_BOOL ok = VARIANT_FALSE;
		HRESULT hr = ctx.view->AutoInsertCenterMarks2(types, 0, slots, slots, VARIANT_TRUE, kAutoMarkSize, kAutoMarkGap,
			VARIANT_FALSE, VARIANT_FALSE, 0.0, &ok);
		if (FAILED(hr))
			return ComError("IView::AutoInsertCenterMarks2", hr);
		std::vector<ATL::CComPtr<IAnnotation>> duplicates;
		for (const auto& annotation : NewAnnotations(ctx.view, before))
		{
			const AttachedAnnotation mark = WithAttached(annotation, *ctx.ids);
			if (std::any_of(mark.on.begin(), mark.on.end(), [&](const std::string& id) { return marked.count(id) > 0; }))
				duplicates.push_back(annotation);
			else
			{
				added.push_back(annotation);
				marked.insert(mark.on.begin(), mark.on.end());
			}
		}
		if (!duplicates.empty() && !DeleteAnnotations(doc, ext, duplicates))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks made " + std::to_string(duplicates.size()) +
				" mark(s) over circles that had one and wouldn't delete them");
		response->set_duplicates_removed(static_cast<int32_t>(duplicates.size()));
		return grpc::Status::OK;
	}

	grpc::Status InsertMark(IDrawingDoc* drawing, long style, ATL::CComPtr<ICenterMark>& mark)
	{
		HRESULT hr = drawing->InsertCenterMark3(style, VARIANT_FALSE, VARIANT_FALSE, &mark);
		if (FAILED(hr))
			return ComError("IDrawingDoc::InsertCenterMark3", hr);
		if (!mark)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't add a center mark (are they circles or arcs seen face-on?)");
		return grpc::Status::OK;
	}

	grpc::Status MarksOnCircles(IModelDoc2* doc, IDrawingDoc* drawing, IModelDocExtension* ext, DrawingViewContext& ctx,
		const sld_mcp::v1::AddCenterMarksRequest& r, std::vector<ATL::CComPtr<IAnnotation>>& added,
		sld_mcp::v1::AddCenterMarksResponse* response)
	{
		const long style = r.style() > 0 ? r.style() : static_cast<long>(swCenterMark_Single);
		std::vector<ViewEntityItem> items(static_cast<size_t>(r.circles_size()));
		std::set<std::string> asked;
		for (int i = 0; i < r.circles_size(); ++i)
		{
			if (grpc::Status status = ResolveDrawingRef(ctx, r.circles(i), "circle " + std::to_string(i + 1),
				items[static_cast<size_t>(i)]); !status.ok())
				return status;
			if (items[static_cast<size_t>(i)].kind != sld_mcp::v1::VIEW_ENTITY_KIND_EDGE)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "circle " + std::to_string(i + 1) +
					" must be an edge (a circle or arc seen face-on)");
			asked.insert(items[static_cast<size_t>(i)].persistId);
		}
		const std::vector<AttachedAnnotation> existing = AnnotationsOfType(ctx.view, swCenterMarkSym, *ctx.ids);
		const std::vector<ATL::CComPtr<IAnnotation>> before = ViewAnnotationList(ctx.view);

		auto select = [&](const ViewEntityItem& item) {
			return SelectViewEntityAt(doc, ctx.view, item, std::nullopt, 0);
		};
		auto fail = [&](grpc::Status why) {
			doc->ClearSelection2(VARIANT_TRUE);
			const auto made = NewAnnotations(ctx.view, before);
			if (!made.empty())
				DeleteAnnotations(doc, ext, made);
			added.clear();
			return why;
		};

		if (style == swCenterMark_Single)
		{
			// SolidWorks would draw a second mark over a marked circle (seen
			// 2026-11-01), and most templates mark circles when a view is made.
			for (size_t i = 0; i < items.size(); ++i)
			{
				if (std::any_of(existing.begin(), existing.end(), [&](const AttachedAnnotation& m) {
					return m.on.count(items[i].persistId) > 0; }))
				{
					response->add_already_marked(static_cast<int32_t>(i));
					continue;
				}
				doc->ClearSelection2(VARIANT_TRUE);
				if (!select(items[i]))
					return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select circle " + std::to_string(i + 1)));
				ATL::CComPtr<ICenterMark> mark;
				if (grpc::Status status = InsertMark(drawing, style, mark); !status.ok())
					return fail(status);
				ATL::CComPtr<IAnnotation> annotation;
				if (FAILED(mark->GetAnnotation(&annotation)) || !annotation)
					return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave the center mark no annotation"));
				added.push_back(annotation);
			}
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status::OK;
		}

		// A group takes the place of its circles' own marks, but not of part
		// of another group.
		std::vector<ATL::CComPtr<IAnnotation>> replaced;
		for (const auto& mark : existing)
		{
			const size_t overlap = static_cast<size_t>(std::count_if(mark.on.begin(), mark.on.end(),
				[&](const std::string& id) { return asked.count(id) > 0; }));
			if (overlap == 0)
				continue;
			if (overlap < mark.attachedCount)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, AnnotationName(mark.annotation) +
					" marks these circles and others: delete it first (delete_drawing_annotations)");
			replaced.push_back(mark.annotation);
		}
		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t i = 0; i < items.size(); ++i)
			if (!select(items[i]))
				return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select circle " + std::to_string(i + 1)));
		ATL::CComPtr<ICenterMark> mark;
		if (grpc::Status status = InsertMark(drawing, style, mark); !status.ok())
			return fail(status);
		doc->ClearSelection2(VARIANT_TRUE);
		// Circles that don't suit the group come back as single marks
		// (circular over three in a row, seen 2026-11-01).
		// A circular group counts the mark at the bolt circle's center too.
		long nowStyle = 0, count = 0;
		mark->get_Style(&nowStyle);
		mark->get_GroupCount(&count);
		const long expected = r.circles_size() + (style == swCenterMark_CircularGroup ? 1 : 0);
		if (nowStyle != style || count != expected || NewAnnotations(ctx.view, before).size() != 1)
			return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't make one " +
				std::string(style == swCenterMark_LinearGroup ? "linear" : "circular") + " group of those circles (do they lie " +
				(style == swCenterMark_LinearGroup ? "in a line?)" : "on a common circle?)")));
		ATL::CComPtr<IAnnotation> annotation;
		if (FAILED(mark->GetAnnotation(&annotation)) || !annotation)
			return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave the center mark no annotation"));
		added.push_back(annotation);
		std::vector<std::string> names;
		for (const auto& old : replaced)
			names.push_back(AnnotationName(old));
		if (!replaced.empty() && !DeleteAnnotations(doc, ext, replaced))
			return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't delete the circles' own marks"));
		for (const auto& name : names)
			response->add_replaced(name);
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::AddCenterMarks(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddCenterMarksRequest* request,
	sld_mcp::v1::AddCenterMarksResponse* response)
{
	if (request->view_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which view");
	if (request->circles_size() > kMaxCenterEntities)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "at most " + std::to_string(kMaxCenterEntities) + " circles per call");
	if (request->style() != 0 && request->style() != swCenterMark_Single && request->style() != swCenterMark_LinearGroup &&
		request->style() != swCenterMark_CircularGroup)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown center mark style " + std::to_string(request->style()));
	if (request->circles_size() == 0 && request->style() != 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a group needs its circles");
	if (request->auto_types() < 0 || request->auto_types() > 7 || (request->circles_size() > 0 && request->auto_types() != 0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give circles, or the kinds to mark throughout the view");

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
	DrawingViewContext ctx;
	if (grpc::Status status = MakeViewContext(sheets, ext, math, request->view_name(), response, ctx); !status.ok())
		return status;

	std::vector<ATL::CComPtr<IAnnotation>> added;
	{
		SheetActivation activation(drawing);
		if (!activation.Show(ctx.sheetName))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + ctx.sheetName);
		EditModeScope editMode(drawing);
		editMode.Set(false);
		VARIANT_BOOL ok = VARIANT_FALSE;
		drawing->ActivateView(FromUtf8(request->view_name()), &ok);
		grpc::Status status = request->circles_size() > 0
			? MarksOnCircles(doc, drawing, ext, ctx, *request, added, response)
			: AutoMarks(doc, ext, ctx, request->auto_types() > 0 ? request->auto_types() : static_cast<long>(swAutoInsertCenterMarkType_Hole),
				added, response);
		doc->GraphicsRedraw2();
		if (!status.ok())
			return status;
	}
	FinishReply(drawing, ext, math, ctx, original, added, response);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddCenterlines(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddCenterlinesRequest* request,
	sld_mcp::v1::AddCenterMarksResponse* response)
{
	if (request->view_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which view");
	if (request->pairs_size() > kMaxCenterEntities)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "at most " + std::to_string(kMaxCenterEntities) + " centerlines per call");
	for (int i = 0; i < request->pairs_size(); ++i)
		if (request->pairs(i).entities_size() != 2)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "centerline " + std::to_string(i + 1) + " goes between 2 entities");

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
	DrawingViewContext ctx;
	if (grpc::Status status = MakeViewContext(sheets, ext, math, request->view_name(), response, ctx); !status.ok())
		return status;

	std::vector<ATL::CComPtr<IAnnotation>> added;
	{
		SheetActivation activation(drawing);
		if (!activation.Show(ctx.sheetName))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + ctx.sheetName);
		EditModeScope editMode(drawing);
		editMode.Set(false);
		VARIANT_BOOL ok = VARIANT_FALSE;
		drawing->ActivateView(FromUtf8(request->view_name()), &ok);

		const std::vector<ATL::CComPtr<IAnnotation>> start = ViewAnnotationList(ctx.view);
		auto fail = [&](grpc::Status why) {
			doc->ClearSelection2(VARIANT_TRUE);
			const auto made = NewAnnotations(ctx.view, start);
			if (!made.empty())
				DeleteAnnotations(doc, ext, made);
			doc->GraphicsRedraw2();
			return why;
		};
		if (request->pairs_size() == 0)
		{
			// With the view selected SolidWorks adds those it finds; none new
			// is a null result, not an error.
			if (!SelectDrawingView(doc, ext, request->view_name()))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select " + request->view_name());
			ATL::CComPtr<ICenterLine> line;
			HRESULT hr = drawing->InsertCenterLine2(&line);
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IDrawingDoc::InsertCenterLine2", hr);
		}
		else
		{
			const std::vector<AttachedAnnotation> existing = AnnotationsOfType(ctx.view, swCenterLine, *ctx.ids);
			for (int i = 0; i < request->pairs_size(); ++i)
			{
				const std::string which = "centerline " + std::to_string(i + 1);
				ViewEntityItem a, b;
				for (auto [ref, item] : { std::make_pair(&request->pairs(i).entities(0), &a), std::make_pair(&request->pairs(i).entities(1), &b) })
					if (grpc::Status status = ResolveDrawingRef(ctx, *ref, which, *item); !status.ok())
						return fail(status);
				// (A silhouette has no persist id of its own to compare.)
				if (a.kind == sld_mcp::v1::VIEW_ENTITY_KIND_EDGE && b.kind == sld_mcp::v1::VIEW_ENTITY_KIND_EDGE &&
					std::any_of(existing.begin(), existing.end(), [&](const AttachedAnnotation& l) {
						return l.on.count(a.persistId) > 0 && l.on.count(b.persistId) > 0; }))
				{
					response->add_already_marked(i);
					continue;
				}
				doc->ClearSelection2(VARIANT_TRUE);
				if (!SelectViewEntityAt(doc, ctx.view, a, std::nullopt, 0) || !SelectViewEntityAt(doc, ctx.view, b, std::nullopt, 0))
					return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select " + which + "'s entities"));
				ATL::CComPtr<ICenterLine> line;
				HRESULT hr = drawing->InsertCenterLine2(&line);
				if (FAILED(hr))
					return fail(ComError("IDrawingDoc::InsertCenterLine2", hr));
				if (!line)
					return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"SolidWorks didn't add " + which + " (are the two parallel?)"));
			}
			doc->ClearSelection2(VARIANT_TRUE);
		}
		added = NewAnnotations(ctx.view, start);
		doc->GraphicsRedraw2();
	}
	FinishReply(drawing, ext, math, ctx, original, added, response);
	return grpc::Status::OK;
}
