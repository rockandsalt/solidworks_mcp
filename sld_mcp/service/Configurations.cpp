// Configurations.cpp : Configurations (listing, creating, activating, deleting them) and
// SetSuppression (see docs/notes/configurations.md).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "ConfigurationHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// More than anyone selects by hand.
	constexpr int kMaxItems = 500;

	// Components, as the assembly's tree lists them.
	constexpr const char* kComponentType = "Reference";

	std::vector<ATL::CComPtr<IComponent2>> TopLevelComponents(IAssemblyDoc* assembly)
	{
		std::vector<ATL::CComPtr<IComponent2>> out;
		ATL::CComVariant all;
		if (FAILED(assembly->GetComponents(VARIANT_TRUE, &all)))
			return out;
		for (const auto& disp : comvariant::ToDispatches(all))
			if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
				out.push_back(ATL::CComPtr<IComponent2>(component));
		return out;
	}

	// Per configuration of `names`: suppressed or not. Empty if SolidWorks
	// gives no answer.
	std::vector<bool> FeatureSuppression(IFeature* feature, const std::vector<std::string>& names)
	{
		ATL::CComVariant states;
		if (FAILED(feature->IsSuppressed2(swSpecifyConfiguration, ToBstrArray(names), &states)))
			return {};
		std::vector<bool> out;
		for (long long state : comvariant::ToInt64s(states))
			out.push_back(state != 0);
		if (out.size() != names.size())
			out.clear();
		return out;
	}

	bool ActiveSuppressed(IFeature* feature)
	{
		VARIANT_BOOL suppressed = VARIANT_FALSE;
		return SUCCEEDED(feature->IsSuppressed(&suppressed)) && suppressed != VARIANT_FALSE;
	}

	// Each configuration with what's suppressed in it, and the dimensions
	// whose value differs between them.
	void ListConfigurations(IModelDoc2* doc, const ConfigurationList& configs,
		sld_mcp::v1::ConfigurationsResponse* response)
	{
		response->set_active(configs.active);
		for (size_t i = 0; i < configs.names.size(); ++i)
		{
			sld_mcp::v1::ConfigurationInfo* info = response->add_configurations();
			info->set_name(configs.names[i]);
			info->set_parent(configs.parents[i]);
			ATL::CComPtr<IDispatch> disp;
			ATL::CComBSTR description;
			if (SUCCEEDED(doc->GetConfigurationByName(FromUtf8(configs.names[i]), &disp)) && disp)
				if (ATL::CComQIPtr<IConfiguration> config(disp.p); config)
				{
					if (SUCCEEDED(config->get_Description(&description)))
						info->set_description(ToUtf8(description));
					// Each configuration's own components, without activating it:
					// IFeature::IsSuppressed2 says false for components (seen
					// 2026-11-02).
					ATL::CComPtr<IComponent2> root;
					ATL::CComVariant children;
					if (SUCCEEDED(config->GetRootComponent3(VARIANT_TRUE, &root)) && root &&
						SUCCEEDED(root->GetChildren(&children)))
						for (const auto& child : comvariant::ToDispatches(children))
							if (ATL::CComQIPtr<IComponent2> component(child.p); component && IsSuppressed(component))
								info->add_suppressed_components(ComponentName(component));
				}
		}

		std::set<std::string> seenDimensions;
		for (const TreeFeature& f : FeatureTree(doc))
		{
			if (f.type == kComponentType)
				continue;
			const std::vector<bool> suppressed = FeatureSuppression(f.feature, configs.names);
			for (size_t i = 0; i < suppressed.size(); ++i)
				if (suppressed[i])
					response->mutable_configurations(static_cast<int>(i))->add_suppressed_features(f.name);
			if (configs.names.size() < 2)
				continue;
			for (const auto& display : FeatureDimensions(f.feature))
			{
				ATL::CComPtr<IDimension> dim;
				ATL::CComBSTR name;
				if (FAILED(display->GetDimension2(0, &dim)) || !dim || FAILED(dim->get_FullName(&name)) ||
					!seenDimensions.insert(ToUtf8(name)).second)
					continue;
				const std::vector<double> values = DimensionValues(dim, configs);
				if (values.empty() || std::all_of(values.begin(), values.end(),
					[&](double v) { return std::abs(v - values[0]) <= 1e-12; }))
					continue;
				long type = swDimensionParamTypeUnknown;
				dim->GetType(&type);
				sld_mcp::v1::ConfiguredDimension* out = response->add_dimensions();
				out->set_name(ToUtf8(name));
				out->set_angle(type == swDimensionParamTypeDoubleAngular);
				for (double v : values)
					out->add_values(v);
			}
		}
	}

	grpc::Status Create(IModelDoc2* doc, const ConfigurationList& configs, const sld_mcp::v1::CreateConfiguration& create)
	{
		if (create.name().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the new configuration a name");
		if (!configs.Find(create.name()).empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the document already has a configuration \"" + configs.Find(create.name()) + "\"");
		ATL::CComPtr<IConfiguration> made;
		if (!create.parent().empty())
		{
			const std::string parent = configs.Find(create.parent());
			if (parent.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the document has no configuration \"" + create.parent() + "\" to derive from -- it has " +
					configs.Quoted());
			ATL::CComPtr<IConfigurationManager> manager;
			HRESULT hr = doc->get_ConfigurationManager(&manager);
			if (FAILED(hr) || !manager)
				return ComError("IModelDoc2::ConfigurationManager", hr);
			hr = manager->AddConfiguration2(FromUtf8(create.name()), ATL::CComBSTR(L""), ATL::CComBSTR(L""), 0,
				FromUtf8(parent), FromUtf8(create.description()), VARIANT_TRUE, &made);
			if (FAILED(hr))
				return ComError("IConfigurationManager::AddConfiguration2", hr);
		}
		else
		{
			ATL::CComPtr<IDispatch> disp;
			HRESULT hr = doc->AddConfiguration3(FromUtf8(create.name()), ATL::CComBSTR(L""), ATL::CComBSTR(L""), 0, &disp);
			if (FAILED(hr))
				return ComError("IModelDoc2::AddConfiguration3", hr);
			made = ATL::CComQIPtr<IConfiguration>(disp.p);
			if (made && !create.description().empty())
				made->put_Description(FromUtf8(create.description()));
		}
		if (!made)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't create the configuration \"" + create.name() + "\"");
		return grpc::Status::OK;
	}

	grpc::Status Activate(IModelDoc2* doc, const ConfigurationList& configs, const std::string& wanted)
	{
		const std::string name = configs.Find(wanted);
		if (name.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the document has no configuration \"" + wanted + "\" -- it has " + configs.Quoted());
		if (name == configs.active)
			return grpc::Status::OK;
		VARIANT_BOOL shown = VARIANT_FALSE;
		HRESULT hr = doc->ShowConfiguration2(FromUtf8(name), &shown);
		if (FAILED(hr))
			return ComError("IModelDoc2::ShowConfiguration2", hr);
		if (!shown)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't activate the configuration \"" + name + "\"");
		return grpc::Status::OK;
	}

	grpc::Status Delete(IModelDoc2* doc, const ConfigurationList& configs, const std::string& wanted,
		sld_mcp::v1::ConfigurationsResponse* response)
	{
		const std::string name = configs.Find(wanted);
		if (name.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the document has no configuration \"" + wanted + "\" -- it has " + configs.Quoted());
		// SolidWorks refuses (seen 2026-11-02).
		if (name == configs.active)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" is the active configuration -- activate another one first" +
				(configs.names.size() < 2 ? std::string(" (create one: a document keeps at least one)") : ""));
		VARIANT_BOOL deleted = VARIANT_FALSE;
		HRESULT hr = doc->DeleteConfiguration2(FromUtf8(name), &deleted);
		if (FAILED(hr))
			return ComError("IModelDoc2::DeleteConfiguration2", hr);
		if (!deleted)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't delete the configuration \"" + name + "\"");
		// Its derived configurations go too (seen 2026-11-02).
		const ConfigurationList after = ReadConfigurations(doc);
		for (const std::string& n : configs.names)
			if (std::find(after.names.begin(), after.names.end(), n) == after.names.end())
				response->add_deleted(n);
		return grpc::Status::OK;
	}

	// One thing to suppress: a feature, or a top-level component (by name:
	// its IComponent2 doesn't survive activating another configuration).
	struct Item
	{
		ATL::CComPtr<IFeature> feature;
		std::string component;
		std::string name;
	};

	grpc::Status ItemOfTree(const TreeFeature& f, IAssemblyDoc* assembly, Item& out)
	{
		if (f.type == kComponentType && assembly)
		{
			out.component = out.name = f.name;
			return grpc::Status::OK;
		}
		if (f.fixed)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + f.name + "\" can't be suppressed: it " + f.fixed);
		out.feature = f.feature;
		out.name = f.name;
		return grpc::Status::OK;
	}

	grpc::Status TopLevelComponentItem(IComponent2* component, Item& out)
	{
		if (ParentOf(component))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + ComponentName(component) + "\" is inside a subassembly -- suppress the whole subassembly, or "
				"open its file (open_document) and suppress it there");
		out.component = out.name = ComponentName(component);
		return grpc::Status::OK;
	}

	grpc::Status ResolveItem(IModelDocExtension* ext, IAssemblyDoc* assembly, const std::vector<TreeFeature>& tree,
		PersistIds& ids, const sld_mcp::v1::FeatureTarget& target, Item& out)
	{
		if (target.target_case() == sld_mcp::v1::FeatureTarget::kName)
		{
			for (const TreeFeature& f : tree)
				if (f.name == target.name())
					return ItemOfTree(f, assembly, out);
			if (assembly)
			{
				ATL::CComPtr<IComponent2> component;
				if (SUCCEEDED(assembly->GetComponentByName(FromUtf8(target.name()), &component)) && component)
					return TopLevelComponentItem(component, out);
			}
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active document has no feature or component named \"" + target.name() +
				"\" -- inspect the model for its features, mates and components");
		}
		if (target.target_case() != sld_mcp::v1::FeatureTarget::kPersistId)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "an item is empty (give its id or name)");

		ATL::CComPtr<IDispatch> obj;
		grpc::Status resolved = ResolvePersistId(ext, target.persist_id(), obj);
		if (resolved.ok())
		{
			if (ATL::CComQIPtr<IComponent2> component(obj.p); component)
				return TopLevelComponentItem(component, out);
			if (!ATL::CComQIPtr<IFeature>(obj.p))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"that id is a face, edge or body, not a feature -- give a feature, mate or component id");
			size_t index = 0;
			if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, target, index); !status.ok())
				return status;
			return ItemOfTree(tree[index], assembly, out);
		}
		// A suppressed feature or component doesn't resolve
		// (swPersistReferencedObject_Suppressed); its persist id is still its own.
		for (const TreeFeature& f : tree)
			if (ids.Get(f.feature) == target.persist_id())
				return ItemOfTree(f, assembly, out);
		if (assembly)
			for (const auto& component : TopLevelComponents(assembly))
				if (ids.Get(component) == target.persist_id())
					return TopLevelComponentItem(component, out);
		return resolved;
	}
}

