// Deleting.cpp : DeleteFeatures, DeleteSketchEntities (see docs/notes/deleting.md).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "SketchHelpers.h"
#include "FeatureTreeHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// Most features one DeleteFeatures call takes.
	constexpr int kMaxDeleteFeatures = 500;

	// Most sketch entities one DeleteSketchEntities call takes.
	constexpr int kMaxDeleteSketchItems = 1000;

	// Most dependent features a refusal names.
	constexpr size_t kMaxListedDependents = 20;

	// Bound on the dependency walk, in features visited.
	constexpr size_t kMaxDependencyWalk = 20000;

	std::set<std::string> TreeNames(const std::vector<TreeFeature>& tree)
	{
		std::set<std::string> names;
		for (const auto& f : tree)
			names.insert(f.name);
		return names;
	}

	using PointKey = std::pair<long long, long long>;

	PointKey KeyOf(const sld_mcp::v1::SketchItemRef& ref)
	{
		return {ref.id0(), ref.id1()};
	}

	// Sketch relations that aren't dimensions.
	size_t CountRelations(ISketch* sketch)
	{
		size_t count = 0;
		for (const auto& relation : SketchRelations(sketch))
		{
			long type = 0;
			if (SUCCEEDED(relation->GetRelationType(&type)) &&
				(type == swConstraintType_DISTANCE || type == swConstraintType_ANGLE ||
				 type == swConstraintType_RADIUS || type == swConstraintType_DIAMETER))
				continue;
			++count;
		}
		return count;
	}
}

