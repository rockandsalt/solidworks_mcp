// FeatureTreeHelpers.h : The feature tree: walking it, parent/child features, rebuild errors,
// resolving feature targets.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	std::string FeatureTypeName(IFeature* feature);

	// One feature of the tree, as DeleteFeatures sees it.
	struct TreeFeature
	{
		ATL::CComPtr<IFeature> feature;
		std::string name;
		std::string type;
		// Why it can't be deleted (the tree's fixed folders, the default
		// planes, the origin, the mates folder, components), or null.
		const char* fixed = nullptr;
	};

	// COM identity -> index in the tree being collected.
	using SeenFeatures = std::map<IUnknown*, size_t>;

	// Adds `f` unless it's already in `out` (then only its fixed reason is
	// kept). True if added.
	bool AddTreeFeature(TreeFeature f, SeenFeatures& seen, std::vector<TreeFeature>& out);

	void CollectSubFeatures(IFeature* parent, const char* parentFixed, int depth, SeenFeatures& seen,
		std::vector<TreeFeature>& out);

	// Every feature of the active document's own tree, in tree order: the
	// top-level ones and, recursively, their sub-features (absorbed
	// sketches, the mates in the mates folder, ...). Components' trees
	// aren't entered. Each feature once: GetNextFeature also visits absorbed
	// sketches, which come again as their feature's sub-features.
	std::vector<TreeFeature> FeatureTree(IModelDoc2* doc);

	std::vector<ATL::CComPtr<IFeature>> FeatureList(const VARIANT& list);

	// Features that depend on `feature` (IFeature::GetChildren).
	std::vector<ATL::CComPtr<IFeature>> ChildFeatures(IFeature* feature);

	// What `feature` absorbed, e.g. an extrude's profile sketch.
	std::vector<ATL::CComPtr<IFeature>> SubFeatures(IFeature* feature);

	void SetFeatureRef(sld_mcp::v1::FeatureRef* out, const std::string& name, const std::string& type);

	// Rebuild errors (and warnings) by feature name.
	struct FeatureError
	{
		std::string type;
		long code = swFeatureErrorNone;
		bool warning = false;
	};

	std::map<std::string, FeatureError> FeatureErrors(const std::vector<TreeFeature>& tree);

	// The errors in `after` that `before` didn't have (or had differently).
	void AddNewErrors(const std::map<std::string, FeatureError>& before,
		const std::map<std::string, FeatureError>& after,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::FeatureProblem>* out);

	// The tree entry a DeleteFeatures target names. A persist id is matched
	// by object (or persist id), not by name, because a component's feature
	// can share its name with one of the assembly's own.
	grpc::Status ResolveFeatureTarget(IModelDocExtension* ext, const std::vector<TreeFeature>& tree,
		PersistIds& ids, const sld_mcp::v1::FeatureTarget& target, size_t& index);

	bool IsSketchType(const std::string& type);
}
