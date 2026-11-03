// FeatureOrder.cpp : Rollback (moving the rollback bar) and ReorderFeature (moving a feature
// in the tree). See docs/notes/editing.md.
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"

using namespace sld_mcp::service;

namespace
{
	bool RolledBack(IFeature* feature)
	{
		VARIANT_BOOL rolledBack = VARIANT_FALSE;
		return SUCCEEDED(feature->IsRolledBack(&rolledBack)) && rolledBack != VARIANT_FALSE;
	}

	size_t IndexOf(const std::vector<TreeFeature>& tree, const std::string& name)
	{
		for (size_t i = 0; i < tree.size(); ++i)
			if (tree[i].name == name)
				return i;
		return tree.size();
	}

	// The tree without what its features absorbed (sketches, the mates in
	// the mates folder): the order the feature tree shows at the top level.
	std::vector<std::string> TopLevelNames(const std::vector<TreeFeature>& tree)
	{
		std::set<std::string> absorbed;
		for (const TreeFeature& f : tree)
			for (const auto& sub : SubFeatures(f.feature))
				absorbed.insert(FeatureName(sub));
		std::vector<std::string> out;
		for (const TreeFeature& f : tree)
			if (!absorbed.count(f.name))
				out.push_back(f.name);
		return out;
	}

	// ResolveFeatureTarget, plus a persist id that doesn't resolve because
	// the feature is rolled back (or suppressed): its id is still its own.
	grpc::Status ResolveTreeTarget(IModelDocExtension* ext, const std::vector<TreeFeature>& tree, PersistIds& ids,
		const sld_mcp::v1::FeatureTarget& target, const char* what, size_t& index)
	{
		grpc::Status status = ResolveFeatureTarget(ext, tree, ids, target, index);
		if (!status.ok() && target.target_case() == sld_mcp::v1::FeatureTarget::kPersistId)
			for (size_t i = 0; i < tree.size(); ++i)
				if (ids.Get(tree[i].feature) == target.persist_id())
				{
					index = i;
					return grpc::Status::OK;
				}
		if (!status.ok())
			return grpc::Status(status.error_code(), std::string(what) + ": " + status.error_message());
		return grpc::Status::OK;
	}

	// The active part or assembly with no sketch open, its extension and tree.
	grpc::Status OpenModel(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc, ATL::CComPtr<IModelDocExtension>& ext)
	{
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return status;
		return GetExtension(doc, ext);
	}
}

