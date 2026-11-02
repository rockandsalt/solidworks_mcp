// SolidWorksService.h : the gRPC service the MCP server talks to.
//
// Implements the RPCs declared in proto/sld_mcp.proto. Adding a tool means:
// add the rpc + messages to the .proto, override the generated method here,
/// and implement it in the matching service/<Area>.cpp (helpers that more than
// one area needs go in service/*Helpers.h or service/Common.h).
#pragma once

#include "GrpcIncludes.h"
#include <mutex>

class SolidWorksService final : public sld_mcp::v1::SolidWorks::Service
{
public:
	// `git` / `swAppCookie` identify the ISldWorks registered in the Global
	// Interface Table by CSwAddin. Both must outlive this service.
	SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie);

	grpc::Status GetActiveDocument(grpc::ServerContext* context,
		const sld_mcp::v1::GetActiveDocumentRequest* request,
		sld_mcp::v1::GetActiveDocumentResponse* response) override;

	grpc::Status NewPart(grpc::ServerContext* context,
		const sld_mcp::v1::NewPartRequest* request,
		sld_mcp::v1::NewPartResponse* response) override;
	grpc::Status NewAssembly(grpc::ServerContext* context,
		const sld_mcp::v1::NewAssemblyRequest* request,
		sld_mcp::v1::NewAssemblyResponse* response) override;
	grpc::Status NewDrawing(grpc::ServerContext* context,
		const sld_mcp::v1::NewDrawingRequest* request,
		sld_mcp::v1::NewDrawingResponse* response) override;
	grpc::Status SetDocumentUnits(grpc::ServerContext* context,
		const sld_mcp::v1::SetDocumentUnitsRequest* request,
		sld_mcp::v1::SetDocumentUnitsResponse* response) override;

	grpc::Status CreateRectangleSketch(grpc::ServerContext* context,
		const sld_mcp::v1::CreateRectangleSketchRequest* request,
		sld_mcp::v1::CreateRectangleSketchResponse* response) override;

	grpc::Status AddSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::AddSketchEntitiesRequest* request,
		sld_mcp::v1::AddSketchEntitiesResponse* response) override;

	grpc::Status ExtrudeSketch(grpc::ServerContext* context,
		const sld_mcp::v1::ExtrudeSketchRequest* request,
		sld_mcp::v1::ExtrudeSketchResponse* response) override;

	grpc::Status RevolveSketch(grpc::ServerContext* context,
		const sld_mcp::v1::RevolveSketchRequest* request,
		sld_mcp::v1::RevolveSketchResponse* response) override;

	grpc::Status SweepSketch(grpc::ServerContext* context,
		const sld_mcp::v1::SweepSketchRequest* request,
		sld_mcp::v1::SweepSketchResponse* response) override;

	grpc::Status LoftSketch(grpc::ServerContext* context,
		const sld_mcp::v1::LoftSketchRequest* request,
		sld_mcp::v1::LoftSketchResponse* response) override;

	grpc::Status FilletEdges(grpc::ServerContext* context,
		const sld_mcp::v1::FilletEdgesRequest* request,
		sld_mcp::v1::EdgeFeatureResponse* response) override;

	grpc::Status ChamferEdges(grpc::ServerContext* context,
		const sld_mcp::v1::ChamferEdgesRequest* request,
		sld_mcp::v1::EdgeFeatureResponse* response) override;

	grpc::Status CreateHelix(grpc::ServerContext* context,
		const sld_mcp::v1::CreateHelixRequest* request,
		sld_mcp::v1::CreateHelixResponse* response) override;

	grpc::Status ShellBody(grpc::ServerContext* context,
		const sld_mcp::v1::ShellBodyRequest* request,
		sld_mcp::v1::EdgeFeatureResponse* response) override;

	grpc::Status RibSketch(grpc::ServerContext* context,
		const sld_mcp::v1::RibSketchRequest* request,
		sld_mcp::v1::RibSketchResponse* response) override;

	grpc::Status DraftFaces(grpc::ServerContext* context,
		const sld_mcp::v1::DraftFacesRequest* request,
		sld_mcp::v1::DraftFacesResponse* response) override;

	grpc::Status MirrorFeatures(grpc::ServerContext* context,
		const sld_mcp::v1::MirrorFeaturesRequest* request,
		sld_mcp::v1::MirrorFeaturesResponse* response) override;

	grpc::Status MirrorSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::MirrorSketchEntitiesRequest* request,
		sld_mcp::v1::MirrorSketchEntitiesResponse* response) override;

	grpc::Status OffsetSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::OffsetSketchEntitiesRequest* request,
		sld_mcp::v1::OffsetSketchEntitiesResponse* response) override;

	grpc::Status ConvertEntities(grpc::ServerContext* context,
		const sld_mcp::v1::ConvertEntitiesRequest* request,
		sld_mcp::v1::ConvertEntitiesResponse* response) override;

	grpc::Status SketchCorners(grpc::ServerContext* context,
		const sld_mcp::v1::SketchCornersRequest* request,
		sld_mcp::v1::SketchEditResponse* response) override;

	grpc::Status TrimSketchEntity(grpc::ServerContext* context,
		const sld_mcp::v1::TrimSketchEntityRequest* request,
		sld_mcp::v1::SketchEditResponse* response) override;

	grpc::Status PatternFeatures(grpc::ServerContext* context,
		const sld_mcp::v1::PatternFeaturesRequest* request,
		sld_mcp::v1::PatternFeaturesResponse* response) override;

	grpc::Status PatternSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::PatternSketchEntitiesRequest* request,
		sld_mcp::v1::PatternSketchEntitiesResponse* response) override;

	grpc::Status InspectModel(grpc::ServerContext* context,
		const sld_mcp::v1::InspectModelRequest* request,
		sld_mcp::v1::InspectModelResponse* response) override;

	grpc::Status GetMesh(grpc::ServerContext* context,
		const sld_mcp::v1::GetMeshRequest* request,
		sld_mcp::v1::GetMeshResponse* response) override;

	grpc::Status InspectSketch(grpc::ServerContext* context,
		const sld_mcp::v1::InspectSketchRequest* request,
		sld_mcp::v1::InspectSketchResponse* response) override;

	grpc::Status AddSketchDimensions(grpc::ServerContext* context,
		const sld_mcp::v1::AddSketchDimensionsRequest* request,
		sld_mcp::v1::AddSketchDimensionsResponse* response) override;

	grpc::Status AddSketchRelations(grpc::ServerContext* context,
		const sld_mcp::v1::AddSketchRelationsRequest* request,
		sld_mcp::v1::AddSketchRelationsResponse* response) override;

	grpc::Status FullyDefineSketch(grpc::ServerContext* context,
		const sld_mcp::v1::FullyDefineSketchRequest* request,
		sld_mcp::v1::FullyDefineSketchResponse* response) override;

	grpc::Status SetDimensionValue(grpc::ServerContext* context,
		const sld_mcp::v1::SetDimensionValueRequest* request,
		sld_mcp::v1::SetDimensionValueResponse* response) override;

	grpc::Status SetDimensionTolerance(grpc::ServerContext* context,
		const sld_mcp::v1::SetDimensionToleranceRequest* request,
		sld_mcp::v1::SetDimensionToleranceResponse* response) override;

	grpc::Status HoleWizard(grpc::ServerContext* context,
		const sld_mcp::v1::HoleWizardRequest* request,
		sld_mcp::v1::HoleWizardResponse* response) override;

	grpc::Status AddMate(grpc::ServerContext* context,
		const sld_mcp::v1::AddMateRequest* request,
		sld_mcp::v1::AddMateResponse* response) override;

	grpc::Status InsertComponents(grpc::ServerContext* context,
		const sld_mcp::v1::InsertComponentsRequest* request,
		sld_mcp::v1::InsertComponentsResponse* response) override;

	grpc::Status ReplaceComponents(grpc::ServerContext* context,
		const sld_mcp::v1::ReplaceComponentsRequest* request,
		sld_mcp::v1::ReplaceComponentsResponse* response) override;

	grpc::Status FixComponents(grpc::ServerContext* context,
		const sld_mcp::v1::FixComponentsRequest* request,
		sld_mcp::v1::FixComponentsResponse* response) override;

	grpc::Status MoveComponent(grpc::ServerContext* context,
		const sld_mcp::v1::MoveComponentRequest* request,
		sld_mcp::v1::MoveComponentResponse* response) override;

	grpc::Status SweepMate(grpc::ServerContext* context,
		const sld_mcp::v1::SweepMateRequest* request,
		sld_mcp::v1::SweepMateResponse* response) override;

	grpc::Status InspectMobility(grpc::ServerContext* context,
		const sld_mcp::v1::InspectMobilityRequest* request,
		sld_mcp::v1::InspectMobilityResponse* response) override;

	grpc::Status CheckInterference(grpc::ServerContext* context,
		const sld_mcp::v1::CheckInterferenceRequest* request,
		sld_mcp::v1::CheckInterferenceResponse* response) override;

	grpc::Status InspectAnnotations(grpc::ServerContext* context,
		const sld_mcp::v1::InspectAnnotationsRequest* request,
		sld_mcp::v1::InspectAnnotationsResponse* response) override;

	grpc::Status AddModelAnnotation(grpc::ServerContext* context,
		const sld_mcp::v1::AddModelAnnotationRequest* request,
		sld_mcp::v1::AddModelAnnotationResponse* response) override;

	grpc::Status InspectDrawing(grpc::ServerContext* context,
		const sld_mcp::v1::InspectDrawingRequest* request,
		sld_mcp::v1::InspectDrawingResponse* response) override;

	grpc::Status AddDrawingDimensions(grpc::ServerContext* context,
		const sld_mcp::v1::AddDrawingDimensionsRequest* request,
		sld_mcp::v1::AddDrawingDimensionsResponse* response) override;

	grpc::Status InsertModelItems(grpc::ServerContext* context,
		const sld_mcp::v1::InsertModelItemsRequest* request,
		sld_mcp::v1::InsertModelItemsResponse* response) override;

	grpc::Status RenderDrawing(grpc::ServerContext* context,
		const sld_mcp::v1::RenderDrawingRequest* request,
		sld_mcp::v1::RenderDrawingResponse* response) override;
	grpc::Status SetupSheet(grpc::ServerContext* context,
		const sld_mcp::v1::SetupSheetRequest* request,
		sld_mcp::v1::SetupSheetResponse* response) override;
	grpc::Status SetupDrawingView(grpc::ServerContext* context,
		const sld_mcp::v1::SetupDrawingViewRequest* request,
		sld_mcp::v1::SetupDrawingViewResponse* response) override;
	grpc::Status SetupDrawingAnnotation(grpc::ServerContext* context,
		const sld_mcp::v1::SetupDrawingAnnotationRequest* request,
		sld_mcp::v1::SetupDrawingAnnotationResponse* response) override;
	grpc::Status DeleteDrawingAnnotations(grpc::ServerContext* context,
		const sld_mcp::v1::DeleteDrawingAnnotationsRequest* request,
		sld_mcp::v1::DeleteDrawingAnnotationsResponse* response) override;

	grpc::Status SetupDrawingTable(grpc::ServerContext* context,
		const sld_mcp::v1::SetupDrawingTableRequest* request,
		sld_mcp::v1::SetupDrawingTableResponse* response) override;

	grpc::Status AddBalloons(grpc::ServerContext* context,
		const sld_mcp::v1::AddBalloonsRequest* request,
		sld_mcp::v1::AddBalloonsResponse* response) override;

	grpc::Status DrawSheetFormat(grpc::ServerContext* context,
		const sld_mcp::v1::DrawSheetFormatRequest* request,
		sld_mcp::v1::DrawSheetFormatResponse* response) override;

	grpc::Status CustomProperties(grpc::ServerContext* context,
		const sld_mcp::v1::CustomPropertiesRequest* request,
		sld_mcp::v1::CustomPropertiesResponse* response) override;

	grpc::Status PartMaterial(grpc::ServerContext* context,
		const sld_mcp::v1::PartMaterialRequest* request,
		sld_mcp::v1::PartMaterialResponse* response) override;

	grpc::Status DeleteFeatures(grpc::ServerContext* context,
		const sld_mcp::v1::DeleteFeaturesRequest* request,
		sld_mcp::v1::DeleteFeaturesResponse* response) override;

	grpc::Status DeleteSketchEntities(grpc::ServerContext* context,
		const sld_mcp::v1::DeleteSketchEntitiesRequest* request,
		sld_mcp::v1::DeleteSketchEntitiesResponse* response) override;

	grpc::Status DiagnoseModel(grpc::ServerContext* context,
		const sld_mcp::v1::DiagnoseModelRequest* request,
		sld_mcp::v1::DiagnoseModelResponse* response) override;

	grpc::Status EditExtrude(grpc::ServerContext* context,
		const sld_mcp::v1::EditExtrudeRequest* request,
		sld_mcp::v1::EditExtrudeResponse* response) override;

	grpc::Status EditMate(grpc::ServerContext* context,
		const sld_mcp::v1::EditMateRequest* request,
		sld_mcp::v1::EditMateResponse* response) override;

	grpc::Status SaveDocument(grpc::ServerContext* context,
		const sld_mcp::v1::SaveDocumentRequest* request,
		sld_mcp::v1::SaveDocumentResponse* response) override;

	grpc::Status OpenDocument(grpc::ServerContext* context,
		const sld_mcp::v1::OpenDocumentRequest* request,
		sld_mcp::v1::OpenDocumentResponse* response) override;

	grpc::Status CloseDocument(grpc::ServerContext* context,
		const sld_mcp::v1::CloseDocumentRequest* request,
		sld_mcp::v1::CloseDocumentResponse* response) override;

	grpc::Status Undo(grpc::ServerContext* context,
		const sld_mcp::v1::UndoRequest* request,
		sld_mcp::v1::UndoResponse* response) override;

	grpc::Status SetVisibility(grpc::ServerContext* context,
		const sld_mcp::v1::SetVisibilityRequest* request,
		sld_mcp::v1::SetVisibilityResponse* response) override;

	grpc::Status CreateReferenceGeometry(grpc::ServerContext* context,
		const sld_mcp::v1::CreateReferenceGeometryRequest* request,
		sld_mcp::v1::CreateReferenceGeometryResponse* response) override;

private:
	// gRPC runs handlers on its own thread pool, not on SolidWorks' main
	// STA thread, so ISldWorks can't be used directly. This fetches a
	// marshaled proxy from the GIT; COM then forwards every call made on it
	// to the main thread. Call only inside a ComApartment scope (see .cpp).
	grpc::Status GetSwApp(ATL::CComPtr<ISldWorks>& swApp) const;

	ATL::CComPtr<IGlobalInterfaceTable> m_git;
	DWORD m_swAppCookie;

	// Serializes RPCs that change the model. Each COM call is marshaled to
	// SolidWorks' main thread on its own, so two concurrent multi-step RPCs
	// (select plane, open sketch, ...) could otherwise interleave their
	// steps and clobber each other's selection or open sketch.
	std::mutex m_editMutex;
};
