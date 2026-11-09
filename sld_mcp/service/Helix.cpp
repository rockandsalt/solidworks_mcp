// Helix.cpp : CreateHelix (helix and spiral curves).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The request's definition as swHelixDefinedBy_e, checking that the values
	// it uses are there.
	grpc::Status HelixDefinedBy(const sld_mcp::v1::CreateHelixRequest& request, long& definedBy)
	{
		bool needHeight = false, needPitch = false, needRevolutions = false;
		switch (request.definition())
		{
		case sld_mcp::v1::HELIX_DEFINITION_PITCH_AND_REVOLUTIONS:
			definedBy = swHelixDefinedByPitchAndRevolution;
			needPitch = needRevolutions = true;
			break;
		case sld_mcp::v1::HELIX_DEFINITION_HEIGHT_AND_REVOLUTIONS:
			definedBy = swHelixDefinedByHeightAndRevolution;
			needHeight = needRevolutions = true;
			break;
		case sld_mcp::v1::HELIX_DEFINITION_HEIGHT_AND_PITCH:
			definedBy = swHelixDefinedByHeightAndPitch;
			needHeight = needPitch = true;
			break;
		case sld_mcp::v1::HELIX_DEFINITION_SPIRAL:
			definedBy = swHelixDefinedBySpiral;
			needPitch = needRevolutions = true;
			break;
		default:
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say how the helix is defined");
		}
		auto positive = [](double v) { return std::isfinite(v) && v > 0.0; };
		if ((needHeight && !positive(request.height())) || (needPitch && !positive(request.pitch())) ||
			(needRevolutions && !positive(request.revolutions())))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"height, pitch and revolutions must be positive");
		return grpc::Status::OK;
	}

	// The one circle of the sketch, which is what InsertHelix wants: anything
	// else makes no feature and no dialog (seen 2026-10-31).
	grpc::Status FindHelixCircle(IFeature* sketchFeature, ATL::CComPtr<ISketch>& sketch,
		sld_mcp::v1::SketchItem& circle)
	{
		const std::string name = FeatureName(sketchFeature);
		if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
			return status;
		SketchItems items;
		items.Load(sketch);
		int circles = 0, others = 0;
		for (const auto& e : items.All())
		{
			sld_mcp::v1::SketchItem item;
			if (e.point || !DescribeSketchItem(e.obj, std::string(), &item) || item.construction())
				continue;
			if (item.type() == sld_mcp::v1::SKETCH_ITEM_TYPE_CIRCLE)
			{
				circle = item;
				++circles;
			}
			else
				++others;
		}
		if (circles != 1 || others != 0)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"a helix is made from a sketch holding exactly one circle, but " + name + " has " +
				std::to_string(circles) + " circle(s) and " + std::to_string(others) +
				" other segment(s) -- draw the circle in a sketch of its own");
		return grpc::Status::OK;
	}

	// The finished curve's ends and length. Informational, so a failure just
	// leaves them out.
	void DescribeHelixCurve(IFeature* helix, sld_mcp::v1::CreateHelixResponse* response)
	{
		ATL::CComPtr<IDispatch> specific;
		if (FAILED(helix->GetSpecificFeature2(&specific)))
			return;
		ATL::CComQIPtr<IReferenceCurve> curve(specific.p);
		ATL::CComPtr<IEdge> edge;
		if (!curve || FAILED(curve->IGetFirstSegment(&edge)) || !edge)
			return;
		ATL::CComVariant params;
		if (FAILED(edge->GetCurveParams2(&params)))
			return;
		// Start point, end point, start and end parameters.
		const std::vector<double> c = comvariant::ToDoubles(params);
		if (c.size() < 8)
			return;
		SetVec3(response->mutable_start(), &c[0]);
		SetVec3(response->mutable_end(), &c[3]);
		ATL::CComPtr<ICurve> geometry;
		double length = 0.0;
		if (SUCCEEDED(edge->IGetCurve(&geometry)) && geometry && SUCCEEDED(geometry->GetLength3(c[6], c[7], &length)))
			response->set_length(length);
	}
}

