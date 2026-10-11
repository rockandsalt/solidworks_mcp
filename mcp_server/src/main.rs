mod addin;
mod inspect;

use std::sync::{Arc, Mutex};

use addin::AddinClient;
use addin::proto;
use inspect::{IdRegistry, MM_PER_M};
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
    #[serde(flatten)]
    placement: SketchPlacement,
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
    #[serde(flatten)]
    placement: SketchPlacement,
}

/// Where to put a sketch other than on a default plane, shared by the sketch tools.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct SketchPlacement {
    /// Sketch on this instead of `plane`: the id (from inspect_model) of a planar face, e.g.
    /// "face:4", or of a reference plane feature.
    #[serde(default)]
    on: Option<String>,
    /// Center as a point in model coordinates [x, y, z], in millimeters; it's projected onto
    /// the sketch plane. Overrides center_x_mm/center_y_mm. When sketching `on` a face without
    /// a center, the shape is centered on the face's bounding box.
    #[serde(default)]
    center_mm: Option<[f64; 3]>,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CutExtrudeSketchParams {
    /// Cut depth, in millimeters. Required unless through_all is set. With mid_plane this is
    /// the total depth, split evenly across both sides of the sketch plane.
    #[serde(default)]
    depth_mm: Option<f64>,
    /// Cut through the entire part instead of to a depth. Defaults to false.
    #[serde(default)]
    through_all: bool,
    /// Feature-tree name of the sketch to cut with, e.g. "Sketch2". Omit to use the most
    /// recently created sketch that no feature uses yet.
    #[serde(default)]
    sketch_name: Option<String>,
    /// Cut in the opposite direction. Ignored with mid_plane. Defaults to false.
    #[serde(default)]
    reverse_direction: bool,
    /// Cut symmetrically to both sides of the sketch plane. Defaults to false.
    #[serde(default)]
    mid_plane: bool,
}

/// What inspect_model lists.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum InspectWhat {
    /// Features and bodies.
    #[default]
    Overview,
    Features,
    Bodies,
    Faces,
    Edges,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum SurfaceKind {
    Plane,
    Cylinder,
    Cone,
    Sphere,
    Torus,
    Bspline,
}

