// FeatureTreeHelpers.cpp : see FeatureTreeHelpers.h.
#include "pch.h"
#include "FeatureTreeHelpers.h"
#include "Common.h"

namespace sld_mcp::service
{
	std::string FeatureTypeName(IFeature* feature)
	{
		ATL::CComBSTR typeName;
		if (feature)
			feature->GetTypeName2(&typeName);
		return ToUtf8(typeName);
	}

	bool AddTreeFeature(TreeFeature f, SeenFeatures& seen, std::vector<TreeFeature>& out)
	{
		ATL::CComPtr<IUnknown> identity;
		if (SUCCEEDED(f.feature->QueryInterface(IID_PPV_ARGS(&identity))))
		{
			// `out` holds a reference, so the pointer stays valid as a key.
			const auto [it, added] = seen.emplace(identity.p, out.size());
			if (!added)
			{
				if (!out[it->second].fixed)
					out[it->second].fixed = f.fixed;
				return false;
			}
		}
		out.push_back(std::move(f));
		return true;
	}

	void CollectSubFeatures(IFeature* parent, const char* parentFixed, int depth, SeenFeatures& seen,
		std::vector<TreeFeature>& out)
	{
		ATL::CComPtr<IFeature> sub;
		if (FAILED(parent->IGetFirstSubFeature(&sub)))
			return;
		while (sub)
		{
			if (AddTreeFeature({sub, FeatureName(sub), FeatureTypeName(sub), parentFixed}, seen, out) && depth < 3)
				CollectSubFeatures(sub, parentFixed, depth + 1, seen, out);
			ATL::CComPtr<IFeature> next;
			if (FAILED(sub->IGetNextSubFeature(&next)))
				break;
			sub = next;
		}
	}

	std::vector<TreeFeature> FeatureTree(IModelDoc2* doc)
	{
		std::vector<TreeFeature> out;
		SeenFeatures seen;
		ATL::CComPtr<IFeature> feature;
		if (FAILED(doc->IFirstFeature(&feature)))
			return out;
		bool started = false;
		int planes = 0;
		while (feature)
		{
			TreeFeature f{feature, FeatureName(feature), FeatureTypeName(feature)};
			if (f.type == "RefPlane")
			{
				started = true;
				if (planes++ < 3)
					f.fixed = "is one of the default planes (Front/Top/Right), which every sketch tool relies on";
			}
			else if (!started)
				f.fixed = "is one of the feature tree's fixed folders";
			else if (f.type == "OriginProfileFeature")
				f.fixed = "is the origin";
			else if (f.type == "MateGroup")
				f.fixed = "is the mates folder -- delete the mates in it instead";
			else if (f.type == "Reference")
				f.fixed = "is a component -- deleting components isn't supported yet";
			// The mates folder's mates are deletable; anything else inside a
			// fixed item isn't.
			const char* subFixed = f.type == "MateGroup" ? nullptr : f.fixed;
			const bool component = f.type == "Reference";
			if (AddTreeFeature(f, seen, out) && !component)
				CollectSubFeatures(feature, subFixed, 0, seen, out);

			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				break;
			feature = next;
		}
		return out;
	}

	std::vector<ATL::CComPtr<IFeature>> FeatureList(const VARIANT& list)
	{
		std::vector<ATL::CComPtr<IFeature>> out;
		for (const auto& disp : comvariant::ToDispatches(list))
			if (ATL::CComQIPtr<IFeature> feature(disp.p); feature)
				out.push_back(ATL::CComPtr<IFeature>(feature));
		return out;
	}

	std::vector<ATL::CComPtr<IFeature>> ChildFeatures(IFeature* feature)
	{
		ATL::CComVariant children;
		if (FAILED(feature->GetChildren(&children)))
			return {};
		return FeatureList(children);
	}

	std::vector<ATL::CComPtr<IFeature>> SubFeatures(IFeature* feature)
	{
		std::vector<ATL::CComPtr<IFeature>> out;
		ATL::CComPtr<IFeature> sub;
		if (FAILED(feature->IGetFirstSubFeature(&sub)))
			return out;
		while (sub)
		{
			out.push_back(sub);
			ATL::CComPtr<IFeature> next;
			if (FAILED(sub->IGetNextSubFeature(&next)))
				break;
			sub = next;
		}
		return out;
	}

	void SetFeatureRef(sld_mcp::v1::FeatureRef* out, const std::string& name, const std::string& type)
	{
		out->set_name(name);
		out->set_type_name(type);
	}

	std::map<std::string, FeatureError> FeatureErrors(const std::vector<TreeFeature>& tree)
	{
		std::map<std::string, FeatureError> out;
		for (const auto& f : tree)
		{
			VARIANT_BOOL warning = VARIANT_FALSE;
			long code = swFeatureErrorNone;
			if (SUCCEEDED(f.feature->GetErrorCode2(&warning, &code)) && code != swFeatureErrorNone)
				out.emplace(f.name, FeatureError{f.type, code, warning != VARIANT_FALSE});
		}
		return out;
	}

	void AddNewErrors(const std::map<std::string, FeatureError>& before,
		const std::map<std::string, FeatureError>& after,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::FeatureProblem>* out)
	{
		for (const auto& [name, error] : after)
		{
			auto it = before.find(name);
			if (it != before.end() && it->second.code == error.code && it->second.warning == error.warning)
				continue;
			sld_mcp::v1::FeatureProblem* problem = out->Add();
			problem->set_name(name);
			problem->set_type_name(error.type);
			problem->set_error_code(static_cast<int32_t>(error.code));
			problem->set_warning(error.warning);
		}
	}

	grpc::Status ResolveFeatureTarget(IModelDocExtension* ext, const std::vector<TreeFeature>& tree,
		PersistIds& ids, const sld_mcp::v1::FeatureTarget& target, size_t& index)
	{
		using Target = sld_mcp::v1::FeatureTarget;
		switch (target.target_case())
		{
		case Target::kName:
		{
			for (size_t i = 0; i < tree.size(); ++i)
				if (tree[i].name == target.name())
				{
					index = i;
					return grpc::Status::OK;
				}
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active document has no feature named \"" + target.name() +
				"\" -- inspect the model for its features (and mates)");
		}
		case Target::kPersistId:
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, target.persist_id(), obj); !status.ok())
				return status;
			ATL::CComQIPtr<IFeature> feature(obj.p);
			if (!feature)
			{
				if (ATL::CComQIPtr<IComponent2>(obj.p))
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						"that id is a component, not a feature -- deleting components isn't supported yet");
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"that id is a face, edge or body, not a feature -- give a feature or mate id");
			}
			const std::string name = FeatureName(feature);
			std::vector<size_t> sameName;
			for (size_t i = 0; i < tree.size(); ++i)
				if (tree[i].name == name)
				{
					if (SameObject(tree[i].feature, feature))
					{
						index = i;
						return grpc::Status::OK;
					}
					sameName.push_back(i);
				}
			const std::string pid = ids.Get(feature);
			for (size_t i : sameName)
				if (!pid.empty() && ids.Get(tree[i].feature) == pid)
				{
					index = i;
					return grpc::Status::OK;
				}
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" belongs to a component, not to the active assembly itself -- open that "
				"part (or subassembly) to delete or edit its features");
		}
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a feature is empty (give its id or name)");
		}
	}

	bool IsSketchType(const std::string& type)
	{
		return type == "ProfileFeature" || type == "3DProfileFeature";
	}
}
