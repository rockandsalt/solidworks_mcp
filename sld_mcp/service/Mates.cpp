// Mates.cpp : AddMate, SweepMate.
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include <chrono>

using namespace sld_mcp::service;

grpc::Status SolidWorksService::AddMate(grpc::ServerContext* context,
	const sld_mcp::v1::AddMateRequest* request,
	sld_mcp::v1::AddMateResponse* response)
{
	// Validate before touching SolidWorks.
	long mateType = swMateCOINCIDENT;
	if (!ToSwMateType(request->type(), mateType))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give a mate type: coincident, concentric, perpendicular, parallel, tangent, distance, "
			"angle or lock");
	if (request->entity_persist_ids_size() != 2)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a mate takes exactly two entities");
	if (request->entity_persist_ids(0) == request->entity_persist_ids(1))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the two entities are the same one");
	double distance = 0.0, angle = 0.0;
	if (mateType == swMateDISTANCE)
	{
		distance = request->distance();
		if (!std::isfinite(distance) || distance < 0.0)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the distance must be 0 or more");
	}
	if (mateType == swMateANGLE)
	{
		angle = request->angle();
		if (!std::isfinite(angle) || angle < 0.0 || angle > M_PI + 1e-9)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the angle must be 0 to 180 degrees");
	}
	if (request->has_limits())
	{
		if (grpc::Status status = CheckMateLimits(mateType, request->limits().min(), request->limits().max(),
				mateType == swMateANGLE ? angle : distance); !status.ok())
			return status;
	}
	const long alignment = mateType == swMateLOCK ? swMateAlignCLOSEST : ToSwMateAlign(request->alignment());
	const VARIANT_BOOL flip = request->flip() ? VARIANT_TRUE : VARIANT_FALSE;
	const VARIANT_BOOL lockRotation =
		mateType == swMateCONCENTRIC && request->lock_rotation() ? VARIANT_TRUE : VARIANT_FALSE;

	return m_executor.Execute(context, "AddMate", ExecutionPolicy::CompleteOperation,
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

		MateRef refs[2];
		for (int i = 0; i < 2; ++i)
		{
			grpc::Status status = ResolveMateRef(ext, assembly, ids, request->entity_persist_ids(i), mateType, refs[i]);
			if (!status.ok())
				return grpc::Status(status.error_code(),
					"entity " + std::to_string(i + 1) + ": " + status.error_message());
		}
		const std::string componentIds[2] = { refs[0].component ? ids.Get(refs[0].component) : std::string(),
			refs[1].component ? ids.Get(refs[1].component) : std::string() };
		// (Two of the assembly's own planes, or planes whose component couldn't
		// be worked out, are left for SolidWorks to judge.)
		if (!componentIds[0].empty() && componentIds[0] == componentIds[1])
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"both entities are on \"" + ComponentName(refs[0].component) + "\" -- a mate joins two "
				"different components");

		MateSettings settings;
		settings.type = mateType;
		settings.alignment = alignment;
		settings.flip = flip != VARIANT_FALSE;
		settings.distance = distance;
		settings.angle = angle;
		settings.lockRotation = lockRotation != VARIANT_FALSE;
		settings.limited = request->has_limits();
		settings.minimum = request->limits().min();
		settings.maximum = request->limits().max();
		settings.refs.assign(std::begin(refs), std::end(refs));
		ATL::CComPtr<IFeature> mateFeature;
		if (grpc::Status status = CreateMate(doc, assembly, settings, mateFeature); !status.ok())
			return status;

		DescribeMate(mateFeature, ids, response->mutable_mate());
		ComponentList components(ids, math, response->mutable_components());
		for (const auto& ref : refs)
			components.Add(ref.component);
		return grpc::Status::OK;
	});
}

namespace
{
	std::string SetValueError(long result, long type, double value)
	{
		switch (result)
		{
		case swSetValue_InvalidValue:
			return "SolidWorks wouldn't take " + MateValueText(type, value) + " (beyond the mate's limits, or a "
				"position its mates can't reach)";
		case swSetValue_DrivenDimension:
			return "its dimension is driven (a reference dimension)";
		default:
			return "SolidWorks couldn't set " + MateValueText(type, value) + " (error " + std::to_string(result) + ")";
		}
	}
}

