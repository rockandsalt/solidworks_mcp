// ConfigurationHelpers.cpp : see ConfigurationHelpers.h.
#include "pch.h"
#include "ConfigurationHelpers.h"

namespace sld_mcp::service
{
	std::string ConfigurationList::Find(const std::string& name) const
	{
		for (const std::string& n : names)
			if (_stricmp(n.c_str(), name.c_str()) == 0)
				return n;
		return {};
	}

	std::string ConfigurationList::Quoted() const
	{
		std::string out;
		for (const std::string& n : names)
			out += (out.empty() ? "\"" : ", \"") + n + "\"";
		return out;
	}

	ConfigurationList ReadConfigurations(IModelDoc2* doc)
	{
		ConfigurationList out;
		ATL::CComVariant namesVar;
		if (SUCCEEDED(doc->GetConfigurationNames(&namesVar)))
			for (const ATL::CComBSTR& name : comvariant::ToBstrs(namesVar))
				out.names.push_back(ToUtf8(name));
		for (const std::string& name : out.names)
		{
			std::string parent;
			ATL::CComPtr<IDispatch> disp;
			if (SUCCEEDED(doc->GetConfigurationByName(FromUtf8(name), &disp)) && disp)
				if (ATL::CComQIPtr<IConfiguration> config(disp.p); config)
				{
					ATL::CComPtr<IConfiguration> parentConfig;
					ATL::CComBSTR parentName;
					if (SUCCEEDED(config->GetParent(&parentConfig)) && parentConfig &&
						SUCCEEDED(parentConfig->get_Name(&parentName)))
						parent = ToUtf8(parentName);
				}
			out.parents.push_back(parent);
		}
		ATL::CComPtr<IDispatch> activeDisp;
		ATL::CComBSTR activeName;
		if (SUCCEEDED(doc->GetActiveConfiguration(&activeDisp)) && activeDisp)
			if (ATL::CComQIPtr<IConfiguration> active(activeDisp.p); active && SUCCEEDED(active->get_Name(&activeName)))
				out.active = ToUtf8(activeName);
		return out;
	}

	grpc::Status ScopeConfigurations(const ConfigurationList& configs, sld_mcp::v1::ConfigurationScope scope,
		const google::protobuf::RepeatedPtrField<std::string>& named, std::vector<std::string>& out)
	{
		out.clear();
		switch (scope)
		{
		case sld_mcp::v1::CONFIGURATION_SCOPE_UNSPECIFIED:
			return grpc::Status::OK;
		case sld_mcp::v1::CONFIGURATION_SCOPE_ACTIVE:
			out.push_back(configs.active);
			return grpc::Status::OK;
		case sld_mcp::v1::CONFIGURATION_SCOPE_ALL:
			out = configs.names;
			return grpc::Status::OK;
		case sld_mcp::v1::CONFIGURATION_SCOPE_NAMED:
			break;
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown configuration scope");
		}
		if (named.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "name at least one configuration");
		std::set<std::string> wanted;
		for (const std::string& name : named)
		{
			const std::string found = configs.Find(name);
			if (found.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the document has no configuration \"" + name + "\" -- it has " + configs.Quoted());
			wanted.insert(found);
		}
		for (const std::string& n : configs.names)
			if (wanted.count(n))
				out.push_back(n);
		return grpc::Status::OK;
	}

	ATL::CComVariant ToBstrArray(const std::vector<std::string>& names)
	{
		std::vector<ATL::CComBSTR> bstrs;
		for (const std::string& n : names)
			bstrs.push_back(FromUtf8(n));
		return comvariant::FromBstrs(bstrs);
	}

	std::vector<double> DimensionValues(IDimension* dim, const ConfigurationList& configs)
	{
		ATL::CComVariant valuesVar;
		if (FAILED(dim->GetSystemValue3(swSpecifyConfiguration, ToBstrArray(configs.names), &valuesVar)))
			return {};
		const std::vector<double> own = comvariant::ToDoubles(valuesVar);
		if (own.size() != configs.names.size())
			return {};
		double shared = 0.0;
		dim->get_SystemValue(&shared);
		std::vector<double> out;
		for (size_t i = 0; i < own.size(); ++i)
		{
			// Up the parents to the first value of its own (a few levels at most).
			size_t at = i;
			for (int depth = 0; own[at] == 0.0 && depth < 16; ++depth)
			{
				const auto parent = std::find(configs.names.begin(), configs.names.end(), configs.parents[at]);
				if (configs.parents[at].empty() || parent == configs.names.end())
					break;
				at = static_cast<size_t>(parent - configs.names.begin());
			}
			out.push_back(own[at] != 0.0 ? own[at] : shared);
		}
		return out;
	}
}