grpc::Status SolidWorksService::DeleteFeatures(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::DeleteFeaturesRequest* request,
	sld_mcp::v1::DeleteFeaturesResponse* response)
{
	const int count = request->features_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one feature to delete");
	if (count > kMaxDeleteFeatures)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many features in one call (max 500)");
	const sld_mcp::v1::ChildFeatures children = request->children();

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
	// Assemblies: not while a component is being edited in context, where
	// the selection would be the component's.
	doc.Release();
	if (grpc::Status status = docType == swDocASSEMBLY ? GetEditableAssembly(swApp, doc) : GetEditablePart(swApp, doc);
		!status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	PersistIds ids(ext);

	const std::vector<TreeFeature> tree = FeatureTree(doc);

	// The requested features, in request order, without repeats.
	std::vector<size_t> targets;
	std::set<std::string> targetNames;
	for (int i = 0; i < count; ++i)
	{
		size_t index = 0;
		if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, request->features(i), index); !status.ok())
			return grpc::Status(status.error_code(),
				"feature " + std::to_string(i + 1) + ": " + status.error_message());
		const TreeFeature& f = tree[index];
		if (f.fixed)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + f.name + "\" can't be deleted: it " + f.fixed);
		if (targetNames.insert(f.name).second)
			targets.push_back(index);
	}

	// What each target absorbed (an extrude's sketch), and with
	// delete_absorbed those go too.
	std::vector<std::pair<std::string, std::string>> absorbed;  // name, type
	std::set<std::string> roots = targetNames;
	for (size_t index : targets)
		for (const auto& sub : SubFeatures(tree[index].feature))
		{
			const std::string name = FeatureName(sub);
			if (targetNames.count(name))
				continue;
			absorbed.emplace_back(name, FeatureTypeName(sub));
			if (request->delete_absorbed())
				roots.insert(name);
		}
	std::set<std::string> absorbedNames;
	for (const auto& a : absorbed)
		absorbedNames.insert(a.first);

	// Everything that depends on what's being deleted, directly or not.
	std::vector<std::pair<std::string, std::string>> dependents;  // name, type
	{
		std::set<std::string> seen = roots;
		std::vector<ATL::CComPtr<IFeature>> queue;
		for (size_t index : targets)
			queue.push_back(tree[index].feature);
		if (request->delete_absorbed())
			for (size_t index : targets)
				for (const auto& sub : SubFeatures(tree[index].feature))
					queue.push_back(sub);
		size_t visited = 0;
		while (!queue.empty() && visited++ < kMaxDependencyWalk)
		{
			ATL::CComPtr<IFeature> feature = queue.back();
			queue.pop_back();
			for (const auto& child : ChildFeatures(feature))
			{
				const std::string name = FeatureName(child);
				// A target's own absorbed features aren't its dependents.
				if (absorbedNames.count(name) || !seen.insert(name).second)
					continue;
				dependents.emplace_back(name, FeatureTypeName(child));
				queue.push_back(child);
			}
		}
	}

	if (!dependents.empty() && (children == sld_mcp::v1::CHILD_FEATURES_UNSPECIFIED ||
		children == sld_mcp::v1::CHILD_FEATURES_REFUSE))
	{
		std::string list;
		for (size_t i = 0; i < dependents.size() && i < kMaxListedDependents; ++i)
			list += (i ? ", " : "") + dependents[i].first + " (" + dependents[i].second + ")";
		if (dependents.size() > kMaxListedDependents)
			list += " and " + std::to_string(dependents.size() - kMaxListedDependents) + " more";
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"nothing was deleted: other features depend on " +
			std::string(targets.size() == 1 ? "it" : "them") + ": " + list +
			" -- pass children=\"delete\" to delete those too, or children=\"keep\" to keep them (they "
			"lose their references and may fail to rebuild)");
	}

	const auto errorsBefore = FeatureErrors(tree);

	// No "What's Wrong" dialog for the features this breaks.
	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	for (size_t index : targets)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		hr = tree[index].feature->Select2(VARIANT_TRUE, 0, &selected);
		if (FAILED(hr) || !selected)
		{
			doc->ClearSelection2(VARIANT_TRUE);
			if (FAILED(hr))
				return ComError("IFeature::Select2", hr);
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks wouldn't select \"" + tree[index].name + "\"");
		}
	}

	long options = 0;
	if (children == sld_mcp::v1::CHILD_FEATURES_DELETE)
		options |= swDelete_Children;
	if (request->delete_absorbed())
		options |= swDelete_Absorbed;
	VARIANT_BOOL deletedOk = VARIANT_FALSE;
	hr = ext->DeleteSelection2(options, &deletedOk);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDocExtension::DeleteSelection2", hr);

	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);

	const std::vector<TreeFeature> after = FeatureTree(doc);
	const std::set<std::string> afterNames = TreeNames(after);

	std::set<std::string> reported;
	for (const auto& f : tree)
		if (!afterNames.count(f.name) && reported.insert(f.name).second)
			SetFeatureRef(response->add_deleted(), f.name, f.type);
	for (size_t index : targets)
		if (afterNames.count(tree[index].name))
			SetFeatureRef(response->add_not_deleted(), tree[index].name, tree[index].type);
	for (const auto& [name, type] : absorbed)
		if (afterNames.count(name))
			SetFeatureRef(response->add_kept_absorbed(), name, type);
	for (const auto& [name, type] : dependents)
		if (afterNames.count(name))
			SetFeatureRef(response->add_kept_children(), name, type);
	AddNewErrors(errorsBefore, FeatureErrors(after), response->mutable_new_errors());

	if (response->not_deleted_size() == static_cast<int>(targets.size()))
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			targets.size() == 1 ? "SolidWorks didn't delete \"" + tree[targets[0]].name + "\""
			                    : std::string("SolidWorks didn't delete any of those features"));
	doc->GraphicsRedraw2();
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::DeleteSketchEntities(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::DeleteSketchEntitiesRequest* request,
	sld_mcp::v1::DeleteSketchEntitiesResponse* response)
{
	const int count = request->items_size();
	if (count == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one sketch entity to delete");
	if (count > kMaxDeleteSketchItems)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "too many sketch entities in one call (max 1000)");
	auto where = [](int i) { return "entity " + std::to_string(i + 1); };

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

	// The sketch: by name, or the one the first entity is in.
	ATL::CComPtr<IFeature> sketchFeature;
	if (!request->sketch_name().empty())
	{
		if (grpc::Status status = FindSketchByName(doc, request->sketch_name(), sketchFeature); !status.ok())
			return status;
	}
	else
	{
		ATL::CComPtr<IDispatch> obj;
		if (grpc::Status status = ResolvePersistId(ext, request->items(0).sketch_persist_id(), obj); !status.ok())
			return grpc::Status(status.error_code(), "the entities' sketch " + status.error_message());
		sketchFeature = ATL::CComQIPtr<IFeature>(obj.p);
		if (!sketchFeature)
			return grpc::Status(grpc::StatusCode::INTERNAL, "a sketch reference didn't resolve to a feature");
	}
	const std::string sketchName = FeatureName(sketchFeature);
	const std::string sketchPid = PersistIdOf(ext, sketchFeature);
	response->set_sketch_name(sketchName);

	// Every entity must be in that sketch.
	{
		google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> refs;
		for (const auto& item : request->items())
			*refs.Add()->mutable_item() = item;
		for (int i = 0; i < count; ++i)
		{
			google::protobuf::RepeatedPtrField<sld_mcp::v1::SketchRef> one;
			*one.Add() = refs[i];
			if (grpc::Status status = CheckRefsInSketch(ext, one, sketchPid, sketchName, where(i)); !status.ok())
				return status;
		}
	}

	// Check everything against the closed sketch before touching it.
	ATL::CComPtr<ISketch> sketch;
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	SketchItems items;
	items.Load(sketch);

	// Which segments use each point as an end or center point.
	std::map<PointKey, std::vector<std::pair<PointKey, std::string>>> pointUsers;  // point -> (segment, name)
	size_t pointsBefore = 0;
	for (const auto& e : items.All())
	{
		if (e.point)
		{
			++pointsBefore;
			continue;
		}
		sld_mcp::v1::SketchItem item;
		if (!DescribeSketchItem(e.obj, sketchPid, &item))
			continue;
		const PointKey segment{e.id0, e.id1};
		const std::string name = item.name().empty() ? "a segment" : item.name();
		for (const sld_mcp::v1::SketchItemRef* p : {&item.start_point(), &item.end_point(), &item.center_point()})
			if (p->point())
			{
				auto& users = pointUsers[KeyOf(*p)];
				if (std::find_if(users.begin(), users.end(), [&](const auto& u) { return u.first == segment; }) ==
					users.end())
					users.emplace_back(segment, name);
			}
	}

	std::set<PointKey> requestedSegments;
	for (const auto& item : request->items())
		if (!item.point())
			requestedSegments.insert(KeyOf(item));

	// What gets selected: the segments and standalone points, once each.
	// A point that only belongs to segments being deleted goes with them.
	std::vector<sld_mcp::v1::SketchItemRef> toSelect;
	std::set<std::pair<bool, PointKey>> seen;
	int standalonePoints = 0;
	for (int i = 0; i < count; ++i)
	{
		const auto& ref = request->items(i);
		if (!items.Find(ref))
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				where(i) + " isn't in sketch \"" + sketchName + "\" any more (already deleted or merged?) -- "
				"inspect the sketch again for current IDs");
		if (!seen.insert({ref.point(), KeyOf(ref)}).second)
			continue;
		if (ref.point())
		{
			auto users = pointUsers.find(KeyOf(ref));
			if (users != pointUsers.end())
			{
				std::string others;
				for (const auto& [segment, name] : users->second)
					if (!requestedSegments.count(segment))
						others += (others.empty() ? "" : ", ") + name;
				if (!others.empty())
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
						where(i) + " is an end or center point of " + others + ", which this call doesn't "
						"delete -- a segment's points can only be deleted with the segment (delete the "
						"segment instead; its end points go with it)");
				continue;  // goes with its segment(s)
			}
			++standalonePoints;
		}
		toSelect.push_back(ref);
	}

	const std::set<std::string> dimsBefore = DimensionNames(sketchFeature);
	const size_t relationsBefore = CountRelations(sketch);
	const auto errorsBefore = FeatureErrors(FeatureTree(doc));

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	ATL::CComPtr<ISketchManager> sketchManager;
	if (grpc::Status status = GetSketchManager(doc, sketchManager); !status.ok())
		return status;
	ATL::CComPtr<ISketch> openSketch;
	if (grpc::Status status = OpenExistingSketch(doc, sketchFeature, openSketch); !status.ok())
		return status;

	const grpc::Status failure = [&]() -> grpc::Status {
		SketchItems open;
		open.Load(openSketch);
		HRESULT hr = doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::ClearSelection2", hr);
		for (size_t i = 0; i < toSelect.size(); ++i)
		{
			ATL::CComPtr<IDispatch> obj = open.Find(toSelect[i]);
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (ATL::CComQIPtr<ISketchSegment> segment(obj.p); segment)
				segment->Select4(VARIANT_TRUE, nullptr, &selected);
			else if (ATL::CComQIPtr<ISketchPoint> point(obj.p); point)
				point->Select4(VARIANT_TRUE, nullptr, &selected);
			if (!selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::INTERNAL,
					"SolidWorks wouldn't select one of the entities in the open sketch -- nothing was deleted");
			}
		}
		hr = doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::EditDelete", hr);
		return grpc::Status::OK;
	}();

	// Closed without the exit rebuild: when the deletion broke the profile of
	// the sketch's feature, InsertSketch(TRUE) opens the Repair Sketch dialog
	// and leaves the sketch open. EditRebuild3 then rebuilds without one.
	doc->ClearSelection2(VARIANT_TRUE);
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_FALSE);
	if (!failure.ok())
		return failure;
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);
	if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"the entities were deleted, but SolidWorks left \"" + sketchName + "\" open for editing");
	VARIANT_BOOL rebuilt = VARIANT_FALSE;
	doc->EditRebuild3(&rebuilt);

	// What's left, read from the closed sketch.
	sketch.Release();
	if (grpc::Status status = SketchOf(sketchFeature, sketch); !status.ok())
		return status;
	SketchItems remaining;
	remaining.Load(sketch);
	int deleted = 0;
	std::set<std::pair<bool, PointKey>> counted;
	for (const auto& ref : request->items())
	{
		if (!counted.insert({ref.point(), KeyOf(ref)}).second)
			continue;
		if (remaining.Find(ref))
			*response->add_kept() = ref;
		else
			++deleted;
	}
	response->set_deleted(deleted);
	if (deleted == 0)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks didn't delete any of the entities from \"" + sketchName + "\"");

	size_t pointsAfter = 0;
	for (const auto& e : remaining.All())
		pointsAfter += e.point ? 1 : 0;
	const long long gonePoints = static_cast<long long>(pointsBefore) - static_cast<long long>(pointsAfter);
	response->set_points_removed(static_cast<int32_t>(std::max(0LL, gonePoints - standalonePoints)));

	const std::set<std::string> dimsAfter = DimensionNames(sketchFeature);
	for (const auto& name : dimsBefore)
		if (!dimsAfter.count(name))
			response->add_dimensions_removed(name);
	const size_t relationsAfter = CountRelations(sketch);
	response->set_relations_removed(
		static_cast<int32_t>(relationsBefore > relationsAfter ? relationsBefore - relationsAfter : 0));

	ReadSketchState(sketch, sketchPid, response->mutable_state());
	sld_mcp::v1::SketchContours contours;
	if (CountContours(sketch, contours))
		*response->mutable_contours() = contours;
	AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
	doc->GraphicsRedraw2();
	return grpc::Status::OK;
}
