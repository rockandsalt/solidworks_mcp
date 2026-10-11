// SolidWorksService.cpp : RPC implementations, each a thin wrapper over the
// SolidWorks API.
#include "pch.h"
#include "SolidWorksService.h"
#include "ComVariant.h"
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <map>
#include <set>
#include <string>
#include <vector>

namespace
{
	// Joins the calling gRPC worker thread to the MTA for the duration of one
	// RPC. COM objects (including the proxy from the GIT) must be released
	// before this goes out of scope, so declare it first in each handler.
	class ComApartment
	{
	public:
		ComApartment() : m_hr(CoInitializeEx(nullptr, COINIT_MULTITHREADED)) {}
		~ComApartment()
		{
			if (SUCCEEDED(m_hr))
				CoUninitialize();
		}
		ComApartment(const ComApartment&) = delete;
		ComApartment& operator=(const ComApartment&) = delete;

		// RPC_E_CHANGED_MODE: the thread is already in an STA, which works too.
		bool Ok() const { return SUCCEEDED(m_hr) || m_hr == RPC_E_CHANGED_MODE; }

	private:
		HRESULT m_hr;
	};

	grpc::Status ComError(const char* what, HRESULT hr)
	{
		char message[128];
		std::snprintf(message, sizeof(message), "%s failed (HRESULT 0x%08lX)",
			what, static_cast<unsigned long>(hr));
		return grpc::Status(grpc::StatusCode::INTERNAL, message);
	}

	grpc::Status ComApartmentError()
	{
		return grpc::Status(grpc::StatusCode::INTERNAL, "CoInitializeEx failed on gRPC worker thread");
	}

	// protobuf strings are UTF-8; SolidWorks hands back UTF-16 BSTRs.
	std::string ToUtf8(const ATL::CComBSTR& s)
	{
		const int wideLen = static_cast<int>(s.Length());
		if (wideLen == 0)
			return {};
		const int len = WideCharToMultiByte(CP_UTF8, 0, s.m_str, wideLen, nullptr, 0, nullptr, nullptr);
		std::string out(static_cast<size_t>(len), '\0');
		WideCharToMultiByte(CP_UTF8, 0, s.m_str, wideLen, out.data(), len, nullptr, nullptr);
		return out;
	}

	ATL::CComBSTR FromUtf8(const std::string& s)
	{
		if (s.empty())
			return ATL::CComBSTR(L"");
		const int srcLen = static_cast<int>(s.size());
		const int len = MultiByteToWideChar(CP_UTF8, 0, s.data(), srcLen, nullptr, 0);
		ATL::CComBSTR out(len);
		MultiByteToWideChar(CP_UTF8, 0, s.data(), srcLen, out.m_str, len);
		return out;
	}