grpc::Status SolidWorksService::Configurations(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::ConfigurationsRequest* request,
	sld_mcp::v1::ConfigurationsResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	long docType = swDocNONE;
	if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
		return status;

	const ConfigurationList before = ReadConfigurations(doc);
	using Action = sld_mcp::v1::ConfigurationsRequest;
	if (request->action_case() != Action::ACTION_NOT_SET)
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return status;
	grpc::Status status = grpc::Status::OK;
	switch (request->action_case())
	{
	case Action::kCreate:
		status = Create(doc, before, request->create());
		break;
	case Action::kActivate:
		status = Activate(doc, before, request->activate());
		break;
	case Action::kDelete:
		status = Delete(doc, before, request->delete_(), response);
		break;
	default:
		break;
	}
	if (!status.ok())
		return status;

	ListConfigurations(doc, ReadConfigurations(doc), response);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::SetSuppression(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetSuppressionRequest* request,
	sld_mcp::v1::SetSuppressionResponse* response)
{
	if (request->items().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one thing to suppress or unsuppress");
	if (request->items_size() > kMaxItems)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxItems) + " per call");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	long docType = swDocNONE;
	if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
		return status;
	if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IAssemblyDoc> assembly;
	if (docType == swDocASSEMBLY)
		assembly = doc;

	const ConfigurationList configs = ReadConfigurations(doc);
	std::vector<std::string> targets;
	const auto scope = request->scope() == sld_mcp::v1::CONFIGURATION_SCOPE_UNSPECIFIED
		? sld_mcp::v1::CONFIGURATION_SCOPE_ACTIVE : request->scope();
	if (grpc::Status status = ScopeConfigurations(configs, scope, request->configurations(), targets); !status.ok())
		return status;

	PersistIds ids(ext);
	const std::vector<TreeFeature> tree = FeatureTree(doc);
	std::vector<Item> items;
	for (int i = 0; i < request->items_size(); ++i)
	{
		Item item;
		if (grpc::Status status = ResolveItem(ext, assembly, tree, ids, request->items(i), item); !status.ok())
			return grpc::Status(status.error_code(), "item " + std::to_string(i + 1) + ": " + status.error_message());
		if (std::none_of(items.begin(), items.end(), [&](const Item& o) { return o.name == item.name; }))
			items.push_back(std::move(item));
	}

	const bool suppress = request->suppressed();
	std::map<IFeature*, bool> activeBefore;
	for (const TreeFeature& f : tree)
		activeBefore[f.feature] = ActiveSuppressed(f.feature);
	const auto errorsBefore = FeatureErrors(tree);

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	// Features: all configurations at once, without activating them.
	std::map<std::string, sld_mcp::v1::SuppressionChange> changes;
	for (const Item& item : items)
	{
		if (!item.feature)
			continue;
		const std::vector<bool> was = FeatureSuppression(item.feature, targets);
		VARIANT_BOOL ok = VARIANT_FALSE;
		item.feature->SetSuppression2(suppress ? swSuppressFeature : swUnSuppressFeature, swSpecifyConfiguration,
			ToBstrArray(targets), &ok);
		const std::vector<bool> now = FeatureSuppression(item.feature, targets);
		sld_mcp::v1::SuppressionChange& change = changes[item.name];
		change.set_name(item.name);
		change.set_kind("feature");
		for (size_t i = 0; i < targets.size(); ++i)
		{
			sld_mcp::v1::SuppressionState* state = change.add_states();
			state->set_configuration(targets[i]);
			const bool isSuppressed = i < now.size() && now[i];
			state->set_suppressed(isSuppressed);
			state->set_changed(i < was.size() && i < now.size() && was[i] != isSuppressed);
		}
	}

	// Components: IComponent2::SetSuppression2 acts on the active
	// configuration only, and a component reached through another
	// configuration's root refuses it (seen 2026-11-02), so each
	// configuration in turn is activated.
	const bool anyComponent = std::any_of(items.begin(), items.end(), [](const Item& i) { return !i.component.empty(); });
	std::string shownNow = configs.active;
	grpc::Status failure = grpc::Status::OK;
	for (const std::string& configuration : anyComponent ? targets : std::vector<std::string>())
	{
		if (configuration != shownNow)
		{
			VARIANT_BOOL shown = VARIANT_FALSE;
			if (FAILED(doc->ShowConfiguration2(FromUtf8(configuration), &shown)) || !shown)
			{
				failure = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't activate the configuration \"" + configuration + "\" to change its components");
				break;
			}
			shownNow = configuration;
		}
		for (const Item& item : items)
		{
			if (item.component.empty())
				continue;
			ATL::CComPtr<IComponent2> component;
			assembly->GetComponentByName(FromUtf8(item.component), &component);
			const bool was = component && IsSuppressed(component);
			if (component && was != suppress)
			{
				long result = 0;
				component->SetSuppression2(suppress ? swComponentSuppressed : swComponentFullyResolved, &result);
			}
			const bool now = component && IsSuppressed(component);
			sld_mcp::v1::SuppressionChange& change = changes[item.name];
			change.set_name(item.name);
			change.set_kind("component");
			sld_mcp::v1::SuppressionState* state = change.add_states();
			state->set_configuration(configuration);
			state->set_suppressed(now);
			state->set_changed(component && was != now);
		}
	}
	if (shownNow != configs.active)
	{
		VARIANT_BOOL shown = VARIANT_FALSE;
		doc->ShowConfiguration2(FromUtf8(configs.active), &shown);
	}

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);

	bool anyChanged = false, anyRefused = false;
	for (const Item& item : items)
	{
		const sld_mcp::v1::SuppressionChange& change = changes[item.name];
		for (const auto& state : change.states())
		{
			anyChanged = anyChanged || state.changed();
			anyRefused = anyRefused || state.suppressed() != suppress;
		}
		*response->add_items() = change;
	}
	for (const TreeFeature& f : tree)
		if (f.type != kComponentType && ActiveSuppressed(f.feature) != activeBefore[f.feature] &&
			std::none_of(items.begin(), items.end(), [&](const Item& i) { return i.name == f.name; }))
			response->add_also_changed(f.name);
	response->set_active(configs.active);
	AddNewErrors(errorsBefore, FeatureErrors(tree), response->mutable_new_errors());

	if (!failure.ok())
		return failure;
	if (anyRefused && !anyChanged)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			std::string("SolidWorks didn't ") + (suppress ? "suppress " : "unsuppress ") + items[0].name +
			(items.size() > 1 ? " or the others" : "") + " in " + (targets.size() > 1 ? "those configurations" : "\"" + targets[0] + "\""));
	return grpc::Status::OK;
}
