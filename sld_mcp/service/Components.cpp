// Components.cpp : InsertComponents, ReplaceComponents, FixComponents,
// MoveComponent, InspectMobility (see docs/notes/architecture.md,
// "Assemblies + mates").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include <chrono>

using namespace sld_mcp::service;

namespace
{
	bool FinitePlacement(const sld_mcp::v1::ComponentPlacement& p)
	{
		return IsFinite(p.origin()) && IsFinite(p.x_axis()) && IsFinite(p.y_axis()) && IsFinite(p.z_axis());
	}

	// One AddComponents3 transform: the images of the component's X, Y and Z
	// axes, the origin, scale 1, then three unused values (verified
	// 2026-10-21: rows, not columns).
	void AppendTransform(const sld_mcp::v1::ComponentPlacement& p, std::vector<double>& out)
	{
		for (const auto* v : { &p.x_axis(), &p.y_axis(), &p.z_axis(), &p.origin() })
			out.insert(out.end(), { v->x(), v->y(), v->z() });
		out.insert(out.end(), { 1.0, 0.0, 0.0, 0.0 });
	}

	bool SameFile(const std::wstring& a, const std::wstring& b)
	{
		std::error_code ec;
		return !a.empty() && !b.empty() && std::filesystem::equivalent(a, b, ec);
	}

	std::wstring ComponentPath(IComponent2* component)
	{
		ATL::CComBSTR path;
		if (component)
			component->GetPathName(&path);
		return path.Length() > 0 ? std::wstring(path.m_str) : std::wstring();
	}

	// INVALID_ARGUMENT unless `path` has the configuration (none given: OK).
	// GetConfigurationNames works on files that aren't open.
	grpc::Status CheckConfiguration(ISldWorks* swApp, const ATL::CComBSTR& path, const ATL::CComBSTR& configuration)
	{
		if (configuration.Length() == 0)
			return grpc::Status::OK;
		ATL::CComVariant namesVariant;
		HRESULT hr = swApp->GetConfigurationNames(path, &namesVariant);
		if (FAILED(hr))
			return ComError("ISldWorks::GetConfigurationNames", hr);
		const std::vector<ATL::CComBSTR> names = comvariant::ToBstrs(namesVariant);
		if (std::any_of(names.begin(), names.end(), [&](const ATL::CComBSTR& n) { return n == configuration; }))
			return grpc::Status::OK;
		std::string list;
		for (const auto& n : names)
			list += (list.empty() ? "" : ", ") + ("\"" + ToUtf8(n) + "\"");
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			ToUtf8(path) + " has no configuration \"" + ToUtf8(configuration) + "\"" +
			(list.empty() ? std::string() : " -- it has " + list));
	}

	Eigen::Vector3d ToVector(const sld_mcp::v1::Vec3& v)
	{
		return Eigen::Vector3d(v.x(), v.y(), v.z());
	}

	// A placement as a transform SetTransformAndSolve2 / put_Transform2
	// take, in AddComponents3's layout (Transform2 reads back the same, seen
	// 2026-10-25).
	ATL::CComPtr<IMathTransform> ToMathTransform(IMathUtility* math, const sld_mcp::v1::ComponentPlacement& p)
	{
		std::vector<double> data;
		AppendTransform(p, data);
		ATL::CComPtr<IDispatch> disp;
		if (FAILED(math->CreateTransform(comvariant::FromDoubles(data.data(), data.size()), &disp)) || !disp)
			return nullptr;
		ATL::CComQIPtr<IMathTransform> xform(disp.p);
		return ATL::CComPtr<IMathTransform>(xform);
	}

	void SetPlacement(const Eigen::Affine3d& placement, sld_mcp::v1::ComponentPlacement* out)
	{
		SetVec3(out->mutable_origin(), Eigen::Vector3d(placement.translation()));
		SetVec3(out->mutable_x_axis(), Eigen::Vector3d(placement.linear().col(0)));
		SetVec3(out->mutable_y_axis(), Eigen::Vector3d(placement.linear().col(1)));
		SetVec3(out->mutable_z_axis(), Eigen::Vector3d(placement.linear().col(2)));
	}

	// Deletes components again (all or nothing). Best effort.
	void DeleteComponents(IModelDoc2* doc, const std::vector<ATL::CComPtr<IComponent2>>& components)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
		for (const auto& component : components)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (component && SUCCEEDED(component->Select4(VARIANT_TRUE, nullptr, VARIANT_FALSE, &selected)) && selected)
				any = true;
		}
		if (any)
			doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
	}

	// The center of the component's assembly-space box; `fallback` if
	// SolidWorks has none.
	Eigen::Vector3d BoxCenter(IComponent2* component, const Eigen::Vector3d& fallback)
	{
		ATL::CComVariant boxVar;
		if (SUCCEEDED(component->GetBox(VARIANT_FALSE, VARIANT_FALSE, &boxVar)))
			if (const Eigen::AlignedBox3d box = ToAlignedBox(comvariant::ToDoubles(boxVar)); !box.isEmpty())
				return box.center();
		return fallback;
	}

	sld_mcp::v1::ConstrainedStatus ToConstrainedStatus(long status)
	{
		switch (status)
		{
		case swUnderConstrained: return sld_mcp::v1::CONSTRAINED_STATUS_UNDER;
		case swFullyConstrained: return sld_mcp::v1::CONSTRAINED_STATUS_FULLY;
		case swOverConstrained: return sld_mcp::v1::CONSTRAINED_STATUS_OVER;
		case swNoSolution: return sld_mcp::v1::CONSTRAINED_STATUS_NO_SOLUTION;
		case swInvalidSolution: return sld_mcp::v1::CONSTRAINED_STATUS_INVALID_SOLUTION;
		case swAutosolveOff: return sld_mcp::v1::CONSTRAINED_STATUS_AUTOSOLVE_OFF;
		default: return sld_mcp::v1::CONSTRAINED_STATUS_UNKNOWN;
		}
	}

}

