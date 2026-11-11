// AssemblyHelpers.cpp : see AssemblyHelpers.h.
#include "pch.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"

namespace sld_mcp::service
{
	ATL::CComPtr<IComponent2> ComponentOf(IUnknown* obj)
	{
		ATL::CComQIPtr<IEntity> entity(obj);
		ATL::CComPtr<IDispatch> disp;
		if (!entity || FAILED(entity->GetComponent(&disp)) || !disp)
			return {};
		ATL::CComQIPtr<IComponent2> component(disp.p);
		return ATL::CComPtr<IComponent2>(component);
	}

	std::string ComponentName(IComponent2* component)
	{
		ATL::CComBSTR name;
		if (component)
			component->get_Name2(&name);
		return ToUtf8(name);
	}

	ATL::CComPtr<IComponent2> AssemblyComponentOf(IAssemblyDoc* assembly, IComponent2* viewComponent)
	{
		ATL::CComPtr<IComponent2> found;
		for (std::string name = ComponentName(viewComponent); !name.empty() && !found;)
		{
			assembly->GetComponentByName(FromUtf8(name), &found);
			const size_t slash = name.find('/');
			name = slash == std::string::npos ? std::string() : name.substr(slash + 1);
		}
		return found;
	}

	std::vector<ATL::CComPtr<IComponent2>> AllComponents(IAssemblyDoc* assembly, RequestControl* control)
	{
		std::vector<ATL::CComPtr<IComponent2>> out;
		if (control)
			control->Checkpoint();
		ATL::CComVariant all;
		if (FAILED(assembly->GetComponents(VARIANT_FALSE, &all)))
			return out;
		for (const auto& disp : comvariant::ToDispatches(all))
		{
			if (control)
				control->Checkpoint();
			if (ATL::CComQIPtr<IComponent2> component(disp.p); component)
				out.push_back(ATL::CComPtr<IComponent2>(component));
		}
		return out;
	}

	bool IsSubassembly(IComponent2* component)
	{
		int children = 0;
		return SUCCEEDED(component->IGetChildrenCount(&children)) && children > 0;
	}

	bool IsSuppressed(IComponent2* component)
	{
		long state = swComponentFullyResolved;
		return SUCCEEDED(component->GetSuppression2(&state)) && state == swComponentSuppressed;
	}

	std::vector<ATL::CComPtr<IComponent2>> GeometryComponents(IAssemblyDoc* assembly, bool visibleOnly,
		RequestControl* control)
	{
		std::vector<ATL::CComPtr<IComponent2>> out;
		for (const auto& component : AllComponents(assembly, control))
		{
			if (control)
				control->Checkpoint();
			if (IsSubassembly(component) || IsSuppressed(component))
				continue;
			// A hidden subassembly hides its parts, whose own state stays "shown"
			// (seen 2026-11-01).
			bool hidden = false;
			for (ATL::CComPtr<IComponent2> c = component; visibleOnly && c && !hidden; c = ParentOf(c))
			{
				if (control)
					control->Checkpoint();
				VARIANT_BOOL flag = VARIANT_FALSE;
				hidden = SUCCEEDED(c->IsHidden(VARIANT_TRUE, &flag)) && flag;
			}
			if (!hidden)
				out.push_back(component);
		}
		return out;
	}

	std::vector<ATL::CComPtr<IBody2>> ComponentBodies(IComponent2* component, RequestControl* control)
	{
		std::vector<ATL::CComPtr<IBody2>> out;
		for (long type : { static_cast<long>(swSolidBody), static_cast<long>(swSheetBody) })
		{
			if (control)
				control->Checkpoint();
			ATL::CComVariant info, bodies;
			if (FAILED(component->GetBodies3(type, &info, &bodies)))
				continue;
			for (const auto& disp : comvariant::ToDispatches(bodies))
			{
				if (control)
					control->Checkpoint();
				if (ATL::CComQIPtr<IBody2> body(disp.p); body)
					out.push_back(ATL::CComPtr<IBody2>(body));
			}
		}
		return out;
	}

	bool PlacementOf(IMathUtility* math, IMathTransform* xform, Eigen::Affine3d& out)
	{
		// Where the component's origin and unit axes land pins down its
		// component -> assembly transform, whatever IMathTransform's internal
		// layout is.
		const Eigen::Vector3d unit[4] = { Eigen::Vector3d::Zero(), Eigen::Vector3d::UnitX(),
			Eigen::Vector3d::UnitY(), Eigen::Vector3d::UnitZ() };
		Eigen::Vector3d placed[4];
		for (int i = 0; i < 4; ++i)
			if (!TransformPoint(math, xform, unit[i], placed[i]))
				return false;
		out = Eigen::Affine3d::Identity();
		out.translation() = placed[0];
		for (int a = 0; a < 3; ++a)
			out.linear().col(a) = placed[a + 1] - placed[0];
		return true;
	}

