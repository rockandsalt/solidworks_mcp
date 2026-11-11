// Visibility.cpp : SetVisibility (hiding and showing bodies, components, sketches and reference geometry).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// More than anyone selects by hand.
	constexpr int kMaxItems = 500;

	// One thing to hide or show: only the member of its kind is set.
	struct Item
	{
		ATL::CComPtr<IBody2> body;
		ATL::CComPtr<IComponent2> component;
		ATL::CComPtr<IFeature> feature;
		bool sketch = false;  // of a feature: else reference geometry
		std::string name;

		IUnknown* Object() const
		{
			return body ? static_cast<IUnknown*>(body) : component ? static_cast<IUnknown*>(component)
				: static_cast<IUnknown*>(feature);
		}

		const char* Kind() const
		{
			return body ? "body" : component ? "component" : sketch ? "sketch" : "reference geometry";
		}

		bool Visible() const
		{
			if (body)
			{
				VARIANT_BOOL visible = VARIANT_TRUE;
				body->get_Visible(&visible);
				return visible != VARIANT_FALSE;
			}
			if (component)
			{
				VARIANT_BOOL hidden = VARIANT_FALSE;
				component->IsHidden(VARIANT_FALSE, &hidden);
				return hidden == VARIANT_FALSE;
			}
			long visibility = swVisibilityStateShown;
			feature->get_Visible(&visibility);
			return visibility != swVisibilityStateHide;
		}
	};

	grpc::Status ResolveItem(IModelDocExtension* ext, bool isAssembly, const std::vector<TreeFeature>& tree,
		PersistIds& ids, const sld_mcp::v1::FeatureTarget& target, Item& out)
	{
		if (target.target_case() == sld_mcp::v1::FeatureTarget::kPersistId)
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, target.persist_id(), obj); !status.ok())
				return status;
			if (ATL::CComQIPtr<IBody2> body(obj.p); body)
			{
				// HideBody would change the part's file, not this assembly.
				if (isAssembly)
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						"that body is in a component -- hide the component, or open its part (open_document) "
						"to hide the body there");
				out.body = body;
				ATL::CComBSTR name;
				body->get_Name(&name);
				out.name = ToUtf8(name);
				return grpc::Status::OK;
			}
			if (ATL::CComQIPtr<IComponent2> component(obj.p); component)
			{
				out.component = component;
				out.name = ComponentName(component);
				return grpc::Status::OK;
			}
		}

		size_t index = 0;
		if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, target, index); !status.ok())
			return status;
		const TreeFeature& f = tree[index];
		out.sketch = IsSketchType(f.type);
		if (!out.sketch && !IsReferenceType(f.type))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + f.name + "\" (" + f.type + ") can't be hidden: only bodies, components, sketches and reference "
				"geometry can -- give the id of what it made or what it is (inspect_model what=\"bodies\" or "
				"\"components\")");
		out.feature = f.feature;
		out.name = f.name;
		return grpc::Status::OK;
	}

	// Sketches and reference geometry go by the selection, each with its own
	// call: BlankRefGeom does nothing to a sketch (seen 2026-11-01).
	void ApplyVisibility(IModelDoc2* doc, const Item& item, bool visible)
	{
		if (item.body)
			item.body->HideBody(visible ? VARIANT_FALSE : VARIANT_TRUE);
		else if (item.component)
			item.component->put_Visible(visible ? swComponentVisible : swComponentHidden);
		else
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(item.feature->Select2(VARIANT_FALSE, 0, &selected)) || !selected)
				return;
			if (item.sketch)
				visible ? doc->UnblankSketch() : doc->BlankSketch();
			else
				visible ? doc->UnBlankRefGeom() : doc->BlankRefGeom();
			doc->ClearSelection2(VARIANT_TRUE);
		}
	}
}

grpc::Status SolidWorksService::SetVisibility(grpc::ServerContext* context,
	const sld_mcp::v1::SetVisibilityRequest* request,
	sld_mcp::v1::SetVisibilityResponse* response)
{
	if (request->items().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one thing to hide or show");
	if (request->items_size() > kMaxItems)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxItems) + " per call");

	return m_executor.Execute(context, "SetVisibility", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		PersistIds ids(ext);
		const std::vector<TreeFeature> tree = FeatureTree(doc);
		std::vector<Item> items;
		for (int i = 0; i < request->items_size(); ++i)
		{
			Item item;
			if (grpc::Status status = ResolveItem(ext, docType == swDocASSEMBLY, tree, ids, request->items(i), item);
				!status.ok())
				return grpc::Status(status.error_code(), "item " + std::to_string(i + 1) + ": " + status.error_message());
			if (std::none_of(items.begin(), items.end(), [&](const Item& o) { return SameObject(o.Object(), item.Object()); }))
				items.push_back(std::move(item));
		}

		const bool visible = request->visible();
		bool anyChanged = false, anyRefused = false;
		for (const Item& item : items)
		{
			const bool wasVisible = item.Visible();
			if (wasVisible != visible)
				ApplyVisibility(doc, item, visible);
			const bool isVisible = item.Visible();
			anyChanged = anyChanged || isVisible != wasVisible;
			anyRefused = anyRefused || isVisible != visible;
			sld_mcp::v1::VisibilityChange* change = response->add_items();
			change->set_name(item.name);
			change->set_kind(item.Kind());
			change->set_changed(isVisible != wasVisible);
			change->set_visible(isVisible);
		}
		doc->GraphicsRedraw2();
		if (anyRefused && !anyChanged)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("SolidWorks didn't ") + (visible ? "show" : "hide") + " " + response->items(0).name() +
				(items.size() > 1 ? " and the others" : ""));
		return grpc::Status::OK;
	});
}
