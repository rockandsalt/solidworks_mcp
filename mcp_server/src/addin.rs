//! gRPC client for the sld_mcp SolidWorks add-in. The wire contract lives in
//! proto/sld_mcp.proto at the repo root, shared with the C++ side.

use std::time::Duration;

use anyhow::{Context, Result};
use tonic::transport::{Channel, Endpoint};

pub mod proto {
    tonic::include_proto!("sld_mcp.v1");
}

use proto::solid_works_client::SolidWorksClient;

const DEFAULT_PORT: u16 = 50551;
const PORT_ENV: &str = "SLD_MCP_PORT";

/// How long to wait for the add-in to accept a connection. It's on
/// loopback, so anything beyond this means it isn't listening.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on a single call. Calls run on SolidWorks' UI thread, so a
/// long rebuild or a modal dialog can legitimately stall them for a while.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AddinClient {
    client: SolidWorksClient<Channel>,
    endpoint: String,
}

impl AddinClient {
    /// Builds a client without connecting. The underlying HTTP/2 connection
    /// is opened on the first call and then reused by every call after it
    /// (concurrent ones included); if SolidWorks restarts, tonic
    /// reconnects on the next call.
    pub fn new() -> Result<Self> {
        let port = match std::env::var(PORT_ENV) {
            Ok(value) => value
                .parse::<u16>()
                .with_context(|| format!("{PORT_ENV}={value:?} is not a valid port"))?,
            Err(_) => DEFAULT_PORT,
        };
        let endpoint = format!("http://127.0.0.1:{port}");

        let channel = Endpoint::from_shared(endpoint.clone())?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .connect_lazy();

        Ok(Self {
            client: SolidWorksClient::new(channel),
            endpoint,
        })
    }

