// AssemblyHelpers.h : Assemblies: components, their placements, and mates.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"

namespace sld_mcp::service
{
	// The component an assembly entity (face, edge, vertex, ...) belongs to;
	// null for the assembly's own entities or anything that isn't an IEntity.
	ATL::CComPtr<IComponent2> ComponentOf(IUnknown* obj);

	std::string ComponentName(IComponent2* component);

	// The assembly's own component behind one a drawing view of it hands out
	// (IView::GetVisibleComponents, an entity's GetComponent). Those are
	// drawing-side stand-ins without a transform or persist id, named after
	// the view's root plus the component's path ("pair-1/block-2"), so the
	// leading names are dropped until the assembly knows the rest.
	ATL::CComPtr<IComponent2> AssemblyComponentOf(IAssemblyDoc* assembly, IComponent2* viewComponent);

	// Every component of the assembly, nested ones included. With control,
	// cancellation unwinds to the executor instead of returning a partial list.
	std::vector<ATL::CComPtr<IComponent2>> AllComponents(IAssemblyDoc* assembly, RequestControl* control = nullptr);

	bool IsSubassembly(IComponent2* component);

	bool IsSuppressed(IComponent2* component);

	// The components that hold geometry: parts (subassemblies' bodies are
	// their children's), not suppressed. `visibleOnly` also drops hidden ones.
	// With control, cancellation unwinds like AllComponents.
	std::vector<ATL::CComPtr<IComponent2>> GeometryComponents(IAssemblyDoc* assembly, bool visibleOnly,
		RequestControl* control = nullptr);

	// A component's solid and surface bodies. With control, cancellation unwinds.
	std::vector<ATL::CComPtr<IBody2>> ComponentBodies(IComponent2* component, RequestControl* control = nullptr);

	// A component transform (IComponent2::Transform2) as component ->
	// assembly, read through where it puts the origin and unit axes.
	bool PlacementOf(IMathUtility* math, IMathTransform* xform, Eigen::Affine3d& out);

	// Sets origin and axes of `info` from a component -> assembly placement.
	void SetPlacement(const Eigen::Affine3d& placement, sld_mcp::v1::ComponentInfo* info);

	// Fills the placement fields of `info` (origin + axes in assembly space)
	// and decides `local_geometry`: whether the bodies' own boxes are in
	// component coordinates. That's what the SolidWorks docs imply, but it's
	// checked here rather than assumed -- the bodies' box, once placed,
	// should land on the component's assembly-space box.
	void DescribePlacement(IComponent2* component, IMathUtility* math, sld_mcp::v1::ComponentInfo* info);

	sld_mcp::v1::ComponentState ToComponentState(long state);

	// The subassembly a component is in; null for a top-level one.
	ATL::CComPtr<IComponent2> ParentOf(IComponent2* component);

	// A component's transform and its raw numbers, to tell whether it moved
	// and to put it back.
	struct SavedPose
	{
		ATL::CComPtr<IComponent2> component;
		ATL::CComPtr<IMathTransform> xform;
		std::vector<double> data;
	};

	bool ReadPose(IComponent2* component, SavedPose& out);

	// Same place to within `tolerance` (meters; the axes' numbers are unit).
	bool SamePose(const std::vector<double>& a, const std::vector<double>& b, double tolerance);

	// Every top-level component where it is now.
	std::vector<SavedPose> TopLevelPoses(IAssemblyDoc* assembly);

	// The ones of `saved` whose transform changed by more than 1e-9 since.
	std::vector<const SavedPose*> MovedSince(const std::vector<SavedPose>& saved);

	// Puts every component of `saved` that moved back (put_Transform2, then
	// a rebuild) and returns the names of the ones that still aren't back.
	std::vector<std::string> RestorePoses(IModelDoc2* doc, const std::vector<SavedPose>& saved);

	void DescribeComponent(IComponent2* component, PersistIds& ids, IMathUtility* math,
		sld_mcp::v1::ComponentInfo* info);

	// The components listed in one reply, each described once.
	class ComponentList
	{
	public:
		// With `viewAssembly`, the components given are a drawing view's
		// stand-ins for that assembly's (see AssemblyComponentOf).
		ComponentList(PersistIds& ids, IMathUtility* math,
			google::protobuf::RepeatedPtrField<sld_mcp::v1::ComponentInfo>* out, IAssemblyDoc* viewAssembly = nullptr)
			: m_ids(ids), m_math(math), m_out(out), m_viewAssembly(viewAssembly) {}