impl From<SurfaceKind> for proto::SurfaceType {
    fn from(kind: SurfaceKind) -> Self {
        match kind {
            SurfaceKind::Plane => proto::SurfaceType::Plane,
            SurfaceKind::Cylinder => proto::SurfaceType::Cylinder,
            SurfaceKind::Cone => proto::SurfaceType::Cone,
            SurfaceKind::Sphere => proto::SurfaceType::Sphere,
            SurfaceKind::Torus => proto::SurfaceType::Torus,
            SurfaceKind::Bspline => proto::SurfaceType::Bspline,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum CurveKind {
    Line,
    Circle,
    Ellipse,
    Bspline,
}

impl From<CurveKind> for proto::CurveType {
    fn from(kind: CurveKind) -> Self {
        match kind {
            CurveKind::Line => proto::CurveType::Line,
            CurveKind::Circle => proto::CurveType::Circle,
            CurveKind::Ellipse => proto::CurveType::Ellipse,
            CurveKind::Bspline => proto::CurveType::Bspline,
        }
    }
}

const DEFAULT_INSPECT_LIMIT: usize = 100;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct InspectModelParams {
    /// What to list: "overview" (default; the feature tree and bodies), "features", "bodies",
    /// "faces" or "edges".
    #[serde(default)]
    what: InspectWhat,
    /// Faces/edges only: restrict to one body, feature or face (edges only), given as an id,
    /// e.g. "body:1", or as a feature name, e.g. "Boss-Extrude1" (the faces that feature created).
    #[serde(default)]
    within: Option<String>,
    /// Faces only: keep only faces of this surface type.
    #[serde(default)]
    surface_type: Option<SurfaceKind>,
    /// Faces only: keep only planar faces whose outward normal points this way (within 1
    /// degree), e.g. [0, 0, 1] for faces facing +Z.
    #[serde(default)]
    normal: Option<[f64; 3]>,
    /// Edges only: keep only edges of this curve type.
    #[serde(default)]
    curve_type: Option<CurveKind>,
    /// Faces/edges only: how many to return. Defaults to 100.
    #[serde(default)]
    limit: Option<usize>,
    /// Faces/edges only: skip this many matching entries first, for paging. Defaults to 0.
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Clone)]
struct SolidWorksTools {
    tool_router: ToolRouter<SolidWorksTools>,
    addin: AddinClient,
    /// Short IDs handed out by inspect_model, shared by every tool call.
    ids: Arc<Mutex<IdRegistry>>,
}

#[tool_router]
impl SolidWorksTools {
    fn new(addin: AddinClient) -> Self {
        Self {
            tool_router: Self::tool_router(),
            addin,
            ids: Arc::default(),
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
                       of the active part document, or `on` a planar face or reference plane \
                       found with inspect_model. The square is centered on the given point \
                       (default: the origin, or the middle of the face). Reports the sketch's \
                       axes in model space. Requires an open part with no sketch being edited; \
                       call create_part first if none is open."
    )]
    async fn create_square_sketch(
        &self,
        Parameters(params): Parameters<SquareSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.rectangle_sketch(
            params.plane,
            params.placement,
            params.center_x_mm,
            params.center_y_mm,
            params.side_length_mm,
            params.side_length_mm,
        )
        .await
    }

    #[tool(
        description = "Create a new sketch containing a rectangle on the Front, Top or Right \
                       plane of the active part document, or `on` a planar face or reference \
                       plane found with inspect_model. Width runs along the sketch's X axis and \
                       height along its Y axis; the result reports those axes in model space. \
                       Centered on the given point (default: the origin, or the middle of the \
                       face). Requires an open part with no sketch being edited; call \
                       create_part first if none is open."
    )]
    async fn create_rectangle_sketch(
        &self,
        Parameters(params): Parameters<RectangleSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.rectangle_sketch(
            params.plane,
            params.placement,
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
            cut: false,
            through_all: false,
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

    #[tool(
        description = "Cut a closed sketch of the active part into the existing solid \
                       (cut-extrude), removing material to a depth or through the whole part. \
                       By default uses the most recently created sketch that isn't used by a \
                       feature yet. Typical use: inspect_model to find a face, \
                       create_rectangle_sketch on it, then this. If SolidWorks reports that the \
                       cut removes nothing, retry with reverse_direction. Requires an open part \
                       with no sketch being edited."
    )]
    async fn cut_extrude_sketch(
        &self,
        Parameters(params): Parameters<CutExtrudeSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        let depth_mm = match (params.through_all, params.depth_mm) {
            (true, _) => 0.0,
            (false, Some(d)) if d.is_finite() && d > 0.0 => d,
            (false, Some(d)) => {
                return Err(McpError::invalid_params(
                    format!("depth must be a positive number of millimeters, got {d}"),
                    None,
                ));
            }
            (false, None) => {
                return Err(McpError::invalid_params(
                    "give depth_mm, or set through_all",
                    None,
                ));
            }
        };
        let sketch_name = params
            .sketch_name
            .map(|name| name.trim().to_string())
            .unwrap_or_default();

        let request = proto::ExtrudeSketchRequest {
            sketch_name,
            depth: depth_mm / MM_PER_M,
            reverse_direction: params.reverse_direction,
            mid_plane: params.mid_plane,
            cut: true,
            through_all: params.through_all,
        };
        let result = self
            .addin
            .extrude_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let how = if params.through_all {
            "through all".to_string()
        } else if params.mid_plane {
            format!("{depth_mm} mm (mid-plane)")
        } else if params.reverse_direction {
            format!("{depth_mm} mm (reversed)")
        } else {
            format!("{depth_mm} mm")
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "created {} by cutting with {} {how}",
            result.feature_name, result.sketch_name
        ))]))
    }

    #[tool(
        description = "Look at the active part without changing it. \
                       what=\"overview\" (default) lists the feature tree (name, type, \
                       parents/children; type ProfileFeature is a sketch) and the bodies (bounding \
                       box, volume, centroid). what=\"faces\" lists faces with their geometry: \
                       planes with outward normal and a point on the plane, cylinders with axis, \
                       radius and whether they're holes, plus area and bounding box. \
                       what=\"edges\" lists edges with type, length, end points, circle \
                       center/radius and the faces they join. Narrow faces/edges with `within` \
                       (a body, a feature name like \"Boss-Extrude1\" for the faces it made, or a \
                       face for its edges) and the filters. Every entity has an id such as \
                       \"face:7\" that other tools accept, e.g. create_rectangle_sketch `on`. IDs \
                       stay valid across edits for as long as the entity exists. All lengths and \
                       coordinates are in millimeters, in the part's model space."
    )]
    async fn inspect_model(
        &self,
        Parameters(params): Parameters<InspectModelParams>,
    ) -> Result<CallToolResult, McpError> {
        let (scope_persist_id, scope_feature_name) = match params.within.as_deref().map(str::trim) {
            None | Some("") => (Vec::new(), String::new()),
            Some(within) if IdRegistry::looks_like_id(within) => {
                (self.resolve_id(within)?, String::new())
            }
            Some(feature_name) => (Vec::new(), feature_name.to_string()),
        };
        if let Some(n) = params.normal
            && !(n.iter().all(|c| c.is_finite()) && n.iter().any(|c| *c != 0.0))
        {
            return Err(McpError::invalid_params(
                "normal must be a non-zero direction like [0, 0, 1]",
                None,
            ));
        }

        let what = params.what;
        let request = proto::InspectModelRequest {
            include_features: matches!(what, InspectWhat::Overview | InspectWhat::Features),
            include_bodies: matches!(what, InspectWhat::Overview | InspectWhat::Bodies),
            include_faces: matches!(what, InspectWhat::Faces),
            include_edges: matches!(what, InspectWhat::Edges),
            scope_persist_id,
            scope_feature_name,
        };
        let reply = self
            .addin
            .inspect_model(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let filters = inspect::Filters {
            surface_type: params.surface_type.map(Into::into),
            normal: params.normal,
            curve_type: params.curve_type.map(Into::into),
            offset: params.offset.unwrap_or(0),
            limit: params.limit.unwrap_or(DEFAULT_INSPECT_LIMIT),
        };
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            inspect::render(&mut ids, &reply, &filters)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }
}

