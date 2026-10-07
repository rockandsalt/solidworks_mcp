mod addin;

use addin::AddinClient;
use addin::proto;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::stdio;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tonic::Code;
use tracing_subscriber::EnvFilter;

/// One of the three default reference planes every part starts with.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum SketchPlane {
    #[default]
    Front,
    Top,
    Right,
}

impl From<SketchPlane> for proto::Plane {
    fn from(plane: SketchPlane) -> Self {
        match plane {
            SketchPlane::Front => proto::Plane::Front,
            SketchPlane::Top => proto::Plane::Top,
            SketchPlane::Right => proto::Plane::Right,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SquareSketchParams {
    /// Side length of the square, in millimeters.
    side_length_mm: f64,
    /// Plane to sketch on. Defaults to the Front plane.
    #[serde(default)]
    plane: SketchPlane,
    /// X coordinate of the square's center in the sketch, in millimeters. Defaults to 0 (origin).
    #[serde(default)]
    center_x_mm: f64,
    /// Y coordinate of the square's center in the sketch, in millimeters. Defaults to 0 (origin).
    #[serde(default)]
    center_y_mm: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RectangleSketchParams {
    /// Width (sketch X direction), in millimeters.
    width_mm: f64,
    /// Height (sketch Y direction), in millimeters.
    height_mm: f64,
    /// Plane to sketch on. Defaults to the Front plane.
    #[serde(default)]
    plane: SketchPlane,
    /// X coordinate of the rectangle's center in the sketch, in millimeters. Defaults to 0 (origin).
    #[serde(default)]
    center_x_mm: f64,
    /// Y coordinate of the rectangle's center in the sketch, in millimeters. Defaults to 0 (origin).
    #[serde(default)]
    center_y_mm: f64,
}

/// SolidWorks API lengths are in meters; tools take millimeters.
const MM_PER_M: f64 = 1000.0;

#[derive(Clone)]
struct SolidWorksTools {
    tool_router: ToolRouter<SolidWorksTools>,
    addin: AddinClient,
}

#[tool_router]
impl SolidWorksTools {
    fn new(addin: AddinClient) -> Self {
        Self {
            tool_router: Self::tool_router(),
            addin,
        }
    }

    #[tool(description = "Get the name of the currently active SolidWorks document")]
    async fn get_active_document_name(&self) -> Result<CallToolResult, McpError> {
        let document = self
            .addin
            .get_active_document()
            .await
            .map_err(|status| self.addin_error(status))?;

        let name = document
            .map(|doc| doc.title)
            .unwrap_or_else(|| "(no active document)".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(name)]))
    }

    #[tool(
        description = "Create a new, empty SolidWorks part document from the user's default \
                       part template and make it the active document"
    )]
    async fn create_part(&self) -> Result<CallToolResult, McpError> {
        let document = self
            .addin
            .new_part()
            .await
            .map_err(|status| self.addin_error(status))?;

        let title = document.map(|doc| doc.title).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "created part {title}"
        ))]))
    }

    #[tool(
        description = "Create a new sketch containing a square on the Front, Top or Right plane \
                       of the active part document. The square is centered on the given point \
                       (default: the origin). Requires an open part with no sketch being edited; \
                       call create_part first if none is open."
    )]
    async fn create_square_sketch(
        &self,
        Parameters(params): Parameters<SquareSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.rectangle_sketch(
            params.plane,
            params.center_x_mm,
            params.center_y_mm,
            params.side_length_mm,
            params.side_length_mm,
        )
        .await
    }

    #[tool(
        description = "Create a new sketch containing a rectangle on the Front, Top or Right \
                       plane of the active part document. The rectangle is centered on the given \
                       point (default: the origin). Requires an open part with no sketch being \
                       edited; call create_part first if none is open."
    )]
    async fn create_rectangle_sketch(
        &self,
        Parameters(params): Parameters<RectangleSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.rectangle_sketch(
            params.plane,
            params.center_x_mm,
            params.center_y_mm,
            params.width_mm,
            params.height_mm,
        )
        .await
    }
}

impl SolidWorksTools {
    /// Shared by the square and rectangle tools. Takes millimeters.
    async fn rectangle_sketch(
        &self,
        plane: SketchPlane,
        center_x_mm: f64,
        center_y_mm: f64,
        width_mm: f64,
        height_mm: f64,
    ) -> Result<CallToolResult, McpError> {
        for (name, value) in [("width", width_mm), ("height", height_mm)] {
            if !(value.is_finite() && value > 0.0) {
                return Err(McpError::invalid_params(
                    format!("{name} must be a positive number of millimeters, got {value}"),
                    None,
                ));
            }
        }
        if !(center_x_mm.is_finite() && center_y_mm.is_finite()) {
            return Err(McpError::invalid_params("center must be finite", None));
        }

        let request = proto::CreateRectangleSketchRequest {
            plane: proto::Plane::from(plane).into(),
            center_x: center_x_mm / MM_PER_M,
            center_y: center_y_mm / MM_PER_M,
            width: width_mm / MM_PER_M,
            height: height_mm / MM_PER_M,
        };
        let sketch = self
            .addin
            .create_rectangle_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "created {sketch}: {width_mm} x {height_mm} mm on the {plane:?} plane, \
             centered at ({center_x_mm}, {center_y_mm}) mm"
        ))]))
    }

    /// Turns a gRPC failure into an MCP error with a message that tells the
    /// model (and the user) what to actually do about it.
    fn addin_error(&self, status: tonic::Status) -> McpError {
        let message = match status.code() {
            Code::Unavailable => format!(
                "can't reach the sld_mcp add-in at {} -- open SolidWorks with the \
                 sld_mcp add-in enabled ({})",
                self.addin.endpoint(),
                status.message()
            ),
            Code::DeadlineExceeded => {
                "SolidWorks didn't respond in time -- it may be busy or showing a dialog"
                    .to_string()
            }
            // The add-in's message already says what's wrong and what to do.
            Code::FailedPrecondition | Code::InvalidArgument => status.message().to_string(),
            code => format!("sld_mcp add-in error ({code:?}): {}", status.message()),
        };
        McpError::internal_error(message, None)
    }
}

// Use the router built once in new(); the macro default calls
// Self::tool_router() again on every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for SolidWorksTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Bridges to a running SolidWorks instance via the sld_mcp COM add-in, \
                 over gRPC on localhost. Requires SolidWorks to be running with that \
                 add-in enabled."
                    .to_string(),
            )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio transport uses stdout for protocol messages, so all logging
    // must go to stderr -- anything on stdout would corrupt the stream.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let addin = AddinClient::new()?;
    let service = SolidWorksTools::new(addin).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
