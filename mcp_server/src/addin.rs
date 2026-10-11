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
}
