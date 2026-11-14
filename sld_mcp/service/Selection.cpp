// Selection.cpp : UserSelection, the active document's selection read (and set)
// for the model (see docs/notes/selection.md).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
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

	// Fills `item` from selected object `index` (1-based).
	void DescribeSelected(ISelectionMgr* selections, long index, IModelDocExtension* ext, PersistIds& ids,
		ComponentList* components, sld_mcp::v1::SelectedItem* item)
	{
		long type = 0, mark = 0;
		selections->GetSelectedObjectType3(index, -1, &type);
		selections->GetSelectedObjectMark(index, &mark);
		item->set_select_type(static_cast<int32_t>(type));
		item->set_mark(static_cast<int32_t>(mark));
		ATL::CComPtr<IDispatch> obj;
		if (FAILED(selections->GetSelectedObject6(index, -1, &obj)) || !obj)
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

		ATL::CComVariant point;
		if (picked && SUCCEEDED(selections->GetSelectionPoint2(index, -1, &point)))
			if (const std::vector<double> p = comvariant::ToDoubles(point); p.size() >= 3)
				SetVec3(item->mutable_point(), p.data());
	}
}

ExecutionPolicy sld_mcp::service::PolicyFor(const sld_mcp::v1::SelectionRequest& request)
{
	const bool changesSelection = request.clear() || !request.select().empty();
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
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
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

		if (request->clear() || (!request->select().empty() && !request->append()))
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

		PersistIds ids(ext);
		ATL::CComQIPtr<IMathUtility> math;
		std::optional<ComponentList> components;
		if (docType == swDocASSEMBLY)
		{
			if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
				return status;
			components.emplace(ids, math, response->mutable_components());
		}
		long count = 0;
		selections->GetSelectedObjectCount2(-1, &count);
		for (long i = 1; i <= count; ++i)
		{
			control.Checkpoint();
			DescribeSelected(selections, i, ext, ids, components ? &*components : nullptr, response->add_items());
		}
		return grpc::Status::OK;
	});
}
