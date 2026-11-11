// Diagnosing.cpp : DiagnoseModel (see docs/notes/diagnosing.md).
#include "pch.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "SketchHelpers.h"
#include "AssemblyHelpers.h"
#include "FeatureTreeHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// Most problems one DiagnoseModel reply lists.
	constexpr int kMaxDiagnosedProblems = 500;

	// Most parents/children listed per problem.
	constexpr int kMaxDiagnosedRelatives = 50;

	bool IsDimensionConstraint(long type)
	{
		return type == swConstraintType_DISTANCE || type == swConstraintType_ANGLE ||
			type == swConstraintType_RADIUS || type == swConstraintType_DIAMETER;
	}

	void AddFeatureRefs(const std::vector<ATL::CComPtr<IFeature>>& features,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::FeatureRef>* out)
	{
		for (const auto& feature : features)
		{
			if (!feature || out->size() >= kMaxDiagnosedRelatives)
				continue;
			SetFeatureRef(out->Add(), FeatureName(feature), FeatureTypeName(feature));
		}
	}

	std::vector<ATL::CComPtr<IFeature>> ParentFeatures(IFeature* feature)
	{
		ATL::CComVariant parents;
		if (FAILED(feature->GetParents(&parents)))
			return {};
		return FeatureList(parents);
	}

	void DescribeRelationProblem(ISketchRelation* relation, sld_mcp::v1::RelationProblem problem,
		const std::string& sketchPid, const SketchItems& items, sld_mcp::v1::SketchRelationProblem* out)
	{
		out->set_problem(problem);
		long type = 0;
		if (SUCCEEDED(relation->GetRelationType(&type)))
			out->set_constraint_type(static_cast<int32_t>(type));
		if (IsDimensionConstraint(type))
		{
			ATL::CComPtr<IDispatch> disp;
			if (SUCCEEDED(relation->GetDisplayDimension(&disp)) && disp)
				if (ATL::CComQIPtr<IDisplayDimension> display(disp.p); display)
				{
					sld_mcp::v1::DimensionInfo info;
					if (DescribeDimension(display, &info))
						out->set_dimension_name(info.name());
				}
		}
		ATL::CComVariant types;
		if (SUCCEEDED(relation->GetEntitiesType(&types)))
			for (long long t : comvariant::ToInt64s(types))
				out->add_entity_types(static_cast<int32_t>(t));
		ATL::CComVariant entities;
		if (SUCCEEDED(relation->GetEntities(&entities)))
			for (const auto& entity : comvariant::ToDispatches(entities))
			{
				// A lost model edge/face isn't null but a stand-in segment
				// outside the sketch's own (SketchItems::Contains), so in a
				// dangling relation anything outside the sketch counts as
				// missing.
				sld_mcp::v1::SketchItemRef ref;
				if (entity && items.Contains(entity) && SetItemRef(entity, sketchPid, &ref))
					*out->add_items() = ref;
				else if (!entity || problem == sld_mcp::v1::RELATION_PROBLEM_DANGLING)
					out->set_missing_count(out->missing_count() + 1);
				else
					out->set_external_count(out->external_count() + 1);
			}
	}

	// The sketch's problem relations and dimensions. True if it has any (or
	// can't be solved).
	bool DiagnoseSketch(IFeature* feature, const std::string& type, bool hasError, IModelDocExtension* ext,
		sld_mcp::v1::SketchDiagnosis* out)
	{
		ATL::CComPtr<ISketch> sketch;
		if (!SketchOf(feature, sketch).ok())
			return false;
		const std::string sketchPid = PersistIdOf(ext, feature);
		out->set_status(SketchStatus(sketch));
		SketchItems items;
		items.Load(sketch);

		// A relation can be on several lists; it's reported once, under the
		// first (worst) one.
		std::set<IUnknown*> seen;
		std::vector<ATL::CComPtr<IUnknown>> alive;
		std::set<std::string> dimensionNames;
		const std::pair<long, sld_mcp::v1::RelationProblem> filters[] = {
			{swDangling, sld_mcp::v1::RELATION_PROBLEM_DANGLING},
			{swBroken, sld_mcp::v1::RELATION_PROBLEM_BROKEN},
			{swOverDefining, sld_mcp::v1::RELATION_PROBLEM_OVER_DEFINING},
		};
		for (const auto& [filter, problem] : filters)
			for (const auto& relation : SketchRelations(sketch, filter))
			{
				ATL::CComPtr<IUnknown> identity;
				if (FAILED(relation->QueryInterface(IID_PPV_ARGS(&identity))) || !seen.insert(identity.p).second)
					continue;
				alive.push_back(identity);
				if (out->relations_size() >= static_cast<int>(kMaxSketchItems))
					break;
				sld_mcp::v1::SketchRelationProblem* p = out->add_relations();
				DescribeRelationProblem(relation, problem, sketchPid, items, p);
				if (!p->dimension_name().empty())
					dimensionNames.insert(p->dimension_name());
			}

		// Dimensions can also be dangling without showing up above.
		for (const auto& display : FeatureDimensions(feature))
		{
			ATL::CComPtr<IAnnotation> annotation;
			VARIANT_BOOL dangling = VARIANT_FALSE;
			if (FAILED(display->IGetAnnotation(&annotation)) || !annotation ||
				FAILED(annotation->IsDangling(&dangling)) || !dangling)
				continue;
			sld_mcp::v1::DimensionInfo info;
			if (DescribeDimension(display, &info) && !info.name().empty() && dimensionNames.insert(info.name()).second)
				out->add_dangling_dimensions(info.name());
		}

		// A 2D sketch that has lost its plane or face. Only claimed for a
		// sketch SolidWorks flags anyway, so a reference SolidWorks just
		// doesn't report can't be mistaken for a missing one.
		if (type == "ProfileFeature" && hasError)
		{
			long entityType = 0;
			ATL::CComPtr<IDispatch> reference;
			if (SUCCEEDED(sketch->GetReferenceEntity(&entityType, &reference)) && !reference)
				out->set_plane_missing(true);
		}

		const auto status = out->status();
		return out->relations_size() > 0 || out->dangling_dimensions_size() > 0 || out->plane_missing() ||
			status == sld_mcp::v1::DEFINITION_STATUS_OVER || status == sld_mcp::v1::DEFINITION_STATUS_NO_SOLUTION ||
			status == sld_mcp::v1::DEFINITION_STATUS_INVALID;
	}

	// SolidWorks' What's Wrong list: each feature with its error code and
	// whether it's only a warning.
	struct WhatsWrongEntry
	{
		ATL::CComPtr<IFeature> feature;
		long code = swFeatureErrorNone;
		bool warning = false;
		bool matched = false;  // found in our feature tree walk
	};

	std::vector<WhatsWrongEntry> WhatsWrong(IModelDocExtension* ext)
	{
		std::vector<WhatsWrongEntry> out;
		ATL::CComVariant features, codes, warnings;
		VARIANT_BOOL ok = VARIANT_FALSE;
		if (FAILED(ext->GetWhatsWrong(&features, &codes, &warnings, &ok)))
			return out;
		const auto codeList = comvariant::ToInt64s(codes);
		const auto warningList = comvariant::ToInt64s(warnings);
		// Positions line up across the three arrays, so nulls are kept.
		const auto raw = comvariant::ToDispatches(features);
		for (size_t i = 0; i < raw.size(); ++i)
		{
			ATL::CComQIPtr<IFeature> feature(raw[i].p);
			if (!feature)
				continue;
			WhatsWrongEntry e;
			e.feature = feature;
			if (i < codeList.size())
				e.code = static_cast<long>(codeList[i]);
			if (i < warningList.size())
				e.warning = warningList[i] != 0;
			out.push_back(e);
		}
		return out;
	}
}

