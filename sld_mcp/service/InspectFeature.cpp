// InspectFeature.cpp : InspectFeature (one feature's definition and dimensions, read-only;
// see docs/notes/inspection-gaps.md).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "ReferenceHelpers.h"

using namespace sld_mcp::service;

namespace
{
	bool On(VARIANT_BOOL flag) { return flag != VARIANT_FALSE; }

	// Runs `read` with `data`'s selections accessed (the model rolled back to
	// the feature, so its edges and faces are the ones it was built on), then
	// releases them.
	template <typename Data, typename Read>
	grpc::Status WithSelections(IModelDoc2* doc, Data* data, const char* what, Read read)
	{
		VARIANT_BOOL access = VARIANT_FALSE;
		if (HRESULT hr = data->AccessSelections(doc, nullptr, &access); FAILED(hr))
			return ComError(what, hr);
		read();
		data->ReleaseSelectionAccess();
		return grpc::Status::OK;
	}

	void DescribeRef(PersistIds& ids, IDispatch* obj, sld_mcp::v1::DefinitionRef* out)
	{
		out->set_what(ReferenceWhat(obj));
		if (ATL::CComQIPtr<IFace2>(obj) || ATL::CComQIPtr<IEdge>(obj))
			out->set_persist_id(ids.Get(obj));
	}

	void AddPersistIds(PersistIds& ids, const VARIANT& list, google::protobuf::RepeatedPtrField<std::string>* out)
	{
		for (const auto& obj : comvariant::ToDispatches(list))
			if (std::string id = ids.Get(obj); !id.empty())
				out->Add(std::move(id));
	}

	bool IsCut(IFeature* feature)
	{
		return FeatureTypeName(feature).find("Cut") != std::string::npos;
	}

	grpc::Status ReadRevolve(IModelDoc2* doc, PersistIds& ids, IFeature* feature, IRevolveFeatureData2* data,
		sld_mcp::v1::RevolveDefinition* out)
	{
		out->set_cut(IsCut(feature));
		out->set_sketch_name(ProfileSketchName(feature));
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->IsThinFeature(&flag);
		out->set_thin(On(flag));
		int type = swRevolveTypeOneDirection;
		data->get_Type(&type);
		out->set_revolve_type(type);
		double angle = 0.0;
		data->GetRevolutionAngle(VARIANT_TRUE, &angle);
		out->set_angle(angle);
		if (type == swRevolveTypeTwoDirection)
		{
			data->GetRevolutionAngle(VARIANT_FALSE, &angle);
			out->set_angle2(angle);
		}
		flag = VARIANT_FALSE;
		data->get_ReverseDirection(&flag);
		out->set_reverse_direction(On(flag));
		return WithSelections(doc, data, "IRevolveFeatureData2::AccessSelections", [&] {
			ATL::CComPtr<IDispatch> axis;
			data->get_Axis(&axis);
			DescribeRef(ids, axis, out->mutable_axis());
		});
	}

	grpc::Status ReadChamfer(IModelDoc2* doc, PersistIds& ids, IChamferFeatureData2* data,
		sld_mcp::v1::ChamferDefinition* out)
	{
		int type = 0;
		data->get_Type(&type);
		out->set_chamfer_type(type);
		double value = 0.0;
		if (type == swChamferVertex)
		{
			for (int side = 0; side < 3; ++side)
			{
				value = 0.0;
				data->GetVertexChamferDistance(side, &value);
				out->add_vertex_distances(value);
			}
		}
		else
		{
			data->GetEdgeChamferDistance(0, &value);
			out->set_distance(value);
			if (type == swChamferDistanceDistance)
			{
				value = 0.0;
				data->GetEdgeChamferDistance(1, &value);
				out->set_distance2(value);
			}
			else if (type == swChamferAngleDistance)
			{
				value = 0.0;
				data->get_EdgeChamferAngle(&value);
				out->set_angle(value);
			}
		}
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_TangentPropagation(&flag);
		out->set_tangent_propagation(On(flag));
		return WithSelections(doc, data, "IChamferFeatureData2::AccessSelections", [&] {
			ATL::CComVariant edges, faces;
			if (SUCCEEDED(data->get_Edges(&edges)))
				for (const auto& edge : comvariant::ToDispatches(edges))
					if (std::string id = ids.Get(edge); !id.empty())
						out->add_items()->set_persist_id(id);
			if (SUCCEEDED(data->get_Faces(&faces)))
				for (const auto& face : comvariant::ToDispatches(faces))
					if (std::string id = ids.Get(face); !id.empty())
					{
						auto* item = out->add_items();
						item->set_persist_id(id);
						item->set_face(true);
					}
			ATL::CComPtr<IDispatch> vertex;
			if (type == swChamferVertex && SUCCEEDED(data->get_Vertex(&vertex)))
				out->set_vertex_persist_id(ids.Get(vertex));
		});
	}