grpc::Status SolidWorksService::Rollback(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::RollbackRequest* request,
	sld_mcp::v1::RollbackResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = OpenModel(swApp, doc, ext); !status.ok())
		return status;

	const std::vector<TreeFeature> tree = FeatureTree(doc);
	std::string name;
	if (request->has_feature())
	{
		PersistIds ids(ext);
		size_t index = 0;
		if (grpc::Status status = ResolveTreeTarget(ext, tree, ids, request->feature(), "feature", index); !status.ok())
			return status;
		name = tree[index].name;
	}
	int before = 0;
	for (const TreeFeature& f : tree)
		before += RolledBack(f.feature) ? 1 : 0;
	response->set_rolled_back_before(before);

	ATL::CComPtr<IFeatureManager> featureManager;
	HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr) || !featureManager)
		return ComError("IModelDoc2::FeatureManager", hr);
	const long location = name.empty() ? swMoveRollbackBarToEnd
		: request->after() ? swMoveRollbackBarToAfterFeature : swMoveRollbackBarToBeforeFeature;
	VARIANT_BOOL moved = VARIANT_FALSE;
	hr = featureManager->EditRollback(location, FromUtf8(name), &moved);
	if (FAILED(hr))
		return ComError("IFeatureManager::EditRollback", hr);

	// Read back: the feature itself is below the bar (before) or above it
	// (after), and so is the next top-level one.
	const std::vector<TreeFeature> now = FeatureTree(doc);
	for (const TreeFeature& f : now)
		if (RolledBack(f.feature))
			SetFeatureRef(response->add_rolled_back(), f.name, f.type);
	bool there = true;
	if (name.empty())
		there = response->rolled_back().empty();
	else
	{
		const size_t at = IndexOf(now, name);
		there = at < now.size() && RolledBack(now[at].feature) != request->after();
		if (there && request->after())
		{
			const std::vector<std::string> top = TopLevelNames(now);
			const auto it = std::find(top.begin(), top.end(), name);
			if (it != top.end() && it + 1 != top.end())
				there = RolledBack(now[IndexOf(now, *(it + 1))].feature);
		}
	}
	if (!there)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't move the rollback bar " + (name.empty() ? std::string("to the end") :
				std::string(request->after() ? "after \"" : "before \"") + name + "\"") +
			(moved ? "" : " (it refused)") + "; " + std::to_string(response->rolled_back_size()) +
			" features are rolled back now");
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::ReorderFeature(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::ReorderFeatureRequest* request,
	sld_mcp::v1::ReorderFeatureResponse* response)
{
	if (!request->has_feature() || !request->has_target())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the feature to move and where to");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = OpenModel(swApp, doc, ext); !status.ok())
		return status;

	const std::vector<TreeFeature> tree = FeatureTree(doc);
	PersistIds ids(ext);
	size_t moving = 0, target = 0;
	if (grpc::Status status = ResolveTreeTarget(ext, tree, ids, request->feature(), "feature", moving); !status.ok())
		return status;
	if (grpc::Status status = ResolveTreeTarget(ext, tree, ids, request->target(), "target", target); !status.ok())
		return status;
	const TreeFeature& feature = tree[moving];
	const std::string targetName = tree[target].name;
	if (feature.fixed)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"\"" + feature.name + "\" can't be moved: it's a fixed part of the tree (a folder, the origin, a "
			"default plane, the mates folder or a component)");
	if (moving == target)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the feature and the target are the same");

	const auto errorsBefore = FeatureErrors(tree);
	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	VARIANT_BOOL moved = VARIANT_FALSE;
	HRESULT hr = ext->ReorderFeature(FromUtf8(feature.name), FromUtf8(targetName),
		request->after() ? swMoveAfter : swMoveBefore, &moved);
	if (FAILED(hr))
		return ComError("IModelDocExtension::ReorderFeature", hr);

	const std::vector<TreeFeature> now = FeatureTree(doc);
	const size_t at = IndexOf(now, feature.name), targetAt = IndexOf(now, targetName);
	const bool there = at < now.size() && targetAt < now.size() && (request->after() ? at > targetAt : at < targetAt);
	if (!there)
	{
		std::string parents;
		ATL::CComVariant list;
		if (SUCCEEDED(feature.feature->GetParents(&list)))
			for (const auto& parent : FeatureList(list))
				parents += (parents.empty() ? "" : ", ") + FeatureName(parent);
		std::string children;
		for (const auto& child : ChildFeatures(feature.feature))
			children += (children.empty() ? "" : ", ") + FeatureName(child);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't move \"" + feature.name + "\" " + (request->after() ? "after" : "before") + " \"" +
			targetName + "\": a feature can't go above what it is built on (" +
			(parents.empty() ? std::string("nothing") : parents) + ") or below what is built on it (" +
			(children.empty() ? std::string("nothing") : children) + ")");
	}

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);
	SetFeatureRef(response->mutable_moved(), feature.name, feature.type);
	const std::vector<std::string> top = TopLevelNames(now);
	if (const auto it = std::find(top.begin(), top.end(), feature.name); it != top.end())
	{
		if (it != top.begin())
			response->set_previous(*(it - 1));
		if (it + 1 != top.end())
			response->set_next(*(it + 1));
	}
	AddNewErrors(errorsBefore, FeatureErrors(now), response->mutable_new_errors());
	return grpc::Status::OK;
}
