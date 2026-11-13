// Preferences.cpp : DocumentPreferences -- the active document's properties
// by SolidWorks id (see docs/notes/dimxpert.md: DimXpert's general
// tolerance is one of them).
#include "pch.h"
#include "Common.h"

using namespace sld_mcp::service;

grpc::Status SolidWorksService::DocumentPreferences(grpc::ServerContext* context,
	const sld_mcp::v1::DocumentPreferencesRequest* request,
	sld_mcp::v1::DocumentPreferencesResponse* response)
{
	return m_executor.Execute(context, "DocumentPreferences", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		HRESULT hr = swApp->get_IActiveDoc2(&doc);
		if (FAILED(hr))
			return ComError("ISldWorks::IActiveDoc2", hr);
		if (!doc)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		const long none = swDetailingNoOptionSpecified;

		// What was there before, to put back if SolidWorks refuses a value.
		std::map<int32_t, VARIANT_BOOL> oldToggles;
		std::map<int32_t, long> oldIntegers;
		std::map<int32_t, double> oldDoubles;
		auto restore = [&] {
			VARIANT_BOOL ok = VARIANT_FALSE;
			for (const auto& [id, value] : oldToggles)
				ext->SetUserPreferenceToggle(id, none, value, &ok);
			for (const auto& [id, value] : oldIntegers)
				ext->SetUserPreferenceInteger(id, none, value, &ok);
			for (const auto& [id, value] : oldDoubles)
				ext->SetUserPreferenceDouble(id, none, value, &ok);
		};
		auto refused = [&](const char* what, int32_t id) {
			restore();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("SolidWorks refused ") + what + " " + std::to_string(id) + " -- nothing was changed");
		};
		for (const auto& [id, value] : request->set_toggles())
		{
			VARIANT_BOOL old = VARIANT_FALSE, ok = VARIANT_FALSE;
			ext->GetUserPreferenceToggle(id, none, &old);
			if (FAILED(ext->SetUserPreferenceToggle(id, none, value ? VARIANT_TRUE : VARIANT_FALSE, &ok)) || !ok)
				return refused("toggle", id);
			oldToggles.emplace(id, old);
		}
		for (const auto& [id, value] : request->set_integers())
		{
			long old = 0;
			VARIANT_BOOL ok = VARIANT_FALSE;
			ext->GetUserPreferenceInteger(id, none, &old);
			if (FAILED(ext->SetUserPreferenceInteger(id, none, value, &ok)) || !ok)
				return refused("integer", id);
			oldIntegers.emplace(id, old);
		}
		for (const auto& [id, value] : request->set_doubles())
		{
			double old = 0.0;
			VARIANT_BOOL ok = VARIANT_FALSE;
			ext->GetUserPreferenceDouble(id, none, &old);
			if (!std::isfinite(value) || FAILED(ext->SetUserPreferenceDouble(id, none, value, &ok)) || !ok)
				return refused("double", id);
			oldDoubles.emplace(id, old);
		}
		const bool changed = !oldToggles.empty() || !oldIntegers.empty() || !oldDoubles.empty();
		if (changed)
		{
			doc->SetSaveFlag();
			if (request->rebuild())
			{
				VARIANT_BOOL rebuilt = VARIANT_FALSE;
				doc->EditRebuild3(&rebuilt);
			}
		}

		// Everything read or set, as it is now.
		auto& toggles = *response->mutable_toggles();
		auto& integers = *response->mutable_integers();
		auto& doubles = *response->mutable_doubles();
		std::set<int32_t> ids(request->toggles().begin(), request->toggles().end());
		for (const auto& [id, value] : request->set_toggles())
			ids.insert(id);
		for (int32_t id : ids)
		{
			VARIANT_BOOL value = VARIANT_FALSE;
			if (SUCCEEDED(ext->GetUserPreferenceToggle(id, none, &value)))
				toggles[id] = value != VARIANT_FALSE;
		}
		ids = std::set<int32_t>(request->integers().begin(), request->integers().end());
		for (const auto& [id, value] : request->set_integers())
			ids.insert(id);
		for (int32_t id : ids)
		{
			long value = 0;
			if (SUCCEEDED(ext->GetUserPreferenceInteger(id, none, &value)))
				integers[id] = static_cast<int32_t>(value);
		}
		ids = std::set<int32_t>(request->doubles().begin(), request->doubles().end());
		for (const auto& [id, value] : request->set_doubles())
			ids.insert(id);
		for (int32_t id : ids)
		{
			double value = 0.0;
			if (SUCCEEDED(ext->GetUserPreferenceDouble(id, none, &value)))
				doubles[id] = value;
		}
		ATL::CComBSTR title;
		if (SUCCEEDED(doc->GetTitle(&title)))
			response->set_document(ToUtf8(title));
		return grpc::Status::OK;
	});
}