impl SolidWorksTools {
    /// Shared by the square and rectangle tools. Takes millimeters.
    async fn rectangle_sketch(
        &self,
        plane: SketchPlane,
        placement: SketchPlacement,
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
        if !(center_x_mm.is_finite() && center_y_mm.is_finite())
            || placement
                .center_mm
                .is_some_and(|c| !c.iter().all(|v| v.is_finite()))
        {
            return Err(McpError::invalid_params("center must be finite", None));
        }

        let on = placement.on.as_deref().map(str::trim).filter(|on| !on.is_empty());
        let on_persist_id = match on {
            Some(id) => self.resolve_id(id)?,
            None => Vec::new(),
        };
        let request = proto::CreateRectangleSketchRequest {
            plane: proto::Plane::from(plane).into(),
            center_x: center_x_mm / MM_PER_M,
            center_y: center_y_mm / MM_PER_M,
            width: width_mm / MM_PER_M,
            height: height_mm / MM_PER_M,
            on_persist_id,
            center_model: placement.center_mm.map(|[x, y, z]| proto::Vec3 {
                x: x / MM_PER_M,
                y: y / MM_PER_M,
                z: z / MM_PER_M,
            }),
        };
        let sketch = self
            .addin
            .create_rectangle_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let target = match on {
            Some(id) => id.to_string(),
            None => format!("the {plane:?} plane"),
        };
        let mut text = format!(
            "created {}: {width_mm} x {height_mm} mm on {target}",
            sketch.sketch_name
        );
        if let Some(center) = &sketch.center_model {
            text += &format!(", centered at {} mm (model)", inspect::point_mm(center));
        } else {
            text += &format!(", centered at ({center_x_mm}, {center_y_mm}) mm (sketch)");
        }
        if let Some(frame) = &sketch.frame
            && let (Some(x), Some(y), Some(n)) = (&frame.x_axis, &frame.y_axis, &frame.normal)
        {
            text += &format!(
                ". Width runs along {}, height along {}; sketch normal {} (a boss extrudes \
                 this way unless reversed)",
                inspect::direction(x),
                inspect::direction(y),
                inspect::direction(n)
            );
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// The persist ID behind an id handed out by inspect_model.
    fn resolve_id(&self, id: &str) -> Result<Vec<u8>, McpError> {
        let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        ids.persist_id(id).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown id {id:?} -- ids come from inspect_model (and are forgotten if \
                     the MCP server restarts); call it to get current ones"
                ),
                None,
            )
        })
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
                 add-in enabled. Use inspect_model to see what's in the active part \
                 (features, bodies, faces, edges with their geometry) before editing it; \
                 the ids it returns let other tools target specific faces. Lengths are \
                 in millimeters."
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
