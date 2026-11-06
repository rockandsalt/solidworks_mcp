//! gRPC client for the sld_mcp SolidWorks add-in. The wire contract lives in
//! proto/sld_mcp.proto at the repo root, shared with the C++ side.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tonic::metadata::{AsciiMetadataValue, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

use crate::discovery;

pub mod proto {
    tonic::include_proto!("sld_mcp.v1");
}

use proto::solid_works_client::SolidWorksClient;

/// Must match kTokenHeader in sld_mcp/RpcServer.cpp.
const TOKEN_HEADER: &str = "x-sld-mcp-token";

/// How long to wait for the add-in to accept a connection. It's on
/// loopback, so anything beyond this means it isn't listening.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on a single call. Calls run on SolidWorks' UI thread, so a
/// long rebuild or a modal dialog can legitimately stall them for a while.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type Client = SolidWorksClient<InterceptedService<Channel, TokenInterceptor>>;

#[derive(Clone, Default)]
pub struct AddinClient {
    /// The add-in last called and the channel to it.
    connection: Arc<Mutex<Option<(discovery::Target, Channel)>>>,
}

/// Puts the instance file's token on every call.
#[derive(Clone)]
pub struct TokenInterceptor(Option<AsciiMetadataValue>);

impl Interceptor for TokenInterceptor {
    fn call(&mut self, mut request: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.0 {
            request.metadata_mut().insert(TOKEN_HEADER, token.clone());
        }
        Ok(request)
    }
}