	grpc::Status ReadShell(IModelDoc2* doc, PersistIds& ids, IShellFeatureData* data, sld_mcp::v1::ShellDefinition* out)
	{
		double thickness = 0.0;
		data->get_Thickness(&thickness);
		out->set_thickness(thickness);
		long direction = 0;
		data->get_Direction(&direction);
		out->set_outward(direction != 0);
		return WithSelections(doc, data, "IShellFeatureData::AccessSelections", [&] {
			ATL::CComVariant removed, walls;
			if (SUCCEEDED(data->get_FacesRemoved(&removed)))
				AddPersistIds(ids, removed, out->mutable_removed_faces());
			if (SUCCEEDED(data->get_MultipleThicknessFaces(&walls)))
			{
				const auto faces = comvariant::ToDispatches(walls);
				for (int i = 0; i < static_cast<int>(faces.size()); ++i)
				{
					auto* wall = out->add_walls();
					wall->set_persist_id(ids.Get(faces[i]));
					double t = 0.0;
					data->GetMultipleThicknessAtIndex(i, &t);
					wall->set_thickness(t);
				}
			}
		});
	}

	grpc::Status ReadDraft(IModelDoc2* doc, PersistIds& ids, IDraftFeatureData2* data, sld_mcp::v1::DraftDefinition* out)
	{
		int type = 0;
		data->get_Type(&type);
		out->set_draft_type(type);
		double angle = 0.0;
		data->get_Angle(&angle);
		out->set_angle(angle);
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_ReverseDirection(&flag);
		out->set_reverse_direction(On(flag));
		short propagation = 0;
		data->get_FacePropagation(&propagation);
		out->set_propagation(propagation);
		return WithSelections(doc, data, "IDraftFeatureData2::AccessSelections", [&] {
			ATL::CComPtr<IDispatch> neutral;
			if (SUCCEEDED(data->get_NeutralPlane(&neutral)) && neutral)
				DescribeRef(ids, neutral, out->mutable_neutral_plane());
			ATL::CComVariant faces;
			if (SUCCEEDED(data->get_FacesToDraft(&faces)))
				AddPersistIds(ids, faces, out->mutable_faces());
		});
	}

	void ReadRib(IFeature* feature, IRibFeatureData2* data, sld_mcp::v1::RibDefinition* out)
	{
		out->set_sketch_name(ProfileSketchName(feature));
		double value = 0.0;
		data->get_Thickness(&value);
		out->set_thickness(value);
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_IsTwoSided(&flag);
		out->set_two_sided(On(flag));
		flag = VARIANT_FALSE;
		data->get_ReverseThicknessDir(&flag);
		out->set_reverse_thickness(On(flag));
		long direction = swRibParallelToSketch;
		data->get_ExtrusionDirection(&direction);
		out->set_normal_to_sketch(direction == swRibNormalToSketch);
		flag = VARIANT_FALSE;
		data->get_FlipSide(&flag);
		out->set_flip_material(On(flag));
		flag = VARIANT_FALSE;
		data->get_EnableDraft(&flag);
		if (On(flag))
		{
			value = 0.0;
			data->get_DraftAngle(&value);
			out->set_draft_angle(value);
			flag = VARIANT_FALSE;
			data->get_DraftOutward(&flag);
			out->set_draft_outward(On(flag));
		}
	}

