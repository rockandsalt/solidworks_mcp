// Common.cpp : see Common.h.
#include "pch.h"
#include "Common.h"

namespace sld_mcp::service
{
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

	grpc::Status GetEditablePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		if (grpc::Status status = GetActivePart(swApp, doc); !status.ok())
			return status;
		return CheckNoOpenSketch(doc);
	}

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

	grpc::Status GetExtension(IModelDoc2* doc, ATL::CComPtr<IModelDocExtension>& ext)
	{
		HRESULT hr = doc->get_Extension(&ext);
		if (FAILED(hr))
			return ComError("IModelDoc2::Extension", hr);
		if (!ext)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no model extension");
		return grpc::Status::OK;
	}

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

	Eigen::AlignedBox3d ToAlignedBox(const std::vector<double>& b)
	{
		if (b.size() < 6)
			return Eigen::AlignedBox3d();
		return Eigen::AlignedBox3d(Eigen::Vector3d(b[0], b[1], b[2]), Eigen::Vector3d(b[3], b[4], b[5]));
	}

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

	bool IsFinite(const sld_mcp::v1::Vec3& p)
	{
		return std::isfinite(p.x()) && std::isfinite(p.y()) && std::isfinite(p.z());
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

	sld_mcp::v1::DocumentType ToDocumentType(long type)
	{
		switch (type)
		{
		case swDocPART:
			return sld_mcp::v1::DOCUMENT_TYPE_PART;
		case swDocASSEMBLY:
			return sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY;
		case swDocDRAWING:
			return sld_mcp::v1::DOCUMENT_TYPE_DRAWING;
		default:
			return sld_mcp::v1::DOCUMENT_TYPE_UNSPECIFIED;
		}
	}

	bool SameObject(IUnknown* a, IUnknown* b)
	{
		ATL::CComPtr<IUnknown> ia, ib;
		if (!a || !b || FAILED(a->QueryInterface(IID_PPV_ARGS(&ia))) || FAILED(b->QueryInterface(IID_PPV_ARGS(&ib))))
			return false;
		return ia == ib;
	}
}
