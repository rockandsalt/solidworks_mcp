// Editing.cpp : EditExtrude, EditMate (see docs/notes/editing.md).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "FeatureTreeHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// One direction of an extrude, as IExtrudeFeatureData2 has it.
	struct ExtrudeSide
	{
		int endCondition = swEndCondBlind;
		double depth = 0.0;
		bool draft = false;
		double draftAngle = 0.0;
		bool draftOutward = false;
		// Up to surface / vertex / body, offset from surface: what it goes up
		// to. Only read while the selections are accessed.
		ATL::CComPtr<IDispatch> reference;
	};

	struct ExtrudeSettings
	{
		bool cut = false;
		bool thin = false;
		bool reverse = false;
		bool both = false;
		bool flipSide = false;
		ExtrudeSide side[2];  // direction 1, direction 2
	};

	sld_mcp::v1::EndCondition ToEndCondition(int raw)
	{
		switch (raw)
		{
		case swEndCondBlind:          return sld_mcp::v1::END_CONDITION_BLIND;
		case swEndCondThroughAll:     return sld_mcp::v1::END_CONDITION_THROUGH_ALL;
		case swEndCondMidPlane:       return sld_mcp::v1::END_CONDITION_MID_PLANE;
		case swEndCondUpToNext:
		case swEndCondThroughNext:    return sld_mcp::v1::END_CONDITION_UP_TO_NEXT;
		case swEndCondThroughAllBoth: return sld_mcp::v1::END_CONDITION_THROUGH_ALL_BOTH;
		default:                      return sld_mcp::v1::END_CONDITION_OTHER;
		}
	}

	bool ToSwEndCondition(sld_mcp::v1::EndCondition condition, int& out)
	{
		switch (condition)
		{
		case sld_mcp::v1::END_CONDITION_BLIND:            out = swEndCondBlind; return true;
		case sld_mcp::v1::END_CONDITION_THROUGH_ALL:      out = swEndCondThroughAll; return true;
		case sld_mcp::v1::END_CONDITION_MID_PLANE:        out = swEndCondMidPlane; return true;
		case sld_mcp::v1::END_CONDITION_UP_TO_NEXT:       out = swEndCondUpToNext; return true;
		case sld_mcp::v1::END_CONDITION_THROUGH_ALL_BOTH: out = swEndCondThroughAllBoth; return true;
		default:                                          return false;
		}
	}

	std::string EndConditionName(int raw)
	{
		switch (raw)
		{
		case swEndCondBlind:             return "blind";
		case swEndCondThroughAll:        return "through all";
		case swEndCondMidPlane:          return "mid plane";
		case swEndCondUpToNext:
		case swEndCondThroughNext:       return "up to next";
		case swEndCondThroughAllBoth:    return "through all both";
		case swEndCondUpToVertex:        return "up to vertex";
		case swEndCondUpToSurface:       return "up to surface";
		case swEndCondOffsetFromSurface: return "offset from surface";
		case swEndCondUpToBody:          return "up to body";
		default:                         return "end condition " + std::to_string(raw);
		}
	}

	// End conditions that use `depth`.
	bool UsesDepth(int endCondition)
	{
		return endCondition == swEndCondBlind || endCondition == swEndCondMidPlane ||
			endCondition == swEndCondOffsetFromSurface;
	}

	// Direction 1's end conditions that leave no room for a direction 2.
	bool IsTwoSided(int endCondition)
	{
		return endCondition == swEndCondMidPlane || endCondition == swEndCondThroughAllBoth;
	}

	ExtrudeSettings ReadExtrude(IExtrudeFeatureData2* data, bool withReferences)
	{
		ExtrudeSettings s;
		VARIANT_BOOL flag = VARIANT_FALSE, base = VARIANT_FALSE;
		// IsBossFeature is false for the part's first extrude (seen in
		// SOLIDWORKS 2026); that one is IsBaseExtrude.
		data->IsBossFeature(&flag);
		data->IsBaseExtrude(&base);
		s.cut = flag == VARIANT_FALSE && base == VARIANT_FALSE;
		flag = VARIANT_FALSE;
		data->IsThinFeature(&flag);
		s.thin = flag != VARIANT_FALSE;
		flag = VARIANT_FALSE;
		data->get_ReverseDirection(&flag);
		s.reverse = flag != VARIANT_FALSE;
		flag = VARIANT_FALSE;
		data->get_BothDirections(&flag);
		s.both = flag != VARIANT_FALSE;
		if (s.cut)
		{
			flag = VARIANT_FALSE;
			data->get_FlipSideToCut(&flag);
			s.flipSide = flag != VARIANT_FALSE;
		}
		for (int i = 0; i < 2; ++i)
		{
			const VARIANT_BOOL forward = i == 0 ? VARIANT_TRUE : VARIANT_FALSE;
			ExtrudeSide& side = s.side[i];
			data->GetEndCondition(forward, &side.endCondition);
			data->GetDepth(forward, &side.depth);
			flag = VARIANT_FALSE;
			data->GetDraftWhileExtruding(forward, &flag);
			side.draft = flag != VARIANT_FALSE;
			data->GetDraftAngle(forward, &side.draftAngle);
			flag = VARIANT_FALSE;
			data->GetDraftOutward(forward, &flag);
			side.draftOutward = flag != VARIANT_FALSE;
			if (withReferences && ToEndCondition(side.endCondition) == sld_mcp::v1::END_CONDITION_OTHER)
			{
				long referenceType = 0;
				data->GetEndConditionReference(forward, &referenceType, &side.reference);
			}
		}
		return s;
	}

	bool Differs(double a, double b) { return std::abs(a - b) > 1e-12; }

	// Writes what differs between `target` and `current` (both read from
	// this feature) into `data`, in the order the Extrude PropertyManager
	// would: directions first, then each side's end condition before its
	// depth.
	void WriteExtrude(IExtrudeFeatureData2* data, const ExtrudeSettings& target, const ExtrudeSettings& current)
	{
		if (target.reverse != current.reverse)
			data->put_ReverseDirection(target.reverse ? VARIANT_TRUE : VARIANT_FALSE);
		if (target.both != current.both)
			data->put_BothDirections(target.both ? VARIANT_TRUE : VARIANT_FALSE);
		for (int i = 0; i < 2; ++i)
		{
			if (i == 1 && !target.both)
				break;
			const VARIANT_BOOL forward = i == 0 ? VARIANT_TRUE : VARIANT_FALSE;
			const ExtrudeSide& want = target.side[i];
			const ExtrudeSide& have = current.side[i];
			if (want.endCondition != have.endCondition)
			{
				data->SetEndCondition(forward, want.endCondition);
				if (want.reference)
					data->SetEndConditionReference(forward, want.reference);
			}
			if (UsesDepth(want.endCondition) && Differs(want.depth, have.depth))
				data->SetDepth(forward, want.depth);
			if (want.draft != have.draft)
				data->SetDraftWhileExtruding(forward, want.draft ? VARIANT_TRUE : VARIANT_FALSE);
			if (want.draft && Differs(want.draftAngle, have.draftAngle))
				data->SetDraftAngle(forward, want.draftAngle);
			if (want.draft && want.draftOutward != have.draftOutward)
				data->SetDraftOutward(forward, want.draftOutward ? VARIANT_TRUE : VARIANT_FALSE);
		}
		if (target.cut && target.flipSide != current.flipSide)
			data->put_FlipSideToCut(target.flipSide ? VARIANT_TRUE : VARIANT_FALSE);
	}

	grpc::Status ExtrudeDataOf(IFeature* feature, ATL::CComQIPtr<IExtrudeFeatureData2>& data)
	{
		ATL::CComPtr<IDispatch> definition;
		HRESULT hr = feature->GetDefinition(&definition);
		if (FAILED(hr))
			return ComError("IFeature::GetDefinition", hr);
		data = definition.p;
		if (!data)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + FeatureName(feature) + "\" is a " + FeatureTypeName(feature) + " feature, not a "
				"boss- or cut-extrude");
		return grpc::Status::OK;
	}

	// Gives the feature `target`'s settings with IFeature::ModifyDefinition.
	// `previous` gets the settings it had, read with the selections accessed
	// (so with end-condition references), for putting them back. False if
	// SolidWorks rejected the change; it then keeps the old definition (seen
	// in SOLIDWORKS 2026 for a cut reversed into nothing).
	grpc::Status ApplyExtrude(IModelDoc2* doc, IFeature* feature, const ExtrudeSettings& target,
		ExtrudeSettings* previous, bool& modified)
	{
		modified = false;
		ATL::CComQIPtr<IExtrudeFeatureData2> data;
		if (grpc::Status status = ExtrudeDataOf(feature, data); !status.ok())
			return status;
		VARIANT_BOOL access = VARIANT_FALSE;
		HRESULT hr = data->AccessSelections(doc, nullptr, &access);
		if (FAILED(hr))
			return ComError("IExtrudeFeatureData2::AccessSelections", hr);
		const ExtrudeSettings current = ReadExtrude(data, true);
		if (previous)
			*previous = current;
		WriteExtrude(data, target, current);
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = feature->ModifyDefinition(data, doc, nullptr, &ok);
		if (FAILED(hr) || !ok)
			data->ReleaseSelectionAccess();
		if (FAILED(hr))
			return ComError("IFeature::ModifyDefinition", hr);
		modified = ok != VARIANT_FALSE;
		return grpc::Status::OK;
	}

	void DescribeExtrudeSide(const ExtrudeSide& side, sld_mcp::v1::ExtrudeDirection* out)
	{
		out->set_end_condition(ToEndCondition(side.endCondition));
		out->set_raw_end_condition(side.endCondition);
		if (UsesDepth(side.endCondition))
			out->set_depth(side.depth);
		if (side.draft)
		{
			out->set_draft_angle(side.draftAngle);
			out->set_draft_outward(side.draftOutward);
		}
	}

	void DescribeExtrude(IFeature* feature, const ExtrudeSettings& s, sld_mcp::v1::ExtrudeDefinition* out)
	{
		out->set_name(FeatureName(feature));
		out->set_type_name(FeatureTypeName(feature));
		out->set_cut(s.cut);
		out->set_thin(s.thin);
		out->set_sketch_name(ProfileSketchName(feature));
		DescribeExtrudeSide(s.side[0], out->mutable_direction1());
		if (s.both && !IsTwoSided(s.side[0].endCondition))
			DescribeExtrudeSide(s.side[1], out->mutable_direction2());
		out->set_reverse_direction(s.reverse);
		out->set_flip_side_to_cut(s.cut && s.flipSide);
		for (const auto& display : FeatureDimensions(feature))
			DescribeDimension(display, out->add_dimensions());
		VARIANT_BOOL warning = VARIANT_FALSE;
		long code = swFeatureErrorNone;
		if (SUCCEEDED(feature->GetErrorCode2(&warning, &code)))
		{
			out->set_error_code(static_cast<int32_t>(code));
			out->set_error_is_warning(code != swFeatureErrorNone && warning != VARIANT_FALSE);
		}
	}

	// Reads the definition fresh (no selection access) and describes it.
	grpc::Status DescribeExtrudeNow(IFeature* feature, sld_mcp::v1::ExtrudeDefinition* out, ExtrudeSettings* settings = nullptr)
	{
		ATL::CComQIPtr<IExtrudeFeatureData2> data;
		if (grpc::Status status = ExtrudeDataOf(feature, data); !status.ok())
			return status;
		const ExtrudeSettings s = ReadExtrude(data, false);
		DescribeExtrude(feature, s, out);
		if (settings)
			*settings = s;
		return grpc::Status::OK;
	}

	std::string ExtrudeErrorText(long code)
	{
		switch (code)
		{
		case swFeatureErrorExtrusionDisjoint:
			return "it would make disjoint bodies";
		case swFeatureErrorExtrusionNoEndFound:
			return "its end condition can't be reached";
		case swFeatureErrorExtrusionBadGeometricConditions:
			return "the geometric conditions are bad";
		case swFeatureErrorCutNotIntersectModel:
			return "the cut doesn't intersect the model";
		default:
			return "SolidWorks error " + std::to_string(code);
		}
	}

	// A mate's current entity, from its definition or IMateEntity2: unlike
	// ResolveMateRef this takes whatever the mate already has (e.g. one of
	// the assembly's planes, which is on no component).
	bool ExistingMateRef(IAssemblyDoc* assembly, PersistIds& ids, IDispatch* obj, MateRef& ref)
	{
		if (!obj)
			return false;
		ref.obj = obj;
		if (ATL::CComQIPtr<IComponent2> component(obj); component)
		{
			ref.component = component;
			ref.isComponent = true;
			ref.what = ComponentName(component);
			return true;
		}
		if (ATL::CComQIPtr<IFeature> feature(obj); feature)
		{
			ref.isFeature = true;
			ref.what = FeatureName(feature);
			ref.component = ComponentOfFeature(assembly, feature, ids);
			return true;
		}
		if (!ATL::CComQIPtr<IEntity>(obj))
			return false;
		ref.what = MateEntityKind(obj);
		ref.component = ComponentOf(obj);
		return true;
	}

	// The mate's current entities: the definition's EntitiesToMate (read
	// late-bound, since each mate type has its own *MateFeatureData
	// interface), else each IMateEntity2's reference.
	std::vector<MateRef> ExistingMateRefs(IAssemblyDoc* assembly, PersistIds& ids, IFeature* feature, IMate2* mate)
	{
		std::vector<MateRef> refs;
		ATL::CComPtr<IDispatch> definition;
		ATL::CComVariant entities;
		if (SUCCEEDED(feature->GetDefinition(&definition)) && definition &&
			InvokeByName(definition, L"EntitiesToMate", entities))
			for (const auto& disp : comvariant::ToDispatches(entities))
			{
				MateRef ref;
				if (!ExistingMateRef(assembly, ids, disp, ref))
				{
					refs.clear();
					break;
				}
				refs.push_back(ref);
			}
		if (refs.size() >= 2)
			return refs;

		refs.clear();
		long count = 0;
		if (FAILED(mate->GetMateEntityCount(&count)))
			return refs;
		for (long i = 0; i < count && i < 16; ++i)
		{
			ATL::CComPtr<IMateEntity2> entity;
			ATL::CComPtr<IDispatch> reference;
			MateRef ref;
			if (FAILED(mate->MateEntity(i, &entity)) || !entity || FAILED(entity->get_Reference(&reference)) ||
				!ExistingMateRef(assembly, ids, reference, ref))
				return {};
			refs.push_back(ref);
		}
		return refs;
	}

	// What the mate is now. Entities are left empty if SolidWorks can't hand
	// them out in a selectable form.
	grpc::Status ReadMate(IAssemblyDoc* assembly, PersistIds& ids, IFeature* feature, MateSettings& s)
	{
		ATL::CComPtr<IDispatch> specific;
		HRESULT hr = feature->GetSpecificFeature2(&specific);
		if (FAILED(hr))
			return ComError("IFeature::GetSpecificFeature2", hr);
		ATL::CComQIPtr<IMate2> mate(specific.p);
		if (!mate)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + FeatureName(feature) + "\" is a " + FeatureTypeName(feature) + " feature, not a mate");
		mate->get_Type(&s.type);
		mate->get_Alignment(&s.alignment);
		VARIANT_BOOL flag = VARIANT_FALSE;
		mate->get_Flipped(&flag);
		s.flip = flag != VARIANT_FALSE;
		for (const auto& display : FeatureDimensions(feature))
		{
			sld_mcp::v1::DimensionInfo info;
			if (!DescribeDimension(display, &info))
				continue;
			if (s.type == swMateANGLE)
				s.angle = info.value();
			else
				s.distance = info.value();
			break;
		}
		s.limited = MateLimitsOf(feature, s.type, s.minimum, s.maximum);
		if (s.type == swMateCONCENTRIC)
		{
			ATL::CComPtr<IDispatch> definition;
			ATL::CComVariant lock;
			if (SUCCEEDED(feature->GetDefinition(&definition)) && definition &&
				InvokeByName(definition, L"LockRotation", lock) && SUCCEEDED(lock.ChangeType(VT_BOOL)))
				s.lockRotation = lock.boolVal != VARIANT_FALSE;
		}
		s.refs = ExistingMateRefs(assembly, ids, feature, mate);
		return grpc::Status::OK;
	}

	// Late-bound property put by name, for the *MateFeatureData objects
	// (each mate type has its own interface). False if the object has no
	// such property or rejects the value.
	bool PutByName(IDispatch* obj, const wchar_t* name, const VARIANT& value)
	{
		if (!obj)
			return false;
		DISPID id = DISPID_UNKNOWN;
		LPOLESTR names[] = { const_cast<LPOLESTR>(name) };
		if (FAILED(obj->GetIDsOfNames(IID_NULL, names, 1, LOCALE_USER_DEFAULT, &id)))
			return false;
		DISPID put = DISPID_PROPERTYPUT;
		DISPPARAMS params = { const_cast<VARIANT*>(&value), &put, 1, 1 };
		return SUCCEEDED(obj->Invoke(id, IID_NULL, LOCALE_USER_DEFAULT, DISPATCH_PROPERTYPUT, &params, nullptr,
			nullptr, nullptr));
	}

	// Changes a mate without changing its type: the settings go into its
	// definition (IFeature::GetDefinition, a *MateFeatureData) and
	// IFeature::ModifyDefinition. The mate keeps its name (its persist id
	// changes). Properties a type doesn't have (an alignment on a
	// perpendicular mate, ...) are skipped. `modified` = what
	// ModifyDefinition returned.
	grpc::Status ModifyMate(IModelDoc2* doc, IFeature* feature, const MateSettings& s, bool& modified)
	{
		modified = false;
		ATL::CComPtr<IDispatch> definition;
		HRESULT hr = feature->GetDefinition(&definition);
		if (FAILED(hr))
			return ComError("IFeature::GetDefinition", hr);
		if (!definition)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks has no editable definition for \"" + FeatureName(feature) + "\"");
		if (s.newRefs)
		{
			std::vector<IDispatch*> objects;
			for (const auto& ref : s.refs)
				objects.push_back(ref.obj);
			if (!PutByName(definition, L"EntitiesToMate", comvariant::FromDispatches(objects)))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't take those entities for \"" + FeatureName(feature) + "\"");
		}
		if (s.type != swMateLOCK)
			PutByName(definition, L"MateAlignment", ATL::CComVariant(s.alignment));
		if (s.type == swMateDISTANCE || s.type == swMateANGLE)
		{
			PutByName(definition, L"FlipDimension", ATL::CComVariant(s.flip));
			// IsAdvancedMate makes a plain distance / angle mate a limit mate
			// in place, keeping its name, and back (seen 2026-10-25).
			const bool angle = s.type == swMateANGLE;
			PutByName(definition, L"IsAdvancedMate", ATL::CComVariant(s.limited));
			if (s.limited)
			{
				PutByName(definition, angle ? L"MinimumAngle" : L"MinimumDistance", ATL::CComVariant(s.minimum));
				PutByName(definition, angle ? L"MaximumAngle" : L"MaximumDistance", ATL::CComVariant(s.maximum));
			}
			if (s.type == swMateDISTANCE)
				PutByName(definition, L"Distance", ATL::CComVariant(s.distance));
			else
				PutByName(definition, L"Angle", ATL::CComVariant(s.angle));
		}
		if (s.type == swMateCONCENTRIC)
			PutByName(definition, L"LockRotation", ATL::CComVariant(s.lockRotation));
		VARIANT_BOOL ok = VARIANT_FALSE;
		hr = feature->ModifyDefinition(definition, doc, nullptr, &ok);
		if (FAILED(hr))
			return ComError("IFeature::ModifyDefinition", hr);
		modified = ok != VARIANT_FALSE;
		return grpc::Status::OK;
	}

	bool SetSuppressed(IFeature* feature, bool suppressed)
	{
		VARIANT_BOOL ok = VARIANT_FALSE;
		return SUCCEEDED(feature->SetSuppression2(suppressed ? swSuppressFeature : swUnSuppressFeature,
			swThisConfiguration, ATL::CComVariant(), &ok)) && ok;
	}
}