	grpc::Status ReadLoft(IModelDoc2* doc, IFeature* feature, ILoftFeatureData* data, sld_mcp::v1::LoftDefinition* out)
	{
		out->set_cut(IsCut(feature));
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->IsThinFeature(&flag);
		out->set_thin(On(flag));
		flag = VARIANT_FALSE;
		data->get_Close(&flag);
		out->set_closed(On(flag));
		short tangency = swTangencyNone;
		data->get_StartTangencyType(&tangency);
		out->set_start_tangency(tangency);
		tangency = swTangencyNone;
		data->get_EndTangencyType(&tangency);
		out->set_end_tangency(tangency);
		return WithSelections(doc, data, "ILoftFeatureData::AccessSelections", [&] {
			ATL::CComVariant profiles, guides;
			if (SUCCEEDED(data->get_Profiles(&profiles)))
				for (const auto& profile : comvariant::ToDispatches(profiles))
					out->add_profiles(ReferenceWhat(profile));
			if (SUCCEEDED(data->get_GuideCurves(&guides)))
				for (const auto& guide : comvariant::ToDispatches(guides))
					out->add_guide_curves(ReferenceWhat(guide));
		});
	}

	grpc::Status ReadMirror(IModelDoc2* doc, PersistIds& ids, IMirrorPatternFeatureData* data,
		sld_mcp::v1::MirrorDefinition* out)
	{
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_GeometryPattern(&flag);
		out->set_geometry_pattern(On(flag));
		long faces = 0;
		data->GetMirrorFaceCount(&faces);
		out->set_faces(static_cast<int32_t>(faces));
		return WithSelections(doc, data, "IMirrorPatternFeatureData::AccessSelections", [&] {
			ATL::CComPtr<IDispatch> plane;
			data->get_Plane(&plane);
			DescribeRef(ids, plane, out->mutable_plane());
			ATL::CComVariant seeds;
			if (SUCCEEDED(data->get_PatternFeatureArray(&seeds)))
				for (const auto& seed : FeatureList(seeds))
					out->add_features(FeatureName(seed));
		});
	}

	grpc::Status ReadMirrorSolid(IModelDoc2* doc, PersistIds& ids, IMirrorSolidFeatureData* data,
		sld_mcp::v1::MirrorDefinition* out)
	{
		long bodies = 0;
		data->GetPatternBodyCount(&bodies);
		out->set_bodies(static_cast<int32_t>(bodies));
		return WithSelections(doc, data, "IMirrorSolidFeatureData::AccessSelections", [&] {
			ATL::CComPtr<IDispatch> plane;
			data->get_Face(&plane);
			DescribeRef(ids, plane, out->mutable_plane());
		});
	}

	void ReadHelix(IHelixFeatureData* data, sld_mcp::v1::HelixFeatureDefinition* out)
	{
		long definedBy = -1;
		data->get_DefinedBy(&definedBy);
		// The enum is swHelixDefinedBy_e + 1.
		if (sld_mcp::v1::HelixDefinition_IsValid(static_cast<int>(definedBy + 1)))
			out->set_defined_by(static_cast<sld_mcp::v1::HelixDefinition>(definedBy + 1));
		double value = 0.0;
		data->get_Height(&value);
		out->set_height(value);
		value = 0.0;
		data->get_Pitch(&value);
		out->set_pitch(value);
		value = 0.0;
		data->get_Revolution(&value);
		out->set_revolutions(value);
		value = 0.0;
		data->get_StartingAngle(&value);
		out->set_start_angle(value);
		VARIANT_BOOL flag = VARIANT_FALSE;
		data->get_Clockwise(&flag);
		out->set_clockwise(On(flag));
		flag = VARIANT_FALSE;
		data->get_ReverseDirection(&flag);
		out->set_reverse_direction(On(flag));
		flag = VARIANT_FALSE;
		data->get_Taper(&flag);
		if (On(flag))
		{
			value = 0.0;
			data->get_TaperAngle(&value);
			out->set_taper_angle(value);
			flag = VARIANT_FALSE;
			data->get_TaperOutward(&flag);
			out->set_taper_outward(On(flag));
		}
		flag = VARIANT_FALSE;
		data->get_VariablePitch(&flag);
		out->set_variable_pitch(On(flag));
	}

