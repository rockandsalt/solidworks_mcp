// DimensionHelpers.h : Reading, creating and deleting dimensions (sketch, feature and drawing).
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	sld_mcp::v1::DimensionType ToDimensionType(long type);

	// Name, value and driven state; the type is the caller's job.
	void DescribeDimensionValue(IDimension* dim, sld_mcp::v1::DimensionInfo* info);

	bool DescribeDimension(IDisplayDimension* display, sld_mcp::v1::DimensionInfo* info);

	// Every display dimension of a feature, in order.
	std::vector<ATL::CComPtr<IDisplayDimension>> FeatureDimensions(IFeature* feature);

	std::set<std::string> DimensionNames(IFeature* feature);

	// Deletes display dimensions from the open sketch. Best effort.
	void DeleteDimensions(IModelDoc2* doc, const std::vector<ATL::CComPtr<IDisplayDimension>>& dims);

	// Sets user preference toggles for one RPC and puts them back after.
	// Declare after the ComApartment so it's restored before COM goes away.
	class PreferenceToggles
	{
	public:
		explicit PreferenceToggles(ISldWorks* app) : m_app(app) {}
		~PreferenceToggles()
		{
			for (auto it = m_saved.rbegin(); it != m_saved.rend(); ++it)
				m_app->SetUserPreferenceToggle(it->first, it->second);
			for (auto it = m_savedIntegers.rbegin(); it != m_savedIntegers.rend(); ++it)
			{
				VARIANT_BOOL ok = VARIANT_FALSE;
				m_app->SetUserPreferenceIntegerValue(it->first, it->second, &ok);
			}
			for (auto it = m_savedDoubles.rbegin(); it != m_savedDoubles.rend(); ++it)
			{
				VARIANT_BOOL ok = VARIANT_FALSE;
				m_app->SetUserPreferenceDoubleValue(it->first, it->second, &ok);
			}
		}
		PreferenceToggles(const PreferenceToggles&) = delete;
		PreferenceToggles& operator=(const PreferenceToggles&) = delete;

		void Set(long preference, bool on)
		{
			VARIANT_BOOL previous = VARIANT_FALSE;
			if (FAILED(m_app->GetUserPreferenceToggle(preference, &previous)))
				return;
			m_saved.emplace_back(preference, previous);
			m_app->SetUserPreferenceToggle(preference, on ? VARIANT_TRUE : VARIANT_FALSE);
		}

		void SetInteger(long preference, long value)
		{
			long previous = 0;
			if (FAILED(m_app->GetUserPreferenceIntegerValue(preference, &previous)))
				return;
			m_savedIntegers.emplace_back(preference, previous);
			VARIANT_BOOL ok = VARIANT_FALSE;
			m_app->SetUserPreferenceIntegerValue(preference, value, &ok);
		}

		void SetDouble(long preference, double value)
		{
			double previous = 0.0;
			if (FAILED(m_app->GetUserPreferenceDoubleValue(preference, &previous)))
				return;
			m_savedDoubles.emplace_back(preference, previous);
			VARIANT_BOOL ok = VARIANT_FALSE;
			m_app->SetUserPreferenceDoubleValue(preference, value, &ok);
		}

	private:
		ATL::CComPtr<ISldWorks> m_app;
		std::vector<std::pair<long, VARIANT_BOOL>> m_saved;
		std::vector<std::pair<long, long>> m_savedIntegers;
		std::vector<std::pair<long, double>> m_savedDoubles;
	};

	// The left side of an equation without its quotes ("Width" for
	// "\"Width\" = 50"), or empty.
	std::string EquationName(const std::string& equation);

	// Whether two names are the same, ignoring case and a trailing
	// "@<document>" ("D1@Sketch1@block.Part").
	bool SameDimensionName(const std::string& a, const std::string& b);

	// The document's equations as SolidWorks has them, in its order.
	std::vector<std::string> ReadEquations(IEquationMgr* equations);

	// The equation that drives the dimension `name`, or empty.
	std::string EquationDriving(IModelDoc2* doc, const std::string& name);

	const char* DimensionKindName(sld_mcp::v1::DimensionKind kind);

	// Creates the dimension for what's selected, with its text at `at`
	// (model space).
	HRESULT CreateDimension(IModelDoc2* doc, sld_mcp::v1::DimensionKind kind, const double at[3],
		ATL::CComPtr<IDispatch>& created);
}
