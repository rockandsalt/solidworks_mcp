// SeedHelpers.cpp : The features and bodies that mirrors and patterns copy.
#include "pch.h"
#include "SeedHelpers.h"
#include "AssemblyHelpers.h"
#include "FeatureTreeHelpers.h"

namespace sld_mcp::service
{
	grpc::Status ResolveSeedFeatures(IModelDoc2* doc, IModelDocExtension* ext,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::FeatureTarget>& targets, const std::string& verb,
		const std::string& sketchHint, std::vector<ATL::CComPtr<IFeature>>& features, std::string& names)
	{
		if (targets.empty())
			return grpc::Status::OK;
		PersistIds ids(ext);
		const std::vector<TreeFeature> tree = FeatureTree(doc);
		std::set<std::string> seen;
		for (int i = 0; i < targets.size(); ++i)
		{
			const std::string label = "feature " + std::to_string(i + 1);
			size_t index = 0;
			if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, targets[i], index); !status.ok())
				return grpc::Status(status.error_code(), label + ": " + status.error_message());
			const TreeFeature& f = tree[index];
			if (f.fixed)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					label + ": \"" + f.name + "\" can't be " + verb + ": it " + f.fixed);
			if (IsSketchType(f.type))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					label + ": \"" + f.name + "\" is a sketch -- " + sketchHint);
			if (!seen.insert(f.name).second)
				continue;
			features.push_back(f.feature);
			names += (names.empty() ? "" : ", ") + f.name;
		}
		return grpc::Status::OK;
	}

	grpc::Status ResolveSeedBodies(IModelDocExtension* ext, const google::protobuf::RepeatedPtrField<std::string>& ids,
		std::vector<ATL::CComPtr<IBody2>>& bodies, std::string& names)
	{
		for (int i = 0; i < ids.size(); ++i)
		{
			const std::string label = "body " + std::to_string(i + 1);
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, ids[i], obj); !status.ok())
				return grpc::Status(status.error_code(), label + " " + status.error_message());
			ATL::CComQIPtr<IBody2> body(obj.p);
			if (!body)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, label + " isn't a body");
			if (std::none_of(bodies.begin(), bodies.end(), [&](const auto& b) { return SameObject(b, body); }))
				bodies.push_back(body);
		}
		if (!bodies.empty())
			names = bodies.size() == 1 ? "1 body" : std::to_string(bodies.size()) + " bodies";
		return grpc::Status::OK;
	}

	grpc::Status SelectSeedFeatures(IModelDoc2* doc, const std::vector<ATL::CComPtr<IFeature>>& features, long mark)
	{
		for (const auto& feature : features)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(feature->Select2(VARIANT_TRUE, mark, &selected)) || !selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select \"" + FeatureName(feature) + "\" -- is it suppressed?");
			}
		}
		return grpc::Status::OK;
	}

	grpc::Status SelectSeedBodies(IModelDoc2* doc, const std::vector<ATL::CComPtr<IBody2>>& bodies, long mark)
	{
		if (bodies.empty())
			return grpc::Status::OK;
		ATL::CComPtr<ISelectionMgr> selectionManager;
		ATL::CComPtr<ISelectData> data;
		HRESULT hr = doc->get_ISelectionManager(&selectionManager);
		if (FAILED(hr) || !selectionManager || FAILED(hr = selectionManager->CreateSelectData(&data)) || !data)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return ComError("ISelectionMgr::CreateSelectData", hr);
		}
		data->put_Mark(mark);
		for (size_t i = 0; i < bodies.size(); ++i)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(bodies[i]->Select2(VARIANT_TRUE, data, &selected)) || !selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select body " + std::to_string(i + 1) + " -- is it hidden?");
			}
		}
		return grpc::Status::OK;
	}

	grpc::Status KeepIfBuilt(IModelDoc2* doc, IFeature* feature, const std::string& why)
	{
		VARIANT_BOOL warning = VARIANT_FALSE;
		long error = swFeatureErrorNone;
		long faces = 0;
		const bool failed =
			SUCCEEDED(feature->GetErrorCode2(&warning, &error)) && error != swFeatureErrorNone && !warning;
		if (!failed && !(SUCCEEDED(feature->GetFaceCount(&faces)) && faces == 0))
			return grpc::Status::OK;
		const std::string name = FeatureName(feature);
		DeleteFeature(doc, feature);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks made " + name + " but it " +
			(failed ? "failed to rebuild (swFeatureError_e " + std::to_string(error) + ")" : "changed nothing") +
			", so it was deleted again -- " + why);
	}

	int SolidBodyCount(IModelDoc2* doc)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		ATL::CComVariant bodies;
		if (!part || FAILED(part->GetBodies2(swSolidBody, VARIANT_FALSE, &bodies)))
			return 0;
		return static_cast<int>(comvariant::ToDispatches(bodies).size());
	}
}
