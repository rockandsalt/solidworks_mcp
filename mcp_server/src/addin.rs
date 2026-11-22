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

type Client = SolidWorksClient<InterceptedService<Channel, RequestInterceptor>>;

#[derive(Clone, Default)]
pub struct AddinClient {
    /// The add-in last called and the channel to it.
    connection: Arc<Mutex<Option<(discovery::Target, Channel)>>>,
}

/// Every RPC carries authentication and a server-visible deadline. Explicit
/// shorter timeouts (e.g. discovery probes) must survive interception.
#[derive(Clone)]
pub struct RequestInterceptor(Option<AsciiMetadataValue>);

impl Interceptor for RequestInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.0 {
            request.metadata_mut().insert(TOKEN_HEADER, token.clone());
        }
        if !request.metadata().contains_key("grpc-timeout") {
            request.set_timeout(REQUEST_TIMEOUT);
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
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let token = target
            .token
            .as_deref()
            .and_then(|token| MetadataValue::try_from(token).ok());
        Ok(SolidWorksClient::with_interceptor(
            channel,
            RequestInterceptor(token),
        ))
    }

    /// Where the add-in last called is listening, for error messages; None
    /// when none was found.
    pub fn endpoint(&self) -> Option<String> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            request.set_timeout(timeout.min(REQUEST_TIMEOUT));
        }
        Ok(client.get_active_document(request).await?.into_inner())
    }

    pub async fn new_part(
        &self,
        units: proto::UnitsChange,
    ) -> Result<Option<proto::Document>, tonic::Status> {
        let mut client = self.client()?;
        let response = client
            .new_part(proto::NewPartRequest { units: Some(units) })
            .await?;
        Ok(response.into_inner().document)
    }

    pub async fn new_assembly(
        &self,
        units: proto::UnitsChange,
    ) -> Result<Option<proto::Document>, tonic::Status> {
        let mut client = self.client()?;
        let response = client
            .new_assembly(proto::NewAssemblyRequest { units: Some(units) })
            .await?;
        Ok(response.into_inner().document)
    }

    /// The active part's (or assembly's) tessellation; coordinates in meters.
    pub async fn get_mesh(
        &self,
        request: proto::GetMeshRequest,
    ) -> Result<proto::GetMeshResponse, tonic::Status> {
        // A detailed part's mesh is far past tonic's default 4 MB cap.
        let mut client = self.client()?.max_decoding_message_size(MAX_MESH_MESSAGE);
        let response = client.get_mesh(request).await?;
        Ok(response.into_inner())
    }

    /// The active document's 3D annotations (PMI); meters / radians.
    pub async fn inspect_annotations(
        &self,
    ) -> Result<proto::InspectAnnotationsResponse, tonic::Status> {
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
        let mut client = self.client()?.max_decoding_message_size(MAX_MESH_MESSAGE);
        Ok(client.inspect_drawing(request).await?.into_inner())
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
}

