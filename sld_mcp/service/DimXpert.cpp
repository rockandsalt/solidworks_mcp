// DimXpert.cpp : AddPmiDimension, AutoDimensionScheme -- DimXpert ("MBD
// Dimension") dimensions on a part's faces (PMI; see docs/notes/dimxpert.md).
// IDimXpertPart and its options are in swdimxpert.tlb, which the add-in
// doesn't #import: they're called late-bound.
#include "pch.h"
#include "Common.h"
#include "AnnotationHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// How far past the part's box a dimension's text goes by default.
	constexpr double kTextClearance = 0.010;

	std::string DimXpertName(IDispatch* dimXpertAnnotation)
	{
		ATL::CComVariant name;
		return InvokeByName(dimXpertAnnotation, L"Name", name) && name.vt == VT_BSTR
			? ToUtf8(ATL::CComBSTR(name.bstrVal)) : std::string();
	}

	std::set<std::string> DimXpertNames(IDispatch* dimXpertPart)
	{
		std::set<std::string> names;
		for (const auto& annotation : DimXpertAnnotations(dimXpertPart))
			names.insert(DimXpertName(annotation));
		return names;
	}

	// DimXpert's annotations that aren't among `before` (by name: the same
	// annotation may come back as another COM object).
	std::vector<ATL::CComPtr<IDispatch>> NewAnnotations(IDispatch* dimXpertPart, const std::set<std::string>& before)
	{
		std::vector<ATL::CComPtr<IDispatch>> out;
		for (const auto& annotation : DimXpertAnnotations(dimXpertPart))
			if (const std::string name = DimXpertName(annotation); !name.empty() && !before.count(name))
				out.push_back(annotation);
		return out;
	}

	// The faces of `persistIds`, resolved before anything changes.
	grpc::Status ResolveFaces(IModelDocExtension* ext, const std::vector<std::string>& persistIds,
		std::vector<ATL::CComQIPtr<IFace2>>& out)
	{
		for (const auto& id : persistIds)
		{
			ATL::CComPtr<IDispatch> entity;
			if (grpc::Status status = ResolvePersistId(ext, id, entity); !status.ok())
				return status;
			ATL::CComQIPtr<IFace2> face(entity.p);
			if (!face)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"DimXpert dimensions go on faces (a hole's cylinder, a plane, ...), not edges or other entities");
			out.push_back(face);
		}
		return grpc::Status::OK;
	}

	// Selects the faces, each with its mark; false if SolidWorks wouldn't.
	bool SelectFaces(IModelDoc2* doc, const std::vector<ATL::CComQIPtr<IFace2>>& faces, const std::vector<long>& marks)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		ATL::CComPtr<ISelectionMgr> selections;
		if (FAILED(doc->get_ISelectionManager(&selections)) || !selections)
			return false;
		for (size_t i = 0; i < faces.size(); ++i)
		{
			ATL::CComPtr<ISelectData> data;
			if (FAILED(selections->CreateSelectData(&data)) || !data)
				return false;
			data->put_Mark(i < marks.size() ? marks[i] : 0);
			ATL::CComQIPtr<IEntity> entity(faces[i].p);
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (!entity || FAILED(entity->Select4(i > 0 ? VARIANT_TRUE : VARIANT_FALSE, data, &selected)) || !selected)
				return false;
		}
		return true;
	}

	// Where a dimension's text goes by default: from the part's middle
	// through the faces' middle, kTextClearance past the part's box.
	// DimXpert's own spot is the origin (seen 2026-11-12), inside the part.
	std::optional<Eigen::Vector3d> DefaultTextSpot(IModelDoc2* doc, const std::vector<ATL::CComQIPtr<IFace2>>& faces)
	{
		ATL::CComQIPtr<IPartDoc> part(doc);
		ATL::CComVariant boxVar;
		if (!part || FAILED(part->GetPartBox(VARIANT_TRUE, &boxVar)))
			return std::nullopt;
		const Eigen::AlignedBox3d box = ToAlignedBox(comvariant::ToDoubles(boxVar));
		if (box.isEmpty())
			return std::nullopt;
		Eigen::Vector3d anchor = Eigen::Vector3d::Zero();
		int count = 0;
		for (const auto& face : faces)
		{
			ATL::CComVariant faceBox;
			if (FAILED(face->GetBox(&faceBox)))
				continue;
			if (const Eigen::AlignedBox3d b = ToAlignedBox(comvariant::ToDoubles(faceBox)); !b.isEmpty())
			{
				anchor += b.center();
				++count;
			}
		}
		if (count == 0)
			return std::nullopt;
		anchor /= count;
		Eigen::Vector3d direction = anchor - box.center();
		if (direction.norm() < 1e-9)
			direction = Eigen::Vector3d::UnitZ();
		direction.normalize();
		double exit = std::numeric_limits<double>::infinity();
		for (int i = 0; i < 3; ++i)
			if (std::abs(direction[i]) > 1e-9)
				exit = std::min(exit, ((direction[i] > 0 ? box.max()[i] : box.min()[i]) - anchor[i]) / direction[i]);
		if (!std::isfinite(exit) || exit < 0.0)
			exit = 0.0;
		const Eigen::Vector3d spot = anchor + direction * (exit + kTextClearance);
		return spot.allFinite() ? std::optional<Eigen::Vector3d>(spot) : std::nullopt;
	}

	// Describes DimXpert's new annotations as InspectAnnotations does.
	void DescribeNew(IModelDocExtension* ext, const std::vector<ATL::CComPtr<IDispatch>>& added,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::AnnotationInfo>* out)
	{
		PersistIds ids(ext);
		for (const auto& dimXpert : added)
		{
			ATL::CComVariant shown;
			if (!InvokeByName(dimXpert, L"GetDisplayEntity", shown) || shown.vt != VT_DISPATCH)
				continue;
			ATL::CComQIPtr<IAnnotation> annotation(shown.pdispVal);
			if (!annotation)
				continue;
			sld_mcp::v1::AnnotationInfo* info = out->Add();
			DescribeAnnotation(annotation, ids, nullptr, info);
			if (info->dimxpert_name().empty())
				info->set_dimxpert_name(DimXpertName(dimXpert));
		}
	}

	const wchar_t* InsertMethod(sld_mcp::v1::PmiDimensionKind kind)
	{
		switch (kind)
		{
		case sld_mcp::v1::PMI_DIMENSION_KIND_SIZE: return L"InsertSizeDimension";
		case sld_mcp::v1::PMI_DIMENSION_KIND_LOCATION: return L"InsertLocationDimension";
		case sld_mcp::v1::PMI_DIMENSION_KIND_ANGLE: return L"InsertAngleDimension";
		case sld_mcp::v1::PMI_DIMENSION_KIND_BASIC: return L"InsertBasicDimension";
		default: return nullptr;
		}
	}
}

