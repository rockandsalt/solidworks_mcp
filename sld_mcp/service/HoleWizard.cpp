// HoleWizard.cpp : HoleWizard.
#include "pch.h"
#include "Common.h"
#include "FeatureTreeHelpers.h"
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

	using HoleRequest = sld_mcp::v1::HoleWizardRequest;
	using Point3 = std::array<double, 3>;
	// Sketch points by ISketchPoint::GetID.
	using PointIds = std::set<std::pair<long long, long long>>;

	grpc::Status Invalid(const std::string& why)
	{
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, why);
	}

	// Checks the request before anything touches SolidWorks. `genericType` =
	// the swWzdGeneralHoleTypes_e of its kind.
	grpc::Status ValidateHoleRequest(const HoleRequest& request, long& genericType)
	{
		switch (request.kind())
		{
		case sld_mcp::v1::HOLE_KIND_HOLE:        genericType = swWzdHole; break;
		case sld_mcp::v1::HOLE_KIND_COUNTERBORE: genericType = swWzdCounterBore; break;
		case sld_mcp::v1::HOLE_KIND_COUNTERSINK: genericType = swWzdCounterSink; break;
		case sld_mcp::v1::HOLE_KIND_TAP:         genericType = swWzdTap; break;
		default: return Invalid("kind must be HOLE, COUNTERBORE, COUNTERSINK or TAP");
		}
		const bool fromSketch = !request.sketch_name().empty();
		if (fromSketch && (!request.face_persist_id().empty() || !request.locations().empty()))
			return Invalid("give either a sketch of hole points or a face (with locations), not both");
		if (!fromSketch && request.face_persist_id().empty())
			return Invalid("give a sketch of hole points, or the face to put the holes on");
		if (request.size().empty())
			return Invalid("give the hole size, e.g. \"M5\"");
		if (request.locations_size() > kMaxHoleLocations)
			return Invalid("too many hole locations in one call (max " + std::to_string(kMaxHoleLocations) + ")");
		for (const auto& location : request.locations())
			if (!IsFinite(location))
				return Invalid("hole locations must be finite");
		if (!request.through_all() && !(std::isfinite(request.depth()) && request.depth() > 0.0))
			return Invalid("depth must be positive (or set through_all)");
		for (const auto& [has, value] : {
				std::pair{request.has_diameter(), request.diameter()},
				std::pair{request.has_counterbore_diameter(), request.counterbore_diameter()},
				std::pair{request.has_counterbore_depth(), request.counterbore_depth()},
				std::pair{request.has_countersink_diameter(), request.countersink_diameter()},
				std::pair{request.has_thread_depth(), request.thread_depth()}})
			if (has && !(std::isfinite(value) && value > 0.0))
				return Invalid("hole dimensions must be positive");
		if (request.has_countersink_angle() &&
			!(std::isfinite(request.countersink_angle()) && request.countersink_angle() > 0.0 &&
				request.countersink_angle() < M_PI))
			return Invalid("the countersink angle must be between 0 and 180 degrees");
		return grpc::Status::OK;
	}

	// An optional size, or -1 = "use the standard's value".
	double OrStandard(bool has, double value)
	{
		return has ? value : -1.0;
	}

	// Value1..Value12 of HoleWizard5, which mean different things per hole
	// type; -1 = "use the standard's value".
	std::array<double, 12> WizardValues(const HoleRequest& request, long genericType)
	{
		double fit = -1.0;
		switch (request.fit())
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
			values[0] = OrStandard(request.has_counterbore_diameter(), request.counterbore_diameter());
			values[1] = OrStandard(request.has_counterbore_depth(), request.counterbore_depth());
			values[3] = fit;
			break;
		case swWzdCounterSink:
			// C-sink diameter, c-sink angle, head clearance, screw fit, ...
			values[0] = OrStandard(request.has_countersink_diameter(), request.countersink_diameter());
			values[1] = OrStandard(request.has_countersink_angle(), request.countersink_angle());
			values[3] = fit;
			break;
		case swWzdHole:
			// Screw fit, drill angle, ...
			values[0] = fit;
			break;
		case swWzdTap:
			// Thread depth, near/far c-sink diameter/angle, drill angle, cosmetic
			// thread type, thread end condition, ...
			values[0] = OrStandard(request.has_thread_depth(), request.thread_depth());
			values[6] = request.cosmetic_thread() ? static_cast<double>(swCosmeticThreadWithCallout) : -1.0;
			break;
		}
		return values;
	}

	// HoleWizard5 with the requested hole, which puts holes wherever the
	// current selection says.
	class HoleWizardCall
	{
	public:
		HoleWizardCall(IFeatureManager* featureManager, const HoleRequest& request, long genericType)
			: m_featureManager(featureManager), m_request(request), m_genericType(genericType),
			  m_values(WizardValues(request, genericType)), m_size(FromUtf8(request.size()))
		{
		}

		HRESULT Run(ATL::CComPtr<IFeature>& hole) const
		{
			hole.Release();
			const bool throughAll = m_request.through_all();
			const ATL::CComBSTR threadClass(L"");
			const std::array<double, 12>& v = m_values;
			return m_featureManager->HoleWizard5(
				m_genericType,
				m_request.standard(),
				m_request.fastener_type(),
				m_size,
				static_cast<short>(throughAll ? swEndCondThroughAll : swEndCondBlind),
				OrStandard(m_request.has_diameter(), m_request.diameter()),
				throughAll ? -1.0 : m_request.depth(),
				-1.0,               // Length: slots only
				v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9], v[10], v[11],
				threadClass,        // ANSI inch thread class; empty = default
				m_request.reverse_direction() ? VARIANT_TRUE : VARIANT_FALSE,
				VARIANT_TRUE,       // FeatureScope
				VARIANT_TRUE,       // AutoSelect: all bodies
				VARIANT_FALSE,      // AssemblyFeatureScope (assemblies only)
				VARIANT_FALSE,      // AutoSelectComponents
				VARIANT_FALSE,      // PropagateFeatureToParts
				&hole);
		}

		// What to check when SolidWorks makes no hole.
		std::string BadHoleHint() const
		{
			return "check that size \"" + m_request.size() + "\" exists for that standard and fastener type "
				"(written as the Hole Wizard lists it, e.g. \"M5\" or \"#10\"), and that the holes go into "
				"material (try reverse_direction)";
		}

	private:
		IFeatureManager* m_featureManager;
		const HoleRequest& m_request;
		long m_genericType;
		std::array<double, 12> m_values;
		ATL::CComBSTR m_size;
	};

	// What the Hole Wizard built.
	struct HoleResult
	{
		ATL::CComPtr<IFeature> hole;
		ATL::CComPtr<IFeature> placementFeature;
		ATL::CComPtr<ISketch> placement;
		// Points of the placement sketch that aren't hole centers (ends and
		// centers of construction geometry in a user's sketch).
		PointIds notCenters;
	};

	// ---- Holes at the points of the user's sketch ------------------------------

	bool UsedByFeature(IFeature* sketchFeature)
	{
		long children = 0;
		return SUCCEEDED(sketchFeature->IGetChildCount(&children)) && children > 0;
	}

	// The points of a user's sketch that are hole centers. Holes go at sketch
	// points, so the sketch may only hold points and construction geometry,
	// whose end/center points aren't holes (they go into `notCenters`).
	grpc::Status HoleCentersOf(ISketch* sketch, const std::string& name,
		std::vector<ATL::CComPtr<ISketchPoint>>& centers, PointIds& notCenters)
	{
		ATL::CComVariant segments;
		const HRESULT hr = sketch->GetSketchSegments(&segments);
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
		for (const auto& point : SketchPoints(sketch))
		{
			bool isPoint = false;
			long long id0 = 0, id1 = 0;
			ATL::CComQIPtr<ISketchPoint> sketchPoint(point.p);
			if (sketchPoint && SketchEntityId(point, isPoint, id0, id1) && !notCenters.count({id0, id1}))
				centers.push_back(sketchPoint);
		}
		if (centers.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" has no sketch points to put holes at -- add them with sketch_point");
		if (centers.size() > static_cast<size_t>(kMaxHoleLocations))
			return Invalid("too many hole points in one sketch (max " + std::to_string(kMaxHoleLocations) + ")");
		return grpc::Status::OK;
	}

	// Runs the wizard with the sketch preselected, like in the UI. Tried two
	// ways: the sketch feature, then its points. Either only counts if the new
	// feature absorbed the sketch; otherwise the hole is deleted again and
	// `hole` stays null.
	HRESULT RunWizardOnSketch(IModelDoc2* doc, IModelDocExtension* ext, const HoleWizardCall& wizard,
		IFeature* sketchFeature, const std::vector<ATL::CComPtr<ISketchPoint>>& centers,
		ATL::CComPtr<IFeature>& hole)
	{
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
				for (size_t i = 0; i < centers.size() && selected; ++i)
				{
					VARIANT_BOOL ok = VARIANT_FALSE;
					selected = SUCCEEDED(centers[i]->Select4(i > 0 ? VARIANT_TRUE : VARIANT_FALSE, nullptr, &ok)) && ok;
				}
			if (!selected)
				continue;
			const HRESULT hr = wizard.Run(hole);
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return hr;
			if (hole && !UsedByFeature(sketchFeature))
			{
				// Holes went somewhere else: not what was asked.
				DeleteFeatureAndSketches(doc, ext, hole);
				hole.Release();
			}
		}
		return S_OK;
	}

	// The user's own sketch of (dimensioned) points becomes the hole feature's
	// placement sketch, so its dimensions keep driving the holes.
	grpc::Status HolesFromSketch(IModelDoc2* doc, IModelDocExtension* ext, const HoleWizardCall& wizard,
		const std::string& sketchName, HoleResult& result)
	{
		ATL::CComPtr<IFeature> sketchFeature;
		if (grpc::Status status = FindSketchByName(doc, sketchName, sketchFeature); !status.ok())
			return status;
		const std::string name = FeatureName(sketchFeature);
		if (UsedByFeature(sketchFeature))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" is already used by a feature -- put the hole points in a sketch of their own");
		ATL::CComPtr<ISketch> sketch;
		if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
			return status;

		std::vector<ATL::CComPtr<ISketchPoint>> centers;
		if (grpc::Status status = HoleCentersOf(sketch, name, centers, result.notCenters); !status.ok())
			return status;
		if (const HRESULT hr = RunWizardOnSketch(doc, ext, wizard, sketchFeature, centers, result.hole); FAILED(hr))
			return ComError("IFeatureManager::HoleWizard5", hr);
		if (!result.hole)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks couldn't put holes at the points of \"" + name + "\" -- " + wizard.BadHoleHint() +
				". The sketch must be on a planar face or plane; or give the face and locations instead");
		result.placementFeature = sketchFeature;
		return SketchOf(result.placementFeature, result.placement);
	}

	// ---- Holes at locations on a face -------------------------------------------

	// Selects a planar face at a point with a short ray straight down onto it,
	// starting outside the material. Selecting the face at a location is what
	// tells the Hole Wizard to put a hole there; one selection per hole.
	class FaceRays
	{
	public:
		FaceRays(IModelDoc2* doc, IModelDocExtension* ext, ISelectionMgr* selections, const FacePlane& plane)
			: m_doc(doc), m_ext(ext), m_selections(selections), m_plane(plane)
		{
		}

		bool SelectAt(const Point3& p, bool append) const
		{
			const double* n = m_plane.normal;
			VARIANT_BOOL selected = VARIANT_FALSE;
			const HRESULT hr = m_ext->SelectByRay(
				p[0] + n[0] * kHoleRayLift, p[1] + n[1] * kHoleRayLift, p[2] + n[2] * kHoleRayLift,
				-n[0], -n[1], -n[2], kHoleRayRadius, swSelFACES, append ? VARIANT_TRUE : VARIANT_FALSE, 0,
				swSelectOptionDefault, &selected);
			return SUCCEEDED(hr) && selected;
		}

		// Selects the first `n` points; returns how many selections stuck.
		size_t SelectFirst(const std::vector<Point3>& points, size_t n) const
		{
			m_doc->ClearSelection2(VARIANT_TRUE);
			for (size_t i = 0; i < n; ++i)
				if (!SelectAt(points[i], i > 0))
					break;
			return static_cast<size_t>(SelectedCount());
		}

		// What a ray at `p` selects; null unless it's exactly one thing.
		ATL::CComPtr<IDispatch> HitAt(const Point3& p) const
		{
			m_doc->ClearSelection2(VARIANT_TRUE);
			ATL::CComPtr<IDispatch> hit;
			if (SelectAt(p, false) && SelectedCount() == 1)
				m_selections->GetSelectedObject6(1, -1, &hit);
			return hit;
		}

	private:
		long SelectedCount() const
		{
			long count = 0;
			m_selections->GetSelectedObjectCount2(-1, &count);
			return count;
		}

		IModelDoc2* m_doc;
		IModelDocExtension* m_ext;
		ISelectionMgr* m_selections;
		FacePlane m_plane;
	};

	// Where on the face the holes go, projected onto its plane: the requested
	// locations, or else the center of the face's box.
	grpc::Status HoleLocations(IFace2* face, const FacePlane& plane, const HoleRequest& request,
		std::vector<Point3>& points)
	{
		for (const auto& location : request.locations())
		{
			const double p[3] = {location.x(), location.y(), location.z()};
			points.push_back(ProjectOntoPlane(plane, p));
		}
		if (!points.empty())
			return grpc::Status::OK;

		ATL::CComVariant box;
		const HRESULT hr = face->GetBox(&box);
		if (FAILED(hr))
			return ComError("IFace2::GetBox", hr);
		const std::vector<double> b = comvariant::ToDoubles(box);
		if (b.size() < 6)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no box for the face");
		const double center[3] = {(b[0] + b[3]) / 2.0, (b[1] + b[4]) / 2.0, (b[2] + b[5]) / 2.0};
		points.push_back(ProjectOntoPlane(plane, center));
		return grpc::Status::OK;
	}

	// Checks, before changing anything, that every location really is on the
	// face (and not, say, in a hole through it, where the ray would hit
	// something deeper -- though a location inside an earlier hole got through
	// in a test, 2026-10-26).
	grpc::Status CheckLocationsOnFace(IModelDoc2* doc, IModelDocExtension* ext, const FaceRays& rays,
		const std::string& facePid, const std::vector<Point3>& points, bool atBoxCenter)
	{
		for (size_t i = 0; i < points.size(); ++i)
		{
			const ATL::CComPtr<IDispatch> hit = rays.HitAt(points[i]);
			if (!hit || PersistIdOf(ext, hit) != facePid)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return Invalid(atBoxCenter
					? "the center of the face's bounding box " + PointText(points[i]) +
						" isn't on the face -- give locations"
					: "hole location " + std::to_string(i + 1) + " " + PointText(points[i]) +
						" isn't on that face (after projecting it onto the face's plane)");
			}
		}
		doc->ClearSelection2(VARIANT_TRUE);
		return grpc::Status::OK;
	}

	// Runs the wizard on the face. Normally every location is selected up
	// front. If SolidWorks won't hold the same face selected more than once, or
	// the wizard doesn't use every selection, falls back to the first location
	// only. `direct` = how many of the locations the hole feature got.
	grpc::Status RunWizardOnFace(IModelDoc2* doc, IModelDocExtension* ext, const HoleWizardCall& wizard,
		const FaceRays& rays, const std::vector<Point3>& points, HoleResult& result, size_t& direct)
	{
		auto selectFirstOnly = [&]() {
			direct = 1;
			if (rays.SelectFirst(points, 1) == 1)
				return grpc::Status::OK;
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the face");
		};

		direct = points.size();
		if (rays.SelectFirst(points, direct) != direct)
			if (grpc::Status status = selectFirstOnly(); !status.ok())
				return status;

		for (int attempt = 0; attempt < 2; ++attempt)
		{
			const HRESULT hr = wizard.Run(result.hole);
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IFeatureManager::HoleWizard5", hr);
			if (!result.hole)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks couldn't create the hole -- " + wizard.BadHoleHint());
			if (grpc::Status status = FindHolePlacementSketch(result.hole, result.placementFeature, result.placement);
				!status.ok())
			{
				DeleteFeatureAndSketches(doc, ext, result.hole);
				return status;
			}
			if (direct == 1 || SketchPoints(result.placement).size() == direct)
				break;
			// The wizard didn't put a hole at every selection: start over with
			// one, and add the rest to the placement sketch.
			DeleteFeatureAndSketches(doc, ext, result.hole);
			if (grpc::Status status = selectFirstOnly(); !status.ok())
				return status;
		}
		return grpc::Status::OK;
	}

	// Adds points[from..] to the hole's placement sketch: the hole feature puts
	// a hole at every point there. All or nothing: on failure the hole is
	// deleted.
	grpc::Status AddPlacementPoints(IModelDoc2* doc, IModelDocExtension* ext, ISketchManager* sketchManager,
		SketchSpace& space, const std::vector<Point3>& points, size_t from, HoleResult& result)
	{
		result.placement.Release(); // OpenExistingSketch fills it with the open sketch
		if (grpc::Status status = OpenExistingSketch(doc, result.placementFeature, result.placement); !status.ok())
		{
			DeleteFeatureAndSketches(doc, ext, result.hole);
			return status;
		}
		// The sketch is now open: no early returns until it's closed.
		space.Attach(result.placement);
		VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
		sketchManager->get_AddToDB(&previousAddToDB);
		sketchManager->put_AddToDB(VARIANT_TRUE);
		grpc::Status failure = grpc::Status::OK;
		for (size_t i = from; i < points.size() && failure.ok(); ++i)
		{
			double s[3];
			ATL::CComPtr<ISketchPoint> point;
			HRESULT hr = S_OK;
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
			(!FindHolePlacementSketch(result.hole, result.placementFeature, result.placement).ok() ||
				SketchPoints(result.placement).size() != points.size()))
			failure = grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks didn't keep every hole location in the placement sketch");
		if (!failure.ok())
			DeleteFeatureAndSketches(doc, ext, result.hole);
		return failure;
	}

	// Holes at locations on a planar face, in a placement sketch the Hole
	// Wizard makes.
	grpc::Status HolesOnFace(IModelDoc2* doc, IModelDocExtension* ext, const HoleWizardCall& wizard,
		SketchSpace& space, const HoleRequest& request, HoleResult& result)
	{
		ATL::CComPtr<IDispatch> faceObj;
		if (grpc::Status status = ResolvePersistId(ext, request.face_persist_id(), faceObj); !status.ok())
			return status;
		ATL::CComQIPtr<IFace2> face(faceObj.p);
		if (!face)
			return Invalid("that ID isn't a face -- the Hole Wizard needs a planar face");
		FacePlane plane;
		if (grpc::Status status = ReadFacePlane(face, plane); !status.ok())
			return status;
		// The same bytes a selected object's persist ID is compared to.
		const std::string facePid = PersistIdOf(ext, face);
		std::vector<Point3> points;
		if (grpc::Status status = HoleLocations(face, plane, request, points); !status.ok())
			return status;

		ATL::CComPtr<ISelectionMgr> selections;
		const HRESULT hr = doc->get_ISelectionManager(&selections);
		if (FAILED(hr))
			return ComError("IModelDoc2::ISelectionManager", hr);
		if (!selections)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no selection manager");
		ATL::CComPtr<ISketchManager> sketchManager;
		if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
			return status;

		const FaceRays rays(doc, ext, selections, plane);
		if (grpc::Status status = CheckLocationsOnFace(doc, ext, rays, facePid, points, request.locations().empty());
			!status.ok())
			return status;
		size_t direct = 0;
		if (grpc::Status status = RunWizardOnFace(doc, ext, wizard, rays, points, result, direct); !status.ok())
			return status;
		if (direct < points.size())
			return AddPlacementPoints(doc, ext, sketchManager, space, points, direct, result);
		return grpc::Status::OK;
	}

	// ---- The reply --------------------------------------------------------------

	// The feature, its placement sketch and frame, the hole centers (with ids
	// for dimensioning them, and in model space) and what the wizard built.
	grpc::Status DescribeHoles(IModelDocExtension* ext, SketchSpace& space, const HoleResult& result,
		sld_mcp::v1::HoleWizardResponse* response)
	{
		ATL::CComBSTR featureName;
		const HRESULT hr = result.hole->get_Name(&featureName);
		if (FAILED(hr))
			return ComError("IFeature::Name", hr);
		response->set_feature_name(ToUtf8(featureName));
		response->set_sketch_name(FeatureName(result.placementFeature));

		space.Attach(result.placement);
		const std::string sketchPid = PersistIdOf(ext, result.placementFeature);
		bool allMapped = true;
		for (const auto& point : SketchPoints(result.placement))
		{
			sld_mcp::v1::SketchItem item;
			if (!point || !DescribeSketchItem(point, sketchPid, &item) ||
				result.notCenters.count({item.ref().id0(), item.ref().id1()}))
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

		ReadHoleSpec(result.hole, response->mutable_spec());
		return grpc::Status::OK;
	}
}

void sld_mcp::service::DescribeHoleSpec(IFeature* hole, sld_mcp::v1::HoleSpec* spec)
{
	ReadHoleSpec(hole, spec);
}

grpc::Status SolidWorksService::HoleWizard(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::HoleWizardRequest* request,
	sld_mcp::v1::HoleWizardResponse* response)
{
	long genericType = 0;
	if (grpc::Status status = ValidateHoleRequest(*request, genericType); !status.ok())
		return status;

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
	const HRESULT hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	SketchSpace space;
	if (grpc::Status status = space.Init(swApp); !status.ok())
		return status;

	const HoleWizardCall wizard(featureManager, *request, genericType);
	HoleResult result;
	const grpc::Status status = request->sketch_name().empty()
		? HolesOnFace(doc, ext, wizard, space, *request, result)
		: HolesFromSketch(doc, ext, wizard, request->sketch_name(), result);
	if (!status.ok())
		return status;
	return DescribeHoles(ext, space, result, response);
}