    /// Where the add-in is expected to be listening, for error messages.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn get_active_document(&self) -> Result<Option<proto::Document>, tonic::Status> {
        // Clients are cheap handles onto the shared channel; tonic needs a
        // &mut one per call.
        let mut client = self.client.clone();
        let response = client
            .get_active_document(proto::GetActiveDocumentRequest {})
            .await?;
        Ok(response.into_inner().document)
    }

    pub async fn new_part(&self) -> Result<Option<proto::Document>, tonic::Status> {
        let mut client = self.client.clone();
        let response = client.new_part(proto::NewPartRequest {}).await?;
        Ok(response.into_inner().document)
    }

    /// Lengths in meters.
    pub async fn create_rectangle_sketch(
        &self,
        request: proto::CreateRectangleSketchRequest,
    ) -> Result<proto::CreateRectangleSketchResponse, tonic::Status> {
        let mut client = self.client.clone();
        let response = client.create_rectangle_sketch(request).await?;
        Ok(response.into_inner())
    }

    /// Coordinates in meters.
    pub async fn add_sketch_entities(
        &self,
        request: proto::AddSketchEntitiesRequest,
    ) -> Result<proto::AddSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client.clone();
        let response = client.add_sketch_entities(request).await?;
        Ok(response.into_inner())
    }

    /// Depth in meters.
    pub async fn extrude_sketch(
        &self,
        request: proto::ExtrudeSketchRequest,
    ) -> Result<proto::ExtrudeSketchResponse, tonic::Status> {
        let mut client = self.client.clone();
        let response = client.extrude_sketch(request).await?;
        Ok(response.into_inner())
    }

    /// Lengths in meters.
    pub async fn inspect_model(
        &self,
        request: proto::InspectModelRequest,
    ) -> Result<proto::InspectModelResponse, tonic::Status> {
        let mut client = self.client.clone();
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
            .client
            .clone()
            .max_decoding_message_size(MAX_MESH_MESSAGE);
        let response = client.get_mesh(request).await?;
        Ok(response.into_inner())
    }

    pub async fn inspect_sketch(
        &self,
        request: proto::InspectSketchRequest,
    ) -> Result<proto::InspectSketchResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.inspect_sketch(request).await?.into_inner())
    }

    /// Values in meters / radians.
    pub async fn add_sketch_dimensions(
        &self,
        request: proto::AddSketchDimensionsRequest,
    ) -> Result<proto::AddSketchDimensionsResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.add_sketch_dimensions(request).await?.into_inner())
    }

    pub async fn add_sketch_relations(
        &self,
        request: proto::AddSketchRelationsRequest,
    ) -> Result<proto::AddSketchRelationsResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.add_sketch_relations(request).await?.into_inner())
    }

    pub async fn fully_define_sketch(
        &self,
        request: proto::FullyDefineSketchRequest,
    ) -> Result<proto::FullyDefineSketchResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.fully_define_sketch(request).await?.into_inner())
    }

    /// Value in meters / radians.
    pub async fn set_dimension_value(
        &self,
        request: proto::SetDimensionValueRequest,
    ) -> Result<proto::SetDimensionValueResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.set_dimension_value(request).await?.into_inner())
    }

    /// Deviations in meters / radians.
    pub async fn set_dimension_tolerance(
        &self,
        request: proto::SetDimensionToleranceRequest,
    ) -> Result<proto::SetDimensionToleranceResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.set_dimension_tolerance(request).await?.into_inner())
    }

    /// Lengths in meters, angles in radians.
    pub async fn hole_wizard(
        &self,
        request: proto::HoleWizardRequest,
    ) -> Result<proto::HoleWizardResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.hole_wizard(request).await?.into_inner())
    }

    /// Distance in meters, angle in radians.
    pub async fn add_mate(
        &self,
        request: proto::AddMateRequest,
    ) -> Result<proto::AddMateResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.add_mate(request).await?.into_inner())
    }

    /// The active document's 3D annotations (PMI); meters / radians.
    pub async fn inspect_annotations(&self) -> Result<proto::InspectAnnotationsResponse, tonic::Status> {
        let mut client = self.client.clone();
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
            .client
            .clone()
            .max_decoding_message_size(MAX_MESH_MESSAGE);
        Ok(client.inspect_drawing(request).await?.into_inner())
    }

    /// Text positions in sheet meters.
    pub async fn add_drawing_dimensions(
        &self,
        request: proto::AddDrawingDimensionsRequest,
    ) -> Result<proto::AddDrawingDimensionsResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.add_drawing_dimensions(request).await?.into_inner())
    }

    pub async fn insert_model_items(
        &self,
        request: proto::InsertModelItemsRequest,
    ) -> Result<proto::InsertModelItemsResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.insert_model_items(request).await?.into_inner())
    }

    pub async fn delete_features(
        &self,
        request: proto::DeleteFeaturesRequest,
    ) -> Result<proto::DeleteFeaturesResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.delete_features(request).await?.into_inner())
    }

    pub async fn delete_sketch_entities(
        &self,
        request: proto::DeleteSketchEntitiesRequest,
    ) -> Result<proto::DeleteSketchEntitiesResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.delete_sketch_entities(request).await?.into_inner())
    }

    pub async fn diagnose_model(
        &self,
        request: proto::DiagnoseModelRequest,
    ) -> Result<proto::DiagnoseModelResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.diagnose_model(request).await?.into_inner())
    }

    /// The image of a whole sheet; can be several MB for a big sheet at a
    /// high resolution.
    pub async fn render_drawing(
        &self,
        request: proto::RenderDrawingRequest,
    ) -> Result<proto::RenderDrawingResponse, tonic::Status> {
        let mut client = self.client.clone().max_decoding_message_size(MAX_MESH_MESSAGE);
        Ok(client.render_drawing(request).await?.into_inner())
    }

    pub async fn edit_extrude(
        &self,
        request: proto::EditExtrudeRequest,
    ) -> Result<proto::EditExtrudeResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.edit_extrude(request).await?.into_inner())
    }

    pub async fn edit_mate(
        &self,
        request: proto::EditMateRequest,
    ) -> Result<proto::EditMateResponse, tonic::Status> {
        let mut client = self.client.clone();
        Ok(client.edit_mate(request).await?.into_inner())
    }
}

/// Largest GetMesh reply accepted: about 5 million triangles with normals.
const MAX_MESH_MESSAGE: usize = 256 * 1024 * 1024;
