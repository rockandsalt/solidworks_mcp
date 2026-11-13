// Document preference transactions. Kept separate from CAD dispatch so rollback
// order and failures can be exercised without a running SolidWorks instance.
#pragma once
#include "Common.h"
#include <functional>

namespace sld_mcp::service
{
	template <typename Extension>
	grpc::Status ApplyDocumentPreferenceChanges(Extension* ext,
		const sld_mcp::v1::DocumentPreferencesRequest& request, bool& changed)
	{
		changed = false;
		for (const auto& [id, value] : request.set_doubles())
			if (!std::isfinite(value))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "preference double must be finite: " + std::to_string(id));

		const long none = swDetailingNoOptionSpecified;
		struct Restore { std::string name; std::function<bool()> write; };
		std::vector<Restore> undo;
		auto refused = [&](const std::string& why) {
			std::string failed;
			// Values depend on the modes enabled by toggles: restore in reverse
			// write order, leaving those toggles enabled until their values are back.
			for (auto it = undo.rbegin(); it != undo.rend(); ++it)
				if (!it->write())
					failed += (failed.empty() ? "" : ", ") + it->name;
			changed = !failed.empty();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, why + (changed
				? " -- couldn't restore " + failed + "; document preferences may remain changed"
				: " -- original preferences were restored"));
		};
		for (const auto& [id, value] : request.set_toggles())
		{
			VARIANT_BOOL old = VARIANT_FALSE, ok = VARIANT_FALSE;
			const std::string what = "toggle " + std::to_string(id);
			if (FAILED(ext->GetUserPreferenceToggle(id, none, &old)))
				return refused("couldn't read original " + what);
			undo.push_back({ what, [ext, id, old, none] {
				VARIANT_BOOL restored = VARIANT_FALSE;
				return SUCCEEDED(ext->SetUserPreferenceToggle(id, none, old, &restored)) && restored;
			} });
			if (FAILED(ext->SetUserPreferenceToggle(id, none, value ? VARIANT_TRUE : VARIANT_FALSE, &ok)) || !ok)
				return refused("SolidWorks refused " + what);
		}
		for (const auto& [id, value] : request.set_integers())
		{
			long old = 0;
			VARIANT_BOOL ok = VARIANT_FALSE;
			const std::string what = "integer " + std::to_string(id);
			if (FAILED(ext->GetUserPreferenceInteger(id, none, &old)))
				return refused("couldn't read original " + what);
			undo.push_back({ what, [ext, id, old, none] {
				VARIANT_BOOL restored = VARIANT_FALSE;
				return SUCCEEDED(ext->SetUserPreferenceInteger(id, none, old, &restored)) && restored;
			} });
			if (FAILED(ext->SetUserPreferenceInteger(id, none, value, &ok)) || !ok)
				return refused("SolidWorks refused " + what);
		}
		for (const auto& [id, value] : request.set_doubles())
		{
			double old = 0.0;
			VARIANT_BOOL ok = VARIANT_FALSE;
			const std::string what = "double " + std::to_string(id);
			if (FAILED(ext->GetUserPreferenceDouble(id, none, &old)))
				return refused("couldn't read original " + what);
			undo.push_back({ what, [ext, id, old, none] {
				VARIANT_BOOL restored = VARIANT_FALSE;
				return SUCCEEDED(ext->SetUserPreferenceDouble(id, none, old, &restored)) && restored;
			} });
			if (FAILED(ext->SetUserPreferenceDouble(id, none, value, &ok)) || !ok)
				return refused("SolidWorks refused " + what);
		}
		changed = !undo.empty();
		return grpc::Status::OK;
	}
}
