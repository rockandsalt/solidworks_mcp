// Selection.cpp : UserSelection, the active document's selection read (and set)
// for the model (see docs/notes/selection.md).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// Selects one entity of the request, added to the selection. Unlike
	// ResolvePick, any feature, body or component is taken: the selection is
	// the point, not a feature's reference.
	grpc::Status SelectOne(IModelDoc2* doc, IModelDocExtension* ext, ISelectData* data,
		const sld_mcp::v1::GeometryRef& ref, const std::string& label)
	{
		using Ref = sld_mcp::v1::GeometryRef;
		ATL::CComPtr<IDispatch> obj;
		std::string what;
		if (ref.target_case() == Ref::kModelPersistId)
		{
			if (grpc::Status status = ResolvePersistId(ext, ref.model_persist_id(), obj); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
		}
		else if (ref.target_case() == Ref::kFeatureName)
		{
			ATL::CComPtr<IFeature> feature;
			if (grpc::Status status = FeatureByName(doc, ref.feature_name(), feature); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
			obj = feature;
		}

		VARIANT_BOOL selected = VARIANT_FALSE;
		bool ok = false;
		data->put_Mark(0);
		if (!obj)
		{
			// Sketch entities, the origin, default planes: as references are.
			Pick pick;
			if (grpc::Status status = ResolvePick(doc, ext, ref, pick); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
			ok = SelectPick(ext, data, pick, 0);
			what = pick.what;
		}
		else if (ATL::CComQIPtr<IFeature> feature(obj.p); feature)
		{
			ok = SUCCEEDED(feature->Select2(VARIANT_TRUE, 0, &selected)) && selected;
			what = FeatureName(feature);
		}
		else if (ATL::CComQIPtr<IComponent2> component(obj.p); component)
		{
			ok = SUCCEEDED(component->Select4(VARIANT_TRUE, data, VARIANT_FALSE, &selected)) && selected;
			what = ComponentName(component);
		}
		else if (ATL::CComQIPtr<IBody2> body(obj.p); body)
		{
			ok = SUCCEEDED(body->Select2(VARIANT_TRUE, data, &selected)) && selected;
			what = "body";
		}
		else if (ATL::CComQIPtr<IEntity> entity(obj.p); entity)
		{
			ok = SUCCEEDED(entity->Select4(VARIANT_TRUE, data, &selected)) && selected;
			what = "entity";
		}
		else
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + " can't be selected");
		if (!ok)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks wouldn't select " + label + " (" + what + ") -- is it hidden or suppressed?");
		return grpc::Status::OK;
	}

	// The sketch a sketch segment or point is in; null otherwise.
	ATL::CComPtr<ISketch> SketchOfItem(IDispatch* obj)
	{
		ATL::CComPtr<ISketch> sketch;
		if (ATL::CComQIPtr<ISketchSegment> segment(obj); segment)
			segment->GetSketch(&sketch);
		else if (ATL::CComQIPtr<ISketchPoint> point(obj); point)
			point->GetSketch(&sketch);
		return sketch;
	}

	// Selected object `index` (1-based), with its select type and mark put
	// in `item`; null if SolidWorks gives no object.
	ATL::CComPtr<IDispatch> SelectedObject(ISelectionMgr* selections, long index, sld_mcp::v1::SelectedItem* item)
	{
		long type = 0, mark = 0;
		selections->GetSelectedObjectType3(index, -1, &type);
		selections->GetSelectedObjectMark(index, &mark);
		item->set_select_type(static_cast<int32_t>(type));
		item->set_mark(static_cast<int32_t>(mark));
		ATL::CComPtr<IDispatch> obj;
		if (FAILED(selections->GetSelectedObject6(index, -1, &obj)))
			obj.Release();
		return obj;
	}

	// Where selected object `index` was picked (model / assembly space;
	// drawings: sheet meters).
	void SetPickPoint(ISelectionMgr* selections, long index, sld_mcp::v1::SelectedItem* item)
	{
		ATL::CComVariant point;
		if (SUCCEEDED(selections->GetSelectionPoint2(index, -1, &point)))
			if (const std::vector<double> p = comvariant::ToDoubles(point); p.size() >= 3)
				SetVec3(item->mutable_point(), p.data());
	}

	// Fills `item` from selected object `index` (1-based).
	void DescribeSelected(ISelectionMgr* selections, long index, IModelDocExtension* ext, PersistIds& ids,
		ComponentList* components, sld_mcp::v1::SelectedItem* item)
	{
		const ATL::CComPtr<IDispatch> obj = SelectedObject(selections, index, item);
		if (!obj)
			return;

		// Assemblies: the component the item is in (or is).
		std::string componentPid;
		if (components)
		{
			ATL::CComPtr<IDispatch> disp;
			if (SUCCEEDED(selections->GetSelectedObjectsComponent4(index, -1, &disp)) && disp)
				if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
					componentPid = components->Add(component);
		}

		bool picked = true;  // a place on the model, so the pick point means something
		if (ATL::CComQIPtr<IFace2> face(obj.p); face)
		{
			InspectFace(face, ids, item->mutable_face());
			item->mutable_face()->set_component_persist_id(componentPid);
		}
		else if (ATL::CComQIPtr<IEdge> edge(obj.p); edge)
		{
			InspectEdge(edge, ids, item->mutable_edge());
			item->mutable_edge()->set_component_persist_id(componentPid);
		}
		else if (ATL::CComQIPtr<IVertex> vertex(obj.p); vertex)
		{
			sld_mcp::v1::VertexInfo* info = item->mutable_vertex();
			info->set_persist_id(ids.Get(vertex));
			info->set_component_persist_id(componentPid);
			ATL::CComVariant at;
			if (SUCCEEDED(vertex->GetPoint(&at)))
				if (const std::vector<double> p = comvariant::ToDoubles(at); p.size() >= 3)
					SetVec3(info->mutable_position(), p.data());
		}
		else if (ATL::CComPtr<ISketch> sketch = SketchOfItem(obj); sketch)
		{
			// A sketch is its feature (QueryInterface).
			ATL::CComQIPtr<IFeature> sketchFeature(sketch.p);
			DescribeSketchItem(obj, ids.Get(sketchFeature), item->mutable_sketch_item());
			if (sketchFeature)
				item->set_sketch_name(FeatureName(sketchFeature));
			VARIANT_BOOL is3d = VARIANT_FALSE;
			if (SUCCEEDED(sketch->Is3D(&is3d)))
				item->set_sketch_3d(is3d != VARIANT_FALSE);
		}
		else if (ATL::CComQIPtr<IComponent2> component(obj.p); component)
		{
			item->set_component_persist_id(componentPid.empty() ? ids.Get(component) : componentPid);
			picked = false;
		}
		else if (ATL::CComQIPtr<IFeature> feature(obj.p); feature)
		{
			// Reference features of a component would need its own extension.
			DescribeFeature(feature, FeatureTypeName(feature), ids, componentPid,
				componentPid.empty() ? ext : nullptr, item->mutable_feature());
			picked = false;
		}
		else if (ATL::CComQIPtr<IBody2> body(obj.p); body)
		{
			InspectBody(body, ids, item->mutable_body());
			item->mutable_body()->set_component_persist_id(componentPid);
			picked = false;
		}
		else
			picked = false;

		if (picked)
			SetPickPoint(selections, index, item);
	}

	// ---- Drawings ----------------------------------------------------------

	// A view the selection touches: what finding its entities needs.
	struct SelectionView
	{
		DrawingViewContext ctx;
		google::protobuf::RepeatedPtrField<sld_mcp::v1::ComponentInfo> components;  // ctx.componentList's
		bool described = false;  // in the reply's views
	};

	using SelectionViews = std::map<std::string, std::unique_ptr<SelectionView>>;

	SelectionView& ViewFor(SelectionViews& views, IView* view, IModelDocExtension* ext, IMathUtility* math)
	{
		auto& slot = views[ViewName(view)];
		if (!slot)
		{
			slot = std::make_unique<SelectionView>();
			DrawingViewContext& ctx = slot->ctx;
			ctx.view = view;
			ctx.model = ViewModelOf(view);
			ctx.ids = std::make_unique<PersistIds>(ext);
			ctx.modelIds = std::make_unique<PersistIds>(ctx.model.ext);
			ctx.components = ViewComponents(view, ctx.model);
			if (ctx.model.type == swDocASSEMBLY)
				ctx.componentList = std::make_unique<ComponentList>(*ctx.modelIds, math, &slot->components,
					ATL::CComQIPtr<IAssemblyDoc>(ctx.model.doc.p));
		}
		return *slot;
	}

	// The annotation a selected dimension, note, GTol, ... belongs to (they
	// all have GetAnnotation, on no shared interface); null for anything else.
	ATL::CComPtr<IAnnotation> AnnotationOfSelected(IDispatch* obj)
	{
		if (ATL::CComQIPtr<IAnnotation> annotation(obj); annotation)
			return ATL::CComPtr<IAnnotation>(annotation);
		ATL::CComVariant result;
		if (!InvokeByName(obj, L"GetAnnotation", result) || result.vt != VT_DISPATCH || !result.pdispVal)
			return {};
		ATL::CComQIPtr<IAnnotation> annotation(result.pdispVal);
		return ATL::CComPtr<IAnnotation>(annotation);
	}

	// The view an annotation belongs to; null for the sheet's and the format's.
	ATL::CComPtr<IView> ViewOfAnnotation(IAnnotation* annotation)
	{
		ATL::CComPtr<IDispatch> owner;
		if (FAILED(annotation->get_Owner(&owner)) || !owner)
			return {};
		ATL::CComQIPtr<IView> view(owner.p);
		return ATL::CComPtr<IView>(view);
	}

	// The view selected object `index` is in: GetSelectedObjectsDrawingView2,
	// else (SolidWorks gives none for a dimension) its annotation's owner.
	// Null for the sheet's and the format's annotations and anything else
	// not in a view.
	ATL::CComPtr<IView> ViewOfSelected(ISelectionMgr* selections, long index, IAnnotation* annotation)
	{
		ATL::CComPtr<IView> view;
		if (SUCCEEDED(selections->GetSelectedObjectsDrawingView2(index, -1, &view)) && view)
			return view;
		return annotation ? ViewOfAnnotation(annotation) : ATL::CComPtr<IView>();
	}

	// A feature picked under a view in a drawing's tree (the drawing's
	// stand-in for the model's) as the view model's feature, by that model's
	// persist ids. Left empty if the model's isn't found.
	void DescribeTreeFeature(DrawingViewContext& ctx, IDispatch* obj, IComponent2* component,
		sld_mcp::v1::SelectedItem* item)
	{
		ModelCounterparts* counterparts = CounterpartsOf(ctx);
		if (!counterparts)
			return;
		const ATL::CComQIPtr<IFeature> modelFeature(counterparts->Find(obj, component).p);
		if (!modelFeature)
			return;
		const std::string componentPid = component && ctx.componentList ? ctx.componentList->Add(component) : std::string();
		DescribeFeature(modelFeature, FeatureTypeName(modelFeature), *ctx.modelIds, componentPid, nullptr,
			item->mutable_feature());
	}

	// Fills `item` from selected object `index` (1-based) of a drawing.
	void DescribeDrawingSelected(ISelectionMgr* selections, long index, IModelDocExtension* ext, IMathUtility* math,
		SelectionViews& views, sld_mcp::v1::SelectionResponse* response, sld_mcp::v1::SelectedItem* item)
	{
		const ATL::CComPtr<IDispatch> obj = SelectedObject(selections, index, item);
		if (!obj)
			return;
		if (ATL::CComQIPtr<IView> view(obj.p); view)
		{
			item->set_drawing_view(ViewName(view));
			return;
		}
		const ATL::CComPtr<IAnnotation> annotation = AnnotationOfSelected(obj);
		const ATL::CComPtr<IView> view = ViewOfSelected(selections, index, annotation);
		if (!view)
		{
			if (annotation)
			{
				// On the sheet or its format.
				PersistIds ids(ext);
				DescribeAnnotation(annotation, ids, nullptr, item->mutable_annotation());
				item->set_view_name(item->annotation().owner_name());
			}
			return;
		}
		SelectionView& selected = ViewFor(views, view, ext, math);
		item->set_view_name(ViewName(view));
		if (!selected.described)
		{
			DescribeViewOnly(view, ext, math, response->add_views());
			selected.described = true;
		}
		if (annotation)
		{
			DescribeAnnotation(annotation, *selected.ctx.ids, selected.ctx.componentList.get(), item->mutable_annotation());
			return;
		}

		ATL::CComPtr<IDispatch> componentDisp;
		selections->GetSelectedObjectsComponent4(index, -1, &componentDisp);
		const ATL::CComQIPtr<IComponent2> component(componentDisp.p);
		if (ATL::CComQIPtr<IFeature>(obj.p).p)
		{
			DescribeTreeFeature(selected.ctx, obj, component, item);
			return;
		}
		sld_mcp::v1::ViewEntity entity;
		if (DescribeSelectedViewEntity(selected.ctx, obj, component, &entity))
		{
			*item->mutable_view_entity() = std::move(entity);
			SetPickPoint(selections, index, item);
		}
	}

	// Selects the view entities of the request, added to the selection. They
	// must be on one sheet, which is shown (and stays so). All are found
	// (and what they are in the model looked up) before any is selected:
	// silhouettes pick in the view, and a pick on something selected starts
	// dragging it (ListViewEntities).
	grpc::Status SelectViewEntities(IDrawingDoc* drawing, IModelDocExtension* ext, IMathUtility* math,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::DrawingEntityRef>& refs, SelectionViews& views)
	{
		const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
		SheetActivation activation(drawing);
		std::string sheet;
		std::vector<std::pair<ATL::CComPtr<IView>, ViewEntityItem>> found;
		for (int i = 0; i < refs.size(); ++i)
		{
			const std::string label = "entity " + std::to_string(i + 1);
			ATL::CComPtr<IView> view;
			std::string sheetName;
			if (grpc::Status status = FindDrawingView(sheets, refs[i].view_name(), view, sheetName); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
			if (sheet.empty())
			{
				sheet = sheetName;
				if (!activation.Show(sheet))
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheet);
				activation.Keep();
			}
			else if (sheetName != sheet)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + " is on sheet " + sheetName +
					", the ones before it on " + sheet + ": select entities of one sheet at a time");
			SelectionView& selected = ViewFor(views, view, ext, math);
			ViewEntityItem item;
			if (grpc::Status status = ResolveDrawingRef(selected.ctx, refs[i], label, item); !status.ok())
				return status;
			if (ModelCounterparts* counterparts = CounterpartsOf(selected.ctx))
			{
				sld_mcp::v1::ViewEntity warm;  // cached for describing it once selected
				counterparts->Describe(item, &warm);
			}
			found.emplace_back(view, std::move(item));
		}
		const ATL::CComQIPtr<IModelDoc2> doc(drawing);
		for (size_t i = 0; i < found.size(); ++i)
		{
			const auto& [view, item] = found[i];
			// ISilhouetteEdge::Select2 without select data replaces the
			// selection (with the view); with the view in it, it adds.
			const bool done = item.kind == sld_mcp::v1::VIEW_ENTITY_KIND_SILHOUETTE
				? SelectViewEntityAt(doc, view, item, std::nullopt, 0)
				: SelectViewEntity(view, item);
			if (!done)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select entity " +
					std::to_string(i + 1) + " in " + refs[static_cast<int>(i)].view_name());
		}
		return grpc::Status::OK;
	}
}