	// The active document, if it's a part.
	grpc::Status GetActivePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		HRESULT hr = swApp->get_IActiveDoc2(&doc);
		if (FAILED(hr))
			return ComError("ISldWorks::IActiveDoc2", hr);
		if (!doc)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"no document is open in SolidWorks -- create or open a part first");

		long docType = 0;
		hr = doc->GetType(&docType);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetType", hr);
		if (docType != swDocPART)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active document isn't a part -- only parts are supported for now");
		return grpc::Status::OK;
	}

	// The active document, if it's a part with no sketch open for editing --
	// the state every model-building RPC needs to start from.
	grpc::Status GetEditablePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		if (grpc::Status status = GetActivePart(swApp, doc); !status.ok())
			return status;

		// InsertSketch toggles, and features can't be added while a sketch is
		// being edited, so insist on a closed one.
		ATL::CComPtr<ISketch> openSketch;
		HRESULT hr = doc->IGetActiveSketch2(&openSketch);
		if (FAILED(hr))
			return ComError("IModelDoc2::IGetActiveSketch2", hr);
		if (openSketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"a sketch is already open for editing in SolidWorks -- exit it first");
		return grpc::Status::OK;
	}

	// The last 2D sketch in the feature tree that no feature uses yet, i.e.
	// what a user means by "extrude this sketch". Sketches a feature has
	// absorbed have that feature as a child, so they're skipped.
	grpc::Status FindLatestUnusedSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		sketch.Release();
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"ProfileFeature")
			{
				long children = 0;
				hr = feature->IGetChildCount(&children);
				if (FAILED(hr))
					return ComError("IFeature::IGetChildCount", hr);
				if (children == 0)
					sketch = feature;
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		if (!sketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active part has no sketch that isn't already used by a feature -- "
				"create a sketch first");
		return grpc::Status::OK;
	}

	// Finds the `index`-th reference plane (0 = Front, 1 = Top, 2 = Right) by
	// walking the feature tree. Every part template starts with those three
	// as its first RefPlane features, so this works even when the planes are
	// renamed or SolidWorks runs in another language (SelectByID2 with
	// "Front Plane" would not).
	grpc::Status FindStandardPlane(IModelDoc2* doc, int index, ATL::CComPtr<IFeature>& plane)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		int planesSeen = 0;
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"RefPlane" && planesSeen++ == index)
			{
				plane = feature;
				return grpc::Status::OK;
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"couldn't find the part's default Front/Top/Right planes");
	}

	// ---- Persist IDs --------------------------------------------------------

	// Hands out persist IDs (IModelDocExtension::GetPersistReference3) for
	// entities of one document, caching them by COM identity: the same face
	// shows up once per adjacent edge, and each lookup is a COM round trip.
	// Holds COM references, so it must not outlive the RPC's ComApartment.
	class PersistIds
	{
	public:
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
			if (disp && SUCCEEDED(m_ext->GetPersistReference3(disp, &id)))
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

	grpc::Status GetExtension(IModelDoc2* doc, ATL::CComPtr<IModelDocExtension>& ext)
	{
		HRESULT hr = doc->get_Extension(&ext);
		if (FAILED(hr))
			return ComError("IModelDoc2::Extension", hr);
		if (!ext)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no model extension");
		return grpc::Status::OK;
	}

	// Looks up the entity a persist ID refers to in the active part.
	grpc::Status ResolvePersistId(IModelDocExtension* ext, const std::string& persistId,
		ATL::CComPtr<IDispatch>& obj)
	{
		ATL::CComVariant id = comvariant::FromBytes(persistId);
		long state = swPersistReferencedObject_Ok;
		HRESULT hr = ext->GetObjectByPersistReference3(id, &state, &obj);
		if (FAILED(hr))
			return ComError("IModelDocExtension::GetObjectByPersistReference3", hr);
		if (obj && state == swPersistReferencedObject_Ok)
			return grpc::Status::OK;

		obj.Release();
		const char* why = "doesn't match anything in the active part (was it from another document?)";
		if (state & swPersistReferencedObject_Deleted)
			why = "refers to geometry that no longer exists";
		else if (state & swPersistReferencedObject_Suppressed)
			why = "refers to a suppressed feature";
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			std::string("the ID ") + why + " -- inspect the model again for current IDs");
	}

	// ---- Geometry helpers -----------------------------------------------------

	void SetVec3(sld_mcp::v1::Vec3* out, const double* p)
	{
		out->set_x(p[0]);
		out->set_y(p[1]);
		out->set_z(p[2]);
	}

	// SolidWorks boxes are [xmin, ymin, zmin, xmax, ymax, zmax].
	void SetBox(sld_mcp::v1::Box3* out, const std::vector<double>& box)
	{
		if (box.size() < 6)
			return;
		SetVec3(out->mutable_min(), &box[0]);
		SetVec3(out->mutable_max(), &box[3]);
	}

	std::string FeatureName(IFeature* feature)
	{
		ATL::CComBSTR name;
		if (feature)
			feature->get_Name(&name);
		return ToUtf8(name);
	}

	// Names of the features in a VARIANT array (GetParents / GetChildren).
	void AddFeatureNames(const VARIANT& features, google::protobuf::RepeatedPtrField<std::string>* out)
	{
		for (const auto& disp : comvariant::ToDispatches(features))
		{
			ATL::CComQIPtr<IFeature> feature(disp.p);
			if (feature)
				*out->Add() = FeatureName(feature);
		}
	}

	sld_mcp::v1::SurfaceType ToSurfaceType(long identity)
	{
		switch (identity)
		{
		case PLANE_TYPE:    return sld_mcp::v1::SURFACE_TYPE_PLANE;
		case CYLINDER_TYPE: return sld_mcp::v1::SURFACE_TYPE_CYLINDER;
		case CONE_TYPE:     return sld_mcp::v1::SURFACE_TYPE_CONE;
		case SPHERE_TYPE:   return sld_mcp::v1::SURFACE_TYPE_SPHERE;
		case TORUS_TYPE:    return sld_mcp::v1::SURFACE_TYPE_TORUS;
		case BSURF_TYPE:    return sld_mcp::v1::SURFACE_TYPE_BSPLINE;
		case BLEND_TYPE:    return sld_mcp::v1::SURFACE_TYPE_BLEND;
		case OFFSET_TYPE:   return sld_mcp::v1::SURFACE_TYPE_OFFSET;
		case EXTRU_TYPE:    return sld_mcp::v1::SURFACE_TYPE_EXTRUSION;
		case SREV_TYPE:     return sld_mcp::v1::SURFACE_TYPE_REVOLUTION;
		default:            return sld_mcp::v1::SURFACE_TYPE_UNSPECIFIED;
		}
	}

	sld_mcp::v1::CurveType ToCurveType(long identity)
	{
		switch (identity)
		{
		case LINE_TYPE:         return sld_mcp::v1::CURVE_TYPE_LINE;
		case CIRCLE_TYPE:       return sld_mcp::v1::CURVE_TYPE_CIRCLE;
		case ELLIPSE_TYPE:      return sld_mcp::v1::CURVE_TYPE_ELLIPSE;
		case INTERSECTION_TYPE: return sld_mcp::v1::CURVE_TYPE_INTERSECTION;
		case BCURVE_TYPE:       return sld_mcp::v1::CURVE_TYPE_BSPLINE;
		case SPCURVE_TYPE:      return sld_mcp::v1::CURVE_TYPE_SPCURVE;
		case CONSTPARAM_TYPE:   return sld_mcp::v1::CURVE_TYPE_CONSTPARAM;
		case TRIMMED_TYPE:      return sld_mcp::v1::CURVE_TYPE_TRIMMED;
		default:                return sld_mcp::v1::CURVE_TYPE_UNSPECIFIED;
		}
	}

	// ---- InspectModel ---------------------------------------------------------
	//
	// Entity details are best-effort: a property SolidWorks won't report is
	// left unset rather than failing the whole inspection.

	// Faces and edges past this many are dropped (and `truncated` set), so a
	// huge part can't stall SolidWorks' UI thread for minutes.
	constexpr size_t kMaxInspectedEntities = 2000;

	// Every feature from the first reference plane on. The ones before it are
	// the tree's fixed folders (History, Sensors, Annotations, Material, ...),
	// which say nothing about the geometry.
	grpc::Status InspectFeatures(IModelDoc2* doc, PersistIds& ids, sld_mcp::v1::InspectModelResponse* response)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		bool started = false;
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"RefPlane")
				started = true;

			if (started)
			{
				sld_mcp::v1::FeatureInfo* info = response->add_features();
				info->set_persist_id(ids.Get(feature));
				info->set_name(FeatureName(feature));
				info->set_type_name(ToUtf8(typeName));

				VARIANT_BOOL suppressed = VARIANT_FALSE;
				if (SUCCEEDED(feature->IsSuppressed(&suppressed)))
					info->set_suppressed(suppressed != VARIANT_FALSE);

				ATL::CComVariant parents;
				if (SUCCEEDED(feature->GetParents(&parents)))
					AddFeatureNames(parents, info->mutable_parents());
				ATL::CComVariant children;
				if (SUCCEEDED(feature->GetChildren(&children)))
					AddFeatureNames(children, info->mutable_children());
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		return grpc::Status::OK;
	}

	void InspectBody(IBody2* body, PersistIds& ids, sld_mcp::v1::BodyInfo* info)
	{
		info->set_persist_id(ids.Get(body));

		ATL::CComBSTR name;
		if (SUCCEEDED(body->get_Name(&name)))
			info->set_name(ToUtf8(name));

		long type = swSolidBody;
		if (SUCCEEDED(body->GetType(&type)))
			info->set_solid(type == swSolidBody);

		ATL::CComVariant box;
		if (SUCCEEDED(body->GetBodyBox(&box)))
			SetBox(info->mutable_box(), comvariant::ToDoubles(box));

		// [CoM x, y, z, volume, surface area, mass, ...]; density 1 since
		// only the geometric values are reported.
		ATL::CComVariant mass;
		if (SUCCEEDED(body->GetMassProperties(1.0, &mass)))
		{
			const std::vector<double> m = comvariant::ToDoubles(mass);
			if (m.size() >= 5)
			{
				sld_mcp::v1::MassProperties* props = info->mutable_mass();
				SetVec3(props->mutable_center_of_mass(), &m[0]);
				props->set_volume(m[3]);
				props->set_surface_area(m[4]);
			}
		}

		long count = 0;
		if (SUCCEEDED(body->GetFaceCount(&count)))
			info->set_face_count(static_cast<int32_t>(count));
		if (SUCCEEDED(body->GetEdgeCount(&count)))
			info->set_edge_count(static_cast<int32_t>(count));
	}

	void InspectFace(IFace2* face, PersistIds& ids, sld_mcp::v1::FaceInfo* info)
	{
		info->set_persist_id(ids.Get(face));

		ATL::CComPtr<IDispatch> body;
		if (SUCCEEDED(face->GetBody(&body)))
			info->set_body_persist_id(ids.Get(body));

		ATL::CComPtr<IFeature> feature;
		if (SUCCEEDED(face->IGetFeature(&feature)) && feature)
			info->set_feature_name(FeatureName(feature));

		double area = 0.0;
		if (SUCCEEDED(face->GetArea(&area)))
			info->set_area(area);

		ATL::CComVariant box;
		if (SUCCEEDED(face->GetBox(&box)))
			SetBox(info->mutable_box(), comvariant::ToDoubles(box));

		long edgeCount = 0;
		if (SUCCEEDED(face->GetEdgeCount(&edgeCount)))
			info->set_edge_count(static_cast<int32_t>(edgeCount));

		VARIANT_BOOL sameSense = VARIANT_TRUE;
		if (SUCCEEDED(face->FaceInSurfaceSense(&sameSense)))
			info->set_reversed(sameSense == VARIANT_FALSE);

		ATL::CComPtr<ISurface> surface;
		if (FAILED(face->IGetSurface(&surface)) || !surface)
			return;
		long identity = 0;
		if (FAILED(surface->Identity(&identity)))
			return;
		info->set_surface_type(ToSurfaceType(identity));

		// A point on a curved face and which way is "out of the material"
		// there -- what tells a hole from a boss. Surface normals can't be
		// used for this (SolidWorks orients them either way), but the shading
		// normals must point outward. GetTessTriangles gives 3 vertices of 3
		// coordinates per triangle (meters, with NoConversion); GetTessNorms
		// gives one normal per vertex, in the same order.
		if (identity != PLANE_TYPE)
		{
			ATL::CComVariant triangles, norms;
			if (SUCCEEDED(face->GetTessTriangles(VARIANT_TRUE, &triangles)) &&
				SUCCEEDED(face->GetTessNorms(&norms)))
			{
				const std::vector<double> t = comvariant::ToDoubles(triangles);
				std::vector<double> n = comvariant::ToDoubles(norms);
				if (t.size() >= 3 && n.size() >= 3)
				{
					const double len = std::sqrt(n[0] * n[0] + n[1] * n[1] + n[2] * n[2]);
					if (std::isfinite(len) && len > 0.0)
					{
						for (int i = 0; i < 3; ++i)
							n[i] /= len;
						SetVec3(info->mutable_sample_point(), t.data());
						SetVec3(info->mutable_sample_normal(), n.data());
					}
				}
			}
		}

		ATL::CComVariant params;
		std::vector<double> p;
		switch (identity)
		{
		case PLANE_TYPE:
		{
			// IFace2::Normal accounts for the face's sense, so it points out
			// of the material; PlaneParams is [normal, root point].
			ATL::CComVariant normal;
			if (SUCCEEDED(face->get_Normal(&normal)))
			{
				const std::vector<double> n = comvariant::ToDoubles(normal);
				if (n.size() >= 3)
					SetVec3(info->mutable_normal(), n.data());
			}
			if (SUCCEEDED(surface->get_PlaneParams(&params)) && (p = comvariant::ToDoubles(params)).size() >= 6)
				SetVec3(info->mutable_origin(), &p[3]);
			break;
		}
		case CYLINDER_TYPE: // [origin, axis, radius]
			if (SUCCEEDED(surface->get_CylinderParams(&params)) && (p = comvariant::ToDoubles(params)).size() >= 7)
			{
				SetVec3(info->mutable_origin(), &p[0]);
				SetVec3(info->mutable_axis(), &p[3]);
				info->set_radius(p[6]);
			}
			break;
		case CONE_TYPE: // [origin, axis, radius, half angle]
			if (SUCCEEDED(surface->get_ConeParams(&params)) && (p = comvariant::ToDoubles(params)).size() >= 8)
			{
				SetVec3(info->mutable_origin(), &p[0]);
				SetVec3(info->mutable_axis(), &p[3]);
				info->set_radius(p[6]);
				info->set_half_angle(p[7]);
			}
			break;
		case SPHERE_TYPE: // [center, radius]
			if (SUCCEEDED(surface->get_SphereParams(&params)) && (p = comvariant::ToDoubles(params)).size() >= 4)
			{
				SetVec3(info->mutable_origin(), &p[0]);
				info->set_radius(p[3]);
			}
			break;
		case TORUS_TYPE: // [center, axis, major radius, minor radius]
			if (SUCCEEDED(surface->get_TorusParams(&params)) && (p = comvariant::ToDoubles(params)).size() >= 8)
			{
				SetVec3(info->mutable_origin(), &p[0]);
				SetVec3(info->mutable_axis(), &p[3]);
				info->set_radius(p[6]);
				info->set_minor_radius(p[7]);
			}
			break;
		}
	}

	void InspectEdge(IEdge* edge, PersistIds& ids, sld_mcp::v1::EdgeInfo* info)
	{
		info->set_persist_id(ids.Get(edge));

		ATL::CComPtr<IBody2> body;
		if (SUCCEEDED(edge->GetBody(&body)))
			info->set_body_persist_id(ids.Get(body));

		ATL::CComVariant faces;
		if (SUCCEEDED(edge->GetTwoAdjacentFaces2(&faces)))
			for (const auto& face : comvariant::ToDispatches(faces))
				if (face)
					info->add_face_persist_ids(ids.Get(face));

		// [start point, end point, start param, end param, ...]
		double t0 = 0.0, t1 = 0.0;
		bool haveParams = false;
		ATL::CComVariant curveParams;
		if (SUCCEEDED(edge->GetCurveParams2(&curveParams)))
		{
			const std::vector<double> c = comvariant::ToDoubles(curveParams);
			if (c.size() >= 8)
			{
				SetVec3(info->mutable_start(), &c[0]);
				SetVec3(info->mutable_end(), &c[3]);
				t0 = c[6];
				t1 = c[7];
				haveParams = true;
			}
		}

		ATL::CComPtr<ICurve> curve;
		if (FAILED(edge->IGetCurve(&curve)) || !curve)
			return;
		long identity = 0;
		if (SUCCEEDED(curve->Identity(&identity)))
			info->set_curve_type(ToCurveType(identity));

		double length = 0.0;
		if (haveParams && SUCCEEDED(curve->GetLength3(std::min(t0, t1), std::max(t0, t1), &length)))
			info->set_length(length);

		if (identity == CIRCLE_TYPE) // [center, axis, radius]
		{
			ATL::CComVariant params;
			std::vector<double> p;
			if (SUCCEEDED(curve->get_CircleParams(&params)) && (p = comvariant::ToDoubles(params)).size() >= 7)
			{
				SetVec3(info->mutable_center(), &p[0]);
				SetVec3(info->mutable_axis(), &p[3]);
				info->set_radius(p[6]);
			}
		}
	}

	// Collects distinct faces / edges (by COM identity), up to the cap.
	template <typename T>
	class EntitySet
	{
	public:
		// Adds each element of a VARIANT array that is a T. Returns false
		// once the cap is reached.
		bool AddAll(const VARIANT& array)
		{
			for (const auto& disp : comvariant::ToDispatches(array))
				if (!Add(disp))
					return false;
			return true;
		}

		bool Add(IUnknown* obj)
		{
			ATL::CComQIPtr<T> entity(obj);
			ATL::CComPtr<IUnknown> identity;
			if (!entity || FAILED(entity->QueryInterface(IID_PPV_ARGS(&identity))))
				return true;
			if (!m_seen.insert(identity.p).second)
				return true;
			if (m_items.size() >= kMaxInspectedEntities)
			{
				m_truncated = true;
				return false;
			}
			m_alive.push_back(identity);
			m_items.push_back(entity);
			return true;
		}

		const std::vector<ATL::CComPtr<T>>& Items() const { return m_items; }
		bool Truncated() const { return m_truncated; }

	private:
		std::vector<ATL::CComPtr<T>> m_items;
		std::set<IUnknown*> m_seen;
		std::vector<ATL::CComPtr<IUnknown>> m_alive;
		bool m_truncated = false;
	};
}

