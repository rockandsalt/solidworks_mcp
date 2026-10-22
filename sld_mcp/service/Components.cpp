// Components.cpp : InsertComponents (see docs/notes/architecture.md, "Assemblies + mates").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"

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
}

grpc::Status SolidWorksService::InsertComponents(grpc::ServerContext* /*context*/,
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

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

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
	if (configuration.Length() > 0)
	{
		ATL::CComVariant namesVariant;
		hr = swApp->GetConfigurationNames(path, &namesVariant);
		if (FAILED(hr))
			return ComError("ISldWorks::GetConfigurationNames", hr);
		const std::vector<ATL::CComBSTR> names = comvariant::ToBstrs(namesVariant);
		if (std::none_of(names.begin(), names.end(), [&](const ATL::CComBSTR& n) { return n == configuration; }))
		{
			std::string list;
			for (const auto& n : names)
				list += (list.empty() ? "" : ", ") + ("\"" + ToUtf8(n) + "\"");
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				ToUtf8(path) + " has no configuration \"" + ToUtf8(configuration) + "\"" +
				(list.empty() ? std::string() : " -- it has " + list));
		}
	}

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
}