	void SetPlacement(const Eigen::Affine3d& placement, sld_mcp::v1::ComponentInfo* info)
	{
		SetVec3(info->mutable_origin(), Eigen::Vector3d(placement.translation()));
		SetVec3(info->mutable_x_axis(), Eigen::Vector3d(placement.linear().col(0)));
		SetVec3(info->mutable_y_axis(), Eigen::Vector3d(placement.linear().col(1)));
		SetVec3(info->mutable_z_axis(), Eigen::Vector3d(placement.linear().col(2)));
	}

	void DescribePlacement(IComponent2* component, IMathUtility* math, sld_mcp::v1::ComponentInfo* info)
	{
		info->set_local_geometry(true);
		ATL::CComPtr<IMathTransform> xform;
		if (FAILED(component->get_Transform2(&xform)) || !xform)
			return;
		Eigen::Affine3d placement;
		if (!PlacementOf(math, xform, placement))
			return;
		SetPlacement(placement, info);

		// Assembly-space box of the component vs. its bodies' own boxes.
		ATL::CComVariant assemblyBoxVar;
		const Eigen::AlignedBox3d assemblyBox =
			SUCCEEDED(component->GetBox(VARIANT_FALSE, VARIANT_FALSE, &assemblyBoxVar))
			? ToAlignedBox(comvariant::ToDoubles(assemblyBoxVar)) : Eigen::AlignedBox3d();
		Eigen::AlignedBox3d bodyBox;
		for (const auto& body : ComponentBodies(component))
		{
			ATL::CComVariant box;
			if (SUCCEEDED(body->GetBodyBox(&box)))
				bodyBox.extend(ToAlignedBox(comvariant::ToDoubles(box)));
		}
		if (assemblyBox.isEmpty() || bodyBox.isEmpty())
			return;
		// The bodies' box with the placement applied (box of its 8 placed
		// corners), compared with the box as it is: whichever is closer to
		// the component's assembly-space box tells which space the body
		// geometry is in. Comparing whole boxes, not just centers, also
		// catches a part rotated about its own center. With an identity
		// placement both match and either answer is right.
		Eigen::AlignedBox3d movedBox;
		for (int corner = 0; corner < 8; ++corner)
			movedBox.extend(placement * bodyBox.corner(static_cast<Eigen::AlignedBox3d::CornerType>(corner)));
		auto mismatch = [&](const Eigen::AlignedBox3d& box) {
			return (box.min() - assemblyBox.min()).cwiseAbs().sum() + (box.max() - assemblyBox.max()).cwiseAbs().sum();
		};
		info->set_local_geometry(mismatch(movedBox) <= mismatch(bodyBox));
	}

	sld_mcp::v1::ComponentState ToComponentState(long state)
	{
		switch (state)
		{
		case swComponentSuppressed:
			return sld_mcp::v1::COMPONENT_STATE_SUPPRESSED;
		case swComponentLightweight:
		case swComponentFullyLightweight:
			return sld_mcp::v1::COMPONENT_STATE_LIGHTWEIGHT;
		default:
			return sld_mcp::v1::COMPONENT_STATE_RESOLVED;
		}
	}

	void DescribeComponent(IComponent2* component, PersistIds& ids, IMathUtility* math,
		sld_mcp::v1::ComponentInfo* info)
	{
		info->set_persist_id(ids.Get(component));
		info->set_name(ComponentName(component));

		ATL::CComBSTR path;
		if (SUCCEEDED(component->GetPathName(&path)))
			info->set_file(ToUtf8(path));
		ATL::CComBSTR configuration;
		if (SUCCEEDED(component->get_ReferencedConfiguration(&configuration)))
			info->set_configuration(ToUtf8(configuration));

		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(component->IsFixed(&flag)))
			info->set_fixed(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(component->IsHidden(VARIANT_FALSE, &flag)))
			info->set_hidden(flag != VARIANT_FALSE);
		long state = swComponentFullyResolved;
		if (SUCCEEDED(component->GetSuppression2(&state)))
			info->set_state(ToComponentState(state));
		info->set_subassembly(IsSubassembly(component));

		ATL::CComPtr<IComponent2> parent;
		if (SUCCEEDED(component->GetParent(&parent)) && parent)
			info->set_parent_persist_id(ids.Get(parent));

		ATL::CComVariant box;
		if (SUCCEEDED(component->GetBox(VARIANT_FALSE, VARIANT_FALSE, &box)))
			SetBox(info->mutable_box(), comvariant::ToDoubles(box));

