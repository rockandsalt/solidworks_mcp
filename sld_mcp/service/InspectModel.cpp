// InspectModel.cpp : InspectModel: features, bodies, faces, edges, components and mates.
//
// Entity details are best-effort: a property SolidWorks won't report is
// left unset rather than failing the whole inspection.
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"

using namespace sld_mcp::service;

namespace
{
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

	// Faces and edges past this many are dropped (and `truncated` set), so a
	// huge part can't stall SolidWorks' UI thread for minutes.
	constexpr size_t kMaxInspectedEntities = 2000;

	// Every feature from `first` on, starting at the first reference plane.
	// The ones before it are the tree's fixed folders (History, Sensors,
	// Annotations, Material, ...), which say nothing about the geometry.
	// `componentPid` is stamped on each (assembly component trees). With
	// `skipAssemblyItems`, components and the mates folder are left out
	// (an assembly's own tree; they're listed separately). With
	// `referenceExt` (the document's own tree), reference features say where
	// they are.
	grpc::Status InspectFeatureList(ATL::CComPtr<IFeature> feature, PersistIds& ids,
		const std::string& componentPid, bool skipAssemblyItems, IModelDocExtension* referenceExt,
		sld_mcp::v1::InspectModelResponse* response)
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

				VARIANT_BOOL warning = VARIANT_FALSE;
				long errorCode = swFeatureErrorNone;
				if (SUCCEEDED(feature->GetErrorCode2(&warning, &errorCode)) && errorCode != swFeatureErrorNone)
				{
					info->set_error_code(static_cast<int32_t>(errorCode));
					info->set_error_is_warning(warning != VARIANT_FALSE);
				}

				ATL::CComVariant parents;
				if (SUCCEEDED(feature->GetParents(&parents)))
					AddFeatureNames(parents, info->mutable_parents());
				ATL::CComVariant children;
				if (SUCCEEDED(feature->GetChildren(&children)))
					AddFeatureNames(children, info->mutable_children());

				if (referenceExt && IsReferenceType(info->type_name()))
					DescribeReferenceGeometry(referenceExt, feature, info->type_name(), info->mutable_reference());

				long visibility = swVisibilityStateShown;
				if ((IsSketchType(info->type_name()) || IsReferenceType(info->type_name())) &&
					SUCCEEDED(feature->get_Visible(&visibility)))
					info->set_hidden(visibility == swVisibilityStateHide);
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		return grpc::Status::OK;
	}

	grpc::Status InspectFeatures(IModelDoc2* doc, IModelDocExtension* ext, PersistIds& ids, bool skipAssemblyItems,
		sld_mcp::v1::InspectModelResponse* response)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);
		return InspectFeatureList(feature, ids, {}, skipAssemblyItems, ext, response);
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

		VARIANT_BOOL visible = VARIANT_TRUE;
		if (SUCCEEDED(body->get_Visible(&visible)))
			info->set_hidden(visible == VARIANT_FALSE);

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
				if (grpc::Status status = InspectFeatureList(first, ids, pid, false, nullptr, response); !status.ok())
					return status;
			}
			else if (grpc::Status status = InspectFeatures(doc, ext, ids, true, response); !status.ok())
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
		if (grpc::Status status = InspectFeatures(doc, ext, ids, false, response); !status.ok())
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