impl AddinClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// A client for the add-in to call now. The instance files are read
    /// again on every call (a directory listing): when SolidWorks restarts
    /// on another port, the next call goes there. The HTTP/2 connection is
    /// opened on the first call to a port and then reused by every call
    /// after it, concurrent ones included.
    fn client(&self) -> Result<Client, tonic::Status> {
        let mut connection = self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let target = match discovery::find() {
            Ok(target) => target,
            Err(message) => {
                *connection = None;
                return Err(tonic::Status::unavailable(message));
            }
        };
        let channel = match &*connection {
            Some((current, channel)) if *current == target => channel.clone(),
            _ => {
                let channel = Endpoint::from_shared(format!("http://127.0.0.1:{}", target.port))
                    .map_err(|error| tonic::Status::internal(error.to_string()))?
                    .connect_timeout(CONNECT_TIMEOUT)
                    .timeout(REQUEST_TIMEOUT)
                    .connect_lazy();
                *connection = Some((target.clone(), channel.clone()));
                channel
            }
        };
        let token = target.token.as_deref().and_then(|token| MetadataValue::try_from(token).ok());
        Ok(SolidWorksClient::with_interceptor(channel, TokenInterceptor(token)))
    }

    /// Where the add-in last called is listening, for error messages; None
    /// when none was found.
    pub fn endpoint(&self) -> Option<String> {
        let connection = self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        connection.as_ref().map(|(target, _)| match target.pid {
            Some(pid) => format!("127.0.0.1:{} (SolidWorks pid {pid})", target.port),
            None => format!("127.0.0.1:{}", target.port),
        })
    }

    /// The active document (none = nothing open) and the SolidWorks session;
    /// `timeout` shortens the usual REQUEST_TIMEOUT.
    pub async fn get_active_document(
        &self,
        timeout: Option<Duration>,
    ) -> Result<proto::GetActiveDocumentResponse, tonic::Status> {
        // Clients are cheap handles onto the shared channel; tonic needs a
        // &mut one per call.
        let mut client = self.client()?;
        let mut request = tonic::Request::new(proto::GetActiveDocumentRequest {});
        if let Some(timeout) = timeout {
            request.set_timeout(timeout);
        }
        Ok(client.get_active_document(request).await?.into_inner())
    }

    pub async fn new_part(&self, units: proto::UnitsChange) -> Result<Option<proto::Document>, tonic::Status> {
        let mut client = self.client()?;
        let response = client.new_part(proto::NewPartRequest { units: Some(units) }).await?;
        Ok(response.into_inner().document)
    }

    pub async fn new_assembly(&self, units: proto::UnitsChange) -> Result<Option<proto::Document>, tonic::Status> {
        let mut client = self.client()?;
        let response = client.new_assembly(proto::NewAssemblyRequest { units: Some(units) }).await?;
        Ok(response.into_inner().document)
    }

    pub async fn set_document_units(
        &self,
        request: proto::SetDocumentUnitsRequest,
    ) -> Result<proto::SetDocumentUnitsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.set_document_units(request).await?.into_inner())
    }

    /// Paper size in meters.
    pub async fn new_drawing(
        &self,
        request: proto::NewDrawingRequest,
    ) -> Result<proto::NewDrawingResponse, tonic::Status> {
        let mut client = self.client()?;
        let response = client.new_drawing(request).await?;
        Ok(response.into_inner())
    }

    /// Lengths in meters.
    pub async fn create_rectangle_sketch(
        &self,
        request: proto::CreateRectangleSketchRequest,
    ) -> Result<proto::CreateRectangleSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        let response = client.create_rectangle_sketch(request).await?;
        Ok(response.into_inner())
    }

    /// Coordinates in meters.
    pub async fn add_sketch_entities(
        &self,
        request: proto::AddSketchEntitiesRequest,
    ) -> Result<proto::AddSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client()?;
        let response = client.add_sketch_entities(request).await?;
        Ok(response.into_inner())
    }

    /// Depth in meters.
    pub async fn extrude_sketch(
        &self,
        request: proto::ExtrudeSketchRequest,
    ) -> Result<proto::ExtrudeSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        let response = client.extrude_sketch(request).await?;
        Ok(response.into_inner())
    }

    /// Angle in radians.
    pub async fn revolve_sketch(
        &self,
        request: proto::RevolveSketchRequest,
    ) -> Result<proto::RevolveSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.revolve_sketch(request).await?.into_inner())
    }

    /// Diameter in meters, twist in radians.
    pub async fn sweep_sketch(
        &self,
        request: proto::SweepSketchRequest,
    ) -> Result<proto::SweepSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.sweep_sketch(request).await?.into_inner())
    }

    pub async fn loft_sketch(
        &self,
        request: proto::LoftSketchRequest,
    ) -> Result<proto::LoftSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.loft_sketch(request).await?.into_inner())
    }

    /// Radius in meters.
    pub async fn fillet_edges(
        &self,
        request: proto::FilletEdgesRequest,
    ) -> Result<proto::EdgeFeatureResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.fillet_edges(request).await?.into_inner())
    }

    /// Distances in meters, angle in radians.
    pub async fn chamfer_edges(
        &self,
        request: proto::ChamferEdgesRequest,
    ) -> Result<proto::EdgeFeatureResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.chamfer_edges(request).await?.into_inner())
    }

    /// Sizes in meters, angles in radians.
    pub async fn create_helix(
        &self,
        request: proto::CreateHelixRequest,
    ) -> Result<proto::CreateHelixResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.create_helix(request).await?.into_inner())
    }

    /// Thicknesses in meters.
    pub async fn shell_body(
        &self,
        request: proto::ShellBodyRequest,
    ) -> Result<proto::EdgeFeatureResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.shell_body(request).await?.into_inner())
    }

    /// Thickness in meters, draft in radians.
    pub async fn rib_sketch(
        &self,
        request: proto::RibSketchRequest,
    ) -> Result<proto::RibSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.rib_sketch(request).await?.into_inner())
    }

    /// Angle in radians.
    pub async fn draft_faces(
        &self,
        request: proto::DraftFacesRequest,
    ) -> Result<proto::DraftFacesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.draft_faces(request).await?.into_inner())
    }

    pub async fn mirror_features(
        &self,
        request: proto::MirrorFeaturesRequest,
    ) -> Result<proto::MirrorFeaturesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.mirror_features(request).await?.into_inner())
    }

    pub async fn mirror_sketch_entities(
        &self,
        request: proto::MirrorSketchEntitiesRequest,
    ) -> Result<proto::MirrorSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.mirror_sketch_entities(request).await?.into_inner())
    }

    /// Lengths in meters.
    pub async fn offset_sketch_entities(
        &self,
        request: proto::OffsetSketchEntitiesRequest,
    ) -> Result<proto::OffsetSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.offset_sketch_entities(request).await?.into_inner())
    }

    pub async fn convert_entities(
        &self,
        request: proto::ConvertEntitiesRequest,
    ) -> Result<proto::ConvertEntitiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.convert_entities(request).await?.into_inner())
    }

    /// Lengths in meters, angles in radians.
    pub async fn sketch_corners(
        &self,
        request: proto::SketchCornersRequest,
    ) -> Result<proto::SketchEditResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.sketch_corners(request).await?.into_inner())
    }

    /// Points in meters.
    pub async fn trim_sketch_entity(
        &self,
        request: proto::TrimSketchEntityRequest,
    ) -> Result<proto::SketchEditResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.trim_sketch_entity(request).await?.into_inner())
    }

    /// Lengths in meters, angles in radians.
    pub async fn pattern_features(
        &self,
        request: proto::PatternFeaturesRequest,
    ) -> Result<proto::PatternFeaturesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.pattern_features(request).await?.into_inner())
    }

    /// Lengths in meters, angles in radians.
    pub async fn pattern_sketch_entities(
        &self,
        request: proto::PatternSketchEntitiesRequest,
    ) -> Result<proto::PatternSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.pattern_sketch_entities(request).await?.into_inner())
    }

    /// Lengths in meters.
    pub async fn inspect_model(
        &self,
        request: proto::InspectModelRequest,
    ) -> Result<proto::InspectModelResponse, tonic::Status> {
        let mut client = self.client()?;
        let mut request = tonic::Request::new(request);
        request.set_timeout(REQUEST_TIMEOUT);
        let response = client.inspect_model(request).await?;
        Ok(response.into_inner())
    }

    /// The active part's (or assembly's) tessellation; coordinates in meters.
    pub async fn get_mesh(
        &self,
        request: proto::GetMeshRequest,
    ) -> Result<proto::GetMeshResponse, tonic::Status> {
        // A detailed part's mesh is far past tonic's default 4 MB cap.
        let mut client = self
            .client()?
            .max_decoding_message_size(MAX_MESH_MESSAGE);
        let response = client.get_mesh(request).await?;
        Ok(response.into_inner())
    }

    pub async fn inspect_sketch(
        &self,
        request: proto::InspectSketchRequest,
    ) -> Result<proto::InspectSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.inspect_sketch(request).await?.into_inner())
    }

    pub async fn inspect_feature(
        &self,
        request: proto::InspectFeatureRequest,
    ) -> Result<proto::InspectFeatureResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.inspect_feature(request).await?.into_inner())
    }

    /// Values in meters / radians.
    pub async fn add_sketch_dimensions(
        &self,
        request: proto::AddSketchDimensionsRequest,
    ) -> Result<proto::AddSketchDimensionsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_sketch_dimensions(request).await?.into_inner())
    }

    pub async fn add_sketch_relations(
        &self,
        request: proto::AddSketchRelationsRequest,
    ) -> Result<proto::AddSketchRelationsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_sketch_relations(request).await?.into_inner())
    }

    pub async fn fully_define_sketch(
        &self,
        request: proto::FullyDefineSketchRequest,
    ) -> Result<proto::FullyDefineSketchResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.fully_define_sketch(request).await?.into_inner())
    }

    /// Value in meters / radians.
    pub async fn set_dimension_value(
        &self,
        request: proto::SetDimensionValueRequest,
    ) -> Result<proto::SetDimensionValueResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.set_dimension_value(request).await?.into_inner())
    }

    /// Deviations in meters / radians.
    pub async fn set_dimension_tolerance(
        &self,
        request: proto::SetDimensionToleranceRequest,
    ) -> Result<proto::SetDimensionToleranceResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.set_dimension_tolerance(request).await?.into_inner())
    }

    /// Lengths in meters, angles in radians.
    pub async fn hole_wizard(
        &self,
        request: proto::HoleWizardRequest,
    ) -> Result<proto::HoleWizardResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.hole_wizard(request).await?.into_inner())
    }

    /// Distance in meters, angle in radians.
    pub async fn add_mate(
        &self,
        request: proto::AddMateRequest,
    ) -> Result<proto::AddMateResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_mate(request).await?.into_inner())
    }

    /// Positions in meters.
    pub async fn insert_components(
        &self,
        request: proto::InsertComponentsRequest,
    ) -> Result<proto::InsertComponentsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.insert_components(request).await?.into_inner())
    }

    pub async fn replace_components(
        &self,
        request: proto::ReplaceComponentsRequest,
    ) -> Result<proto::ReplaceComponentsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.replace_components(request).await?.into_inner())
    }

    pub async fn fix_components(
        &self,
        request: proto::FixComponentsRequest,
    ) -> Result<proto::FixComponentsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.fix_components(request).await?.into_inner())
    }

    /// Moves a component while SolidWorks solves its mates; meters / radians.
    pub async fn move_component(
        &self,
        request: proto::MoveComponentRequest,
    ) -> Result<proto::MoveComponentResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.move_component(request).await?.into_inner())
    }

    /// Drives a distance / angle mate of the active assembly through a range
    /// of values, checking interference at each; meters / radians.
    pub async fn sweep_mate(
        &self,
        request: proto::SweepMateRequest,
    ) -> Result<proto::SweepMateResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.sweep_mate(request).await?.into_inner())
    }

    /// Nudges components of the active assembly to see how they can move;
    /// meters / radians, everything put back.
    pub async fn inspect_mobility(
        &self,
        request: proto::InspectMobilityRequest,
    ) -> Result<proto::InspectMobilityResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.inspect_mobility(request).await?.into_inner())
    }

    /// Where the components of the active assembly (or a drawing view's)
    /// overlap; meters.
    pub async fn check_interference(
        &self,
        request: proto::CheckInterferenceRequest,
    ) -> Result<proto::CheckInterferenceResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.check_interference(request).await?.into_inner())
    }

    pub async fn measure(&self, request: proto::MeasureRequest) -> Result<proto::MeasureResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.measure(request).await?.into_inner())
    }

    pub async fn get_mass_properties(
        &self,
        request: proto::GetMassPropertiesRequest,
    ) -> Result<proto::GetMassPropertiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.get_mass_properties(request).await?.into_inner())
    }

    /// The active document's 3D annotations (PMI); meters / radians.
    pub async fn inspect_annotations(&self) -> Result<proto::InspectAnnotationsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client
            .inspect_annotations(proto::InspectAnnotationsRequest {})
            .await?
            .into_inner())
    }

    /// The active drawing's sheets and views (optionally with view entities
    /// and annotations); sheet coordinates in meters.
    pub async fn inspect_drawing(
        &self,
        request: proto::InspectDrawingRequest,
    ) -> Result<proto::InspectDrawingResponse, tonic::Status> {
        // A detailed view lists thousands of edges.
        let mut client = self
            .client()?
            .max_decoding_message_size(MAX_MESH_MESSAGE);
        Ok(client.inspect_drawing(request).await?.into_inner())
    }

    /// Text positions in sheet meters.
    pub async fn add_drawing_dimensions(
        &self,
        request: proto::AddDrawingDimensionsRequest,
    ) -> Result<proto::AddDrawingDimensionsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_drawing_dimensions(request).await?.into_inner())
    }

    pub async fn insert_model_items(
        &self,
        request: proto::InsertModelItemsRequest,
    ) -> Result<proto::InsertModelItemsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.insert_model_items(request).await?.into_inner())
    }

    /// Paper size in meters.
    pub async fn setup_sheet(
        &self,
        request: proto::SetupSheetRequest,
    ) -> Result<proto::SetupSheetResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.setup_sheet(request).await?.into_inner())
    }

    /// Positions in sheet meters.
    pub async fn setup_drawing_view(
        &self,
        request: proto::SetupDrawingViewRequest,
    ) -> Result<proto::SetupDrawingViewResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.setup_drawing_view(request).await?.into_inner())
    }

    /// Positions in sheet meters.
    pub async fn derive_drawing_view(
        &self,
        request: proto::DeriveDrawingViewRequest,
    ) -> Result<proto::SetupDrawingViewResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.derive_drawing_view(request).await?.into_inner())
    }

    /// Positions in sheet meters.
    pub async fn shape_drawing_view(
        &self,
        request: proto::ShapeDrawingViewRequest,
    ) -> Result<proto::SetupDrawingViewResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.shape_drawing_view(request).await?.into_inner())
    }

    /// Positions in sheet meters (or a view's own coordinates).
    pub async fn setup_drawing_annotation(
        &self,
        request: proto::SetupDrawingAnnotationRequest,
    ) -> Result<proto::SetupDrawingAnnotationResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.setup_drawing_annotation(request).await?.into_inner())
    }

    /// A feature control frame or datum symbol on the active part or
    /// assembly.
    pub async fn add_model_annotation(
        &self,
        request: proto::AddModelAnnotationRequest,
    ) -> Result<proto::AddModelAnnotationResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_model_annotation(request).await?.into_inner())
    }

    pub async fn delete_drawing_annotations(
        &self,
        request: proto::DeleteDrawingAnnotationsRequest,
    ) -> Result<proto::DeleteDrawingAnnotationsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.delete_drawing_annotations(request).await?.into_inner())
    }

    pub async fn setup_drawing_table(
        &self,
        request: proto::SetupDrawingTableRequest,
    ) -> Result<proto::SetupDrawingTableResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.setup_drawing_table(request).await?.into_inner())
    }

    pub async fn add_balloons(
        &self,
        request: proto::AddBalloonsRequest,
    ) -> Result<proto::AddBalloonsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_balloons(request).await?.into_inner())
    }

    pub async fn add_center_marks(
        &self,
        request: proto::AddCenterMarksRequest,
    ) -> Result<proto::AddCenterMarksResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_center_marks(request).await?.into_inner())
    }

    pub async fn add_centerlines(
        &self,
        request: proto::AddCenterlinesRequest,
    ) -> Result<proto::AddCenterMarksResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.add_centerlines(request).await?.into_inner())
    }

    pub async fn draw_sheet_format(
        &self,
        request: proto::DrawSheetFormatRequest,
    ) -> Result<proto::DrawSheetFormatResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.draw_sheet_format(request).await?.into_inner())
    }

    pub async fn custom_properties(
        &self,
        request: proto::CustomPropertiesRequest,
    ) -> Result<proto::CustomPropertiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.custom_properties(request).await?.into_inner())
    }

    pub async fn part_material(
        &self,
        request: proto::PartMaterialRequest,
    ) -> Result<proto::PartMaterialResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.part_material(request).await?.into_inner())
    }

    pub async fn delete_features(
        &self,
        request: proto::DeleteFeaturesRequest,
    ) -> Result<proto::DeleteFeaturesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.delete_features(request).await?.into_inner())
    }

    pub async fn delete_sketch_entities(
        &self,
        request: proto::DeleteSketchEntitiesRequest,
    ) -> Result<proto::DeleteSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.delete_sketch_entities(request).await?.into_inner())
    }

    pub async fn diagnose_model(
        &self,
        request: proto::DiagnoseModelRequest,
    ) -> Result<proto::DiagnoseModelResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.diagnose_model(request).await?.into_inner())
    }

    /// The image of a whole sheet; can be several MB for a big sheet at a
    /// high resolution.
    pub async fn render_drawing(
        &self,
        request: proto::RenderDrawingRequest,
    ) -> Result<proto::RenderDrawingResponse, tonic::Status> {
        let mut client = self.client()?.max_decoding_message_size(MAX_MESH_MESSAGE);
        Ok(client.render_drawing(request).await?.into_inner())
    }

    pub async fn edit_extrude(
        &self,
        request: proto::EditExtrudeRequest,
    ) -> Result<proto::EditExtrudeResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.edit_extrude(request).await?.into_inner())
    }

    pub async fn edit_mate(
        &self,
        request: proto::EditMateRequest,
    ) -> Result<proto::EditMateResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.edit_mate(request).await?.into_inner())
    }

    pub async fn edit_fillet(
        &self,
        request: proto::EditFilletRequest,
    ) -> Result<proto::EditFilletResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.edit_fillet(request).await?.into_inner())
    }

    pub async fn edit_pattern(
        &self,
        request: proto::EditPatternRequest,
    ) -> Result<proto::EditPatternResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.edit_pattern(request).await?.into_inner())
    }

    pub async fn edit_sweep(
        &self,
        request: proto::EditSweepRequest,
    ) -> Result<proto::EditSweepResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.edit_sweep(request).await?.into_inner())
    }

    pub async fn save_document(
        &self,
        request: proto::SaveDocumentRequest,
    ) -> Result<proto::SaveDocumentResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.save_document(request).await?.into_inner())
    }

    pub async fn open_document(
        &self,
        request: proto::OpenDocumentRequest,
    ) -> Result<proto::OpenDocumentResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.open_document(request).await?.into_inner())
    }

    pub async fn close_document(
        &self,
        request: proto::CloseDocumentRequest,
    ) -> Result<proto::CloseDocumentResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.close_document(request).await?.into_inner())
    }

    pub async fn undo(&self, request: proto::UndoRequest) -> Result<proto::UndoResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.undo(request).await?.into_inner())
    }

    pub async fn run_macro(&self, request: proto::RunMacroRequest) -> Result<proto::RunMacroResponse, tonic::Status> {
        let mut client = self.client()?;
        // Send a deadline to the add-in as well as bounding the transport wait,
        // so an expired macro can be discarded while it waits for the model lock.
        let mut request = tonic::Request::new(request);
        request.set_timeout(REQUEST_TIMEOUT);
        Ok(client.run_macro(request).await?.into_inner())
    }

    pub async fn set_visibility(
        &self,
        request: proto::SetVisibilityRequest,
    ) -> Result<proto::SetVisibilityResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.set_visibility(request).await?.into_inner())
    }

    pub async fn create_reference_geometry(
        &self,
        request: proto::CreateReferenceGeometryRequest,
    ) -> Result<proto::CreateReferenceGeometryResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.create_reference_geometry(request).await?.into_inner())
    }

    pub async fn configurations(
        &self,
        request: proto::ConfigurationsRequest,
    ) -> Result<proto::ConfigurationsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.configurations(request).await?.into_inner())
    }

    pub async fn set_suppression(
        &self,
        request: proto::SetSuppressionRequest,
    ) -> Result<proto::SetSuppressionResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.set_suppression(request).await?.into_inner())
    }

    pub async fn equations(
        &self,
        request: proto::EquationsRequest,
    ) -> Result<proto::EquationsResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.equations(request).await?.into_inner())
    }

    pub async fn design_table(
        &self,
        request: proto::DesignTableRequest,
    ) -> Result<proto::DesignTableResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.design_table(request).await?.into_inner())
    }

    pub async fn rollback(&self, request: proto::RollbackRequest) -> Result<proto::RollbackResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.rollback(request).await?.into_inner())
    }

    pub async fn reorder_feature(
        &self,
        request: proto::ReorderFeatureRequest,
    ) -> Result<proto::ReorderFeatureResponse, tonic::Status> {
        let mut client = self.client()?;
        Ok(client.reorder_feature(request).await?.into_inner())
    }
}

/// Largest GetMesh reply accepted: about 5 million triangles with normals.
const MAX_MESH_MESSAGE: usize = 256 * 1024 * 1024;

#[cfg(test)]
#[path = "addin_live_tests.rs"]
mod live_tests;
