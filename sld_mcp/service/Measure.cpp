// Measure.cpp : Measure and GetMassProperties (see docs/notes/measure.md).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "ReferenceHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The active part or assembly, no sketch open.
	grpc::Status GetMeasuredModel(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc, long& docType)
	{
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		return CheckNoOpenSketch(doc);
	}

	// One entity to measure: a pick, or a component (which picks don't cover).
	struct Measured
	{
		Pick pick;
		ATL::CComPtr<IComponent2> component;
	};

	grpc::Status ResolveMeasured(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GeometryRef& ref,
		Measured& out)
	{
		if (ref.target_case() == sld_mcp::v1::GeometryRef::kModelPersistId)
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, ref.model_persist_id(), obj); !status.ok())
				return status;
			if (ATL::CComQIPtr<IComponent2> component(obj.p); component)
			{
				out.component = component;
				out.pick.obj = obj;
				out.pick.what = ComponentName(component);
				return grpc::Status::OK;
			}
		}
		return ResolvePick(doc, ext, ref, out.pick);
	}

	// IMeasure is used by name: through the vtable its properties answer
	// other ones' values (get_Radius gave a face's perimeter, get_Area -1;
	// seen 2026-11-03, SolidWorks 2025 SP5), by name they're right.
	// It answers -1 for what doesn't apply.
	template <class Set>
	void SetMeasured(ATL::CComPtr<IDispatch>& measure, const wchar_t* name, Set set)
	{
		ATL::CComVariant v;
		if (SUCCEEDED(measure.GetPropertyByName(name, &v)) && SUCCEEDED(v.ChangeType(VT_R8)) && v.dblVal != -1.0)
			set(v.dblVal);
	}

	bool MeasuredFlag(ATL::CComPtr<IDispatch>& measure, const wchar_t* name)
	{
		ATL::CComVariant v;
		return SUCCEEDED(measure.GetPropertyByName(name, &v)) && SUCCEEDED(v.ChangeType(VT_BOOL)) && v.boolVal;
	}

	std::string BodyOrComponentName(IDispatch* obj)
	{
		if (ATL::CComQIPtr<IComponent2> component(obj); component)
			return ComponentName(component);
		ATL::CComBSTR name;
		if (ATL::CComQIPtr<IBody2> body(obj); body)
			body->get_Name(&name);
		return ToUtf8(name);
	}

	std::vector<double> MomentOfInertia(IMassProperty2* mass, long whereTaken)
	{
		ATL::CComVariant v;
		std::vector<double> m;
		if (SUCCEEDED(mass->GetMomentOfInertia(whereTaken, &v)))
			m = comvariant::ToDoubles(v);
		return m.size() == 9 ? m : std::vector<double>{};
	}

	std::string ActiveConfigurationName(IModelDoc2* doc)
	{
		ATL::CComPtr<IDispatch> disp;
		ATL::CComBSTR name;
		if (SUCCEEDED(doc->GetActiveConfiguration(&disp)) && disp)
			if (ATL::CComQIPtr<IConfiguration> config(disp.p); config)
				config->get_Name(&name);
		return ToUtf8(name);
	}
}