grpc::Status sld_mcp::service::DescribeExtrudeDefinition(IFeature* feature, sld_mcp::v1::ExtrudeDefinition* out)
{
	return DescribeExtrudeNow(feature, out);
}

grpc::Status SolidWorksService::EditExtrude(grpc::ServerContext* context,
	const sld_mcp::v1::EditExtrudeRequest* request,
	sld_mcp::v1::EditExtrudeResponse* response)
{
	// Validate before touching SolidWorks.
	int endCondition = swEndCondBlind, endCondition2 = swEndCondBlind;
	const bool setEnd = request->end_condition() != sld_mcp::v1::END_CONDITION_UNSPECIFIED;
	const bool setEnd2 = request->end_condition2() != sld_mcp::v1::END_CONDITION_UNSPECIFIED;
	if ((setEnd && !ToSwEndCondition(request->end_condition(), endCondition)) ||
		(setEnd2 && !ToSwEndCondition(request->end_condition2(), endCondition2)))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"end conditions that can be set: blind, through all, mid plane, up to next, through all both");
	if (setEnd2 && IsTwoSided(endCondition2))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"mid plane and through all both are direction 1 end conditions");
	if ((request->has_depth() && !(std::isfinite(request->depth()) && request->depth() > 0.0)) ||
		(request->has_depth2() && !(std::isfinite(request->depth2()) && request->depth2() > 0.0)))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a depth must be positive");
	if (request->has_draft_angle() &&
		!(std::isfinite(request->draft_angle()) && request->draft_angle() >= 0.0 && request->draft_angle() < M_PI_2))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the draft angle must be 0 to under 90 degrees");
	const bool anyChange = setEnd || setEnd2 || request->has_depth() || request->has_depth2() ||
		request->has_reverse_direction() || request->has_draft_angle() || request->has_draft_outward() ||
		request->has_flip_side_to_cut() || request->has_both_directions();

	return m_executor.Execute(context, "EditExtrude", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
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

		ExtrudeSettings before;
		if (grpc::Status status = DescribeExtrudeNow(feature, response->mutable_before(), &before); !status.ok())
			return status;
		if (!anyChange)
			return grpc::Status::OK;

		// What it should become: the current settings with the request's changes.
		ExtrudeSettings target = before;
		ExtrudeSide& side1 = target.side[0];
		ExtrudeSide& side2 = target.side[1];
		if (setEnd && endCondition != side1.endCondition)
			side1.endCondition = endCondition;
		if (request->has_depth())
			side1.depth = request->depth();
		if (request->has_reverse_direction())
			target.reverse = request->reverse_direction();
		if (request->has_draft_angle())
		{
			side1.draft = request->draft_angle() > 0.0;
			side1.draftAngle = request->draft_angle();
		}
		if (request->has_draft_outward())
			side1.draftOutward = request->draft_outward();
		if (request->has_flip_side_to_cut())
		{
			if (!target.cut)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" is a boss; flip_side_to_cut only applies to cuts");
			target.flipSide = request->flip_side_to_cut();
		}
		if (request->has_both_directions())
			target.both = request->both_directions();
		if (IsTwoSided(side1.endCondition))
		{
			if (request->has_both_directions() && request->both_directions())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"a mid plane or through-all-both extrude has no second direction");
			target.both = false;
		}
		if ((setEnd2 || request->has_depth2()) && !target.both)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" doesn't go in two directions -- set both_directions to add the second one");
		if (target.both && !before.both)
		{
			// A new second direction starts blind, without draft, unless told
			// otherwise.
			side2.endCondition = swEndCondBlind;
			side2.draft = false;
		}
		if (setEnd2)
			side2.endCondition = endCondition2;
		if (request->has_depth2())
			side2.depth = request->depth2();
		if (request->has_depth() && !UsesDepth(side1.endCondition))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" doesn't end at a depth (" + EndConditionName(side1.endCondition) +
				") -- give end_condition blind or mid plane with the depth");
		if (request->has_depth2() && target.both && !UsesDepth(side2.endCondition))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the second direction doesn't end at a depth (" + EndConditionName(side2.endCondition) +
				") -- give end_condition2 blind with depth2");
		if (UsesDepth(side1.endCondition) && side1.depth <= 0.0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a depth for that end condition");
		if (target.both && UsesDepth(side2.endCondition) && side2.depth <= 0.0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give depth2 for the second direction");
		if (!target.cut && side1.endCondition == swEndCondThroughAllBoth)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "through all both only applies to cuts");

		const auto errorsBefore = FeatureErrors(tree);

		// No "What's Wrong" dialog for features the change breaks.
		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);

		// Puts the old definition back (with any end-condition reference, as
		// read with the selections accessed), then rebuilds.
		ExtrudeSettings previous = before;
		auto restore = [&]() {
			bool ignored = false;
			ApplyExtrude(doc, feature, previous, nullptr, ignored);
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
		};

		bool modified = false;
		if (grpc::Status status = ApplyExtrude(doc, feature, target, &previous, modified); !status.ok())
			return status;
		if (!modified)
		{
			// SolidWorks normally keeps the old definition; make sure.
			sld_mcp::v1::ExtrudeDefinition now;
			ExtrudeSettings settings;
			if (DescribeExtrudeNow(feature, &now, &settings).ok() &&
				(settings.side[0].endCondition != before.side[0].endCondition || settings.reverse != before.reverse ||
					settings.both != before.both || Differs(settings.side[0].depth, before.side[0].depth)))
				restore();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks rejected that change to \"" + name + "\", so it's unchanged -- e.g. a cut that "
				"would remove nothing (try the other direction), an end condition that can't be reached, or "
				"a draft that can't be made");
		}

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		HRESULT hr = doc->EditRebuild3(&rebuilt);
		if (FAILED(hr))
			return ComError("IModelDoc2::EditRebuild3", hr);

		VARIANT_BOOL warning = VARIANT_FALSE;
		long errorCode = swFeatureErrorNone;
		feature->GetErrorCode2(&warning, &errorCode);
		if (errorCode != swFeatureErrorNone && !warning)
		{
			restore();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"with that change \"" + name + "\" fails to rebuild (" + ExtrudeErrorText(errorCode) +
				"), so the old definition was put back");
		}

		if (grpc::Status status = DescribeExtrudeNow(feature, response->mutable_after()); !status.ok())
			return status;
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		DropProblem(response->mutable_new_errors(), name);  // its own state is in `after`
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::EditMate(grpc::ServerContext* context,
	const sld_mcp::v1::EditMateRequest* request,
	sld_mcp::v1::EditMateResponse* response)
{
	// Validate before touching SolidWorks.
	long newType = swMateCOINCIDENT;
	const bool setType = request->type() != sld_mcp::v1::MATE_TYPE_UNSPECIFIED;
	if (setType && !ToSwMateType(request->type(), newType))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"mate types: coincident, concentric, perpendicular, parallel, tangent, distance, angle or lock");
	const int entityCount = request->entity_persist_ids_size();
	if (entityCount != 0 && entityCount != 2)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give no entities (keep the mated ones) or exactly two");
	if (entityCount == 2 && request->entity_persist_ids(0) == request->entity_persist_ids(1))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the two entities are the same one");
	if (request->has_distance() && !(std::isfinite(request->distance()) && request->distance() >= 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the distance must be 0 or more");
	if (request->has_angle() &&
		!(std::isfinite(request->angle()) && request->angle() >= 0.0 && request->angle() <= M_PI + 1e-9))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the angle must be 0 to 180 degrees");
	const bool setLimits = request->has_min_limit() || request->has_max_limit();
	if (setLimits && request->remove_limits())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give new limits or remove them, not both");
	const bool anyChange = setType || entityCount == 2 ||
		request->alignment() != sld_mcp::v1::MATE_ALIGNMENT_UNSPECIFIED || request->has_flip() ||
		request->has_distance() || request->has_angle() || request->has_lock_rotation() || setLimits ||
		request->remove_limits();
	if (!anyChange)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"nothing to change -- give a new type, entities, alignment, flip, distance, angle, limits or lock "
			"rotation");

	return m_executor.Execute(context, "EditMate", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditableAssembly(swApp, doc); !status.ok())
			return status;
		ATL::CComQIPtr<IAssemblyDoc> assembly(doc.p);
		if (!assembly)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active assembly has no IAssemblyDoc interface");
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		ATL::CComQIPtr<IMathUtility> math;
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		PersistIds ids(ext);

		const std::vector<TreeFeature> tree = FeatureTree(doc);
		size_t index = 0;
		if (grpc::Status status = ResolveFeatureTarget(ext, tree, ids, request->mate(), index); !status.ok())
			return status;
		ATL::CComPtr<IFeature> feature = tree[index].feature;
		const std::string name = tree[index].name;

		MateSettings before;
		if (grpc::Status status = ReadMate(assembly, ids, feature, before); !status.ok())
			return status;
		long unused = 0;
		if (!ToSwMateType(ToMateType(before.type), unused))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + name + "\" is a kind of mate this can't edit (" + tree[index].type + ") -- delete it and "
				"add a new one");
		DescribeMate(feature, ids, response->mutable_before());

		// What it should become: the current mate with the request's changes.
		MateSettings target = before;
		if (setType)
			target.type = newType;
		if (request->alignment() != sld_mcp::v1::MATE_ALIGNMENT_UNSPECIFIED)
			target.alignment = ToSwMateAlign(request->alignment());
		if (request->has_flip())
			target.flip = request->flip();
		if (request->has_distance())
		{
			if (target.type != swMateDISTANCE)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "only distance mates take a distance");
			target.distance = request->distance();
		}
		else if (target.type == swMateDISTANCE && before.type != swMateDISTANCE)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the distance for a distance mate");
		if (request->has_angle())
		{
			if (target.type != swMateANGLE)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "only angle mates take an angle");
			target.angle = request->angle();
		}
		else if (target.type == swMateANGLE && before.type != swMateANGLE)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give the angle for an angle mate");
		if (request->has_lock_rotation())
		{
			if (target.type != swMateCONCENTRIC)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "lock rotation only applies to concentric mates");
			target.lockRotation = request->lock_rotation();
		}
		// Limits: kept with the type (a distance range means nothing to an angle
		// mate), set or removed as asked; the value must lie in them.
		if (target.type != before.type)
			target.limited = false;
		if (request->remove_limits())
		{
			if (!before.limited)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + name + "\" has no limits to remove");
			target.limited = false;
		}
		if (setLimits)
		{
			if (!target.limited && !(request->has_min_limit() && request->has_max_limit()))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" has no limits yet -- give both the minimum and the maximum");
			target.limited = true;
			if (request->has_min_limit())
				target.minimum = request->min_limit();
			if (request->has_max_limit())
				target.maximum = request->max_limit();
		}
		if (target.limited)
		{
			if (grpc::Status status = CheckMateLimits(target.type, target.minimum, target.maximum,
					target.type == swMateANGLE ? target.angle : target.distance); !status.ok())
				return grpc::Status(status.error_code(), status.error_message() +
					(request->has_distance() || request->has_angle() ? "" : " -- give a value in the range too"));
		}
		if (entityCount == 2)
		{
			target.refs.assign(2, MateRef());
			target.newRefs = true;
			for (int i = 0; i < 2; ++i)
			{
				grpc::Status status =
					ResolveMateRef(ext, assembly, ids, request->entity_persist_ids(i), target.type, target.refs[i]);
				if (!status.ok())
					return grpc::Status(status.error_code(),
						"entity " + std::to_string(i + 1) + ": " + status.error_message());
			}
			const std::string a = ids.Get(target.refs[0].component), b = ids.Get(target.refs[1].component);
			if (!a.empty() && a == b)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"both entities are on \"" + ComponentName(target.refs[0].component) + "\" -- a mate joins two "
					"different components");
		}
		else
		{
			if (before.refs.size() != 2)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't hand out \"" + name + "\"'s mated entities (a lost reference? see "
					"diagnose_model) -- give the two entities to mate");
			const bool lockNow = before.type == swMateLOCK, lockAfter = target.type == swMateLOCK;
			if (lockAfter && !lockNow)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"a lock mate takes two components -- give them as the entities");
			if (lockNow && !lockAfter)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" mates two whole components -- give the two faces/edges/planes to mate as "
					"the entities");
		}

		const auto errorsBefore = FeatureErrors(tree);
		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;

		if (target.type != before.type)
		{
			// A new type is a new mate (SolidWorks' own EditMate4 replaces the
			// mate too, and names it after the new type -- seen in SOLIDWORKS
			// 2026). Made all or nothing: the old mate is suppressed so it can't
			// conflict, the new one added, and only then the old one deleted.
			if (!SetSuppressed(feature, true))
				return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't suppress \"" + name + "\"");
			ATL::CComPtr<IFeature> created;
			grpc::Status status = CreateMate(doc, assembly, target, created);
			if (!status.ok())
			{
				SetSuppressed(feature, false);
				doc->EditRebuild3(&rebuilt);
				return grpc::Status(status.error_code(), "\"" + name + "\" is unchanged: " + status.error_message());
			}
			doc->ClearSelection2(VARIANT_TRUE);
			VARIANT_BOOL selected = VARIANT_FALSE, deleted = VARIANT_FALSE;
			if (SUCCEEDED(feature->Select2(VARIANT_FALSE, 0, &selected)) && selected)
				ext->DeleteSelection2(0, &deleted);
			doc->ClearSelection2(VARIANT_TRUE);
			feature = created;
			doc->EditRebuild3(&rebuilt);
		}
		else
		{
			// Puts the old settings back, then rebuilds.
			auto restore = [&]() {
				bool ignored = false;
				MateSettings old = before;
				old.newRefs = target.newRefs;
				ModifyMate(doc, feature, old, ignored);
				doc->EditRebuild3(&rebuilt);
			};

			bool modified = false;
			if (grpc::Status status = ModifyMate(doc, feature, target, modified); !status.ok())
				return status;
			if (!modified)
			{
				restore();
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks rejected that change to \"" + name + "\", so it's unchanged -- e.g. entities this "
					"kind of mate can't take, or a change that conflicts with other mates");
			}
			HRESULT hr = doc->EditRebuild3(&rebuilt);
			if (FAILED(hr))
				return ComError("IModelDoc2::EditRebuild3", hr);

			VARIANT_BOOL warning = VARIANT_FALSE;
			long errorCode = swFeatureErrorNone;
			feature->GetErrorCode2(&warning, &errorCode);
			if (errorCode != swFeatureErrorNone && !warning)
			{
				restore();
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"with that change SolidWorks couldn't solve \"" + name + "\" -- " + MateErrorText(errorCode) +
					" -- so it was put back");
			}

			// Check that the definition took (a property the mate type doesn't
			// have is skipped silently).
			MateSettings after;
			if (grpc::Status status = ReadMate(assembly, ids, feature, after); !status.ok())
				return status;
			std::string mismatch;
			if (target.type == swMateDISTANCE && std::abs(after.distance - target.distance) > 1e-9)
				mismatch = "distance " + std::to_string(after.distance * 1000.0) + " mm";
			else if (target.type == swMateANGLE && std::abs(after.angle - target.angle) > 1e-7)
				mismatch = "angle " + std::to_string(Degrees(after.angle)) + " deg";
			else if (after.limited != target.limited ||
				(target.limited && (std::abs(after.minimum - target.minimum) > 1e-9 ||
					std::abs(after.maximum - target.maximum) > 1e-9)))
				mismatch = after.limited ? "limits " + MateValueText(after.type, after.minimum) + " .. " +
					MateValueText(after.type, after.maximum) : std::string("no limits");
			else if (request->alignment() != sld_mcp::v1::MATE_ALIGNMENT_UNSPECIFIED &&
				request->alignment() != sld_mcp::v1::MATE_ALIGNMENT_CLOSEST && after.alignment != target.alignment)
				mismatch = "its alignment unchanged";
			if (!mismatch.empty())
			{
				restore();
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks accepted the change but \"" + name + "\" came out with " + mismatch +
					", so it was put back");
			}
		}

		DescribeMate(feature, ids, response->mutable_after());
		DiagnoseMate(feature, ids, response->mutable_entities());
		ComponentList components(ids, math, response->mutable_components());
		for (const auto& ref : target.refs)
			components.Add(ref.component);
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		DropProblem(response->mutable_new_errors(), FeatureName(feature));  // its own state is in `after`
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}
