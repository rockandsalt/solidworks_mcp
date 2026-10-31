// SketchHelpers.h : Sketches: finding and opening them, the sketch frame (SketchSpace),
// sketch-entity ids and descriptions, sketch state and relations.
//
// Sketch segments and points are identified by SolidWorks' sketch entity
// IDs (two numbers, stable for as long as the entity exists) plus the
// persist ID of the sketch feature they're in.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	// Finds the `index`-th reference plane (0 = Front, 1 = Top, 2 = Right) by
	// walking the feature tree. Every part template starts with those three
	// as its first RefPlane features, so this works even when the planes are
	// renamed or SolidWorks runs in another language (SelectByID2 with
	// "Front Plane" would not).
	grpc::Status FindStandardPlane(IModelDoc2* doc, int index, ATL::CComPtr<IFeature>& plane);

	// PLANE_FRONT/TOP/RIGHT as FindStandardPlane's index.
	bool StandardPlaneIndex(sld_mcp::v1::Plane plane, int& index);

	// The part origin as a sketch point (the Origin feature's sketch has it as
	// its first point), plus that feature's name for SelectByID2.
	void FindOrigin(IModelDoc2* doc, ATL::CComPtr<ISketchPoint>& point, std::string& featureName);

	// A 2D sketch by its feature-tree name.
	grpc::Status FindSketchByName(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& sketch);

	// Converts points between model space and an open sketch's own space
	// (x, y in the sketch plane; z = distance from it) with IMathPoints.
	class SketchSpace
	{
	public:
		// Call before opening the sketch, so a failure here needs no cleanup.
		grpc::Status Init(ISldWorks* swApp)
		{
			return GetMathUtility(swApp, m_math);
		}

		// Once the sketch is open. If SolidWorks has no transform for it,
		// the conversions below just fail.
		void Attach(ISketch* sketch)
		{
			m_toSketch.Release();
			m_toModel.Release();
			if (SUCCEEDED(sketch->get_ModelToSketchTransform(&m_toSketch)) && m_toSketch)
			{
				ATL::CComPtr<IDispatch> inverse;
				if (SUCCEEDED(m_toSketch->Inverse(&inverse)) && inverse)
					inverse->QueryInterface(IID_PPV_ARGS(&m_toModel));
			}
		}

		bool ToSketch(const double in[3], double out[3]) const { return Apply(m_toSketch, in, out); }
		bool ToModel(const double in[3], double out[3]) const { return Apply(m_toModel, in, out); }

		// The sketch's origin and unit axes in model space.
		bool Frame(sld_mcp::v1::SketchFrame& frame) const
		{
			const double sketchPoints[4][3] = {{0, 0, 0}, {1, 0, 0}, {0, 1, 0}, {0, 0, 1}};
			double modelPoints[4][3];
			for (int i = 0; i < 4; ++i)
				if (!ToModel(sketchPoints[i], modelPoints[i]))
					return false;

			SetVec3(frame.mutable_origin(), modelPoints[0]);
			sld_mcp::v1::Vec3* axes[3] = {frame.mutable_x_axis(), frame.mutable_y_axis(), frame.mutable_normal()};
			for (int a = 0; a < 3; ++a)
			{
				double d[3];
				for (int i = 0; i < 3; ++i)
					d[i] = modelPoints[a + 1][i] - modelPoints[0][i];
				const double len = std::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
				if (len > 0.0)
					for (double& v : d)
						v /= len;
				SetVec3(axes[a], d);
			}
			return true;
		}

	private:
		bool Apply(IMathTransform* xform, const double in[3], double out[3]) const
		{
			Eigen::Vector3d moved;
			if (!TransformPoint(m_math, xform, Eigen::Vector3d(in[0], in[1], in[2]), moved))
				return false;
			Eigen::Map<Eigen::Vector3d> result(out);
			result = moved;
			return true;
		}

		ATL::CComQIPtr<IMathUtility> m_math;
		ATL::CComPtr<IMathTransform> m_toSketch;
		ATL::CComPtr<IMathTransform> m_toModel;
	};

	// Counts the sketch's closed and open contours.
	bool CountContours(ISketch* sketch, sld_mcp::v1::SketchContours& contours);

	// InspectSketch lists at most this many segments + points.
	constexpr size_t kMaxSketchItems = 5000;

	// A SketchState lists at most this many entities that aren't fully defined.
	constexpr int kMaxListedUndefined = 50;

	// The last 2D sketch in the feature tree, used by a feature or not --
	// "the sketch I just drew".
	grpc::Status FindLatestSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch);

	// The last 2D sketch in the feature tree that no feature uses yet, i.e.
	// what a user means by "extrude this sketch". Sketches a feature has
	// absorbed have that feature as a child, so they're skipped, and so is
	// `except` (e.g. a sweep's path).
	grpc::Status FindLatestUnusedSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch,
		IFeature* except = nullptr);

	// A sketch by name, or the latest one when `name` is empty.
	grpc::Status ResolveSketchFeature(IModelDoc2* doc, const std::string& name, ATL::CComPtr<IFeature>& sketch);

	// The ISketch behind a sketch feature.
	grpc::Status SketchOf(IFeature* feature, ATL::CComPtr<ISketch>& sketch);

	// Re-opens an existing, closed sketch for editing. On failure nothing is
	// left open.
	grpc::Status OpenExistingSketch(IModelDoc2* doc, IFeature* sketchFeature, ATL::CComPtr<ISketch>& sketch);

	grpc::Status GetSketchManager(IModelDoc2* doc, ATL::CComPtr<ISketchManager>& sketchManager);

	// What a new sketch goes on: a planar face (`face` + `faceEntity`) or a
	// reference plane feature (`plane`); exactly one is set once resolved.
	struct SketchTarget
	{
		ATL::CComPtr<IFace2> face;
		ATL::CComPtr<IEntity> faceEntity;
		ATL::CComPtr<IFeature> plane;
	};

	// Resolves `onPersistId` (a planar face or a reference plane feature), or
	// the standard plane `planeIndex` when it's empty.
	grpc::Status ResolveSketchTarget(IModelDoc2* doc, const std::string& onPersistId, int planeIndex,
		SketchTarget& target);

	// Selects the target and opens a new sketch on it. On failure nothing is
	// left open.
	grpc::Status OpenNewSketch(IModelDoc2* doc, ISketchManager* sketchManager, const SketchTarget& target,
		ATL::CComPtr<ISketch>& sketch);

	// Deletes a (closed) sketch feature, e.g. one left empty by a failed
	// RPC. Best effort.
	void DeleteSketchFeature(IModelDoc2* doc, IFeature* sketchFeature);

	// Deletes segments or points of the open sketch. Best effort.
	void DeleteDrawnEntities(IModelDoc2* doc, const std::vector<ATL::CComPtr<IDispatch>>& entities);

	// The persist ID of a feature (empty if SolidWorks won't give one).
	std::string PersistIdOf(IModelDocExtension* ext, IUnknown* obj);

	sld_mcp::v1::DefinitionStatus ToDefinitionStatus(long status);

	sld_mcp::v1::DefinitionStatus SketchStatus(ISketch* sketch);

	// The two numbers SolidWorks identifies a sketch segment or point by.
	bool SketchEntityId(IDispatch* entity, bool& isPoint, long long& id0, long long& id1);

	bool SetItemRef(IDispatch* entity, const std::string& sketchPid, sld_mcp::v1::SketchItemRef* ref);

	// Constrained status of a sketch segment or point (swConstrainedStatus_e).
	long ItemStatus(IDispatch* entity);

	// Sketch points report sketch coordinates.
	bool PointPosition(IDispatch* pointDisp, double out[3]);

	// Fills a point's position and ref, e.g. a line's start. Leaves both
	// unset if SolidWorks won't describe the point.
	void SetItemPoint(IDispatch* pointDisp, const std::string& sketchPid, sld_mcp::v1::Vec3* position,
		sld_mcp::v1::SketchItemRef* ref);

	// Describes a sketch segment or point. False if it isn't one.
	bool DescribeSketchItem(IDispatch* entity, const std::string& sketchPid, sld_mcp::v1::SketchItem* item);

	// A sketch's segments and points, to look up by ID.
	class SketchItems
	{
	public:
		struct Entry
		{
			ATL::CComPtr<IDispatch> obj;
			bool point;
			long long id0;
			long long id1;
			long type;  // segments: swSketchSegments_e
		};

		void Load(ISketch* sketch)
		{
			m_entries.clear();
			ATL::CComVariant segments, points;
			if (SUCCEEDED(sketch->GetSketchSegments(&segments)))
				Add(segments);
			if (SUCCEEDED(sketch->GetSketchPoints2(&points)))
				Add(points);
		}

		// The entity `ref` names, or null if the sketch has no such entity.
		ATL::CComPtr<IDispatch> Find(const sld_mcp::v1::SketchItemRef& ref) const
		{
			for (const Entry& e : m_entries)
				if (e.point == ref.point() && e.id0 == ref.id0() && e.id1 == ref.id1() &&
					(e.point || e.type == ref.segment_type()))
					return e.obj;
			return nullptr;
		}

		const std::vector<Entry>& All() const { return m_entries; }

		// Whether `entity` is one of these segments or points. A relation to
		// model geometry lists a hidden stand-in segment instead of the edge,
		// whose sketch is this one and whose ID can equal a real segment's
		// (seen in SOLIDWORKS 2026: a converted edge's model edge came back
		// as line [0,1] next to the sketch's circle [0,1]), so segments must
		// match in type too.
		bool Contains(IDispatch* entity) const
		{
			bool point = false;
			long long id0 = 0, id1 = 0;
			if (!SketchEntityId(entity, point, id0, id1))
				return false;
			for (const Entry& e : m_entries)
				if (e.point == point && e.id0 == id0 && e.id1 == id1)
					return point || e.type == SegmentType(entity);
			return false;
		}

	private:
		static long SegmentType(IDispatch* entity)
		{
			long type = -1;
			if (ATL::CComQIPtr<ISketchSegment> segment(entity); segment)
				segment->GetType(&type);
			return type;
		}

		void Add(const VARIANT& array)
		{
			for (const auto& disp : comvariant::ToDispatches(array))
			{
				Entry e{disp, false, 0, 0, -1};
				if (SketchEntityId(disp, e.point, e.id0, e.id1))
				{
					if (!e.point)
						e.type = SegmentType(disp);
					m_entries.push_back(e);
				}
			}
		}

		std::vector<Entry> m_entries;
	};

	// The sketch's overall status and the entities that aren't fully defined.
	void ReadSketchState(ISketch* sketch, const std::string& sketchPid, sld_mcp::v1::SketchState* state);

	// Checks that every sketch item ref in `refs` is in the target sketch
	// (`sketchPid` / `sketchName`). Call before opening the sketch.
	grpc::Status CheckRefsInSketch(IModelDocExtension* ext,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef>& refs,
		const std::string& sketchPid, const std::string& sketchName, const std::string& where);

	// The sketch's relations (dimensions included), or those matching a
	// swSketchRelationFilterType_e filter.
	std::vector<ATL::CComPtr<ISketchRelation>> SketchRelations(ISketch* sketch, long filter = swAll);

	// A sketch item's identity within its sketch: segment type (-1 for
	// points: segments are numbered per type) and ID.
	using SketchItemKey = std::tuple<long, long long, long long>;

	SketchItemKey SketchKeyOf(const SketchItems::Entry& e);
	SketchItemKey SketchKeyOf(const sld_mcp::v1::SketchItemRef& ref);
	std::set<SketchItemKey> SketchKeys(const SketchItems& items);

	// The sketch an entity-editing request works in: `sketchName`, else the
	// sketch its first item is in.
	grpc::Status ResolveItemsSketch(IModelDoc2* doc, IModelDocExtension* ext, const std::string& sketchName,
		const sld_mcp::v1::SketchItemRef& first, ATL::CComPtr<IFeature>& sketchFeature);

	// Adds the items of the open sketch to the selection, in order. False
	// (selection cleared, `failed` = the item's index) if SolidWorks won't
	// select one.
	bool SelectSketchItems(IModelDoc2* doc, const SketchItems& open,
		const std::vector<sld_mcp::v1::SketchItemRef>& refs, size_t& failed);

	// The `added` items of a (closed) sketch, described into `out`: the
	// segments, then, with `withPoints`, the points that aren't an end or
	// center point of any segment.
	void DescribeAddedItems(const SketchItems& items, const std::set<SketchItemKey>& added,
		const std::string& sketchPid, bool withPoints,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchItem>* out);
}
