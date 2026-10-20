// ReferenceHelpers.h : Reference geometry features (planes, axes, points, coordinate systems):
// where they are in model space.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	// Whether a feature type (IFeature::GetTypeName2) is reference geometry.
	bool IsReferenceType(const std::string& type);

	// Where a reference plane / axis / point / coordinate system of `doc`
	// itself is, read from SolidWorks. False (and `out` partly set at most)
	// if `type` isn't one or SolidWorks won't say.
	bool DescribeReferenceGeometry(IModelDocExtension* ext, IFeature* feature, const std::string& type,
		sld_mcp::v1::ReferenceGeometry* out);
}
