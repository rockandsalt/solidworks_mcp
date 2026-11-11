// Revolve.cpp : RevolveSketch (boss and cut revolves).
#include "pch.h"
#include "Common.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection mark FeatureRevolve2 takes the axis from; the profile
	// sketch is selected with mark 0.
	constexpr long kAxisMark = 4;

	// The sketch's centerlines (construction lines).
	std::vector<ATL::CComPtr<IDispatch>> Centerlines(ISketch* sketch)
	{
		std::vector<ATL::CComPtr<IDispatch>> lines;
		ATL::CComVariant segments;
		if (FAILED(sketch->GetSketchSegments(&segments)))
			return lines;
		for (const auto& disp : comvariant::ToDispatches(segments))
		{
			ATL::CComQIPtr<ISketchSegment> segment(disp.p);
			long type = -1;
			VARIANT_BOOL construction = VARIANT_FALSE;
			if (segment && SUCCEEDED(segment->GetType(&type)) && type == swSketchLINE &&
				SUCCEEDED(segment->get_ConstructionGeometry(&construction)) && construction)
				lines.push_back(disp);
		}
		return lines;
	}

	// Whether a resolved reference can be an axis of revolution: a sketch
	// line, a straight edge or a reference axis.
	grpc::Status CheckAxis(const Pick& pick)
	{
		const auto notAnAxis = [&](const std::string& why) {
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"axis: " + pick.what + " " + why +
				" -- give a sketch line (e.g. a centerline), a straight edge or a reference axis");
		};
		switch (pick.kind)
		{
		case Pick::SketchSegment:
		{
			ATL::CComQIPtr<ISketchSegment> segment(pick.obj.p);
			long type = -1;
			if (!segment || FAILED(segment->GetType(&type)) || type != swSketchLINE)
				return notAnAxis("isn't a line");
			return grpc::Status::OK;
		}
		case Pick::Entity:
		{
			ATL::CComQIPtr<IEdge> edge(pick.obj.p);
			if (!edge)
				return notAnAxis("isn't an edge");
			ATL::CComPtr<ICurve> curve;
			VARIANT_BOOL line = VARIANT_FALSE;
			if (FAILED(edge->IGetCurve(&curve)) || !curve || FAILED(curve->IsLine(&line)) || !line)
				return notAnAxis("isn't straight");
			return grpc::Status::OK;
		}
		case Pick::Feature:
		{
			ATL::CComQIPtr<IFeature> feature(pick.obj.p);
			if (!feature || FeatureTypeName(feature) != "RefAxis")
				return notAnAxis("isn't a reference axis");
			return grpc::Status::OK;
		}
		default:
			return notAnAxis("is a point");
		}
	}
}

