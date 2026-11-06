// Fillet.cpp : FilletEdges and ChamferEdges (rounding and beveling edges), EditFillet.
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// The selection mark FeatureFillet3 / InsertFeatureChamfer read the
	// edges and faces from (InsertFeatureChamfer takes 0 or 1, seen
	// 2026-10-19).
	constexpr long kEdgeMark = 1;

	constexpr double kRightAngle = 3.14159265358979323846 / 2.0;

	// More references than anyone selects by hand.
	constexpr int kMaxEdges = 500;

	// The active part and its edges / faces, selected with `mark`, ready for
	// a feature that works on the selection. On error nothing is selected.
	grpc::Status SelectEdges(ISldWorks* swApp,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::GeometryRef>& refs, long mark,
		ATL::CComPtr<IModelDoc2>& doc, ATL::CComPtr<IFeatureManager>& featureManager)
	{
		if (refs.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give at least one edge or face");
		if (refs.size() > kMaxEdges)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"at most " + std::to_string(kMaxEdges) + " edges and faces per call");

		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		HRESULT hr = doc->get_FeatureManager(&featureManager);
		if (FAILED(hr))
			return ComError("IModelDoc2::FeatureManager", hr);
		if (!featureManager)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

		Selection selection;
		for (int i = 0; i < refs.size(); ++i)
		{
			const std::string label = "edges entry " + std::to_string(i + 1);
			if (grpc::Status status = selection.Add(doc, ext, refs[i], mark, label); !status.ok())
				return status;
			const Pick& pick = selection.picks.back();
			if (pick.kind != Pick::Entity || pick.what == "vertex")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					label + ": " + pick.what + " isn't an edge or a face");
		}
		return selection.Select(doc, ext);
	}

	// What the feature call gave back: a feature that made no faces (an
	// equal-distance InsertFeatureChamfer does that, seen 2026-10-19) is
	// deleted and counts as failed, like no feature.
	grpc::Status CheckMadeFaces(IModelDoc2* doc, IFeature* feature, const std::string& failure)
	{
		long faces = 0;
		if (feature && SUCCEEDED(feature->GetFaceCount(&faces)) && faces > 0)
			return grpc::Status::OK;
		if (feature)
			DeleteFeature(doc, feature);
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, failure);
	}

	// A fillet's settings, as ISimpleFilletFeatureData2 has them. Its edges
	// and faces by persist id, as the model is where the fillet is built.
	struct FilletSettings
	{
		int type = swFeatureFilletType_Simple;
		double radius = 0.0;
		bool multipleRadius = false;
		bool propagate = false;
		std::vector<std::string> edges;
		std::vector<std::string> faces;
		long otherItems = 0;  // loops, features

		// Edges, then faces.
		std::vector<std::string> Items() const
		{
			std::vector<std::string> all = edges;
			all.insert(all.end(), faces.begin(), faces.end());
			return all;
		}
	};

	grpc::Status FilletDataOf(IFeature* feature, ATL::CComQIPtr<ISimpleFilletFeatureData2>& data)
	{
		ATL::CComPtr<IDispatch> definition;
		HRESULT hr = feature->GetDefinition(&definition);
		if (FAILED(hr))
			return ComError("IFeature::GetDefinition", hr);
		data = definition.p;
		if (!data)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + FeatureName(feature) + "\" is a " + FeatureTypeName(feature) + " feature, not a fillet "
				"edit_fillet can change (constant-radius fillets only)");
		return grpc::Status::OK;
	}

	std::vector<std::string> PersistIdsOf(PersistIds& ids, const VARIANT& list)
	{
		std::vector<std::string> out;
		for (const auto& obj : comvariant::ToDispatches(list))
			if (std::string id = ids.Get(obj); !id.empty())
				out.push_back(id);
		return out;
	}

	// Reads the settings; `data`'s selections must be accessed (the model
	// is then rolled back to the fillet, so the edges are the ones it rounds).
	FilletSettings ReadFillet(IModelDocExtension* ext, ISimpleFilletFeatureData2* data)
	{
		FilletSettings s;
		data->get_Type(&s.type);
		data->get_DefaultRadius(&s.radius);
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_IsMultipleRadius(&flag);
		s.multipleRadius = flag != VARIANT_FALSE;
		flag = VARIANT_FALSE;
		data->get_PropagateToTangentFaces(&flag);
		s.propagate = flag != VARIANT_FALSE;
		PersistIds ids(ext);
		ATL::CComVariant edges, faces;
		if (SUCCEEDED(data->get_Edges(&edges)))
			s.edges = PersistIdsOf(ids, edges);
		if (SUCCEEDED(data->GetFaces(swSimpleFilletSingleRadius, &faces)))
			s.faces = PersistIdsOf(ids, faces);
		long loops = 0, features = 0;
		data->GetLoopCount(&loops);
		data->GetFeatureCount(&features);
		s.otherItems = loops + features;
		return s;
	}

	// Reads the fillet with its selections accessed, then releases them.
	grpc::Status ReadFilletNow(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature, FilletSettings& s)
	{
		ATL::CComQIPtr<ISimpleFilletFeatureData2> data;
		if (grpc::Status status = FilletDataOf(feature, data); !status.ok())
			return status;
		VARIANT_BOOL access = VARIANT_FALSE;
		HRESULT hr = data->AccessSelections(doc, nullptr, &access);
		if (FAILED(hr))
			return ComError("ISimpleFilletFeatureData2::AccessSelections", hr);
		s = ReadFillet(ext, data);
		data->ReleaseSelectionAccess();
		return grpc::Status::OK;
	}

	grpc::Status DescribeFillet(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature,
		sld_mcp::v1::FilletDefinition* out, FilletSettings* settings)
	{
		FilletSettings s;
		if (grpc::Status status = ReadFilletNow(doc, ext, feature, s); !status.ok())
			return status;
		out->set_name(FeatureName(feature));
		out->set_fillet_type(s.type);
		out->set_radius(s.radius);
		out->set_multiple_radius(s.multipleRadius);
		out->set_tangent_propagation(s.propagate);
		for (const auto& id : s.edges)
			out->add_items()->set_persist_id(id);
		for (const auto& id : s.faces)
		{
			auto* item = out->add_items();
			item->set_persist_id(id);
			item->set_face(true);
		}
		out->set_other_items(static_cast<int32_t>(s.otherItems));
		for (const auto& display : FeatureDimensions(feature))
			DescribeDimension(display, out->add_dimensions());
		int32_t code = 0;
		bool warning = false;
		SetFeatureError(feature, code, warning);
		out->set_error_code(code);
		out->set_error_is_warning(warning);
		if (settings)
			*settings = s;
		return grpc::Status::OK;
	}

	// Gives the fillet `items` (persist ids of edges and faces) and
	// `target`'s radius and propagation with IFeature::ModifyDefinition. The
	// items are resolved with the selections accessed, i.e. where the fillet
	// is built; `labels` names them in errors.
	grpc::Status ApplyFillet(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature,
		const FilletSettings& target, const std::vector<std::string>& items,
		const std::map<std::string, std::string>& labels, bool& modified)
	{
		modified = false;
		const std::string name = FeatureName(feature);
		ATL::CComQIPtr<ISimpleFilletFeatureData2> data;
		if (grpc::Status status = FilletDataOf(feature, data); !status.ok())
			return status;
		VARIANT_BOOL access = VARIANT_FALSE;
		HRESULT hr = data->AccessSelections(doc, nullptr, &access);
		if (FAILED(hr))
			return ComError("ISimpleFilletFeatureData2::AccessSelections", hr);
		const FilletSettings current = ReadFillet(ext, data);
		auto fail = [&](grpc::Status status) {
			data->ReleaseSelectionAccess();
			return status;
		};

		if (items != current.Items())
		{
			std::vector<ATL::CComPtr<IDispatch>> held;
			std::vector<IDispatch*> edges, faces;
			for (const auto& id : items)
			{
				const auto label = labels.find(id);
				const std::string what = label != labels.end() ? label->second : "one of its edges";
				ATL::CComPtr<IDispatch> obj;
				if (!ResolvePersistId(ext, id, obj).ok())
					return fail(grpc::Status(grpc::StatusCode::NOT_FOUND,
						what + " isn't there where \"" + name + "\" is built (made by the fillet itself or a later "
						"feature?) -- pick edges that exist before it: rollback before=\"" + name + "\", "
						"inspect_model, then rollback to the end again"));
				if (ATL::CComQIPtr<IEdge>(obj.p))
					edges.push_back(obj);
				else if (ATL::CComQIPtr<IFace2>(obj.p))
					faces.push_back(obj);
				else
					return fail(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, what + " isn't an edge or a face"));
				held.push_back(obj);
			}
			data->put_Edges(edges.empty() ? ATL::CComVariant() : comvariant::FromDispatches(edges));
			if (!faces.empty() || !current.faces.empty())
				data->SetFaces(swSimpleFilletSingleRadius,
					faces.empty() ? ATL::CComVariant() : comvariant::FromDispatches(faces));
		}
		if (std::abs(target.radius - current.radius) > 1e-12)
			data->put_DefaultRadius(target.radius);
		if (target.propagate != current.propagate)
			data->put_PropagateToTangentFaces(target.propagate ? VARIANT_TRUE : VARIANT_FALSE);

		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = feature->ModifyDefinition(data, doc, nullptr, &ok);
		if (FAILED(hr) || !ok)
			data->ReleaseSelectionAccess();
		if (FAILED(hr))
			return ComError("IFeature::ModifyDefinition", hr);
		modified = ok != VARIANT_FALSE;
		return grpc::Status::OK;
	}
}

