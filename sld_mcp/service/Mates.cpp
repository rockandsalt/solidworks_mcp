// Mates.cpp : AddMate.
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"

using namespace sld_mcp::service;

grpc::Status SolidWorksService::AddMate(grpc::ServerContext* /*context*/,
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
		if (!std::isfinite(angle) || angle < 0.0 || angle > 3.14159265358979323846 + 1e-9)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the angle must be 0 to 180 degrees");
	}
	const long alignment = mateType == swMateLOCK ? swMateAlignCLOSEST : ToSwMateAlign(request->alignment());
	const VARIANT_BOOL flip = request->flip() ? VARIANT_TRUE : VARIANT_FALSE;
	const VARIANT_BOOL lockRotation =
		mateType == swMateCONCENTRIC && request->lock_rotation() ? VARIANT_TRUE : VARIANT_FALSE;

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

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
	settings.refs.assign(std::begin(refs), std::end(refs));
	ATL::CComPtr<IFeature> mateFeature;
	if (grpc::Status status = CreateMate(doc, assembly, settings, mateFeature); !status.ok())
		return status;

	DescribeMate(mateFeature, ids, response->mutable_mate());
	ComponentList components(ids, math, response->mutable_components());
	for (const auto& ref : refs)
		components.Add(ref.component);
	return grpc::Status::OK;
}