grpc::Status SolidWorksService::InsertComponents(grpc::ServerContext* context,
	const sld_mcp::v1::InsertComponentsRequest* request,
	sld_mcp::v1::InsertComponentsResponse* response)
{
	// Validate before touching SolidWorks.
	if (request->source_case() == sld_mcp::v1::InsertComponentsRequest::SOURCE_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a file to insert or a component to copy");
	if (request->placements_size() == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one placement");
	for (const auto& placement : request->placements())
		if (!FinitePlacement(placement))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a placement isn't a finite position and axes");

	return m_executor.Execute(context, "InsertComponents", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
			return status;
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		PersistIds ids(ext);

		// The file, and the configuration to use.
		ATL::CComBSTR path;
		ATL::CComBSTR configuration = FromUtf8(request->configuration());
		if (request->source_case() == sld_mcp::v1::InsertComponentsRequest::kCopyOfPersistId)
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, request->copy_of_persist_id(), obj); !status.ok())
				return status;
			ATL::CComQIPtr<IComponent2> original(obj.p);
			if (!original)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "copy_of isn't a component");
			HRESULT hr = original->GetPathName(&path);
			if (FAILED(hr))
				return ComError("IComponent2::GetPathName", hr);
			if (configuration.Length() == 0)
			{
				hr = original->get_ReferencedConfiguration(&configuration);
				if (FAILED(hr))
					return ComError("IComponent2::ReferencedConfiguration", hr);
			}
		}
		else
			path = FromUtf8(request->path());
		if (path.Length() == 0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the component has no file");

		ATL::CComBSTR assemblyPath;
		HRESULT hr = doc->GetPathName(&assemblyPath);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetPathName", hr);
		if (SameFile(path.m_str, assemblyPath.m_str))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that's the assembly itself -- an assembly can't contain itself");

		// AddComponents3 has no configuration argument, and SolidWorks silently
		// ignores a ReferencedConfiguration it doesn't know, so check first.
		if (grpc::Status status = CheckConfiguration(swApp, path, configuration); !status.ok())
			return status;

		long before = 0;
		hr = assembly->GetComponentCount(VARIANT_TRUE, &before);
		if (FAILED(hr))
			return ComError("IAssemblyDoc::GetComponentCount", hr);

		const int count = request->placements_size();
		std::vector<ATL::CComBSTR> names(count, path);
		std::vector<ATL::CComBSTR> coordinateSystems(count, ATL::CComBSTR(L""));
		std::vector<double> transforms;
		for (const auto& placement : request->placements())
			AppendTransform(placement, transforms);
		ATL::CComVariant created;
		hr = assembly->AddComponents3(comvariant::FromBstrs(names),
			comvariant::FromDoubles(transforms.data(), transforms.size()), comvariant::FromBstrs(coordinateSystems),
			&created);
		if (FAILED(hr))
			return ComError("IAssemblyDoc::AddComponents3", hr);

		std::vector<ATL::CComPtr<IComponent2>> components;
		for (const auto& disp : comvariant::ToDispatches(created))
			if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
				components.push_back(ATL::CComPtr<IComponent2>(component));
		if (components.size() != static_cast<size_t>(count))
		{
			DeleteComponents(doc, components);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't insert " + ToUtf8(path) +
				" -- is it a part or assembly file of this SolidWorks version, and not an assembly that "
				"contains this one?");
		}

		if (configuration.Length() > 0)
		{
			for (const auto& component : components)
			{
				ATL::CComBSTR current;
				component->get_ReferencedConfiguration(&current);
				if (current != configuration)
					component->put_ReferencedConfiguration(configuration);
			}
		}

		const bool fixAll = request->has_fixed() && request->fixed();
		const bool fixFirst = !request->has_fixed() && before == 0;
		if (fixAll || fixFirst)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			for (size_t i = 0; i < (fixAll ? components.size() : 1); ++i)
			{
				VARIANT_BOOL selected = VARIANT_FALSE;
				components[i]->Select4(VARIANT_TRUE, nullptr, VARIANT_FALSE, &selected);
			}
			assembly->FixComponent();
			doc->ClearSelection2(VARIANT_TRUE);
		}

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);

		ComponentList list(ids, math, response->mutable_components());
		for (const auto& component : components)
			list.Add(component);
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::ReplaceComponents(grpc::ServerContext* context,
	const sld_mcp::v1::ReplaceComponentsRequest* request,
	sld_mcp::v1::ReplaceComponentsResponse* response)
{
	if (request->component_persist_ids_size() == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one component to replace");
	if (request->path().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the file to replace them with");

	return m_executor.Execute(context, "ReplaceComponents", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
			return status;
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		PersistIds ids(ext);

		const ATL::CComBSTR path = FromUtf8(request->path());
		const ATL::CComBSTR configuration = FromUtf8(request->configuration());
		ATL::CComBSTR assemblyPath;
		HRESULT hr = doc->GetPathName(&assemblyPath);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetPathName", hr);
		if (SameFile(path.m_str, assemblyPath.m_str))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that's the assembly itself -- an assembly can't contain itself");
		if (grpc::Status status = CheckConfiguration(swApp, path, configuration); !status.ok())
			return status;

		// The components asked for: top-level ones (ReplaceComponents2 on a
		// nested component would change the subassembly's file), not already
		// of the new file.
		std::vector<ATL::CComPtr<IComponent2>> targets;
		std::set<std::string> targetIds;
		std::vector<std::wstring> targetFiles;
		for (const auto& persistId : request->component_persist_ids())
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, persistId, obj); !status.ok())
				return status;
			ATL::CComQIPtr<IComponent2> component(obj.p);
			if (!component)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "that isn't a component");
			const std::string name = ComponentName(component);
			if (ATL::CComPtr<IComponent2> parent = ParentOf(component))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" is inside the subassembly \"" + ComponentName(parent) + "\" -- replace the "
					"whole subassembly, or open its file (open_document) and replace the component there");
			const std::wstring file = ComponentPath(component);
			if (SameFile(file, path.m_str))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" already is an instance of " + ToUtf8(path));
			if (!targetIds.insert(ids.Get(component)).second)
				continue;
			targets.push_back(ATL::CComPtr<IComponent2>(component));
			targetFiles.push_back(file);
		}

		// What the replacement covers (with all_instances, the other top-level
		// instances of the same files too), in assembly order, and what it was.
		const std::vector<ATL::CComPtr<IComponent2>> allBefore = AllComponents(assembly);
		struct Covered
		{
			std::string persistId;
			sld_mcp::v1::ComponentReplacement* out = nullptr;
		};
		std::vector<Covered> covered;
		std::set<std::string> coveredIds;
		std::set<std::string> newFileBefore;  // components of the new file already there
		for (const auto& component : allBefore)
		{
			const std::wstring file = ComponentPath(component);
			const std::string pid = ids.Get(component);
			if (SameFile(file, path.m_str))
				newFileBefore.insert(pid);
			if (ParentOf(component))
				continue;
			const bool asked = targetIds.count(pid) > 0;
			const bool sameFile = request->all_instances() &&
				std::any_of(targetFiles.begin(), targetFiles.end(), [&](const std::wstring& f) { return SameFile(f, file); });
			if (!asked && !sameFile)
				continue;
			Covered c{ pid, response->add_replaced() };
			DescribeComponent(component, ids, math, c.out->mutable_before());
			covered.push_back(c);
			coveredIds.insert(pid);
		}

		// Mates that touch a covered component or anything inside one: those
		// are the ones SolidWorks has to re-attach.
		std::set<std::string> affected = coveredIds;
		for (const auto& component : allBefore)
		{
			for (ATL::CComPtr<IComponent2> c = ParentOf(component); c; c = ParentOf(c))
				if (coveredIds.count(ids.Get(c)))
				{
					affected.insert(ids.Get(component));
					break;
				}
		}
		std::vector<sld_mcp::v1::MateInfo> matesBefore;
		for (const auto& mate : MateFeatures(doc))
		{
			sld_mcp::v1::MateInfo info;
			DescribeMate(mate, ids, &info);
			if (std::any_of(info.component_persist_ids().begin(), info.component_persist_ids().end(),
					[&](const std::string& pid) { return affected.count(pid) > 0; }))
				matesBefore.push_back(std::move(info));
		}
		const auto errorsBefore = FeatureErrors(FeatureTree(doc));

		// ReplaceComponents2 works on the selection, like the UI's Replace
		// Components.
		doc->ClearSelection2(VARIANT_TRUE);
		for (const auto& component : targets)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(component->Select4(VARIANT_TRUE, nullptr, VARIANT_FALSE, &selected)) || !selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select \"" + ComponentName(component) + "\" -- is it suppressed or hidden?");
			}
		}
		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = assembly->ReplaceComponents2(path, configuration, request->all_instances() ? VARIANT_TRUE : VARIANT_FALSE,
			configuration.Length() > 0 ? swReplaceComponentsConfiguration_ManuallySelect
									   : swReplaceComponentsConfiguration_MatchName,
			VARIANT_TRUE, &ok);
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IAssemblyDoc::ReplaceComponents2", hr);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);

		// Find the components again. Persist ids are looked up afresh: the
		// replaced component may be a new object, or the old one with a new
		// reference. A covered component's persist id that still resolves
		// answers directly; the rest are paired in assembly order with the new
		// file's components that weren't there before (the replacement keeps
		// the tree position), when the counts agree.
		PersistIds idsAfter(ext);
		std::set<std::string> paired;
		std::vector<Covered*> lost;
		for (auto& c : covered)
		{
			ATL::CComPtr<IDispatch> obj;
			ATL::CComQIPtr<IComponent2> component;
			if (ResolvePersistId(ext, c.persistId, obj).ok())
				component = obj.p;
			if (!component)
			{
				lost.push_back(&c);
				continue;
			}
			DescribeComponent(component, idsAfter, math, c.out->mutable_after());
			paired.insert(c.out->after().persist_id());
		}
		std::vector<ATL::CComPtr<IComponent2>> fresh;
		for (const auto& component : AllComponents(assembly))
		{
			if (ParentOf(component) || !SameFile(ComponentPath(component), path.m_str))
				continue;
			const std::string pid = idsAfter.Get(component);
			if (!newFileBefore.count(pid) && !paired.count(pid))
				fresh.push_back(component);
		}
		if (!lost.empty() && fresh.size() == lost.size())
		{
			for (size_t i = 0; i < lost.size(); ++i)
				DescribeComponent(fresh[i], idsAfter, math, lost[i]->out->mutable_after());
			fresh.clear();
		}
		for (const auto& component : fresh)
			DescribeComponent(component, idsAfter, math, response->add_unpaired());

		const bool anyReplaced = std::any_of(covered.begin(), covered.end(), [&](const Covered& c) {
			return c.out->has_after() && SameFile(std::wstring(FromUtf8(c.out->after().file()).m_str), path.m_str);
		}) || response->unpaired_size() > 0;
		if (!anyReplaced)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't replace anything with " + ToUtf8(path) + (ok ? "" : " (ReplaceComponents2 failed)") +
				" -- is it a part or assembly file of this SolidWorks version, and not an assembly that contains "
				"this one?");

		// The mates again, by name (a replacement keeps them, re-attached or not).
		std::map<std::string, ATL::CComPtr<IFeature>> matesAfter;
		for (const auto& mate : MateFeatures(doc))
			matesAfter.emplace(FeatureName(mate), mate);
		std::set<std::string> reported;
		for (const auto& before : matesBefore)
		{
			sld_mcp::v1::ReplacedMate* out = response->add_mates();
			*out->mutable_before() = before;
			reported.insert(before.name());
			auto it = matesAfter.find(before.name());
			if (it == matesAfter.end())
			{
				out->set_broken(true);
				continue;
			}
			DescribeMate(it->second, idsAfter, out->mutable_after());
			const bool missing = DiagnoseMate(it->second, idsAfter, out->mutable_entities());
			const auto& after = out->after();
			out->set_broken(missing || (after.error_code() != swFeatureErrorNone && !after.error_is_warning()));
		}

		auto* problems = response->mutable_new_errors();
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), problems);
		for (int i = problems->size() - 1; i >= 0; --i)
			if (reported.count(problems->Get(i).name()))
				problems->DeleteSubrange(i, 1);
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::FixComponents(grpc::ServerContext* context,
	const sld_mcp::v1::FixComponentsRequest* request,
	sld_mcp::v1::FixComponentsResponse* response)
{
	if (request->component_persist_ids_size() == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one component to fix or float");

	return m_executor.Execute(context, "FixComponents", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
			return status;
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		PersistIds ids(ext);

		// The components, each once. A nested one is fixed or floating in its
		// subassembly's file: FixComponent / UnfixComponent leave it alone, even
		// in a flexible subassembly (seen 2026-10-25).
		const bool fix = request->fixed();
		std::vector<ATL::CComPtr<IComponent2>> components;
		std::vector<ATL::CComPtr<IComponent2>> toChange;
		std::set<std::string> seen;
		for (const auto& persistId : request->component_persist_ids())
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, persistId, obj); !status.ok())
				return status;
			ATL::CComQIPtr<IComponent2> component(obj.p);
			if (!component)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "that isn't a component");
			if (ATL::CComPtr<IComponent2> parent = ParentOf(component))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + ComponentName(component) + "\" is inside the subassembly \"" + ComponentName(parent) +
					"\" -- fix or float the whole subassembly, or open its file (open_document) and do it there");
			if (!seen.insert(ids.Get(component)).second)
				continue;
			components.push_back(ATL::CComPtr<IComponent2>(component));
			DescribeComponent(component, ids, math, response->add_components()->mutable_before());
			VARIANT_BOOL fixed = VARIANT_FALSE;
			component->IsFixed(&fixed);
			if ((fixed != VARIANT_FALSE) != fix)
				toChange.push_back(ATL::CComPtr<IComponent2>(component));
		}

		if (!toChange.empty())
		{
			const auto errorsBefore = FeatureErrors(FeatureTree(doc));
			// FixComponent / UnfixComponent work on the selection, like the UI's
			// Fix / Float.
			doc->ClearSelection2(VARIANT_TRUE);
			for (const auto& component : toChange)
			{
				VARIANT_BOOL selected = VARIANT_FALSE;
				if (FAILED(component->Select4(VARIANT_TRUE, nullptr, VARIANT_FALSE, &selected)) || !selected)
				{
					doc->ClearSelection2(VARIANT_TRUE);
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"SolidWorks wouldn't select \"" + ComponentName(component) + "\" -- is it suppressed?");
				}
			}
			PreferenceToggles toggles(swApp);
			toggles.Set(swShowErrorsEveryRebuild, false);
			HRESULT hr = fix ? assembly->FixComponent() : assembly->UnfixComponent();
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError(fix ? "IAssemblyDoc::FixComponent" : "IAssemblyDoc::UnfixComponent", hr);
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
			doc->GraphicsRedraw2();
		}

		for (size_t i = 0; i < components.size(); ++i)
			DescribeComponent(components[i], ids, math, response->mutable_components(static_cast<int>(i))->mutable_after());
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::MoveComponent(grpc::ServerContext* context,
	const sld_mcp::v1::MoveComponentRequest* request,
	sld_mcp::v1::MoveComponentResponse* response)
{
	constexpr int kMaxSteps = 360;
	if (!IsFinite(request->translation()) || !IsFinite(request->rotation_axis()) ||
		!std::isfinite(request->rotation_angle()) || !IsFinite(request->rotation_center()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the move isn't finite numbers");
	const Eigen::Vector3d translation = ToVector(request->translation());
	const double angle = request->rotation_angle();
	Eigen::Vector3d axis = ToVector(request->rotation_axis());
	if (angle != 0.0)
	{
		if (axis.norm() < 1e-9)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a rotation needs an axis");
		axis.normalize();
	}
	if (translation.isZero(0.0) && angle == 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a translation or a rotation");
	if (request->steps() < 1 || request->steps() > kMaxSteps)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"steps must be 1.." + std::to_string(kMaxSteps));
	if (!std::isfinite(request->time_budget()) || request->time_budget() < 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "time_budget must be 0 or more seconds");
	const bool checkCollisions = request->check_collisions() || request->stop_at_collision();

	return m_executor.Execute(context, "MoveComponent", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
			return status;
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		PersistIds ids(ext);

		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, request->component_persist_id(), obj); !status.ok())
			return status;
		ATL::CComQIPtr<IComponent2> component(obj.p);
		if (!component)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "that isn't a component");
		const std::string name = ComponentName(component);
		if (ATL::CComPtr<IComponent2> parent = ParentOf(component))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" is inside the subassembly \"" + ComponentName(parent) +
				"\" -- move the whole subassembly, or open its file (open_document) and move it there");
		if (IsSuppressed(component))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "\"" + name + "\" is suppressed");
		// SetTransformAndSolve2 moves a fixed component too, and whatever is
		// locked to it (seen 2026-10-25).
		VARIANT_BOOL fixed = VARIANT_FALSE;
		component->IsFixed(&fixed);
		if (fixed)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" is fixed -- float it first (fix_components with fixed=false), or move what is "
				"mated to it instead");

		SavedPose start;
		Eigen::Affine3d startPlacement;
		if (!ReadPose(component, start) || !PlacementOf(math, start.xform, startPlacement))
			return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't read where \"" + name + "\" is");
		const Eigen::Vector3d center = request->has_rotation_center()
			? ToVector(request->rotation_center())
			: BoxCenter(component, startPlacement.translation());
		SetVec3(response->mutable_rotation_center(), center);
		DescribeComponent(component, ids, math, response->mutable_component()->mutable_before());

		// Every top-level component where it is: who the solve moved, and what
		// to put back.
		const std::vector<SavedPose> saved = TopLevelPoses(assembly);
		const auto errorsBefore = FeatureErrors(FeatureTree(doc));

		// Each step's target is the starting pose moved by that fraction of the
		// motion: turned about the center, then translated.
		auto motion = [&](double s) {
			Eigen::Affine3d m = Eigen::Affine3d::Identity();
			m.translate(s * translation);
			if (angle != 0.0)
				m.translate(center).rotate(Eigen::AngleAxisd(s * angle, axis)).translate(-center);
			return m;
		};
		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);
		std::unique_ptr<QuietSolves> quiet;
		if (checkCollisions)
			quiet = std::make_unique<QuietSolves>(swApp, doc);

		// Collision checks: interferences before the move, then after each step;
		// with stop_at_collision the poses before each step, to go back to.
		constexpr int kMaxInterferences = 200;
		std::unique_ptr<InterferenceSession> session;
		std::set<std::string> overlapping;
		std::vector<SavedPose> previous;
		if (checkCollisions)
		{
			if (grpc::Status status = StartCollisionChecks(assembly, session); !status.ok())
				return status;
			bool truncated = false;
			if (grpc::Status status = session->Check(ids, nullptr, kMaxInterferences,
					response->mutable_start_interferences(), truncated); !status.ok())
				return status;
			for (const auto& interference : response->start_interferences())
				overlapping.insert(InterferenceKey(interference));
			if (request->stop_at_collision())
				previous = saved;
		}
		const auto started = std::chrono::steady_clock::now();
		const int steps = request->steps();
		for (int k = 1; k <= steps; ++k)
		{
			if (checkCollisions && k > 1 && request->time_budget() > 0.0 &&
				std::chrono::duration<double>(std::chrono::steady_clock::now() - started).count() > request->time_budget())
			{
				response->set_out_of_time(true);
				break;
			}
			sld_mcp::v1::ComponentPlacement target;
			SetPlacement(motion(static_cast<double>(k) / steps) * startPlacement, &target);
			ATL::CComPtr<IMathTransform> xform = ToMathTransform(math, target);
			if (!xform)
				return grpc::Status(grpc::StatusCode::INTERNAL, "IMathUtility::CreateTransform failed");
			VARIANT_BOOL ok = VARIANT_FALSE;
			HRESULT hr = component->SetTransformAndSolve2(xform, &ok);
			if (FAILED(hr))
				return ComError("IComponent2::SetTransformAndSolve2", hr);
			if (!ok)
			{
				response->set_refused_step(k);
				break;
			}
			if (!checkCollisions)
				continue;

			sld_mcp::v1::MotionStep* step = response->add_collision_steps();
			step->set_step(k);
			SavedPose now;
			Eigen::Affine3d reached;
			if (ReadPose(component, now) && PlacementOf(math, now.xform, reached))
				SetPlacement(reached, step->mutable_placement());
			bool truncated = false;
			if (grpc::Status status = session->Check(ids, nullptr, kMaxInterferences, step->mutable_interferences(),
					truncated); !status.ok())
				return status;
			step->set_truncated(truncated);
			if (response->collision_step() == 0 && HasNewInterference(overlapping, step->interferences()))
			{
				response->set_collision_step(k);
				if (request->stop_at_collision())
				{
					response->set_backed_off(true);
					break;
				}
			}
			if (request->stop_at_collision())
				previous = TopLevelPoses(assembly);
		}
		session.reset();  // Done: out of interference mode before anything else
		quiet.reset();
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		if (response->backed_off())
			RestorePoses(doc, previous);
		SetPlacement(motion(1.0) * startPlacement, response->mutable_target());
		doc->EditRebuild3(&rebuilt);
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());

		DescribeComponent(component, ids, math, response->mutable_component()->mutable_after());
		const std::vector<const SavedPose*> moved = MovedSince(saved);
		for (const SavedPose* pose : moved)
		{
			if (SameObject(pose->component, component))
				continue;
			sld_mcp::v1::ComponentMove* out = response->add_others();
			DescribeComponent(pose->component, ids, math, out->mutable_after());
			Eigen::Affine3d before;
			out->mutable_before()->set_persist_id(out->after().persist_id());
			out->mutable_before()->set_name(out->after().name());
			if (PlacementOf(math, pose->xform, before))
				SetPlacement(before, out->mutable_before());
		}

		if (request->restore())
		{
			for (const auto& name : RestorePoses(doc, saved))
				response->add_not_restored(name);
			response->set_restored(response->not_restored_size() == 0);
		}
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::InspectMobility(grpc::ServerContext* context,
	const sld_mcp::v1::InspectMobilityRequest* request,
	sld_mcp::v1::InspectMobilityResponse* response)
{
	const double step = request->step();
	const double stepAngle = request->step_angle();
	if (!std::isfinite(step) || step <= 0.0 || !std::isfinite(stepAngle) || stepAngle <= 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "step and step_angle must be positive");
	if (!std::isfinite(request->time_budget()) || request->time_budget() < 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "time_budget must be 0 or more");
	const auto started = std::chrono::steady_clock::now();
	auto outOfTime = [&] {
		return request->time_budget() > 0.0 &&
			std::chrono::duration<double>(std::chrono::steady_clock::now() - started).count() > request->time_budget();
	};

	return m_executor.Execute(context, "InspectMobility", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
			return status;
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		PersistIds ids(ext);

		// Every top-level component where it is: what a probe moved, and what
		// to put back.
		std::vector<SavedPose> saved;
		for (const auto& c : AllComponents(assembly))
		{
			if (ParentOf(c))
				continue;
			SavedPose pose;
			if (ReadPose(c, pose))
				saved.push_back(std::move(pose));
		}

		// The components to probe, each once.
		std::vector<ATL::CComPtr<IComponent2>> targets;
		std::set<std::string> targetIds;
		if (request->component_persist_ids_size() == 0)
		{
			for (const auto& pose : saved)
				if (targetIds.insert(ids.Get(pose.component)).second)
					targets.push_back(pose.component);
		}
		for (const auto& persistId : request->component_persist_ids())
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, persistId, obj); !status.ok())
				return status;
			ATL::CComQIPtr<IComponent2> component(obj.p);
			if (!component)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "that isn't a component");
			if (ATL::CComPtr<IComponent2> parent = ParentOf(component))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + ComponentName(component) + "\" is inside the subassembly \"" + ComponentName(parent) +
					"\" -- it moves with the whole subassembly: ask about that, or open its file (open_document)");
			if (targetIds.insert(ids.Get(component)).second)
				targets.push_back(ATL::CComPtr<IComponent2>(component));
		}

		// A probe: from the starting pose, translate by +-step along an axis, or
		// turn by +-stepAngle about an axis through the center.
		auto nudge = [&](int axis, double sign, const Eigen::Vector3d& center) {
			Eigen::Affine3d m = Eigen::Affine3d::Identity();
			const Eigen::Vector3d unit = Eigen::Vector3d::Unit(axis % 3);
			if (axis < 3)
				m.translate(sign * step * unit);
			else
				m.translate(center).rotate(Eigen::AngleAxisd(sign * stepAngle, unit)).translate(-center);
			return m;
		};

		// Every target described first: once a probe has put components back
		// (put_Transform2, no solve), GetConstrainedStatus answers swNoSolution
		// for all of them until the rebuild (seen 2026-10-25).
		std::vector<Eigen::Affine3d> starts;
		for (const auto& component : targets)
		{
			sld_mcp::v1::ComponentMobility* out = response->add_components();
			DescribeComponent(component, ids, math, out->mutable_component());
			long constrained = 0;
			component->GetConstrainedStatus(&constrained);
			out->set_constrained(ToConstrainedStatus(constrained));
			const auto start = std::find_if(saved.begin(), saved.end(),
				[&](const SavedPose& p) { return SameObject(p.component, component); });
			Eigen::Affine3d startPlacement;
			if (start == saved.end() || !PlacementOf(math, start->xform, startPlacement))
				return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't read where \"" + ComponentName(component) + "\" is");
			starts.push_back(startPlacement);
			SetVec3(out->mutable_center(), BoxCenter(component, startPlacement.translation()));
		}

		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);
		std::set<std::string> movedOthers;
		bool probedAny = false;
		{
			QuietSolves quiet(swApp, doc);
			for (size_t i = 0; i < targets.size(); ++i)
			{
				const auto& component = targets[i];
				sld_mcp::v1::ComponentMobility* out = response->mutable_components(static_cast<int>(i));
				const std::string pid = out->component().persist_id();
				const Eigen::Affine3d& startPlacement = starts[i];
				const Eigen::Vector3d center = ToVector(out->center());
				// SetTransformAndSolve2 moves a fixed component too (seen
				// 2026-10-25): nothing to learn from probing one.
				if (out->component().fixed() || IsSuppressed(component))
					continue;
				if (outOfTime())
				{
					response->set_out_of_time(true);
					continue;
				}
				out->set_probed(true);
				// Asked about everything, a component SolidWorks calls fully
				// defined isn't nudged: its mates hold it against what it's mated
				// to, and any motion it shares with that shows up when the
				// floating ones are nudged. Several times faster on a real
				// assembly (a component takes seconds).
				if (request->component_persist_ids_size() == 0 && out->constrained() == sld_mcp::v1::CONSTRAINED_STATUS_FULLY)
					continue;
				probedAny = true;

				for (int axis = 0; axis < 6; ++axis)
				{
					// The + nudge went all the way: that way is free, and the -
					// one can't add a direction (it matters against a limit).
					bool reached = false;
					for (double sign : { 1.0, -1.0 })
					{
						if (reached)
							break;
						sld_mcp::v1::MobilityProbe* probe = out->add_probes();
						probe->set_axis(axis);
						probe->set_negative(sign < 0.0);
						const Eigen::Affine3d wanted = nudge(axis, sign, center) * startPlacement;
						sld_mcp::v1::ComponentPlacement target;
						SetPlacement(wanted, &target);
						ATL::CComPtr<IMathTransform> xform = ToMathTransform(math, target);
						if (!xform)
							return grpc::Status(grpc::StatusCode::INTERNAL, "IMathUtility::CreateTransform failed");
						VARIANT_BOOL ok = VARIANT_FALSE;
						HRESULT hr = component->SetTransformAndSolve2(xform, &ok);
						if (FAILED(hr))
							return ComError("IComponent2::SetTransformAndSolve2", hr);
						probe->set_refused(!ok);

						// Who moved, and everyone back.
						for (const auto& pose : saved)
						{
							SavedPose now;
							if (!ReadPose(pose.component, now) || SamePose(now.data, pose.data, 1e-9))
								continue;
							sld_mcp::v1::MovedComponent* moved = probe->add_moved();
							moved->set_persist_id(ids.Get(pose.component));
							Eigen::Affine3d placement;
							if (PlacementOf(math, now.xform, placement))
							{
								SetPlacement(placement, moved->mutable_placement());
								if (moved->persist_id() == pid)
									reached = (placement.translation() - wanted.translation()).norm() < 0.01 * step &&
										(placement.linear() - wanted.linear()).norm() < 0.01 * stepAngle;
							}
							if (moved->persist_id() != pid)
								movedOthers.insert(moved->persist_id());
							pose.component->put_Transform2(pose.xform);
						}
					}
				}
			}
		}

		// The other components that moved along, where they are (back at the
		// start).
		for (const auto& pose : saved)
		{
			const std::string pid = ids.Get(pose.component);
			if (!movedOthers.count(pid) || targetIds.count(pid))
				continue;
			sld_mcp::v1::ComponentMobility* out = response->add_others();
			DescribeComponent(pose.component, ids, math, out->mutable_component());
			Eigen::Affine3d placement;
			PlacementOf(math, pose.xform, placement);
			SetVec3(out->mutable_center(), BoxCenter(pose.component, placement.translation()));
		}

		if (probedAny)
		{
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
		}
		// The rebuild re-solves what isn't solved exactly: two components
		// locked to each other moved by micrometers (seen 2026-10-25).
		for (const auto& pose : saved)
		{
			SavedPose now;
			if (!ReadPose(pose.component, now) || !SamePose(now.data, pose.data, 1e-5))
				response->add_not_restored(ComponentName(pose.component));
		}
		response->set_restored(response->not_restored_size() == 0);
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}