grpc::Status SolidWorksService::SweepMate(grpc::ServerContext* context,
	const sld_mcp::v1::SweepMateRequest* request,
	sld_mcp::v1::SweepMateResponse* response)
{
	constexpr int kMaxSteps = 360;
	constexpr int kMaxInterferences = 200;
	if (request->steps() < 1 || request->steps() > kMaxSteps)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "steps must be 1.." + std::to_string(kMaxSteps));
	if ((request->has_from() && !std::isfinite(request->from())) || (request->has_to() && !std::isfinite(request->to())))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the range isn't finite numbers");
	if (!std::isfinite(request->time_budget()) || request->time_budget() < 0.0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "time_budget must be 0 or more seconds");

	return m_executor.Execute(context, "SweepMate", ExecutionPolicy::CompleteOperation,
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
		ATL::CComPtr<IDispatch> specific;
		feature->GetSpecificFeature2(&specific);
		ATL::CComQIPtr<IMate2> mate(specific.p);
		if (!mate)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" is a " + tree[index].type + " feature, not a mate");
		long type = swMateCOINCIDENT;
		mate->get_Type(&type);
		if (type != swMateDISTANCE && type != swMateANGLE)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" is a " + MateTypeName(type) + " mate -- only distance and angle mates (limit or "
				"not) have a value to sweep");
		const bool angular = type == swMateANGLE;
		if ((request->has_from() || request->has_to()) && request->angular() != angular)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" is " + (angular ? "an angle mate: give the range in degrees"
				                                  : "a distance mate: give the range in millimeters"));
		DescribeMate(feature, ids, response->mutable_mate());

		ATL::CComPtr<IDimension> dim;
		for (const auto& display : FeatureDimensions(feature))
			if (SUCCEEDED(display->GetDimension2(0, &dim)) && dim)
				break;
		if (!dim)
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks gave no dimension for \"" + name + "\"");
		const double current = response->mate().dimension().value();

		double minimum = 0.0, maximum = 0.0;
		const bool limited = MateLimitsOf(feature, type, minimum, maximum);
		if (!request->has_to() && !limited)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + name + "\" isn't a limit mate, so it has no range of its own -- give the value to sweep to "
				"(it's at " + MateValueText(type, current) + " now)");
		const double from = request->has_from() ? request->from() : limited ? minimum : current;
		const double to = request->has_to() ? request->to() : maximum;
		for (double v : { from, to })
		{
			if (v < 0.0 || (angular && v > M_PI + 1e-9))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					angular ? "an angle mate's value is 0 to 180 degrees" : "a distance mate's value is 0 or more");
			if (limited && (v < minimum - 1e-10 || v > maximum + 1e-10))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"\"" + name + "\" only goes from " + MateValueText(type, minimum) + " to " +
					MateValueText(type, maximum) + " -- sweep within its limits (or widen them with edit_mate)");
		}
		if (std::abs(to - from) < 1e-12)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"nothing to sweep: the range starts and ends at " + MateValueText(type, from));
		response->set_from(from);
		response->set_to(to);

		const std::vector<SavedPose> saved = TopLevelPoses(assembly);
		const auto errorsBefore = FeatureErrors(tree);
		PreferenceToggles toggles(swApp);
		toggles.Set(swShowErrorsEveryRebuild, false);
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		auto setValue = [&](double v, long& result) {
			result = swSetValue_Failure;
			HRESULT hr = dim->SetSystemValue3(v, swSetValue_UseCurrentSetting, ATL::CComVariant(), &result);
			if (FAILED(hr))
				return false;
			if (result == swSetValue_Successful)
				doc->EditRebuild3(&rebuilt);
			return result == swSetValue_Successful;
		};

		// The values in turn, checked for interference after each rebuild; the
		// sweep ends at the last value that worked.
		double last = current;
		bool stoppedEarly = false;
		{
			QuietSolves quiet(swApp, doc);
			std::unique_ptr<InterferenceSession> session;
			if (grpc::Status status = StartCollisionChecks(assembly, session); !status.ok())
				return status;
			bool truncated = false;
			if (grpc::Status status = session->Check(ids, nullptr, kMaxInterferences,
					response->mutable_start_interferences(), truncated); !status.ok())
				return status;
			std::set<std::string> overlapping;
			for (const auto& interference : response->start_interferences())
				overlapping.insert(InterferenceKey(interference));

			const auto started = std::chrono::steady_clock::now();
			const int steps = request->steps();
			for (int k = 0; k <= steps; ++k)
			{
				if (k > 0 && request->time_budget() > 0.0 &&
					std::chrono::duration<double>(std::chrono::steady_clock::now() - started).count() >
						request->time_budget())
				{
					response->set_out_of_time(true);
					stoppedEarly = true;
					break;
				}
				const double v = k == steps ? to : from + (to - from) * k / steps;
				long result = swSetValue_Failure;
				if (!setValue(v, result))
				{
					response->set_refused_step(k + 1);
					response->set_failure(SetValueError(result, type, v));
					stoppedEarly = true;
					break;
				}
				VARIANT_BOOL warning = VARIANT_FALSE;
				long errorCode = swFeatureErrorNone;
				feature->GetErrorCode2(&warning, &errorCode);
				if (errorCode != swFeatureErrorNone && !warning)
				{
					response->set_refused_step(k + 1);
					response->set_failure("SolidWorks couldn't solve the mates at " + MateValueText(type, v) + " -- " +
						MateErrorText(errorCode));
					stoppedEarly = true;
					break;
				}

				sld_mcp::v1::MotionStep* step = response->add_steps();
				step->set_step(k + 1);
				step->set_value(v);
				if (grpc::Status status = session->Check(ids, nullptr, kMaxInterferences, step->mutable_interferences(),
						truncated); !status.ok())
					return status;
				step->set_truncated(truncated);
				if (response->collision_step() == 0 && HasNewInterference(overlapping, step->interferences()))
				{
					response->set_collision_step(k + 1);
					if (request->stop_at_collision())
					{
						response->set_backed_off(true);
						stoppedEarly = true;
						break;
					}
				}
				last = v;
			}
		}  // Done: out of interference mode
		if (stoppedEarly)
		{
			long result = swSetValue_Failure;
			setValue(last, result);
		}

		for (const SavedPose* pose : MovedSince(saved))
		{
			sld_mcp::v1::ComponentMove* out = response->add_moved();
			DescribeComponent(pose->component, ids, math, out->mutable_after());
			out->mutable_before()->set_persist_id(out->after().persist_id());
			out->mutable_before()->set_name(out->after().name());
			Eigen::Affine3d before;
			if (PlacementOf(math, pose->xform, before))
				SetPlacement(before, out->mutable_before());
		}

		if (request->restore())
		{
			long result = swSetValue_Failure;
			const bool valueBack = std::abs(last - current) < 1e-12 || setValue(current, result);
			for (const auto& stray : RestorePoses(doc, saved))
				response->add_not_restored(stray);
			if (!valueBack)
				response->add_not_restored(name + " (its value " + MateValueText(type, last) + ")");
			response->set_restored(response->not_restored_size() == 0);
		}
		AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}
