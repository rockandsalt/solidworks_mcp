// SeedHelpers.h : The features and bodies that mirrors and patterns copy (their seeds):
// resolving, selecting, and checking the copy that SolidWorks made.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	// More than anyone copies in one go.
	constexpr int kMaxSeeds = 500;

	// The features a request names, in request order, once each; `names` =
	// "Boss-Extrude1, Cut-Extrude1". Refuses the tree's fixed items and
	// sketches (`sketchHint` says what to do with a sketch instead). Errors
	// name the feature by its position ("feature 2: ...").
	grpc::Status ResolveSeedFeatures(IModelDoc2* doc, IModelDocExtension* ext,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::FeatureTarget>& targets, const std::string& verb,
		const std::string& sketchHint, std::vector<ATL::CComPtr<IFeature>>& features, std::string& names);

	// The bodies a request names by persist id, once each; `names` = "1 body"
	// / "2 bodies".
	grpc::Status ResolveSeedBodies(IModelDocExtension* ext, const google::protobuf::RepeatedPtrField<std::string>& ids,
		std::vector<ATL::CComPtr<IBody2>>& bodies, std::string& names);

	// Adds the features / bodies to the selection with `mark`. On failure
	// the selection is cleared.
	grpc::Status SelectSeedFeatures(IModelDoc2* doc, const std::vector<ATL::CComPtr<IFeature>>& features, long mark);
	grpc::Status SelectSeedBodies(IModelDoc2* doc, const std::vector<ATL::CComPtr<IBody2>>& bodies, long mark);

	// A new mirror or pattern that failed to rebuild, or changed nothing (no
	// faces: a copied cut beyond the body builds without an error), is
	// deleted again: FAILED_PRECONDITION saying so, `why` appended.
	grpc::Status KeepIfBuilt(IModelDoc2* doc, IFeature* feature, const std::string& why);

	// Solid bodies in the part.
	int SolidBodyCount(IModelDoc2* doc);

	// A volume change smaller than this (m^3, 1 mm^3) is no change.
	constexpr double kVolumeTolerance = 1e-9;

	// The volume of every solid body of the part, m^3.
	double SolidVolume(IModelDoc2* doc);
}