grpc::Status SolidWorksService::RevolveSketch(grpc::ServerContext* context,
	const sld_mcp::v1::RevolveSketchRequest* request,
	sld_mcp::v1::RevolveSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const bool cut = request->cut();
	const double angle = request->angle();
	if (!(std::isfinite(angle) && angle > 0.0 && angle <= 2 * M_PI + 1e-9))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "angle must be more than 0 and at most 360 degrees");
	const bool fullTurn = angle >= 2 * M_PI - 1e-9;

	return m_executor.Execute(context, "RevolveSketch", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;

		ATL::CComPtr<IModelDocExtension> ext;
		HRESULT hr = doc->get_Extension(&ext);
		if (FAILED(hr) || !ext)
			return ComError("IModelDoc2::Extension", hr);

		// Resolve the sketch: by name if given, else the latest unused one.
		ATL::CComPtr<IFeature> sketch;
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
		const std::string sketchName = FeatureName(sketch);

		ATL::CComPtr<ISketch> sketchData;
		if (grpc::Status status = SketchOf(sketch, sketchData); !status.ok())
			return status;
		// Informational: whether the profile is fully defined.
		response->set_sketch_status(SketchStatus(sketchData));

		// The axis: as given, else the sketch's one centerline.
		Selection axis;
		if (request->has_axis())
		{
			if (grpc::Status status = axis.Add(doc, ext, request->axis(), kAxisMark, "axis"); !status.ok())
				return status;
			if (request->axis().has_sketch_item())
				*response->mutable_axis_item() = request->axis().sketch_item();
		}
		else
		{
			const auto centerlines = Centerlines(sketchData);
			if (centerlines.size() != 1)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					sketchName + " has " + std::to_string(centerlines.size()) +
					" centerlines -- give the axis to revolve about (a sketch line, a straight edge or a "
					"reference axis), or draw one centerline (a construction line) in the sketch");
			axis.picks.push_back(Pick{Pick::SketchSegment, centerlines.front(), {}, "the centerline of " + sketchName});
			axis.marks.push_back(kAxisMark);
			SetItemRef(centerlines.front(), PersistIdOf(ext, sketch), response->mutable_axis_item());
		}
		if (grpc::Status status = CheckAxis(axis.picks.front()); !status.ok())
			return status;

		ATL::CComPtr<IFeatureManager> featureManager;
		hr = doc->get_FeatureManager(&featureManager);
		if (FAILED(hr))
			return ComError("IModelDoc2::FeatureManager", hr);
		if (!featureManager)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

		// Mid-plane splits the angle across both sides of the sketch plane and has
		// no direction, so `reverse_direction` doesn't apply.
		const bool midPlane = request->mid_plane();
		const VARIANT_BOOL reverse = (!midPlane && request->reverse_direction()) ? VARIANT_TRUE : VARIANT_FALSE;
		const long endCondition = midPlane ? swEndCondMidPlane : swEndCondBlind;

		// FeatureRevolve2 revolves the selected sketch (mark 0) about the
		// selected axis (kAxisMark).
		ATL::CComPtr<IFeature> revolve;
		grpc::Status selectStatus;
		auto create = [&](VARIANT_BOOL dir) -> HRESULT {
			revolve.Release();
			selectStatus = axis.Select(doc, ext);
			if (!selectStatus.ok())
				return S_OK;
			VARIANT_BOOL selected = VARIANT_FALSE;
			HRESULT selectHr = sketch->Select2(VARIANT_TRUE, 0, &selected);
			if (FAILED(selectHr))
				return selectHr;
			if (!selected)
			{
				selectStatus = grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select " + sketchName);
				return S_OK;
			}
			return featureManager->FeatureRevolve2(
				VARIANT_TRUE,      // SingleDir: one direction (or mid-plane)
				VARIANT_TRUE,      // IsSolid
				VARIANT_FALSE,     // IsThin
				cut ? VARIANT_TRUE : VARIANT_FALSE, // IsCut
				dir,               // ReverseDir
				VARIANT_FALSE,     // BothDirectionUpToSameEntity
				endCondition,      // Dir1Type
				swEndCondBlind,    // Dir2Type (unused, single direction)
				angle,             // Dir1Angle
				0.0,               // Dir2Angle
				VARIANT_FALSE,     // OffsetReverse1
				VARIANT_FALSE,     // OffsetReverse2
				0.0,               // OffsetDistance1
				0.0,               // OffsetDistance2
				0,                 // ThinType (not thin)
				0.0,               // ThinThickness1
				0.0,               // ThinThickness2
				VARIANT_TRUE,      // Merge with existing bodies
				VARIANT_TRUE,      // UseFeatScope
				VARIANT_TRUE,      // UseAutoSelect: all bodies in scope
				&revolve);
		};
		hr = create(reverse);
		// As with cut-extrudes: a partial cut the default way may miss the
		// material the other way would remove. Unless a direction was asked for,
		// try that before giving up (a failed attempt creates nothing).
		bool flipped = false;
		if (SUCCEEDED(hr) && selectStatus.ok() && !revolve && cut && !fullTurn && !midPlane && !reverse)
		{
			hr = create(VARIANT_TRUE);
			flipped = SUCCEEDED(hr) && revolve;
		}
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IFeatureManager::FeatureRevolve2", hr);
		if (!selectStatus.ok())
			return selectStatus;
		const std::string axisWhat = axis.picks.front().what;
		if (!revolve)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't " + std::string(cut ? "cut-revolve " : "revolve ") + sketchName + " about " +
				axisWhat + " -- the sketch must contain a closed, non-self-intersecting profile that lies on one "
				"side of the axis (touching it is fine, crossing it isn't), and the axis must lie in the sketch "
				"plane" + std::string(cut ? "; the cut must pass through material" : "") +
				std::string(cut && !fullTurn && !midPlane && !reverse ? " (tried both directions)" : ""));

		response->set_feature_name(FeatureName(revolve));
		response->set_sketch_name(sketchName);
		response->set_axis(axisWhat);
		response->set_direction_flipped(flipped);
		return grpc::Status::OK;
	});
}