grpc::Status SolidWorksService::DiagnoseModel(grpc::ServerContext* context,
	const sld_mcp::v1::DiagnoseModelRequest* request,
	sld_mcp::v1::DiagnoseModelResponse* response)
{
	return m_executor.Execute(context, "DiagnoseModel", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		PersistIds ids(ext);

		ATL::CComBSTR title;
		if (SUCCEEDED(doc->GetTitle(&title)))
			response->mutable_document()->set_title(ToUtf8(title));
		response->mutable_document()->set_type(
			docType == swDocASSEMBLY ? sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY : sld_mcp::v1::DOCUMENT_TYPE_PART);

		if (!request->skip_rebuild())
		{
			// No "What's Wrong" dialog popping up over the errors being read.
			PreferenceToggles toggles(swApp);
			toggles.Set(swShowErrorsEveryRebuild, false);
			// It returns false when the rebuild has errors, which is what
			// we're here for: rebuilt means it ran.
			VARIANT_BOOL ok = VARIANT_FALSE;
			response->set_rebuilt(SUCCEEDED(doc->EditRebuild3(&ok)));
		}

		const std::vector<TreeFeature> tree = FeatureTree(doc);
		std::vector<WhatsWrongEntry> whatsWrong = WhatsWrong(ext);

		auto addProblem = [&]() -> sld_mcp::v1::FeatureDiagnosis* {
			if (response->problems_size() >= kMaxDiagnosedProblems)
			{
				response->set_truncated(true);
				return nullptr;
			}
			return response->add_problems();
		};
		auto describeRelatives = [&](IFeature* feature, sld_mcp::v1::FeatureDiagnosis* d) {
			AddFeatureRefs(ParentFeatures(feature), d->mutable_parents());
			AddFeatureRefs(ChildFeatures(feature), d->mutable_children());
		};

		int checked = 0;
		int sketches = 0;
		for (const TreeFeature& f : tree)
		{
			// The tree's fixed folders say nothing about the model.
			if (f.fixed && f.type != "Reference" && f.type != "RefPlane")
				continue;
			++checked;

			VARIANT_BOOL warning = VARIANT_FALSE;
			long code = swFeatureErrorNone;
			if (FAILED(f.feature->GetErrorCode2(&warning, &code)))
				code = swFeatureErrorNone;
			bool listed = false;
			for (WhatsWrongEntry& e : whatsWrong)
				if (!e.matched && SameObject(e.feature, f.feature))
				{
					e.matched = true;
					listed = true;
					if (code == swFeatureErrorNone)
					{
						code = e.code;
						warning = e.warning ? VARIANT_TRUE : VARIANT_FALSE;
					}
				}

			sld_mcp::v1::SketchDiagnosis sketch;
			bool sketchProblem = false;
			if (IsSketchType(f.type))
			{
				++sketches;
				sketchProblem = DiagnoseSketch(f.feature, f.type, code != swFeatureErrorNone, ext, &sketch);
			}

			google::protobuf::RepeatedPtrField<sld_mcp::v1::MateEntityState> mateEntities;
			bool mateProblem = false;
			if (f.type.rfind("Mate", 0) == 0 && f.type != "MateGroup" && f.type != "MateReferenceGroupFeature")
				mateProblem = DiagnoseMate(f.feature, ids, &mateEntities);

			if (code == swFeatureErrorNone && !listed && !sketchProblem && !mateProblem)
				continue;
			sld_mcp::v1::FeatureDiagnosis* d = addProblem();
			if (!d)
				break;
			d->set_persist_id(ids.Get(f.feature));
			d->set_name(f.name);
			d->set_type_name(f.type);
			d->set_error_code(static_cast<int32_t>(code));
			d->set_warning(code != swFeatureErrorNone && warning != VARIANT_FALSE);
			VARIANT_BOOL suppressed = VARIANT_FALSE;
			if (SUCCEEDED(f.feature->IsSuppressed(&suppressed)))
				d->set_suppressed(suppressed != VARIANT_FALSE);
			d->set_whats_wrong(listed);
			describeRelatives(f.feature, d);
			if (IsSketchType(f.type))
				d->mutable_sketch()->Swap(&sketch);
			d->mutable_mate_entities()->Swap(&mateEntities);
		}

		// What's Wrong entries our walk didn't reach: in an assembly, the
		// components' own features.
		for (const WhatsWrongEntry& e : whatsWrong)
		{
			if (e.matched || response->truncated())
				continue;
			sld_mcp::v1::FeatureDiagnosis* d = addProblem();
			if (!d)
				break;
			d->set_persist_id(ids.Get(e.feature));
			d->set_name(FeatureName(e.feature));
			d->set_type_name(FeatureTypeName(e.feature));
			d->set_component_persist_id(ids.Get(ComponentOf(e.feature)));
			d->set_error_code(static_cast<int32_t>(e.code));
			d->set_warning(e.code != swFeatureErrorNone && e.warning);
			d->set_whats_wrong(true);
			describeRelatives(e.feature, d);
		}

		response->set_checked_features(checked);
		response->set_checked_sketches(sketches);
		return grpc::Status::OK;
	});
}
