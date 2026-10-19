// Annotations.cpp : InspectAnnotations (PMI / MBD).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"

using namespace sld_mcp::service;

grpc::Status SolidWorksService::InspectAnnotations(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::InspectAnnotationsRequest* /*request*/,
	sld_mcp::v1::InspectAnnotationsResponse* response)
{
	// Read-only, but serialized with edits like InspectModel.
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

	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	PersistIds ids(ext);

	ATL::CComBSTR title;
	HRESULT hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);
	response->mutable_document()->set_title(ToUtf8(title));
	const bool isAssembly = docType == swDocASSEMBLY;
	response->mutable_document()->set_type(isAssembly ? sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY
		: sld_mcp::v1::DOCUMENT_TYPE_PART);

	// Assemblies: attached faces/edges belong to components, listed with
	// their placements like InspectModel does.
	ATL::CComQIPtr<IMathUtility> math;
	std::optional<ComponentList> components;
	if (isAssembly)
	{
		if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
			return status;
		components.emplace(ids, math, response->mutable_components());
	}

	bool truncated = false;
	for (const auto& annotation : AllAnnotations(doc, ext, truncated))
		DescribeAnnotation(annotation, ids, components ? &*components : nullptr, response->add_annotations());
	response->set_truncated(truncated);
	return grpc::Status::OK;
}
