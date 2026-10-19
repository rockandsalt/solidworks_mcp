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

	// Every component of the assembly, nested ones included.
	std::vector<ATL::CComPtr<IComponent2>> AllComponents(IAssemblyDoc* assembly);

	bool IsSubassembly(IComponent2* component);

	bool IsSuppressed(IComponent2* component);

	// The components that hold geometry: parts (subassemblies' bodies are
	// their children's), not suppressed. `visibleOnly` also drops hidden ones.
	std::vector<ATL::CComPtr<IComponent2>> GeometryComponents(IAssemblyDoc* assembly, bool visibleOnly);

	// A component's solid and surface bodies.
	std::vector<ATL::CComPtr<IBody2>> ComponentBodies(IComponent2* component);

	// Fills the placement fields of `info` (origin + axes in assembly space)
	// and decides `local_geometry`: whether the bodies' own boxes are in
	// component coordinates. That's what the SolidWorks docs imply, but it's
	// checked here rather than assumed -- the bodies' box, once placed,
	// should land on the component's assembly-space box.
	void DescribePlacement(IComponent2* component, IMathUtility* math, sld_mcp::v1::ComponentInfo* info);

	sld_mcp::v1::ComponentState ToComponentState(long state);

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
}
