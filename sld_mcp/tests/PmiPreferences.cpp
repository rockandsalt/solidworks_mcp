#include "pch.h"
#include "service/PreferenceHelpers.h"
#include "service/AnnotationHelpers.h"
#include <iostream>

namespace
{
	// The block mode gates its decimal settings, as SolidWorks does. The
	// refused double simulates a later setter failing after an earlier edit.
	struct Preferences
	{
		VARIANT_BOOL block = VARIANT_FALSE;
		long decimals = 2;
		double tolerance = 0.01;
		bool failRestore = false;
		bool failRead = false;
		HRESULT GetUserPreferenceToggle(long, long, VARIANT_BOOL* out) { *out = block; return S_OK; }
		HRESULT GetUserPreferenceInteger(long id, long, long* out)
		{
			if (failRead) return E_FAIL;
			*out = id == swDetailingAngularDimPrecision ? 1 : id == swDetailingLinearDimPrecision ? 3 : decimals;
			return S_OK;
		}
		HRESULT GetUserPreferenceDouble(long, long, double* out) { *out = tolerance; return S_OK; }
		HRESULT SetUserPreferenceToggle(long, long, VARIANT_BOOL value, VARIANT_BOOL* ok)
		{
			block = value; *ok = VARIANT_TRUE; return S_OK;
		}
		HRESULT SetUserPreferenceInteger(long, long, long value, VARIANT_BOOL* ok)
		{
			*ok = block && !(failRestore && value == 2) ? VARIANT_TRUE : VARIANT_FALSE;
			if (*ok) decimals = value;
			return S_OK;
		}
		HRESULT SetUserPreferenceDouble(long, long, double value, VARIANT_BOOL* ok)
		{
			*ok = value == 0.5 ? VARIANT_FALSE : VARIANT_TRUE;
			if (*ok) tolerance = value;
			return S_OK;
		}
	};
	struct Display
	{
		long precision = swPrecisionFollowsDocumentSetting;
		HRESULT GetPrimaryPrecision2(long* out) { *out = precision; return S_OK; }
	};
}

bool TestPmiPreferences()
{
	using namespace sld_mcp::service;
	sld_mcp::v1::DocumentPreferencesRequest request;
	(*request.mutable_set_toggles())[424] = true;
	(*request.mutable_set_integers())[405] = 3;
	(*request.mutable_set_doubles())[126] = 0.5;
	Preferences preferences;
	bool changed = false;
	const auto refused = ApplyDocumentPreferenceChanges(&preferences, request, changed);
	if (refused.ok() || changed || preferences.block || preferences.decimals != 2 || preferences.tolerance != 0.01)
	{
		std::cerr << "FAIL: failed preference edit did not restore dependent values before the mode\n";
		return false;
	}
	preferences.failRestore = true;
	const auto partial = ApplyDocumentPreferenceChanges(&preferences, request, changed);
	if (partial.ok() || !changed || partial.error_message().find("couldn't restore integer 405") == std::string::npos)
	{
		std::cerr << "FAIL: preference rollback failure was not reported\n";
		return false;
	}
	preferences = Preferences{};
	preferences.failRead = true;
	const auto unreadable = ApplyDocumentPreferenceChanges(&preferences, request, changed);
	if (unreadable.ok() || changed || preferences.block || preferences.decimals != 2)
	{
		std::cerr << "FAIL: unreadable original preference must not be overwritten\n";
		return false;
	}
	preferences = Preferences{};
	request.mutable_set_doubles()->clear();
	const auto success = ApplyDocumentPreferenceChanges(&preferences, request, changed);
	if (!success.ok() || !changed || !preferences.block || preferences.decimals != 3)
		return false;

	Display display;
	if (PrimaryDimensionPrecision(&display, &preferences, false) != 3 ||
		PrimaryDimensionPrecision(&display, &preferences, true) != 1)
	{
		std::cerr << "FAIL: inherited precision must resolve the matching document default\n";
		return false;
	}
	display.precision = 4;
	preferences.failRead = true;
	if (PrimaryDimensionPrecision(&display, &preferences, false) != 4)
		return false;
	display.precision = swPrecisionFollowsDocumentSetting;
	if (PrimaryDimensionPrecision(&display, &preferences, false).has_value())
	{
		std::cerr << "FAIL: failed precision read must remain unknown\n";
		return false;
	}
	std::cout << "PASS: preference rollback order/failures and inherited linear/angular precision\n";
	return true;
}
