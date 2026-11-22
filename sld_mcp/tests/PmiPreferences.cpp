#include "pch.h"
#include "service/PreferenceHelpers.h"
#include "service/AnnotationHelpers.h"
#include <catch2/catch_test_macros.hpp>

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

TEST_CASE("document preference edits roll back in order and report failures")
{
	using namespace sld_mcp::service;
	sld_mcp::v1::DocumentPreferencesRequest request;
	(*request.mutable_set_toggles())[424] = true;
	(*request.mutable_set_integers())[405] = 3;
	(*request.mutable_set_doubles())[126] = 0.5;
	Preferences preferences;
	bool changed = false;

	SECTION("a failed edit restores dependent values before the mode")
	{
		const auto refused = ApplyDocumentPreferenceChanges(&preferences, request, changed);
		CHECK_FALSE(refused.ok());
		CHECK_FALSE(changed);
		CHECK_FALSE(preferences.block);
		CHECK(preferences.decimals == 2);
		CHECK(preferences.tolerance == 0.01);
	}
	SECTION("a rollback that fails is reported")
	{
		preferences.failRestore = true;
		const auto partial = ApplyDocumentPreferenceChanges(&preferences, request, changed);
		CHECK_FALSE(partial.ok());
		CHECK(changed);
		CHECK(partial.error_message().find("couldn't restore integer 405") != std::string::npos);
	}
	SECTION("an unreadable original preference is not overwritten")
	{
		preferences.failRead = true;
		const auto unreadable = ApplyDocumentPreferenceChanges(&preferences, request, changed);
		CHECK_FALSE(unreadable.ok());
		CHECK_FALSE(changed);
		CHECK_FALSE(preferences.block);
		CHECK(preferences.decimals == 2);
	}
	SECTION("edits that all succeed are applied")
	{
		request.mutable_set_doubles()->clear();
		const auto success = ApplyDocumentPreferenceChanges(&preferences, request, changed);
		CHECK(success.ok());
		CHECK(changed);
		CHECK(preferences.block);
		CHECK(preferences.decimals == 3);
	}
}

TEST_CASE("inherited precision resolves the matching linear or angular document default")
{
	using namespace sld_mcp::service;
	Preferences preferences;
	Display display;
	CHECK(PrimaryDimensionPrecision(&display, &preferences, false) == 3);
	CHECK(PrimaryDimensionPrecision(&display, &preferences, true) == 1);

	preferences.failRead = true;
	display.precision = 4;
	CHECK(PrimaryDimensionPrecision(&display, &preferences, false) == 4);
	// A failed read of the document default stays unknown.
	display.precision = swPrecisionFollowsDocumentSetting;
	CHECK_FALSE(PrimaryDimensionPrecision(&display, &preferences, false).has_value());
}