/// The RPCs that only send their request and return the reply: one line
/// each, with what the wire's numbers mean.
macro_rules! rpcs {
    ($($(#[$doc:meta])* $name:ident($request:ty) -> $response:ty;)*) => {
        impl AddinClient {
            $(
                $(#[$doc])*
                pub async fn $name(&self, request: $request) -> Result<$response, tonic::Status> {
                    let mut client = self.client()?;
                    Ok(client.$name(request).await?.into_inner())
                }
            )*
        }
    };
}

rpcs! {
    set_document_units(proto::SetDocumentUnitsRequest) -> proto::SetDocumentUnitsResponse;
    /// Paper size in meters.
    new_drawing(proto::NewDrawingRequest) -> proto::NewDrawingResponse;
    /// Lengths in meters.
    create_rectangle_sketch(proto::CreateRectangleSketchRequest) -> proto::CreateRectangleSketchResponse;
    /// Coordinates in meters.
    add_sketch_entities(proto::AddSketchEntitiesRequest) -> proto::AddSketchEntitiesResponse;
    /// Depth in meters.
    extrude_sketch(proto::ExtrudeSketchRequest) -> proto::ExtrudeSketchResponse;
    /// Angle in radians.
    revolve_sketch(proto::RevolveSketchRequest) -> proto::RevolveSketchResponse;
    /// Diameter in meters, twist in radians.
    sweep_sketch(proto::SweepSketchRequest) -> proto::SweepSketchResponse;
    loft_sketch(proto::LoftSketchRequest) -> proto::LoftSketchResponse;
    /// Radius in meters.
    fillet_edges(proto::FilletEdgesRequest) -> proto::EdgeFeatureResponse;
    /// Distances in meters, angle in radians.
    chamfer_edges(proto::ChamferEdgesRequest) -> proto::EdgeFeatureResponse;
    /// Sizes in meters, angles in radians.
    create_helix(proto::CreateHelixRequest) -> proto::CreateHelixResponse;
    /// Thicknesses in meters.
    shell_body(proto::ShellBodyRequest) -> proto::EdgeFeatureResponse;
    /// Thickness in meters, draft in radians.
    rib_sketch(proto::RibSketchRequest) -> proto::RibSketchResponse;
    /// Angle in radians.
    draft_faces(proto::DraftFacesRequest) -> proto::DraftFacesResponse;
    mirror_features(proto::MirrorFeaturesRequest) -> proto::MirrorFeaturesResponse;
    mirror_sketch_entities(proto::MirrorSketchEntitiesRequest) -> proto::MirrorSketchEntitiesResponse;
    /// Lengths in meters.
    offset_sketch_entities(proto::OffsetSketchEntitiesRequest) -> proto::OffsetSketchEntitiesResponse;
    convert_entities(proto::ConvertEntitiesRequest) -> proto::ConvertEntitiesResponse;
    /// Lengths in meters, angles in radians.
    sketch_corners(proto::SketchCornersRequest) -> proto::SketchEditResponse;
    /// Points in meters.
    trim_sketch_entity(proto::TrimSketchEntityRequest) -> proto::SketchEditResponse;
    /// Lengths in meters, angles in radians.
    pattern_features(proto::PatternFeaturesRequest) -> proto::PatternFeaturesResponse;
    /// Lengths in meters, angles in radians.
    pattern_sketch_entities(proto::PatternSketchEntitiesRequest) -> proto::PatternSketchEntitiesResponse;
    /// Lengths in meters.
    inspect_model(proto::InspectModelRequest) -> proto::InspectModelResponse;
    inspect_sketch(proto::InspectSketchRequest) -> proto::InspectSketchResponse;
    inspect_feature(proto::InspectFeatureRequest) -> proto::InspectFeatureResponse;
    /// Values in meters / radians.
    add_sketch_dimensions(proto::AddSketchDimensionsRequest) -> proto::AddSketchDimensionsResponse;
    add_sketch_relations(proto::AddSketchRelationsRequest) -> proto::AddSketchRelationsResponse;
    fully_define_sketch(proto::FullyDefineSketchRequest) -> proto::FullyDefineSketchResponse;
    /// Value in meters / radians.
    set_dimension_value(proto::SetDimensionValueRequest) -> proto::SetDimensionValueResponse;
    /// Deviations in meters / radians.
    set_dimension_tolerance(proto::SetDimensionToleranceRequest) -> proto::SetDimensionToleranceResponse;
    /// Lengths in meters, angles in radians.
    hole_wizard(proto::HoleWizardRequest) -> proto::HoleWizardResponse;
    /// Distance in meters, angle in radians.
    add_mate(proto::AddMateRequest) -> proto::AddMateResponse;
    /// Positions in meters.
    insert_components(proto::InsertComponentsRequest) -> proto::InsertComponentsResponse;
    replace_components(proto::ReplaceComponentsRequest) -> proto::ReplaceComponentsResponse;
    fix_components(proto::FixComponentsRequest) -> proto::FixComponentsResponse;
    /// Moves a component while SolidWorks solves its mates; meters / radians.
    move_component(proto::MoveComponentRequest) -> proto::MoveComponentResponse;
    /// Drives a distance / angle mate of the active assembly through a range
    /// of values, checking interference at each; meters / radians.
    sweep_mate(proto::SweepMateRequest) -> proto::SweepMateResponse;
    /// Nudges components of the active assembly to see how they can move;
    /// meters / radians, everything put back.
    inspect_mobility(proto::InspectMobilityRequest) -> proto::InspectMobilityResponse;
    /// Where the components of the active assembly (or a drawing view's)
    /// overlap; meters.
    check_interference(proto::CheckInterferenceRequest) -> proto::CheckInterferenceResponse;
    measure(proto::MeasureRequest) -> proto::MeasureResponse;
    user_selection(proto::SelectionRequest) -> proto::SelectionResponse;
    get_mass_properties(proto::GetMassPropertiesRequest) -> proto::GetMassPropertiesResponse;
    /// Text positions in sheet meters.
    add_drawing_dimensions(proto::AddDrawingDimensionsRequest) -> proto::AddDrawingDimensionsResponse;
    insert_model_items(proto::InsertModelItemsRequest) -> proto::InsertModelItemsResponse;
    /// Paper size in meters.
    setup_sheet(proto::SetupSheetRequest) -> proto::SetupSheetResponse;
    /// Positions in sheet meters.
    setup_drawing_view(proto::SetupDrawingViewRequest) -> proto::SetupDrawingViewResponse;
    /// Positions in sheet meters.
    derive_drawing_view(proto::DeriveDrawingViewRequest) -> proto::SetupDrawingViewResponse;
    /// Positions in sheet meters.
    shape_drawing_view(proto::ShapeDrawingViewRequest) -> proto::SetupDrawingViewResponse;
    /// Positions in sheet meters (or a view's own coordinates).
    setup_drawing_annotation(proto::SetupDrawingAnnotationRequest) -> proto::SetupDrawingAnnotationResponse;
    /// A feature control frame or datum symbol on the active part or
    /// assembly.
    add_model_annotation(proto::AddModelAnnotationRequest) -> proto::AddModelAnnotationResponse;
    /// A DimXpert dimension on faces of the active part.
    add_pmi_dimension(proto::AddPmiDimensionRequest) -> proto::AddPmiDimensionResponse;
    auto_dimension_scheme(proto::AutoDimensionSchemeRequest) -> proto::AutoDimensionSchemeResponse;
    /// Document properties of the active document by SolidWorks id.
    document_preferences(proto::DocumentPreferencesRequest) -> proto::DocumentPreferencesResponse;
    delete_drawing_annotations(proto::DeleteDrawingAnnotationsRequest) -> proto::DeleteDrawingAnnotationsResponse;
    setup_drawing_table(proto::SetupDrawingTableRequest) -> proto::SetupDrawingTableResponse;
    add_balloons(proto::AddBalloonsRequest) -> proto::AddBalloonsResponse;
    add_center_marks(proto::AddCenterMarksRequest) -> proto::AddCenterMarksResponse;
    add_centerlines(proto::AddCenterlinesRequest) -> proto::AddCenterMarksResponse;
    draw_sheet_format(proto::DrawSheetFormatRequest) -> proto::DrawSheetFormatResponse;
    custom_properties(proto::CustomPropertiesRequest) -> proto::CustomPropertiesResponse;
    part_material(proto::PartMaterialRequest) -> proto::PartMaterialResponse;
    delete_features(proto::DeleteFeaturesRequest) -> proto::DeleteFeaturesResponse;
    delete_sketch_entities(proto::DeleteSketchEntitiesRequest) -> proto::DeleteSketchEntitiesResponse;
    diagnose_model(proto::DiagnoseModelRequest) -> proto::DiagnoseModelResponse;
    edit_extrude(proto::EditExtrudeRequest) -> proto::EditExtrudeResponse;
    edit_mate(proto::EditMateRequest) -> proto::EditMateResponse;
    edit_fillet(proto::EditFilletRequest) -> proto::EditFilletResponse;
    edit_pattern(proto::EditPatternRequest) -> proto::EditPatternResponse;
    edit_sweep(proto::EditSweepRequest) -> proto::EditSweepResponse;
    save_document(proto::SaveDocumentRequest) -> proto::SaveDocumentResponse;
    open_document(proto::OpenDocumentRequest) -> proto::OpenDocumentResponse;
    close_document(proto::CloseDocumentRequest) -> proto::CloseDocumentResponse;
    file_references(proto::FileReferencesRequest) -> proto::FileReferencesResponse;
    undo(proto::UndoRequest) -> proto::UndoResponse;
    run_macro(proto::RunMacroRequest) -> proto::RunMacroResponse;
    set_visibility(proto::SetVisibilityRequest) -> proto::SetVisibilityResponse;
    create_reference_geometry(proto::CreateReferenceGeometryRequest) -> proto::CreateReferenceGeometryResponse;
    configurations(proto::ConfigurationsRequest) -> proto::ConfigurationsResponse;
    set_suppression(proto::SetSuppressionRequest) -> proto::SetSuppressionResponse;
    equations(proto::EquationsRequest) -> proto::EquationsResponse;
    design_table(proto::DesignTableRequest) -> proto::DesignTableResponse;
    rollback(proto::RollbackRequest) -> proto::RollbackResponse;
    reorder_feature(proto::ReorderFeatureRequest) -> proto::ReorderFeatureResponse;
}

/// Largest GetMesh reply accepted: about 5 million triangles with normals.
const MAX_MESH_MESSAGE: usize = 256 * 1024 * 1024;

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn all_calls_get_a_server_deadline_and_authentication() {
        let token = MetadataValue::from_static("test-token");
        let request = RequestInterceptor(Some(token.clone()))
            .call(tonic::Request::new(()))
            .unwrap();
        assert_eq!(request.metadata().get(TOKEN_HEADER), Some(&token));
        assert!(request.metadata().contains_key("grpc-timeout"));
    }

    #[test]
    fn explicit_short_deadline_survives_interception() {
        let mut request = tonic::Request::new(());
        request.set_timeout(Duration::from_millis(250));
        let timeout = request.metadata().get("grpc-timeout").unwrap().clone();
        let request = RequestInterceptor(None).call(request).unwrap();
        assert_eq!(request.metadata().get("grpc-timeout"), Some(&timeout));
    }
}

#[cfg(test)]
#[path = "addin_live_tests.rs"]
mod live_tests;