		// The component's persist id; describes it the first time. Empty for null.
		std::string Add(IComponent2* component)
		{
			if (!component)
				return {};
			ATL::CComPtr<IComponent2> own(component);
			if (m_viewAssembly && !(own = AssemblyComponentOf(m_viewAssembly, component)))
				return {};
			std::string pid = m_ids.Get(own);
			if (!pid.empty() && m_listed.insert(pid).second)
				DescribeComponent(own, m_ids, m_math, m_out->Add());
			return pid;
		}

	private:
		PersistIds& m_ids;
		IMathUtility* m_math;
		google::protobuf::RepeatedPtrField<sld_mcp::v1::ComponentInfo>* m_out;
		ATL::CComPtr<IAssemblyDoc> m_viewAssembly;
		std::set<std::string> m_listed;
	};

	sld_mcp::v1::MateType ToMateType(long type);

	bool ToSwMateType(sld_mcp::v1::MateType type, long& out);

	const char* MateTypeName(long type);

	sld_mcp::v1::MateAlignment ToMateAlignment(long align);

	long ToSwMateAlign(sld_mcp::v1::MateAlignment align);

	// Mates live under the assembly's "MateGroup" feature, possibly inside
	// folders; anything whose specific feature is an IMate2 is one.
	constexpr size_t kMaxMates = 5000;

	void CollectMates(IFeature* parent, int depth, std::vector<ATL::CComPtr<IFeature>>& mates,
		std::set<std::string>& seen);

	std::vector<ATL::CComPtr<IFeature>> MateFeatures(IModelDoc2* doc);

	std::set<std::string> MateNames(IModelDoc2* doc);

	// A distance / angle mate's limits, from its definition (late-bound: the
	// *MateFeatureData's IsAdvancedMate and Minimum/MaximumDistance or
	// Angle; IMate2's Minimum/MaximumVariation are the same relative to the
	// value, seen 2026-10-25). False for a mate without limits.
	bool MateLimitsOf(IFeature* feature, long type, double& minimum, double& maximum);

	// A distance / angle mate's value in words: "12.5 mm", "30 deg".
	std::string MateValueText(long type, double value);

	// INVALID_ARGUMENT unless `type` is a distance / angle mate, min..max a
	// range it can have, and `value` in it.
	grpc::Status CheckMateLimits(long type, double minimum, double maximum, double value);

	void DescribeMate(IFeature* feature, PersistIds& ids, sld_mcp::v1::MateInfo* info);

	std::string MateErrorText(long code);

	std::string AddMateErrorText(long error, long type);

	// Deletes a feature (e.g. a mate that failed). Best effort.
	void DeleteFeature(IModelDoc2* doc, IFeature* feature);

	// One side of a mate, resolved.
	struct MateRef
	{
		ATL::CComPtr<IDispatch> obj;
		ATL::CComPtr<IComponent2> component;  // null: the assembly's own (or unknown)
		bool isComponent = false;
		bool isFeature = false;
		std::string what;  // for messages: "face", "Front Plane", "bracket-1", ...
	};

	// The component a reference plane/axis feature belongs to. Features may
	// not answer IEntity::GetComponent, so failing that, look for a feature
	// of the same name in each component and compare persist ids. Null for
	// the assembly's own features.
	ATL::CComPtr<IComponent2> ComponentOfFeature(IAssemblyDoc* assembly, IFeature* feature, PersistIds& ids);

	grpc::Status ResolveMateRef(IModelDocExtension* ext, IAssemblyDoc* assembly, PersistIds& ids,
		const std::string& persistId, long mateType, MateRef& ref);

	// Appends a mate ref to the selection with mark 1, as AddMate5 expects.
	bool SelectMateRef(const MateRef& ref, ISelectData* data);

	// What a mate entity is attached to, by interface.
	std::string MateEntityKind(IDispatch* reference);

	// Each entity of a mate and whether it's still there. True if one is
	// missing.
	bool DiagnoseMate(IFeature* feature, PersistIds& ids,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::MateEntityState>* out);

	// A mate as AddMate / EditMate make it: AddMate5's arguments plus the
	// entities.
	struct MateSettings
	{
		long type = swMateCOINCIDENT;
		long alignment = swMateAlignCLOSEST;
		bool flip = false;
		double distance = 0.0;
		double angle = 0.0;
		bool lockRotation = false;
		// DISTANCE / ANGLE: a limit mate, the value between these (meters /
		// radians).
		bool limited = false;
		double minimum = 0.0;
		double maximum = 0.0;
		std::vector<MateRef> refs;
		// EditMate: `refs` aren't the mate's current entities.
		bool newRefs = false;
	};

