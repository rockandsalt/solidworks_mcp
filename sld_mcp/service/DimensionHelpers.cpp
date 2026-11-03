// DimensionHelpers.cpp : see DimensionHelpers.h.
#include "pch.h"
#include "DimensionHelpers.h"
#include "Common.h"
#include "SketchHelpers.h"

namespace sld_mcp::service
{
	sld_mcp::v1::DimensionType ToDimensionType(long type)
	{
		switch (type)
		{
		case swLinearDimension:
		case swHorLinearDimension:
		case swVertLinearDimension:
		case swRadialLinearDimension:
		case swDiametricLinearDimension:
			return sld_mcp::v1::DIMENSION_TYPE_LINEAR;
		case swAngularDimension:
		case swAngularOrdinateDimension:
			return sld_mcp::v1::DIMENSION_TYPE_ANGULAR;
		case swRadialDimension:
			return sld_mcp::v1::DIMENSION_TYPE_RADIUS;
		case swDiameterDimension:
			return sld_mcp::v1::DIMENSION_TYPE_DIAMETER;
		case swArcLengthDimension:
			return sld_mcp::v1::DIMENSION_TYPE_ARC_LENGTH;
		case swOrdinateDimension:
		case swHorOrdinateDimension:
		case swVertOrdinateDimension:
			return sld_mcp::v1::DIMENSION_TYPE_ORDINATE;
		default:
			return sld_mcp::v1::DIMENSION_TYPE_OTHER;
		}
	}

	void DescribeDimensionValue(IDimension* dim, sld_mcp::v1::DimensionInfo* info)
	{
		ATL::CComBSTR name;
		if (SUCCEEDED(dim->get_FullName(&name)))
			info->set_name(ToUtf8(name));
		double value = 0.0;
		if (SUCCEEDED(dim->get_SystemValue(&value)))
			info->set_value(value);
		long driven = swDimensionDrivenUnknown;
		if (SUCCEEDED(dim->get_DrivenState(&driven)))
			info->set_driven(driven == swDimensionDriven);
	}

	bool DescribeDimension(IDisplayDimension* display, sld_mcp::v1::DimensionInfo* info)
	{
		ATL::CComPtr<IDimension> dim;
		if (FAILED(display->GetDimension2(0, &dim)) || !dim)
			return false;
		// Type2, not the obsolete GetType: GetType says swRadialDimension for a
		// diameter dimension (seen in SOLIDWORKS 2026 on AddDiameterDimension2's).
		long type = 0;
		if (SUCCEEDED(display->get_Type2(&type)))
			info->set_type(ToDimensionType(type));
		DescribeDimensionValue(dim, info);
		return true;
	}

	std::vector<ATL::CComPtr<IDisplayDimension>> FeatureDimensions(IFeature* feature)
	{
		std::vector<ATL::CComPtr<IDisplayDimension>> out;
		ATL::CComPtr<IDispatch> disp;
		if (FAILED(feature->GetFirstDisplayDimension(&disp)))
			return out;
		while (disp && out.size() < kMaxSketchItems)
		{
			if (ATL::CComQIPtr<IDisplayDimension> display(disp.p); display)
				out.push_back(display);
			ATL::CComPtr<IDispatch> next;
			if (FAILED(feature->GetNextDisplayDimension(disp, &next)))
				break;
			disp = next;
		}
		return out;
	}

	std::set<std::string> DimensionNames(IFeature* feature)
	{
		std::set<std::string> names;
		for (const auto& display : FeatureDimensions(feature))
		{
			sld_mcp::v1::DimensionInfo info;
			if (DescribeDimension(display, &info))
				names.insert(info.name());
		}
		return names;
	}

	void DeleteDimensions(IModelDoc2* doc, const std::vector<ATL::CComPtr<IDisplayDimension>>& dims)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
		for (const auto& dim : dims)
		{
			ATL::CComPtr<IAnnotation> annotation;
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (SUCCEEDED(dim->IGetAnnotation(&annotation)) && annotation &&
				SUCCEEDED(annotation->Select3(VARIANT_TRUE, nullptr, &selected)))
				any = any || selected;
		}
		if (any)
			doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
	}

	const char* DimensionKindName(sld_mcp::v1::DimensionKind kind)
	{
		switch (kind)
		{
		case sld_mcp::v1::DIMENSION_KIND_LENGTH:     return "length";
		case sld_mcp::v1::DIMENSION_KIND_DISTANCE:   return "distance";
		case sld_mcp::v1::DIMENSION_KIND_HORIZONTAL: return "horizontal";
		case sld_mcp::v1::DIMENSION_KIND_VERTICAL:   return "vertical";
		case sld_mcp::v1::DIMENSION_KIND_DIAMETER:   return "diameter";
		case sld_mcp::v1::DIMENSION_KIND_RADIUS:     return "radius";
		case sld_mcp::v1::DIMENSION_KIND_ANGLE:      return "angle";
		default:                                      return "unknown";
		}
	}

	std::string EquationName(const std::string& equation)
	{
		const size_t open = equation.find('"');
		const size_t close = open == std::string::npos ? open : equation.find('"', open + 1);
		return close == std::string::npos ? std::string() : equation.substr(open + 1, close - open - 1);
	}

	bool SameDimensionName(const std::string& a, const std::string& b)
	{
		auto trimmed = [](const std::string& name) {
			const size_t first = name.find('@');
			const size_t second = first == std::string::npos ? first : name.find('@', first + 1);
			return second == std::string::npos ? name : name.substr(0, second);
		};
		return _stricmp(trimmed(a).c_str(), trimmed(b).c_str()) == 0;
	}

	std::vector<std::string> ReadEquations(IEquationMgr* equations)
	{
		std::vector<std::string> out;
		long count = 0;
		if (!equations || FAILED(equations->GetCount(&count)))
			return out;
		for (long i = 0; i < count; ++i)
		{
			ATL::CComBSTR text;
			equations->get_Equation(i, &text);
			out.push_back(ToUtf8(text));
		}
		return out;
	}

	std::string EquationDriving(IModelDoc2* doc, const std::string& name)
	{
		ATL::CComPtr<IEquationMgr> equations;
		if (FAILED(doc->GetEquationMgr(&equations)) || !equations)
			return {};
		for (const std::string& equation : ReadEquations(equations))
			if (SameDimensionName(EquationName(equation), name))
				return equation;
		return {};
	}

	HRESULT CreateDimension(IModelDoc2* doc, sld_mcp::v1::DimensionKind kind, const double at[3],
		ATL::CComPtr<IDispatch>& created)
	{
		switch (kind)
		{
		case sld_mcp::v1::DIMENSION_KIND_HORIZONTAL:
			return doc->AddHorizontalDimension2(at[0], at[1], at[2], &created);
		case sld_mcp::v1::DIMENSION_KIND_VERTICAL:
			return doc->AddVerticalDimension2(at[0], at[1], at[2], &created);
		case sld_mcp::v1::DIMENSION_KIND_DIAMETER:
			return doc->AddDiameterDimension2(at[0], at[1], at[2], &created);
		case sld_mcp::v1::DIMENSION_KIND_RADIUS:
			return doc->AddRadialDimension2(at[0], at[1], at[2], &created);
		default:
			// Smart dimension: length, distance and angle follow from what's
			// selected and where the text goes.
			return doc->AddDimension2(at[0], at[1], at[2], &created);
		}
	}
}
