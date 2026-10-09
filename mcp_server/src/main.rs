mod addin;

use addin::AddinClient;
use addin::proto;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResult, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_handler,
    tool_router,
};
use serde::Deserialize;
use tonic::Code;
use tracing_subscriber::EnvFilter;

/// How long a client may cache the tool list. The list is fixed for the life
/// of the process, so this only bounds how long a cache outlives a rebuild.
const TOOL_LIST_TTL_MS: u64 = 5 * 60 * 1000;

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExtrudeSketchParams {
    /// Extrusion depth, in millimeters. With mid_plane this is the total depth, split evenly
    /// across both sides of the sketch plane.
    depth_mm: f64,
    /// Feature-tree name of the sketch to extrude, e.g. "Sketch1". Omit to extrude the most
    /// recently created sketch that no feature uses yet.
    #[serde(default)]
    sketch_name: Option<String>,
    /// Extrude in the opposite direction (away from the sketch plane's normal). Ignored with
    /// mid_plane. Defaults to false.
    #[serde(default)]
    reverse_direction: bool,
    /// Extrude symmetrically to both sides of the sketch plane. Defaults to false.
    #[serde(default)]
    mid_plane: bool,
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

    #[tool(
        description = "Extrude a closed sketch of the active part into a solid (boss-extrude, \
                       blind depth), merging with any existing body. By default extrudes the \
                       most recently created sketch that isn't used by a feature yet, e.g. the \
                       one create_square_sketch or create_rectangle_sketch just made. Requires \
                       an open part with no sketch being edited."
    )]
    async fn extrude_sketch(
        &self,
        Parameters(params): Parameters<ExtrudeSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        let depth_mm = params.depth_mm;
        if !(depth_mm.is_finite() && depth_mm > 0.0) {
            return Err(McpError::invalid_params(
                format!("depth must be a positive number of millimeters, got {depth_mm}"),
                None,
            ));
        }
        let sketch_name = params
            .sketch_name
            .map(|name| name.trim().to_string())
            .unwrap_or_default();

        let request = proto::ExtrudeSketchRequest {
            sketch_name,
            depth: depth_mm / MM_PER_M,
            reverse_direction: params.reverse_direction,
            mid_plane: params.mid_plane,
        };
        let result = self
            .addin
            .extrude_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let how = if params.mid_plane {
            " (mid-plane)"
        } else if params.reverse_direction {
            " (reversed)"
        } else {
            ""
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "created {} by extruding {} {depth_mm} mm{how}",
            result.feature_name, result.sketch_name
        ))]))
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
            Code::FailedPrecondition | Code::InvalidArgument | Code::NotFound => {
                status.message().to_string()
            }
            code => format!("sld_mcp add-in error ({code:?}): {}", status.message()),
        };
        McpError::internal_error(message, None)
    }
}

// Use the router built once in new(); the macro default calls
// Self::tool_router() again on every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for SolidWorksTools {
    // Written by hand because the one #[tool_handler] generates leaves out
    // ttlMs and cacheScope. Protocol 2026-07-28 requires both on tools/list,
    // and Claude Code rejects the whole tool list without them.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all())
            .with_ttl_ms(TOOL_LIST_TTL_MS)
            .with_cache_scope(CacheScope::Public))
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // from_build_env() expands rmcp's own Cargo env vars, so it reports "rmcp".
            .with_server_info(Implementation::new("solidworks-mcp", env!("CARGO_PKG_VERSION")))
            // Only a fallback: rmcp answers with whichever known version the
            // client asks for, currently 2026-07-28 for Claude Code.
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