grpc::Status SolidWorksService::AddPmiDimension(grpc::ServerContext* context,
	const sld_mcp::v1::AddPmiDimensionRequest* request,
	sld_mcp::v1::AddPmiDimensionResponse* response)
{
	const wchar_t* method = InsertMethod(request->kind());
	if (!method)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say what to add: a size, location, angle or basic dimension");
	const bool size = request->kind() == sld_mcp::v1::PMI_DIMENSION_KIND_SIZE;
	if (request->faces_size() != (size ? 1 : 2))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			size ? "a size dimension takes one face of the feature"
			     : "a location, angle or basic dimension takes two faces: from, to");
	if (request->has_position() && !IsFinite(request->position()))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the text position must be finite");

	return m_executor.Execute(context, "AddPmiDimension", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		std::vector<ATL::CComQIPtr<IFace2>> faces;
		if (grpc::Status status = ResolveFaces(ext, { request->faces().begin(), request->faces().end() }, faces);
			!status.ok())
			return status;
		ATL::CComPtr<IDispatch> dimXpert = DimXpertPartOf(doc, ext, true);
		if (!dimXpert)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gives no DimXpert for this part");
		const std::set<std::string> before = DimXpertNames(dimXpert);

		ATL::CComVariant option;
		if (!CallByName(dimXpert, L"GetDimOption", {}, &option) || option.vt != VT_DISPATCH || !option.pdispVal)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "DimXpert gave no dimension options");
		// TextPosition takes an array of 3 doubles (a math point does nothing;
		// seen 2026-11-12).
		std::optional<Eigen::Vector3d> spot;
		if (request->has_position())
			spot = Eigen::Vector3d(request->position().x(), request->position().y(), request->position().z());
		else
			spot = DefaultTextSpot(doc, faces);
		if (spot && !CallByName(option.pdispVal, L"TextPosition", { comvariant::FromDoubles(spot->data(), 3) }, nullptr, true))
			return grpc::Status(grpc::StatusCode::INTERNAL, "DimXpert didn't take the text position");

		if (!SelectFaces(doc, faces, {}))
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select the faces");
		}
		// Returns false even when it adds the dimension (seen 2026-11-12):
		// what's new tells.
		ATL::CComVariant ignored;
		CallByName(dimXpert, method, { ATL::CComVariant(option.pdispVal) }, &ignored);
		doc->ClearSelection2(VARIANT_TRUE);
		const std::vector<ATL::CComPtr<IDispatch>> added = NewAnnotations(dimXpert, before);
		if (added.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"DimXpert made no dimension of those faces -- it dimensions the features it recognizes: a size on "
				"a hole's, shaft's, fillet's or chamfer's face (two parallel planes take a location dimension), a "
				"location between two features (from a plane parallel to a hole's axis, not across it)");
		doc->SetSaveFlag();
		doc->GraphicsRedraw2();
		DescribeNew(ext, added, response->mutable_annotations());
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::AutoDimensionScheme(grpc::ServerContext* context,
	const sld_mcp::v1::AutoDimensionSchemeRequest* request,
	sld_mcp::v1::AutoDimensionSchemeResponse* response)
{
	if (request->primary_datum().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the scheme needs a primary datum face");
	if (request->tertiary_datum().size() && request->secondary_datum().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a tertiary datum needs a secondary one");
	if (request->part_type() < 0 || request->part_type() > 1 || request->tolerance_type() < 0 ||
		request->tolerance_type() > 1 || request->pattern_type() < 0 || request->pattern_type() > 1 ||
		request->feature_filters() < 0)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unsupported scheme option");

	return m_executor.Execute(context, "AutoDimensionScheme", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;
		std::vector<std::string> datums = { request->primary_datum() };
		if (!request->secondary_datum().empty())
			datums.push_back(request->secondary_datum());
		if (!request->tertiary_datum().empty())
			datums.push_back(request->tertiary_datum());
		std::vector<ATL::CComQIPtr<IFace2>> faces;
		if (grpc::Status status = ResolveFaces(ext, datums, faces); !status.ok())
			return status;
		for (size_t i = 0; i < faces.size(); ++i)
			for (size_t j = 0; j < i; ++j)
				if (SameObject(faces[i], faces[j]))
					return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "each datum needs a face of its own");
		ATL::CComPtr<IDispatch> dimXpert = DimXpertPartOf(doc, ext, true);
		if (!dimXpert)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gives no DimXpert for this part");
		const std::set<std::string> before = DimXpertNames(dimXpert);

		ATL::CComVariant option;
		if (!CallByName(dimXpert, L"GetAutoDimSchemeOption", {}, &option) || option.vt != VT_DISPATCH || !option.pdispVal)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "DimXpert gave no scheme options");
		IDispatch* scheme = option.pdispVal;
		const bool set = CallByName(scheme, L"PartType", { ATL::CComVariant(static_cast<long>(request->part_type())) }, nullptr, true) &&
			CallByName(scheme, L"ToleranceType", { ATL::CComVariant(static_cast<long>(request->tolerance_type())) }, nullptr, true) &&
			CallByName(scheme, L"PatternType", { ATL::CComVariant(static_cast<long>(request->pattern_type())) }, nullptr, true) &&
			CallByName(scheme, L"ScopeAllFeature", { ATL::CComVariant(true) }, nullptr, true) &&
			(request->feature_filters() == 0 ||
				CallByName(scheme, L"FeatureFilters", { ATL::CComVariant(static_cast<long>(request->feature_filters())) }, nullptr, true));
		if (!set)
			return grpc::Status(grpc::StatusCode::INTERNAL, "DimXpert didn't take the scheme options");

		// Primary, secondary, tertiary: marks 1, 2, 4.
		if (!SelectFaces(doc, faces, { 1, 2, 4 }))
		{
			doc->ClearSelection2(VARIANT_TRUE);
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select the datum faces");
		}
		// Returns false even when it adds a scheme (seen 2026-11-12).
		ATL::CComVariant ignored;
		CallByName(dimXpert, L"AutoDimensionScheme", { ATL::CComVariant(scheme) }, &ignored);
		doc->ClearSelection2(VARIANT_TRUE);
		const std::vector<ATL::CComPtr<IDispatch>> added = NewAnnotations(dimXpert, before);
		if (added.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"DimXpert's scheme added nothing (datum faces it can't use, or every feature already dimensioned?)");
		doc->SetSaveFlag();
		doc->GraphicsRedraw2();
		DescribeNew(ext, added, response->mutable_annotations());
		return grpc::Status::OK;
	});
}
