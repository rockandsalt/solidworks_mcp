// ConfigurationHelpers.h : The configurations of a part or assembly: their names and
// parents, checking the ones a request names, and a dimension's value in each.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	struct ConfigurationList
	{
		std::vector<std::string> names;    // in the document's order
		std::vector<std::string> parents;  // per name: a derived one's parent, else empty
		std::string active;

		// The document's spelling of `name` (SolidWorks ignores case), or empty.
		std::string Find(const std::string& name) const;
		std::string Quoted() const;  // "Default", "Long"
	};

	ConfigurationList ReadConfigurations(IModelDoc2* doc);

	// The configurations `scope` covers, in the document's order: none for
	// UNSPECIFIED (the caller decides what that means). NAMED names are
	// checked first, since SolidWorks ignores one it doesn't have without a
	// word (SetSystemValue3, SetSuppression2; seen 2026-11-02).
	grpc::Status ScopeConfigurations(const ConfigurationList& configs, sld_mcp::v1::ConfigurationScope scope,
		const google::protobuf::RepeatedPtrField<std::string>& named, std::vector<std::string>& out);

	ATL::CComVariant ToBstrArray(const std::vector<std::string>& names);

	// A dimension's value in each of `configs`, in their order; empty if
	// SolidWorks gives none. GetSystemValue3 answers 0 for a configuration
	// without a value of its own: a derived one then has its parent's, any
	// other the one value all share, the active one's (seen 2026-11-02).
	std::vector<double> DimensionValues(IDimension* dim, const ConfigurationList& configs);
}
