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

	// A GeometryRef resolved to what gets selected.
	struct Pick
	{
		enum Kind { Entity, Feature, SketchSegment, SketchPoint, ByName } kind = Entity;
		ATL::CComPtr<IDispatch> obj;
		// ByName: SelectByID2 name and type (the origin's point when
		// SolidWorks won't hand out the object).
		std::string selectName;
		// For messages: "face", "sketch line", "Plane1", "the origin", ...
		std::string what;
	};

	// Resolves a reference: a face, edge, vertex or reference feature by
	// persist id, a sketch segment or point, the origin, a default plane, or a
	// reference feature by name. Other features are refused.
	grpc::Status ResolvePick(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref,
		Pick& out);

	// Adds a pick to the selection with `mark` (the role SolidWorks reads it in).
	bool SelectPick(IModelDocExtension* ext, ISelectData* data, const Pick& pick, long mark);

	// The references of one request, resolved, with the mark each is
	// selected with.
	struct Selection
	{
		std::vector<Pick> picks;
		std::vector<long> marks;

		// Resolves `ref`; `label` ("reference 2") prefixes errors.
		grpc::Status Add(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref,
			long mark, const std::string& label);

		// "face, sketch line of Sketch2" for messages.
		std::string Describe() const;

		// Clears the selection, then selects every pick with its mark.
		grpc::Status Select(IModelDoc2* doc, IModelDocExtension* ext) const;
	};
}