SolidWorksService::SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie)
	: m_git(git), m_swAppCookie(swAppCookie)
{
}

grpc::Status SolidWorksService::GetSwApp(ATL::CComPtr<ISldWorks>& swApp) const
{
	HRESULT hr = m_git->GetInterfaceFromGlobal(m_swAppCookie, __uuidof(ISldWorks),
		reinterpret_cast<void**>(&swApp));
	if (FAILED(hr))
		return grpc::Status(grpc::StatusCode::UNAVAILABLE, "lost connection to SolidWorks");
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::GetActiveDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::GetActiveDocumentRequest* /*request*/,
	sld_mcp::v1::GetActiveDocumentResponse* response)
{
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = swApp->get_IActiveDoc2(&doc);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	if (!doc)
		return grpc::Status::OK; // nothing open: leave `document` unset

	ATL::CComBSTR title;
	hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);

	response->mutable_document()->set_title(ToUtf8(title));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::NewPart(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::NewPartRequest* /*request*/,
	sld_mcp::v1::NewPartResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	// Tools > Options > System Options > Default Templates > Parts
	ATL::CComBSTR templatePath;
	HRESULT hr = swApp->GetUserPreferenceStringValue(swDefaultTemplatePart, &templatePath);
	if (FAILED(hr))
		return ComError("ISldWorks::GetUserPreferenceStringValue", hr);
	if (templatePath.Length() == 0)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks has no default part template set -- set one under Tools > Options > "
			"System Options > Default Templates");

	// Paper size and dimensions only apply to drawings.
	ATL::CComPtr<IDispatch> created;
	hr = swApp->NewDocument(templatePath, 0, 0.0, 0.0, &created);
	if (FAILED(hr))
		return ComError("ISldWorks::NewDocument", hr);
	ATL::CComQIPtr<IModelDoc2> doc(created.p);
	if (!doc)
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"SolidWorks couldn't create a part from the default template " + ToUtf8(templatePath));

	ATL::CComBSTR title;
	hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);

	response->mutable_document()->set_title(ToUtf8(title));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::CreateRectangleSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CreateRectangleSketchRequest* request,
	sld_mcp::v1::CreateRectangleSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool onEntity = !request->on_persist_id().empty();
	int planeIndex = 0;
	if (!onEntity)
	{
		switch (request->plane())
		{
		case sld_mcp::v1::PLANE_FRONT: planeIndex = 0; break;
		case sld_mcp::v1::PLANE_TOP:   planeIndex = 1; break;
		case sld_mcp::v1::PLANE_RIGHT: planeIndex = 2; break;
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
		}
	}
	const double halfWidth = request->width() / 2.0;
	const double halfHeight = request->height() / 2.0;
	if (!(std::isfinite(halfWidth) && halfWidth > 0.0 && std::isfinite(halfHeight) && halfHeight > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "width and height must be positive");

	// The center is either 2D sketch coordinates or a model-space point that
	// gets projected onto the sketch plane once the sketch exists.
	double cx = request->center_x();
	double cy = request->center_y();
	bool haveModelCenter = request->has_center_model();
	double modelCenter[3] = {};
	if (haveModelCenter)
	{
		modelCenter[0] = request->center_model().x();
		modelCenter[1] = request->center_model().y();
		modelCenter[2] = request->center_model().z();
		if (!(std::isfinite(modelCenter[0]) && std::isfinite(modelCenter[1]) && std::isfinite(modelCenter[2])))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "center must be finite");
	}
	else if (!(std::isfinite(cx) && std::isfinite(cy)))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "center must be finite");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
		return status;

	// What to sketch on: a planar face, a reference plane feature, or one of
	// the three default planes.
	ATL::CComPtr<IEntity> faceEntity;
	ATL::CComPtr<IFeature> plane;
	HRESULT hr = S_OK;
	if (onEntity)
	{
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComPtr<IDispatch> target;
		if (grpc::Status status = ResolvePersistId(ext, request->on_persist_id(), target); !status.ok())
			return status;

		if (ATL::CComQIPtr<IFace2> face(target.p); face)
		{
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL planar = VARIANT_FALSE;
			if (FAILED(face->IGetSurface(&surface)) || !surface ||
				FAILED(surface->IsPlane(&planar)) || !planar)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"that face isn't planar -- sketches need a planar face or a reference plane");
			if (FAILED(face->QueryInterface(IID_PPV_ARGS(&faceEntity))) || !faceEntity)
				return grpc::Status(grpc::StatusCode::INTERNAL, "face has no IEntity interface");

			// Default to the middle of the face (well, of its bounding box).
			if (!haveModelCenter)
			{
				ATL::CComVariant box;
				hr = face->GetBox(&box);
				if (FAILED(hr))
					return ComError("IFace2::GetBox", hr);
				const std::vector<double> b = comvariant::ToDoubles(box);
				if (b.size() < 6)
					return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no box for the face");
				for (int i = 0; i < 3; ++i)
					modelCenter[i] = (b[i] + b[i + 3]) / 2.0;
				haveModelCenter = true;
			}
		}
		else if (ATL::CComQIPtr<IFeature> feature(target.p); feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName != L"RefPlane")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + FeatureName(feature) + "\" isn't a reference plane -- sketches need a "
					"planar face or a reference plane");
			plane = feature;
		}
		else
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that ID isn't a face or a reference plane -- sketches need a planar face or a reference plane");
	}
	else if (grpc::Status status = FindStandardPlane(doc, planeIndex, plane); !status.ok())
		return status;

	// Points are mapped between model and sketch space with IMathPoints.
	ATL::CComPtr<IDispatch> mathDisp;
	hr = swApp->GetMathUtility(&mathDisp);
	if (FAILED(hr))
		return ComError("ISldWorks::GetMathUtility", hr);
	ATL::CComQIPtr<IMathUtility> math(mathDisp.p);
	if (!math)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no math utility");

	ATL::CComPtr<ISketchManager> sketchManager;
	hr = doc->get_SketchManager(&sketchManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::SketchManager", hr);
	if (!sketchManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");

	hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	VARIANT_BOOL selected = VARIANT_FALSE;
	hr = faceEntity ? faceEntity->Select4(VARIANT_FALSE, nullptr, &selected)
	                : plane->Select2(VARIANT_FALSE, 0, &selected);
	if (FAILED(hr))
		return ComError(faceEntity ? "IEntity::Select4" : "IFeature::Select2", hr);
	if (!selected)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch plane");

	// Opens a new sketch on the selected plane or face.
	hr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("ISketchManager::InsertSketch", hr);
	ATL::CComPtr<ISketch> sketch;
	hr = doc->IGetActiveSketch2(&sketch);
	if (FAILED(hr) || !sketch)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't open a sketch on the plane");
	}

	// The sketch is now open: everything below must fall through to the
	// InsertSketch call that closes it, so no early returns until then.
	grpc::Status failure = grpc::Status::OK;

	// The sketch's placement in model space, from its model-to-sketch
	// transform: the inverse maps sketch (0,0,0), (1,0,0), ... to model space.
	ATL::CComPtr<IMathTransform> toSketch;
	ATL::CComPtr<IMathTransform> toModel;
	if (SUCCEEDED(sketch->get_ModelToSketchTransform(&toSketch)) && toSketch)
	{
		ATL::CComPtr<IDispatch> inverse;
		if (SUCCEEDED(toSketch->Inverse(&inverse)) && inverse)
			inverse->QueryInterface(IID_PPV_ARGS(&toModel));
	}
	auto transform = [&](IMathTransform* xform, const double in[3], double out[3]) {
		if (!xform)
			return false;
		ATL::CComVariant data = comvariant::FromDoubles(in, 3);
		ATL::CComPtr<IDispatch> pointDisp;
		if (FAILED(math->CreatePoint(data, &pointDisp)))
			return false;
		ATL::CComQIPtr<IMathPoint> point(pointDisp.p);
		ATL::CComPtr<IDispatch> movedDisp;
		if (!point || FAILED(point->MultiplyTransform(xform, &movedDisp)))
			return false;
		ATL::CComQIPtr<IMathPoint> moved(movedDisp.p);
		ATL::CComVariant result;
		if (!moved || FAILED(moved->get_ArrayData(&result)))
			return false;
		const std::vector<double> p = comvariant::ToDoubles(result);
		if (p.size() < 3)
			return false;
		std::copy(p.begin(), p.begin() + 3, out);
		return true;
	};

	if (haveModelCenter)
	{
		// Dropping the sketch Z (the distance from the plane) projects the
		// point onto the sketch plane.
		double sketchCenter[3];
		if (transform(toSketch, modelCenter, sketchCenter))
		{
			cx = sketchCenter[0];
			cy = sketchCenter[1];
		}
		else
			failure = grpc::Status(grpc::StatusCode::INTERNAL,
				"couldn't map the center point into the sketch's coordinates");
	}

	// AddToDB skips snapping and inferencing, so the rectangle lands exactly
	// at the requested coordinates instead of snapping to nearby geometry.
	VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
	sketchManager->get_AddToDB(&previousAddToDB);
	sketchManager->put_AddToDB(VARIANT_TRUE);

	// Center point, then one corner; sketch coordinates in meters. Returns
	// the created segments, or nothing if SolidWorks rejected the geometry.
	ATL::CComVariant segments;
	HRESULT createHr = S_OK;
	bool created = false;
	if (failure.ok())
	{
		createHr = sketchManager->CreateCenterRectangle(
			cx, cy, 0.0, cx + halfWidth, cy + halfHeight, 0.0, &segments);
		created = SUCCEEDED(createHr) && segments.vt != VT_EMPTY && segments.vt != VT_NULL;
		if (FAILED(createHr))
			failure = ComError("ISketchManager::CreateCenterRectangle", createHr);
		else if (!created)
			failure = grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the rectangle");
	}

	sketchManager->put_AddToDB(previousAddToDB);

	// Report the frame and where the center actually landed. Purely
	// informational, so a failure here just leaves them out.
	if (created)
	{
		const double sketchPoints[5][3] = {
			{0, 0, 0}, {1, 0, 0}, {0, 1, 0}, {0, 0, 1}, {cx, cy, 0}};
		double modelPoints[5][3];
		bool mapped = true;
		for (int i = 0; i < 5 && mapped; ++i)
			mapped = transform(toModel, sketchPoints[i], modelPoints[i]);
		if (mapped)
		{
			sld_mcp::v1::SketchFrame* frame = response->mutable_frame();
			SetVec3(frame->mutable_origin(), modelPoints[0]);
			sld_mcp::v1::Vec3* axes[3] = {
				frame->mutable_x_axis(), frame->mutable_y_axis(), frame->mutable_normal()};
			for (int a = 0; a < 3; ++a)
			{
				double d[3];
				for (int i = 0; i < 3; ++i)
					d[i] = modelPoints[a + 1][i] - modelPoints[0][i];
				const double len = std::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
				if (len > 0.0)
					for (double& v : d)
						v /= len;
				SetVec3(axes[a], d);
			}
			SetVec3(response->mutable_center_model(), modelPoints[4]);
		}
	}

	// An ISketch is also its IFeature in the tree; read the name while it's
	// still the active sketch.
	ATL::CComQIPtr<IFeature> sketchFeature(sketch.p);
	ATL::CComBSTR sketchName;
	if (sketchFeature)
		sketchFeature->get_Name(&sketchName);

	// Close the sketch (and rebuild).
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	doc->ClearSelection2(VARIANT_TRUE);

	if (!created)
	{
		// Don't leave an empty sketch behind.
		VARIANT_BOOL reselected = VARIANT_FALSE;
		if (SUCCEEDED(closeHr) && sketchFeature &&
			SUCCEEDED(sketchFeature->Select2(VARIANT_FALSE, 0, &reselected)) && reselected)
		{
			doc->EditDelete();
			doc->ClearSelection2(VARIANT_TRUE);
		}
		return failure;
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	response->set_sketch_name(ToUtf8(sketchName));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::ExtrudeSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::ExtrudeSketchRequest* request,
	sld_mcp::v1::ExtrudeSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool cut = request->cut();
	const bool throughAll = request->through_all();
	if (throughAll && !cut)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "through_all only applies to cuts");
	const double depth = throughAll ? 0.0 : request->depth();
	if (!throughAll && !(std::isfinite(depth) && depth > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "depth must be positive");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
		return status;

	// Resolve the sketch: by name if given, else the latest unused one.
	ATL::CComPtr<IFeature> sketch;
	HRESULT hr = S_OK;
	if (request->sketch_name().empty())
	{
		if (grpc::Status status = FindLatestUnusedSketch(doc, sketch); !status.ok())
			return status;
	}
	else
	{
		ATL::CComQIPtr<IPartDoc> part(doc.p);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		hr = part->IFeatureByName(FromUtf8(request->sketch_name()), &sketch);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!sketch)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active part has no feature named \"" + request->sketch_name() + "\"");

		ATL::CComBSTR typeName;
		hr = sketch->GetTypeName2(&typeName);
		if (FAILED(hr))
			return ComError("IFeature::GetTypeName2", hr);
		if (typeName != L"ProfileFeature")
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + request->sketch_name() + "\" isn't a 2D sketch");
	}

	ATL::CComBSTR sketchName;
	hr = sketch->get_Name(&sketchName);
	if (FAILED(hr))
		return ComError("IFeature::Name", hr);

	ATL::CComPtr<IFeatureManager> featureManager;
	hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	// FeatureExtrusion3 / FeatureCut4 extrude whatever profile is selected.
	hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	VARIANT_BOOL selected = VARIANT_FALSE;
	hr = sketch->Select2(VARIANT_FALSE, 0, &selected);
	if (FAILED(hr))
		return ComError("IFeature::Select2", hr);
	if (!selected)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch");

	// Mid-plane takes the total depth in D1 and splits it across both sides;
	// it has no direction, so `reverse_direction` doesn't apply. Through-all
	// overrides both.
	const bool midPlane = !throughAll && request->mid_plane();
	const VARIANT_BOOL reverse = (!midPlane && request->reverse_direction()) ? VARIANT_TRUE : VARIANT_FALSE;
	const long endCondition = throughAll ? swEndCondThroughAll : midPlane ? swEndCondMidPlane : swEndCondBlind;

	ATL::CComPtr<IFeature> extrusion;
	if (cut)
		hr = featureManager->FeatureCut4(
			VARIANT_TRUE,       // Sd: one direction only
			VARIANT_FALSE,      // Flip: keep the material outside the profile
			reverse,            // Dir: reverse direction 1
			endCondition,       // T1
			swEndCondBlind,     // T2 (unused, single direction)
			depth,              // D1
			0.0,                // D2
			VARIANT_FALSE,      // Dchk1: no draft
			VARIANT_FALSE,      // Dchk2
			VARIANT_FALSE,      // Ddir1
			VARIANT_FALSE,      // Ddir2
			0.0,                // Dang1
			0.0,                // Dang2
			VARIANT_FALSE,      // OffsetReverse1
			VARIANT_FALSE,      // OffsetReverse2
			VARIANT_FALSE,      // TranslateSurface1
			VARIANT_FALSE,      // TranslateSurface2
			VARIANT_FALSE,      // NormalCut (sheet metal only)
			VARIANT_TRUE,       // UseFeatScope
			VARIANT_TRUE,       // UseAutoSelect: all bodies in scope
			VARIANT_FALSE,      // AssemblyFeatureScope (assemblies only)
			VARIANT_FALSE,      // AutoSelectComponents
			VARIANT_FALSE,      // PropagateFeatureToParts
			swStartSketchPlane, // T0: start at the sketch plane
			0.0,                // StartOffset
			VARIANT_FALSE,      // FlipStartOffset
			VARIANT_FALSE,      // OptimizeGeometry
			&extrusion);
	else
		hr = featureManager->FeatureExtrusion3(
			VARIANT_TRUE,       // Sd: one direction only
			VARIANT_FALSE,      // Flip: side to cut (cuts only)
			reverse,            // Dir: reverse direction 1
			endCondition,       // T1
			swEndCondBlind,     // T2 (unused, single direction)
			depth,              // D1
			0.0,                // D2
			VARIANT_FALSE,      // Dchk1: no draft
			VARIANT_FALSE,      // Dchk2
			VARIANT_FALSE,      // Ddir1
			VARIANT_FALSE,      // Ddir2
			0.0,                // Dang1
			0.0,                // Dang2
			VARIANT_FALSE,      // OffsetReverse1
			VARIANT_FALSE,      // OffsetReverse2
			VARIANT_FALSE,      // TranslateSurface1
			VARIANT_FALSE,      // TranslateSurface2
			VARIANT_TRUE,       // Merge with existing bodies
			VARIANT_TRUE,       // UseFeatScope
			VARIANT_TRUE,       // UseAutoSelect: all bodies in scope
			swStartSketchPlane, // T0: start at the sketch plane
			0.0,                // StartOffset
			VARIANT_FALSE,      // FlipStartOffset
			&extrusion);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError(cut ? "IFeatureManager::FeatureCut4" : "IFeatureManager::FeatureExtrusion3", hr);
	if (!extrusion)
	{
		if (cut)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't cut with " + ToUtf8(sketchName) +
				" -- the sketch must contain a closed, non-self-intersecting profile, and the cut "
				"must pass through material (if the sketch is on a face, try reversing the direction)");
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't extrude " + ToUtf8(sketchName) +
			" -- the sketch must contain a closed, non-self-intersecting profile");
	}

	ATL::CComBSTR featureName;
	hr = extrusion->get_Name(&featureName);
	if (FAILED(hr))
		return ComError("IFeature::Name", hr);

	response->set_feature_name(ToUtf8(featureName));
	response->set_sketch_name(ToUtf8(sketchName));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::InspectModel(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectModelRequest* request,
	sld_mcp::v1::InspectModelResponse* response)
{
	if (!request->scope_persist_id().empty() && !request->scope_feature_name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give either a scope ID or a scope feature name, not both");
	const bool wantGeometry = request->include_faces() || request->include_edges();

	// Read-only, but an edit running at the same time could leave the model
	// half-built (a sketch open, a feature not yet created) mid-walk.
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetActivePart(swApp, doc); !status.ok())
		return status;
	ATL::CComQIPtr<IPartDoc> part(doc.p);
	if (!part)
		return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	PersistIds ids(ext);

	ATL::CComBSTR title;
	HRESULT hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);
	response->mutable_document()->set_title(ToUtf8(title));

	if (request->include_features())
		if (grpc::Status status = InspectFeatures(doc, ids, response); !status.ok())
			return status;

	// Every body, for the body list and as the default face/edge scope.
	std::vector<ATL::CComPtr<IBody2>> bodies;
	if (request->include_bodies() ||
		(wantGeometry && request->scope_persist_id().empty() && request->scope_feature_name().empty()))
	{
		ATL::CComVariant all;
		hr = part->GetBodies2(swAllBodies, VARIANT_FALSE, &all);
		if (FAILED(hr))
			return ComError("IPartDoc::GetBodies2", hr);
		for (const auto& disp : comvariant::ToDispatches(all))
			if (ATL::CComQIPtr<IBody2> body(disp.p); body)
				bodies.push_back(body);
	}
	if (request->include_bodies())
		for (const auto& body : bodies)
			InspectBody(body, ids, response->add_bodies());

	if (!wantGeometry)
		return grpc::Status::OK;

	// Resolve the scope into the faces and edges to report.
	EntitySet<IFace2> faces;
	EntitySet<IEdge> edges;
	auto addBody = [&](IBody2* body) {
		ATL::CComVariant array;
		if (request->include_faces() && SUCCEEDED(body->GetFaces(&array)))
			faces.AddAll(array);
		array.Clear();
		if (request->include_edges() && SUCCEEDED(body->GetEdges(&array)))
			edges.AddAll(array);
	};
	// A feature's edges are the edges of the faces it created.
	auto addFeature = [&](IFeature* feature) {
		ATL::CComVariant array;
		if (FAILED(feature->GetFaces(&array)))
			return;
		for (const auto& disp : comvariant::ToDispatches(array))
		{
			ATL::CComQIPtr<IFace2> face(disp.p);
			if (!face)
				continue;
			if (request->include_faces())
				faces.Add(face);
			ATL::CComVariant faceEdges;
			if (request->include_edges() && SUCCEEDED(face->GetEdges(&faceEdges)))
				edges.AddAll(faceEdges);
		}
	};

	if (!request->scope_feature_name().empty())
	{
		ATL::CComPtr<IFeature> feature;
		hr = part->IFeatureByName(FromUtf8(request->scope_feature_name()), &feature);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!feature)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active part has no feature named \"" + request->scope_feature_name() + "\"");
		addFeature(feature);
	}
	else if (!request->scope_persist_id().empty())
	{
		ATL::CComPtr<IDispatch> scope;
		if (grpc::Status status = ResolvePersistId(ext, request->scope_persist_id(), scope); !status.ok())
			return status;
		if (ATL::CComQIPtr<IBody2> body(scope.p); body)
			addBody(body);
		else if (ATL::CComQIPtr<IFeature> feature(scope.p); feature)
			addFeature(feature);
		else if (ATL::CComQIPtr<IFace2> face(scope.p); face)
		{
			if (request->include_faces())
				faces.Add(face);
			ATL::CComVariant faceEdges;
			if (request->include_edges() && SUCCEEDED(face->GetEdges(&faceEdges)))
				edges.AddAll(faceEdges);
		}
		else
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the scope must be a body, a feature or a face");
	}
	else
	{
		for (const auto& body : bodies)
			addBody(body);
	}

	for (const auto& face : faces.Items())
		InspectFace(face, ids, response->add_faces());
	for (const auto& edge : edges.Items())
		InspectEdge(edge, ids, response->add_edges());
	response->set_truncated(faces.Truncated() || edges.Truncated());
	return grpc::Status::OK;
}
