// Tolerances.cpp : SetDimensionTolerance (see docs/notes/drawings.md).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// A dimension found by its full name, with the display dimension showing
	// it when there is one (needed for precision).
	struct NamedDimension
	{
		ATL::CComPtr<IDimension> dim;
		ATL::CComPtr<IDisplayDimension> display;
		std::string viewName;  // drawings
		std::string fullName;
		std::vector<std::string> otherViews;  // also shown there
	};

	std::string DimensionFullName(IDimension* dim)
	{
		ATL::CComBSTR name;
		return SUCCEEDED(dim->get_FullName(&name)) ? ToUtf8(name) : std::string();
	}

	// Display dimensions among `annotations` named `name`: exactly, or with
	// the trailing document part left off ("RD1@Drawing View1" for
	// "RD1@Drawing View1@Draw1.Drawing", "D1@Boss-Extrude1" for
	// "D1@Boss-Extrude1@block.Part"). Exact matches go to `exact`.
	void MatchDimensions(const std::vector<ATL::CComPtr<IAnnotation>>& annotations, const std::string& name,
		const std::string& viewName, std::vector<NamedDimension>& exact, std::vector<NamedDimension>& partial)
	{
		for (const auto& annotation : annotations)
		{
			long type = 0;
			if (FAILED(annotation->GetType(&type)) || type != swDisplayDimension)
				continue;
			ATL::CComPtr<IDispatch> specific;
			if (FAILED(annotation->GetSpecificAnnotation(&specific)) || !specific)
				continue;
			ATL::CComQIPtr<IDisplayDimension> display(specific.p);
			ATL::CComPtr<IDimension> dim;
			if (!display || FAILED(display->GetDimension2(0, &dim)) || !dim)
				continue;
			const std::string full = DimensionFullName(dim);
			const bool isExact = full == name;
			if (!isExact && full.compare(0, name.size() + 1, name + "@") != 0)
				continue;
			NamedDimension found;
			found.dim = dim;
			found.display = display;
			found.viewName = viewName;
			found.fullName = full;
			(isExact ? exact : partial).push_back(found);
		}
	}

	// Drawings: the views' annotations on every sheet (what InspectDrawing
	// lists, under the same names). Parts and assemblies:
	// IModelDoc2::Parameter (sketch, feature and mate dimensions; the display
	// dimension from the owning feature), else the document's annotations
	// (DimXpert, reference dimensions). NOT_FOUND, or INVALID_ARGUMENT if a
	// shortened name fits several.
	grpc::Status FindNamedDimension(IModelDoc2* doc, long docType, const std::string& name, NamedDimension& out)
	{
		std::vector<NamedDimension> exact, partial;
		if (docType == swDocDRAWING)
		{
			if (ATL::CComQIPtr<IDrawingDoc> drawing(doc); drawing)
				for (const auto& sheet : DrawingSheets(drawing))
				{
					std::vector<ATL::CComPtr<IView>> views = sheet.views;
					if (sheet.sheetView)
						views.insert(views.begin(), sheet.sheetView);
					for (const auto& view : views)
						MatchDimensions(ViewAnnotationList(view), name, ViewName(view), exact, partial);
				}
		}
		else
		{
			ATL::CComPtr<IDispatch> param;
			if (SUCCEEDED(doc->Parameter(FromUtf8(name), &param)) && param)
				if (ATL::CComQIPtr<IDimension> dim(param.p); dim)
				{
					out.dim = dim;
					out.fullName = DimensionFullName(dim);
					ATL::CComPtr<IFeature> owner;
					if (SUCCEEDED(dim->GetFeatureOwner(&owner)) && owner)
						for (const auto& display : FeatureDimensions(owner))
						{
							ATL::CComPtr<IDimension> shown;
							if (SUCCEEDED(display->GetDimension2(0, &shown)) && shown &&
								DimensionFullName(shown) == out.fullName)
							{
								out.display = display;
								break;
							}
						}
					return grpc::Status::OK;
				}
			ATL::CComPtr<IModelDocExtension> ext;
			if (SUCCEEDED(doc->get_Extension(&ext)) && ext)
			{
				bool truncated = false;
				MatchDimensions(AllAnnotations(doc, ext, truncated), name, std::string(), exact, partial);
			}
		}

		// Views showing one model dimension (same full name) show the same
		// IDimension: one hit, listed in each view.
		std::vector<NamedDimension> hits;
		for (const auto& hit : exact.empty() ? partial : exact)
		{
			auto same = std::find_if(hits.begin(), hits.end(),
				[&](const NamedDimension& h) { return h.fullName == hit.fullName; });
			if (same == hits.end())
				hits.push_back(hit);
			else if (!hit.viewName.empty() && hit.viewName != same->viewName &&
				std::find(same->otherViews.begin(), same->otherViews.end(), hit.viewName) == same->otherViews.end())
				same->otherViews.push_back(hit.viewName);
		}
		if (hits.size() == 1)
		{
			out = hits[0];
			return grpc::Status::OK;
		}
		if (hits.empty())
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active document has no dimension named \"" + name + "\" -- give the name " +
				(docType == swDocDRAWING ? "inspect_drawing lists, e.g. RD1@Drawing View1 or D1@Boss-Extrude1@block.Part"
				                         : "e.g. D1@Sketch1 or D1@Boss-Extrude1"));
		std::string names;
		for (size_t i = 0; i < hits.size() && i < 10; ++i)
			names += (i ? ", " : "") + hits[i].fullName + (hits[i].viewName.empty() ? "" : " (" + hits[i].viewName + ")");
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"\"" + name + "\" fits several dimensions: " + names + " -- give the full name");
	}

	struct ToleranceState
	{
		long type = swTolNONE;
		double max = 0.0;  // upper deviation
		double min = 0.0;  // lower deviation
		ATL::CComBSTR holeFit;
		ATL::CComBSTR shaftFit;
	};

	ToleranceState ReadTolerance(IDimensionTolerance* tolerance)
	{
		ToleranceState s;
		tolerance->get_Type(&s.type);
		tolerance->GetMaxValue(&s.max);
		tolerance->GetMinValue(&s.min);
		tolerance->GetHoleFitValue(&s.holeFit);
		tolerance->GetShaftFitValue(&s.shaftFit);
		return s;
	}

	bool ToleranceHasValues(long type)
	{
		return type == swTolBILAT || type == swTolLIMIT || type == swTolSYMMETRIC;
	}

	bool ToleranceHasFits(long type)
	{
		return type == swTolFIT || type == swTolFITWITHTOL || type == swTolFITTOLONLY;
	}

	// Type first (SolidWorks may reset the rest with it), then deviations
	// and fit classes as the type uses them. False if SolidWorks refused
	// any of it or ended up with another type.
	bool WriteTolerance(IDimensionTolerance* tolerance, const ToleranceState& s)
	{
		if (FAILED(tolerance->put_Type(s.type)))
			return false;
		if (ToleranceHasValues(s.type))
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(tolerance->SetValues2(s.min, s.max, swThisConfiguration, ATL::CComVariant(), &ok)) || !ok)
			{
				ok = VARIANT_FALSE;
				if (FAILED(tolerance->SetValues(s.min, s.max, &ok)) || !ok)
					return false;
			}
		}
		if (ToleranceHasFits(s.type))
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(tolerance->SetFitValues(s.holeFit, s.shaftFit, &ok)) || !ok)
				return false;
		}
		long now = swTolNONE;
		return SUCCEEDED(tolerance->get_Type(&now)) && now == s.type;
	}
}

