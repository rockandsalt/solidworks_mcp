// Common.h : COM plumbing shared by every RPC: error statuses,
// UTF-8 conversion, the active-document getters, persist ids and small
// geometry helpers.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "SolidWorksService.h"
#include "ComVariant.h"
#include <algorithm>
#include <array>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <limits>
#include <map>
#include <memory>
#include <optional>
#include <set>
#include <string>
#include <vector>

// Eigen from vcpkg (manifest dependency "eigen3"), which installs its
// headers under include/eigen3.
#include <eigen3/Eigen/Geometry>

namespace sld_mcp::service
{
	grpc::Status ComError(const char* what, HRESULT hr);

	// protobuf strings are UTF-8; SolidWorks hands back UTF-16 BSTRs.
	std::string ToUtf8(const ATL::CComBSTR& s);

	ATL::CComBSTR FromUtf8(const std::string& s);

	// The active document, if it's a part or an assembly; `docType` is its
	// swDocumentTypes_e.
	grpc::Status GetActiveModel(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc, long& docType);

	// The active document, if it's a part.
	grpc::Status GetActivePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc);

	// FAILED_PRECONDITION if a sketch is open for editing.
	grpc::Status CheckNoOpenSketch(IModelDoc2* doc);

	// The active document, if it's a part with no sketch open for editing --
	// the state every model-building RPC needs to start from.
	grpc::Status GetEditablePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc);

	// The active document, if it's an assembly that is itself being edited:
	// no sketch open, and no component being edited in context (mates and
	// other assembly features can't be added then).
	grpc::Status GetEditableAssembly(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc);

	// Hands out persist IDs (IModelDocExtension::GetPersistReference3) for
	// entities of one document, caching them by COM identity: the same face
	// shows up once per adjacent edge, and each lookup is a COM round trip.
	// Holds COM references, so it must stay inside the executor's work callback.
	class PersistIds
	{
	public:
		// A null `ext` (e.g. a drawing view whose model isn't loaded) hands
		// out empty IDs.
		explicit PersistIds(IModelDocExtension* ext) : m_ext(ext) {}

		// Empty if `obj` is null or SolidWorks can't reference it.
		std::string Get(IUnknown* obj)
		{
			if (!obj)
				return {};
			ATL::CComPtr<IUnknown> identity;
			if (FAILED(obj->QueryInterface(IID_PPV_ARGS(&identity))) || !identity)
				return {};
			if (auto it = m_cache.find(identity.p); it != m_cache.end())
				return it->second;

			std::string bytes;
			ATL::CComQIPtr<IDispatch> disp(obj);
			ATL::CComVariant id;
			if (disp && m_ext && SUCCEEDED(m_ext->GetPersistReference3(disp, &id)))
				bytes = comvariant::ToBytes(id);
			// Keep the object alive so its address can't be reused by another
			// entity while it's a cache key.
			m_alive.push_back(identity);
			m_cache.emplace(identity.p, bytes);
			return bytes;
		}

	private:
		ATL::CComPtr<IModelDocExtension> m_ext;
		std::map<IUnknown*, std::string> m_cache;
		std::vector<ATL::CComPtr<IUnknown>> m_alive;
	};

	grpc::Status GetExtension(IModelDoc2* doc, ATL::CComPtr<IModelDocExtension>& ext);

	// Looks up the entity a persist ID refers to in the active document.
	grpc::Status ResolvePersistId(IModelDocExtension* ext, const std::string& persistId,
		ATL::CComPtr<IDispatch>& obj);

	void SetVec3(sld_mcp::v1::Vec3* out, const double* p);

	void SetVec3(sld_mcp::v1::Vec3* out, const Eigen::Vector3d& p);

	// A SolidWorks box ([xmin, ymin, zmin, xmax, ymax, zmax]); an empty box
	// if SolidWorks returned something else.
	Eigen::AlignedBox3d ToAlignedBox(const std::vector<double>& b);

	// SolidWorks boxes are [xmin, ymin, zmin, xmax, ymax, zmax].
	void SetBox(sld_mcp::v1::Box3* out, const std::vector<double>& box);

	std::string FeatureName(IFeature* feature);

	sld_mcp::v1::CurveType ToCurveType(long identity);

	// Applies a SolidWorks transform to a point with an IMathPoint, so the
	// transform's internal layout never matters.
	bool TransformPoint(IMathUtility* math, IMathTransform* xform, const Eigen::Vector3d& in, Eigen::Vector3d& out);

	grpc::Status GetMathUtility(ISldWorks* swApp, ATL::CComQIPtr<IMathUtility>& math);

	bool IsFinite(const sld_mcp::v1::Vec3& p);

	// SolidWorks angles are in radians; messages report degrees.
	inline double Degrees(double radians) { return radians * 180.0 / M_PI; }

	void InspectEdge(IEdge* edge, PersistIds& ids, sld_mcp::v1::EdgeInfo* info);

	sld_mcp::v1::DocumentType ToDocumentType(long type);

	bool SameObject(IUnknown* a, IUnknown* b);
}