	// Adds a mate between `s.refs` (selected with mark 1, then AddMate5) and
	// solves it. A mate SolidWorks rejects, or that fails to solve, is
	// deleted again (FAILED_PRECONDITION saying why). `created` = the new
	// mate: the one whose name wasn't in the Mates folder before, since
	// AddMate5 can leave a broken mate behind even when it reports an error.
	grpc::Status CreateMate(IModelDoc2* doc, IAssemblyDoc* assembly, const MateSettings& s,
		ATL::CComPtr<IFeature>& created);

	// While it lives (many solves or rebuilds in a row): no redrawing the active view or
	// the FeatureManager tree, and SolidWorks told an API command is running.
	class QuietSolves
	{
	public:
		QuietSolves(ISldWorks* swApp, IModelDoc2* doc) : m_swApp(swApp)
		{
			ATL::CComPtr<IDispatch> disp;
			if (SUCCEEDED(doc->get_ActiveView(&disp)) && disp)
				m_view = disp;
			if (m_view)
				m_view->put_EnableGraphicsUpdate(VARIANT_FALSE);
			doc->get_FeatureManager(&m_features);
			if (m_features)
				m_features->put_EnableFeatureTree(VARIANT_FALSE);
			m_swApp->put_CommandInProgress(VARIANT_TRUE);
		}
		~QuietSolves()
		{
			m_swApp->put_CommandInProgress(VARIANT_FALSE);
			if (m_features)
				m_features->put_EnableFeatureTree(VARIANT_TRUE);
			if (m_view)
				m_view->put_EnableGraphicsUpdate(VARIANT_TRUE);
		}
		QuietSolves(const QuietSolves&) = delete;
		QuietSolves& operator=(const QuietSolves&) = delete;

	private:
		ATL::CComPtr<ISldWorks> m_swApp;
		ATL::CComQIPtr<IModelView> m_view;
		ATL::CComPtr<IFeatureManager> m_features;
	};

	// Interference Detection on an assembly while this lives: the options
	// that pick what is checked set as asked, then put back as they were
	// when it goes out of scope (the manager keeps them between checks),
	// then Done -- which leaves interference mode (interferences shown red,
	// other components wireframe) and marks the assembly modified (seen
	// 2026-10-24). Only those options: changing ShowIgnoredInterferences,
	// MakeInterferingPartsTransparent or NonInterferingComponentDisplay made
	// SolidWorks drop the coincident (touching) interferences, and the
	// display goes back with Done anyway. One session can check again after
	// components move (SetTransformAndSolve2): GetInterferences recomputes
	// (seen 2026-10-25).
	class InterferenceSession
	{
	public:
		explicit InterferenceSession(IInterferenceDetectionMgr* mgr);
		~InterferenceSession();
		InterferenceSession(const InterferenceSession&) = delete;
		InterferenceSession& operator=(const InterferenceSession&) = delete;

		void Apply(bool coincident, bool subassemblies, bool multibody, bool includeHidden);

		// The interferences as the components are now, at most `limit` of
		// them (`truncated` past that). With `components`, each component is
		// described there once (its persist id comes from there).
		grpc::Status Check(PersistIds& ids, ComponentList* components, int limit,
			google::protobuf::RepeatedPtrField<sld_mcp::v1::Interference>* out, bool& truncated);

	private:
		ATL::CComPtr<IInterferenceDetectionMgr> m_mgr;
		VARIANT_BOOL m_coincidence = VARIANT_FALSE, m_subassemblies = VARIANT_FALSE, m_multibody = VARIANT_FALSE,
			m_ignoreHidden = VARIANT_FALSE;
	};

	// A session with the Interference Detection dialog's defaults (solids
	// that overlap, hidden ones ignored), for checks along a motion.
	grpc::Status StartCollisionChecks(IAssemblyDoc* assembly, std::unique_ptr<InterferenceSession>& out);

	// Which components an interference is between (their persist ids,
	// sorted), to tell the same pair again at another step.
	std::string InterferenceKey(const sld_mcp::v1::Interference& interference);

	// Whether `now` has an interference whose key isn't in `before`.
	bool HasNewInterference(const std::set<std::string>& before,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::Interference>& now);
}