ExecutionPolicy sld_mcp::service::PolicyFor(const sld_mcp::v1::SelectionRequest& request)
{
	const bool changesSelection = request.clear() || !request.select().empty() ||
		!request.select_view_entities().empty();
	return changesSelection ? ExecutionPolicy::CompleteOperation : ExecutionPolicy::Cancellable;
}

grpc::Status SolidWorksService::UserSelection(grpc::ServerContext* context,
	const sld_mcp::v1::SelectionRequest* request,
	sld_mcp::v1::SelectionResponse* response)
{
	return m_executor.Execute(context, "UserSelection", PolicyFor(*request),
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		ATL::CComQIPtr<IDrawingDoc> drawing;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
		{
			if (!GetActiveDrawing(swApp, doc, drawing).ok())
				return status;
			docType = swDocDRAWING;
		}
		if (drawing && !request->select().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the active document is a drawing: select its view entities (ids from inspect_drawing)");
		if (!drawing && !request->select_view_entities().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"view entity ids are a drawing's, and the active document is a part or an assembly");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComPtr<ISelectionMgr> selections;
		HRESULT hr = doc->get_ISelectionManager(&selections);
		if (FAILED(hr) || !selections)
			return ComError("IModelDoc2::ISelectionManager", hr);

		ATL::CComBSTR title;
		doc->GetTitle(&title);
		response->mutable_document()->set_title(ToUtf8(title));
		response->mutable_document()->set_type(ToDocumentType(docType));

		const bool selecting = !request->select().empty() || !request->select_view_entities().empty();
		if (request->clear() || (selecting && !request->append()))
			doc->ClearSelection2(VARIANT_TRUE);
		if (!request->select().empty())
		{
			ATL::CComPtr<ISelectData> data;
			hr = selections->CreateSelectData(&data);
			if (FAILED(hr) || !data)
				return ComError("ISelectionMgr::CreateSelectData", hr);
			for (int i = 0; i < request->select_size(); ++i)
				if (grpc::Status status = SelectOne(doc, ext, data, request->select(i),
					"entity " + std::to_string(i + 1)); !status.ok())
					return status;
		}

		ATL::CComQIPtr<IMathUtility> math;
		long count = 0;
		if (drawing)
		{
			if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
				return status;
			SelectionViews views;
			if (!request->select_view_entities().empty())
				if (grpc::Status status = SelectViewEntities(drawing, ext, math, request->select_view_entities(), views);
					!status.ok())
					return status;
			selections->GetSelectedObjectCount2(-1, &count);
			for (long i = 1; i <= count; ++i)
			{
				control.Checkpoint();
				DescribeDrawingSelected(selections, i, ext, math, views, response, response->add_items());
			}
			return grpc::Status::OK;
		}

		PersistIds ids(ext);
		std::optional<ComponentList> components;
		if (docType == swDocASSEMBLY)
		{
			if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
				return status;
			components.emplace(ids, math, response->mutable_components());
		}
		selections->GetSelectedObjectCount2(-1, &count);
		for (long i = 1; i <= count; ++i)
		{
			control.Checkpoint();
			DescribeSelected(selections, i, ext, ids, components ? &*components : nullptr, response->add_items());
		}
		return grpc::Status::OK;
	});
}
