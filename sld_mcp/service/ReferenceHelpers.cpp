// ReferenceHelpers.cpp : see ReferenceHelpers.h.
#include "pch.h"
#include "ReferenceHelpers.h"

namespace sld_mcp::service
{
	namespace
	{
		// The origin and unit axes of a transform's frame, in model space.
		// Read from ArrayData ([X axis, Y axis, Z axis, origin, scale, ...]) in
		// one call: mapping four IMathPoints through it (as TransformPoint
		// does) took ~0.2 s per plane, which made listing a part's features
		// several times slower. The layout was checked against planes and
		// coordinate systems of known orientation (devloop/scenarios/reference.py).
		bool FrameOf(IMathTransform* xform, Eigen::Vector3d& origin, Eigen::Vector3d axes[3])
		{
			ATL::CComVariant data;
			std::vector<double> m;
			if (FAILED(xform->get_ArrayData(&data)) || (m = comvariant::ToDoubles(data)).size() < 12)
				return false;
			for (int a = 0; a < 3; ++a)
				axes[a] = Eigen::Vector3d(m[3 * a], m[3 * a + 1], m[3 * a + 2]).normalized();
			origin = Eigen::Vector3d(m[9], m[10], m[11]);
			return true;
		}
	}

	bool IsReferenceType(const std::string& type)
	{
		return type == "RefPlane" || type == "RefAxis" || type == "RefPoint" || type == "CoordSys";
	}

	bool DescribeReferenceGeometry(IModelDocExtension* ext, IFeature* feature, const std::string& type,
		sld_mcp::v1::ReferenceGeometry* out)
	{
		if (type == "CoordSys")
		{
			ATL::CComPtr<IMathTransform> xform;
			Eigen::Vector3d origin, axes[3];
			if (!ext || FAILED(ext->GetCoordinateSystemTransformByName(FromUtf8(FeatureName(feature)), &xform)) ||
				!xform || !FrameOf(xform, origin, axes))
				return false;
			SetVec3(out->mutable_origin(), origin);
			SetVec3(out->mutable_x_axis(), axes[0]);
			SetVec3(out->mutable_y_axis(), axes[1]);
			SetVec3(out->mutable_z_axis(), axes[2]);
			return true;
		}

		ATL::CComPtr<IDispatch> specific;
		if (FAILED(feature->GetSpecificFeature2(&specific)) || !specific)
			return false;
		if (type == "RefPlane")
		{
			ATL::CComQIPtr<IRefPlane> plane(specific.p);
			ATL::CComPtr<IMathTransform> xform;
			Eigen::Vector3d origin, axes[3];
			// The plane is the XY plane of its transform's frame.
			if (!plane || FAILED(plane->get_Transform(&xform)) || !xform || !FrameOf(xform, origin, axes))
				return false;
			SetVec3(out->mutable_origin(), origin);
			SetVec3(out->mutable_direction(), axes[2]);
			SetVec3(out->mutable_x_axis(), axes[0]);
			SetVec3(out->mutable_y_axis(), axes[1]);
			return true;
		}
		if (type == "RefAxis")
		{
			ATL::CComQIPtr<IRefAxis> axis(specific.p);
			ATL::CComVariant params;
			std::vector<double> p;  // [start xyz, end xyz]
			if (!axis || FAILED(axis->GetRefAxisParams(&params)) || (p = comvariant::ToDoubles(params)).size() < 6)
				return false;
			const Eigen::Vector3d start(p[0], p[1], p[2]), end(p[3], p[4], p[5]);
			SetVec3(out->mutable_origin(), start);
			SetVec3(out->mutable_direction(), (end - start).normalized());
			out->set_length((end - start).norm());
			return true;
		}
		if (type == "RefPoint")
		{
			ATL::CComQIPtr<IRefPoint> point(specific.p);
			ATL::CComPtr<IMathPoint> at;
			ATL::CComVariant data;
			std::vector<double> p;
			if (!point || FAILED(point->GetRefPoint(&at)) || !at || FAILED(at->get_ArrayData(&data)) ||
				(p = comvariant::ToDoubles(data)).size() < 3)
				return false;
			SetVec3(out->mutable_origin(), &p[0]);
			return true;
		}
		return false;
	}
}