		if (state != swComponentSuppressed)
			DescribePlacement(component, math, info);
	}

	ATL::CComPtr<IComponent2> ParentOf(IComponent2* component)
	{
		ATL::CComPtr<IComponent2> parent;
		component->GetParent(&parent);
		return parent;
	}

	bool ReadPose(IComponent2* component, SavedPose& out)
	{
		out.component = component;
		out.xform.Release();
		ATL::CComVariant data;
		if (FAILED(component->get_Transform2(&out.xform)) || !out.xform || FAILED(out.xform->get_ArrayData(&data)))
			return false;
		out.data = comvariant::ToDoubles(data);
		return true;
	}

	bool SamePose(const std::vector<double>& a, const std::vector<double>& b, double tolerance)
	{
		if (a.size() != b.size())
			return false;
		for (size_t i = 0; i < a.size(); ++i)
			if (std::abs(a[i] - b[i]) > tolerance)
				return false;
		return true;
	}

	std::vector<SavedPose> TopLevelPoses(IAssemblyDoc* assembly)
	{
		std::vector<SavedPose> saved;
		for (const auto& c : AllComponents(assembly))
		{
			if (ParentOf(c))
				continue;
			SavedPose pose;
			if (ReadPose(c, pose))
				saved.push_back(std::move(pose));
		}
		return saved;
	}

	std::vector<const SavedPose*> MovedSince(const std::vector<SavedPose>& saved)
	{
		std::vector<const SavedPose*> moved;
		for (const auto& pose : saved)
		{
			SavedPose now;
			if (ReadPose(pose.component, now) && !SamePose(now.data, pose.data, 1e-9))
				moved.push_back(&pose);
		}
		return moved;
	}

	std::vector<std::string> RestorePoses(IModelDoc2* doc, const std::vector<SavedPose>& saved)
	{
		for (const SavedPose* pose : MovedSince(saved))
			pose->component->put_Transform2(pose->xform);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		std::vector<std::string> notBack;
		for (const auto& pose : saved)
		{
			SavedPose now;
			if (!ReadPose(pose.component, now) || !SamePose(now.data, pose.data, 1e-9))
				notBack.push_back(ComponentName(pose.component));
		}
		return notBack;
	}

	sld_mcp::v1::MateType ToMateType(long type)
	{
		switch (type)
		{
		case swMateCOINCIDENT:    return sld_mcp::v1::MATE_TYPE_COINCIDENT;
		case swMateCONCENTRIC:    return sld_mcp::v1::MATE_TYPE_CONCENTRIC;
		case swMatePERPENDICULAR: return sld_mcp::v1::MATE_TYPE_PERPENDICULAR;
		case swMatePARALLEL:      return sld_mcp::v1::MATE_TYPE_PARALLEL;
		case swMateTANGENT:       return sld_mcp::v1::MATE_TYPE_TANGENT;
		case swMateDISTANCE:      return sld_mcp::v1::MATE_TYPE_DISTANCE;
		case swMateANGLE:         return sld_mcp::v1::MATE_TYPE_ANGLE;
		case swMateLOCK:          return sld_mcp::v1::MATE_TYPE_LOCK;
		default:                  return sld_mcp::v1::MATE_TYPE_OTHER;
		}
	}

	bool ToSwMateType(sld_mcp::v1::MateType type, long& out)
	{
		switch (type)
		{
		case sld_mcp::v1::MATE_TYPE_COINCIDENT:    out = swMateCOINCIDENT; return true;
		case sld_mcp::v1::MATE_TYPE_CONCENTRIC:    out = swMateCONCENTRIC; return true;
		case sld_mcp::v1::MATE_TYPE_PERPENDICULAR: out = swMatePERPENDICULAR; return true;
		case sld_mcp::v1::MATE_TYPE_PARALLEL:      out = swMatePARALLEL; return true;
		case sld_mcp::v1::MATE_TYPE_TANGENT:       out = swMateTANGENT; return true;
		case sld_mcp::v1::MATE_TYPE_DISTANCE:      out = swMateDISTANCE; return true;
		case sld_mcp::v1::MATE_TYPE_ANGLE:         out = swMateANGLE; return true;
		case sld_mcp::v1::MATE_TYPE_LOCK:          out = swMateLOCK; return true;
		default:                                   return false;
		}
	}

	const char* MateTypeName(long type)
	{
		switch (type)
		{
		case swMateCOINCIDENT:    return "coincident";
		case swMateCONCENTRIC:    return "concentric";
		case swMatePERPENDICULAR: return "perpendicular";
		case swMatePARALLEL:      return "parallel";
		case swMateTANGENT:       return "tangent";
		case swMateDISTANCE:      return "distance";
		case swMateANGLE:         return "angle";
		case swMateLOCK:          return "lock";
		default:                  return "mate";
		}
	}

	sld_mcp::v1::MateAlignment ToMateAlignment(long align)
	{
		switch (align)
		{
		case swMateAlignALIGNED:      return sld_mcp::v1::MATE_ALIGNMENT_ALIGNED;
		case swMateAlignANTI_ALIGNED: return sld_mcp::v1::MATE_ALIGNMENT_ANTI_ALIGNED;
		case swMateAlignCLOSEST:      return sld_mcp::v1::MATE_ALIGNMENT_CLOSEST;
		default:                      return sld_mcp::v1::MATE_ALIGNMENT_UNSPECIFIED;
		}
	}

	long ToSwMateAlign(sld_mcp::v1::MateAlignment align)
	{
		switch (align)
		{
		case sld_mcp::v1::MATE_ALIGNMENT_ALIGNED:      return swMateAlignALIGNED;
		case sld_mcp::v1::MATE_ALIGNMENT_ANTI_ALIGNED: return swMateAlignANTI_ALIGNED;
		default:                                       return swMateAlignCLOSEST;
		}
	}

	void CollectMates(IFeature* parent, int depth, std::vector<ATL::CComPtr<IFeature>>& mates,
		std::set<std::string>& seen)
	{
		ATL::CComPtr<IFeature> sub;
		if (FAILED(parent->IGetFirstSubFeature(&sub)))
			return;
		while (sub && mates.size() < kMaxMates)
		{
			ATL::CComPtr<IDispatch> specific;
			if (SUCCEEDED(sub->GetSpecificFeature2(&specific)) && specific &&
				ATL::CComQIPtr<IMate2>(specific.p))
			{
				if (seen.insert(FeatureName(sub)).second)
					mates.push_back(sub);
			}
			else if (depth < 3)
				CollectMates(sub, depth + 1, mates, seen);

			ATL::CComPtr<IFeature> next;
			if (FAILED(sub->IGetNextSubFeature(&next)))
				break;
			sub = next;
		}
	}

	std::vector<ATL::CComPtr<IFeature>> MateFeatures(IModelDoc2* doc)
	{
		std::vector<ATL::CComPtr<IFeature>> mates;
		std::set<std::string> seen;
		ATL::CComPtr<IFeature> feature;
		if (FAILED(doc->IFirstFeature(&feature)))
			return mates;
		while (feature)
		{
			ATL::CComBSTR typeName;
			if (SUCCEEDED(feature->GetTypeName2(&typeName)) && typeName == L"MateGroup")
				CollectMates(feature, 0, mates, seen);
			ATL::CComPtr<IFeature> next;
			if (FAILED(feature->IGetNextFeature(&next)))
				break;
			feature = next;
		}
		return mates;
	}

	std::set<std::string> MateNames(IModelDoc2* doc)
	{
		std::set<std::string> names;
		for (const auto& mate : MateFeatures(doc))
			names.insert(FeatureName(mate));
		return names;
	}

	bool MateLimitsOf(IFeature* feature, long type, double& minimum, double& maximum)
	{
		if (type != swMateDISTANCE && type != swMateANGLE)
			return false;
		ATL::CComPtr<IDispatch> definition;
		ATL::CComVariant advanced, low, high;
		if (FAILED(feature->GetDefinition(&definition)) || !definition ||
			!InvokeByName(definition, L"IsAdvancedMate", advanced) || FAILED(advanced.ChangeType(VT_BOOL)) ||
			advanced.boolVal == VARIANT_FALSE)
			return false;
		const bool angle = type == swMateANGLE;
		if (!InvokeByName(definition, angle ? L"MinimumAngle" : L"MinimumDistance", low) ||
			!InvokeByName(definition, angle ? L"MaximumAngle" : L"MaximumDistance", high) ||
			FAILED(low.ChangeType(VT_R8)) || FAILED(high.ChangeType(VT_R8)))
			return false;
		// SolidWorks leaves rounding noise on them (a 0 minimum read back as
		// -6.9e-15 after a few value changes, seen 2026-10-25).
		auto clean = [](double v) { return std::abs(v) < 1e-12 ? 0.0 : v; };
		minimum = clean(low.dblVal);
		maximum = clean(high.dblVal);
		return std::isfinite(minimum) && std::isfinite(maximum);
	}

	std::string MateValueText(long type, double value)
	{
		char text[64];
		if (type == swMateANGLE)
			std::snprintf(text, sizeof(text), "%g deg", Degrees(value));
		else
			std::snprintf(text, sizeof(text), "%g mm", value * 1000.0);
		return text;
	}

	grpc::Status CheckMateLimits(long type, double minimum, double maximum, double value)
	{
		if (type != swMateDISTANCE && type != swMateANGLE)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"only distance and angle mates take limits");
		if (!std::isfinite(minimum) || !std::isfinite(maximum) || minimum > maximum)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the limits must be two numbers, the minimum no bigger than the maximum");
		if (type == swMateDISTANCE && minimum < 0.0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a distance limit can't be below 0 (flip measures to the other side)");
		if (type == swMateANGLE && (minimum < 0.0 || maximum > M_PI + 1e-9))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "angle limits must be 0 to 180 degrees");
		const double tolerance = type == swMateANGLE ? 1e-9 : 1e-10;
		if (value < minimum - tolerance || value > maximum + tolerance)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the mate's value " + MateValueText(type, value) + " isn't within its limits " +
				MateValueText(type, minimum) + " .. " + MateValueText(type, maximum));
		return grpc::Status::OK;
	}

	void DescribeMate(IFeature* feature, PersistIds& ids, sld_mcp::v1::MateInfo* info)
	{
		info->set_persist_id(ids.Get(feature));
		info->set_name(FeatureName(feature));
		ATL::CComBSTR typeName;
		if (SUCCEEDED(feature->GetTypeName2(&typeName)))
			info->set_type_name(ToUtf8(typeName));
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(feature->IsSuppressed(&flag)))
			info->set_suppressed(flag != VARIANT_FALSE);
		VARIANT_BOOL warning = VARIANT_FALSE;
		long code = swFeatureErrorNone;
		if (SUCCEEDED(feature->GetErrorCode2(&warning, &code)))
		{
			info->set_error_code(static_cast<int32_t>(code));
			info->set_error_is_warning(code != swFeatureErrorNone && warning != VARIANT_FALSE);
		}

		ATL::CComPtr<IDispatch> specific;
		if (SUCCEEDED(feature->GetSpecificFeature2(&specific)) && specific)
		{
			if (ATL::CComQIPtr<IMate2> mate(specific.p); mate)
			{
				long value = 0;
				if (SUCCEEDED(mate->get_Type(&value)))
				{
					info->set_type(ToMateType(value));
					double minimum = 0.0, maximum = 0.0;
					if (MateLimitsOf(feature, value, minimum, maximum))
					{
						info->mutable_limits()->set_min(minimum);
						info->mutable_limits()->set_max(maximum);
					}
				}
				if (SUCCEEDED(mate->get_Alignment(&value)))
					info->set_alignment(ToMateAlignment(value));
				flag = VARIANT_FALSE;
				if (SUCCEEDED(mate->get_Flipped(&flag)))
					info->set_flipped(flag != VARIANT_FALSE);

				long count = 0;
				std::set<std::string> components;
				if (SUCCEEDED(mate->GetMateEntityCount(&count)))
					for (long i = 0; i < count; ++i)
					{
						ATL::CComPtr<IMateEntity2> entity;
						if (FAILED(mate->MateEntity(i, &entity)) || !entity)
							continue;
						ATL::CComPtr<IComponent2> component;
						entity->get_ReferenceComponent(&component);
						const std::string pid = component ? ids.Get(component) : std::string();
						if (!components.insert(pid).second)
							continue;
						info->add_component_persist_ids(pid);
						info->add_component_names(ComponentName(component));
					}
			}
		}

		// Distance and angle mates carry their value as a dimension.
		for (const auto& display : FeatureDimensions(feature))
			if (DescribeDimension(display, info->mutable_dimension()))
				break;
	}

	std::string MateErrorText(long code)
	{
		switch (code)
		{
		case swFeatureErrorMateOverdefined:
			return "it over-defines the assembly: it conflicts with existing mates or fixed components";
		case swFeatureErrorMateIlldefined:
			return "it can't be solved (ill-defined) from the components' positions";
		case swFeatureErrorMateInvalidEdge:
		case swFeatureErrorMateInvalidFace:
		case swFeatureErrorMateInvalidEntity:
		case swFeatureErrorMateEntityNotLinear:
			return "that kind of mate doesn't work with those entities";
		case swFeatureErrorMateUnknownTangent:
			return "SolidWorks couldn't work out the tangency";
		case swFeatureErrorMateDanglingGeometry:
		case swFeatureErrorMateBroken:
			return "it refers to geometry that's missing";
		default:
			return "SolidWorks reported error " + std::to_string(code);
		}
	}

	std::string AddMateErrorText(long error, long type)
	{
		const std::string kind = MateTypeName(type);
		switch (error)
		{
		case swAddMateError_IncorrectSelections:
			return "SolidWorks can't make a " + kind + " mate between those two entities -- e.g. "
				"concentric needs round faces/edges or axes, tangent needs a curved face, lock needs "
				"two components";
		case swAddMateError_IncorrectAlignment:
			return "that alignment isn't possible for these entities -- try alignment \"closest\"";
		case swAddMateError_OverDefinedAssembly:
			return "the " + kind + " mate would over-define the assembly: it conflicts with existing "
				"mates or fixed components";
		case swAddMateError_IncorrectMateType:
			return "SolidWorks rejected the mate type for these entities";
		default:
			return "SolidWorks couldn't add the " + kind + " mate (error " + std::to_string(error) + ")";
		}
	}

	void DeleteFeature(IModelDoc2* doc, IFeature* feature)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (feature && SUCCEEDED(feature->Select2(VARIANT_FALSE, 0, &selected)) && selected)
			doc->EditDelete();
		doc->ClearSelection2(VARIANT_TRUE);
	}

	ATL::CComPtr<IComponent2> ComponentOfFeature(IAssemblyDoc* assembly, IFeature* feature, PersistIds& ids)
	{
		if (ATL::CComPtr<IComponent2> component = ComponentOf(feature); component)
			return component;
		const std::string pid = ids.Get(feature);
		const ATL::CComBSTR name = FromUtf8(FeatureName(feature));
		if (pid.empty())
			return {};
		for (const auto& component : AllComponents(assembly))
		{
			ATL::CComPtr<IFeature> candidate;
			if (SUCCEEDED(component->FeatureByName(name, &candidate)) && candidate && ids.Get(candidate) == pid)
				return component;
		}
		return {};
	}

	grpc::Status ResolveMateRef(IModelDocExtension* ext, IAssemblyDoc* assembly, PersistIds& ids,
		const std::string& persistId, long mateType, MateRef& ref)
	{
		if (grpc::Status status = ResolvePersistId(ext, persistId, ref.obj); !status.ok())
			return status;
		const bool lock = mateType == swMateLOCK;

		if (ATL::CComQIPtr<IComponent2> component(ref.obj.p); component)
		{
			if (!lock)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + ComponentName(component) + "\" is a whole component; only lock mates take "
					"components -- give a face, edge or reference plane/axis");
			ref.component = component;
			ref.isComponent = true;
			ref.what = ComponentName(component);
			return grpc::Status::OK;
		}
		if (lock)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a lock mate takes two components, not faces or other geometry");

		if (ATL::CComQIPtr<IFace2>(ref.obj.p))
			ref.what = "face";
		else if (ATL::CComQIPtr<IEdge>(ref.obj.p))
			ref.what = "edge";
		else if (ATL::CComQIPtr<IVertex>(ref.obj.p))
			ref.what = "vertex";
		else if (ATL::CComQIPtr<IFeature> feature(ref.obj.p); feature)
		{
			ATL::CComBSTR typeName;
			feature->GetTypeName2(&typeName);
			if (typeName != L"RefPlane" && typeName != L"RefAxis" && typeName != L"RefPoint" &&
				typeName != L"CoordSys")
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + FeatureName(feature) + "\" (" + ToUtf8(typeName) + ") can't be mated -- give "
					"a face, an edge, or a reference plane / axis / point");
			ref.isFeature = true;
			ref.what = FeatureName(feature);
		}
		else
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that id isn't something a mate can use -- give a face, an edge, or a reference "
				"plane / axis / point");

		if (ref.isFeature)
		{
			ATL::CComQIPtr<IFeature> feature(ref.obj.p);
			ref.component = ComponentOfFeature(assembly, feature, ids);
		}
		else
			ref.component = ComponentOf(ref.obj);
		if (!ref.component && !ref.isFeature)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"that " + ref.what + " isn't on a component of the active assembly");
		if (ref.component && IsSuppressed(ref.component))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"\"" + ComponentName(ref.component) + "\" is suppressed -- unsuppress it first");
		return grpc::Status::OK;
	}

	bool SelectMateRef(const MateRef& ref, ISelectData* data)
	{
		VARIANT_BOOL selected = VARIANT_FALSE;
		if (ref.isComponent)
			return SUCCEEDED(ref.component->Select4(VARIANT_TRUE, data, VARIANT_FALSE, &selected)) && selected;
		if (ref.isFeature)
		{
			ATL::CComQIPtr<IFeature> feature(ref.obj.p);
			return feature && SUCCEEDED(feature->Select2(VARIANT_TRUE, 1, &selected)) && selected;
		}
		ATL::CComQIPtr<IEntity> entity(ref.obj.p);
		return entity && SUCCEEDED(entity->Select4(VARIANT_TRUE, data, &selected)) && selected;
	}

	std::string MateEntityKind(IDispatch* reference)
	{
		if (ATL::CComQIPtr<IFace2>(reference))
			return "face";
		if (ATL::CComQIPtr<IEdge>(reference))
			return "edge";
		if (ATL::CComQIPtr<IVertex>(reference))
			return "vertex";
		if (ATL::CComQIPtr<IComponent2>(reference))
			return "component";
		if (ATL::CComQIPtr<IRefPlane>(reference))
			return "plane";
		if (ATL::CComQIPtr<IRefAxis>(reference))
			return "axis";
		if (ATL::CComQIPtr<IRefPoint>(reference))
			return "point";
		if (ATL::CComQIPtr<IFeature> feature(reference); feature)
		{
			const std::string type = FeatureTypeName(feature);
			if (type == "RefPlane")
				return "plane";
			if (type == "RefAxis")
				return "axis";
			if (type == "RefPoint")
				return "point";
			if (type == "CoordSys")
				return "coordinate system";
		}
		return "other";
	}

	bool DiagnoseMate(IFeature* feature, PersistIds& ids,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::MateEntityState>* out)
	{
		ATL::CComPtr<IDispatch> specific;
		if (FAILED(feature->GetSpecificFeature2(&specific)) || !specific)
			return false;
		ATL::CComQIPtr<IMate2> mate(specific.p);
		long count = 0;
		if (!mate || FAILED(mate->GetMateEntityCount(&count)))
			return false;
		bool missing = false;
		for (long i = 0; i < count && i < 16; ++i)
		{
			sld_mcp::v1::MateEntityState* state = out->Add();
			ATL::CComPtr<IMateEntity2> entity;
			if (FAILED(mate->MateEntity(i, &entity)) || !entity)
			{
				state->set_missing(true);
				missing = true;
				continue;
			}
			long refType = swMateEntity2ReferenceType_UNKNOWN;
			if (SUCCEEDED(entity->get_ReferenceType2(&refType)))
				state->set_reference_type(static_cast<int32_t>(refType));
			ATL::CComPtr<IComponent2> component;
			if (SUCCEEDED(entity->get_ReferenceComponent(&component)) && component)
			{
				state->set_component_persist_id(ids.Get(component));
				state->set_component_name(ComponentName(component));
			}
			ATL::CComPtr<IDispatch> reference;
			if (FAILED(entity->get_Reference(&reference)) || !reference)
			{
				state->set_missing(true);
				missing = true;
				continue;
			}
			state->set_kind(MateEntityKind(reference));
			state->set_persist_id(ids.Get(reference));
		}
		return missing;
	}

	grpc::Status CreateMate(IModelDoc2* doc, IAssemblyDoc* assembly, const MateSettings& s,
		ATL::CComPtr<IFeature>& created)
	{
		created.Release();
		ATL::CComPtr<ISelectionMgr> selectionManager;
		HRESULT hr = doc->get_ISelectionManager(&selectionManager);
		if (FAILED(hr) || !selectionManager)
			return ComError("IModelDoc2::ISelectionManager", hr);
		ATL::CComPtr<ISelectData> selectData;
		hr = selectionManager->CreateSelectData(&selectData);
		if (FAILED(hr) || !selectData)
			return ComError("ISelectionMgr::CreateSelectData", hr);
		selectData->put_Mark(1);

		const std::set<std::string> matesBefore = MateNames(doc);
		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t i = 0; i < s.refs.size(); ++i)
			if (!SelectMateRef(s.refs[i], selectData))
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::INTERNAL,
					"SolidWorks wouldn't select entity " + std::to_string(i + 1) + " (" + s.refs[i].what + ")");
			}

		const long alignment = s.type == swMateLOCK ? swMateAlignCLOSEST : s.alignment;
		// Value, upper limit, lower limit; limits equal to the value: not a
		// limit mate.
		const bool distanceLimits = s.limited && s.type == swMateDISTANCE;
		const bool angleLimits = s.limited && s.type == swMateANGLE;
		long addError = swAddMateError_ErrorUknown;
		ATL::CComPtr<IMate2> mate;
		hr = assembly->AddMate5(s.type, alignment, s.flip ? VARIANT_TRUE : VARIANT_FALSE,
			s.distance, distanceLimits ? s.maximum : s.distance, distanceLimits ? s.minimum : s.distance,
			1.0, 1.0,                            // gear ratio, unused
			s.angle, angleLimits ? s.maximum : s.angle, angleLimits ? s.minimum : s.angle,
			VARIANT_FALSE,                       // a real mate, not just for positioning
			s.type == swMateCONCENTRIC && s.lockRotation ? VARIANT_TRUE : VARIANT_FALSE,
			0,                                   // width mate option, unused
			&addError, &mate);
		doc->ClearSelection2(VARIANT_TRUE);

		ATL::CComPtr<IFeature> mateFeature;
		for (const auto& feature : MateFeatures(doc))
			if (!matesBefore.count(FeatureName(feature)))
			{
				mateFeature = feature;
				break;
			}
		auto removeMate = [&]() {
			if (mateFeature)
			{
				DeleteFeature(doc, mateFeature);
				VARIANT_BOOL rebuilt = VARIANT_FALSE;
				doc->EditRebuild3(&rebuilt);
			}
		};

		if (FAILED(hr))
		{
			removeMate();
			return ComError("IAssemblyDoc::AddMate5", hr);
		}
		if (!mate || addError != swAddMateError_NoError)
		{
			removeMate();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, AddMateErrorText(addError, s.type));
		}
		if (!mateFeature)
			return grpc::Status(grpc::StatusCode::INTERNAL,
				"SolidWorks reported the mate as added, but there's no new mate in the Mates folder");

		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		hr = doc->EditRebuild3(&rebuilt);
		if (FAILED(hr))
			return ComError("IModelDoc2::EditRebuild3", hr);

		VARIANT_BOOL warning = VARIANT_FALSE;
		long errorCode = swFeatureErrorNone;
		mateFeature->GetErrorCode2(&warning, &errorCode);
		if (errorCode != swFeatureErrorNone && !warning)
		{
			removeMate();
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				std::string("SolidWorks added the ") + MateTypeName(s.type) + " mate but couldn't solve it -- " +
				MateErrorText(errorCode) + " -- so it was removed again");
		}
		created = mateFeature;
		return grpc::Status::OK;
	}

	InterferenceSession::InterferenceSession(IInterferenceDetectionMgr* mgr) : m_mgr(mgr)
	{
		m_mgr->get_TreatCoincidenceAsInterference(&m_coincidence);
		m_mgr->get_TreatSubAssembliesAsComponents(&m_subassemblies);
		m_mgr->get_IncludeMultibodyPartInterferences(&m_multibody);
		m_mgr->get_IgnoreHiddenBodies(&m_ignoreHidden);
	}

	InterferenceSession::~InterferenceSession()
	{
		m_mgr->put_TreatCoincidenceAsInterference(m_coincidence);
		m_mgr->put_TreatSubAssembliesAsComponents(m_subassemblies);
		m_mgr->put_IncludeMultibodyPartInterferences(m_multibody);
		m_mgr->put_IgnoreHiddenBodies(m_ignoreHidden);
		m_mgr->Done();
	}

	void InterferenceSession::Apply(bool coincident, bool subassemblies, bool multibody, bool includeHidden)
	{
		auto b = [](bool v) { return v ? VARIANT_TRUE : VARIANT_FALSE; };
		m_mgr->put_TreatCoincidenceAsInterference(b(coincident));
		m_mgr->put_TreatSubAssembliesAsComponents(b(subassemblies));
		m_mgr->put_IncludeMultibodyPartInterferences(b(multibody));
		m_mgr->put_IgnoreHiddenBodies(b(!includeHidden));
	}

	grpc::Status InterferenceSession::Check(PersistIds& ids, ComponentList* components, int limit,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::Interference>* out, bool& truncated)
	{
		truncated = false;
		ATL::CComVariant interferences;
		HRESULT hr = m_mgr->GetInterferences(&interferences);
		if (FAILED(hr))
			return ComError("IInterferenceDetectionMgr::GetInterferences", hr);
		for (const auto& disp : comvariant::ToDispatches(interferences))
		{
			ATL::CComQIPtr<IInterference> interference(disp.p);
			if (!interference)
				continue;
			if (out->size() >= limit)
			{
				truncated = true;
				break;
			}
			sld_mcp::v1::Interference* item = out->Add();
			ATL::CComVariant list;
			if (SUCCEEDED(interference->get_Components(&list)))
				for (const auto& c : comvariant::ToDispatches(list))
					if (ATL::CComQIPtr<IComponent2> component(c.p); component)
					{
						item->add_component_persist_ids(components ? components->Add(component) : ids.Get(component));
						item->add_component_names(ComponentName(component));
					}
			double volume = 0.0;
			if (SUCCEEDED(interference->get_Volume(&volume)) && std::isfinite(volume))
				item->set_volume(volume);
			VARIANT_BOOL flag = VARIANT_FALSE;
			if (SUCCEEDED(interference->get_IsPossibleInterference(&flag)))
				item->set_possible(flag != VARIANT_FALSE);
			flag = VARIANT_FALSE;
			if (SUCCEEDED(interference->get_IsFastener(&flag)))
				item->set_fastener(flag != VARIANT_FALSE);
			flag = VARIANT_FALSE;
			if (SUCCEEDED(interference->get_Ignore(&flag)))
				item->set_ignored(flag != VARIANT_FALSE);
			ATL::CComPtr<IDispatch> bodyDisp;
			if (SUCCEEDED(interference->GetInterferenceBody(&bodyDisp)) && bodyDisp)
				if (ATL::CComQIPtr<IBody2> body(bodyDisp.p); body)
				{
					ATL::CComVariant box;
					if (SUCCEEDED(body->GetBodyBox(&box)))
						if (const std::vector<double> b = comvariant::ToDoubles(box); b.size() == 6)
							SetBox(item->mutable_box(), b);
				}
		}
		return grpc::Status::OK;
	}

	grpc::Status StartCollisionChecks(IAssemblyDoc* assembly, std::unique_ptr<InterferenceSession>& out)
	{
		ATL::CComPtr<IInterferenceDetectionMgr> mgr;
		HRESULT hr = assembly->get_InterferenceDetectionManager(&mgr);
		if (FAILED(hr))
			return ComError("IAssemblyDoc::InterferenceDetectionManager", hr);
		if (!mgr)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks has no interference detection for this assembly");
		out = std::make_unique<InterferenceSession>(mgr);
		out->Apply(false, false, false, false);
		return grpc::Status::OK;
	}

	std::string InterferenceKey(const sld_mcp::v1::Interference& interference)
	{
		std::vector<std::string> parts;
		for (int i = 0; i < interference.component_persist_ids_size(); ++i)
			parts.push_back(interference.component_persist_ids(i).empty() && i < interference.component_names_size()
				? "name:" + interference.component_names(i)
				: interference.component_persist_ids(i));
		std::sort(parts.begin(), parts.end());
		std::string key;
		for (const auto& part : parts)
			key += std::to_string(part.size()) + ":" + part;
		return key;
	}

	bool HasNewInterference(const std::set<std::string>& before,
		const google::protobuf::RepeatedPtrField<sld_mcp::v1::Interference>& now)
	{
		for (const auto& interference : now)
			if (!before.count(InterferenceKey(interference)))
				return true;
		return false;
	}
}
