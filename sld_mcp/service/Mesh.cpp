// Mesh.cpp : GetMesh: tessellation for render_views.
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"

using namespace sld_mcp::service;

namespace
{
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
