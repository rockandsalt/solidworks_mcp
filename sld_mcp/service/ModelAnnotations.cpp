// ModelAnnotations.cpp : AddModelAnnotation -- feature control frames and
// datum feature symbols on a part or assembly (PMI; see docs/notes/gdt.md).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// A frame on a face or edge goes this far out from its arrow by default.
	constexpr double kTextOffset = 0.010;

	bool FiniteVec3(const sld_mcp::v1::Vec3& v)
	{
		return std::isfinite(v.x()) && std::isfinite(v.y()) && std::isfinite(v.z());
	}

	bool ReadPosition(IAnnotation* annotation, Eigen::Vector3d& out)
	{
		ATL::CComVariant positionVar;
		if (FAILED(annotation->GetPosition(&positionVar)))
			return false;
		const std::vector<double> p = comvariant::ToDoubles(positionVar);
		if (p.size() < 3)
			return false;
		out = Eigen::Vector3d(p[0], p[1], p[2]);
		return out.allFinite();
	}

	// The tip of the annotation's first leader (its last point).
	std::optional<Eigen::Vector3d> LeaderTip(IAnnotation* annotation)
	{
		long count = 0;
		ATL::CComVariant pointsVar;
		if (FAILED(annotation->GetLeaderCount(&count)) || count < 1 ||
			FAILED(annotation->GetLeaderPointsAtIndex(0, &pointsVar)))
			return std::nullopt;
		const std::vector<double> p = comvariant::ToDoubles(pointsVar);
		if (p.size() < 3)
			return std::nullopt;
		const Eigen::Vector3d tip(p[p.size() - 3], p[p.size() - 2], p[p.size() - 1]);
		return tip.allFinite() ? std::optional<Eigen::Vector3d>(tip) : std::nullopt;
	}

	// The middle of the part's (assembly's) box, model space.
	std::optional<Eigen::Vector3d> ModelCenter(IModelDoc2* doc, long docType)
	{
		ATL::CComVariant boxVar;
		if (docType == swDocPART)
		{
			if (ATL::CComQIPtr<IPartDoc> part(doc); !part || FAILED(part->GetPartBox(VARIANT_TRUE, &boxVar)))
				return std::nullopt;
		}
		else if (ATL::CComQIPtr<IAssemblyDoc> assembly(doc); !assembly || FAILED(assembly->GetBox(0, &boxVar)))
			return std::nullopt;
		const std::vector<double> b = comvariant::ToDoubles(boxVar);
		if (b.size() < 6)
			return std::nullopt;
		const Eigen::Vector3d center = (Eigen::Vector3d(b[0], b[1], b[2]) + Eigen::Vector3d(b[3], b[4], b[5])) / 2.0;
		return center.allFinite() ? std::optional<Eigen::Vector3d>(center) : std::nullopt;
	}

	// Which way is "out" at the arrow: a planar face's normal (parts; an
	// assembly's component faces report it in component space), else away
	// from the model's middle.
	std::optional<Eigen::Vector3d> OutwardAt(IDispatch* entity, long docType, const Eigen::Vector3d& tip,
		const std::optional<Eigen::Vector3d>& center)
	{
		if (docType == swDocPART)
			if (ATL::CComQIPtr<IFace2> face(entity); face)
			{
				ATL::CComPtr<ISurface> surface;
				VARIANT_BOOL plane = VARIANT_FALSE;
				ATL::CComVariant normalVar;
				if (SUCCEEDED(face->IGetSurface(&surface)) && surface && SUCCEEDED(surface->IsPlane(&plane)) && plane &&
					SUCCEEDED(face->get_Normal(&normalVar)))
					if (const std::vector<double> n = comvariant::ToDoubles(normalVar); n.size() >= 3)
					{
						const Eigen::Vector3d normal(n[0], n[1], n[2]);
						if (normal.allFinite() && normal.norm() > 1e-9)
							return normal.normalized();
					}
			}
		if (!center)
			return std::nullopt;
		const Eigen::Vector3d away = tip - *center;
		if (away.norm() < 1e-9)
			return std::nullopt;
		return away.normalized();
	}

	// The annotation view the annotation is in: its name and normal (toward
	// the viewer, model space).
	void DescribeAnnotationView(IAnnotation* annotation, sld_mcp::v1::AddModelAnnotationResponse* response)
	{
		ATL::CComPtr<IAnnotationView> view;
		if (FAILED(annotation->get_AnnotationView(&view)) || !view)
			return;
		ATL::CComVariant name;
		// IAnnotationView's name is only reachable late-bound.
		if (InvokeByName(view, L"Name", name) && name.vt == VT_BSTR)
			response->set_annotation_view(ToUtf8(ATL::CComBSTR(name.bstrVal)));
		// A 3 x 3 rotation whose columns are the view's x, y and z (toward the
		// viewer) in model space: identity for *Front, z = +X for *Right
		// (probed 2026-10-24).
		ATL::CComVariant rotation;
		if (SUCCEEDED(view->GetViewRotation(&rotation)))
			if (const std::vector<double> r = comvariant::ToDoubles(rotation); r.size() >= 9)
			{
				const double normal[3] = { r[2], r[5], r[8] };
				SetVec3(response->mutable_annotation_view_normal(), normal);
			}
	}

	grpc::Status ValidateRequest(const sld_mcp::v1::AddModelAnnotationRequest& r)
	{
		if (!r.has_symbol())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say what to add: a feature control frame or a datum symbol");
		if (grpc::Status status = ValidateGdtSymbol(r.symbol()); !status.ok())
			return status;
		if (r.symbol().has_datum() && r.attach_to().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a datum symbol goes on a face or an edge");
		if (r.has_arrow_at() && r.attach_to().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "arrow_at goes with a face or edge to point at");
		if ((r.has_arrow_at() && !FiniteVec3(r.arrow_at())) || (r.has_position() && !FiniteVec3(r.position())))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "points must be finite");
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::AddModelAnnotation(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::AddModelAnnotationRequest* request,
	sld_mcp::v1::AddModelAnnotationResponse* response)
{
	if (grpc::Status status = ValidateRequest(*request); !status.ok())
		return status;

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
	if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	const sld_mcp::v1::GdtSymbol& symbol = request->symbol();

	// Everything looked up before anything changes.
	{
		bool truncated = false;
		if (grpc::Status status = CheckDatumLabelFree(symbol, DatumLabels(AllAnnotations(doc, ext, truncated)));
			!status.ok())
			return status;
	}
	ATL::CComPtr<IDispatch> entity;
	if (!request->attach_to().empty())
	{
		if (grpc::Status status = ResolvePersistId(ext, request->attach_to(), entity); !status.ok())
			return status;
		if (!ATL::CComQIPtr<IFace2>(entity.p) && !ATL::CComQIPtr<IEdge>(entity.p))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a feature control frame or datum symbol goes on a face or an edge");
	}

	// Selected, then inserted: SolidWorks attaches a new frame / datum
	// symbol to what's selected. A face picked at a point gets the arrow
	// there; picked as a whole, at its middle (probed 2026-10-24).
	doc->ClearSelection2(VARIANT_TRUE);
	if (entity)
	{
		ATL::CComPtr<ISelectionMgr> selections;
		ATL::CComPtr<ISelectData> data;
		if (request->has_arrow_at() && SUCCEEDED(doc->get_ISelectionManager(&selections)) && selections &&
			SUCCEEDED(selections->CreateSelectData(&data)) && data)
		{
			data->put_X(request->arrow_at().x());
			data->put_Y(request->arrow_at().y());
			data->put_Z(request->arrow_at().z());
		}
		VARIANT_BOOL selected = VARIANT_FALSE;
		ATL::CComQIPtr<IEntity> selectable(entity.p);
		if (!selectable || FAILED(selectable->Select4(VARIANT_FALSE, data, &selected)) || !selected)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select the face / edge");
	}

	ATL::CComPtr<IAnnotation> annotation;
	grpc::Status status = InsertGdtSymbol(doc, ext, symbol, annotation);
	doc->ClearSelection2(VARIANT_TRUE);
	if (!status.ok())
		return status;
	auto fail = [&](grpc::Status why) {
		DeleteAnnotations(doc, ext, { annotation });
		doc->GraphicsRedraw2();
		return why;
	};
	if (entity && !GdtAttached(annotation))
		return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't attach the symbol to the face / edge; it was deleted again"));

	// Where the text goes. SolidWorks puts a frame's text on its arrow; a
	// datum symbol already stands off its face.
	std::optional<Eigen::Vector3d> position;
	if (request->has_position())
		position = Eigen::Vector3d(request->position().x(), request->position().y(), request->position().z());
	else if (symbol.has_gtol() && entity)
		if (const auto tip = LeaderTip(annotation); tip)
			if (const auto out = OutwardAt(entity, docType, *tip, ModelCenter(doc, docType)); out)
				position = *tip + *out * kTextOffset;
	if (position)
	{
		// It lands in the annotation view's plane (the depth along that view
		// stays, probed 2026-10-24), so it isn't compared; the reply has
		// where it went.
		VARIANT_BOOL ok = VARIANT_FALSE;
		Eigen::Vector3d now;
		if (FAILED(annotation->SetPosition2(position->x(), position->y(), position->z(), &ok)) || !ok ||
			!ReadPosition(annotation, now))
			return fail(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't move the symbol's text; it was deleted again"));
	}
	doc->GraphicsRedraw2();

	PersistIds ids(ext);
	ATL::CComQIPtr<IMathUtility> math;
	std::optional<ComponentList> components;
	if (docType == swDocASSEMBLY && GetMathUtility(swApp, math).ok())
		components.emplace(ids, math, response->mutable_components());
	DescribeAnnotation(annotation, ids, components ? &*components : nullptr, response->mutable_annotation());
	DescribeAnnotationView(annotation, response);
	return grpc::Status::OK;
}