grpc::Status sld_mcp::service::DescribeFilletDefinition(IModelDoc2* doc, IModelDocExtension* ext,
	IFeature* feature, sld_mcp::v1::FilletDefinition* out)
{
	return DescribeFillet(doc, ext, feature, out, nullptr);
}

grpc::Status SolidWorksService::FilletEdges(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::FilletEdgesRequest* request,
	sld_mcp::v1::EdgeFeatureResponse* response)
{
	const double radius = request->radius();
	if (!(std::isfinite(radius) && radius > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "radius must be positive");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComPtr<IFeatureManager> featureManager;
	if (grpc::Status status = SelectEdges(swApp, request->edges(), kEdgeMark, doc, featureManager);
		!status.ok())
		return status;

	// A constant-radius fillet, as the UI makes one: uniform radius, keep
	// features, attach edges, plus tangent propagation if asked.
	const long options = swFeatureFilletUniformRadius | swFeatureFilletAttachEdges |
		swFeatureFilletKeepFeatures | (request->tangent_propagation() ? swFeatureFilletPropagate : 0);
	ATL::CComVariant none;
	ATL::CComPtr<IDispatch> filletObj;
	HRESULT hr = featureManager->FeatureFillet3(
		options,
		radius,                        // R1
		0.0,                           // R2 (asymmetric fillets)
		0.0,                           // Rho (conic fillets)
		swFeatureFilletType_Simple,    // Ftyp: constant radius
		swFilletOverFlowType_Default,  // OverflowType
		swFeatureFilletCircular,       // ConicRhoType: circular profile
		none,                          // Radii (per-edge radii)
		none,                          // Dist2Arr
		none,                          // RhoArr
		none,                          // SetBackDistances
		none,                          // PointRadiusArray
		none,                          // PointDist2Array
		none,                          // PointRhoArray
		&filletObj);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IFeatureManager::FeatureFillet3", hr);
	ATL::CComQIPtr<IFeature> fillet(filletObj);
	if (grpc::Status status = CheckMadeFaces(doc, fillet,
			"SolidWorks couldn't fillet those edges -- the radius is probably too big for them (it must "
			"fit within the faces on both sides of each edge, and next to other fillets); try a smaller one");
		!status.ok())
		return status;

	response->set_feature_name(FeatureName(fillet));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::ChamferEdges(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::ChamferEdgesRequest* request,
	sld_mcp::v1::EdgeFeatureResponse* response)
{
	const double distance = request->distance();
	const double angle = request->angle();
	const double otherDistance = request->other_distance();
	if (!(std::isfinite(distance) && distance > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "distance must be positive");
	if (!(std::isfinite(angle) && angle >= 0.0 && angle < kRightAngle))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "angle must be more than 0 and less than 90 degrees");
	if (!(std::isfinite(otherDistance) && otherDistance >= 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "other distance must be positive");
	if (angle > 0.0 && otherDistance > 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give an angle or a second distance, not both");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	ATL::CComPtr<IFeatureManager> featureManager;
	if (grpc::Status status = SelectEdges(swApp, request->edges(), kEdgeMark, doc, featureManager);
		!status.ok())
		return status;

	// Equal distance = angle-distance at 45 degrees: swChamferEqualDistance
	// makes an empty feature through this call. Distance-distance ignores
	// swFeatureChamferFlipDirection, so flipping it swaps the distances
	// (both seen 2026-10-19).
	const bool equal = angle == 0.0 && otherDistance == 0.0;
	const bool twoDistances = otherDistance > 0.0;
	const bool swap = twoDistances && request->flip();
	const long type = twoDistances ? swChamferDistanceDistance : swChamferAngleDistance;
	const long options = (request->flip() && !equal && !twoDistances ? swFeatureChamferFlipDirection : 0) |
		(request->tangent_propagation() ? swFeatureChamferTangentPropagation : 0);
	ATL::CComPtr<IFeature> chamfer;
	HRESULT hr = featureManager->InsertFeatureChamfer(
		options,
		type,                        // ChamferType
		swap ? otherDistance : distance, // Width
		equal ? kRightAngle / 2.0 : angle, // Angle (angle-distance)
		swap ? distance : otherDistance, // OtherDist (distance-distance)
		0.0,                         // VertexChamDist1..3 (vertex chamfers)
		0.0,
		0.0,
		&chamfer);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IFeatureManager::InsertFeatureChamfer", hr);
	if (grpc::Status status = CheckMadeFaces(doc, chamfer,
			"SolidWorks couldn't chamfer those edges -- the chamfer is probably too big for them (it must "
			"fit within the faces on both sides of each edge); try a smaller distance");
		!status.ok())
		return status;

	response->set_feature_name(FeatureName(chamfer));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::EditFillet(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::EditFilletRequest* request,
	sld_mcp::v1::EditFilletResponse* response)
{
	// Validate before touching SolidWorks.
	if (request->has_radius() && !(std::isfinite(request->radius()) && request->radius() > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "radius must be positive");
	if (request->edges_size() > 0 && request->add_edges_size() > 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give edges (replacing what it rounds) or add_edges, not both");
	if (request->edges_size() + request->add_edges_size() + request->remove_edges_size() > kMaxEdges)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"at most " + std::to_string(kMaxEdges) + " edges and faces per call");
	const bool newItems = request->edges_size() > 0 || request->add_edges_size() > 0 || request->remove_edges_size() > 0;
	const bool anyChange = newItems || request->has_radius() || request->has_tangent_propagation();

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
	PersistIds ids(ext);

	const std::vector<TreeFeature> tree = FeatureTree(doc);
	size_t index = 0;
	if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, request->feature(), index); !status.ok())
		return status;
	ATL::CComPtr<IFeature> feature = tree[index].feature;
	const std::string name = tree[index].name;

	FilletSettings before;
	if (grpc::Status status = DescribeFillet(doc, ext, feature, response->mutable_before(), &before); !status.ok())
		return status;
	if (!anyChange)
		return grpc::Status::OK;
	if (before.type != swFeatureFilletType_Simple)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"\"" + name + "\" isn't a constant-radius fillet (swFeatureFilletType_e " + std::to_string(before.type) +
			"), which is all edit_fillet changes -- set_dimension for its sizes, or delete it and make it again");
	if (request->has_radius() && before.multipleRadius)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"\"" + name + "\" has a radius per edge -- set_dimension on the one to change");

	// What it should become: the current settings with the request's changes.
	FilletSettings target = before;
	if (request->has_radius())
		target.radius = request->radius();
	if (request->has_tangent_propagation())
		target.propagate = request->tangent_propagation();
	std::vector<std::string> items = before.Items();
	std::map<std::string, std::string> labels;
	for (int i = 0; i < request->remove_edges_size(); ++i)
	{
		const auto it = std::find(items.begin(), items.end(), request->remove_edges(i));
		if (it == items.end())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"remove_edges entry " + std::to_string(i + 1) + " isn't one of the edges or faces \"" + name +
				"\" rounds -- give ids from its own list (edit_fillet with only the feature reports it)");
		items.erase(it);
	}
	auto addItems = [&](const google::protobuf::RepeatedPtrField<std::string>& list, const std::string& field) {
		for (int i = 0; i < list.size(); ++i)
			if (std::find(items.begin(), items.end(), list[i]) == items.end())
			{
				items.push_back(list[i]);
				labels.emplace(list[i], field + " entry " + std::to_string(i + 1));
			}
	};
	if (request->edges_size() > 0)
		items.clear();
	addItems(request->edges(), "edges");
	addItems(request->add_edges(), "add_edges");
	if (items.empty() && before.otherItems == 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"that leaves \"" + name + "\" nothing to round -- delete it instead");

	const std::vector<std::string> oldItems = before.Items();
	DefinitionEdit edit;
	edit.apply = [&](bool& modified) {
		return ApplyFillet(doc, ext, feature, target, items, labels, modified);
	};
	edit.restore = [&]() {
		bool ignored = false;
		ApplyFillet(doc, ext, feature, before, oldItems, {}, ignored);
	};
	edit.check = [&]() -> std::string {
		FilletSettings now;
		if (!ReadFilletNow(doc, ext, feature, now).ok())
			return "no definition SolidWorks would hand out";
		if (std::abs(now.radius - target.radius) > 1e-9)
			return "radius " + std::to_string(now.radius * 1000.0) + " mm";
		if (now.propagate != target.propagate)
			return std::string("tangent propagation ") + (now.propagate ? "on" : "off");
		if (newItems && now.Items().size() != items.size())
			return std::to_string(now.Items().size()) + " edges and faces instead of " + std::to_string(items.size());
		return {};
	};
	edit.rejected = "e.g. a radius too big for the edges (it must fit within the faces on both sides of each edge, "
		"and next to other fillets), or edges SolidWorks can't round together";
	if (grpc::Status status = EditDefinition(swApp, doc, feature, tree, edit, response->mutable_new_errors());
		!status.ok())
		return status;
	return DescribeFillet(doc, ext, feature, response->mutable_after(), nullptr);
}
