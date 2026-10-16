// SolidWorksService.cpp : RPC implementations, each a thin wrapper over the
// SolidWorks API.
#include "pch.h"
#include "SolidWorksService.h"
#include "ComVariant.h"
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <map>
#include <optional>
#include <set>
#include <string>
#include <vector>

// Eigen from vcpkg (manifest dependency "eigen3"), which installs its
// headers under include/eigen3.
#include <eigen3/Eigen/Geometry>

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

	// The active document, if it's a part or an assembly; `docType` is its
	// swDocumentTypes_e.
	grpc::Status GetActiveModel(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc, long& docType)
	{
		HRESULT hr = swApp->get_IActiveDoc2(&doc);
		if (FAILED(hr))
			return ComError("ISldWorks::IActiveDoc2", hr);
		if (!doc)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"no document is open in SolidWorks -- create or open a part first");

		docType = swDocNONE;
		hr = doc->GetType(&docType);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetType", hr);
		if (docType != swDocPART && docType != swDocASSEMBLY)
		{
			doc.Release();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active document isn't a part or an assembly");
		}
		return grpc::Status::OK;
	}

	// The active document, if it's a part.
	grpc::Status GetActivePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		if (docType != swDocPART)
		{
			doc.Release();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active document is an assembly -- this only works on parts (open the part "
				"itself to edit it)");
		}
		return grpc::Status::OK;
	}

	// FAILED_PRECONDITION if a sketch is open for editing.
	grpc::Status CheckNoOpenSketch(IModelDoc2* doc)
	{
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

	// The active document, if it's a part with no sketch open for editing --
	// the state every model-building RPC needs to start from.
	grpc::Status GetEditablePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		if (grpc::Status status = GetActivePart(swApp, doc); !status.ok())
			return status;
		return CheckNoOpenSketch(doc);
	}

	// The active part or assembly, with no sketch open for editing.
	grpc::Status GetEditableModel(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		return CheckNoOpenSketch(doc);
	}

	// The active document, if it's an assembly that is itself being edited:
	// no sketch open, and no component being edited in context (mates and
	// other assembly features can't be added then).
	grpc::Status GetEditableAssembly(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		if (docType != swDocASSEMBLY)
		{
			doc.Release();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active document isn't an assembly -- open (or switch to) the assembly first");
		}
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return status;

		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IDispatch> target;
		HRESULT hr = assembly->GetEditTarget(&target);
		if (FAILED(hr))
			return ComError("IAssemblyDoc::GetEditTarget", hr);
		if (target)
		{
			ATL::CComPtr<IUnknown> targetIdentity, docIdentity;
			target->QueryInterface(IID_PPV_ARGS(&targetIdentity));
			doc->QueryInterface(IID_PPV_ARGS(&docIdentity));
			if (targetIdentity != docIdentity)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"a component is being edited in the context of the assembly -- return to editing "
					"the assembly first");
		}
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

	// Looks up the entity a persist ID refers to in the active document.
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
		const char* why = "doesn't match anything in the active document (was it from another document?)";
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

	void SetVec3(sld_mcp::v1::Vec3* out, const Eigen::Vector3d& p)
	{
		SetVec3(out, p.data());
	}

	// A SolidWorks box ([xmin, ymin, zmin, xmax, ymax, zmax]); an empty box
	// if SolidWorks returned something else.
	Eigen::AlignedBox3d ToAlignedBox(const std::vector<double>& b)
	{
		if (b.size() < 6)
			return Eigen::AlignedBox3d();
		return Eigen::AlignedBox3d(Eigen::Vector3d(b[0], b[1], b[2]), Eigen::Vector3d(b[3], b[4], b[5]));
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

	// ---- Sketch helpers -------------------------------------------------------

	// PLANE_FRONT/TOP/RIGHT as FindStandardPlane's index.
	bool StandardPlaneIndex(sld_mcp::v1::Plane plane, int& index)
	{
		switch (plane)
		{
		case sld_mcp::v1::PLANE_FRONT: index = 0; return true;
		case sld_mcp::v1::PLANE_TOP:   index = 1; return true;
		case sld_mcp::v1::PLANE_RIGHT: index = 2; return true;
		default:                       return false;
		}
	}

	// What a new sketch goes on: a planar face (`face` + `faceEntity`) or a
	// reference plane feature (`plane`); exactly one is set once resolved.
	struct SketchTarget
	{
		ATL::CComPtr<IFace2> face;
		ATL::CComPtr<IEntity> faceEntity;
		ATL::CComPtr<IFeature> plane;
	};

	// Resolves `onPersistId` (a planar face or a reference plane feature), or
	// the standard plane `planeIndex` when it's empty.
	grpc::Status ResolveSketchTarget(IModelDoc2* doc, const std::string& onPersistId, int planeIndex,
		SketchTarget& target)
	{
		if (onPersistId.empty())
			return FindStandardPlane(doc, planeIndex, target.plane);

		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, onPersistId, obj); !status.ok())
			return status;

		if (ATL::CComQIPtr<IFace2> face(obj.p); face)
		{
			ATL::CComPtr<ISurface> surface;
			VARIANT_BOOL planar = VARIANT_FALSE;
			if (FAILED(face->IGetSurface(&surface)) || !surface ||
				FAILED(surface->IsPlane(&planar)) || !planar)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"that face isn't planar -- sketches need a planar face or a reference plane");
			if (FAILED(face->QueryInterface(IID_PPV_ARGS(&target.faceEntity))) || !target.faceEntity)
				return grpc::Status(grpc::StatusCode::INTERNAL, "face has no IEntity interface");
			target.face = face;
			return grpc::Status::OK;
		}
		if (ATL::CComQIPtr<IFeature> feature(obj.p); feature)
		{
			ATL::CComBSTR typeName;
			HRESULT hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName != L"RefPlane")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + FeatureName(feature) + "\" isn't a reference plane -- sketches need a "
					"planar face or a reference plane");
			target.plane = feature;
			return grpc::Status::OK;
		}
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"that ID isn't a face or a reference plane -- sketches need a planar face or a reference plane");
	}

	// Selects the target and opens a new sketch on it. On failure nothing is
	// left open.
	grpc::Status OpenNewSketch(IModelDoc2* doc, ISketchManager* sketchManager, const SketchTarget& target,
		ATL::CComPtr<ISketch>& sketch)
	{
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		VARIANT_BOOL selected = VARIANT_FALSE;
		hr = target.faceEntity ? target.faceEntity->Select4(VARIANT_FALSE, nullptr, &selected)
		                       : target.plane->Select2(VARIANT_FALSE, 0, &selected);
		if (FAILED(hr))
			return ComError(target.faceEntity ? "IEntity::Select4" : "IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch plane");

		// Opens a new sketch on the selected plane or face.
		hr = sketchManager->InsertSketch(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("ISketchManager::InsertSketch", hr);
		hr = doc->IGetActiveSketch2(&sketch);
		if (FAILED(hr) || !sketch)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't open a sketch on the plane");
		}
		return grpc::Status::OK;
	}

	// A 2D sketch by its feature-tree name.
	grpc::Status FindSketchByName(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& sketch)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		HRESULT hr = part->IFeatureByName(FromUtf8(name), &sketch);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!sketch)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active part has no feature named \"" + name + "\"");

		ATL::CComBSTR typeName;
		hr = sketch->GetTypeName2(&typeName);
		if (FAILED(hr))
			return ComError("IFeature::GetTypeName2", hr);
		if (typeName != L"ProfileFeature")
		{
			sketch.Release();
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + name + "\" isn't a 2D sketch");
		}
		return grpc::Status::OK;
	}

	// Deletes a (closed) sketch feature, e.g. one left empty by a failed
	// RPC. Best effort.
	void DeleteSketchFeature(IModelDoc2* doc, IFeature* sketchFeature)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (sketchFeature && SUCCEEDED(sketchFeature->Select2(VARIANT_FALSE, 0, &selected)) && selected)
		{
			doc->EditDelete();
			doc->ClearSelection2(VARIANT_TRUE);
		}
	}

	// Applies a SolidWorks transform to a point with an IMathPoint, so the
	// transform's internal layout never matters.
	bool TransformPoint(IMathUtility* math, IMathTransform* xform, const Eigen::Vector3d& in, Eigen::Vector3d& out)
	{
		if (!xform || !math)
			return false;
		ATL::CComVariant data = comvariant::FromDoubles(in.data(), 3);
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
		out = Eigen::Vector3d(p[0], p[1], p[2]);
		return true;
	}

	grpc::Status GetMathUtility(ISldWorks* swApp, ATL::CComQIPtr<IMathUtility>& math)
	{
		ATL::CComPtr<IDispatch> mathDisp;
		HRESULT hr = swApp->GetMathUtility(&mathDisp);
		if (FAILED(hr))
			return ComError("ISldWorks::GetMathUtility", hr);
		math = mathDisp;
		if (!math)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no math utility");
		return grpc::Status::OK;
	}

	// Converts points between model space and an open sketch's own space
	// (x, y in the sketch plane; z = distance from it) with IMathPoints.
	class SketchSpace
	{
	public:
		// Call before opening the sketch, so a failure here needs no cleanup.
		grpc::Status Init(ISldWorks* swApp)
		{
			return GetMathUtility(swApp, m_math);
		}

		// Once the sketch is open. If SolidWorks has no transform for it,
		// the conversions below just fail.
		void Attach(ISketch* sketch)
		{
			m_toSketch.Release();
			m_toModel.Release();
			if (SUCCEEDED(sketch->get_ModelToSketchTransform(&m_toSketch)) && m_toSketch)
			{
				ATL::CComPtr<IDispatch> inverse;
				if (SUCCEEDED(m_toSketch->Inverse(&inverse)) && inverse)
					inverse->QueryInterface(IID_PPV_ARGS(&m_toModel));
			}
		}

		bool ToSketch(const double in[3], double out[3]) const { return Apply(m_toSketch, in, out); }
		bool ToModel(const double in[3], double out[3]) const { return Apply(m_toModel, in, out); }

		// The sketch's origin and unit axes in model space.
		bool Frame(sld_mcp::v1::SketchFrame& frame) const
		{
			const double sketchPoints[4][3] = {{0, 0, 0}, {1, 0, 0}, {0, 1, 0}, {0, 0, 1}};
			double modelPoints[4][3];
			for (int i = 0; i < 4; ++i)
				if (!ToModel(sketchPoints[i], modelPoints[i]))
					return false;

			SetVec3(frame.mutable_origin(), modelPoints[0]);
			sld_mcp::v1::Vec3* axes[3] = {frame.mutable_x_axis(), frame.mutable_y_axis(), frame.mutable_normal()};
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
			return true;
		}

	private:
		bool Apply(IMathTransform* xform, const double in[3], double out[3]) const
		{
			Eigen::Vector3d moved;
			if (!TransformPoint(m_math, xform, Eigen::Vector3d(in[0], in[1], in[2]), moved))
				return false;
			Eigen::Map<Eigen::Vector3d> result(out);
			result = moved;
			return true;
		}

		ATL::CComQIPtr<IMathUtility> m_math;
		ATL::CComPtr<IMathTransform> m_toSketch;
		ATL::CComPtr<IMathTransform> m_toModel;
	};

	// ---- AddSketchEntities ----------------------------------------------------

	// Requests with more entities (or spline points) than this are refused.
	constexpr int kMaxSketchEntities = 1000;
	constexpr int kMaxSplinePoints = 1000;
	// Points closer than this (meters) count as the same point.
	constexpr double kSketchTolerance = 1e-9;
	// End points closer than this (meters) are merged after drawing, so
	// chains of entities connect into contours.
	constexpr double kMergeDistance = 1e-8;

	bool IsFinite(const sld_mcp::v1::Vec3& p)
	{
		return std::isfinite(p.x()) && std::isfinite(p.y()) && std::isfinite(p.z());
	}

	// Checks an entity's shape before SolidWorks is touched: the right
	// points are present and every number is finite. Geometric degeneracy is
	// checked when drawing, once points are in sketch coordinates.
	grpc::Status ValidateSketchEntity(const sld_mcp::v1::SketchEntity& entity)
	{
		using Entity = sld_mcp::v1::SketchEntity;
		auto invalid = [](const char* why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		auto point = [](bool has, const sld_mcp::v1::Vec3& p) { return has && IsFinite(p); };
		switch (entity.geometry_case())
		{
		case Entity::kLine:
			if (!point(entity.line().has_start(), entity.line().start()) ||
				!point(entity.line().has_end(), entity.line().end()))
				return invalid("a line needs a finite start and end");
			return grpc::Status::OK;
		case Entity::kCircle:
		{
			const double r = entity.circle().radius();
			if (!point(entity.circle().has_center(), entity.circle().center()))
				return invalid("a circle needs a finite center");
			if (!(std::isfinite(r) && r > kSketchTolerance))
				return invalid("a circle's radius must be positive");
			return grpc::Status::OK;
		}
		case Entity::kArc:
		{
			const auto& arc = entity.arc();
			if (!point(arc.has_start(), arc.start()) || !point(arc.has_end(), arc.end()))
				return invalid("an arc needs a finite start and end");
			if (arc.has_center() == arc.has_through())
				return invalid("an arc needs exactly one of center or through");
			if (!IsFinite(arc.has_center() ? arc.center() : arc.through()))
				return invalid("an arc's center/through point must be finite");
			if (arc.clockwise() && !arc.has_center())
				return invalid("clockwise only applies to arcs given by their center");
			return grpc::Status::OK;
		}
		case Entity::kSpline:
		{
			const auto& points = entity.spline().points();
			const int minPoints = entity.spline().closed() ? 3 : 2;
			if (points.size() < minPoints)
				return invalid(entity.spline().closed() ? "a closed spline needs at least 3 points"
				                                        : "a spline needs at least 2 points");
			if (points.size() > kMaxSplinePoints)
				return invalid("a spline can have at most 1000 points");
			for (const auto& p : points)
				if (!IsFinite(p))
					return invalid("spline points must be finite");
			return grpc::Status::OK;
		}
		case Entity::kPoint:
			if (!point(entity.point().has_position(), entity.point().position()))
				return invalid("a point needs a finite position");
			return grpc::Status::OK;
		default:
			return invalid("an entity has no geometry (line, circle, arc, spline or point)");
		}
	}

	// Maps request points into the open sketch's 2D coordinates (z = 0).
	struct SketchPointMapper
	{
		const SketchSpace& space;
		bool modelSpace;

		bool operator()(const sld_mcp::v1::Vec3& p, double out[3]) const
		{
			if (modelSpace)
			{
				const double in[3] = {p.x(), p.y(), p.z()};
				if (!space.ToSketch(in, out))
					return false;
			}
			else
			{
				out[0] = p.x();
				out[1] = p.y();
			}
			// Dropping the sketch z projects the point onto the sketch plane.
			out[2] = 0.0;
			return true;
		}
	};

	double Distance2D(const double a[3], const double b[3])
	{
		return std::hypot(a[0] - b[0], a[1] - b[1]);
	}

	// Draws one (validated) entity into the open sketch and returns what was
	// created: an ISketchSegment, or an ISketchPoint for points.
	grpc::Status DrawSketchEntity(ISketchManager* sketchManager, const SketchPointMapper& map,
		const sld_mcp::v1::SketchEntity& entity, ATL::CComPtr<IDispatch>& created)
	{
		using Entity = sld_mcp::v1::SketchEntity;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const grpc::Status mapFailed(grpc::StatusCode::INTERNAL,
			"couldn't map a point into the sketch's coordinates");

		ATL::CComPtr<ISketchSegment> segment;
		HRESULT hr = S_OK;
		std::string what;
		switch (entity.geometry_case())
		{
		case Entity::kLine:
		{
			what = "line";
			double s[3], e[3];
			if (!map(entity.line().start(), s) || !map(entity.line().end(), e))
				return mapFailed;
			if (Distance2D(s, e) < kSketchTolerance)
				return invalid("a line's start and end are the same point in the sketch plane");
			hr = sketchManager->CreateLine(s[0], s[1], 0.0, e[0], e[1], 0.0, &segment);
			break;
		}
		case Entity::kCircle:
		{
			what = "circle";
			double c[3];
			if (!map(entity.circle().center(), c))
				return mapFailed;
			hr = sketchManager->CreateCircleByRadius(c[0], c[1], 0.0, entity.circle().radius(), &segment);
			break;
		}
		case Entity::kArc:
		{
			what = "arc";
			const auto& arc = entity.arc();
			double s[3], e[3];
			if (!map(arc.start(), s) || !map(arc.end(), e))
				return mapFailed;
			if (arc.has_center())
			{
				double c[3];
				if (!map(arc.center(), c))
					return mapFailed;
				const double radius = Distance2D(s, c);
				const double toEnd = Distance2D(e, c);
				if (radius < kSketchTolerance || toEnd < kSketchTolerance)
					return invalid("an arc's start and end must both be away from its center");
				// The radius comes from the start; the end only gives the angle.
				for (int i = 0; i < 2; ++i)
					e[i] = c[i] + (e[i] - c[i]) * radius / toEnd;
				if (Distance2D(s, e) < kSketchTolerance)
					return invalid("an arc's start and end are at the same angle around its center -- "
					               "draw a circle for a full turn");
				// Direction: +1 counterclockwise, -1 clockwise.
				hr = sketchManager->CreateArc(c[0], c[1], 0.0, s[0], s[1], 0.0, e[0], e[1], 0.0,
					static_cast<short>(arc.clockwise() ? -1 : 1), &segment);
			}
			else
			{
				double t[3];
				if (!map(arc.through(), t))
					return mapFailed;
				const double chord = Distance2D(s, e);
				if (chord < kSketchTolerance)
					return invalid("an arc's start and end are the same point -- draw a circle instead");
				// Twice the triangle's area over its base = the through point's
				// distance from the chord.
				const double cross = (t[0] - s[0]) * (e[1] - s[1]) - (t[1] - s[1]) * (e[0] - s[0]);
				if (std::abs(cross) / chord < kSketchTolerance)
					return invalid("an arc's three points are in a straight line -- draw a line instead");
				// Start, end, then a point on the arc.
				hr = sketchManager->Create3PointArc(s[0], s[1], 0.0, e[0], e[1], 0.0, t[0], t[1], 0.0, &segment);
			}
			break;
		}
		case Entity::kSpline:
		{
			what = "spline";
			const auto& spline = entity.spline();
			std::vector<double> data;
			data.reserve(static_cast<size_t>(spline.points_size() + 1) * 3);
			double first[3] = {};
			double previous[3] = {};
			for (int i = 0; i < spline.points_size(); ++i)
			{
				double p[3];
				if (!map(spline.points(i), p))
					return mapFailed;
				if (i > 0 && Distance2D(p, previous) < kSketchTolerance)
					return invalid("a spline has two consecutive points at the same place in the sketch plane");
				if (i == 0)
					std::copy(p, p + 3, first);
				std::copy(p, p + 3, previous);
				data.insert(data.end(), p, p + 3);
			}
			// A spline whose last point is its first is closed.
			if (spline.closed() && Distance2D(previous, first) >= kSketchTolerance)
				data.insert(data.end(), first, first + 3);
			ATL::CComVariant points = comvariant::FromDoubles(data.data(), data.size());
			hr = sketchManager->CreateSpline(points, &segment);
			break;
		}
		case Entity::kPoint:
		{
			double p[3];
			if (!map(entity.point().position(), p))
				return mapFailed;
			ATL::CComPtr<ISketchPoint> point;
			hr = sketchManager->CreatePoint(p[0], p[1], 0.0, &point);
			if (FAILED(hr))
				return ComError("ISketchManager::CreatePoint", hr);
			if (!point)
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the point");
			created = point.p;
			return grpc::Status::OK;
		}
		default:
			return invalid("an entity has no geometry");
		}

		if (FAILED(hr))
			return ComError(("ISketchManager creating a " + what).c_str(), hr);
		if (!segment)
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks didn't create the " + what + " (degenerate geometry?)");
		if (entity.construction())
			segment->put_ConstructionGeometry(VARIANT_TRUE);
		created = segment.p;
		return grpc::Status::OK;
	}

	// Deletes entities drawn into the still-open sketch. Best effort.
	void DeleteSketchEntities(IModelDoc2* doc, const std::vector<ATL::CComPtr<IDispatch>>& entities)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		bool any = false;
		for (const auto& entity : entities)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (ATL::CComQIPtr<ISketchSegment> segment(entity.p); segment)
				segment->Select4(VARIANT_TRUE, nullptr, &selected);
			else if (ATL::CComQIPtr<ISketchPoint> point(entity.p); point)
				point->Select4(VARIANT_TRUE, nullptr, &selected);
			any = any || selected;
		}
		if (any)
			doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
	}

	// Counts the sketch's closed and open contours.
	bool CountContours(ISketch* sketch, sld_mcp::v1::SketchContours& contours)
	{
		ATL::CComVariant list;
		if (FAILED(sketch->GetSketchContours(&list)))
			return false;
		// VT_EMPTY / VT_NULL: no contours at all (e.g. only points).
		for (const auto& disp : comvariant::ToDispatches(list))
		{
			if (!disp)
				continue;
			ATL::CComQIPtr<ISketchContour> contour(disp.p);
			VARIANT_BOOL closed = VARIANT_FALSE;
			if (!contour || FAILED(contour->IsClosed(&closed)))
				return false;
			if (closed)
				contours.set_closed(contours.closed() + 1);
			else
				contours.set_open(contours.open() + 1);
		}
		return true;
	}

	// ---- InspectModel ---------------------------------------------------------
	//
	// Entity details are best-effort: a property SolidWorks won't report is
	// left unset rather than failing the whole inspection.

	// Faces and edges past this many are dropped (and `truncated` set), so a
	// huge part can't stall SolidWorks' UI thread for minutes.
	constexpr size_t kMaxInspectedEntities = 2000;

	// Every feature from `first` on, starting at the first reference plane.
	// The ones before it are the tree's fixed folders (History, Sensors,
	// Annotations, Material, ...), which say nothing about the geometry.
	// `componentPid` is stamped on each (assembly component trees). With
	// `skipAssemblyItems`, components and the mates folder are left out
	// (an assembly's own tree; they're listed separately).
	grpc::Status InspectFeatureList(ATL::CComPtr<IFeature> feature, PersistIds& ids,
		const std::string& componentPid, bool skipAssemblyItems, sld_mcp::v1::InspectModelResponse* response)
	{
		HRESULT hr = S_OK;
		bool started = false;
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"RefPlane")
				started = true;
			const bool skip = skipAssemblyItems &&
				(typeName == L"Reference" || typeName == L"MateGroup" || typeName == L"MateReferenceGroupFeature");

			if (started && !skip)
			{
				sld_mcp::v1::FeatureInfo* info = response->add_features();
				info->set_persist_id(ids.Get(feature));
				info->set_name(FeatureName(feature));
				info->set_type_name(ToUtf8(typeName));
				info->set_component_persist_id(componentPid);

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

	grpc::Status InspectFeatures(IModelDoc2* doc, PersistIds& ids, bool skipAssemblyItems,
		sld_mcp::v1::InspectModelResponse* response)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);
		return InspectFeatureList(feature, ids, {}, skipAssemblyItems, response);
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
		explicit EntitySet(size_t cap = kMaxInspectedEntities) : m_cap(cap) {}

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
			if (m_items.size() >= m_cap)
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
		size_t m_cap;
		bool m_truncated = false;
	};

	// ---- GetMesh --------------------------------------------------------------

	// Rendering needs every face, so the caps are well above InspectModel's;
	// they only guard against a pathological part stalling the UI thread.
	constexpr size_t kMaxMeshFaces = 20000;
	constexpr size_t kMaxMeshEdges = 40000;

	void AppendFloats(const std::vector<double>& values, google::protobuf::RepeatedField<float>* out)
	{
		out->Reserve(out->size() + static_cast<int>(values.size()));
		for (double v : values)
			out->Add(static_cast<float>(v));
	}

	// The face's display tessellation. False if it has none (nothing to draw).
	bool MeshFace(IFace2* face, sld_mcp::v1::FaceMesh* mesh)
	{
		// 3 vertices x 3 coordinates per triangle, meters with NoConversion.
		ATL::CComVariant triangles;
		if (FAILED(face->GetTessTriangles(VARIANT_TRUE, &triangles)))
			return false;
		std::vector<double> t = comvariant::ToDoubles(triangles);
		t.resize(t.size() - t.size() % 9);
		if (t.empty())
			return false;
		AppendFloats(t, mesh->mutable_triangles());

		// One normal per vertex, same order; dropped if it doesn't line up.
		ATL::CComVariant norms;
		if (SUCCEEDED(face->GetTessNorms(&norms)))
		{
			const std::vector<double> n = comvariant::ToDoubles(norms);
			if (n.size() == t.size())
				AppendFloats(n, mesh->mutable_normals());
		}
		return true;
	}

	// A point on `curve` at parameter `t`. False if SolidWorks won't say.
	bool CurvePoint(ICurve* curve, double t, std::array<double, 3>& point)
	{
		ATL::CComVariant result;
		if (FAILED(curve->Evaluate2(t, 0, &result)))
			return false;
		const std::vector<double> p = comvariant::ToDoubles(result);
		if (p.size() < 3)
			return false;
		std::copy(p.begin(), p.begin() + 3, point.begin());
		return true;
	}

	// The edge as a polyline (x, y, z, ...), meters. Lines are just their
	// two ends; curves use ICurve::GetTessPts between points on the edge.
	// A closed edge (a full circle) starts and ends at the same point, which
	// doesn't say which way round to go, so it's done in thirds.
	bool TessellateEdge(IEdge* edge, double chordTolerance, double lengthTolerance, std::vector<double>& points)
	{
		points.clear();
		// [start point, end point, start param, end param, ...]
		ATL::CComVariant curveParams;
		if (FAILED(edge->GetCurveParams2(&curveParams)))
			return false;
		const std::vector<double> c = comvariant::ToDoubles(curveParams);
		if (c.size() < 8)
			return false;
		const std::array<double, 3> start = { c[0], c[1], c[2] };
		const std::array<double, 3> end = { c[3], c[4], c[5] };
		const double t0 = c[6], t1 = c[7];

		ATL::CComPtr<ICurve> curve;
		long identity = 0;
		if (FAILED(edge->IGetCurve(&curve)) || !curve || FAILED(curve->Identity(&identity)) ||
			identity == LINE_TYPE)
		{
			points.assign(start.begin(), start.end());
			points.insert(points.end(), end.begin(), end.end());
			return true;
		}

		const double gap = std::sqrt((start[0] - end[0]) * (start[0] - end[0]) +
			(start[1] - end[1]) * (start[1] - end[1]) + (start[2] - end[2]) * (start[2] - end[2]));
		const int pieces = gap < 1e-9 ? 3 : 1;
		std::vector<std::array<double, 3>> stops = { start };
		for (int i = 1; i < pieces; ++i)
		{
			std::array<double, 3> p{};
			if (!CurvePoint(curve, t0 + (t1 - t0) * i / pieces, p))
				break;
			stops.push_back(p);
		}
		stops.push_back(end);

		bool ok = stops.size() == static_cast<size_t>(pieces) + 1;
		for (size_t i = 0; ok && i + 1 < stops.size(); ++i)
		{
			ATL::CComVariant from = comvariant::FromDoubles(stops[i].data(), 3);
			ATL::CComVariant to = comvariant::FromDoubles(stops[i + 1].data(), 3);
			ATL::CComVariant tess;
			std::vector<double> segment;
			if (FAILED(curve->GetTessPts(chordTolerance, lengthTolerance, from, to, &tess)) ||
				(segment = comvariant::ToDoubles(tess)).size() < 6)
			{
				ok = false;
				break;
			}
			segment.resize(segment.size() - segment.size() % 3);
			// Each piece starts where the last one ended.
			points.insert(points.end(), segment.begin() + (points.empty() ? 0 : 3), segment.end());
		}
		if (ok && points.size() >= 6)
			return true;

		// Fall back to sampling the parameter range evenly.
		constexpr int kSamples = 32;
		points.clear();
		for (int i = 0; i <= kSamples; ++i)
		{
			std::array<double, 3> p{};
			if (CurvePoint(curve, t0 + (t1 - t0) * i / kSamples, p))
				points.insert(points.end(), p.begin(), p.end());
		}
		if (points.size() >= 6)
			return true;
		points.assign(start.begin(), start.end());
		points.insert(points.end(), end.begin(), end.end());
		return gap > 0.0;
	}

	// ---- Sketch items -----------------------------------------------------------
	//
	// Sketch segments and points are identified by SolidWorks' sketch entity
	// IDs (two numbers, stable for as long as the entity exists) plus the
	// persist ID of the sketch feature they're in.

	// InspectSketch lists at most this many segments + points.
	constexpr size_t kMaxSketchItems = 5000;
	// A SketchState lists at most this many entities that aren't fully defined.
	constexpr int kMaxListedUndefined = 50;
	// Requests with more dimensions or relations than this are refused.
	constexpr int kMaxSketchConstraints = 200;
	// Most refs one dimension or relation can have.
	constexpr int kMaxConstraintRefs = 3;

	// The last 2D sketch in the feature tree, used by a feature or not --
	// "the sketch I just drew".
	grpc::Status FindLatestSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch)
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
				sketch = feature;

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		if (!sketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active part has no 2D sketch -- create one first");
		return grpc::Status::OK;
	}

	// A sketch by name, or the latest one when `name` is empty.
	grpc::Status ResolveSketchFeature(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& sketch)
	{
		return name.empty() ? FindLatestSketch(doc, sketch) : FindSketchByName(doc, name, sketch);
	}

	// The ISketch behind a sketch feature.
	grpc::Status SketchOf(IFeature* feature, ATL::CComPtr<ISketch>& sketch)
	{
		ATL::CComPtr<IDispatch> specific;
		HRESULT hr = feature->GetSpecificFeature2(&specific);
		if (FAILED(hr))
			return ComError("IFeature::GetSpecificFeature2", hr);
		sketch.Release();
		if (specific)
			specific->QueryInterface(IID_PPV_ARGS(&sketch));
		if (!sketch)
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks returned no sketch data for \"" + FeatureName(feature) + "\"");
		return grpc::Status::OK;
	}

	// Re-opens an existing, closed sketch for editing. On failure nothing is
	// left open.
	grpc::Status OpenExistingSketch(IModelDoc2* doc, IFeature* sketchFeature, ATL::CComPtr<ISketch>& sketch)
	{
		const std::string name = FeatureName(sketchFeature);
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		VARIANT_BOOL selected = VARIANT_FALSE;
		hr = sketchFeature->Select2(VARIANT_FALSE, 0, &selected);
		if (FAILED(hr))
			return ComError("IFeature::Select2", hr);
		if (!selected)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select sketch \"" + name + "\"");
		hr = doc->EditSketch();
		if (FAILED(hr))
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return ComError("IModelDoc2::EditSketch", hr);
		}
		hr = doc->IGetActiveSketch2(&sketch);
		if (FAILED(hr) || !sketch)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks didn't open sketch \"" + name + "\" for editing");
		}
		return grpc::Status::OK;
	}

	grpc::Status GetSketchManager(IModelDoc2* doc, ATL::CComPtr<ISketchManager>& sketchManager)
	{
		HRESULT hr = doc->get_SketchManager(&sketchManager);
		if (FAILED(hr))
			return ComError("IModelDoc2::SketchManager", hr);
		if (!sketchManager)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");
		return grpc::Status::OK;
	}

	// The persist ID of a feature (empty if SolidWorks won't give one).
	std::string PersistIdOf(IModelDocExtension* ext, IUnknown* obj)
	{
		ATL::CComQIPtr<IDispatch> disp(obj);
		ATL::CComVariant id;
		if (!disp || FAILED(ext->GetPersistReference3(disp, &id)))
			return {};
		return comvariant::ToBytes(id);
	}

	sld_mcp::v1::DefinitionStatus ToDefinitionStatus(long status)
	{
		switch (status)
		{
		case swUnderConstrained: return sld_mcp::v1::DEFINITION_STATUS_UNDER;
		case swFullyConstrained: return sld_mcp::v1::DEFINITION_STATUS_FULLY;
		case swOverConstrained:  return sld_mcp::v1::DEFINITION_STATUS_OVER;
		case swNoSolution:       return sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION;
		case swInvalidSolution:  return sld_mcp::v1::DEFINITION_STATUS_INVALID;
		case swAutosolveOff:     return sld_mcp::v1::DEFINITION_STATUS_NOT_SOLVED;
		default:                 return sld_mcp::v1::DEFINITION_STATUS_UNSPECIFIED;
		}
	}

	const char* DefinitionStatusText(sld_mcp::v1::DefinitionStatus status)
	{
		switch (status)
		{
		case sld_mcp::v1::DEFINITION_STATUS_UNDER:       return "under defined";
		case sld_mcp::v1::DEFINITION_STATUS_FULLY:       return "fully defined";
		case sld_mcp::v1::DEFINITION_STATUS_OVER:        return "over defined";
		case sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION: return "unsolvable";
		case sld_mcp::v1::DEFINITION_STATUS_INVALID:     return "solved into invalid geometry";
		case sld_mcp::v1::DEFINITION_STATUS_NOT_SOLVED:  return "not solved (automatic solve is off)";
		default:                                         return "of unknown status";
		}
	}

	sld_mcp::v1::DefinitionStatus SketchStatus(ISketch* sketch)
	{
		long status = 0;
		if (FAILED(sketch->GetConstrainedStatus(&status)))
			return sld_mcp::v1::DEFINITION_STATUS_UNSPECIFIED;
		return ToDefinitionStatus(status);
	}

	// The two numbers SolidWorks identifies a sketch segment or point by.
	bool SketchEntityId(IDispatch* entity, bool& isPoint, long long& id0, long long& id1)
	{
		if (!entity)
			return false;
		ATL::CComVariant id;
		if (ATL::CComQIPtr<ISketchSegment> segment(entity); segment)
		{
			isPoint = false;
			if (FAILED(segment->GetID(&id)))
				return false;
		}
		else if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
		{
			isPoint = true;
			if (FAILED(point->GetID(&id)))
				return false;
		}
		else
			return false;
		const std::vector<long long> v = comvariant::ToInt64s(id);
		if (v.size() < 2)
			return false;
		id0 = v[0];
		id1 = v[1];
		return true;
	}

	bool SetItemRef(IDispatch* entity, const std::string& sketchPid, sld_mcp::v1::SketchItemRef* ref)
	{
		bool isPoint = false;
		long long id0 = 0, id1 = 0;
		if (!SketchEntityId(entity, isPoint, id0, id1))
			return false;
		ref->set_sketch_persist_id(sketchPid);
		ref->set_point(isPoint);
		ref->set_id0(id0);
		ref->set_id1(id1);
		return true;
	}

	// Constrained status of a sketch segment or point (swConstrainedStatus_e).
	long ItemStatus(IDispatch* entity)
	{
		long status = 0;
		if (ATL::CComQIPtr<ISketchSegment> segment(entity); segment)
			segment->get_Status(&status);
		else if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
			point->get_Status(&status);
		return status;
	}

	// Sketch points report sketch coordinates.
	bool PointPosition(IDispatch* pointDisp, double out[3])
	{
		ATL::CComQIPtr<ISketchPoint> point(pointDisp);
		if (!point || FAILED(point->get_X(&out[0])) || FAILED(point->get_Y(&out[1])))
			return false;
		out[2] = 0.0;
		return true;
	}

	// Fills a point's position and ref, e.g. a line's start. Leaves both
	// unset if SolidWorks won't describe the point.
	void SetItemPoint(IDispatch* pointDisp, const std::string& sketchPid, sld_mcp::v1::Vec3* position,
		sld_mcp::v1::SketchItemRef* ref)
	{
		double p[3];
		sld_mcp::v1::SketchItemRef r;
		if (!PointPosition(pointDisp, p) || !SetItemRef(pointDisp, sketchPid, &r))
			return;
		SetVec3(position, p);
		*ref = r;
	}

	// Describes a sketch segment or point. False if it isn't one.
	bool DescribeSketchItem(IDispatch* entity, const std::string& sketchPid, sld_mcp::v1::SketchItem* item)
	{
		if (!SetItemRef(entity, sketchPid, item->mutable_ref()))
			return false;
		item->set_status(ToDefinitionStatus(ItemStatus(entity)));

		if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
		{
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_POINT);
			double p[3];
			if (PointPosition(point, p))
				SetVec3(item->mutable_position(), p);
			return true;
		}
		ATL::CComQIPtr<ISketchSegment> segment(entity);
		if (!segment)
			return false;

		ATL::CComBSTR name;
		if (SUCCEEDED(segment->GetName(&name)))
			item->set_name(ToUtf8(name));
		VARIANT_BOOL construction = VARIANT_FALSE;
		if (SUCCEEDED(segment->get_ConstructionGeometry(&construction)))
			item->set_construction(construction != VARIANT_FALSE);
		double length = 0.0;
		if (SUCCEEDED(segment->GetLength(&length)))
			item->set_length(length);

		auto startEnd = [&](IDispatch* start, IDispatch* end) {
			sld_mcp::v1::Vec3 position;
			sld_mcp::v1::SketchItemRef ref;
			SetItemPoint(start, sketchPid, &position, &ref);
			if (ref.id0() || ref.id1() || ref.point())
			{
				*item->mutable_start() = position;
				*item->mutable_start_point() = ref;
			}
			position.Clear();
			ref.Clear();
			SetItemPoint(end, sketchPid, &position, &ref);
			if (ref.id0() || ref.id1() || ref.point())
			{
				*item->mutable_end() = position;
				*item->mutable_end_point() = ref;
			}
		};

		long type = -1;
		segment->GetType(&type);
		switch (type)
		{
		case swSketchLINE:
		{
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_LINE);
			ATL::CComQIPtr<ISketchLine> line(segment);
			ATL::CComPtr<IDispatch> start, end;
			if (line && SUCCEEDED(line->GetStartPoint2(&start)) && SUCCEEDED(line->GetEndPoint2(&end)))
				startEnd(start, end);
			break;
		}
		case swSketchARC:
		{
			ATL::CComQIPtr<ISketchArc> arc(segment);
			int circle = 0;
			if (arc)
				arc->IsCircle(&circle);
			item->set_type(circle ? sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE : sld_mcp::v1::SKETCH_ITEM_TYPE_ARC);
			if (!arc)
				break;
			ATL::CComPtr<IDispatch> center;
			if (SUCCEEDED(arc->GetCenterPoint2(&center)))
				SetItemPoint(center, sketchPid, item->mutable_center(), item->mutable_center_point());
			double radius = 0.0;
			if (SUCCEEDED(arc->GetRadius(&radius)))
				item->set_radius(radius);
			ATL::CComPtr<IDispatch> start, end;
			if (!circle && SUCCEEDED(arc->GetStartPoint2(&start)) && SUCCEEDED(arc->GetEndPoint2(&end)))
				startEnd(start, end);
			break;
		}
		case swSketchSPLINE:
		{
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_SPLINE);
			ATL::CComQIPtr<ISketchSpline> spline(segment);
			ATL::CComVariant points;
			if (spline && SUCCEEDED(spline->GetPoints2(&points)))
			{
				const auto list = comvariant::ToDispatches(points);
				if (list.size() >= 2)
					startEnd(list.front(), list.back());
			}
			break;
		}
		case swSketchELLIPSE:
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_ELLIPSE);
			break;
		default:
			item->set_type(sld_mcp::v1::SKETCH_ITEM_TYPE_OTHER);
			break;
		}
		return true;
	}

	// Sets the sketch persist ID on every ref in an item, for items described
	// before the sketch's persist ID was known.
	void StampSketchId(sld_mcp::v1::SketchItem* item, const std::string& sketchPid)
	{
		item->mutable_ref()->set_sketch_persist_id(sketchPid);
		if (item->has_start_point())
			item->mutable_start_point()->set_sketch_persist_id(sketchPid);
		if (item->has_end_point())
			item->mutable_end_point()->set_sketch_persist_id(sketchPid);
		if (item->has_center_point())
			item->mutable_center_point()->set_sketch_persist_id(sketchPid);
	}

	// A sketch's segments and points, to look up by ID.
	class SketchItems
	{
	public:
		struct Entry
		{
			ATL::CComPtr<IDispatch> obj;
			bool point;
			long long id0;
			long long id1;
		};

		void Load(ISketch* sketch)
		{
			m_entries.clear();
			ATL::CComVariant segments, points;
			if (SUCCEEDED(sketch->GetSketchSegments(&segments)))
				Add(segments);
			if (SUCCEEDED(sketch->GetSketchPoints2(&points)))
				Add(points);
		}

		// The entity `ref` names, or null if the sketch has no such entity.
		ATL::CComPtr<IDispatch> Find(const sld_mcp::v1::SketchItemRef& ref) const
		{
			for (const Entry& e : m_entries)
				if (e.point == ref.point() && e.id0 == ref.id0() && e.id1 == ref.id1())
					return e.obj;
			return nullptr;
		}

		const std::vector<Entry>& All() const { return m_entries; }

	private:
		void Add(const VARIANT& array)
		{
			for (const auto& disp : comvariant::ToDispatches(array))
			{
				Entry e{disp, false, 0, 0};
				if (SketchEntityId(disp, e.point, e.id0, e.id1))
					m_entries.push_back(e);
			}
		}

		std::vector<Entry> m_entries;
	};

	// The sketch's overall status and the entities that aren't fully defined.
	void ReadSketchState(ISketch* sketch, const std::string& sketchPid, sld_mcp::v1::SketchState* state)
	{
		state->set_status(SketchStatus(sketch));
		SketchItems items;
		items.Load(sketch);
		int count = 0;
		for (const auto& e : items.All())
		{
			const long status = ItemStatus(e.obj);
			if (status == swFullyConstrained || status == 0 || status == swUnknownConstraint)
				continue;
			++count;
			if (state->not_fully_defined_size() < kMaxListedUndefined)
			{
				sld_mcp::v1::SketchItem item;
				if (DescribeSketchItem(e.obj, sketchPid, &item))
					state->add_not_fully_defined()->Swap(&item);
			}
		}
		state->set_not_fully_defined_count(count);
	}

	// Whether a sketch segment or point belongs to the sketch called `sketchName`.
	bool InSketch(IDispatch* entity, const std::string& sketchName)
	{
		ATL::CComPtr<ISketch> sketch;
		if (ATL::CComQIPtr<ISketchSegment> segment(entity); segment)
			segment->GetSketch(&sketch);
		else if (ATL::CComQIPtr<ISketchPoint> point(entity); point)
			point->GetSketch(&sketch);
		ATL::CComQIPtr<IFeature> feature(sketch.p);
		return feature && FeatureName(feature) == sketchName;
	}

	// ---- Dimensions -------------------------------------------------------------

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

	// Name, value and driven state; the type is the caller's job.
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
		long type = 0;
		if (SUCCEEDED(display->GetType(&type)))
			info->set_type(ToDimensionType(type));
		DescribeDimensionValue(dim, info);
		return true;
	}

	// Every display dimension of a feature, in order.
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

	// Deletes display dimensions from the open sketch. Best effort.
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

	private:
		ATL::CComPtr<ISldWorks> m_app;
		std::vector<std::pair<long, VARIANT_BOOL>> m_saved;
	};

	// No dialogs while adding dimensions: don't ask for a value on creation,
	// and make a dimension that would over-define the sketch driven instead
	// of asking what to do (the RPC then reports it as a conflict).
	void SilenceDimensionPrompts(PreferenceToggles& toggles)
	{
		toggles.Set(swInputDimValOnCreate, false);
		toggles.Set(swSketchOverdefiningDimsPromptToSetState, false);
		toggles.Set(swSketchOverdefiningDimsSetDrivenByDefault, true);
	}

	// ---- Refs (what dimensions and relations attach to) -------------------------

	// The part origin as a sketch point (the Origin feature's sketch has it as
	// its first point), plus that feature's name for SelectByID2.
	void FindOrigin(IModelDoc2* doc, ATL::CComPtr<ISketchPoint>& point, std::string& featureName)
	{
		ATL::CComPtr<IFeature> feature;
		if (FAILED(doc->IFirstFeature(&feature)))
			return;
		while (feature)
		{
			ATL::CComBSTR typeName;
			if (SUCCEEDED(feature->GetTypeName2(&typeName)) && typeName == L"OriginProfileFeature")
			{
				featureName = FeatureName(feature);
				ATL::CComPtr<ISketch> sketch;
				ATL::CComVariant points;
				if (SketchOf(feature, sketch).ok() && SUCCEEDED(sketch->GetSketchPoints2(&points)))
				{
					const auto list = comvariant::ToDispatches(points);
					if (!list.empty())
						point = ATL::CComQIPtr<ISketchPoint>(list.front().p);
				}
				return;
			}
			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				return;
			feature = next;
		}
	}

	// Where a ref is, in sketch coordinates, for placing dimension text.
	struct RefShape
	{
		enum Kind { Point, Line, Round } kind = Point;
		double a[2] = {};  // the point; a line's start; a circle's center
		double b[2] = {};  // a line's end
		double radius = 0.0;
	};

	// A SketchRef resolved against the open sketch.
	struct ResolvedRef
	{
		ATL::CComPtr<IDispatch> obj;  // null for the origin
		bool origin = false;
		sld_mcp::v1::SketchItemType type = sld_mcp::v1::SKETCH_ITEM_TYPE_UNSPECIFIED;  // sketch items
		bool isPoint = false;  // a sketch point or the origin
		bool isLine = false;   // a sketch line or straight model edge
		bool isRound = false;  // a sketch arc/circle or circular model edge
		RefShape shape;
	};

	// Maps a model-space point into the sketch (z dropped).
	bool ModelToSketch2D(const SketchSpace& space, const double model[3], double out[2])
	{
		double p[3];
		if (!space.ToSketch(model, p))
			return false;
		out[0] = p[0];
		out[1] = p[1];
		return true;
	}

	// Context for resolving refs: the open sketch and how to reach the rest
	// of the model.
	struct RefContext
	{
		IModelDocExtension* ext;
		const SketchItems& items;
		const SketchSpace& space;
		const std::string& sketchPid;
		const std::string& sketchName;
	};

	grpc::Status ResolveRef(const RefContext& ctx, const sld_mcp::v1::SketchRef& ref, ResolvedRef& out)
	{
		using Ref = sld_mcp::v1::SketchRef;
		switch (ref.target_case())
		{
		case Ref::kOrigin:
		{
			out.origin = true;
			out.isPoint = true;
			const double zero[3] = {0.0, 0.0, 0.0};
			out.shape.kind = RefShape::Point;
			ModelToSketch2D(ctx.space, zero, out.shape.a);
			return grpc::Status::OK;
		}
		case Ref::kItem:
		{
			out.obj = ctx.items.Find(ref.item());
			if (!out.obj)
				return grpc::Status(grpc::StatusCode::NOT_FOUND,
					"no such entity in sketch \"" + ctx.sketchName +
					"\" (deleted or merged?) -- inspect the sketch again for current IDs");
			sld_mcp::v1::SketchItem item;
			if (!DescribeSketchItem(out.obj, ctx.sketchPid, &item))
				return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't read a sketch entity's geometry");
			out.type = item.type();
			switch (item.type())
			{
			case sld_mcp::v1::SKETCH_ITEM_TYPE_POINT:
				out.isPoint = true;
				out.shape.kind = RefShape::Point;
				out.shape.a[0] = item.position().x();
				out.shape.a[1] = item.position().y();
				break;
			case sld_mcp::v1::SKETCH_ITEM_TYPE_LINE:
				out.isLine = true;
				out.shape.kind = RefShape::Line;
				out.shape.a[0] = item.start().x();
				out.shape.a[1] = item.start().y();
				out.shape.b[0] = item.end().x();
				out.shape.b[1] = item.end().y();
				break;
			case sld_mcp::v1::SKETCH_ITEM_TYPE_ARC:
			case sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE:
				out.isRound = true;
				out.shape.kind = RefShape::Round;
				out.shape.a[0] = item.center().x();
				out.shape.a[1] = item.center().y();
				out.shape.radius = item.radius();
				break;
			default:
				// Splines etc.: aim at the middle of the end points.
				out.shape.kind = RefShape::Point;
				out.shape.a[0] = (item.start().x() + item.end().x()) / 2.0;
				out.shape.a[1] = (item.start().y() + item.end().y()) / 2.0;
				break;
			}
			return grpc::Status::OK;
		}
		case Ref::kModelPersistId:
		{
			if (grpc::Status status = ResolvePersistId(ctx.ext, ref.model_persist_id(), out.obj); !status.ok())
				return status;
			if (ATL::CComQIPtr<IEdge> edge(out.obj.p); edge)
			{
				ATL::CComVariant params;
				std::vector<double> c;
				if (SUCCEEDED(edge->GetCurveParams2(&params)) && (c = comvariant::ToDoubles(params)).size() >= 6)
				{
					ModelToSketch2D(ctx.space, &c[0], out.shape.a);
					ModelToSketch2D(ctx.space, &c[3], out.shape.b);
				}
				ATL::CComPtr<ICurve> curve;
				long identity = 0;
				if (SUCCEEDED(edge->IGetCurve(&curve)) && curve && SUCCEEDED(curve->Identity(&identity)))
				{
					if (identity == LINE_TYPE)
					{
						out.isLine = true;
						out.shape.kind = RefShape::Line;
					}
					else if (identity == CIRCLE_TYPE)
					{
						ATL::CComVariant circle;
						std::vector<double> p;
						if (SUCCEEDED(curve->get_CircleParams(&circle)) && (p = comvariant::ToDoubles(circle)).size() >= 7)
						{
							out.isRound = true;
							out.shape.kind = RefShape::Round;
							ModelToSketch2D(ctx.space, &p[0], out.shape.a);
							out.shape.radius = p[6];
						}
					}
				}
				if (out.shape.kind == RefShape::Point)
					for (int i = 0; i < 2; ++i)
						out.shape.a[i] = (out.shape.a[i] + out.shape.b[i]) / 2.0;
				return grpc::Status::OK;
			}
			if (ATL::CComQIPtr<IFace2> face(out.obj.p); face)
			{
				ATL::CComVariant box;
				std::vector<double> b;
				if (SUCCEEDED(face->GetBox(&box)) && (b = comvariant::ToDoubles(box)).size() >= 6)
				{
					const double center[3] = {(b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2};
					ModelToSketch2D(ctx.space, center, out.shape.a);
				}
				return grpc::Status::OK;
			}
			if (ATL::CComQIPtr<IVertex> vertex(out.obj.p); vertex)
			{
				ATL::CComVariant point;
				std::vector<double> p;
				if (SUCCEEDED(vertex->GetPoint(&point)) && (p = comvariant::ToDoubles(point)).size() >= 3)
					ModelToSketch2D(ctx.space, p.data(), out.shape.a);
				out.isPoint = true;
				return grpc::Status::OK;
			}
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"only sketch entities, the origin, and model edges, faces or vertices can be referenced");
		}
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a reference is empty");
		}
	}

	// Adds a resolved ref to the selection.
	bool SelectRef(IModelDocExtension* ext, const ResolvedRef& ref, ISketchPoint* originPoint,
		const std::string& originName)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (ref.origin)
		{
			if (originPoint && SUCCEEDED(originPoint->Select4(VARIANT_TRUE, nullptr, &selected)) && selected)
				return true;
			if (originName.empty())
				return false;
			// The origin's point as the UI names it ("Point1@Origin").
			return SUCCEEDED(ext->SelectByID2(FromUtf8("Point1@" + originName), ATL::CComBSTR(L"EXTSKETCHPOINT"),
				0.0, 0.0, 0.0, VARIANT_TRUE, 0, nullptr, swSelectOptionDefault, &selected)) && selected;
		}
		if (ATL::CComQIPtr<ISketchSegment> segment(ref.obj.p); segment)
			return SUCCEEDED(segment->Select4(VARIANT_TRUE, nullptr, &selected)) && selected;
		if (ATL::CComQIPtr<ISketchPoint> point(ref.obj.p); point)
			return SUCCEEDED(point->Select4(VARIANT_TRUE, nullptr, &selected)) && selected;
		if (ATL::CComQIPtr<IEntity> entity(ref.obj.p); entity)
			return SUCCEEDED(entity->Select4(VARIANT_TRUE, nullptr, &selected)) && selected;
		return false;
	}

	// Checks that every sketch item ref in `refs` is in the target sketch
	// (`sketchPid` / `sketchName`). Call before opening the sketch.
	grpc::Status CheckRefsInSketch(IModelDocExtension* ext,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef>& refs,
		const std::string& sketchPid, const std::string& sketchName, const std::string& where)
	{
		for (int i = 0; i < refs.size(); ++i)
		{
			if (!refs[i].has_item() || refs[i].item().sketch_persist_id() == sketchPid)
				continue;
			// Same sketch, different bytes? Compare by name.
			ATL::CComPtr<IDispatch> obj;
			grpc::Status status = ResolvePersistId(ext, refs[i].item().sketch_persist_id(), obj);
			ATL::CComQIPtr<IFeature> other(obj.p);
			const std::string otherName = other ? FeatureName(other) : std::string();
			if (status.ok() && otherName == sketchName)
				continue;
			const std::string prefix = where + ", reference " + std::to_string(i + 1) + ": ";
			if (!status.ok())
				return grpc::Status(status.error_code(), prefix + "its sketch " + status.error_message());
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				prefix + "that entity is in sketch \"" + otherName + "\", not \"" + sketchName + "\"");
		}
		return grpc::Status::OK;
	}

	// Picks the sketch a dimension/relation request is about: by name, or the
	// sketch its first sketch item ref belongs to.
	template <typename Constraint>
	grpc::Status ResolveConstraintSketch(IModelDoc2* doc, IModelDocExtension* ext, const std::string& name,
		const google::protobuf::RepeatedPtrField<Constraint>& constraints, ATL::CComPtr<IFeature>& sketch)
	{
		if (!name.empty())
			return FindSketchByName(doc, name, sketch);
		for (const auto& constraint : constraints)
			for (const auto& ref : constraint.refs())
				if (ref.has_item())
				{
					ATL::CComPtr<IDispatch> obj;
					if (grpc::Status status = ResolvePersistId(ext, ref.item().sketch_persist_id(), obj); !status.ok())
						return grpc::Status(status.error_code(), "the referenced sketch " + status.error_message());
					sketch = ATL::CComQIPtr<IFeature>(obj.p);
					if (!sketch)
						return grpc::Status(grpc::StatusCode::INTERNAL, "a sketch reference didn't resolve to a feature");
					return grpc::Status::OK;
				}
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"name the sketch, or reference at least one of its entities");
	}

	// ---- Dimension placement ------------------------------------------------------

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

	// Shape checks that need only the request.
	grpc::Status ValidateSketchDimension(const sld_mcp::v1::SketchDimension& dim)
	{
		using K = sld_mcp::v1::DimensionKind;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		const int n = dim.refs_size();
		switch (dim.kind())
		{
		case K::DIMENSION_KIND_LENGTH:
		case K::DIMENSION_KIND_DIAMETER:
		case K::DIMENSION_KIND_RADIUS:
			if (n != 1)
				return invalid(std::string("a ") + DimensionKindName(dim.kind()) + " dimension takes 1 entity");
			break;
		case K::DIMENSION_KIND_DISTANCE:
		case K::DIMENSION_KIND_ANGLE:
			if (n != 2)
				return invalid(std::string("a ") + DimensionKindName(dim.kind()) + " dimension takes 2 entities");
			break;
		case K::DIMENSION_KIND_HORIZONTAL:
		case K::DIMENSION_KIND_VERTICAL:
			if (n != 1 && n != 2)
				return invalid(std::string("a ") + DimensionKindName(dim.kind()) +
					" dimension takes 1 line or 2 entities");
			break;
		default:
			return invalid("unknown dimension kind");
		}
		bool anyItem = false;
		for (const auto& ref : dim.refs())
		{
			if (ref.target_case() == sld_mcp::v1::SketchRef::TARGET_NOT_SET)
				return invalid("a reference is empty");
			anyItem = anyItem || ref.has_item();
		}
		if (!anyItem)
			return invalid("a dimension must involve at least one entity of the sketch");
		if (dim.has_value())
		{
			const double v = dim.value();
			if (!std::isfinite(v) || v <= 0.0)
				return invalid("a dimension value must be positive");
			if (dim.kind() == K::DIMENSION_KIND_ANGLE && v >= 2.0 * 3.14159265358979323846)
				return invalid("an angle must be less than 360 degrees");
		}
		return grpc::Status::OK;
	}

	// Shape checks once the refs are resolved.
	grpc::Status CheckDimensionRefs(sld_mcp::v1::DimensionKind kind, const std::vector<ResolvedRef>& refs)
	{
		using K = sld_mcp::v1::DimensionKind;
		auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
		switch (kind)
		{
		case K::DIMENSION_KIND_LENGTH:
			if (!refs[0].isLine)
				return invalid("a length dimension needs a line (use distance for other entities)");
			break;
		case K::DIMENSION_KIND_DIAMETER:
		case K::DIMENSION_KIND_RADIUS:
			if (!refs[0].isRound)
				return invalid(std::string("a ") + DimensionKindName(kind) + " dimension needs a circle or an arc");
			break;
		case K::DIMENSION_KIND_ANGLE:
			if (!refs[0].isLine || !refs[1].isLine)
				return invalid("an angle dimension needs 2 lines");
			break;
		case K::DIMENSION_KIND_HORIZONTAL:
		case K::DIMENSION_KIND_VERTICAL:
			if (refs.size() == 1 && !refs[0].isLine)
				return invalid(std::string("a ") + DimensionKindName(kind) +
					" dimension of a single entity needs a line; give 2 entities to measure between them");
			break;
		default:
			break;
		}
		return grpc::Status::OK;
	}

	// The point a ref is measured from: a point, a line's middle, a circle's center.
	void RefAnchor(const RefShape& s, double out[2])
	{
		if (s.kind == RefShape::Line)
		{
			out[0] = (s.a[0] + s.b[0]) / 2.0;
			out[1] = (s.a[1] + s.b[1]) / 2.0;
		}
		else
		{
			out[0] = s.a[0];
			out[1] = s.a[1];
		}
	}

	// Where to put a new dimension's text, in sketch coordinates: off to the
	// side of what it measures. For distances and angles the spot also tells
	// SolidWorks which dimension is meant (aligned, which angle sector).
	grpc::Status PlaceDimension(sld_mcp::v1::DimensionKind kind, const std::vector<ResolvedRef>& refs, double out[2])
	{
		using K = sld_mcp::v1::DimensionKind;
		// Bounding box of everything involved sets the scale of the offset.
		double lo[2] = {HUGE_VAL, HUGE_VAL}, hi[2] = {-HUGE_VAL, -HUGE_VAL};
		auto grow = [&](const double p[2], double r) {
			for (int i = 0; i < 2; ++i)
			{
				lo[i] = std::min(lo[i], p[i] - r);
				hi[i] = std::max(hi[i], p[i] + r);
			}
		};
		for (const auto& ref : refs)
		{
			grow(ref.shape.a, ref.shape.kind == RefShape::Round ? ref.shape.radius : 0.0);
			if (ref.shape.kind == RefShape::Line)
				grow(ref.shape.b, 0.0);
		}
		const double size = std::hypot(hi[0] - lo[0], hi[1] - lo[1]);
		const double offset = std::max(0.2 * size, 0.002);

		switch (kind)
		{
		case K::DIMENSION_KIND_HORIZONTAL:
			out[0] = (lo[0] + hi[0]) / 2.0;
			out[1] = hi[1] + offset;
			return grpc::Status::OK;
		case K::DIMENSION_KIND_VERTICAL:
			out[0] = hi[0] + offset;
			out[1] = (lo[1] + hi[1]) / 2.0;
			return grpc::Status::OK;
		case K::DIMENSION_KIND_DIAMETER:
		case K::DIMENSION_KIND_RADIUS:
		{
			const double d = (refs[0].shape.radius + offset) * std::sqrt(0.5);
			out[0] = refs[0].shape.a[0] + d;
			out[1] = refs[0].shape.a[1] + d;
			return grpc::Status::OK;
		}
		case K::DIMENSION_KIND_ANGLE:
		{
			const RefShape& l1 = refs[0].shape;
			const RefShape& l2 = refs[1].shape;
			const double d1[2] = {l1.b[0] - l1.a[0], l1.b[1] - l1.a[1]};
			const double d2[2] = {l2.b[0] - l2.a[0], l2.b[1] - l2.a[1]};
			const double cross = d1[0] * d2[1] - d1[1] * d2[0];
			const double len1 = std::hypot(d1[0], d1[1]), len2 = std::hypot(d2[0], d2[1]);
			if (len1 <= 0.0 || len2 <= 0.0 || std::abs(cross) < 1e-9 * len1 * len2)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the two lines are parallel -- there's no angle between them (use distance)");
			// Where the (extended) lines meet.
			const double t = ((l2.a[0] - l1.a[0]) * d2[1] - (l2.a[1] - l1.a[1]) * d2[0]) / cross;
			const double x[2] = {l1.a[0] + t * d1[0], l1.a[1] + t * d1[1]};
			// Toward the far end of each line from there: the sector the lines are in.
			auto away = [&](const RefShape& l, double u[2]) {
				const double da = std::hypot(l.a[0] - x[0], l.a[1] - x[1]);
				const double db = std::hypot(l.b[0] - x[0], l.b[1] - x[1]);
				const double* tip = db >= da ? l.b : l.a;
				const double len = std::max(da, db);
				u[0] = (tip[0] - x[0]) / len;
				u[1] = (tip[1] - x[1]) / len;
				return len;
			};
			double u1[2], u2[2];
			const double reach = std::min(away(l1, u1), away(l2, u2));
			double bis[2] = {u1[0] + u2[0], u1[1] + u2[1]};
			double bl = std::hypot(bis[0], bis[1]);
			if (bl < 1e-9)
			{
				bis[0] = -u1[1];
				bis[1] = u1[0];
				bl = 1.0;
			}
			const double dist = std::max(0.5 * reach, offset);
			out[0] = x[0] + bis[0] / bl * dist;
			out[1] = x[1] + bis[1] / bl * dist;
			return grpc::Status::OK;
		}
		case K::DIMENSION_KIND_LENGTH:
		case K::DIMENSION_KIND_DISTANCE:
		default:
		{
			double a[2], b[2];
			if (kind == K::DIMENSION_KIND_LENGTH)
			{
				std::copy(refs[0].shape.a, refs[0].shape.a + 2, a);
				std::copy(refs[0].shape.b, refs[0].shape.b + 2, b);
			}
			else
			{
				RefAnchor(refs[0].shape, a);
				RefAnchor(refs[1].shape, b);
			}
			const double mid[2] = {(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0};
			const double d[2] = {b[0] - a[0], b[1] - a[1]};
			const double len = std::hypot(d[0], d[1]);
			if (len < 1e-12)
			{
				out[0] = mid[0] + offset;
				out[1] = mid[1] + offset;
			}
			else
			{
				// Beside the segment between them, not past either end, so
				// SolidWorks makes an aligned dimension.
				out[0] = mid[0] - d[1] / len * offset;
				out[1] = mid[1] + d[0] / len * offset;
			}
			return grpc::Status::OK;
		}
		}
	}

	// Creates the dimension for what's selected, with its text at `at`
	// (model space).
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

	// ---- Relations ----------------------------------------------------------------

	const char* RelationKindName(sld_mcp::v1::RelationKind kind)
	{
		switch (kind)
		{
		case sld_mcp::v1::RELATION_KIND_HORIZONTAL:    return "horizontal";
		case sld_mcp::v1::RELATION_KIND_VERTICAL:      return "vertical";
		case sld_mcp::v1::RELATION_KIND_COINCIDENT:    return "coincident";
		case sld_mcp::v1::RELATION_KIND_MIDPOINT:      return "midpoint";
		case sld_mcp::v1::RELATION_KIND_COLLINEAR:     return "collinear";
		case sld_mcp::v1::RELATION_KIND_PARALLEL:      return "parallel";
		case sld_mcp::v1::RELATION_KIND_PERPENDICULAR: return "perpendicular";
		case sld_mcp::v1::RELATION_KIND_TANGENT:       return "tangent";
		case sld_mcp::v1::RELATION_KIND_CONCENTRIC:    return "concentric";
		case sld_mcp::v1::RELATION_KIND_CORADIAL:      return "coradial";
		case sld_mcp::v1::RELATION_KIND_EQUAL:         return "equal";
		case sld_mcp::v1::RELATION_KIND_FIXED:         return "fixed";
		case sld_mcp::v1::RELATION_KIND_SYMMETRIC:     return "symmetric";
		case sld_mcp::v1::RELATION_KIND_MERGE:         return "merge";
		default:                                        return "unknown";
		}
	}

	grpc::Status ValidateSketchRelation(const sld_mcp::v1::SketchRelation& relation)
	{
		using K = sld_mcp::v1::RelationKind;
		const int n = relation.refs_size();
		int lo = 2, hi = 2;
		switch (relation.kind())
		{
		case K::RELATION_KIND_HORIZONTAL:
		case K::RELATION_KIND_VERTICAL:
		case K::RELATION_KIND_FIXED:
			lo = 1;
			hi = kMaxConstraintRefs;
			break;
		case K::RELATION_KIND_COLLINEAR:
		case K::RELATION_KIND_PARALLEL:
		case K::RELATION_KIND_CONCENTRIC:
		case K::RELATION_KIND_EQUAL:
			hi = kMaxConstraintRefs;
			break;
		case K::RELATION_KIND_SYMMETRIC:
			lo = hi = 3;
			break;
		case K::RELATION_KIND_COINCIDENT:
		case K::RELATION_KIND_MIDPOINT:
		case K::RELATION_KIND_PERPENDICULAR:
		case K::RELATION_KIND_TANGENT:
		case K::RELATION_KIND_CORADIAL:
		case K::RELATION_KIND_MERGE:
			break;
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown relation kind");
		}
		if (n < lo || n > hi)
		{
			std::string want = lo == hi ? std::to_string(lo) : std::to_string(lo) + " to " + std::to_string(hi);
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				std::string("a ") + RelationKindName(relation.kind()) + " relation takes " + want + " entities");
		}
		bool anyItem = false;
		for (const auto& ref : relation.refs())
		{
			if (ref.target_case() == sld_mcp::v1::SketchRef::TARGET_NOT_SET)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a reference is empty");
			anyItem = anyItem || ref.has_item();
		}
		if (!anyItem)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a relation must involve at least one entity of the sketch");
		return grpc::Status::OK;
	}

	// The IModelDoc2::SketchAddConstraints command for a relation.
	const wchar_t* RelationCommand(sld_mcp::v1::RelationKind kind, bool allPoints)
	{
		switch (kind)
		{
		case sld_mcp::v1::RELATION_KIND_HORIZONTAL:    return allPoints ? L"sgHORIZPOINTS2D" : L"sgHORIZONTAL2D";
		case sld_mcp::v1::RELATION_KIND_VERTICAL:      return allPoints ? L"sgVERTPOINTS2D" : L"sgVERTICAL2D";
		case sld_mcp::v1::RELATION_KIND_COINCIDENT:    return L"sgCOINCIDENT";
		case sld_mcp::v1::RELATION_KIND_MIDPOINT:      return L"sgATMIDDLE";
		case sld_mcp::v1::RELATION_KIND_COLLINEAR:     return L"sgCOLINEAR";
		case sld_mcp::v1::RELATION_KIND_PARALLEL:      return L"sgPARALLEL";
		case sld_mcp::v1::RELATION_KIND_PERPENDICULAR: return L"sgPERPENDICULAR";
		case sld_mcp::v1::RELATION_KIND_TANGENT:       return L"sgTANGENT";
		case sld_mcp::v1::RELATION_KIND_CONCENTRIC:    return L"sgCONCENTRIC";
		case sld_mcp::v1::RELATION_KIND_CORADIAL:      return L"sgCORADIAL";
		case sld_mcp::v1::RELATION_KIND_EQUAL:         return L"sgSAMELENGTH";
		case sld_mcp::v1::RELATION_KIND_FIXED:         return L"sgFIXED";
		case sld_mcp::v1::RELATION_KIND_SYMMETRIC:     return L"sgSYMMETRIC";
		case sld_mcp::v1::RELATION_KIND_MERGE:         return L"sgMERGEPOINTS";
		default:                                        return nullptr;
		}
	}

	std::vector<ATL::CComPtr<ISketchRelation>> SketchRelations(ISketch* sketch)
	{
		std::vector<ATL::CComPtr<ISketchRelation>> out;
		ATL::CComPtr<ISketchRelationManager> manager;
		ATL::CComVariant list;
		if (FAILED(sketch->get_RelationManager(&manager)) || !manager || FAILED(manager->GetRelations(swAll, &list)))
			return out;
		for (const auto& disp : comvariant::ToDispatches(list))
			if (ATL::CComQIPtr<ISketchRelation> relation(disp.p); relation)
				out.push_back(relation);
		return out;
	}

	// Deletes the relations past the first `keep`, newest first. Best effort:
	// relies on SolidWorks listing relations in the order they were made.
	void DeleteRelationsAfter(ISketch* sketch, size_t keep)
	{
		ATL::CComPtr<ISketchRelationManager> manager;
		if (FAILED(sketch->get_RelationManager(&manager)) || !manager)
			return;
		const auto relations = SketchRelations(sketch);
		for (size_t i = relations.size(); i > keep; --i)
		{
			VARIANT_BOOL deleted = VARIANT_FALSE;
			manager->DeleteRelation(relations[i - 1], &deleted);
		}
	}

	void DescribeRelation(ISketchRelation* relation, const std::string& sketchPid, const std::string& sketchName,
		sld_mcp::v1::RelationInfo* info)
	{
		long type = 0;
		if (SUCCEEDED(relation->GetRelationType(&type)))
			info->set_constraint_type(static_cast<int32_t>(type));
		ATL::CComVariant entities;
		if (FAILED(relation->GetEntities(&entities)))
			return;
		for (const auto& entity : comvariant::ToDispatches(entities))
		{
			sld_mcp::v1::SketchItemRef ref;
			if (entity && InSketch(entity, sketchName) && SetItemRef(entity, sketchPid, &ref))
				*info->add_items() = ref;
			else
				info->set_external_count(info->external_count() + 1);
		}
	}

	// What FullyDefineSketch's error codes mean (swAutodimStatus_e).
	std::string AutodimError(long code)
	{
		switch (code)
		{
		case swAutodimStatusSketchIsEmpty:           return "the sketch is empty";
		case swAutodimStatusSketchIsOverDefined:     return "the sketch is over defined -- remove the conflicting dimension or relation first";
		case swAutodimStatusNoEntities:              return "there's nothing left to define";
		case swAutodimStatusEntitiesNotValid:        return "the sketch has entities SolidWorks can't auto-dimension";
		case swAutodimStatusDatumNotSupplied:        return "no datum was given";
		case swAutodimStatusDatumNotUnique:          return "the datum is ambiguous";
		case swAutodimStatusDatumNotValidType:       return "a datum must be a point, a line or the origin";
		case swAutodimStatusDatumLineNotCenterline:  return "a datum line must be a construction line";
		case swAutodimStatusDatumLineNotVertical:    return "the horizontal-dimension datum line must be vertical";
		case swAutodimStatusDatumLineNotHorizontal:  return "the vertical-dimension datum line must be horizontal";
		case swAutodimStatusAlgorithmFailed:         return "SolidWorks' algorithm failed on this sketch";
		case swAutodimStatusSketchNoSolutionFound:   return "the sketch has no solution";
		case swAutodimStatusCenterlineNotAllowed:    return "a centerline isn't allowed here";
		default:                                     return "error code " + std::to_string(code);
		}
	}

	// ---- HoleWizard -------------------------------------------------------------

	// Most hole locations one HoleWizard call may take.
	constexpr int kMaxHoleLocations = 500;
	// Locations are selected with a short ray straight down onto the face,
	// starting this far (meters) above it, so the ray starts outside the
	// material and its first hit is the face.
	constexpr double kHoleRayLift = 1e-4;
	constexpr double kHoleRayRadius = 1e-5;

	// A planar face's plane: outward unit normal and a point on it.
	struct FacePlane
	{
		double normal[3] = {};
		double origin[3] = {};
	};

	grpc::Status ReadFacePlane(IFace2* face, FacePlane& plane)
	{
		ATL::CComPtr<ISurface> surface;
		VARIANT_BOOL planar = VARIANT_FALSE;
		if (FAILED(face->IGetSurface(&surface)) || !surface || FAILED(surface->IsPlane(&planar)) || !planar)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that face isn't planar -- the Hole Wizard needs a planar face");

		// IFace2::Normal accounts for the face's sense, so it points out of
		// the material; PlaneParams is [normal, root point].
		ATL::CComVariant normal, params;
		std::vector<double> n, p;
		if (FAILED(face->get_Normal(&normal)) || (n = comvariant::ToDoubles(normal)).size() < 3 ||
			FAILED(surface->get_PlaneParams(&params)) || (p = comvariant::ToDoubles(params)).size() < 6)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't report the face's plane");
		const double len = std::sqrt(n[0] * n[0] + n[1] * n[1] + n[2] * n[2]);
		if (!(std::isfinite(len) && len > 0.0))
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks reported a zero normal for the face");
		for (int i = 0; i < 3; ++i)
		{
			plane.normal[i] = n[i] / len;
			plane.origin[i] = p[3 + i];
		}
		return grpc::Status::OK;
	}

	std::array<double, 3> ProjectOntoPlane(const FacePlane& plane, const double in[3])
	{
		double distance = 0.0;
		for (int i = 0; i < 3; ++i)
			distance += (in[i] - plane.origin[i]) * plane.normal[i];
		return {in[0] - distance * plane.normal[0], in[1] - distance * plane.normal[1],
			in[2] - distance * plane.normal[2]};
	}

	// "(10, 20, 5) mm" for error messages; `p` in meters.
	std::string PointText(const std::array<double, 3>& p)
	{
		char text[96];
		std::snprintf(text, sizeof(text), "(%g, %g, %g) mm", p[0] * 1000.0, p[1] * 1000.0, p[2] * 1000.0);
		return text;
	}

	// The Hole Wizard's placement sketch: the sub-feature sketch that holds
	// only points (the hole centers). The other one is the hole's profile,
	// made of lines.
	grpc::Status FindHolePlacementSketch(IFeature* hole, ATL::CComPtr<IFeature>& sketchFeature,
		ATL::CComPtr<ISketch>& sketch)
	{
		sketchFeature.Release();
		sketch.Release();
		ATL::CComPtr<IFeature> sub;
		HRESULT hr = hole->IGetFirstSubFeature(&sub);
		while (SUCCEEDED(hr) && sub)
		{
			ATL::CComBSTR typeName;
			ATL::CComPtr<ISketch> candidate;
			if (SUCCEEDED(sub->GetTypeName2(&typeName)) &&
				(typeName == L"ProfileFeature" || typeName == L"3DProfileFeature") &&
				SketchOf(sub, candidate).ok())
			{
				ATL::CComVariant segments, points;
				candidate->GetSketchSegments(&segments);
				candidate->GetSketchPoints2(&points);
				size_t realSegments = 0;
				for (const auto& disp : comvariant::ToDispatches(segments))
				{
					ATL::CComQIPtr<ISketchSegment> segment(disp.p);
					VARIANT_BOOL construction = VARIANT_FALSE;
					if (segment && SUCCEEDED(segment->get_ConstructionGeometry(&construction)) && !construction)
						++realSegments;
				}
				if (realSegments == 0 && !comvariant::ToDispatches(points).empty())
				{
					sketchFeature = sub;
					sketch = candidate;
				}
			}
			ATL::CComPtr<IFeature> next;
			hr = sub->IGetNextSubFeature(&next);
			sub = next;
		}
		if (!sketchFeature)
			return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't find the hole feature's placement sketch");
		return grpc::Status::OK;
	}

	std::vector<ATL::CComPtr<IDispatch>> SketchPoints(ISketch* sketch)
	{
		ATL::CComVariant points;
		if (FAILED(sketch->GetSketchPoints2(&points)))
			return {};
		return comvariant::ToDispatches(points);
	}

	// Deletes a feature and the sketches it absorbed. Best effort.
	void DeleteFeatureAndSketches(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (feature && SUCCEEDED(feature->Select2(VARIANT_FALSE, 0, &selected)) && selected)
		{
			VARIANT_BOOL deleted = VARIANT_FALSE;
			ext->DeleteSelection2(swDelete_Absorbed, &deleted);
		}
		doc->ClearSelection2(VARIANT_TRUE);
	}

	// Reads back what the Hole Wizard built. Best effort: anything SolidWorks
	// won't report (or reports as -1 for "doesn't apply") stays 0.
	void ReadHoleSpec(IFeature* hole, sld_mcp::v1::HoleSpec* spec)
	{
		ATL::CComPtr<IDispatch> definition;
		if (FAILED(hole->GetDefinition(&definition)) || !definition)
			return;
		ATL::CComQIPtr<IWizardHoleFeatureData2> data(definition.p);
		if (!data)
			return;

		ATL::CComBSTR size;
		if (SUCCEEDED(data->get_FastenerSize(&size)))
			spec->set_size(ToUtf8(size));
		long value = 0;
		if (SUCCEEDED(data->get_Standard2(&value)))
			spec->set_standard(static_cast<int32_t>(value));
		if (SUCCEEDED(data->get_FastenerType2(&value)))
			spec->set_fastener_type(static_cast<int32_t>(value));
		int endCondition = -1;
		if (SUCCEEDED(data->get_EndCondition(&endCondition)))
			spec->set_through_all(endCondition == swEndCondThroughAll);

		using Getter = HRESULT (__stdcall IWizardHoleFeatureData2::*)(double*);
		auto read = [&](Getter getter) {
			double v = 0.0;
			return SUCCEEDED((data.p->*getter)(&v)) && std::isfinite(v) && v > 0.0 ? v : 0.0;
		};
		double diameter = read(&IWizardHoleFeatureData2::get_HoleDiameter);
		if (diameter == 0.0)
			diameter = read(&IWizardHoleFeatureData2::get_ThruHoleDiameter);
		spec->set_diameter(diameter);
		if (!spec->through_all())
			spec->set_depth(read(&IWizardHoleFeatureData2::get_HoleDepth));
		spec->set_counterbore_diameter(read(&IWizardHoleFeatureData2::get_CounterBoreDiameter));
		spec->set_counterbore_depth(read(&IWizardHoleFeatureData2::get_CounterBoreDepth));
		spec->set_countersink_diameter(read(&IWizardHoleFeatureData2::get_CounterSinkDiameter));
		spec->set_countersink_angle(read(&IWizardHoleFeatureData2::get_CounterSinkAngle));
		spec->set_tap_drill_diameter(read(&IWizardHoleFeatureData2::get_TapDrillDiameter));
		spec->set_thread_diameter(read(&IWizardHoleFeatureData2::get_ThreadDiameter));
		spec->set_thread_depth(read(&IWizardHoleFeatureData2::get_ThreadDepth));
	}

	// ---- Assemblies ---------------------------------------------------------------

	// The component an assembly entity (face, edge, vertex, ...) belongs to;
	// null for the assembly's own entities or anything that isn't an IEntity.
	ATL::CComPtr<IComponent2> ComponentOf(IUnknown* obj)
	{
		ATL::CComQIPtr<IEntity> entity(obj);
		ATL::CComPtr<IDispatch> disp;
		if (!entity || FAILED(entity->GetComponent(&disp)) || !disp)
			return {};
		ATL::CComQIPtr<IComponent2> component(disp.p);
		return ATL::CComPtr<IComponent2>(component);
	}

	std::string ComponentName(IComponent2* component)
	{
		ATL::CComBSTR name;
		if (component)
			component->get_Name2(&name);
		return ToUtf8(name);
	}

	// Every component of the assembly, nested ones included.
	std::vector<ATL::CComPtr<IComponent2>> AllComponents(IAssemblyDoc* assembly)
	{
		std::vector<ATL::CComPtr<IComponent2>> out;
		ATL::CComVariant all;
		if (FAILED(assembly->GetComponents(VARIANT_FALSE, &all)))
			return out;
		for (const auto& disp : comvariant::ToDispatches(all))
			if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
				out.push_back(ATL::CComPtr<IComponent2>(component));
		return out;
	}

	bool IsSubassembly(IComponent2* component)
	{
		int children = 0;
		return SUCCEEDED(component->IGetChildrenCount(&children)) && children > 0;
	}

	bool IsSuppressed(IComponent2* component)
	{
		long state = swComponentFullyResolved;
		return SUCCEEDED(component->GetSuppression2(&state)) && state == swComponentSuppressed;
	}

	// The components that hold geometry: parts (subassemblies' bodies are
	// their children's), not suppressed. `visibleOnly` also drops hidden ones.
	std::vector<ATL::CComPtr<IComponent2>> GeometryComponents(IAssemblyDoc* assembly, bool visibleOnly)
	{
		std::vector<ATL::CComPtr<IComponent2>> out;
		for (const auto& component : AllComponents(assembly))
		{
			if (IsSubassembly(component) || IsSuppressed(component))
				continue;
			VARIANT_BOOL hidden = VARIANT_FALSE;
			if (visibleOnly && SUCCEEDED(component->IsHidden(VARIANT_TRUE, &hidden)) && hidden)
				continue;
			out.push_back(component);
		}
		return out;
	}

	// A component's solid and surface bodies.
	std::vector<ATL::CComPtr<IBody2>> ComponentBodies(IComponent2* component)
	{
		std::vector<ATL::CComPtr<IBody2>> out;
		for (long type : { static_cast<long>(swSolidBody), static_cast<long>(swSheetBody) })
		{
			ATL::CComVariant info, bodies;
			if (FAILED(component->GetBodies3(type, &info, &bodies)))
				continue;
			for (const auto& disp : comvariant::ToDispatches(bodies))
				if (ATL::CComQIPtr<IBody2> body(disp.p); body)
					out.push_back(ATL::CComPtr<IBody2>(body));
		}
		return out;
	}

	// Fills the placement fields of `info` (origin + axes in assembly space)
	// and decides `local_geometry`: whether the bodies' own boxes are in
	// component coordinates. That's what the SolidWorks docs imply, but it's
	// checked here rather than assumed -- the bodies' box, once placed,
	// should land on the component's assembly-space box.
	void DescribePlacement(IComponent2* component, IMathUtility* math, sld_mcp::v1::ComponentInfo* info)
	{
		info->set_local_geometry(true);
		ATL::CComPtr<IMathTransform> xform;
		if (FAILED(component->get_Transform2(&xform)) || !xform)
			return;

		// Where the component's origin and unit axes land pins down its
		// component -> assembly transform, whatever IMathTransform's internal
		// layout is.
		const Eigen::Vector3d unit[4] = { Eigen::Vector3d::Zero(), Eigen::Vector3d::UnitX(),
			Eigen::Vector3d::UnitY(), Eigen::Vector3d::UnitZ() };
		Eigen::Vector3d placed[4];
		for (int i = 0; i < 4; ++i)
			if (!TransformPoint(math, xform, unit[i], placed[i]))
				return;
		Eigen::Affine3d placement = Eigen::Affine3d::Identity();
		placement.translation() = placed[0];
		for (int a = 0; a < 3; ++a)
			placement.linear().col(a) = placed[a + 1] - placed[0];

		SetVec3(info->mutable_origin(), Eigen::Vector3d(placement.translation()));
		SetVec3(info->mutable_x_axis(), Eigen::Vector3d(placement.linear().col(0)));
		SetVec3(info->mutable_y_axis(), Eigen::Vector3d(placement.linear().col(1)));
		SetVec3(info->mutable_z_axis(), Eigen::Vector3d(placement.linear().col(2)));

		// Assembly-space box of the component vs. its bodies' own boxes.
		ATL::CComVariant assemblyBoxVar;
		const Eigen::AlignedBox3d assemblyBox =
			SUCCEEDED(component->GetBox(VARIANT_FALSE, VARIANT_FALSE, &assemblyBoxVar))
			? ToAlignedBox(comvariant::ToDoubles(assemblyBoxVar)) : Eigen::AlignedBox3d();
		Eigen::AlignedBox3d bodyBox;
		for (const auto& body : ComponentBodies(component))
		{
			ATL::CComVariant box;
			if (SUCCEEDED(body->GetBodyBox(&box)))
				bodyBox.extend(ToAlignedBox(comvariant::ToDoubles(box)));
		}
		if (assemblyBox.isEmpty() || bodyBox.isEmpty())
			return;
		// The bodies' box with the placement applied (box of its 8 placed
		// corners), compared with the box as it is: whichever is closer to
		// the component's assembly-space box tells which space the body
		// geometry is in. Comparing whole boxes, not just centers, also
		// catches a part rotated about its own center. With an identity
		// placement both match and either answer is right.
		Eigen::AlignedBox3d movedBox;
		for (int corner = 0; corner < 8; ++corner)
			movedBox.extend(placement * bodyBox.corner(static_cast<Eigen::AlignedBox3d::CornerType>(corner)));
		auto mismatch = [&](const Eigen::AlignedBox3d& box) {
			return (box.min() - assemblyBox.min()).cwiseAbs().sum() + (box.max() - assemblyBox.max()).cwiseAbs().sum();
		};
		info->set_local_geometry(mismatch(movedBox) <= mismatch(bodyBox));
	}

	sld_mcp::v1::ComponentState ToComponentState(long state)
	{
		switch (state)
		{
		case swComponentSuppressed:
			return sld_mcp::v1::COMPONENT_STATE_SUPPRESSED;
		case swComponentLightweight:
		case swComponentFullyLightweight:
			return sld_mcp::v1::COMPONENT_STATE_LIGHTWEIGHT;
		default:
			return sld_mcp::v1::COMPONENT_STATE_RESOLVED;
		}
	}

	void DescribeComponent(IComponent2* component, PersistIds& ids, IMathUtility* math,
		sld_mcp::v1::ComponentInfo* info)
	{
		info->set_persist_id(ids.Get(component));
		info->set_name(ComponentName(component));

		ATL::CComBSTR path;
		if (SUCCEEDED(component->GetPathName(&path)))
			info->set_file(ToUtf8(path));
		ATL::CComBSTR configuration;
		if (SUCCEEDED(component->get_ReferencedConfiguration(&configuration)))
			info->set_configuration(ToUtf8(configuration));

		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(component->IsFixed(&flag)))
			info->set_fixed(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(component->IsHidden(VARIANT_FALSE, &flag)))
			info->set_hidden(flag != VARIANT_FALSE);
		long state = swComponentFullyResolved;
		if (SUCCEEDED(component->GetSuppression2(&state)))
			info->set_state(ToComponentState(state));
		info->set_subassembly(IsSubassembly(component));

		ATL::CComPtr<IComponent2> parent;
		if (SUCCEEDED(component->GetParent(&parent)) && parent)
			info->set_parent_persist_id(ids.Get(parent));

		ATL::CComVariant box;
		if (SUCCEEDED(component->GetBox(VARIANT_FALSE, VARIANT_FALSE, &box)))
			SetBox(info->mutable_box(), comvariant::ToDoubles(box));

		if (state != swComponentSuppressed)
			DescribePlacement(component, math, info);
	}

	// The components listed in one reply, each described once.
	class ComponentList
	{
	public:
		ComponentList(PersistIds& ids, IMathUtility* math,
			google::protobuf::RepeatedPtrField<sld_mcp::v1::ComponentInfo>* out)
			: m_ids(ids), m_math(math), m_out(out) {}

		// The component's persist id; describes it the first time. Empty for null.
		std::string Add(IComponent2* component)
		{
			if (!component)
				return {};
			std::string pid = m_ids.Get(component);
			if (!pid.empty() && m_listed.insert(pid).second)
				DescribeComponent(component, m_ids, m_math, m_out->Add());
			return pid;
		}

	private:
		PersistIds& m_ids;
		IMathUtility* m_math;
		google::protobuf::RepeatedPtrField<sld_mcp::v1::ComponentInfo>* m_out;
		std::set<std::string> m_listed;
	};

	// ---- Mates ----------------------------------------------------------------------

	sld_mcp::v1::MateType ToMateType(long type)
	{
		switch (type)
		{
		case swMateCOINCIDENT:    return sld_mcp::v1::MATE_TYPE_COINCIDENT;
		case swMateCONCENTRIC:    return sld_mcp::v1::MATE_TYPE_CONCENTRIC;
		case swMatePERPENDICULAR: return sld_mcp::v1::MATE_TYPE_PERPENDICULAR;
		case swMatePARALLEL:      return sld_mcp::v1::MATE_TYPE_PARALLEL;
		case swMateTANGENT:       return sld_mcp::v1::MATE_TYPE_TANGENT;
		case swMateDISTANCE:      return sld_mcp::v1::MATE_TYPE_DISTANCE;
		case swMateANGLE:         return sld_mcp::v1::MATE_TYPE_ANGLE;
		case swMateLOCK:          return sld_mcp::v1::MATE_TYPE_LOCK;
		default:                  return sld_mcp::v1::MATE_TYPE_OTHER;
		}
	}

	bool ToSwMateType(sld_mcp::v1::MateType type, long& out)
	{
		switch (type)
		{
		case sld_mcp::v1::MATE_TYPE_COINCIDENT:    out = swMateCOINCIDENT; return true;
		case sld_mcp::v1::MATE_TYPE_CONCENTRIC:    out = swMateCONCENTRIC; return true;
		case sld_mcp::v1::MATE_TYPE_PERPENDICULAR: out = swMatePERPENDICULAR; return true;
		case sld_mcp::v1::MATE_TYPE_PARALLEL:      out = swMatePARALLEL; return true;
		case sld_mcp::v1::MATE_TYPE_TANGENT:       out = swMateTANGENT; return true;
		case sld_mcp::v1::MATE_TYPE_DISTANCE:      out = swMateDISTANCE; return true;
		case sld_mcp::v1::MATE_TYPE_ANGLE:         out = swMateANGLE; return true;
		case sld_mcp::v1::MATE_TYPE_LOCK:          out = swMateLOCK; return true;
		default:                                   return false;
		}
	}

	const char* MateTypeName(long type)
	{
		switch (type)
		{
		case swMateCOINCIDENT:    return "coincident";
		case swMateCONCENTRIC:    return "concentric";
		case swMatePERPENDICULAR: return "perpendicular";
		case swMatePARALLEL:      return "parallel";
		case swMateTANGENT:       return "tangent";
		case swMateDISTANCE:      return "distance";
		case swMateANGLE:         return "angle";
		case swMateLOCK:          return "lock";
		default:                  return "mate";
		}
	}

	sld_mcp::v1::MateAlignment ToMateAlignment(long align)
	{
		switch (align)
		{
		case swMateAlignALIGNED:      return sld_mcp::v1::MATE_ALIGNMENT_ALIGNED;
		case swMateAlignANTI_ALIGNED: return sld_mcp::v1::MATE_ALIGNMENT_ANTI_ALIGNED;
		case swMateAlignCLOSEST:      return sld_mcp::v1::MATE_ALIGNMENT_CLOSEST;
		default:                      return sld_mcp::v1::MATE_ALIGNMENT_UNSPECIFIED;
		}
	}

	long ToSwMateAlign(sld_mcp::v1::MateAlignment align)
	{
		switch (align)
		{
		case sld_mcp::v1::MATE_ALIGNMENT_ALIGNED:      return swMateAlignALIGNED;
		case sld_mcp::v1::MATE_ALIGNMENT_ANTI_ALIGNED: return swMateAlignANTI_ALIGNED;
		default:                                       return swMateAlignCLOSEST;
		}
	}

	// Mates live under the assembly's "MateGroup" feature, possibly inside
	// folders; anything whose specific feature is an IMate2 is one.
	constexpr size_t kMaxMates = 5000;

	void CollectMates(IFeature* parent, int depth, std::vector<ATL::CComPtr<IFeature>>& mates,
		std::set<std::string>& seen)
	{
		ATL::CComPtr<IFeature> sub;
		if (FAILED(parent->IGetFirstSubFeature(&sub)))
			return;
		while (sub && mates.size() < kMaxMates)
		{
			ATL::CComPtr<IDispatch> specific;
			if (SUCCEEDED(sub->GetSpecificFeature2(&specific)) && specific &&
				ATL::CComQIPtr<IMate2>(specific.p))
			{
				if (seen.insert(FeatureName(sub)).second)
					mates.push_back(sub);
			}
			else if (depth < 3)
				CollectMates(sub, depth + 1, mates, seen);

			ATL::CComPtr<IFeature> next;
			if (FAILED(sub->IGetNextSubFeature(&next)))
				break;
			sub = next;
		}
	}

	std::vector<ATL::CComPtr<IFeature>> MateFeatures(IModelDoc2* doc)
	{
		std::vector<ATL::CComPtr<IFeature>> mates;
		std::set<std::string> seen;
		ATL::CComPtr<IFeature> feature;
		if (FAILED(doc->IFirstFeature(&feature)))
			return mates;
		while (feature)
		{
			ATL::CComBSTR typeName;
			if (SUCCEEDED(feature->GetTypeName2(&typeName)) && typeName == L"MateGroup")
				CollectMates(feature, 0, mates, seen);
			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				break;
			feature = next;
		}
		return mates;
	}

	std::set<std::string> MateNames(IModelDoc2* doc)
	{
		std::set<std::string> names;
		for (const auto& mate : MateFeatures(doc))
			names.insert(FeatureName(mate));
		return names;
	}

	void DescribeMate(IFeature* feature, PersistIds& ids, sld_mcp::v1::MateInfo* info)
	{
		info->set_persist_id(ids.Get(feature));
		info->set_name(FeatureName(feature));
		ATL::CComBSTR typeName;
		if (SUCCEEDED(feature->GetTypeName2(&typeName)))
			info->set_type_name(ToUtf8(typeName));
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(feature->IsSuppressed(&flag)))
			info->set_suppressed(flag != VARIANT_FALSE);
		VARIANT_BOOL warning = VARIANT_FALSE;
		long code = swFeatureErrorNone;
		if (SUCCEEDED(feature->GetErrorCode2(&warning, &code)))
		{
			info->set_error_code(static_cast<int32_t>(code));
			info->set_error_is_warning(code != swFeatureErrorNone && warning != VARIANT_FALSE);
		}

		ATL::CComPtr<IDispatch> specific;
		if (SUCCEEDED(feature->GetSpecificFeature2(&specific)) && specific)
		{
			if (ATL::CComQIPtr<IMate2> mate(specific.p); mate)
			{
				long value = 0;
				if (SUCCEEDED(mate->get_Type(&value)))
					info->set_type(ToMateType(value));
				if (SUCCEEDED(mate->get_Alignment(&value)))
					info->set_alignment(ToMateAlignment(value));
				flag = VARIANT_FALSE;
				if (SUCCEEDED(mate->get_Flipped(&flag)))
					info->set_flipped(flag != VARIANT_FALSE);

				long count = 0;
				std::set<std::string> components;
				if (SUCCEEDED(mate->GetMateEntityCount(&count)))
					for (long i = 0; i < count; ++i)
					{
						ATL::CComPtr<IMateEntity2> entity;
						if (FAILED(mate->MateEntity(i, &entity)) || !entity)
							continue;
						ATL::CComPtr<IComponent2> component;
						entity->get_ReferenceComponent(&component);
						const std::string pid = component ? ids.Get(component) : std::string();
						if (!components.insert(pid).second)
							continue;
						info->add_component_persist_ids(pid);
						info->add_component_names(ComponentName(component));
					}
			}
		}

		// Distance and angle mates carry their value as a dimension.
		for (const auto& display : FeatureDimensions(feature))
			if (DescribeDimension(display, info->mutable_dimension()))
				break;
	}

	std::string MateErrorText(long code)
	{
		switch (code)
		{
		case swFeatureErrorMateOverdefined:
			return "it over-defines the assembly: it conflicts with existing mates or fixed components";
		case swFeatureErrorMateIlldefined:
			return "it can't be solved (ill-defined) from the components' positions";
		case swFeatureErrorMateInvalidEdge:
		case swFeatureErrorMateInvalidFace:
		case swFeatureErrorMateInvalidEntity:
		case swFeatureErrorMateEntityNotLinear:
			return "that kind of mate doesn't work with those entities";
		case swFeatureErrorMateUnknownTangent:
			return "SolidWorks couldn't work out the tangency";
		case swFeatureErrorMateDanglingGeometry:
		case swFeatureErrorMateBroken:
			return "it refers to geometry that's missing";
		default:
			return "SolidWorks reported error " + std::to_string(code);
		}
	}

	std::string AddMateErrorText(long error, long type)
	{
		const std::string kind = MateTypeName(type);
		switch (error)
		{
		case swAddMateError_IncorrectSelections:
			return "SolidWorks can't make a " + kind + " mate between those two entities -- e.g. "
				"concentric needs round faces/edges or axes, tangent needs a curved face, lock needs "
				"two components";
		case swAddMateError_IncorrectAlignment:
			return "that alignment isn't possible for these entities -- try alignment \"closest\"";
		case swAddMateError_OverDefinedAssembly:
			return "the " + kind + " mate would over-define the assembly: it conflicts with existing "
				"mates or fixed components";
		case swAddMateError_IncorrectMateType:
			return "SolidWorks rejected the mate type for these entities";
		default:
			return "SolidWorks couldn't add the " + kind + " mate (error " + std::to_string(error) + ")";
		}
	}

	// Deletes a feature (e.g. a mate that failed). Best effort.
	void DeleteFeature(IModelDoc2* doc, IFeature* feature)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (feature && SUCCEEDED(feature->Select2(VARIANT_FALSE, 0, &selected)) && selected)
			doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
	}

	// One side of a mate, resolved.
	struct MateRef
	{
		ATL::CComPtr<IDispatch> obj;
		ATL::CComPtr<IComponent2> component;  // null: the assembly's own (or unknown)
		bool isComponent = false;
		bool isFeature = false;
		std::string what;  // for messages: "face", "Front Plane", "bracket-1", ...
	};

	// The component a reference plane/axis feature belongs to. Features may
	// not answer IEntity::GetComponent, so failing that, look for a feature
	// of the same name in each component and compare persist ids. Null for
	// the assembly's own features.
	ATL::CComPtr<IComponent2> ComponentOfFeature(IAssemblyDoc* assembly, IFeature* feature, PersistIds& ids)
	{
		if (ATL::CComPtr<IComponent2> component = ComponentOf(feature); component)
			return component;
		const std::string pid = ids.Get(feature);
		const ATL::CComBSTR name = FromUtf8(FeatureName(feature));
		if (pid.empty())
			return {};
		for (const auto& component : AllComponents(assembly))
		{
			ATL::CComPtr<IFeature> candidate;
			if (SUCCEEDED(component->FeatureByName(name, &candidate)) && candidate && ids.Get(candidate) == pid)
				return component;
		}
		return {};
	}

	grpc::Status ResolveMateRef(IModelDocExtension* ext, IAssemblyDoc* assembly, PersistIds& ids,
		const std::string& persistId, long mateType, MateRef& ref)
	{
		if (grpc::Status status = ResolvePersistId(ext, persistId, ref.obj); !status.ok())
			return status;
		const bool lock = mateType == swMateLOCK;

		if (ATL::CComQIPtr<IComponent2> component(ref.obj.p); component)
		{
			if (!lock)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + ComponentName(component) + "\" is a whole component; only lock mates take "
					"components -- give a face, edge or reference plane/axis");
			ref.component = component;
			ref.isComponent = true;
			ref.what = ComponentName(component);
			return grpc::Status::OK;
		}
		if (lock)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a lock mate takes two components, not faces or other geometry");

		if (ATL::CComQIPtr<IFace2>(ref.obj.p))
			ref.what = "face";
		else if (ATL::CComQIPtr<IEdge>(ref.obj.p))
			ref.what = "edge";
		else if (ATL::CComQIPtr<IVertex>(ref.obj.p))
			ref.what = "vertex";
		else if (ATL::CComQIPtr<IFeature> feature(ref.obj.p); feature)
		{
			ATL::CComBSTR typeName;
			feature->GetTypeName2(&typeName);
			if (typeName != L"RefPlane" && typeName != L"RefAxis" && typeName != L"RefPoint" &&
				typeName != L"CoordSys")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + FeatureName(feature) + "\" (" + ToUtf8(typeName) + ") can't be mated -- give "
					"a face, an edge, or a reference plane / axis / point");
			ref.isFeature = true;
			ref.what = FeatureName(feature);
		}
		else
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that id isn't something a mate can use -- give a face, an edge, or a reference "
				"plane / axis / point");

		if (ref.isFeature)
		{
			ATL::CComQIPtr<IFeature> feature(ref.obj.p);
			ref.component = ComponentOfFeature(assembly, feature, ids);
		}
		else
			ref.component = ComponentOf(ref.obj);
		if (!ref.component && !ref.isFeature)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that " + ref.what + " isn't on a component of the active assembly");
		if (ref.component && IsSuppressed(ref.component))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + ComponentName(ref.component) + "\" is suppressed -- unsuppress it first");
		return grpc::Status::OK;
	}

	// Appends a mate ref to the selection with mark 1, as AddMate5 expects.
	bool SelectMateRef(const MateRef& ref, ISelectData* data)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (ref.isComponent)
			return SUCCEEDED(ref.component->Select4(VARIANT_TRUE, data, VARIANT_FALSE, &selected)) && selected;
		if (ref.isFeature)
		{
			ATL::CComQIPtr<IFeature> feature(ref.obj.p);
			return feature && SUCCEEDED(feature->Select2(VARIANT_TRUE, 1, &selected)) && selected;
		}
		ATL::CComQIPtr<IEntity> entity(ref.obj.p);
		return entity && SUCCEEDED(entity->Select4(VARIANT_TRUE, data, &selected)) && selected;
	}

	// Whether `component` is `ancestor` or inside it.
	bool IsWithin(IComponent2* component, const std::string& ancestorPid, PersistIds& ids)
	{
		ATL::CComPtr<IComponent2> current(component);
		for (int depth = 0; current && depth < 64; ++depth)
		{
			if (ids.Get(current) == ancestorPid)
				return true;
			ATL::CComPtr<IComponent2> parent;
			if (FAILED(current->GetParent(&parent)))
				break;
			current = parent;
		}
		return false;
	}

	// InspectModel for an assembly; see the .proto for what's listed.
	grpc::Status InspectAssembly(IModelDoc2* doc, IModelDocExtension* ext, PersistIds& ids, IMathUtility* math,
		const sld_mcp::v1::InspectModelRequest& request, sld_mcp::v1::InspectModelResponse* response)
	{
		if (!request.scope_feature_name().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"in an assembly, narrow by a component id rather than a feature name (feature names "
				"repeat across components)");
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		const bool wantGeometry = request.include_faces() || request.include_edges();
		ComponentList components(ids, math, response->mutable_components());

		if (request.include_components())
			for (const auto& component : AllComponents(assembly))
				components.Add(component);
		if (request.include_mates())
			for (const auto& mate : MateFeatures(doc))
				DescribeMate(mate, ids, response->add_mates());

		// A component scope narrows everything; any other scope (body,
		// feature, face) only faces/edges, as in a part.
		ATL::CComPtr<IComponent2> scopeComponent;
		ATL::CComPtr<IDispatch> scope;
		if (!request.scope_persist_id().empty())
		{
			if (grpc::Status status = ResolvePersistId(ext, request.scope_persist_id(), scope); !status.ok())
				return status;
			if (ATL::CComQIPtr<IComponent2> component(scope.p); component)
			{
				scopeComponent = component;
				scope.Release();
			}
		}

		if (request.include_features())
		{
			if (scopeComponent)
			{
				ATL::CComPtr<IFeature> first;
				HRESULT hr = scopeComponent->FirstFeature(&first);
				if (FAILED(hr))
					return ComError("IComponent2::FirstFeature", hr);
				const std::string pid = components.Add(scopeComponent);
				if (grpc::Status status = InspectFeatureList(first, ids, pid, false, response); !status.ok())
					return status;
			}
			else if (grpc::Status status = InspectFeatures(doc, ids, true, response); !status.ok())
				return status;
		}

		// Bodies with their components: for the body list, and as the
		// default face/edge scope. Next to the component list (the overview)
		// every component's bodies would mostly repeat it, so they're only
		// listed on their own or for one component.
		const bool listBodies = request.include_bodies() && (scopeComponent || !request.include_components());
		std::vector<std::pair<ATL::CComPtr<IBody2>, ATL::CComPtr<IComponent2>>> bodies;
		if (listBodies || (wantGeometry && !scope))
		{
			std::vector<ATL::CComPtr<IComponent2>> owners;
			if (scopeComponent && !IsSubassembly(scopeComponent))
				owners.push_back(scopeComponent);
			else
			{
				const std::string scopePid = scopeComponent ? ids.Get(scopeComponent) : std::string();
				for (const auto& component : GeometryComponents(assembly, false))
					if (scopePid.empty() || IsWithin(component, scopePid, ids))
						owners.push_back(component);
			}
			for (const auto& owner : owners)
				for (const auto& body : ComponentBodies(owner))
					bodies.emplace_back(body, owner);
		}
		if (listBodies)
			for (const auto& [body, owner] : bodies)
			{
				sld_mcp::v1::BodyInfo* info = response->add_bodies();
				InspectBody(body, ids, info);
				info->set_component_persist_id(components.Add(owner));
			}

		if (!wantGeometry)
			return grpc::Status::OK;

		EntitySet<IFace2> faces;
		EntitySet<IEdge> edges;
		auto addBody = [&](IBody2* body) {
			ATL::CComVariant array;
			if (request.include_faces() && SUCCEEDED(body->GetFaces(&array)))
				faces.AddAll(array);
			array.Clear();
			if (request.include_edges() && SUCCEEDED(body->GetEdges(&array)))
				edges.AddAll(array);
		};
		auto addFace = [&](IFace2* face) {
			if (request.include_faces())
				faces.Add(face);
			ATL::CComVariant faceEdges;
			if (request.include_edges() && SUCCEEDED(face->GetEdges(&faceEdges)))
				edges.AddAll(faceEdges);
		};

		if (scope)
		{
			if (ATL::CComQIPtr<IBody2> body(scope.p); body)
				addBody(body);
			else if (ATL::CComQIPtr<IFace2> face(scope.p); face)
				addFace(face);
			else if (ATL::CComQIPtr<IFeature> feature(scope.p); feature)
			{
				ATL::CComVariant array;
				if (SUCCEEDED(feature->GetFaces(&array)))
					for (const auto& disp : comvariant::ToDispatches(array))
						if (ATL::CComQIPtr<IFace2> face(disp.p); face)
							addFace(face);
			}
			else
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the scope must be a component, a body, a feature or a face");
		}
		else
		{
			for (const auto& [body, owner] : bodies)
				addBody(body);
		}

		for (const auto& face : faces.Items())
		{
			sld_mcp::v1::FaceInfo* info = response->add_faces();
			InspectFace(face, ids, info);
			info->set_component_persist_id(components.Add(ComponentOf(face)));
		}
		for (const auto& edge : edges.Items())
		{
			sld_mcp::v1::EdgeInfo* info = response->add_edges();
			InspectEdge(edge, ids, info);
			info->set_component_persist_id(components.Add(ComponentOf(edge)));
		}
		response->set_truncated(faces.Truncated() || edges.Truncated());
		return grpc::Status::OK;
	}

	// ---- Annotations (PMI / MBD) ------------------------------------------------

	constexpr size_t kMaxAnnotations = 5000;
	constexpr int kMaxAttachedPerAnnotation = 200;
	constexpr long kMaxAnnotationTexts = 50;
	constexpr short kMaxGtolFrames = 10;

	// Late-bound call of a property or argument-less method by name. Used for
	// DimXpert objects (IDimXpertFeature, ...), whose interfaces live in
	// swdimxpert.tlb, which the add-in doesn't #import.
	bool InvokeByName(IDispatch* obj, const wchar_t* name, ATL::CComVariant& result)
	{
		result.Clear();
		if (!obj)
			return false;
		DISPID id = DISPID_UNKNOWN;
		LPOLESTR names[] = { const_cast<LPOLESTR>(name) };
		if (FAILED(obj->GetIDsOfNames(IID_NULL, names, 1, LOCALE_USER_DEFAULT, &id)))
			return false;
		DISPPARAMS none = { nullptr, nullptr, 0, 0 };
		return SUCCEEDED(obj->Invoke(id, IID_NULL, LOCALE_USER_DEFAULT,
			DISPATCH_METHOD | DISPATCH_PROPERTYGET, &none, &result, nullptr, nullptr));
	}

	// Every annotation of the document, once each. IModelDocExtension::
	// GetAnnotations should have them all; the annotation views' lists and
	// the GetFirstAnnotation2 chain are merged in too in case it leaves some
	// out (it isn't documented whether DimXpert's or hidden ones are in it).
	std::vector<ATL::CComPtr<IAnnotation>> AllAnnotations(IModelDoc2* doc, IModelDocExtension* ext,
		bool& truncated)
	{
		std::vector<ATL::CComPtr<IAnnotation>> out;
		std::set<IUnknown*> seen;  // identities; `alive` keeps them from being reused
		std::vector<ATL::CComPtr<IUnknown>> alive;
		auto add = [&](IUnknown* obj) {
			ATL::CComQIPtr<IAnnotation> annotation(obj);
			if (!annotation)
				return;
			ATL::CComPtr<IUnknown> identity;
			if (FAILED(annotation->QueryInterface(IID_PPV_ARGS(&identity))) || !identity)
				return;
			if (!seen.insert(identity.p).second)
				return;
			alive.push_back(identity);
			if (out.size() >= kMaxAnnotations)
			{
				truncated = true;
				return;
			}
			out.push_back(ATL::CComPtr<IAnnotation>(annotation));
		};

		ATL::CComVariant all;
		if (SUCCEEDED(ext->GetAnnotations(&all)))
			for (const auto& disp : comvariant::ToDispatches(all))
				add(disp);

		ATL::CComVariant views;
		if (SUCCEEDED(ext->get_AnnotationViews(&views)))
			for (const auto& disp : comvariant::ToDispatches(views))
			{
				ATL::CComQIPtr<IAnnotationView> view(disp.p);
				ATL::CComVariant annotations;
				if (view && SUCCEEDED(view->get_Annotations(&annotations)))
					for (const auto& a : comvariant::ToDispatches(annotations))
						add(a);
			}

		ATL::CComPtr<IAnnotation> annotation;
		if (SUCCEEDED(doc->IGetFirstAnnotation2(&annotation)))
		{
			// Bounded in case the chain ever loops.
			for (size_t steps = 0; annotation && steps < 4 * kMaxAnnotations; ++steps)
			{
				add(annotation);
				ATL::CComPtr<IAnnotation> next;
				if (FAILED(annotation->GetNext3(&next)))
					break;
				annotation = next;
			}
		}
		return out;
	}

	void AddText(const ATL::CComBSTR& text, sld_mcp::v1::AnnotationInfo* info)
	{
		if (text.Length() > 0)
			info->add_text(ToUtf8(text));
	}

	// The pieces of text of the annotation types that list them by index
	// (datum targets, feature control frames, surface finish symbols, ...).
	template <typename T>
	void AddIndexedTexts(T* annotation, sld_mcp::v1::AnnotationInfo* info)
	{
		long count = 0;
		if (FAILED(annotation->GetTextCount(&count)))
			return;
		for (long i = 0; i < count && i < kMaxAnnotationTexts; ++i)
		{
			ATL::CComBSTR text;
			if (SUCCEEDED(annotation->GetTextAtIndex(i, &text)))
				AddText(text, info);
		}
	}

	// Classifies an entity an annotation is attached to and references it.
	void DescribeAttached(IDispatch* obj, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AttachedEntity* out)
	{
		auto kind = sld_mcp::v1::ATTACHED_KIND_OTHER;
		if (ATL::CComQIPtr<IFace2> face(obj); face)
			kind = sld_mcp::v1::ATTACHED_KIND_FACE;
		else if (ATL::CComQIPtr<IEdge> edge(obj); edge)
			kind = sld_mcp::v1::ATTACHED_KIND_EDGE;
		else if (ATL::CComQIPtr<IVertex> vertex(obj); vertex)
			kind = sld_mcp::v1::ATTACHED_KIND_VERTEX;
		else if (ATL::CComQIPtr<IFeature> feature(obj); feature)
			kind = sld_mcp::v1::ATTACHED_KIND_FEATURE;
		out->set_kind(kind);
		if (kind != sld_mcp::v1::ATTACHED_KIND_OTHER)
			out->set_persist_id(ids.Get(obj));
		if (components)
			out->set_component_persist_id(components->Add(ComponentOf(obj)));
	}

	void DescribeAnnotationDimension(IDisplayDimension* display, sld_mcp::v1::AnnotationDimension* out)
	{
		auto text = [&](long which) {
			ATL::CComBSTR s;
			display->GetText(which, &s);
			return ToUtf8(s);
		};
		out->set_prefix(text(swDimensionTextPrefix));
		out->set_suffix(text(swDimensionTextSuffix));
		out->set_callout_above(text(swDimensionTextCalloutAbove));
		out->set_callout_below(text(swDimensionTextCalloutBelow));

		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(display->IsHoleCallout(&flag)))
			out->set_hole_callout(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(display->get_Inspection(&flag)))
			out->set_inspection(flag != VARIANT_FALSE);

		ATL::CComPtr<IDimension> dim;
		if (FAILED(display->GetDimension2(0, &dim)) || !dim)
			return;
		DescribeDimension(display, out->mutable_dimension());
		flag = VARIANT_FALSE;
		if (SUCCEEDED(dim->IsReference(&flag)))
			out->set_reference(flag != VARIANT_FALSE);

		ATL::CComPtr<IDimensionTolerance> tolerance;
		if (SUCCEEDED(dim->get_Tolerance(&tolerance)) && tolerance)
		{
			long type = swTolNONE;
			if (SUCCEEDED(tolerance->get_Type(&type)))
				out->set_tolerance_type(static_cast<int32_t>(type));
			double value = 0.0;
			if (SUCCEEDED(tolerance->GetMaxValue(&value)) && std::isfinite(value))
				out->set_tolerance_max(value);
			value = 0.0;
			if (SUCCEEDED(tolerance->GetMinValue(&value)) && std::isfinite(value))
				out->set_tolerance_min(value);
			ATL::CComBSTR fit;
			if (SUCCEEDED(tolerance->GetHoleFitValue(&fit)))
				out->set_hole_fit(ToUtf8(fit));
			fit.Empty();
			if (SUCCEEDED(tolerance->GetShaftFitValue(&fit)))
				out->set_shaft_fit(ToUtf8(fit));
		}

		ATL::CComPtr<IFeature> owner;
		if (SUCCEEDED(dim->GetFeatureOwner(&owner)) && owner)
		{
			out->set_owner_feature(FeatureName(owner));
			ATL::CComBSTR typeName;
			if (SUCCEEDED(owner->GetTypeName2(&typeName)))
				out->set_owner_feature_type(ToUtf8(typeName));
		}
	}

	void DescribeGtol(IGtol* gtol, sld_mcp::v1::AnnotationGtol* out)
	{
		// Reads one row of the frame; false if there's nothing in it.
		auto readFrame = [&](short frame) {
			ATL::CComVariant valuesVar, symbolsVar;
			std::vector<ATL::CComBSTR> values;
			std::vector<long long> symbols;
			if (SUCCEEDED(gtol->GetFrameValues(frame, &valuesVar)))
				values = comvariant::ToBstrs(valuesVar);
			if (SUCCEEDED(gtol->GetFrameSymbols(frame, &symbolsVar)))
				symbols = comvariant::ToInt64s(symbolsVar);
			const bool anyValue = std::any_of(values.begin(), values.end(),
				[](const ATL::CComBSTR& s) { return s.Length() > 0; });
			const bool anySymbol = !symbols.empty() && symbols[0] != 0 && symbols[0] != swGcsNONE;
			if (!anyValue && !anySymbol)
				return false;

			// Same order as IGtol::SetFrameValues / SetFrameSymbols:
			// values = tol1, tol2, datum1..3; symbols = GCS, tol1 diameter,
			// tol1 MC, tol2 diameter, tol2 MC, datum1..3 MC.
			auto value = [&](size_t i) { return i < values.size() ? ToUtf8(values[i]) : std::string(); };
			auto symbol = [&](size_t i, long long fallback) { return i < symbols.size() ? symbols[i] : fallback; };
			sld_mcp::v1::GtolFrame* f = out->add_frames();
			f->set_symbol(static_cast<int32_t>(symbol(0, -1)));
			f->set_tolerance1(value(0));
			f->set_tolerance2(value(1));
			f->set_tolerance1_diameter(symbol(1, 0) != 0);
			f->set_tolerance1_condition(static_cast<int32_t>(symbol(2, 0)));
			f->set_tolerance2_diameter(symbol(3, 0) != 0);
			f->set_tolerance2_condition(static_cast<int32_t>(symbol(4, 0)));
			for (size_t i = 0; i < 3; ++i)
			{
				f->add_datums(value(2 + i));
				f->add_datum_conditions(static_cast<int32_t>(symbol(5 + i, 0)));
			}
			return true;
		};

		short count = 0;
		if (FAILED(gtol->GetFrameCount(&count)) || count <= 0)
			count = 2;  // unknown: a frame has at most two rows in the classic API
		bool any = false;
		for (short frame = 1; frame <= count && frame <= kMaxGtolFrames; ++frame)
			any = readFrame(frame) || any;
		// Frame numbers are documented as 1-based; try 0 in case they aren't.
		if (!any)
			readFrame(0);

		ATL::CComBSTR datum;
		if (SUCCEEDED(gtol->GetDatumIdentifier(&datum)))
			out->set_datum_label(ToUtf8(datum));
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(gtol->GetCompositeFrame(&flag)))
			out->set_composite(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(gtol->GetAllAround(&flag)))
			out->set_all_around(flag != VARIANT_FALSE);
		ATL::CComBSTR ptz;
		if (SUCCEEDED(gtol->GetPTZHeight(&ptz)))
			out->set_projected_zone_height(ToUtf8(ptz));
		long format = 0;
		if (SUCCEEDED(gtol->GetFormat(&format)))
			out->set_format(static_cast<int32_t>(format));
	}

	void DescribeAnnotation(IAnnotation* annotation, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AnnotationInfo* info)
	{
		ATL::CComBSTR name;
		if (SUCCEEDED(annotation->GetName(&name)))
			info->set_name(ToUtf8(name));
		long type = 0;
		if (SUCCEEDED(annotation->GetType(&type)))
			info->set_type(static_cast<int32_t>(type));
		long visibility = swAnnotationVisibilityUnknown;
		if (SUCCEEDED(annotation->get_Visible(&visibility)))
			info->set_visibility(static_cast<int32_t>(visibility));
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(annotation->IsDangling(&flag)))
			info->set_dangling(flag != VARIANT_FALSE);
		long pmiType = 0;
		if (SUCCEEDED(annotation->GetPMIType(&pmiType)))
			info->set_pmi_type(static_cast<int32_t>(pmiType));

		ATL::CComVariant positionVar;
		if (SUCCEEDED(annotation->GetPosition(&positionVar)))
			if (const std::vector<double> p = comvariant::ToDoubles(positionVar);
				p.size() >= 3 && std::isfinite(p[0]) && std::isfinite(p[1]) && std::isfinite(p[2]))
				SetVec3(info->mutable_position(), p.data());

		// Attached geometry, each entity once.
		std::set<std::string> attachedIds;
		auto attach = [&](IDispatch* obj) {
			if (!obj || info->attached_size() >= kMaxAttachedPerAnnotation)
				return;
			sld_mcp::v1::AttachedEntity entity;
			DescribeAttached(obj, ids, components, &entity);
			if (!entity.persist_id().empty() && !attachedIds.insert(entity.persist_id()).second)
				return;
			*info->add_attached() = std::move(entity);
		};
		ATL::CComVariant attachedVar;
		if (SUCCEEDED(annotation->GetAttachedEntities3(&attachedVar)))
			for (const auto& disp : comvariant::ToDispatches(attachedVar))
				attach(disp);

		flag = VARIANT_FALSE;
		if (SUCCEEDED(annotation->IsDimXpert(&flag)) && flag != VARIANT_FALSE)
		{
			info->set_dimxpert(true);
			ATL::CComBSTR dimXpertName;
			if (SUCCEEDED(annotation->GetDimXpertName(&dimXpertName)))
				info->set_dimxpert_name(ToUtf8(dimXpertName));
			ATL::CComPtr<IDispatch> feature;
			if (SUCCEEDED(annotation->GetDimXpertFeature(&feature)) && feature)
			{
				ATL::CComVariant result;
				if (InvokeByName(feature, L"Name", result) && result.vt == VT_BSTR)
					info->set_dimxpert_feature(ToUtf8(ATL::CComBSTR(result.bstrVal)));
				// DimXpert annotations may report no attachments; their
				// feature knows its faces.
				if (info->attached_size() == 0 && InvokeByName(feature, L"GetFaces", result))
					for (const auto& disp : comvariant::ToDispatches(result))
						attach(disp);
			}
		}

		ATL::CComPtr<IDispatch> specific;
		if (FAILED(annotation->GetSpecificAnnotation(&specific)) || !specific)
			return;
		switch (type)
		{
		case swDisplayDimension:
			if (ATL::CComQIPtr<IDisplayDimension> display(specific.p); display)
			{
				DescribeAnnotationDimension(display, info->mutable_dimension());
				ATL::CComBSTR all;
				if (SUCCEEDED(display->GetText(swDimensionTextAll, &all)))
					AddText(all, info);
			}
			break;
		case swGTol:
			if (ATL::CComQIPtr<IGtol> gtol(specific.p); gtol)
			{
				DescribeGtol(gtol, info->mutable_gtol());
				AddIndexedTexts(gtol.p, info);
			}
			break;
		case swDatumTag:
			if (ATL::CComQIPtr<IDatumTag> datum(specific.p); datum)
			{
				ATL::CComBSTR label;
				if (SUCCEEDED(datum->GetLabel(&label)))
					info->set_datum_label(ToUtf8(label));
			}
			break;
		case swDatumTargetSym:
			if (ATL::CComQIPtr<IDatumTargetSym> target(specific.p); target)
				AddIndexedTexts(target.p, info);
			break;
		case swSFSymbol:
			if (ATL::CComQIPtr<ISFSymbol> finish(specific.p); finish)
				AddIndexedTexts(finish.p, info);
			break;
		case swNote:
			if (ATL::CComQIPtr<INote> note(specific.p); note)
			{
				ATL::CComBSTR text;
				if (SUCCEEDED(note->GetText(&text)))
					AddText(text, info);
			}
			break;
		default:
			break;
		}
	}
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
	if (!onEntity && !StandardPlaneIndex(request->plane(), planeIndex))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
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
	SketchTarget target;
	if (grpc::Status status = ResolveSketchTarget(doc, request->on_persist_id(), planeIndex, target); !status.ok())
		return status;

	// On a face, default to the middle of the face (well, of its bounding box).
	HRESULT hr = S_OK;
	if (target.face && !haveModelCenter)
	{
		ATL::CComVariant box;
		hr = target.face->GetBox(&box);
		if (FAILED(hr))
			return ComError("IFace2::GetBox", hr);
		const std::vector<double> b = comvariant::ToDoubles(box);
		if (b.size() < 6)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no box for the face");
		for (int i = 0; i < 3; ++i)
			modelCenter[i] = (b[i] + b[i + 3]) / 2.0;
		haveModelCenter = true;
	}

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<ISketchManager> sketchManager;
	hr = doc->get_SketchManager(&sketchManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::SketchManager", hr);
	if (!sketchManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenNewSketch(doc, sketchManager, target, sketch); !status.ok())
		return status;

	// The sketch is now open: everything below must fall through to the
	// InsertSketch call that closes it, so no early returns until then.
	grpc::Status failure = grpc::Status::OK;
	space.Attach(sketch);

	if (haveModelCenter)
	{
		// Dropping the sketch Z (the distance from the plane) projects the
		// point onto the sketch plane.
		double sketchCenter[3];
		if (space.ToSketch(modelCenter, sketchCenter))
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
		// The sides (and any construction geometry), with IDs for
		// dimensioning; stamped with the sketch's persist ID once it's closed.
		for (const auto& segment : comvariant::ToDispatches(segments))
		{
			sld_mcp::v1::SketchItem item;
			if (segment && DescribeSketchItem(segment, std::string(), &item))
				response->add_items()->Swap(&item);
		}

		sld_mcp::v1::SketchFrame frame;
		const double sketchCenter[3] = {cx, cy, 0.0};
		double center[3];
		if (space.Frame(frame) && space.ToModel(sketchCenter, center))
		{
			*response->mutable_frame() = frame;
			SetVec3(response->mutable_center_model(), center);
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
		if (SUCCEEDED(closeHr))
			DeleteSketchFeature(doc, sketchFeature);
		return failure;
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (auto& item : *response->mutable_items())
		StampSketchId(&item, sketchPid);
	response->set_sketch_name(ToUtf8(sketchName));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddSketchEntities(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddSketchEntitiesRequest* request,
	sld_mcp::v1::AddSketchEntitiesResponse* response)
{
	// Validate before touching SolidWorks.
	const bool existing = !request->sketch_name().empty();
	const bool onEntity = !request->on_persist_id().empty();
	int planeIndex = 0;
	if (existing)
	{
		if (onEntity || request->plane() != sld_mcp::v1::PLANE_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"give either an existing sketch_name or where to put a new sketch, not both");
	}
	else if (!onEntity && !StandardPlaneIndex(request->plane(), planeIndex))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
	if (request->entities_size() == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "nothing to draw -- give at least one entity");
	if (request->entities_size() > kMaxSketchEntities)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many entities in one call (max 1000)");
	for (int i = 0; i < request->entities_size(); ++i)
		if (grpc::Status status = ValidateSketchEntity(request->entities(i)); !status.ok())
			return grpc::Status(status.error_code(),
				"entity " + std::to_string(i + 1) + ": " + status.error_message());

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

	// Resolve everything that can fail before a sketch is opened.
	ATL::CComPtr<IFeature> sketchFeature;
	SketchTarget target;
	if (existing)
	{
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
			return status;
	}
	else if (grpc::Status status = ResolveSketchTarget(doc, request->on_persist_id(), planeIndex, target);
		!status.ok())
		return status;

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;

	ATL::CComPtr<ISketchManager> sketchManager;
	HRESULT hr = doc->get_SketchManager(&sketchManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::SketchManager", hr);
	if (!sketchManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	// Open the sketch: re-enter the existing one, or create one.
	ATL::CComPtr<ISketch> sketch;
	if (existing)
	{
		if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
			return status;
	}
	else
	{
		if (grpc::Status status = OpenNewSketch(doc, sketchManager, target, sketch); !status.ok())
			return status;
		sketchFeature = ATL::CComQIPtr<IFeature>(sketch.p);
	}

	// The sketch is now open: everything below must fall through to the
	// InsertSketch call that closes it, so no early returns until then.
	space.Attach(sketch);
	const SketchPointMapper map{space, request->model_space()};

	// AddToDB skips snapping and inferencing, so entities land exactly at the
	// requested coordinates instead of snapping to nearby geometry.
	VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
	sketchManager->get_AddToDB(&previousAddToDB);
	sketchManager->put_AddToDB(VARIANT_TRUE);

	grpc::Status failure = grpc::Status::OK;
	std::vector<ATL::CComPtr<IDispatch>> created;
	for (int i = 0; i < request->entities_size() && failure.ok(); ++i)
	{
		ATL::CComPtr<IDispatch> entity;
		failure = DrawSketchEntity(sketchManager, map, request->entities(i), entity);
		if (failure.ok())
			created.push_back(entity);
		else
			failure = grpc::Status(failure.error_code(),
				"entity " + std::to_string(i + 1) + ": " + failure.error_message());
	}

	sketchManager->put_AddToDB(previousAddToDB);

	if (failure.ok())
	{
		// With AddToDB on, entities that meet end to end each keep their own
		// end point; merging them is what connects them into contours.
		VARIANT_BOOL merged = VARIANT_FALSE;
		sketch->MergePoints(kMergeDistance, &merged);

		// IDs for later dimensioning. Merging can delete a drawn point (it's
		// folded into another), so describe after it, skipping what's gone.
		for (const auto& entity : created)
		{
			sld_mcp::v1::SketchItem item;
			if (DescribeSketchItem(entity, std::string(), &item))
				response->add_items()->Swap(&item);
		}

		sld_mcp::v1::SketchFrame frame;
		if (space.Frame(frame))
			*response->mutable_frame() = frame;
	}
	else if (existing)
	{
		// All or nothing: take back what this call drew.
		DeleteSketchEntities(doc, created);
	}

	ATL::CComBSTR sketchName;
	if (sketchFeature)
		sketchFeature->get_Name(&sketchName);

	// Close the sketch (and rebuild).
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	doc->ClearSelection2(VARIANT_TRUE);

	if (!failure.ok())
	{
		// Don't leave a half-drawn new sketch behind.
		if (!existing && SUCCEEDED(closeHr))
			DeleteSketchFeature(doc, sketchFeature);
		return failure;
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	// After the rebuild, so contours reflect the merged points.
	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;

	// Read once the sketch is closed, so it's the ID later calls compare to.
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (auto& item : *response->mutable_items())
		StampSketchId(&item, sketchPid);

	response->set_sketch_name(ToUtf8(sketchName));
	response->set_created_sketch(!existing);
	response->set_entity_count(static_cast<int32_t>(created.size()));
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
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketch); !status.ok())
			return status;
	}

	ATL::CComBSTR sketchName;
	hr = sketch->get_Name(&sketchName);
	if (FAILED(hr))
		return ComError("IFeature::Name", hr);

	// Informational: whether the profile is fully defined.
	if (ATL::CComPtr<ISketch> sketchData; SketchOf(sketch, sketchData).ok())
		response->set_sketch_status(SketchStatus(sketchData));

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
	long docType = swDocNONE;
	if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
		return status;

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	PersistIds ids(ext);

	ATL::CComBSTR title;
	HRESULT hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);
	response->mutable_document()->set_title(ToUtf8(title));

	if (docType == swDocASSEMBLY)
	{
		response->mutable_document()->set_type(sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY);
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		return InspectAssembly(doc, ext, ids, math, *request, response);
	}
	response->mutable_document()->set_type(sld_mcp::v1::DOCUMENT_TYPE_PART);
	ATL::CComQIPtr<IPartDoc> part(doc.p);
	if (!part)
		return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");

	if (request->include_features())
		if (grpc::Status status = InspectFeatures(doc, ids, false, response); !status.ok())
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

grpc::Status SolidWorksService::GetMesh(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::GetMeshRequest* request,
	sld_mcp::v1::GetMeshResponse* response)
{
	// Read-only, but must not see a half-finished edit (see InspectModel).
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	long docType = swDocNONE;
	if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
		return status;
	const bool isAssembly = docType == swDocASSEMBLY;

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	PersistIds ids(ext);

	ATL::CComBSTR title;
	HRESULT hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);
	response->mutable_document()->set_title(ToUtf8(title));
	response->mutable_document()->set_type(isAssembly ? sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY
	                                                  : sld_mcp::v1::DOCUMENT_TYPE_PART);

	// Only what's shown: hidden bodies (or components) aren't in the picture
	// either. In an assembly each body comes with its component, and its
	// geometry may be in that component's coordinates; the overall size
	// comes from the components' assembly-space boxes.
	std::vector<std::pair<ATL::CComPtr<IBody2>, std::string>> bodies;  // body, component persist id
	Eigen::AlignedBox3d sizeBox;
	if (isAssembly)
	{
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		ComponentList components(ids, math, response->mutable_components());
		for (const auto& component : GeometryComponents(assembly, true))
		{
			const std::vector<ATL::CComPtr<IBody2>> componentBodies = ComponentBodies(component);
			if (componentBodies.empty())
				continue;
			const std::string pid = components.Add(component);
			for (const auto& body : componentBodies)
				bodies.emplace_back(body, pid);
			ATL::CComVariant box;
			if (SUCCEEDED(component->GetBox(VARIANT_FALSE, VARIANT_FALSE, &box)))
				sizeBox.extend(ToAlignedBox(comvariant::ToDoubles(box)));
		}
	}
	else
	{
		ATL::CComQIPtr<IPartDoc> part(doc.p);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		ATL::CComVariant all;
		hr = part->GetBodies2(swAllBodies, VARIANT_TRUE, &all);
		if (FAILED(hr))
			return ComError("IPartDoc::GetBodies2", hr);
		for (const auto& disp : comvariant::ToDispatches(all))
			if (ATL::CComQIPtr<IBody2> body(disp.p); body)
			{
				bodies.emplace_back(ATL::CComPtr<IBody2>(body), std::string());
				ATL::CComVariant box;
				if (SUCCEEDED(body->GetBodyBox(&box)))
					sizeBox.extend(ToAlignedBox(comvariant::ToDoubles(box)));
			}
	}

	// Edge tessellation tolerances scale with the model: chords within 0.1%
	// of its size, segments no longer than 5% of it.
	double size = sizeBox.isEmpty() ? 0.0 : sizeBox.diagonal().norm();
	if (!(size > 0.0) || !std::isfinite(size))
		size = 0.1;
	const double chordTolerance = size * 1e-3;
	const double lengthTolerance = size * 0.05;

	size_t faceCount = 0, edgeCount = 0;
	bool truncated = false;
	for (const auto& [body, componentId] : bodies)
	{
		const std::string bodyId = ids.Get(body);

		ATL::CComVariant faces;
		if (SUCCEEDED(body->GetFaces(&faces)))
		{
			for (const auto& disp : comvariant::ToDispatches(faces))
			{
				ATL::CComQIPtr<IFace2> face(disp.p);
				if (!face)
					continue;
				if (faceCount >= kMaxMeshFaces)
				{
					truncated = true;
					break;
				}
				sld_mcp::v1::FaceMesh mesh;
				if (!MeshFace(face, &mesh))
					continue;
				mesh.set_persist_id(ids.Get(face));
				mesh.set_body_persist_id(bodyId);
				mesh.set_component_persist_id(componentId);
				ATL::CComPtr<IFeature> feature;
				if (SUCCEEDED(face->IGetFeature(&feature)) && feature)
				{
					mesh.set_feature_persist_id(ids.Get(feature));
					mesh.set_feature_name(FeatureName(feature));
				}
				response->add_faces()->Swap(&mesh);
				++faceCount;
			}
		}

		if (!request->include_edges())
			continue;
		ATL::CComVariant edges;
		if (SUCCEEDED(body->GetEdges(&edges)))
		{
			std::vector<double> points;
			for (const auto& disp : comvariant::ToDispatches(edges))
			{
				ATL::CComQIPtr<IEdge> edge(disp.p);
				if (!edge)
					continue;
				if (edgeCount >= kMaxMeshEdges)
				{
					truncated = true;
					break;
				}
				if (!TessellateEdge(edge, chordTolerance, lengthTolerance, points))
					continue;
				sld_mcp::v1::EdgePolyline* polyline = response->add_edges();
				polyline->set_persist_id(ids.Get(edge));
				polyline->set_body_persist_id(bodyId);
				polyline->set_component_persist_id(componentId);
				AppendFloats(points, polyline->mutable_points());
				++edgeCount;
			}
		}
	}
	response->set_truncated(truncated);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::InspectSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectSketchRequest* request,
	sld_mcp::v1::InspectSketchResponse* response)
{
	// Read-only, but must not see a half-finished edit (see InspectModel).
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
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<IFeature> feature;
	if (grpc::Status status = ResolveSketchFeature(doc, request->sketch_name(), feature); !status.ok())
		return status;
	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = SketchOf(feature, sketch); !status.ok())
		return status;
	const std::string sketchName = FeatureName(feature);
	const std::string sketchPid = PersistIdOf(ext, feature);
	response->set_sketch_name(sketchName);

	SketchSpace space;
	if (space.Init(swApp).ok())
	{
		space.Attach(sketch);
		sld_mcp::v1::SketchFrame frame;
		if (space.Frame(frame))
			*response->mutable_frame() = frame;
	}

	response->set_status(SketchStatus(sketch));

	SketchItems items;
	items.Load(sketch);
	for (const auto& entry : items.All())
	{
		if (static_cast<size_t>(response->items_size()) >= kMaxSketchItems)
		{
			response->set_truncated(true);
			break;
		}
		sld_mcp::v1::SketchItem item;
		if (DescribeSketchItem(entry.obj, sketchPid, &item))
			response->add_items()->Swap(&item);
	}

	for (const auto& display : FeatureDimensions(feature))
	{
		sld_mcp::v1::DimensionInfo info;
		if (DescribeDimension(display, &info))
			response->add_dimensions()->Swap(&info);
	}

	for (const auto& relation : SketchRelations(sketch))
	{
		long type = 0;
		// Dimensions show up as relations too; they're listed above.
		if (SUCCEEDED(relation->GetRelationType(&type)) &&
			(type == swConstraintType_DISTANCE || type == swConstraintType_ANGLE ||
			 type == swConstraintType_RADIUS || type == swConstraintType_DIAMETER))
			continue;
		DescribeRelation(relation, sketchPid, sketchName, response->add_relations());
	}

	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddSketchDimensions(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddSketchDimensionsRequest* request,
	sld_mcp::v1::AddSketchDimensionsResponse* response)
{
	// Validate before touching SolidWorks.
	const int count = request->dimensions_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one dimension");
	if (count > kMaxSketchConstraints)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many dimensions in one call (max 200)");
	auto where = [](int i) { return "dimension " + std::to_string(i + 1); };
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = ValidateSketchDimension(request->dimensions(i)); !status.ok())
			return grpc::Status(status.error_code(), where(i) + ": " + status.error_message());

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
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	// Resolve everything that can fail before the sketch is opened.
	ATL::CComPtr<IFeature> sketchFeature;
	if (grpc::Status status = ResolveConstraintSketch(doc, ext, request->sketch_name(), request->dimensions(),
		sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = CheckRefsInSketch(ext, request->dimensions(i).refs(), sketchPid, sketchName,
			where(i)); !status.ok())
			return status;

	ATL::CComPtr<ISketchPoint> originPoint;
	std::string originName;
	FindOrigin(doc, originPoint, originName);

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;
	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;

	PreferenceToggles toggles(swApp);
	SilenceDimensionPrompts(toggles);

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
		return status;

	// The sketch is open: the work below returns its status instead of
	// returning from the RPC, so the sketch is always closed after.
	space.Attach(sketch);
	std::vector<ATL::CComPtr<IDisplayDimension>> created;
	std::vector<double> originalValues;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems items;
		items.Load(sketch);
		const RefContext ctx{ext, items, space, sketchPid, sketchName};

		// Rolls back: values first (moving the geometry back), then the
		// dimensions themselves.
		auto rollback = [&](const grpc::Status& why) {
			for (size_t i = created.size(); i-- > 0;)
			{
				ATL::CComPtr<IDimension> dim;
				long result = 0;
				if (i < originalValues.size() && SUCCEEDED(created[i]->GetDimension2(0, &dim)) && dim)
					dim->SetSystemValue3(originalValues[i], swSetValue_UseCurrentSetting, ATL::CComVariant(), &result);
			}
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			DeleteDimensions(doc, created);
			return why;
		};

		// Create every dimension at the geometry's current value first, so
		// setting values afterwards solves with all of them in place.
		for (int i = 0; i < count; ++i)
		{
			const auto& dimension = request->dimensions(i);
			std::vector<ResolvedRef> refs(dimension.refs_size());
			for (int r = 0; r < dimension.refs_size(); ++r)
				if (grpc::Status status = ResolveRef(ctx, dimension.refs(r), refs[r]); !status.ok())
					return rollback(grpc::Status(status.error_code(),
						where(i) + ", reference " + std::to_string(r + 1) + ": " + status.error_message()));
			if (grpc::Status status = CheckDimensionRefs(dimension.kind(), refs); !status.ok())
				return rollback(grpc::Status(status.error_code(), where(i) + ": " + status.error_message()));

			double text2d[2];
			if (grpc::Status status = PlaceDimension(dimension.kind(), refs, text2d); !status.ok())
				return rollback(grpc::Status(status.error_code(), where(i) + ": " + status.error_message()));
			const double textSketch[3] = {text2d[0], text2d[1], 0.0};
			double text[3];
			if (!space.ToModel(textSketch, text))
				return rollback(grpc::Status(grpc::StatusCode::INTERNAL,
					"couldn't map a point out of the sketch's coordinates"));

			doc->ClearSelection2(VARIANT_TRUE);
			for (int r = 0; r < static_cast<int>(refs.size()); ++r)
				if (!SelectRef(ext, refs[r], originPoint, originName))
					return rollback(grpc::Status(grpc::StatusCode::INTERNAL,
						where(i) + ": SolidWorks wouldn't select reference " + std::to_string(r + 1)));

			ATL::CComPtr<IDispatch> dimDisp;
			const HRESULT hr = CreateDimension(doc, dimension.kind(), text, dimDisp);
			doc->ClearSelection2(VARIANT_TRUE);
			ATL::CComQIPtr<IDisplayDimension> display(dimDisp.p);
			if (FAILED(hr))
				return rollback(ComError("IModelDoc2::Add*Dimension2", hr));
			if (!display)
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": SolidWorks couldn't make a " + DimensionKindName(dimension.kind()) +
					" dimension from those entities"));
			created.push_back(display);

			ATL::CComPtr<IDimension> dim;
			if (FAILED(display->GetDimension2(0, &dim)) || !dim)
				return rollback(grpc::Status(grpc::StatusCode::INTERNAL, where(i) + ": the new dimension has no value"));
			double value = 0.0;
			dim->get_SystemValue(&value);
			originalValues.push_back(value);
			long driven = swDimensionDrivenUnknown;
			if (SUCCEEDED(dim->get_DrivenState(&driven)) && driven == swDimensionDriven)
				return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where(i) + ": that would over-define the sketch -- what it measures is already fixed by "
					"other dimensions or relations"));
		}

		// Now drive them to the requested values.
		bool anyValue = false;
		for (int i = 0; i < count; ++i)
		{
			if (!request->dimensions(i).has_value())
				continue;
			anyValue = true;
			ATL::CComPtr<IDimension> dim;
			long result = swSetValue_Failure;
			if (FAILED(created[i]->GetDimension2(0, &dim)) || !dim ||
				FAILED(dim->SetSystemValue3(request->dimensions(i).value(), swSetValue_UseCurrentSetting,
					ATL::CComVariant(), &result)) || result != swSetValue_Successful)
				return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where(i) + ": SolidWorks wouldn't take that value (error " + std::to_string(result) + ")"));
		}
		if (anyValue)
		{
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
		}

		const sld_mcp::v1::DefinitionStatus status = SketchStatus(sketch);
		if (status == sld_mcp::v1::DEFINITION_STATUS_OVER || status == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION ||
			status == sld_mcp::v1::DEFINITION_STATUS_INVALID)
			return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("with those values the sketch is ") + DefinitionStatusText(status) +
				" -- nothing was changed; check the values against the sketch's other dimensions"));

		for (const auto& display : created)
			DescribeDimension(display, response->add_dimensions());
		ReadSketchState(sketch, sketchPid, response->mutable_state());
		return grpc::Status::OK;
	}();

	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	response->set_sketch_name(sketchName);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddSketchRelations(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddSketchRelationsRequest* request,
	sld_mcp::v1::AddSketchRelationsResponse* response)
{
	const int count = request->relations_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one relation");
	if (count > kMaxSketchConstraints)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many relations in one call (max 200)");
	auto where = [](int i) { return "relation " + std::to_string(i + 1); };
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = ValidateSketchRelation(request->relations(i)); !status.ok())
			return grpc::Status(status.error_code(), where(i) + ": " + status.error_message());

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
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<IFeature> sketchFeature;
	if (grpc::Status status = ResolveConstraintSketch(doc, ext, request->sketch_name(), request->relations(),
		sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	for (int i = 0; i < count; ++i)
		if (grpc::Status status = CheckRefsInSketch(ext, request->relations(i).refs(), sketchPid, sketchName,
			where(i)); !status.ok())
			return status;

	ATL::CComPtr<ISketchPoint> originPoint;
	std::string originName;
	FindOrigin(doc, originPoint, originName);

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;
	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
		return status;

	space.Attach(sketch);
	int added = 0;
	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems items;
		items.Load(sketch);
		const RefContext ctx{ext, items, space, sketchPid, sketchName};
		const size_t before = SketchRelations(sketch).size();
		auto rollback = [&](const grpc::Status& why) {
			DeleteRelationsAfter(sketch, before);
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			return why;
		};

		for (int i = 0; i < count; ++i)
		{
			const auto& relation = request->relations(i);
			std::vector<ResolvedRef> refs(relation.refs_size());
			bool allPoints = true;
			for (int r = 0; r < relation.refs_size(); ++r)
			{
				if (grpc::Status status = ResolveRef(ctx, relation.refs(r), refs[r]); !status.ok())
					return rollback(grpc::Status(status.error_code(),
						where(i) + ", reference " + std::to_string(r + 1) + ": " + status.error_message()));
				allPoints = allPoints && refs[r].isPoint;
			}
			if (relation.kind() == sld_mcp::v1::RELATION_KIND_SYMMETRIC && !refs.back().isLine)
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": the last entity of a symmetric relation must be the line to mirror about"));
			// Horizontal/vertical between points needs exactly two of them.
			if (allPoints && refs.size() != 2 &&
				(relation.kind() == sld_mcp::v1::RELATION_KIND_HORIZONTAL ||
				 relation.kind() == sld_mcp::v1::RELATION_KIND_VERTICAL))
				return rollback(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					where(i) + ": horizontal/vertical relations take lines, or exactly 2 points"));

			doc->ClearSelection2(VARIANT_TRUE);
			for (int r = 0; r < static_cast<int>(refs.size()); ++r)
				if (!SelectRef(ext, refs[r], originPoint, originName))
					return rollback(grpc::Status(grpc::StatusCode::INTERNAL,
						where(i) + ": SolidWorks wouldn't select reference " + std::to_string(r + 1)));

			const size_t relationsBefore = SketchRelations(sketch).size();
			const HRESULT hr = doc->SketchAddConstraints(ATL::CComBSTR(RelationCommand(relation.kind(), allPoints)));
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return rollback(ComError("IModelDoc2::SketchAddConstraints", hr));
			if (SketchRelations(sketch).size() <= relationsBefore)
				return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					where(i) + ": SolidWorks didn't add the " + RelationKindName(relation.kind()) +
					" relation -- the entities may not suit it, already have it, or it would over-define "
					"the sketch"));
			++added;
		}

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		const sld_mcp::v1::DefinitionStatus status = SketchStatus(sketch);
		if (status == sld_mcp::v1::DEFINITION_STATUS_OVER || status == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION ||
			status == sld_mcp::v1::DEFINITION_STATUS_INVALID)
			return rollback(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("with those relations the sketch is ") + DefinitionStatusText(status) +
				" -- they were removed again"));

		ReadSketchState(sketch, sketchPid, response->mutable_state());
		return grpc::Status::OK;
	}();

	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	response->set_sketch_name(sketchName);
	response->set_added(added);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::FullyDefineSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::FullyDefineSketchRequest* request,
	sld_mcp::v1::FullyDefineSketchResponse* response)
{
	if (request->skip_relations() && request->skip_dimensions())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"skipping both relations and dimensions leaves nothing to add");
	long scheme = swAutodimSchemeBaseline;
	switch (request->scheme())
	{
	case sld_mcp::v1::DIMENSION_SCHEME_UNSPECIFIED:
	case sld_mcp::v1::DIMENSION_SCHEME_BASELINE: scheme = swAutodimSchemeBaseline; break;
	case sld_mcp::v1::DIMENSION_SCHEME_CHAIN:    scheme = swAutodimSchemeChain; break;
	case sld_mcp::v1::DIMENSION_SCHEME_ORDINATE: scheme = swAutodimSchemeOrdinate; break;
	default:
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown dimension scheme");
	}

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
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<IFeature> sketchFeature;
	if (grpc::Status status = ResolveSketchFeature(doc, request->sketch_name(), sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);

	// Datums, as refs (checked like a dimension's).
	google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> datums;
	if (request->has_horizontal_datum())
		*datums.Add() = request->horizontal_datum();
	if (request->has_vertical_datum())
		*datums.Add() = request->vertical_datum();
	if (grpc::Status status = CheckRefsInSketch(ext, datums, sketchPid, sketchName, "datum"); !status.ok())
		return status;

	ATL::CComPtr<ISketchPoint> originPoint;
	std::string originName;
	FindOrigin(doc, originPoint, originName);

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;
	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;

	const std::set<std::string> dimensionsBefore = DimensionNames(sketchFeature);

	PreferenceToggles toggles(swApp);
	SilenceDimensionPrompts(toggles);

	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, sketch); !status.ok())
		return status;

	space.Attach(sketch);
	size_t relationsBefore = 0;
	const grpc::Status failure = [&]() -> grpc::Status {
		relationsBefore = SketchRelations(sketch).size();
		const sld_mcp::v1::DefinitionStatus initial = SketchStatus(sketch);
		if (initial == sld_mcp::v1::DEFINITION_STATUS_FULLY)
			return grpc::Status::OK;  // nothing to do
		if (initial == sld_mcp::v1::DEFINITION_STATUS_OVER || initial == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("the sketch is ") + DefinitionStatusText(initial) +
				" -- fix the conflicting dimensions or relations first");

		SketchItems items;
		items.Load(sketch);
		const RefContext ctx{ext, items, space, sketchPid, sketchName};
		ATL::CComPtr<IDispatch> datum[2];  // horizontal, vertical; null = sketch origin
		const sld_mcp::v1::SketchRef* datumRefs[2] = {
			request->has_horizontal_datum() ? &request->horizontal_datum() : nullptr,
			request->has_vertical_datum() ? &request->vertical_datum() : nullptr};
		for (int d = 0; d < 2; ++d)
		{
			if (!datumRefs[d] || datumRefs[d]->has_origin())
				continue;
			ResolvedRef ref;
			if (grpc::Status status = ResolveRef(ctx, *datumRefs[d], ref); !status.ok())
				return grpc::Status(status.error_code(),
					std::string(d == 0 ? "horizontal" : "vertical") + " datum: " + status.error_message());
			datum[d] = ref.obj;
		}

		const VARIANT_BOOL relations = request->skip_relations() ? VARIANT_FALSE : VARIANT_TRUE;
		const VARIANT_BOOL dimensions = request->skip_dimensions() ? VARIANT_FALSE : VARIANT_TRUE;
		const long allRelations = swSketchFullyDefineRelationType_Equal | swSketchFullyDefineRelationType_Horizontal |
			swSketchFullyDefineRelationType_Vertical | swSketchFullyDefineRelationType_Tangent |
			swSketchFullyDefineRelationType_Perpendicular | swSketchFullyDefineRelationType_Colinear |
			swSketchFullyDefineRelationType_Concentric | swSketchFullyDefineRelationType_Parallel |
			swSketchFullyDefineRelationType_Midpoint | swSketchFullyDefineRelationType_Coincident;
		auto run = [&](VARIANT_BOOL allEntities, long& code) {
			doc->ClearSelection2(VARIANT_TRUE);
			if (allEntities == VARIANT_FALSE)
				for (const auto& entry : items.All())
				{
					VARIANT_BOOL selected = VARIANT_FALSE;
					if (ATL::CComQIPtr<ISketchSegment> segment(entry.obj.p); segment)
						segment->Select4(VARIANT_TRUE, nullptr, &selected);
				}
			code = swAutodimStatusSuccess;
			const HRESULT hr = sketchManager->FullyDefineSketch(allEntities, relations, allRelations, dimensions,
				scheme, datum[0], scheme, datum[1], swAutodimHorizontalPlacementBelow,
				swAutodimVerticalPlacementLeft, &code);
			doc->ClearSelection2(VARIANT_TRUE);
			return hr;
		};

		long code = swAutodimStatusSuccess;
		HRESULT hr = run(VARIANT_TRUE, code);
		// If SolidWorks wants the entities selected after all, select them all.
		if (SUCCEEDED(hr) && code == swAutodimStatusNoEntities)
			hr = run(VARIANT_FALSE, code);
		// If it insists on explicit datums, use the origin for the missing ones.
		if (SUCCEEDED(hr) && code == swAutodimStatusDatumNotSupplied && originPoint)
		{
			for (auto& d : datum)
				if (!d)
					d = originPoint.p;
			hr = run(VARIANT_TRUE, code);
		}
		if (FAILED(hr))
			return ComError("ISketchManager::FullyDefineSketch", hr);
		if (code != swAutodimStatusSuccess)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't fully define the sketch: " + AutodimError(code));

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		ReadSketchState(sketch, sketchPid, response->mutable_state());
		return grpc::Status::OK;
	}();

	if (failure.ok() && !response->has_state())
		ReadSketchState(sketch, sketchPid, response->mutable_state());
	const size_t relationsAfter = SketchRelations(sketch).size();

	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	// What it added, read from the closed sketch.
	for (const auto& display : FeatureDimensions(sketchFeature))
	{
		sld_mcp::v1::DimensionInfo info;
		if (DescribeDimension(display, &info) && !dimensionsBefore.count(info.name()))
			response->add_added_dimensions()->Swap(&info);
	}
	response->set_added_relations(static_cast<int32_t>(relationsAfter > relationsBefore ? relationsAfter - relationsBefore : 0));
	response->set_sketch_name(sketchName);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::SetDimensionValue(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetDimensionValueRequest* request,
	sld_mcp::v1::SetDimensionValueResponse* response)
{
	const bool angle = request->value_case() == sld_mcp::v1::SetDimensionValueRequest::kAngle;
	if (request->value_case() == sld_mcp::v1::SetDimensionValueRequest::VALUE_NOT_SET)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a length or an angle");
	const double value = angle ? request->angle() : request->length();
	if (!std::isfinite(value) || value <= 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the value must be positive");
	if (request->name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the dimension's name, e.g. D1@Sketch1");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditableModel(swApp, doc); !status.ok())
		return status;

	ATL::CComPtr<IDispatch> param;
	HRESULT hr = doc->Parameter(FromUtf8(request->name()), &param);
	if (FAILED(hr))
		return ComError("IModelDoc2::Parameter", hr);
	ATL::CComQIPtr<IDimension> dim(param.p);
	if (!dim)
		return grpc::Status(grpc::StatusCode::NOT_FOUND,
			"the active document has no dimension named \"" + request->name() + "\" -- names look like "
			"D1@Sketch1");

	long paramType = swDimensionParamTypeUnknown;
	dim->GetType(&paramType);
	if (paramType == swDimensionParamTypeDoubleAngular && !angle)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + request->name() + "\" is an angle; give degrees");
	if (paramType == swDimensionParamTypeDoubleLinear && angle)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + request->name() + "\" is a length; give millimeters");

	long result = swSetValue_Failure;
	hr = dim->SetSystemValue3(value, swSetValue_UseCurrentSetting, ATL::CComVariant(), &result);
	if (FAILED(hr))
		return ComError("IDimension::SetSystemValue3", hr);
	switch (result)
	{
	case swSetValue_Successful:
		break;
	case swSetValue_DrivenDimension:
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"\"" + request->name() + "\" is a driven (reference) dimension; it only measures");
	case swSetValue_InvalidValue:
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks rejected that value");
	default:
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't set \"" + request->name() + "\" (error " + std::to_string(result) + ")");
	}

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	hr = doc->EditRebuild3(&rebuilt);
	if (FAILED(hr))
		return ComError("IModelDoc2::EditRebuild3", hr);

	sld_mcp::v1::DimensionInfo* info = response->mutable_dimension();
	DescribeDimensionValue(dim, info);
	info->set_type(paramType == swDimensionParamTypeDoubleAngular ? sld_mcp::v1::DIMENSION_TYPE_ANGULAR
	                                                              : sld_mcp::v1::DIMENSION_TYPE_LINEAR);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::HoleWizard(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::HoleWizardRequest* request,
	sld_mcp::v1::HoleWizardResponse* response)
{
	// Validate before touching SolidWorks.
	auto invalid = [](const std::string& why) { return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why); };
	long genericType = 0;
	switch (request->kind())
	{
	case sld_mcp::v1::HOLE_KIND_HOLE:        genericType = swWzdHole; break;
	case sld_mcp::v1::HOLE_KIND_COUNTERBORE: genericType = swWzdCounterBore; break;
	case sld_mcp::v1::HOLE_KIND_COUNTERSINK: genericType = swWzdCounterSink; break;
	case sld_mcp::v1::HOLE_KIND_TAP:         genericType = swWzdTap; break;
	default: return invalid("kind must be HOLE, COUNTERBORE, COUNTERSINK or TAP");
	}
	const bool fromSketch = !request->sketch_name().empty();
	if (fromSketch && (!request->face_persist_id().empty() || !request->locations().empty()))
		return invalid("give either a sketch of hole points or a face (with locations), not both");
	if (!fromSketch && request->face_persist_id().empty())
		return invalid("give a sketch of hole points, or the face to put the holes on");
	if (request->size().empty())
		return invalid("give the hole size, e.g. \"M5\"");
	if (request->locations_size() > kMaxHoleLocations)
		return invalid("too many hole locations in one call (max 500)");
	for (const auto& location : request->locations())
		if (!IsFinite(location))
			return invalid("hole locations must be finite");
	const bool throughAll = request->through_all();
	if (!throughAll && !(std::isfinite(request->depth()) && request->depth() > 0.0))
		return invalid("depth must be positive (or set through_all)");
	for (const auto& [has, value] : {
			std::pair{request->has_diameter(), request->diameter()},
			std::pair{request->has_counterbore_diameter(), request->counterbore_diameter()},
			std::pair{request->has_counterbore_depth(), request->counterbore_depth()},
			std::pair{request->has_countersink_diameter(), request->countersink_diameter()},
			std::pair{request->has_thread_depth(), request->thread_depth()}})
		if (has && !(std::isfinite(value) && value > 0.0))
			return invalid("hole dimensions must be positive");
	if (request->has_countersink_angle() &&
		!(std::isfinite(request->countersink_angle()) && request->countersink_angle() > 0.0 &&
			request->countersink_angle() < 3.14159265358979))
		return invalid("the countersink angle must be between 0 and 180 degrees");

	// Value1..Value12 of HoleWizard5 mean different things per hole type;
	// -1 = "use the standard's value".
	auto opt = [](bool has, double value) { return has ? value : -1.0; };
	double fit = -1.0;
	switch (request->fit())
	{
	case sld_mcp::v1::SCREW_FIT_CLOSE:  fit = swScrewClearanceClose; break;
	case sld_mcp::v1::SCREW_FIT_NORMAL: fit = swScrewClearanceNormal; break;
	case sld_mcp::v1::SCREW_FIT_LOOSE:  fit = swScrewClearanceLoose; break;
	default: break;
	}
	std::array<double, 12> values;
	values.fill(-1.0);
	switch (genericType)
	{
	case swWzdCounterBore:
		// C-bore diameter, c-bore depth, head clearance, screw fit, ...
		values[0] = opt(request->has_counterbore_diameter(), request->counterbore_diameter());
		values[1] = opt(request->has_counterbore_depth(), request->counterbore_depth());
		values[3] = fit;
		break;
	case swWzdCounterSink:
		// C-sink diameter, c-sink angle, head clearance, screw fit, ...
		values[0] = opt(request->has_countersink_diameter(), request->countersink_diameter());
		values[1] = opt(request->has_countersink_angle(), request->countersink_angle());
		values[3] = fit;
		break;
	case swWzdHole:
		// Screw fit, drill angle, ...
		values[0] = fit;
		break;
	case swWzdTap:
		// Thread depth, near/far c-sink diameter/angle, drill angle, cosmetic
		// thread type, thread end condition, ...
		values[0] = opt(request->has_thread_depth(), request->thread_depth());
		values[6] = request->cosmetic_thread() ? static_cast<double>(swCosmeticThreadWithCallout) : -1.0;
		break;
	}

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

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;

	ATL::CComPtr<IFeatureManager> featureManager;
	HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;

	const ATL::CComBSTR size = FromUtf8(request->size());
	const ATL::CComBSTR threadClass(L"");
	// Puts holes wherever the current selection says.
	auto runWizard = [&](ATL::CComPtr<IFeature>& hole) {
		hole.Release();
		return featureManager->HoleWizard5(
			genericType,
			request->standard(),
			request->fastener_type(),
			size,
			static_cast<short>(throughAll ? swEndCondThroughAll : swEndCondBlind),
			opt(request->has_diameter(), request->diameter()),
			throughAll ? -1.0 : request->depth(),
			-1.0,               // Length: slots only
			values[0], values[1], values[2], values[3], values[4], values[5],
			values[6], values[7], values[8], values[9], values[10], values[11],
			threadClass,        // ANSI inch thread class; empty = default
			request->reverse_direction() ? VARIANT_TRUE : VARIANT_FALSE,
			VARIANT_TRUE,       // FeatureScope
			VARIANT_TRUE,       // AutoSelect: all bodies
			VARIANT_FALSE,      // AssemblyFeatureScope (assemblies only)
			VARIANT_FALSE,      // AutoSelectComponents
			VARIANT_FALSE,      // PropagateFeatureToParts
			&hole);
	};
	const std::string badHole =
		"check that size \"" + request->size() + "\" exists for that standard and fastener type (written as "
		"the Hole Wizard lists it, e.g. \"M5\" or \"#10\"), and that the holes go into material (try "
		"reverse_direction)";

	ATL::CComPtr<IFeature> hole;
	ATL::CComPtr<IFeature> placementFeature;
	ATL::CComPtr<ISketch> placement;
	// Sketch points that aren't hole centers (ends and centers of
	// construction geometry in a user's sketch), by GetID.
	std::set<std::pair<long long, long long>> notCenters;

	if (fromSketch)
	{
		// The user's own sketch of (dimensioned) points becomes the hole
		// feature's placement sketch, so its dimensions keep driving the holes.
		ATL::CComPtr<IFeature> sketchFeature;
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
			return status;
		const std::string name = FeatureName(sketchFeature);
		auto usedByFeature = [&]() {
			long children = 0;
			return SUCCEEDED(sketchFeature->IGetChildCount(&children)) && children > 0;
		};
		if (usedByFeature())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" is already used by a feature -- put the hole points in a sketch of their own");
		ATL::CComPtr<ISketch> sketch;
		if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
			return status;

		// Holes go at sketch points, so the sketch may only hold points and
		// construction geometry (whose end/center points aren't holes).
		ATL::CComVariant segments;
		hr = sketch->GetSketchSegments(&segments);
		if (FAILED(hr))
			return ComError("ISketch::GetSketchSegments", hr);
		for (const auto& segment : comvariant::ToDispatches(segments))
		{
			sld_mcp::v1::SketchItem item;
			if (!segment || !DescribeSketchItem(segment, std::string(), &item))
				continue;
			if (!item.construction())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"\"" + name + "\" has " + (item.name().empty() ? std::string("a segment") : item.name()) +
					", which isn't construction geometry -- a hole-point sketch should hold only sketch points "
					"(plus construction lines for reference)");
			for (const auto* ref : {&item.start_point(), &item.end_point(), &item.center_point()})
				if (ref->id0() || ref->id1())
					notCenters.emplace(ref->id0(), ref->id1());
		}
		std::vector<ATL::CComPtr<ISketchPoint>> holePoints;
		for (const auto& point : SketchPoints(sketch))
		{
			bool isPoint = false;
			long long id0 = 0, id1 = 0;
			ATL::CComQIPtr<ISketchPoint> sketchPoint(point.p);
			if (sketchPoint && SketchEntityId(point, isPoint, id0, id1) && !notCenters.count({id0, id1}))
				holePoints.push_back(sketchPoint);
		}
		if (holePoints.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" has no sketch points to put holes at -- add them with sketch_point");
		if (holePoints.size() > static_cast<size_t>(kMaxHoleLocations))
			return invalid("too many hole points in one sketch (max 500)");

		// Like preselecting the sketch in the UI. Tried two ways: the sketch
		// feature, then its points. Either only counts if the new feature
		// absorbed the sketch; otherwise the hole is deleted again.
		for (int attempt = 0; attempt < 2 && !hole; ++attempt)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			bool selected = true;
			if (attempt == 0)
			{
				VARIANT_BOOL ok = VARIANT_FALSE;
				selected = SUCCEEDED(sketchFeature->Select2(VARIANT_FALSE, 0, &ok)) && ok;
			}
			else
				for (size_t i = 0; i < holePoints.size() && selected; ++i)
				{
					VARIANT_BOOL ok = VARIANT_FALSE;
					selected = SUCCEEDED(holePoints[i]->Select4(i > 0 ? VARIANT_TRUE : VARIANT_FALSE, nullptr, &ok)) && ok;
				}
			if (!selected)
				continue;
			hr = runWizard(hole);
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IFeatureManager::HoleWizard5", hr);
			if (hole && !usedByFeature())
			{
				// Holes went somewhere else: not what was asked.
				DeleteFeatureAndSketches(doc, ext, hole);
				hole.Release();
			}
		}
		if (!hole)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't put holes at the points of \"" + name + "\" -- " + badHole +
				". The sketch must be on a planar face or plane; or give the face and locations instead");
		placementFeature = sketchFeature;
		if (grpc::Status status = SketchOf(placementFeature, placement); !status.ok())
			return status;
	}
	else
	{
		// The face, and where on it the holes go.
		ATL::CComPtr<IDispatch> faceObj;
		if (grpc::Status status = ResolvePersistId(ext, request->face_persist_id(), faceObj); !status.ok())
			return status;
		ATL::CComQIPtr<IFace2> face(faceObj.p);
		if (!face)
			return invalid("that ID isn't a face -- the Hole Wizard needs a planar face");
		FacePlane plane;
		if (grpc::Status status = ReadFacePlane(face, plane); !status.ok())
			return status;
		// The same bytes a selected object's persist ID is compared to.
		const std::string facePid = PersistIdOf(ext, face);

		std::vector<std::array<double, 3>> points;
		if (request->locations().empty())
		{
			ATL::CComVariant box;
			hr = face->GetBox(&box);
			if (FAILED(hr))
				return ComError("IFace2::GetBox", hr);
			const std::vector<double> b = comvariant::ToDoubles(box);
			if (b.size() < 6)
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no box for the face");
			const double center[3] = {(b[0] + b[3]) / 2.0, (b[1] + b[4]) / 2.0, (b[2] + b[5]) / 2.0};
			points.push_back(ProjectOntoPlane(plane, center));
		}
		else
			for (const auto& location : request->locations())
			{
				const double p[3] = {location.x(), location.y(), location.z()};
				points.push_back(ProjectOntoPlane(plane, p));
			}

		ATL::CComPtr<ISelectionMgr> selections;
		hr = doc->get_ISelectionManager(&selections);
		if (FAILED(hr))
			return ComError("IModelDoc2::ISelectionManager", hr);
		if (!selections)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no selection manager");

		ATL::CComPtr<ISketchManager> sketchManager;
		if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
			return status;

		// Selecting the face with a ray at a location is what tells the Hole
		// Wizard to put a hole there; one selection per hole.
		auto selectAt = [&](const std::array<double, 3>& p, bool append) {
			VARIANT_BOOL selected = VARIANT_FALSE;
			const HRESULT rayHr = ext->SelectByRay(
				p[0] + plane.normal[0] * kHoleRayLift, p[1] + plane.normal[1] * kHoleRayLift,
				p[2] + plane.normal[2] * kHoleRayLift, -plane.normal[0], -plane.normal[1], -plane.normal[2],
				kHoleRayRadius, swSelFACES, append ? VARIANT_TRUE : VARIANT_FALSE, 0, swSelectOptionDefault,
				&selected);
			return SUCCEEDED(rayHr) && selected;
		};
		auto selectedCount = [&]() {
			long count = 0;
			selections->GetSelectedObjectCount2(-1, &count);
			return count;
		};
		// Selects the first `n` locations; returns how many selections stuck.
		auto selectLocations = [&](size_t n) {
			doc->ClearSelection2(VARIANT_TRUE);
			for (size_t i = 0; i < n; ++i)
				if (!selectAt(points[i], i > 0))
					break;
			return static_cast<size_t>(selectedCount());
		};

		// Check every location really is on the face (and not, say, in a hole
		// through it, where the ray would hit something deeper) before
		// changing anything.
		for (size_t i = 0; i < points.size(); ++i)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			ATL::CComPtr<IDispatch> hit;
			if (selectAt(points[i], false) && selectedCount() == 1)
				selections->GetSelectedObject6(1, -1, &hit);
			if (!hit || PersistIdOf(ext, hit) != facePid)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return invalid(request->locations().empty()
					? "the center of the face's bounding box " + PointText(points[i]) +
						" isn't on the face -- give locations"
					: "hole location " + std::to_string(i + 1) + " " + PointText(points[i]) +
						" isn't on that face (after projecting it onto the face's plane)");
			}
		}
		doc->ClearSelection2(VARIANT_TRUE);

		// Normally every location is selected up front. If SolidWorks won't
		// hold the same face selected more than once, or the wizard doesn't
		// use every selection, fall back to one location and add the rest as
		// points in the placement sketch afterwards.
		size_t direct = points.size();
		if (selectLocations(direct) != direct)
		{
			direct = 1;
			if (selectLocations(1) != 1)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the face");
			}
		}

		for (int attempt = 0; attempt < 2; ++attempt)
		{
			hr = runWizard(hole);
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IFeatureManager::HoleWizard5", hr);
			if (!hole)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks couldn't create the hole -- " + badHole);
			if (grpc::Status status = FindHolePlacementSketch(hole, placementFeature, placement); !status.ok())
			{
				DeleteFeatureAndSketches(doc, ext, hole);
				return status;
			}
			if (direct == 1 || SketchPoints(placement).size() == direct)
				break;
			// The wizard didn't put a hole at every selection: start over with
			// one, and add the rest to the placement sketch.
			DeleteFeatureAndSketches(doc, ext, hole);
			direct = 1;
			if (selectLocations(1) != 1)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the face");
			}
		}

		// Remaining locations become points in the placement sketch; the hole
		// feature puts a hole at every point there.
		if (direct < points.size())
		{
			placement.Release(); // OpenExistingSketch fills it with the open sketch
			if (grpc::Status status = OpenExistingSketch(doc, placementFeature, placement); !status.ok())
			{
				DeleteFeatureAndSketches(doc, ext, hole);
				return status;
			}
			// The sketch is now open: no early returns until it's closed.
			space.Attach(placement);
			VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
			sketchManager->get_AddToDB(&previousAddToDB);
			sketchManager->put_AddToDB(VARIANT_TRUE);
			grpc::Status failure = grpc::Status::OK;
			for (size_t i = direct; i < points.size() && failure.ok(); ++i)
			{
				double s[3];
				ATL::CComPtr<ISketchPoint> point;
				if (!space.ToSketch(points[i].data(), s))
					failure = grpc::Status(grpc::StatusCode::INTERNAL,
						"couldn't map a hole location into the placement sketch's coordinates");
				else if (FAILED(hr = sketchManager->CreatePoint(s[0], s[1], 0.0, &point)))
					failure = ComError("ISketchManager::CreatePoint", hr);
				else if (!point)
					failure = grpc::Status(grpc::StatusCode::INTERNAL,
						"SolidWorks didn't create hole location " + std::to_string(i + 1));
			}
			sketchManager->put_AddToDB(previousAddToDB);
			const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
			doc->ClearSelection2(VARIANT_TRUE);
			if (failure.ok() && FAILED(closeHr))
				failure = ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
			if (failure.ok() &&
				(!FindHolePlacementSketch(hole, placementFeature, placement).ok() ||
					SketchPoints(placement).size() != points.size()))
				failure = grpc::Status(grpc::StatusCode::INTERNAL,
					"SolidWorks didn't keep every hole location in the placement sketch");
			if (!failure.ok())
			{
				// All or nothing.
				DeleteFeatureAndSketches(doc, ext, hole);
				return failure;
			}
		}
	}

	ATL::CComBSTR featureName;
	hr = hole->get_Name(&featureName);
	if (FAILED(hr))
		return ComError("IFeature::Name", hr);
	response->set_feature_name(ToUtf8(featureName));
	response->set_sketch_name(FeatureName(placementFeature));

	// The centers, with IDs for dimensioning them, and in model space.
	space.Attach(placement);
	const std::string sketchPid = PersistIdOf(ext, placementFeature);
	bool allMapped = true;
	for (const auto& point : SketchPoints(placement))
	{
		sld_mcp::v1::SketchItem item;
		if (!point || !DescribeSketchItem(point, sketchPid, &item) ||
			notCenters.count({item.ref().id0(), item.ref().id1()}))
			continue;
		const double s[3] = {item.position().x(), item.position().y(), 0.0};
		double m[3];
		allMapped = allMapped && space.ToModel(s, m);
		if (allMapped)
			SetVec3(response->add_centers_model(), m);
		response->add_centers()->Swap(&item);
	}
	// Both lists in step, or no model positions at all.
	if (!allMapped)
		response->clear_centers_model();
	sld_mcp::v1::SketchFrame frame;
	if (space.Frame(frame))
		*response->mutable_frame() = frame;

	ReadHoleSpec(hole, response->mutable_spec());
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::AddMate(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddMateRequest* request,
	sld_mcp::v1::AddMateResponse* response)
{
	// Validate before touching SolidWorks.
	long mateType = swMateCOINCIDENT;
	if (!ToSwMateType(request->type(), mateType))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give a mate type: coincident, concentric, perpendicular, parallel, tangent, distance, "
			"angle or lock");
	if (request->entity_persist_ids_size() != 2)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a mate takes exactly two entities");
	if (request->entity_persist_ids(0) == request->entity_persist_ids(1))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the two entities are the same one");
	double distance = 0.0, angle = 0.0;
	if (mateType == swMateDISTANCE)
	{
		distance = request->distance();
		if (!std::isfinite(distance) || distance < 0.0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the distance must be 0 or more");
	}
	if (mateType == swMateANGLE)
	{
		angle = request->angle();
		if (!std::isfinite(angle) || angle < 0.0 || angle > 3.14159265358979323846 + 1e-9)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the angle must be 0 to 180 degrees");
	}
	const long alignment = mateType == swMateLOCK ? swMateAlignCLOSEST : ToSwMateAlign(request->alignment());
	const VARIANT_BOOL flip = request->flip() ? VARIANT_TRUE : VARIANT_FALSE;
	const VARIANT_BOOL lockRotation =
		mateType == swMateCONCENTRIC && request->lock_rotation() ? VARIANT_TRUE : VARIANT_FALSE;

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
		return status;
	ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
	if (!assembly)
		return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;
	PersistIds ids(ext);

	MateRef refs[2];
	for (int i = 0; i < 2; ++i)
	{
		grpc::Status status = ResolveMateRef(ext, assembly, ids, request->entity_persist_ids(i), mateType, refs[i]);
		if (!status.ok())
			return grpc::Status(status.error_code(),
				"entity " + std::to_string(i + 1) + ": " + status.error_message());
	}
	const std::string componentIds[2] = { refs[0].component ? ids.Get(refs[0].component) : std::string(),
		refs[1].component ? ids.Get(refs[1].component) : std::string() };
	// (Two of the assembly's own planes, or planes whose component couldn't
	// be worked out, are left for SolidWorks to judge.)
	if (!componentIds[0].empty() && componentIds[0] == componentIds[1])
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"both entities are on \"" + ComponentName(refs[0].component) + "\" -- a mate joins two "
			"different components");

	// AddMate5 mates the current selection, marked 1.
	ATL::CComPtr<ISelectionMgr> selectionManager;
	HRESULT hr = doc->get_ISelectionManager(&selectionManager);
	if (FAILED(hr) || !selectionManager)
		return ComError("IModelDoc2::ISelectionManager", hr);
	ATL::CComPtr<ISelectData> selectData;
	hr = selectionManager->CreateSelectData(&selectData);
	if (FAILED(hr) || !selectData)
		return ComError("ISelectionMgr::CreateSelectData", hr);
	selectData->put_Mark(1);

	const std::set<std::string> matesBefore = MateNames(doc);
	doc->ClearSelection2(VARIANT_TRUE);
	for (int i = 0; i < 2; ++i)
		if (!SelectMateRef(refs[i], selectData))
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks wouldn't select entity " + std::to_string(i + 1) + " (" + refs[i].what + ")");
		}

	long addError = swAddMateError_ErrorUknown;
	ATL::CComPtr<IMate2> mate;
	hr = assembly->AddMate5(mateType, alignment, flip,
		distance, distance, distance,  // distance + its limits (same: not a limit mate)
		1.0, 1.0,                      // gear ratio, unused
		angle, angle, angle,           // angle + its limits
		VARIANT_FALSE,                 // a real mate, not just for positioning
		lockRotation,
		0,                             // width mate option, unused
		&addError, &mate);
	doc->ClearSelection2(VARIANT_TRUE);

	// The new mate is whatever wasn't in the Mates folder before. AddMate5
	// can leave a broken mate behind even when it reports an error.
	ATL::CComPtr<IFeature> mateFeature;
	for (const auto& feature : MateFeatures(doc))
		if (!matesBefore.count(FeatureName(feature)))
		{
			mateFeature = feature;
			break;
		}
	auto removeMate = [&]() {
		if (mateFeature)
		{
			DeleteFeature(doc, mateFeature);
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
		}
	};

	if (FAILED(hr))
	{
		removeMate();
		return ComError("IAssemblyDoc::AddMate5", hr);
	}
	if (!mate || addError != swAddMateError_NoError)
	{
		removeMate();
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, AddMateErrorText(addError, mateType));
	}
	if (!mateFeature)
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"SolidWorks reported the mate as added, but there's no new mate in the Mates folder");

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	hr = doc->EditRebuild3(&rebuilt);
	if (FAILED(hr))
		return ComError("IModelDoc2::EditRebuild3", hr);

	VARIANT_BOOL warning = VARIANT_FALSE;
	long errorCode = swFeatureErrorNone;
	mateFeature->GetErrorCode2(&warning, &errorCode);
	if (errorCode != swFeatureErrorNone && !warning)
	{
		removeMate();
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			std::string("SolidWorks added the ") + MateTypeName(mateType) + " mate but couldn't solve it -- " +
			MateErrorText(errorCode) + " -- so it was removed again");
	}

	DescribeMate(mateFeature, ids, response->mutable_mate());
	ComponentList components(ids, math, response->mutable_components());
	for (const auto& ref : refs)
		components.Add(ref.component);
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::InspectAnnotations(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectAnnotationsRequest* /*request*/,
	sld_mcp::v1::InspectAnnotationsResponse* response)
{
	// Read-only, but serialized with edits like InspectModel.
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	long docType = swDocNONE;
	if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
		return status;

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	PersistIds ids(ext);

	ATL::CComBSTR title;
	HRESULT hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);
	response->mutable_document()->set_title(ToUtf8(title));
	const bool isAssembly = docType == swDocASSEMBLY;
	response->mutable_document()->set_type(isAssembly ? sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY
		: sld_mcp::v1::DOCUMENT_TYPE_PART);

	// Assemblies: attached faces/edges belong to components, listed with
	// their placements like InspectModel does.
	ATL::CComQIPtr<IMathUtility> math;
	std::optional<ComponentList> components;
	if (isAssembly)
	{
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		components.emplace(ids, math, response->mutable_components());
	}

	bool truncated = false;
	for (const auto& annotation : AllAnnotations(doc, ext, truncated))
		DescribeAnnotation(annotation, ids, components ? &*components : nullptr, response->add_annotations());
	response->set_truncated(truncated);
	return grpc::Status::OK;
}
