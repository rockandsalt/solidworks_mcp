// HoleWizard.cpp : HoleWizard.
#include "pch.h"
#include "Common.h"
#include "SketchHelpers.h"

using namespace sld_mcp::service;

namespace
{
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