grpc::Status SolidWorksService::CreateHelix(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CreateHelixRequest* request,
	sld_mcp::v1::CreateHelixResponse* response)
{
	// Validate before touching SolidWorks.
	long definedBy = swHelixDefinedByPitchAndRevolution;
	if (grpc::Status status = HelixDefinedBy(*request, definedBy); !status.ok())
		return status;
	const double taper = request->taper_angle();
	const bool tapered = taper != 0.0;
	if (!std::isfinite(request->start_angle()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "start angle must be a number");
	if (tapered && !(std::isfinite(taper) && taper > 0.0 && taper < M_PI_2))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a taper angle must be between 0 and 90 degrees");
	if (tapered && definedBy == swHelixDefinedBySpiral)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a spiral can't be tapered");

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

	ATL::CComPtr<IFeature> sketchFeature;
	if (request->sketch_name().empty())
	{
		if (grpc::Status status = FindLatestUnusedSketch(doc, sketchFeature); !status.ok())
			return status;
	}
	else if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
		return status;
	const std::string sketchName = FeatureName(sketchFeature);

	ATL::CComPtr<ISketch> sketch;
	sld_mcp::v1::SketchItem circle;
	if (grpc::Status status = FindHelixCircle(sketchFeature, sketch, circle); !status.ok())
		return status;

	// The circle's center in model space. Informational.
	SketchSpace space;
	if (space.Init(swApp).ok())
	{
		space.Attach(sketch);
		const double inSketch[3] = {circle.center().x(), circle.center().y(), 0.0};
		double center[3];
		if (space.ToModel(inSketch, center))
			SetVec3(response->mutable_center(), center);
	}

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	VARIANT_BOOL selected = VARIANT_FALSE;
	hr = sketchFeature->Select2(VARIANT_FALSE, 0, &selected);
	if (FAILED(hr))
		return ComError("IFeature::Select2", hr);
	if (!selected)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select " + sketchName);

	// InsertHelix returns nothing: the new feature is the new last one.
	ATL::CComPtr<IFeature> lastBefore;
	doc->IFeatureByPositionReverse(0, &lastBefore);
	hr = doc->InsertHelix(
		request->reverse_direction() ? VARIANT_TRUE : VARIANT_FALSE,
		request->clockwise() ? VARIANT_TRUE : VARIANT_FALSE,
		tapered ? VARIANT_TRUE : VARIANT_FALSE,
		request->taper_outward() ? VARIANT_TRUE : VARIANT_FALSE,
		definedBy,
		request->height(),
		request->pitch(),
		request->revolutions(),
		taper,
		request->start_angle());
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::InsertHelix", hr);
	ATL::CComPtr<IFeature> helix;
	doc->IFeatureByPositionReverse(0, &helix);

	const std::string why =
		"check the sizes: too few turns, or a taper that narrows the helix down to nothing, won't build";
	if (!helix || SameObject(helix, lastBefore))
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't make a helix from " + sketchName + " -- " + why);
	VARIANT_BOOL warning = VARIANT_FALSE;
	long error = swFeatureErrorNone;
	if (SUCCEEDED(helix->GetErrorCode2(&warning, &error)) && error != swFeatureErrorNone && !warning)
	{
		const std::string name = FeatureName(helix);
		DeleteFeature(doc, helix);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks made " + name + " but it failed to rebuild (swFeatureError_e " + std::to_string(error) +
			"), so it was deleted again -- " + why);
	}

	response->set_feature_name(FeatureName(helix));
	response->set_sketch_name(sketchName);
	response->set_radius(circle.radius());
	ATL::CComPtr<IDispatch> definition;
	if (SUCCEEDED(helix->GetDefinition(&definition)))
		if (ATL::CComQIPtr<IHelixFeatureData> data(definition.p); data)
		{
			double value = 0.0;
			if (SUCCEEDED(data->get_Height(&value)))
				response->set_height(value);
			if (SUCCEEDED(data->get_Pitch(&value)))
				response->set_pitch(value);
			if (SUCCEEDED(data->get_Revolution(&value)))
				response->set_revolutions(value);
		}
	DescribeHelixCurve(helix, response);
	return grpc::Status::OK;
}