	// The definition, by the interface SolidWorks hands out for it; kinds
	// not listed here leave it unset.
	grpc::Status DescribeDefinition(IModelDoc2* doc, IModelDocExtension* ext, IFeature* feature,
		sld_mcp::v1::InspectFeatureResponse* out)
	{
		ATL::CComPtr<IDispatch> definition;
		if (FAILED(feature->GetDefinition(&definition)) || !definition)
			return grpc::Status::OK;
		PersistIds ids(ext);
		IDispatch* d = definition.p;
		if (ATL::CComQIPtr<IExtrudeFeatureData2>(d))
			return DescribeExtrudeDefinition(feature, out->mutable_extrude());
		if (ATL::CComQIPtr<IRevolveFeatureData2> data(d); data)
			return ReadRevolve(doc, ids, feature, data, out->mutable_revolve());
		if (ATL::CComQIPtr<ISweepFeatureData>(d))
			return DescribeSweepDefinition(doc, feature, out->mutable_sweep());
		if (ATL::CComQIPtr<ILoftFeatureData> data(d); data)
			return ReadLoft(doc, feature, data, out->mutable_loft());
		if (ATL::CComQIPtr<ISimpleFilletFeatureData2>(d))
			return DescribeFilletDefinition(doc, ext, feature, out->mutable_fillet());
		if (ATL::CComQIPtr<IChamferFeatureData2> data(d); data)
			return ReadChamfer(doc, ids, data, out->mutable_chamfer());
		if (ATL::CComQIPtr<IShellFeatureData> data(d); data)
			return ReadShell(doc, ids, data, out->mutable_shell());
		if (ATL::CComQIPtr<IDraftFeatureData2> data(d); data)
			return ReadDraft(doc, ids, data, out->mutable_draft());
		if (ATL::CComQIPtr<IRibFeatureData2> data(d); data)
		{
			ReadRib(feature, data, out->mutable_rib());
			return grpc::Status::OK;
		}
		if (ATL::CComQIPtr<IMirrorPatternFeatureData> data(d); data)
			return ReadMirror(doc, ids, data, out->mutable_mirror());
		if (ATL::CComQIPtr<IMirrorSolidFeatureData> data(d); data)
			return ReadMirrorSolid(doc, ids, data, out->mutable_mirror());
		if (ATL::CComQIPtr<ILinearPatternFeatureData>(d) || ATL::CComQIPtr<ICircularPatternFeatureData>(d))
			return DescribePatternDefinition(doc, ext, feature, out->mutable_pattern());
		if (ATL::CComQIPtr<IHelixFeatureData> data(d); data)
		{
			ReadHelix(data, out->mutable_helix());
			return grpc::Status::OK;
		}
		if (ATL::CComQIPtr<IWizardHoleFeatureData2>(d))
			DescribeHoleSpec(feature, out->mutable_hole());
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::InspectFeature(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectFeatureRequest* request,
	sld_mcp::v1::InspectFeatureResponse* response)
{
	// Read-only, but accessing a definition's selections rolls the model back
	// for a moment: no other RPC in between.
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
	IFeature* feature = tree[index].feature;

	response->set_name(tree[index].name);
	response->set_type_name(tree[index].type);
	VARIANT_BOOL suppressed = VARIANT_FALSE;
	if (SUCCEEDED(feature->IsSuppressed(&suppressed)))
		response->set_suppressed(suppressed != VARIANT_FALSE);
	int32_t code = 0;
	bool warning = false;
	SetFeatureError(feature, code, warning);
	response->set_error_code(code);
	response->set_error_is_warning(warning);
	for (const auto& display : FeatureDimensions(feature))
		DescribeDimension(display, response->add_dimensions());
	return DescribeDefinition(doc, ext, feature, response);
}