grpc::Status SolidWorksService::Measure(grpc::ServerContext* context,
	const sld_mcp::v1::MeasureRequest* request,
	sld_mcp::v1::MeasureResponse* response)
{
	return m_executor.Execute(context, "Measure", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		if (grpc::Status status = GetMeasuredModel(swApp, doc, docType); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		if (request->entities().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "nothing to measure");

		std::vector<Measured> measured(request->entities_size());
		for (int i = 0; i < request->entities_size(); ++i)
		{
			if (grpc::Status status = ResolveMeasured(doc, ext, request->entities(i), measured[i]); !status.ok())
				return grpc::Status(status.error_code(), "entity " + std::to_string(i + 1) + ": " + status.error_message());
			response->add_measured(measured[i].pick.what);
		}

		ATL::CComPtr<ISelectionMgr> selectionManager;
		HRESULT hr = doc->get_ISelectionManager(&selectionManager);
		if (FAILED(hr) || !selectionManager)
			return ComError("IModelDoc2::ISelectionManager", hr);
		ATL::CComPtr<ISelectData> data;
		hr = selectionManager->CreateSelectData(&data);
		if (FAILED(hr) || !data)
			return ComError("ISelectionMgr::CreateSelectData", hr);
		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t i = 0; i < measured.size(); ++i)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			const bool ok = measured[i].component
				? SUCCEEDED(measured[i].component->Select4(VARIANT_TRUE, data, VARIANT_FALSE, &selected)) && selected
				: SelectPick(ext, data, measured[i].pick, 0);
			if (!ok)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select entity " + std::to_string(i + 1) + " (" + measured[i].pick.what +
					") -- is it hidden or suppressed?");
			}
		}

		ATL::CComPtr<IMeasure> created;
		hr = ext->CreateMeasure(&created);
		ATL::CComPtr<IDispatch> measure(created.p);
		if (FAILED(hr) || !measure)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return ComError("IModelDocExtension::CreateMeasure", hr);
		}
		long arc = swMeasureArcCircle_CenterToCenter;
		if (request->arc() == sld_mcp::v1::ARC_MEASURE_MINIMUM)
			arc = swMeasureArcCircle_MinimumDistance;
		else if (request->arc() == sld_mcp::v1::ARC_MEASURE_MAXIMUM)
			arc = swMeasureArcCircle_MaximumDistance;
		ATL::CComVariant arcOption(arc);
		measure.PutPropertyByName(L"ArcOption", &arcOption);
		// Nothing: measure the selection.
		ATL::CComVariant selection;
		selection.vt = VT_DISPATCH;
		selection.pdispVal = nullptr;
		ATL::CComVariant calculated;
		hr = measure.Invoke1(L"Calculate", &selection, &calculated);
		if (FAILED(hr) || FAILED(calculated.ChangeType(VT_BOOL)) || !calculated.boolVal)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks measured nothing for " + std::string(measured.size() == 1 ? "this entity" : "these entities"));
		}

		auto* r = response;
		SetMeasured(measure, L"Distance", [r](double x) { r->set_distance(x); });
		SetMeasured(measure, L"CenterDistance", [r](double x) { r->set_center_distance(x); });
		SetMeasured(measure, L"NormalDistance", [r](double x) { r->set_normal_distance(x); });
		SetMeasured(measure, L"Angle", [r](double x) { r->set_angle(x); });
		SetMeasured(measure, L"Length", [r](double x) { r->set_length(x); });
		SetMeasured(measure, L"TotalLength", [r](double x) { r->set_total_length(x); });
		SetMeasured(measure, L"Area", [r](double x) { r->set_area(x); });
		SetMeasured(measure, L"TotalArea", [r](double x) { r->set_total_area(x); });
		SetMeasured(measure, L"Perimeter", [r](double x) { r->set_perimeter(x); });
		SetMeasured(measure, L"Radius", [r](double x) { r->set_radius(x); });
		SetMeasured(measure, L"Diameter", [r](double x) { r->set_diameter(x); });
		SetMeasured(measure, L"ArcLength", [r](double x) { r->set_arc_length(x); });
		SetMeasured(measure, L"ChordLength", [r](double x) { r->set_chord_length(x); });
		response->set_parallel(MeasuredFlag(measure, L"IsParallel"));
		response->set_perpendicular(MeasuredFlag(measure, L"IsPerpendicular"));
		response->set_intersect(MeasuredFlag(measure, L"IsIntersect"));
		doc->ClearSelection2(VARIANT_TRUE);

		if (measured.size() == 2 && measured[0].pick.obj && measured[1].pick.obj)
		{
			ATL::CComVariant p1, p2;
			double distance = -1;
			if (SUCCEEDED(doc->ClosestDistance(measured[0].pick.obj, measured[1].pick.obj, &p1, &p2, &distance)) &&
				distance >= 0)
			{
				const std::vector<double> a = comvariant::ToDoubles(p1), b = comvariant::ToDoubles(p2);
				if (a.size() >= 3 && b.size() >= 3)
				{
					SetVec3(response->mutable_closest_point1(), a.data());
					SetVec3(response->mutable_closest_point2(), b.data());
					response->set_closest_distance(distance);
				}
			}
		}
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::GetMassProperties(grpc::ServerContext* context,
	const sld_mcp::v1::GetMassPropertiesRequest* request,
	sld_mcp::v1::GetMassPropertiesResponse* response)
{
	return m_executor.Execute(context, "GetMassProperties", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		if (grpc::Status status = GetMeasuredModel(swApp, doc, docType); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		ATL::CComBSTR title;
		doc->GetTitle(&title);
		response->mutable_document()->set_title(ToUtf8(title));
		response->mutable_document()->set_type(ToDocumentType(docType));
		response->set_configuration(ActiveConfigurationName(doc));

		// Resolved first: CreateMassProperty2 starts from the selection.
		std::vector<ATL::CComPtr<IDispatch>> items;
		for (int i = 0; i < request->persist_ids_size(); ++i)
		{
			ATL::CComPtr<IDispatch> obj;
			if (grpc::Status status = ResolvePersistId(ext, request->persist_ids(i), obj); !status.ok())
				return grpc::Status(status.error_code(), "item " + std::to_string(i + 1) + ": " + status.error_message());
			if (!ATL::CComQIPtr<IBody2>(obj.p) && !ATL::CComQIPtr<IComponent2>(obj.p))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"item " + std::to_string(i + 1) + " is neither a body nor a component");
			response->add_measured(BodyOrComponentName(obj));
			items.push_back(obj);
		}
		ATL::CComPtr<IMathTransform> coordinates;
		if (!request->coordinate_system().empty() &&
			(FAILED(ext->GetCoordinateSystemTransformByName(FromUtf8(request->coordinate_system()), &coordinates)) ||
				!coordinates))
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active document has no coordinate system named \"" + request->coordinate_system() + "\"");

		doc->ClearSelection2(VARIANT_TRUE);
		ATL::CComPtr<IDispatch> disp;
		HRESULT hr = ext->CreateMassProperty2(&disp);
		ATL::CComQIPtr<IMassProperty2> mass(disp.p);
		if (FAILED(hr) || !mass)
			return ComError("IModelDocExtension::CreateMassProperty2", hr);
		mass->put_UseSystemUnits(VARIANT_TRUE);
		mass->put_IncludeHiddenBodiesOrComponents(request->include_hidden() ? VARIANT_TRUE : VARIANT_FALSE);
		if (!items.empty())
		{
			std::vector<IDispatch*> raw;
			for (const auto& item : items)
				raw.push_back(item);
			if (FAILED(hr = mass->put_SelectedItems(comvariant::FromDispatches(raw))))
				return ComError("IMassProperty2::SelectedItems", hr);
		}
		VARIANT_BOOL ok = VARIANT_FALSE;
		if (coordinates && (FAILED(mass->SetCoordinateSystem(coordinates, &ok)) || !ok))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks won't take coordinate system \"" + request->coordinate_system() + "\" for mass properties");
		if (FAILED(mass->Recalculate(&ok)) || !ok)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks computes no mass properties here (no solid bodies?)");

		double value = 0;
		if (SUCCEEDED(mass->get_Mass(&value)))
			response->set_mass(value);
		if (SUCCEEDED(mass->get_Volume(&value)))
			response->set_volume(value);
		if (SUCCEEDED(mass->get_SurfaceArea(&value)))
			response->set_surface_area(value);
		if (SUCCEEDED(mass->get_Density(&value)))
			response->set_density(value);
		ATL::CComVariant v;
		if (SUCCEEDED(mass->get_CenterOfMass(&v)))
			if (const std::vector<double> c = comvariant::ToDoubles(v); c.size() >= 3)
				SetVec3(response->mutable_center_of_mass(), c.data());
		for (double m : MomentOfInertia(mass, swMassPropertyMomentAboutCenterOfMass))
			response->add_inertia_at_center(m);
		for (double m : MomentOfInertia(mass, swMassPropertyMomentAboutCoordSys))
			response->add_inertia_at_origin(m);
		v.Clear();
		if (SUCCEEDED(mass->get_PrincipalMomentsOfInertia(&v)))
			if (const std::vector<double> p = comvariant::ToDoubles(v); p.size() == 3)
				for (long axis = 0; axis < 3; ++axis)
				{
					ATL::CComVariant a;
					std::vector<double> d;
					if (FAILED(mass->get_PrincipalAxesOfInertia(axis, &a)) || (d = comvariant::ToDoubles(a)).size() < 3)
						break;
					response->add_principal_moments(p[axis]);
					SetVec3(response->add_principal_axes(), d.data());
				}
		return grpc::Status::OK;
	});
}