grpc::Status SolidWorksService::SetDimensionTolerance(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::SetDimensionToleranceRequest* request,
	sld_mcp::v1::SetDimensionToleranceResponse* response)
{
	const long type = request->tolerance_type();
	if (type < swTolNONE || type > swTolFITTOLONLY)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unsupported tolerance type " + std::to_string(type));
	if (request->name().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the dimension's full name, e.g. RD1@Drawing View1");
	if (ToleranceHasValues(type) &&
		!(std::isfinite(request->upper()) && std::isfinite(request->lower()) && request->upper() >= request->lower()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the upper deviation must be at least the lower one");
	if (ToleranceHasFits(type) && request->hole_fit().empty() && request->shaft_fit().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a fit tolerance needs a hole and/or shaft fit class");
	if (request->has_value_precision() && (request->value_precision() < -2 || request->value_precision() > 8))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "value_precision must be -2 or 0..8");
	if (request->has_tolerance_precision() &&
		(request->tolerance_precision() < -3 || request->tolerance_precision() == -1 || request->tolerance_precision() > 8))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "tolerance_precision must be -3, -2 or 0..8");

	std::lock_guard<std::mutex> lock(m_editMutex);
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
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
	long docType = swDocNONE;
	hr = doc->GetType(&docType);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetType", hr);

	NamedDimension found;
	if (grpc::Status status = FindNamedDimension(doc, docType, request->name(), found); !status.ok())
		return status;

	long paramType = swDimensionParamTypeUnknown;
	found.dim->GetType(&paramType);
	if (ToleranceHasValues(type))
	{
		if (paramType == swDimensionParamTypeDoubleAngular && !request->angle())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + request->name() + "\" is an angle; give the deviations in degrees");
		if (paramType == swDimensionParamTypeDoubleLinear && request->angle())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + request->name() + "\" is a length; give the deviations in millimeters");
	}
	const bool precision = request->has_value_precision() || request->has_tolerance_precision();
	if (precision && !found.display)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"couldn't find where \"" + request->name() + "\" is displayed, so its decimal places can't be set");

	ATL::CComPtr<IDimensionTolerance> tolerance;
	hr = found.dim->get_Tolerance(&tolerance);
	if (FAILED(hr))
		return ComError("IDimension::Tolerance", hr);
	if (!tolerance)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks gives no tolerance for \"" + request->name() + "\"");

	const ToleranceState old = ReadTolerance(tolerance);
	ToleranceState wanted;
	wanted.type = type;
	wanted.max = request->upper();
	wanted.min = request->lower();
	wanted.holeFit = FromUtf8(request->hole_fit());
	wanted.shaftFit = FromUtf8(request->shaft_fit());
	if (!WriteTolerance(tolerance, wanted))
	{
		WriteTolerance(tolerance, old);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks refused that tolerance for \"" + request->name() + "\" (an unknown fit class, or a "
			"dimension that can't be toleranced?) -- the old tolerance was put back");
	}

	if (precision)
	{
		long oldValue = swDoNotChangePrecisionSetting, oldTolerance = swDoNotChangePrecisionSetting;
		found.display->GetPrimaryPrecision2(&oldValue);
		found.display->GetPrimaryTolPrecision2(&oldTolerance);
		const long value = request->has_value_precision() ? request->value_precision() : swDoNotChangePrecisionSetting;
		const long tol = request->has_tolerance_precision() ? request->tolerance_precision() : swDoNotChangePrecisionSetting;
		long result = 0;
		hr = found.display->SetPrecision3(value, swDoNotChangePrecisionSetting, tol, swDoNotChangePrecisionSetting, &result);
		long nowValue = -1, nowTolerance = -1;
		found.display->GetPrimaryPrecision2(&nowValue);
		found.display->GetPrimaryTolPrecision2(&nowTolerance);
		const bool valueOk = value < 0 || nowValue == value;
		const bool tolOk = tol < 0 || nowTolerance == tol;
		if (FAILED(hr) || !valueOk || !tolOk)
		{
			found.display->SetPrecision3(oldValue, swDoNotChangePrecisionSetting, oldTolerance,
				swDoNotChangePrecisionSetting, &result);
			WriteTolerance(tolerance, old);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't take those decimal places for \"" + request->name() + "\" (SetPrecision3 -> " +
				std::to_string(result) + ") -- the old tolerance was put back");
		}
	}

	doc->SetSaveFlag();
	doc->GraphicsRedraw2();

	sld_mcp::v1::AnnotationDimension* out = response->mutable_dimension();
	if (found.display)
		DescribeAnnotationDimension(found.display, out);
	else
	{
		DescribeDimensionValue(found.dim, out->mutable_dimension());
		out->mutable_dimension()->set_type(paramType == swDimensionParamTypeDoubleAngular
			? sld_mcp::v1::DIMENSION_TYPE_ANGULAR : sld_mcp::v1::DIMENSION_TYPE_LINEAR);
		DescribeModelDimension(found.dim, out);
	}
	response->set_view_name(found.viewName);
	for (const auto& view : found.otherViews)
		response->add_other_views(view);
	long nowValue = -1, nowTolerance = -1;
	if (found.display)
	{
		found.display->GetPrimaryPrecision2(&nowValue);
		found.display->GetPrimaryTolPrecision2(&nowTolerance);
	}
	response->set_value_precision(static_cast<int32_t>(nowValue));
	response->set_tolerance_precision(static_cast<int32_t>(nowTolerance));
	return grpc::Status::OK;
}
