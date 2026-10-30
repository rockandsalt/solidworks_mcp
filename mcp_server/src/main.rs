mod addin;
mod assembly;
mod component;
mod delete;
mod diagnose;
mod discovery;
mod drawing;
mod drawing_image;
mod edit;
mod files;
mod fillet;
mod gdt;
mod hole;
mod inspect;
mod interference;
mod loft;
mod mirror;
mod mobility;
mod motion;
mod note;
mod pattern;
mod pmi;
mod properties;
mod refgeom;
mod render;
mod revolve;
mod sheet;
mod sheet_format;
mod sketch;
mod sweep;
mod table;
mod tolerance;
mod units;
mod view;

use std::sync::{Arc, Mutex};

use addin::AddinClient;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use addin::proto;
use inspect::{IdRegistry, Kind, MM_PER_M, SKETCH_KINDS};
use sketch::RefTarget;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult,
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

/// How long the active-document check before each tool call may take. It's a
/// few COM calls; if SolidWorks is too busy for that, the call goes ahead with
/// the document the last check found.
const DOCUMENT_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

/// A point for the sketch_* tools, in millimeters: [x, y] in the sketch's own 2D coordinates,
/// or [x, y, z] in model coordinates, projected onto the sketch plane. All points in one call
/// must use the same form.
type Point = Vec<f64>;

/// Which sketch the sketch_* tools draw in.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct SketchTarget {
    /// Add to this existing sketch, e.g. "Sketch1" (the name an earlier sketch_* or
    /// create_*_sketch call reported, or a ProfileFeature from inspect_model). Omit to create a
    /// new sketch.
    #[serde(default)]
    sketch_name: Option<String>,
    /// New sketch only: the plane to sketch on. Defaults to the Front plane.
    #[serde(default)]
    plane: Option<SketchPlane>,
    /// New sketch only: sketch on this instead of `plane`: the id (from inspect_model) of a
    /// planar face, e.g. "face:4", or of a reference plane feature. Give points as [x, y, z]
    /// model coordinates to place geometry on a face without knowing its sketch axes.
    #[serde(default)]
    on: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SketchLineParams {
    /// Points to connect with straight lines, in order: 2 points draw one line, more draw a
    /// connected chain. Each is [x, y] (sketch) or [x, y, z] (model), in millimeters.
    points: Vec<Point>,
    /// Also draw a line from the last point back to the first, closing the shape (e.g. a
    /// polygon to extrude). Defaults to false.
    #[serde(default)]
    closed: bool,
    /// Draw construction lines (reference only, ignored by features). Defaults to false.
    #[serde(default)]
    construction: bool,
    #[serde(flatten)]
    target: SketchTarget,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SketchCircleParams {
    /// Center, [x, y] (sketch) or [x, y, z] (model), in millimeters.
    center: Point,
    /// Radius in millimeters. Give this or diameter_mm.
    #[serde(default)]
    radius_mm: Option<f64>,
    /// Diameter in millimeters. Give this or radius_mm.
    #[serde(default)]
    diameter_mm: Option<f64>,
    /// Draw a construction circle (reference only, ignored by features). Defaults to false.
    #[serde(default)]
    construction: bool,
    #[serde(flatten)]
    target: SketchTarget,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SketchArcParams {
    /// Where the arc starts, [x, y] (sketch) or [x, y, z] (model), in millimeters.
    start: Point,
    /// Where the arc ends. With `center`, only its direction from the center matters: it's
    /// moved onto the radius set by `start`.
    end: Point,
    /// Center of the arc. Give this or `through`.
    #[serde(default)]
    center: Option<Point>,
    /// A point the arc passes through between start and end (3-point arc). Give this or
    /// `center`. Simplest with model coordinates, since there's no direction to pick.
    #[serde(default)]
    through: Option<Point>,
    /// With `center`: go from start to end clockwise instead of counterclockwise, as seen
    /// looking at the sketch from the side its normal points to. Defaults to false.
    #[serde(default)]
    clockwise: bool,
    /// Draw a construction arc (reference only, ignored by features). Defaults to false.
    #[serde(default)]
    construction: bool,
    #[serde(flatten)]
    target: SketchTarget,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SketchSplineParams {
    /// Points the spline passes through, in order (at least 2, or 3 when closed). Each is
    /// [x, y] (sketch) or [x, y, z] (model), in millimeters.
    points: Vec<Point>,
    /// Make a closed loop back through the first point. Defaults to false.
    #[serde(default)]
    closed: bool,
    /// Draw a construction spline (reference only, ignored by features). Defaults to false.
    #[serde(default)]
    construction: bool,
    #[serde(flatten)]
    target: SketchTarget,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SketchPointParams {
    /// Points to place, each [x, y] (sketch) or [x, y, z] (model), in millimeters.
    points: Vec<Point>,
    #[serde(flatten)]
    target: SketchTarget,
}

/// Most entities or spline points one sketch_* call may draw (the add-in's limit).
const MAX_SKETCH_ITEMS: usize = 1000;

/// Turns tool points into wire points (meters), checking that they're well formed and that a
/// call doesn't mix sketch [x, y] and model [x, y, z] points.
#[derive(Default)]
struct PointConverter {
    /// Settled by the first point: Some(true) for model coordinates.
    model_space: Option<bool>,
}

impl PointConverter {
    fn convert(&mut self, point: &[f64], what: &str) -> Result<proto::Vec3, McpError> {
        let model = match point.len() {
            2 => false,
            3 => true,
            n => {
                return Err(McpError::invalid_params(
                    format!(
                        "{what} has {n} coordinates; give [x, y] in sketch coordinates or \
                         [x, y, z] in model coordinates"
                    ),
                    None,
                ));
            }
        };
        if !point.iter().all(|v| v.is_finite()) {
            return Err(McpError::invalid_params(format!("{what} must be finite"), None));
        }
        match self.model_space {
            Some(previous) if previous != model => {
                return Err(McpError::invalid_params(
                    "don't mix [x, y] sketch points and [x, y, z] model points in one call",
                    None,
                ));
            }
            _ => self.model_space = Some(model),
        }
        Ok(proto::Vec3 {
            x: point[0] / MM_PER_M,
            y: point[1] / MM_PER_M,
            z: point.get(2).copied().unwrap_or(0.0) / MM_PER_M,
        })
    }

    fn convert_all(&mut self, points: &[Point], what: &str) -> Result<Vec<proto::Vec3>, McpError> {
        points
            .iter()
            .enumerate()
            .map(|(i, p)| self.convert(p, &format!("{what} {}", i + 1)))
            .collect()
    }

    fn model_space(&self) -> bool {
        self.model_space.unwrap_or(false)
    }
}

/// Flags an extruded, revolved, swept or lofted sketch that isn't fully defined.
fn sketch_status_note(sketch_name: &str, status: proto::DefinitionStatus) -> String {
    match status {
        proto::DefinitionStatus::Fully | proto::DefinitionStatus::Unspecified => String::new(),
        status => format!(
            " Note: {sketch_name} is {} (dimension_sketch / fully_define_sketch can fix that).",
            sketch::status_name(status)
        ),
    }
}

/// A trimmed, possibly empty name for the wire ("" = the add-in's default).
fn trimmed(name: Option<String>) -> String {
    name.map(|n| n.trim().to_string()).unwrap_or_default()
}

/// Puts "dimension 2: " (or similar) in front of an error's message.
fn prefix_error(error: McpError, what: &str) -> McpError {
    McpError::invalid_params(format!("{what}: {}", error.message), error.data)
}

/// Millimeters -> meters, for a value that must be positive.
fn positive_mm(value_mm: f64) -> Result<f64, McpError> {
    if !(value_mm.is_finite() && value_mm > 0.0) {
        return Err(McpError::invalid_params(
            format!("the value must be a positive number of millimeters, got {value_mm}"),
            None,
        ));
    }
    Ok(sketch::m(value_mm))
}

/// Degrees -> radians, for an angle strictly between 0 and 360.
fn angle_rad(value_deg: f64) -> Result<f64, McpError> {
    if !(value_deg.is_finite() && value_deg > 0.0 && value_deg < 360.0) {
        return Err(McpError::invalid_params(
            format!("an angle must be between 0 and 360 degrees, got {value_deg}"),
            None,
        ));
    }
    Ok(value_deg.to_radians())
}

/// A dimension's value on the wire (meters, or radians for angles), checking the right unit
/// was used.
fn dimension_value(spec: &DimensionSpec) -> Result<Option<f64>, McpError> {
    match (spec.kind == DimensionKind::Angle, spec.value_mm, spec.value_deg) {
        (_, Some(_), Some(_)) => Err(McpError::invalid_params(
            "give value_mm or value_deg, not both",
            None,
        )),
        (true, Some(_), None) => Err(McpError::invalid_params(
            "an angle takes value_deg (degrees), not value_mm",
            None,
        )),
        (false, None, Some(_)) => Err(McpError::invalid_params(
            "only angles take value_deg; give value_mm",
            None,
        )),
        (true, None, Some(d)) => angle_rad(d).map(Some),
        (false, Some(v), None) => positive_mm(v).map(Some),
        (_, None, None) => Ok(None),
    }
}

fn sketch_entity(
    construction: bool,
    geometry: proto::sketch_entity::Geometry,
) -> proto::SketchEntity {
    proto::SketchEntity {
        construction,
        geometry: Some(geometry),
    }
}

/// "1 line", "3 lines".
fn count_of(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
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
    /// Cut along the sketch's normal (the way a boss would grow) instead of against it, and
    /// only that way. Ignored with mid_plane. Defaults to false.
    #[serde(default)]
    reverse_direction: bool,
    /// Cut symmetrically to both sides of the sketch plane. Defaults to false.
    #[serde(default)]
    mid_plane: bool,
}

/// What inspect_model lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum InspectWhat {
    /// Features and bodies (and in an assembly, components and mates).
    #[default]
    Overview,
    Features,
    Bodies,
    Faces,
    Edges,
    /// Assemblies: the components, with where each sits.
    Components,
    /// Assemblies: the mates.
    Mates,
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
    /// What to list: "overview" (default; the feature tree and bodies, plus components and
    /// mates in an assembly), "features", "bodies", "faces", "edges", or for assemblies
    /// "components" or "mates".
    #[serde(default)]
    what: InspectWhat,
    /// Faces/edges: restrict to one body, feature or face (edges only), given as an id, e.g.
    /// "body:1", or (parts only) as a feature name, e.g. "Boss-Extrude1" (the faces that feature
    /// created). Assemblies: a component id, e.g. "component:2", restricts features (that
    /// component's own tree, e.g. its planes), bodies, faces and edges to that component.
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

const DEFAULT_TILE_PX: u32 = 512;
const TILE_PX_RANGE: std::ops::RangeInclusive<u32> = 256..=1024;
const MAX_VIEWS: usize = 9;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct RenderViewsParams {
    /// Views to draw, in order, as a grid. Defaults to ["top", "isometric", "front", "right"]
    /// in a 2x2 grid: top above front and right beside it, like a third-angle drawing.
    #[serde(default)]
    views: Option<Vec<render::View>>,
    /// Entities to color in, each an id from inspect_model ("face:7", "edge:3", "body:1",
    /// "feature:5", or in an assembly "component:2") or a feature name ("Cut-Extrude1", for the
    /// faces it made). Up to 6; each gets its own color, reported in the result.
    #[serde(default)]
    highlight: Vec<String>,
    /// Size of each view in pixels (square), 256 to 1024. Defaults to 512.
    #[serde(default)]
    size_px: Option<u32>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct InspectSketchParams {
    /// The sketch, e.g. "Sketch1". Defaults to the last sketch in the feature tree.
    #[serde(default)]
    sketch_name: Option<String>,
}

/// What a dimension measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DimensionKind {
    /// 1 line: its length.
    Length,
    /// 2 entities: point-point (aligned), point-line (perpendicular), between parallel lines,
    /// or circle/arc to anything (from its center).
    Distance,
    /// 1 line's extent, or the distance between 2 entities, along the sketch's X axis.
    Horizontal,
    /// 1 line's extent, or the distance between 2 entities, along the sketch's Y axis.
    Vertical,
    /// 1 circle or arc.
    Diameter,
    /// 1 circle or arc.
    Radius,
    /// 2 lines (degrees).
    Angle,
}

impl DimensionKind {
    fn name(self) -> &'static str {
        match self {
            DimensionKind::Length => "length",
            DimensionKind::Distance => "distance",
            DimensionKind::Horizontal => "horizontal",
            DimensionKind::Vertical => "vertical",
            DimensionKind::Diameter => "diameter",
            DimensionKind::Radius => "radius",
            DimensionKind::Angle => "angle",
        }
    }
}

impl From<DimensionKind> for proto::DimensionKind {
    fn from(kind: DimensionKind) -> Self {
        match kind {
            DimensionKind::Length => proto::DimensionKind::Length,
            DimensionKind::Distance => proto::DimensionKind::Distance,
            DimensionKind::Horizontal => proto::DimensionKind::Horizontal,
            DimensionKind::Vertical => proto::DimensionKind::Vertical,
            DimensionKind::Diameter => proto::DimensionKind::Diameter,
            DimensionKind::Radius => proto::DimensionKind::Radius,
            DimensionKind::Angle => proto::DimensionKind::Angle,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DimensionSpec {
    /// length (1 line), distance (2 entities), horizontal / vertical (1 line, or 2 entities,
    /// measured along the sketch's X / Y axis), diameter / radius (1 circle or arc), angle
    /// (2 lines).
    kind: DimensionKind,
    /// What it measures: sketch entity ids from inspect_sketch or the sketch_* tools ("line:3",
    /// "point:7", "circle:1"), "origin" for the sketch origin, or an edge/face id from
    /// inspect_model ("edge:12") to measure from the model.
    entities: Vec<String>,
    /// Drive the dimension to this many millimeters, moving the geometry. Omit to keep the
    /// current size and just lock it. Not for angles.
    #[serde(default)]
    value_mm: Option<f64>,
    /// Angles only: drive the angle to this many degrees. Omit to keep the current angle.
    #[serde(default)]
    value_deg: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DimensionSketchParams {
    /// Dimensions to add to one sketch, applied together.
    dimensions: Vec<DimensionSpec>,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the entity ids belong to.
    #[serde(default)]
    sketch_name: Option<String>,
}

/// A geometric relation between sketch entities.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum RelationKind {
    /// Lines, or exactly 2 points (level with each other).
    Horizontal,
    /// Lines, or exactly 2 points (one above the other).
    Vertical,
    /// A point and a point, line, arc or the origin.
    Coincident,
    /// A point and a line: the point sits at the line's middle.
    Midpoint,
    /// 2+ lines on one infinite line.
    Collinear,
    /// 2+ lines.
    Parallel,
    /// 2 lines.
    Perpendicular,
    /// An arc/circle/spline and a line or arc.
    Tangent,
    /// 2+ arcs/circles/points sharing a center.
    Concentric,
    /// 2 arcs/circles: same center and radius.
    Coradial,
    /// 2+ lines (same length) or arcs/circles (same radius).
    Equal,
    /// Lock entities where they are.
    Fixed,
    /// 2 entities and then a line to mirror them about (3 in all).
    Symmetric,
    /// 2 points become one.
    Merge,
}

impl RelationKind {
    fn name(self) -> &'static str {
        match self {
            RelationKind::Horizontal => "horizontal",
            RelationKind::Vertical => "vertical",
            RelationKind::Coincident => "coincident",
            RelationKind::Midpoint => "midpoint",
            RelationKind::Collinear => "collinear",
            RelationKind::Parallel => "parallel",
            RelationKind::Perpendicular => "perpendicular",
            RelationKind::Tangent => "tangent",
            RelationKind::Concentric => "concentric",
            RelationKind::Coradial => "coradial",
            RelationKind::Equal => "equal",
            RelationKind::Fixed => "fixed",
            RelationKind::Symmetric => "symmetric",
            RelationKind::Merge => "merge",
        }
    }
}

impl From<RelationKind> for proto::RelationKind {
    fn from(kind: RelationKind) -> Self {
        match kind {
            RelationKind::Horizontal => proto::RelationKind::Horizontal,
            RelationKind::Vertical => proto::RelationKind::Vertical,
            RelationKind::Coincident => proto::RelationKind::Coincident,
            RelationKind::Midpoint => proto::RelationKind::Midpoint,
            RelationKind::Collinear => proto::RelationKind::Collinear,
            RelationKind::Parallel => proto::RelationKind::Parallel,
            RelationKind::Perpendicular => proto::RelationKind::Perpendicular,
            RelationKind::Tangent => proto::RelationKind::Tangent,
            RelationKind::Concentric => proto::RelationKind::Concentric,
            RelationKind::Coradial => proto::RelationKind::Coradial,
            RelationKind::Equal => proto::RelationKind::Equal,
            RelationKind::Fixed => proto::RelationKind::Fixed,
            RelationKind::Symmetric => proto::RelationKind::Symmetric,
            RelationKind::Merge => proto::RelationKind::Merge,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RelationSpec {
    kind: RelationKind,
    /// The entities, in order: sketch entity ids ("line:3", "point:7"), "origin", or an
    /// edge/face id from inspect_model.
    entities: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AddSketchRelationsParams {
    /// Relations to add to one sketch, applied together.
    relations: Vec<RelationSpec>,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the entity ids belong to.
    #[serde(default)]
    sketch_name: Option<String>,
}

/// How fully_define_sketch lays out the dimensions it adds.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DimensionScheme {
    /// Every dimension from the datum (default).
    #[default]
    Baseline,
    /// Each dimension from the previous feature.
    Chain,
    /// Ordinate dimensions from the datum.
    Ordinate,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FullyDefineSketchParams {
    /// The sketch, e.g. "Sketch1". Defaults to the last sketch in the feature tree.
    #[serde(default)]
    sketch_name: Option<String>,
    /// Add relations (horizontal, vertical, coincident, tangent, ...) wherever the geometry
    /// already satisfies them. Defaults to true.
    #[serde(default = "default_true")]
    relations: bool,
    /// Add dimensions for whatever relations don't fix. Defaults to true.
    #[serde(default = "default_true")]
    dimensions: bool,
    /// How to lay out the dimensions: "baseline" (default), "chain" or "ordinate".
    #[serde(default)]
    scheme: DimensionScheme,
    /// What horizontal dimensions are measured from: "origin" (default), a sketch point or
    /// vertical line id, or a model edge id.
    #[serde(default)]
    horizontal_datum: Option<String>,
    /// What vertical dimensions are measured from: "origin" (default), a sketch point or
    /// horizontal line id, or a model edge id.
    #[serde(default)]
    vertical_datum: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetDimensionParams {
    /// The dimension's full name, e.g. "D1@Sketch1" (inspect_sketch lists them).
    name: String,
    /// New value in millimeters, for lengths, distances, diameters and radii.
    #[serde(default)]
    value_mm: Option<f64>,
    /// New value in degrees, for angles.
    #[serde(default)]
    value_deg: Option<f64>,
}

/// Most entities one dimension takes.
const MAX_DIMENSION_ENTITIES: usize = 3;
/// Most entities one relation takes (equal, parallel, ... across many entities at once;
/// the add-in's limit).
const MAX_RELATION_ENTITIES: usize = 32;
/// Most dimensions or relations per call (the add-in's limit).
const MAX_CONSTRAINTS: usize = 200;

#[derive(Clone)]
struct SolidWorksTools {
    tool_router: ToolRouter<SolidWorksTools>,
    addin: AddinClient,
    /// Short IDs handed out by inspect_model, shared by every tool call.
    ids: Arc<Mutex<IdRegistry>>,
    renderer: Arc<render::Renderer>,
    /// Sheet geometry of the drawing view entities inspect_drawing listed,
    /// for checking and placing drawing dimensions.
    drawings: Arc<Mutex<drawing::Cache>>,
    /// The active document's units, as of the tool call's start (None:
    /// nothing open, or the add-in didn't answer).
    units: Arc<Mutex<Option<proto::DocumentUnits>>>,
}

#[tool_router]
impl SolidWorksTools {
    fn new(addin: AddinClient) -> Self {
        Self {
            tool_router: Self::tool_router(),
            addin,
            ids: Arc::default(),
            renderer: Arc::default(),
            drawings: Arc::default(),
            units: Arc::default(),
        }
    }

    #[tool(
        description = "Get the name of the currently active SolidWorks document, its type and \
                       its file (or that it was never saved)"
    )]
    async fn get_active_document_name(&self) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .get_active_document(None)
            .await
            .map_err(|status| self.addin_error(status))?;

        let name = reply
            .document
            .map(|doc| match &doc.units {
                Some(units) => format!("{}; units {}", files::describe(&doc), units::describe(units)),
                None => files::describe(&doc),
            })
            .unwrap_or_else(|| "(no active document)".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(name)]))
    }

    #[tool(
        description = "Read or change the active document's units: `units` the unit system \
                       (mmgs, ips = inch, cgs, mks), `decimals` the decimal places lengths show, \
                       `drafting_standard` how dimensions, datums and frames are drawn (ansi, \
                       iso, ...). Nothing given: just report them. This is how SolidWorks shows \
                       the document -- its dimensions, tolerances, sheet sizes, what a drawing \
                       prints -- not the tools: they always take and report mm and degrees. Inch \
                       documents also get inch defaults: ANSI Inch holes, ANSI B sheets, ASME \
                       borders and title blocks."
    )]
    async fn document_units(
        &self,
        Parameters(params): Parameters<units::UnitsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.is_empty() {
            let reply = self
                .addin
                .get_active_document(None)
                .await
                .map_err(|status| self.addin_error(status))?;
            let Some(document) = reply.document else {
                return Err(McpError::invalid_params("no document is open in SolidWorks", None));
            };
            return Ok(CallToolResult::success(vec![ContentBlock::text(units::reply_text(&document, None))]));
        }
        let reply = self
            .addin
            .set_document_units(proto::SetDocumentUnitsRequest { units: Some(units::change(&params)?) })
            .await
            .map_err(|status| self.addin_error(status))?;
        let document = reply.document.unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(units::reply_text(&document, reply.before.as_ref()))]))
    }

    #[tool(
        description = "Save the active document (part, assembly or drawing), without any \
                       SolidWorks dialogs. No path: save it to its own file (a document never \
                       saved needs a path). With an absolute path, the extension picks what \
                       happens: .SLDPRT / .SLDASM / .SLDDRW = Save As (the document becomes that \
                       file, unless copy=true); .STEP/.STP, .IGS/.IGES, .X_T/.X_B, .SAT, .STL, \
                       .3MF (parts, assemblies) or .PDF, .DXF, .DWG (drawings) = export a copy \
                       with SolidWorks' own export options. A drawing's PDF has one page per \
                       sheet, every sheet unless pdf_sheets names some; pdf_color=false makes it \
                       black and white. An existing file is only replaced \
                       with overwrite=true. Saving an assembly or drawing to its own SolidWorks \
                       file also saves the changed parts it uses. A part with rebuild errors is \
                       still saved, and the reply says so."
    )]
    async fn save_document(
        &self,
        Parameters(params): Parameters<files::SaveDocumentParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = files::save_request(&params)?;
        let reply = self
            .addin
            .save_document(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        // Save As gives the document another file, and so another key.
        self.track_document(true).await;
        let text = files::save_reply_text(&request, &reply)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Open a SolidWorks part, assembly or drawing file (.SLDPRT, .SLDASM, \
                       .SLDDRW, by absolute path) and make it the active document, without any \
                       SolidWorks dialogs; a document that's already open is just made active \
                       again, so this also switches between open documents. Reports problems \
                       SolidWorks found, e.g. an assembly's missing components. The other tools \
                       then work on it: call inspect_model (or inspect_drawing) first, ids from \
                       another document don't apply to it."
    )]
    async fn open_document(
        &self,
        Parameters(params): Parameters<files::OpenDocumentParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = files::open_request(&params)?;
        let reply = self
            .addin
            .open_document(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = files::open_reply_text(&request, &reply)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Create a new, empty SolidWorks part document from the user's default \
                       part template and make it the active document. `units` (mmgs, ips = \
                       inch, ...), `decimals` and `drafting_standard` set how SolidWorks shows it \
                       (default: the template's); tools still take mm."
    )]
    async fn create_part(
        &self,
        Parameters(params): Parameters<units::UnitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let document = self
            .addin
            .new_part(units::change(&params)?)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(files::new_document_reply_text("part", document.as_ref()))]))
    }

    #[tool(
        description = "Create a new, empty SolidWorks assembly document from the user's default \
                       assembly template and make it the active document; insert_components puts \
                       parts and subassemblies into it. `units` (mmgs, ips = inch, ...), \
                       `decimals` and `drafting_standard` set how SolidWorks shows it (default: \
                       the template's); tools still take mm."
    )]
    async fn create_assembly(
        &self,
        Parameters(params): Parameters<units::UnitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let document = self
            .addin
            .new_assembly(units::change(&params)?)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(files::new_document_reply_text("assembly", document.as_ref()))]))
    }

    #[tool(
        description = "Create a new SolidWorks drawing document and make it the active document. \
                       `template` picks the drawing template (.drwdot) and `sheet_format` the \
                       sheet's border and title block (.slddrt, e.g. \"a3 - iso\"), each an \
                       absolute path or a name from SolidWorks' template / sheet format folder \
                       (an unknown name lists the ones there are). `paper_size` or \
                       `paper_size_mm` sets the sheet size (default: the sheet format's own \
                       size, else the template's, else A3 -- ANSI B for units=ips); `units` the \
                       drawing's unit system (mmgs, ips = inch, ...), `decimals` and \
                       `drafting_standard` (ansi, iso, ...) how it shows dimensions (default: \
                       the template's; tools still take mm). Reports the sheet as set up. The \
                       drawing starts with no views."
    )]
    async fn create_drawing(
        &self,
        Parameters(params): Parameters<files::CreateDrawingParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = files::new_drawing_request(&params)?;
        let reply = self
            .addin
            .new_drawing(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(files::new_drawing_reply_text(&reply))]))
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
        description = "Draw straight lines in a sketch of the active part: one line between 2 \
                       points, or a connected chain through more (closed=true joins the last \
                       point back to the first, e.g. for a polygon to extrude). Draws into a new \
                       sketch on the Front/Top/Right plane or `on` a planar face, or into an \
                       existing sketch given by sketch_name -- use that to combine lines, arcs \
                       and splines into one profile. Points are [x, y] in sketch coordinates or \
                       [x, y, z] in model coordinates (projected onto the sketch plane), in \
                       millimeters. End points that meet are joined, and the result says how many \
                       closed contours the sketch has (extrude_sketch needs closed ones) and how \
                       its axes map to model space. Requires an open part with no sketch being \
                       edited."
    )]
    async fn sketch_line(
        &self,
        Parameters(params): Parameters<SketchLineParams>,
    ) -> Result<CallToolResult, McpError> {
        let n = params.points.len();
        if n < 2 {
            return Err(McpError::invalid_params("give at least 2 points", None));
        }
        if params.closed && n < 3 {
            return Err(McpError::invalid_params(
                "a closed chain needs at least 3 points",
                None,
            ));
        }
        if n > MAX_SKETCH_ITEMS {
            return Err(McpError::invalid_params(
                format!("at most {MAX_SKETCH_ITEMS} points per call"),
                None,
            ));
        }
        let mut points = PointConverter::default();
        let mut vertices = points.convert_all(&params.points, "point")?;
        if params.closed {
            vertices.push(vertices[0]);
        }
        let entities = vertices
            .windows(2)
            .map(|pair| {
                sketch_entity(
                    params.construction,
                    proto::sketch_entity::Geometry::Line(proto::SketchLine {
                        start: Some(pair[0]),
                        end: Some(pair[1]),
                    }),
                )
            })
            .collect::<Vec<_>>();
        let what = count_of(entities.len(), "line");
        self.draw_sketch(params.target, points.model_space(), entities, what)
            .await
    }

    #[tool(
        description = "Draw a circle in a sketch of the active part, by center and radius or \
                       diameter -- e.g. a hole profile to cut-extrude. Draws into a new sketch on \
                       the Front/Top/Right plane or `on` a planar face, or into an existing sketch \
                       given by sketch_name. The center is [x, y] in sketch coordinates or \
                       [x, y, z] in model coordinates (projected onto the sketch plane), in \
                       millimeters. Requires an open part with no sketch being edited."
    )]
    async fn sketch_circle(
        &self,
        Parameters(params): Parameters<SketchCircleParams>,
    ) -> Result<CallToolResult, McpError> {
        let radius_mm = match (params.radius_mm, params.diameter_mm) {
            (Some(r), None) => r,
            (None, Some(d)) => d / 2.0,
            _ => {
                return Err(McpError::invalid_params(
                    "give exactly one of radius_mm or diameter_mm",
                    None,
                ));
            }
        };
        if !(radius_mm.is_finite() && radius_mm > 0.0) {
            return Err(McpError::invalid_params(
                "the radius/diameter must be a positive number of millimeters",
                None,
            ));
        }
        let mut points = PointConverter::default();
        let center = points.convert(&params.center, "center")?;
        let entity = sketch_entity(
            params.construction,
            proto::sketch_entity::Geometry::Circle(proto::SketchCircle {
                center: Some(center),
                radius: radius_mm / MM_PER_M,
            }),
        );
        let what = format!("a circle of radius {radius_mm} mm");
        self.draw_sketch(params.target, points.model_space(), vec![entity], what)
            .await
    }

    #[tool(
        description = "Draw an arc in a sketch of the active part from `start` to `end`, \
                       defined either by its `center` (counterclockwise unless clockwise=true) or \
                       by a third point it passes `through`. Draws into a new sketch on the \
                       Front/Top/Right plane or `on` a planar face, or into an existing sketch \
                       given by sketch_name -- e.g. to round off a chain of lines into a slot or \
                       a U shape. Points are [x, y] in sketch coordinates or [x, y, z] in model \
                       coordinates (projected onto the sketch plane), in millimeters. End points \
                       that meet other entities are joined. Requires an open part with no sketch \
                       being edited."
    )]
    async fn sketch_arc(
        &self,
        Parameters(params): Parameters<SketchArcParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut points = PointConverter::default();
        let start = points.convert(&params.start, "start")?;
        let end = points.convert(&params.end, "end")?;
        let (center, through) = match (&params.center, &params.through) {
            (Some(c), None) => (Some(points.convert(c, "center")?), None),
            (None, Some(t)) => {
                if params.clockwise {
                    return Err(McpError::invalid_params(
                        "clockwise only applies with `center`; a 3-point arc's direction comes \
                         from its `through` point",
                        None,
                    ));
                }
                (None, Some(points.convert(t, "through")?))
            }
            _ => {
                return Err(McpError::invalid_params(
                    "give exactly one of center or through",
                    None,
                ));
            }
        };
        let entity = sketch_entity(
            params.construction,
            proto::sketch_entity::Geometry::Arc(proto::SketchArc {
                start: Some(start),
                end: Some(end),
                center,
                through,
                clockwise: params.clockwise,
            }),
        );
        self.draw_sketch(
            params.target,
            points.model_space(),
            vec![entity],
            "an arc".to_string(),
        )
        .await
    }

    #[tool(
        description = "Draw a smooth spline curve through the given points, in order, in a \
                       sketch of the active part (closed=true makes a closed loop). Draws into a \
                       new sketch on the Front/Top/Right plane or `on` a planar face, or into an \
                       existing sketch given by sketch_name. Points are [x, y] in sketch \
                       coordinates or [x, y, z] in model coordinates (projected onto the sketch \
                       plane), in millimeters. Its ends join other entities they meet. Requires \
                       an open part with no sketch being edited."
    )]
    async fn sketch_spline(
        &self,
        Parameters(params): Parameters<SketchSplineParams>,
    ) -> Result<CallToolResult, McpError> {
        let n = params.points.len();
        let min = if params.closed { 3 } else { 2 };
        if n < min {
            return Err(McpError::invalid_params(
                format!("give at least {min} points"),
                None,
            ));
        }
        if n > MAX_SKETCH_ITEMS {
            return Err(McpError::invalid_params(
                format!("at most {MAX_SKETCH_ITEMS} points per spline"),
                None,
            ));
        }
        let mut points = PointConverter::default();
        let spline_points = points.convert_all(&params.points, "point")?;
        let entity = sketch_entity(
            params.construction,
            proto::sketch_entity::Geometry::Spline(proto::SketchSpline {
                points: spline_points,
                closed: params.closed,
            }),
        );
        let what = format!(
            "a {}spline through {n} points",
            if params.closed { "closed " } else { "" }
        );
        self.draw_sketch(params.target, points.model_space(), vec![entity], what)
            .await
    }

    #[tool(
        description = "Place sketch points in a sketch of the active part, e.g. reference \
                       points for later geometry. Draws into a new sketch on the Front/Top/Right \
                       plane or `on` a planar face, or into an existing sketch given by \
                       sketch_name. Points are [x, y] in sketch coordinates or [x, y, z] in model \
                       coordinates (projected onto the sketch plane), in millimeters. Requires an \
                       open part with no sketch being edited."
    )]
    async fn sketch_point(
        &self,
        Parameters(params): Parameters<SketchPointParams>,
    ) -> Result<CallToolResult, McpError> {
        let n = params.points.len();
        if n == 0 {
            return Err(McpError::invalid_params("give at least 1 point", None));
        }
        if n > MAX_SKETCH_ITEMS {
            return Err(McpError::invalid_params(
                format!("at most {MAX_SKETCH_ITEMS} points per call"),
                None,
            ));
        }
        let mut points = PointConverter::default();
        let entities = points
            .convert_all(&params.points, "point")?
            .into_iter()
            .map(|position| {
                sketch_entity(
                    false,
                    proto::sketch_entity::Geometry::Point(proto::SketchPointEntity {
                        position: Some(position),
                    }),
                )
            })
            .collect::<Vec<_>>();
        let what = count_of(n, "point");
        self.draw_sketch(params.target, points.model_space(), entities, what)
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
            "created {} by extruding {} {depth_mm} mm{how}.{}",
            result.feature_name,
            result.sketch_name,
            sketch_status_note(&result.sketch_name, result.sketch_status())
        ))]))
    }

    #[tool(
        description = "Cut a closed sketch of the active part into the existing solid \
                       (cut-extrude), removing material to a depth or through the whole part. \
                       By default uses the most recently created sketch that isn't used by a \
                       feature yet. Typical use: inspect_model to find a face, \
                       create_rectangle_sketch on it, then this. Cuts against the sketch's \
                       normal (into the part, for a sketch on a face); if that would remove \
                       nothing, e.g. a sketch on a plane with the part on its normal side, it \
                       cuts the other way and says so. Requires an open part with no sketch \
                       being edited."
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
        let flipped = if result.direction_flipped {
            " It cut along the sketch normal (reversed): against it there was nothing to remove."
        } else {
            ""
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "created {} by cutting with {} {how}.{flipped}{}",
            result.feature_name,
            result.sketch_name,
            sketch_status_note(&result.sketch_name, result.sketch_status())
        ))]))
    }

    #[tool(
        description = "Revolve a closed sketch of the active part about an axis into a solid \
                       (revolved boss: a shaft, knob, flange, ...), merging with any existing \
                       body. The axis is the sketch's centerline by default: draw the half \
                       profile on one side of a sketch_line with construction=true, in the same \
                       sketch. Or give axis = a sketch line id, a straight edge or a reference \
                       axis. A full turn (360 degrees) unless angle_deg says otherwise. By \
                       default revolves the most recently created sketch that isn't used by a \
                       feature yet. The profile may touch the axis but not cross it. Requires an \
                       open part with no sketch being edited."
    )]
    async fn revolve_sketch(
        &self,
        Parameters(params): Parameters<revolve::RevolveParams>,
    ) -> Result<CallToolResult, McpError> {
        self.revolve(params, false).await
    }

    #[tool(
        description = "Cut by revolving a closed sketch of the active part about an axis \
                       (cut-revolve), removing material: a groove round a shaft, a turned \
                       recess, a chamfer-like profile. Axis, angle and sketch as for \
                       revolve_sketch (default: the sketch's centerline, a full turn); typically \
                       the sketch is on a plane through the part's axis. For a partial turn that \
                       would remove nothing the default way, it cuts the other way and says so. \
                       Requires an open part with no sketch being edited."
    )]
    async fn cut_revolve_sketch(
        &self,
        Parameters(params): Parameters<revolve::RevolveParams>,
    ) -> Result<CallToolResult, McpError> {
        self.revolve(params, true).await
    }

    #[tool(
        description = "Sweep a closed profile sketch of the active part along a path into a solid (swept boss: a handle, pipe, rail, spring, ...), merging with any existing body. Draw the path first (e.g. sketch_line / sketch_arc on Front), then the profile in its own sketch on a plane across the path's start (e.g. Right, or a create_reference_plane through the path's end point), and give path = the path sketch's name. Or skip the profile sketch: circle_diameter_mm sweeps a round section (a rod or wire). By default the profile is the most recently created sketch that isn't used by a feature yet, other than the path. twist_deg turns the profile along the path. Requires an open part with no sketch being edited."
    )]
    async fn sweep_sketch(
        &self,
        Parameters(params): Parameters<sweep::SweepParams>,
    ) -> Result<CallToolResult, McpError> {
        self.sweep(params, false).await
    }

    #[tool(
        description = "Cut by sweeping a closed profile sketch (or a circle, circle_diameter_mm) of the active part along a path (cut-sweep), removing material: a groove that follows a curve, a channel, a thread-like helical cut. Path, profile and twist as for sweep_sketch; the profile must overlap the part where it starts or along the way. Requires an open part with no sketch being edited."
    )]
    async fn cut_sweep_sketch(
        &self,
        Parameters(params): Parameters<sweep::SweepParams>,
    ) -> Result<CallToolResult, McpError> {
        self.sweep(params, true).await
    }

    #[tool(
        description = "Loft through two or more profile sketches of the active part, in order, into a solid (lofted boss: a transition between a square and a round section, a bottle neck, a duct, a blade, a pyramid), merging with any existing body. Draw each profile as one closed contour in its own sketch on its own plane (e.g. Front, and a create_reference_plane offset from it), then give profiles = their sketch names in the order to pass through. The first or last profile may be a sketch holding a single sketch_point (a loft to a tip). guide_curves (sketches touching every profile) shape the sides; closed loops back to the first profile; start_normal / end_normal make the loft leave / meet the end profiles at right angles. Requires an open part with no sketch being edited."
    )]
    async fn loft_sketch(
        &self,
        Parameters(params): Parameters<loft::LoftParams>,
    ) -> Result<CallToolResult, McpError> {
        self.loft(params, false).await
    }

    #[tool(
        description = "Cut by lofting through two or more profile sketches of the active part (cut-loft), removing material: a tapered pocket, a funnel-shaped hole, a transition from a square to a round opening. Profiles, guide_curves, closed and start_normal / end_normal as for loft_sketch; the loft must pass through material. Requires an open part with no sketch being edited."
    )]
    async fn cut_loft_sketch(
        &self,
        Parameters(params): Parameters<loft::LoftParams>,
    ) -> Result<CallToolResult, McpError> {
        self.loft(params, true).await
    }

    #[tool(
        description = "Round edges of the active part with a constant-radius fillet. edges = \
                       edge ids (\"edge:7\") or face ids (\"face:3\": all of that face's edges, \
                       e.g. the top face of a box) from inspect_model (what=\"edges\" / \"faces\"; \
                       render_views with highlight shows which is which). Edges tangent to the \
                       given ones are rounded too unless tangent_propagation=false. Usually the \
                       last step: fillet after the extrudes and cuts that make the edges. \
                       Requires an open part with no sketch being edited."
    )]
    async fn fillet_edges(
        &self,
        Parameters(params): Parameters<fillet::FilletParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            fillet::fillet_request(&ids, &params)?
        };
        let reply = self
            .addin
            .fillet_edges(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(fillet::fillet_text(&params, &reply))]))
    }

    #[tool(
        description = "Bevel edges of the active part with a chamfer. edges = edge ids \
                       (\"edge:7\") or face ids (\"face:3\": all of that face's edges) from \
                       inspect_model. distance_mm alone makes a 45 degree, equal-distance chamfer; \
                       add angle_deg (angle-distance) or distance2_mm (distance-distance) for an \
                       uneven one, and flip to swap which face each is measured on. Edges tangent \
                       to the given ones are beveled too unless tangent_propagation=false. \
                       Requires an open part with no sketch being edited."
    )]
    async fn chamfer_edges(
        &self,
        Parameters(params): Parameters<fillet::ChamferParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            fillet::chamfer_request(&ids, &params)?
        };
        let reply = self
            .addin
            .chamfer_edges(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(fillet::chamfer_text(&params, &reply))]))
    }

    #[tool(
        description = "Mirror features or whole bodies of the active part about a plane. plane = \
                       \"front\" / \"top\" / \"right\", a planar face id (\"face:3\") or a \
                       reference plane (create_reference_plane). features = feature names \
                       (\"Boss-Extrude1\") or ids from inspect_model: copies are built on the \
                       other side of the plane, e.g. a boss, cut or hole drawn on one half of a \
                       symmetric part. Or bodies = body ids (\"body:1\") to mirror whole bodies, \
                       e.g. to finish a half-modelled part about its cut face (merged with it \
                       unless merge=false). To mirror sketch geometry inside a sketch, use \
                       mirror_sketch_entities. Requires an open part with no sketch being edited."
    )]
    async fn mirror_features(
        &self,
        Parameters(params): Parameters<mirror::MirrorFeaturesParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            mirror::features_request(&ids, &params)?
        };
        let noun = if params.features.is_empty() { "body" } else { "feature" };
        let reply = self
            .addin
            .mirror_features(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), noun, &mirror::given(&params)))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(mirror::features_text(&params, &reply))]))
    }

    #[tool(
        description = "Copy features or whole bodies of the active part in a row, or a grid with \
                       direction2, e.g. a row of holes or slots. features = feature names \
                       (\"Cut-Extrude1\") or ids from inspect_model; or bodies = body ids \
                       (\"body:1\"). direction = a straight edge (\"edge:7\"), a sketch line, a \
                       reference axis, or a planar face or plane (\"front\", \"face:3\") to \
                       pattern along its normal; count = instances including the original; \
                       spacing_mm = distance from one to the next. Which way an edge points \
                       isn't obvious: if the copies go the wrong way, delete the pattern and \
                       repeat with reverse_direction. To copy round an axis, use \
                       circular_pattern_features; to copy sketch geometry inside a sketch, \
                       linear_pattern_sketch_entities. Requires an open part with no sketch being \
                       edited."
    )]
    async fn linear_pattern_features(
        &self,
        Parameters(params): Parameters<pattern::LinearPatternParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pattern::linear_request(&ids, &params)?
        };
        let noun = if params.features.is_empty() { "body" } else { "feature" };
        let reply = self
            .addin
            .pattern_features(request)
            .await
            .map_err(|status| {
                delete::with_legend(self.addin_error(status), noun, &pattern::given(&params.features, &params.bodies))
            })?;
        Ok(CallToolResult::success(vec![ContentBlock::text(pattern::linear_text(&params, &reply))]))
    }

    #[tool(
        description = "Copy features or whole bodies of the active part round an axis, e.g. bolt \
                       holes round a flange or spokes of a wheel. features = feature names \
                       (\"Cut-Extrude1\") or ids from inspect_model; or bodies = body ids \
                       (\"body:1\"). axis = a reference axis (\"Axis1\"), a cylindrical face \
                       (\"face:3\", e.g. the bore or the outside of a round part), a straight or \
                       circular edge, or a sketch line; count = instances including the \
                       original, spread evenly all the way round unless angle_deg (the total) or \
                       spacing_deg (between neighbours) says otherwise. To copy in a row, use \
                       linear_pattern_features; to copy sketch geometry inside a sketch, \
                       circular_pattern_sketch_entities. Requires an open part with no sketch \
                       being edited."
    )]
    async fn circular_pattern_features(
        &self,
        Parameters(params): Parameters<pattern::CircularPatternParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pattern::circular_request(&ids, &params)?
        };
        let noun = if params.features.is_empty() { "body" } else { "feature" };
        let reply = self
            .addin
            .pattern_features(request)
            .await
            .map_err(|status| {
                delete::with_legend(self.addin_error(status), noun, &pattern::given(&params.features, &params.bodies))
            })?;
        Ok(CallToolResult::success(vec![ContentBlock::text(pattern::circular_text(&params, &reply))]))
    }

    #[tool(
        description = "Look at the active part or assembly without changing it. \
                       what=\"overview\" (default) lists the feature tree (name, type, \
                       parents/children; type ProfileFeature is a sketch; features that fail to \
                       rebuild carry an `error` or `warning` -- diagnose_model explains them) and \
                       the bodies (bounding box, volume, centroid). what=\"faces\" lists faces with their geometry: \
                       planes with outward normal and a point on the plane, cylinders with axis, \
                       radius and whether they're holes, plus area and bounding box. \
                       what=\"edges\" lists edges with type, length, end points, circle \
                       center/radius and the faces they join. Narrow faces/edges with `within` \
                       (a body, a feature name like \"Boss-Extrude1\" for the faces it made, or a \
                       face for its edges) and the filters. In an assembly the overview also \
                       lists the components (\"component:2\": file, fixed or floating, origin \
                       and axes in the assembly) and the mates; within=\"component:2\" narrows \
                       features (that part's own planes, for mating), bodies, faces and edges to \
                       one component, and every face/edge names its component. Every entity has \
                       an id such as \"face:7\" that other tools accept, e.g. \
                       create_rectangle_sketch `on` or add_mate `entities`. IDs stay valid across \
                       edits for as long as the entity exists. All lengths and coordinates are in \
                       millimeters, in the part's model space or the assembly's space."
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
            include_components: matches!(what, InspectWhat::Overview | InspectWhat::Components),
            include_mates: matches!(what, InspectWhat::Overview | InspectWhat::Mates),
        };
        let mut reply = self
            .addin
            .inspect_model(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        assembly::place_inspect_reply(&mut reply);
        let is_assembly = reply
            .document
            .as_ref()
            .is_some_and(|d| d.r#type() == proto::DocumentType::Assembly);

        let filters = inspect::Filters {
            surface_type: params.surface_type.map(Into::into),
            normal: params.normal,
            curve_type: params.curve_type.map(Into::into),
            offset: params.offset.unwrap_or(0),
            limit: params.limit.unwrap_or(DEFAULT_INSPECT_LIMIT),
        };
        let mut json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            inspect::render(&mut ids, &reply, &filters)
        };
        if let Some(obj) = json.as_object_mut() {
            match what {
                InspectWhat::Components | InspectWhat::Mates if !is_assembly => {
                    obj.insert(
                        "note".into(),
                        serde_json::json!("the active document is a part; only assemblies have components and mates"),
                    );
                }
                InspectWhat::Mates if reply.mates.is_empty() => {
                    obj.insert("mates".into(), serde_json::json!([]));
                }
                _ => {}
            }
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Read the active part's (or assembly's) MBD / PMI: the 3D annotations \
                       made with SolidWorks' MBD Dimension (DimXpert) tools or the annotation \
                       tools, without changing anything. Lists dimensions with their tolerances \
                       (bilateral, limit, symmetric, basic, fits like H7) and nominal values, \
                       datum feature symbols (\"A\", \"B\"), geometric tolerances as feature \
                       control frames (characteristic, tolerance, diameter zone, MMC/LMC \
                       modifiers, datum references, plus a one-line callout such as \"position | \
                       dia 0.1 (M) | A | B | C\"), datum targets, surface finish symbols and notes. \
                       Each annotation lists the faces/edges it's attached to as inspect_model \
                       ids (\"face:7\"), so render_views can highlight them; filter by `kind` \
                       or by an entity with `on`. Dimensions that drive sketches/features are \
                       left out unless include_feature_dimensions is set. Millimeters and \
                       degrees."
    )]
    async fn inspect_annotations(
        &self,
        Parameters(params): Parameters<pmi::InspectAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let on = match params.on.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(id) => Some(self.resolve_id(id)?),
        };
        let reply = self
            .addin
            .inspect_annotations()
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pmi::render(&mut ids, &reply, &params, on.as_deref())
        };
        let json = units::note_text_units(json, self.active_units().as_ref());
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Add a geometric tolerance -- a feature control frame -- to the active part \
                       or assembly (3D PMI / MBD) or drawing. `characteristic`: flatness, \
                       straightness, circularity, cylindricity (form: no datums), \
                       profile_of_a_surface / profile_of_a_line, perpendicularity, parallelism, \
                       angularity (need a datum), position, concentricity, symmetry, \
                       circular_runout / total_runout (need a datum). `tolerance` in mm (written \
                       in the document's unit: 0.002 in an inch document; a text goes as is), \
                       `diameter_zone` (the diameter sign, e.g. a hole's position), `modifier` \
                       (MMC / LMC; none = RFS), `datums` in order of precedence (\"A\", \"B(M)\", \"C\"; \
                       \"A-B\" for a common datum). Attach with `on` -- in a part a face or edge id \
                       from inspect_model (\"face:7\"), in a drawing a view entity from \
                       inspect_drawing (\"view_edge:4\") -- for a leader to it (arrow at \
                       `arrow_at_mm`; a hole's cylindrical face for its position), in a drawing \
                       also `dimension` to put it on a size dimension (\"RD1@Drawing View1\"), or \
                       neither for a free frame. `at_mm` places it ([x, y, z] model mm in a part, [x, y] \
                       sheet mm in a drawing; default: 10 mm out from the arrow). Also \
                       `second_frame` (+ `composite`), `datum_feature` (a datum symbol on the \
                       frame), `projected_zone_mm`. `standard`: iso (ISO 1101) or asme (ASME \
                       Y14.5), the GD&T rules it's checked against (default: the document's \
                       drafting standard's -- ANSI is asme); SolidWorks draws it by the \
                       document's drafting standard (document_units). The frame is read back from SolidWorks; the \
                       reply gives its callout (\"position | dia 0.1 (M) | A | B | C\"), name \
                       (delete_drawing_annotations removes it, in parts too) and where it went."
    )]
    async fn add_geometric_tolerance(
        &self,
        Parameters(params): Parameters<gdt::AddGeometricToleranceParams>,
    ) -> Result<CallToolResult, McpError> {
        let units = self.active_units();
        let (symbol, remarks) = gdt::gtol(&params, units.as_ref())?;
        self.add_gdt(symbol, &params.place, remarks).await
    }

    #[tool(
        description = "Add a datum feature symbol -- the boxed letter on a triangle that makes a \
                       face, edge or feature of size datum A, B, ... -- to the active part, assembly \
                       or drawing, for add_geometric_tolerance's frames to reference. `on`: a face \
                       or edge id from inspect_model in a part (\"face:7\"), a view entity from \
                       inspect_drawing in a drawing (\"view_edge:4\"; a hole's circle) -- a \
                       hole's or shaft's cylindrical face makes its axis the datum. `label`: the letter (default: SolidWorks' next one; refused if \
                       another datum symbol has it), `standard` (iso / asme, as for \
                       add_geometric_tolerance). SolidWorks stands it just off the entity; \
                       `at_mm` moves it ([x, y, z] model mm in a part, [x, y] sheet mm in a \
                       drawing)."
    )]
    async fn add_datum_symbol(
        &self,
        Parameters(params): Parameters<gdt::AddDatumSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        let units = self.active_units();
        let (symbol, remarks) = gdt::datum(&params, units.as_ref())?;
        self.add_gdt(symbol, &params.place, remarks).await
    }

    #[tool(
        description = "Look at the active drawing (.SLDDRW) without changing it: its sheets \
                       (paper size, scale, first/third-angle projection) and their drawing views \
                       (type, orientation such as *Front, scale, the part or assembly shown, the \
                       view's outline on the sheet, which model axes point right/up on the sheet, \
                       and how many dimensions/notes it has). With view=\"Drawing View1\" it also \
                       lists that view's visible edges with ids (\"view_edge:3\"; \"silhouette:1\" \
                       for the outline of a curved face seen side-on, e.g. a cylinder's sides; \
                       vertices with include_vertices) and what each looks like on the sheet: \
                       lines with start/end, circles/arcs seen face-on with center and \
                       diameter/radius, or seen edge-on as a line -- plus the view's dimensions \
                       (name, value, tolerance, text position, the edges they're attached to) and \
                       other annotations. Positions are sheet millimeters from the sheet's \
                       lower-left corner; sizes are the model's true sizes. Use the ids with \
                       add_drawing_dimensions."
    )]
    async fn inspect_drawing(
        &self,
        Parameters(params): Parameters<drawing::InspectDrawingParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .inspect_drawing(drawing::inspect_request(&params))
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            drawing::render_inspect(&mut ids, &mut cache, &reply, &params)
        };
        let json = units::note_text_units(json, self.active_units().as_ref());
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Add dimensions to views of the active drawing, measuring the edges \
                       inspect_drawing listed (call it with the view first for ids). Kinds: length (1 \
                       straight edge), distance (2 entities: between parallel edges, from a point or \
                       a circle's center to an edge, ...), horizontal / vertical (1 edge's extent or \
                       the distance between 2 entities, along the sheet's x / y), diameter / radius \
                       (1 circle or arc seen face-on), angle (2 straight edges). The entities of one \
                       dimension must be in one view. Text goes at at_mm ([x, y] sheet mm) or by \
                       default outside the view on the side nearer the measured edges (above/below \
                       for horizontal, left/right for vertical), next to the circle for \
                       diameter/radius, inside the corner for angles. Drawing dimensions show the model's sizes; \
                       they don't change the model (to change a size, edit the part's own \
                       dimensions). All dimensions of a call are added together, or none if one \
                       fails. To bring in the model's own sketch/feature dimensions instead, use \
                       insert_model_items."
    )]
    async fn add_drawing_dimensions(
        &self,
        Parameters(params): Parameters<drawing::AddDrawingDimensionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let (request, labels) = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            drawing::dimensions_request(&ids, &cache, &params)?
        };
        let reply = self
            .addin
            .add_drawing_dimensions(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            drawing::dimensions_reply_text(&reply, &labels),
        )]))
    }

    #[tool(
        description = "SolidWorks' Model Items for the active drawing: insert the model's own \
                       dimensions (the sketch and feature dimensions that define the part, those \
                       marked for drawing by default) into drawing views, placed where they sit in \
                       the model's sketches. Optionally also hole callouts, Hole Wizard locations, \
                       datums, geometric tolerances, surface finish symbols, notes or cosmetic \
                       threads. Unlike add_drawing_dimensions these stay linked to the model: \
                       changing one in the drawing changes the part. Defaults to every view of the \
                       active sheet; dimensions already shown in another view are skipped unless \
                       include_duplicates. Reports what each view gained."
    )]
    async fn insert_model_items(
        &self,
        Parameters(params): Parameters<drawing::InsertModelItemsParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .insert_model_items(drawing::model_items_request(&params))
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            drawing::render_model_items(&mut ids, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Look at a sheet of the active drawing as SolidWorks prints it: a PNG of the \
                       sheet with rulers in sheet mm along the left and bottom edges and light grid \
                       lines, so positions read the same as inspect_drawing's and at_mm. Use it \
                       after placing views or dimensions to check the layout: overlapping or \
                       crowded dimension text, dimensions crossing views or other dimensions, \
                       things off the sheet or outside the border. `view` zooms to one view (and the \
                       dimensions around it), `region_mm` to a rectangle; defaults to the whole \
                       active sheet (`sheet` for another). Each picture costs about 1-2k tokens; \
                       zoom in when the whole sheet is too small to read."
    )]
    async fn render_drawing(
        &self,
        Parameters(params): Parameters<drawing_image::RenderDrawingParams>,
    ) -> Result<CallToolResult, McpError> {
        use drawing_image as di;
        let max_px = di::max_px(&params)?;
        let region = di::region(&params)?;
        let view = params.view.as_deref().map(str::trim).filter(|v| !v.is_empty());
        let sheet = params.sheet.as_deref().map(str::trim).unwrap_or_default().to_string();
        // The whole sheet: the add-in fits it to max_px. A zoom: the
        // region's resolution, or a guess for a view until its outline is
        // known.
        let (dpi, cap) = match (region, view) {
            (Some(r), _) => (di::dpi_for(r.width_m().max(r.height_m()), max_px), 0),
            (None, Some(_)) => (di::VIEW_DPI, 0),
            (None, None) => (di::MAX_DPI, max_px as i32),
        };
        let request = |sheet_name: String, dpi: i32, max_px: i32| proto::RenderDrawingRequest {
            sheet_name,
            view_name: view.unwrap_or_default().to_string(),
            dpi,
            max_px,
        };
        let mut reply = self
            .addin
            .render_drawing(request(sheet, dpi, cap))
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut image = di::decode(&reply)?;
        let mut crop = di::crop_rect(&reply, view, region)?;
        // Ask again at the resolution the region shown needs.
        if di::needs_other_dpi(di::crop_pixels(&image, &reply, crop), max_px) {
            let better = di::dpi_for(crop.width_m().max(crop.height_m()), max_px);
            if better != reply.dpi {
                reply = self
                    .addin
                    .render_drawing(request(reply.sheet_name.clone(), better, cap))
                    .await
                    .map_err(|status| self.addin_error(status))?;
                image = di::decode(&reply)?;
                crop = di::crop_rect(&reply, view, region)?;
            }
        }
        let picture = di::picture(&image, &reply, crop, max_px)?;
        let document = if reply.document_title.is_empty() { "the drawing" } else { reply.document_title.as_str() };
        let text = di::reply_text(&reply, document, crop, &picture);
        Ok(CallToolResult::success(vec![
            ContentBlock::image(BASE64.encode(&picture.png), "image/png"),
            ContentBlock::text(text),
        ]))
    }

    #[tool(
        description = "Add a sheet to the active drawing and make it the active sheet (the one \
                       render_drawing and insert_model_items work on by default). It starts as a \
                       copy of the active sheet's setup -- paper size, sheet format (border and \
                       title block), scale, projection -- unless given: `paper_size` or \
                       `paper_size_mm`, `sheet_format` (an absolute .slddrt path or a name from \
                       SolidWorks' sheet format folder such as \"a4 - iso\"; without a paper size \
                       the sheet takes the format's own size), `scale` (\"1:2\"), `projection` \
                       (first_angle / third_angle). `name` defaults to the next \"SheetN\". The \
                       new sheet has no views."
    )]
    async fn add_sheet(
        &self,
        Parameters(params): Parameters<sheet::AddSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_sheet(sheet::add_request(&params)?).await
    }

    #[tool(
        description = "Show another sheet of the active drawing, making it the active sheet: \
                       the one render_drawing and insert_model_items work on by default and \
                       SolidWorks shows. Reports its setup and views."
    )]
    async fn activate_sheet(
        &self,
        Parameters(params): Parameters<sheet::ActivateSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_sheet(sheet::activate_request(&params)?).await
    }

    #[tool(
        description = "Change a sheet of the active drawing (default: the active sheet; others \
                       stay in the background): `paper_size` or `paper_size_mm`, `sheet_format` \
                       (border and title block, an absolute .slddrt path or a name from \
                       SolidWorks' sheet format folder such as \"a3 - iso\"; without a paper size \
                       the sheet takes the format's own size), `scale` (\"1:2\"; views using the \
                       sheet scale follow it, and views projected from them), `projection` \
                       (first_angle / third_angle; projected views keep their places and show \
                       what that projection puts there), `rename`. Views keep their positions on \
                       the sheet; the reply lists any that now reach past its edges. Changes \
                       apply together, or not at all."
    )]
    async fn edit_sheet(
        &self,
        Parameters(params): Parameters<sheet::EditSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_sheet(sheet::edit_request(&params)?).await
    }

    #[tool(
        description = "Add a view of a part or assembly to a sheet of the active drawing \
                       (default: the active sheet). `model`: an absolute .SLDPRT / .SLDASM path or \
                       the name of an open one (default: the model the drawing already shows, else \
                       the one open part or assembly; it must be saved). `configuration` (default: \
                       the model's active one). `orientation`: front, back, left, right, top, \
                       bottom, isometric, dimetric, trimetric or a view saved in the model \
                       (default: front). `at_mm`: the view's center, [x, y] sheet mm (default: a \
                       free spot clear of the other views). `scale`: \"1:2\" or \"sheet\" \
                       (default: the sheet's scale; on an empty sheet SolidWorks may rescale the \
                       sheet to fit the model, and says so). `display`: hidden_lines_removed, \
                       hidden_lines_visible, wireframe, shaded, shaded_with_edges. Reports the \
                       view's outline and which model axes point right / up on the sheet. Then \
                       inspect_drawing with the view lists its edges for add_drawing_dimensions."
    )]
    async fn create_drawing_view(
        &self,
        Parameters(params): Parameters<view::CreateDrawingViewParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_drawing_view(view::create_request(&params)?).await
    }

    #[tool(
        description = "Change a view of the active drawing (on any sheet; the active sheet stays \
                       as it is): `model` (show another part or assembly), `configuration`, \
                       `orientation` (front, top, isometric, ... or a view saved in the model; \
                       views projected from it turn with it, a projected view's own direction \
                       follows its parent), `at_mm` (move its center to [x, y] sheet mm; views \
                       aligned with another one only slide along the alignment), `scale` \
                       (\"1:2\", \"sheet\", or \"parent\" for projected views), `display` \
                       (hidden_lines_removed, hidden_lines_visible, wireframe, shaded, \
                       shaded_with_edges, or \"parent\" for projected views), `rename`. Changes \
                       apply together, or not at all."
    )]
    async fn edit_drawing_view(
        &self,
        Parameters(params): Parameters<view::EditDrawingViewParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_drawing_view(view::edit_request(&params)?).await
    }

    #[tool(
        description = "List the annotations of the active drawing -- notes, dimensions, balloons, \
                       ... -- with what each belongs to (`in`): a view (it moves with the view), \
                       the sheet (stays put on the paper) or the sheet format (the border / title \
                       block layer; only counted unless asked for). Each has its name (for \
                       edit_drawing_annotation / delete_drawing_annotations), text, at_mm (the \
                       text's upper-left corner in sheet mm, as render_drawing's rulers read), for a \
                       view's annotations at_view_mm (the view's own coordinates: mm at the model's \
                       size from the model origin), its box on the sheet, style (font, height, \
                       bold, angle, border), layer and leaders (style, arrow tips in both \
                       coordinates, the view edges they're attached to). Filters: sheet, view, in, \
                       kind (note, dimension, ...), contains (text)."
    )]
    async fn inspect_drawing_annotations(
        &self,
        Parameters(params): Parameters<note::InspectDrawingAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .inspect_drawing(note::inspect_request(&params))
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            note::render_inspect(&mut ids, &reply, &params)?
        };
        let json = units::note_text_units(json, self.active_units().as_ref());
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Add a note to the active drawing. Where it belongs (`in`): \"view\" -- in \
                       `view`, moving with it (the default with `view` or `attach_to`); \"sheet\" -- \
                       on the paper, staying put when views move (the default otherwise); \
                       \"sheet_format\" -- the border / title block layer. Position: `at_mm` = the \
                       text's upper-left corner in sheet mm from the sheet's lower-left corner (the \
                       sheet format uses the same coordinates), or for a view note `at_view_mm` = \
                       the view's own coordinates (mm at the model's size from the model origin as \
                       the view shows it, x to the view's right; for a 1:2 view 10 view mm are 5 on \
                       the paper). Leaders: `attach_to` view entity ids from inspect_drawing \
                       (\"view_edge:3\"), one leader each, arrows at `arrow_at_mm` or a line's \
                       middle / a circle's rim; without a position the text goes next to the first \
                       arrow outside the view. Look: height_mm, font, bold, italic, underline, \
                       justification, angle_deg, border (box, circle, ...), layer, leader \
                       (straight, bent, underlined), arrow (closed, open, dot, ...). The reply says \
                       where it ended up in both coordinates and flags notes over a view they \
                       don't belong to, far from their view, or off the sheet."
    )]
    async fn add_drawing_note(
        &self,
        Parameters(params): Parameters<note::AddDrawingNoteParams>,
    ) -> Result<CallToolResult, McpError> {
        let (request, remarks) = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            note::add_request(&ids, &cache, &params)?
        };
        self.setup_drawing_annotation(request, remarks).await
    }

    #[tool(
        description = "Change an annotation of the active drawing, by the name \
                       inspect_drawing_annotations lists (e.g. \"DetailItem12\", \"RD1@Drawing \
                       View1\"). Any annotation: move it (`at_mm` sheet mm, `at_view_mm` in `view`'s \
                       own coordinates, or `move_by_mm`), `layer`, `attach_to_view` (put it in a view \
                       so it moves with it; it stays where it is on the paper). Notes also: `text`, \
                       height_mm, font, bold, italic, underline, document_font (back to the \
                       document's), justification, angle_deg, border, leaders (`attach_to` replaces \
                       them with leaders to these view entities, `arrow_at_mm` moves the arrow tips \
                       in order, `leader` straight / bent / underlined / none, `arrow`). Sheet \
                       format (title block) notes can be changed too. Changes apply together, or \
                       not at all."
    )]
    async fn edit_drawing_annotation(
        &self,
        Parameters(params): Parameters<note::EditDrawingAnnotationParams>,
    ) -> Result<CallToolResult, McpError> {
        let (request, remarks) = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            note::edit_request(&ids, &cache, &params)?
        };
        self.setup_drawing_annotation(request, remarks).await
    }

    #[tool(
        description = "Delete annotations of the active drawing -- notes, dimensions, balloons, \
                       feature control frames, datum symbols, sheet format (title block) notes, ... \
                       -- by the names inspect_drawing_annotations lists; or of the active part or \
                       assembly (frames, datum symbols, ... by the names inspect_annotations \
                       lists). Nothing is deleted if a name isn't found; `sheet` / `view` narrow \
                       the search when a name is on several sheets of a drawing."
    )]
    async fn delete_drawing_annotations(
        &self,
        Parameters(params): Parameters<note::DeleteDrawingAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .delete_drawing_annotations(note::delete_request(&params)?)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(note::delete_reply(&reply))]))
    }

    #[tool(
        description = "List the tables of the active drawing -- bills of materials, hole tables, \
                       general tables, revision tables -- with their name (for edit_drawing_table / \
                       delete_drawing_annotations), kind, anchor corner and box on the sheet (mm), \
                       columns (width, what a BOM / hole column shows) and every row's cells (rows[0] \
                       = the top row); a BOM's rows also name the components they count. Filters: \
                       sheet, name, kind. include_format_lines adds the sheet format's lines (border, \
                       title block grid)."
    )]
    async fn inspect_drawing_tables(
        &self,
        Parameters(params): Parameters<table::InspectDrawingTablesParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .inspect_drawing(table::inspect_request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = table::render_inspect(&reply, &params)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Add a native table to the active drawing. kind=bom: SolidWorks' bill of \
                       materials of an assembly `view` (item numbers, part numbers, quantities kept \
                       up to date with the assembly; bom_type top_level / parts_only / indented; \
                       balloons from add_balloons show its item numbers) -- use it instead of \
                       composing a parts list from notes. kind=hole: a hole table (a coordinate \
                       table) of `holes` (edge ids of hole circles in `view`, from inspect_drawing) \
                       with their X / Y measured from `origin` (a vertex id of that view) and their \
                       size; SolidWorks tags the holes A1, A2, ... kind=general: a table of `rows` of \
                       text you give (coordinate lists, notes tables, ...). Position: `at_mm` + \
                       `anchor` corner, or a free spot clear of the views, other tables and the \
                       title block. Optional template, column_widths_mm, title, layer."
    )]
    async fn add_drawing_table(
        &self,
        Parameters(params): Parameters<table::AddDrawingTableParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            table::add_request(&ids, &params)?
        };
        let mut reply = self
            .addin
            .setup_drawing_table(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        // Not placed: move it to a free spot now that its size is known.
        let mut moved_to = None;
        if request.position.is_none()
            && let (Some(sheet), Some(t)) = (&reply.sheet, &reply.table)
            && let Some(to) = table::relocation(sheet, t, params.kind, Some(request.view_name.as_str()))
        {
            let move_request = proto::SetupDrawingTableRequest {
                name: t.name.clone(),
                sheet_name: sheet.name.clone(),
                position: Some(proto::Vec3 { x: to.x, y: to.y, z: 0.0 }),
                ..Default::default()
            };
            match self.addin.setup_drawing_table(move_request).await {
                Ok(moved) => {
                    let tags = std::mem::take(&mut reply.hole_tags);
                    reply = proto::SetupDrawingTableResponse { created: true, hole_tags: tags, ..moved };
                    moved_to = Some(to);
                }
                Err(status) => tracing::warn!("moving the new table clear failed: {status}"),
            }
        }
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            table::setup_reply(&mut ids, &request, &reply, moved_to)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Change a table of the active drawing by the name inspect_drawing_tables \
                       lists: `cells` [{row, column, text}] (in a BOM, typing over an item number \
                       renumbers it and its balloons follow; computed cells such as quantities or \
                       hole locations may be refused), general tables' add_rows / delete_rows, \
                       column_widths_mm, title (row indices never count it), layer, at_mm / \
                       move_by_mm (the anchor corner), anchor. All or nothing. \
                       Delete a table with delete_drawing_annotations."
    )]
    async fn edit_drawing_table(
        &self,
        Parameters(params): Parameters<table::EditDrawingTableParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut request = table::edit_request(&params)?;
        if let Some(by) = params.move_by_mm {
            let tables = self
                .addin
                .inspect_drawing(table::inspect_request())
                .await
                .map_err(|status| self.addin_error(status))?;
            let found = tables
                .sheets
                .iter()
                .filter(|s| request.sheet_name.is_empty() || s.name.eq_ignore_ascii_case(&request.sheet_name))
                .flat_map(|s| s.tables.iter())
                .find(|t| t.name == request.name)
                .ok_or_else(|| McpError::invalid_params(format!("the drawing has no table named {:?}", request.name), None))?;
            table::apply_move_by(&mut request, found, by)?;
        }
        let reply = self
            .addin
            .setup_drawing_table(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            table::setup_reply(&mut ids, &request, &reply, None)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Add BOM balloons to an assembly view of the active drawing: each points at \
                       a component and shows its item number from the drawing's bill of materials \
                       (add_drawing_table kind=bom first) -- use these instead of notes with \
                       numbers. Default: every component of the view (Auto Balloon; `layout` square \
                       / circle / top / bottom / left / right, every_instance). Or `components` by \
                       name (as inspect_drawing lists them), or `attach_to` view entity ids (the \
                       leader goes to that edge, arrow at arrow_at_mm), each placed at `at_mm`. \
                       Look: style (circle, split_circle, box, ...), size_chars, text (item_number, \
                       quantity, part_number, file_name, custom), lower_text for split circles, \
                       layer. Balloons are notes: move / delete them with edit_drawing_annotation / \
                       delete_drawing_annotations."
    )]
    async fn add_balloons(
        &self,
        Parameters(params): Parameters<table::AddBalloonsParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            table::balloons_request(&ids, &params)?
        };
        let reply = self
            .addin
            .add_balloons(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            table::balloons_reply(&mut ids, &request, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Draw a drawing border into a sheet's format (shown behind the views on \
                       every print): a frame `margins_mm` [left, right, top, bottom] in from the \
                       edges, zones (even counts) with tick marks and labels, centering marks. \
                       Defaults follow `standard`: iso (ISO 5457: margins 20, 10, 10, 10, zones \
                       ~50 mm numbered from the left, lettered from the top) or asme (ASME Y14.1: \
                       0.5 in margins, ~4.25 in zones from the lower right); asme for an inch \
                       drawing. \
                       On its own layer (BORDER): drawing it again replaces it, leaving the rest of \
                       the format alone. Then draw_title_block."
    )]
    async fn draw_sheet_border(
        &self,
        Parameters(params): Parameters<sheet_format::DrawSheetBorderParams>,
    ) -> Result<CallToolResult, McpError> {
        let info = self
            .addin
            .inspect_drawing(table::inspect_request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let sheet = sheet_format::find_sheet(&info, params.sheet.as_deref().unwrap_or("").trim())?;
        let standard = params
            .standard
            .unwrap_or(sheet_format::SheetStandard::for_document(units::is_inch(self.active_units().as_ref())));
        let border = sheet_format::border(glam::DVec2::new(sheet.width, sheet.height), &params, standard)?;
        let reply = self
            .addin
            .draw_sheet_format(sheet_format::border_request(&sheet.name, &border, params.weight))
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(sheet_format::border_reply(&sheet.name, &border, &reply))]))
    }

    #[tool(
        description = "Draw a title block into a sheet's format: a grid of `rows` of cells, each a \
                       small `label` plus a value that shows a custom `property` of the drawing (or \
                       `of`: model -- the sheet's model, e.g. Material) so it updates by itself, or \
                       fixed text (`value` without property). `value` with property sets it. \
                       Default rows: TITLE, DWG NO., MATERIAL, FINISH, SCALE, SHEET, DRAWN / CHECKED \
                       with dates, REV, COMPANY, FILE. Goes in the border's lower-right corner \
                       (corner_mm otherwise), width_mm (default 180; 7 in for `standard` asme, \
                       the default for an inch drawing). On its own layer (TITLE \
                       BLOCK): drawing it again replaces it. Values: fill_title_block."
    )]
    async fn draw_title_block(
        &self,
        Parameters(params): Parameters<sheet_format::DrawTitleBlockParams>,
    ) -> Result<CallToolResult, McpError> {
        let info = self
            .addin
            .inspect_drawing(table::inspect_request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let sheet = sheet_format::find_sheet(&info, params.sheet.as_deref().unwrap_or("").trim())?;
        let frame = sheet_format::frame_of(sheet);
        let corner = match params.corner_mm {
            Some(c) if c.iter().all(|v| v.is_finite()) => glam::DVec2::new(c[0], c[1]) / MM_PER_M,
            Some(_) => return Err(McpError::invalid_params("corner_mm must be finite", None)),
            None => frame
                .map(|(lo, hi)| glam::DVec2::new(hi.x, lo.y))
                .unwrap_or(glam::DVec2::new(sheet.width - 0.01, 0.01)),
        };
        let on_frame = params.corner_mm.is_none() && frame.is_some();
        let standard = params
            .standard
            .unwrap_or(sheet_format::SheetStandard::for_document(units::is_inch(self.active_units().as_ref())));
        let width_mm = params.width_mm.unwrap_or(standard.title_block_width_mm());
        let rows = if params.rows.is_empty() { sheet_format::default_rows(width_mm, standard) } else { params.rows.clone() };
        let block = sheet_format::title_block(
            &rows,
            width_mm / MM_PER_M,
            corner,
            on_frame,
            params.fields.unwrap_or(true),
            standard,
        )?;
        if block.properties.iter().any(|p| p.of == sheet_format::PropertyOf::Model) && sheet.views.is_empty() {
            return Err(McpError::invalid_params(
                format!("{} has no views, so there's no model to set model properties on; add a view first", sheet.name),
                None,
            ));
        }
        for request in sheet_format::property_requests(&sheet.name, &block.properties, false) {
            self.addin.custom_properties(request).await.map_err(|status| self.addin_error(status))?;
        }
        let reply = self
            .addin
            .draw_sheet_format(sheet_format::title_block_request(&sheet.name, &block))
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(sheet_format::title_block_reply(
            sheet,
            &block,
            &reply,
            &block.properties,
        ))]))
    }

    #[tool(
        description = "List a sheet's title block fields: every custom property its sheet format's \
                       notes show (the stock formats' and draw_title_block's alike) -- name, whether \
                       it's the drawing's ($PRP) or the sheet model's ($PRPSHEET), its value, the \
                       note and what it shows. SW-... properties are SolidWorks' own (read-only)."
    )]
    async fn inspect_title_block(
        &self,
        Parameters(params): Parameters<sheet_format::InspectTitleBlockParams>,
    ) -> Result<CallToolResult, McpError> {
        let json = self.title_block_fields(params.sheet.as_deref().unwrap_or("").trim()).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Fill in a sheet's title block: `values` {property: value} for the fields \
                       inspect_title_block lists, e.g. {\"DrawnBy\": \"MW\", \"Description\": \
                       \"Bracket\"}. Each goes where the title block reads it from -- the drawing's \
                       custom properties or the sheet's model's (save the model too) -- and the \
                       notes update. Names the title block doesn't show are refused unless \
                       allow_unlisted. A property linked to what SolidWorks works out (Material = \
                       \"SW-Material@part\": the physical material) is refused unless replace_links. \
                       Part number / revision / material by meaning: drawing_properties."
    )]
    async fn fill_title_block(
        &self,
        Parameters(params): Parameters<sheet_format::FillTitleBlockParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.values.is_empty() {
            return Err(McpError::invalid_params("give `values` to fill in (inspect_title_block lists the fields)", None));
        }
        let sheet_name = params.sheet.as_deref().unwrap_or("").trim().to_string();
        let info = self
            .addin
            .inspect_drawing(sheet_format::fields_request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let sheet = sheet_format::find_sheet(&info, &sheet_name)?;
        let fields = sheet_format::fields(sheet);
        let (values, unlisted) = sheet_format::route_values(&fields, &params.values, params.allow_unlisted)?;
        for request in sheet_format::property_requests(&sheet.name, &values, params.replace_links) {
            self.addin.custom_properties(request).await.map_err(|status| self.addin_error(status))?;
        }
        let set: Vec<String> = values
            .iter()
            .map(|v| {
                let of = if v.of == sheet_format::PropertyOf::Model { "model: save it too" } else { "drawing" };
                format!("{}={:?} ({of})", v.name, v.value)
            })
            .collect();
        let mut lines = vec![format!("set {} for {}'s title block.", set.join(", "), sheet.name)];
        if !unlisted.is_empty() {
            lines.push(format!("No title block note shows {} (set on the drawing anyway).", unlisted.join(", ")));
        }
        let json = self.title_block_fields(&sheet.name).await?;
        lines.push(json.to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(lines.join("\n"))]))
    }

    #[tool(
        description = "Read and set custom properties (File > Properties) of the active document, \
                       of an open `document` by title or path (e.g. a part a BOM lists: its \
                       Description fills the BOM's DESCRIPTION column), or with `sheet` of the model \
                       a drawing sheet takes $PRPSHEET values from. `set` {name: value}, `remove` \
                       [names], `configuration` for configuration-specific ones, `bom_part_number` \
                       for the part number BOMs list. Properties linked to what SolidWorks works out \
                       (Material = \"SW-Material@part\" shows the physical material) are kept \
                       unless replace_links. Lists every property and the BOM part number after \
                       the change. Physical material: part_material."
    )]
    async fn custom_properties(
        &self,
        Parameters(params): Parameters<properties::CustomPropertiesParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .custom_properties(properties::properties_request(&params)?)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(properties::properties_json(&reply).to_string())]))
    }

    #[tool(
        description = "Set the properties a drawing shows, by meaning: `part_number`, `revision`, \
                       `description` and `material` (a designation text, e.g. \"EN AW-6061-T6\") \
                       go where the sheet's title block reads them from -- the drawing ($PRP) or \
                       the model the sheet shows ($PRPSHEET), under the name its note uses (PartNo, \
                       Rev, ...) -- else to the usual property (model PartNo / Description / \
                       Material, drawing Revision). part_number also becomes the model's BOM part \
                       number (bom: false to skip). `drawing` / `model` {name: value} for any other \
                       custom properties. `material` is only text: the part's physical material \
                       (density, mass) is part_material's, and a Material property that shows it \
                       (a link) is kept unless replace_links. Nothing given: lists both sides' \
                       properties. Requires a drawing; save the model too."
    )]
    async fn drawing_properties(
        &self,
        Parameters(params): Parameters<properties::DrawingPropertiesParams>,
    ) -> Result<CallToolResult, McpError> {
        // Title block fields are only read (slow: every annotation is
        // described) when a field is given by meaning.
        let known = properties::has_known(&params);
        let info = self
            .addin
            .inspect_drawing(if known { sheet_format::fields_request() } else { proto::InspectDrawingRequest::default() })
            .await
            .map_err(|status| self.addin_error(status))?;
        let sheet = sheet_format::find_sheet(&info, params.sheet.as_deref().unwrap_or("").trim())?;
        let fields = if known { sheet_format::fields(sheet) } else { Vec::new() };
        let plan = properties::plan(&params, &fields)?;
        if plan.needs_model() && sheet.views.is_empty() {
            return Err(McpError::invalid_params(
                format!("{} has no views, so there's no model to set model properties on; add a view first", sheet.name),
                None,
            ));
        }
        // The model first: a refused link there leaves the drawing untouched.
        let model = if sheet.views.is_empty() {
            None
        } else {
            Some(
                self.addin
                    .custom_properties(properties::plan_request(&plan, sheet_format::PropertyOf::Model, &sheet.name, params.replace_links))
                    .await
                    .map_err(|status| self.addin_error(status))?,
            )
        };
        let drawing = self
            .addin
            .custom_properties(properties::plan_request(&plan, sheet_format::PropertyOf::Drawing, &sheet.name, params.replace_links))
            .await
            .map_err(|status| self.addin_error(status))?;
        let is_part = model.as_ref().and_then(|m| m.document.as_ref()).is_some_and(|d| d.r#type() == proto::DocumentType::Part);
        let material = if is_part {
            self.addin
                .part_material(proto::PartMaterialRequest { sheet_name: sheet.name.clone(), ..Default::default() })
                .await
                .ok()
        } else {
            None
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(properties::drawing_properties_reply(
            &sheet.name,
            &plan,
            &drawing,
            model.as_ref(),
            material.as_ref(),
        ))]))
    }

    #[tool(
        description = "Read or assign a part's physical material -- the material database entry \
                       (e.g. \"AISI 1020\", \"6061-T6 (SS)\") that sets its density, mass, \
                       appearance and hatch -- of the active part, an open `document`, or with \
                       `sheet` the part a drawing sheet shows. `search` lists names (\"*\" the \
                       classes); `remove` takes it off. Not the \"Material\" text a title block shows: \
                       the reply says whether that property follows this material (a link) or is \
                       separate text; link_property makes it follow. Reports density and mass."
    )]
    async fn part_material(
        &self,
        Parameters(params): Parameters<properties::PartMaterialParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = properties::material_request(&params)?;
        let before = self.addin.part_material(request.clone()).await.map_err(|status| self.addin_error(status))?;
        let materials = properties::load_databases(&before.database_folders);
        if let Some(query) = &params.search {
            return Ok(CallToolResult::success(vec![ContentBlock::text(properties::search_reply(&before, &materials, query))]));
        }
        let name = params.material.as_deref().unwrap_or("").trim();
        let database = params.database.as_deref().unwrap_or("").trim();
        let entry = if name.is_empty() { None } else { properties::resolve(&materials, name, database)? };
        let changed = params.remove || !name.is_empty();
        let mut after = if changed {
            let set = proto::PartMaterialRequest {
                material: entry.map(|m| m.name.clone()).unwrap_or_else(|| name.to_string()),
                database: entry.map(|m| m.database.clone()).unwrap_or_else(|| database.to_string()),
                remove: params.remove,
                ..request.clone()
            };
            self.addin.part_material(set).await.map_err(|status| self.addin_error(status))?
        } else {
            before.clone()
        };
        let entry = entry.or_else(|| materials.iter().find(|m| m.name == after.material));
        if params.link_property {
            if after.material.is_empty() {
                return Err(McpError::invalid_params("the part has no material to link; give material", None));
            }
            let part = after.document.as_ref().map(|d| d.title.clone()).unwrap_or_default();
            self.addin
                .custom_properties(proto::CustomPropertiesRequest {
                    document: request.document.clone(),
                    sheet_name: request.sheet_name.clone(),
                    set: vec![proto::CustomProperty {
                        name: "Material".into(),
                        value: properties::material_link(&part),
                        ..Default::default()
                    }],
                    // Replaces a designation text; an existing link is the same link.
                    replace_links: true,
                    ..Default::default()
                })
                .await
                .map_err(|status| self.addin_error(status))?;
            after = self.addin.part_material(request).await.map_err(|status| self.addin_error(status))?;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(properties::material_reply(
            &before,
            &after,
            entry,
            changed,
            params.link_property,
        ))]))
    }

    #[tool(
        description = "Draw the active part or assembly as a picture: shaded, with edges, in \
                       orthographic standard views (front/back/left/right/top/bottom/isometric) at \
                       one shared scale, as a grid in one PNG. Use it to check the overall shape \
                       after edits -- which side a cut went, where a boss landed, where a mate put \
                       a component (highlight \"component:N\") -- and `highlight` \
                       ids from inspect_model to see which face or edge an id is before using \
                       it. Read exact sizes and positions from inspect_model, not the picture. \
                       Doesn't change the model."
    )]
    async fn render_views(
        &self,
        Parameters(params): Parameters<RenderViewsParams>,
    ) -> Result<CallToolResult, McpError> {
        let (views, columns) = match params.views {
            None => (render::View::DEFAULT_GRID.to_vec(), 2),
            Some(views) if views.is_empty() || views.len() > MAX_VIEWS => {
                return Err(McpError::invalid_params(
                    format!("give between 1 and {MAX_VIEWS} views"),
                    None,
                ));
            }
            Some(views) => {
                // As square as possible: 1-3 in a row, 4 as 2x2, 5-6 as 3x2, ...
                let columns = match views.len() {
                    n @ 1..=3 => n,
                    4 => 2,
                    _ => 3,
                };
                (views, columns)
            }
        };
        let tile_px = params.size_px.unwrap_or(DEFAULT_TILE_PX);
        if !TILE_PX_RANGE.contains(&tile_px) {
            return Err(McpError::invalid_params(
                format!(
                    "size_px must be {} to {}",
                    TILE_PX_RANGE.start(),
                    TILE_PX_RANGE.end()
                ),
                None,
            ));
        }
        if params.highlight.len() > render::HIGHLIGHTS.len() {
            return Err(McpError::invalid_params(
                format!("at most {} highlights", render::HIGHLIGHTS.len()),
                None,
            ));
        }
        let highlight: Vec<String> = params
            .highlight
            .iter()
            .map(|h| h.trim().to_string())
            .collect();
        let targets = highlight
            .iter()
            .map(|h| self.highlight_target(h))
            .collect::<Result<Vec<_>, _>>()?;

        let mut mesh = self
            .addin
            .get_mesh(proto::GetMeshRequest {
                include_edges: true,
            })
            .await
            .map_err(|status| self.addin_error(status))?;
        assembly::place_mesh(&mut mesh);
        let (model, matched) = render::model_from_mesh(&mesh, &targets);
        if model.faces.is_empty() {
            return Err(McpError::invalid_params(
                "the active document has nothing to draw yet (no visible bodies or components) \
                 -- create some geometry first",
                None,
            ));
        }
        let triangles = model.triangle_count();
        let edge_count = model.edges.len();

        let request = render::Request {
            model,
            views: views.clone(),
            columns,
            tile_px,
        };
        let rendered = self.renderer.render(request).await.map_err(|err| {
            McpError::internal_error(
                format!(
                    "couldn't render the part: {err:#} (the renderer needs a GPU or a software \
                         Vulkan/DirectX 12 driver)"
                ),
                None,
            )
        })?;

        let mut text = format!(
            "{} view(s) of {} ({} x {} px, {triangles} triangles, {edge_count} edges). Orthographic, \
             same scale in every view: each tile spans {:.1} mm. Layout, row by row:",
            views.len(),
            mesh.document
                .as_ref()
                .map_or("the active document", |d| d.title.as_str()),
            rendered.width,
            rendered.height,
            rendered.tile_span_mm
        );
        for (row, chunk) in views.chunks(columns).enumerate() {
            let tiles: Vec<String> = chunk
                .iter()
                .map(|v| match v.screen_axes() {
                    Some(axes) => format!("{} ({axes})", v.label()),
                    None => v.label().to_string(),
                })
                .collect();
            text += &format!("\n  row {}: {}", row + 1, tiles.join(" | "));
        }
        for ((name, count), (color, _)) in highlight.iter().zip(&matched).zip(render::HIGHLIGHTS) {
            text += &match count {
                0 => format!("\n{name}: not visible (deleted, hidden, or on a hidden body?)"),
                _ => format!("\n{name}: {color}"),
            };
        }
        if mesh.truncated {
            text += "\nwarning: the model is too detailed to draw completely; some faces or edges are missing";
        }
        Ok(CallToolResult::success(vec![
            ContentBlock::image(BASE64.encode(&rendered.png), "image/png"),
            ContentBlock::text(text),
        ]))
    }

    #[tool(
        description = "Look at one 2D sketch of the active part without changing it: every \
                       segment and point with an id (\"line:3\", \"arc:1\", \"circle:2\", \
                       \"point:7\" -- end points and centers are points) and its geometry in the \
                       sketch's own 2D coordinates (mm), the sketch's dimensions (names like \
                       \"D1@Sketch1\" and values) and relations, and whether the sketch is fully \
                       defined; entities that can still move carry a status. Defaults to the last \
                       sketch. Use the ids with dimension_sketch and add_sketch_relations."
    )]
    async fn inspect_sketch(
        &self,
        Parameters(params): Parameters<InspectSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .inspect_sketch(proto::InspectSketchRequest {
                sketch_name: trimmed(params.sketch_name),
            })
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sketch::render_inspect(&mut ids, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Add dimensions to a sketch of the active part -- the way to give a sketch \
                       exact sizes and positions (design intent) and toward making it fully \
                       defined. Each dimension has a kind and the entities it measures: length (1 \
                       line), distance (2 entities), horizontal / vertical (1 line, or 2 entities \
                       along the sketch's X / Y axis), diameter / radius (1 circle or arc), angle \
                       (2 lines). Entities are sketch ids from inspect_sketch or the sketch_* tools \
                       (\"line:3\", \"point:7\"), \"origin\", or an edge/face id from \
                       inspect_model. With value_mm (value_deg for angles) the dimension drives the \
                       geometry to that size; without, it locks the current size. All dimensions of \
                       a call are applied together, or none if any fails (e.g. one would \
                       over-define the sketch). Reports the new dimension names and what in the \
                       sketch can still move. Requires no sketch open for editing."
    )]
    async fn dimension_sketch(
        &self,
        Parameters(params): Parameters<DimensionSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        let n = params.dimensions.len();
        if n == 0 || n > MAX_CONSTRAINTS {
            return Err(McpError::invalid_params(
                format!("give between 1 and {MAX_CONSTRAINTS} dimensions"),
                None,
            ));
        }
        let mut dimensions = Vec::with_capacity(n);
        let mut labels = Vec::with_capacity(n);
        for (i, spec) in params.dimensions.iter().enumerate() {
            let at = |e: McpError| prefix_error(e, &format!("dimension {}", i + 1));
            let value = dimension_value(spec).map_err(at)?;
            let refs = self.sketch_refs(&spec.entities, MAX_DIMENSION_ENTITIES).map_err(at)?;
            dimensions.push(proto::SketchDimension {
                kind: proto::DimensionKind::from(spec.kind).into(),
                refs,
                value,
            });
            labels.push(format!("{} of {}", spec.kind.name(), spec.entities.join(" & ")));
        }
        let reply = self
            .addin
            .add_sketch_dimensions(proto::AddSketchDimensionsRequest {
                sketch_name: trimmed(params.sketch_name),
                dimensions,
            })
            .await
            .map_err(|status| self.addin_error(status))?;

        let added: Vec<String> = reply
            .dimensions
            .iter()
            .zip(&labels)
            .map(|(d, label)| format!("{} ({label})", sketch::dimension_text(d)))
            .collect();
        let mut text = format!("added to {}: {}.", reply.sketch_name, added.join(", "));
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        text += &sketch::state_text(&mut ids, &reply.sketch_name, reply.state.as_ref());
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Add geometric relations between entities of a sketch of the active part: \
                       horizontal, vertical, coincident, midpoint, collinear, parallel, \
                       perpendicular, tangent, concentric, coradial, equal, fixed, symmetric \
                       (2 entities then the line to mirror about), merge. Entities are sketch ids \
                       from inspect_sketch or the sketch_* tools (\"line:3\", \"point:7\"), \
                       \"origin\" (e.g. coincident a circle's center point with the origin), or \
                       an edge/face id from inspect_model. Relations can move geometry to satisfy \
                       them. Applied together; if one fails, the ones added by this call are \
                       removed again. Reports what in the sketch can still move. Requires no \
                       sketch open for editing."
    )]
    async fn add_sketch_relations(
        &self,
        Parameters(params): Parameters<AddSketchRelationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let n = params.relations.len();
        if n == 0 || n > MAX_CONSTRAINTS {
            return Err(McpError::invalid_params(
                format!("give between 1 and {MAX_CONSTRAINTS} relations"),
                None,
            ));
        }
        let mut relations = Vec::with_capacity(n);
        let mut labels = Vec::with_capacity(n);
        for (i, spec) in params.relations.iter().enumerate() {
            let refs = self
                .sketch_refs(&spec.entities, MAX_RELATION_ENTITIES)
                .map_err(|e| prefix_error(e, &format!("relation {}", i + 1)))?;
            relations.push(proto::SketchRelation {
                kind: proto::RelationKind::from(spec.kind).into(),
                refs,
            });
            labels.push(format!("{} {}", spec.kind.name(), spec.entities.join(" & ")));
        }
        let reply = self
            .addin
            .add_sketch_relations(proto::AddSketchRelationsRequest {
                sketch_name: trimmed(params.sketch_name),
                relations,
            })
            .await
            .map_err(|status| self.addin_error(status))?;

        let mut text = format!(
            "added {} to {}: {}.",
            count_of(reply.added.max(0) as usize, "relation"),
            reply.sketch_name,
            labels.join(", ")
        );
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        text += &sketch::state_text(&mut ids, &reply.sketch_name, reply.state.as_ref());
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Fully define a sketch of the active part (SolidWorks' Fully Define \
                       Sketch): adds the relations and dimensions still missing so nothing can \
                       move, locking the geometry where it is. Dimensions are measured from the \
                       sketch origin unless datums are given. Best after dimension_sketch / \
                       add_sketch_relations have set the sizes that matter, since what this adds \
                       just records the current geometry. Defaults to the last sketch. Reports the \
                       dimensions it added and whether the sketch is now fully defined. Requires \
                       no sketch open for editing."
    )]
    async fn fully_define_sketch(
        &self,
        Parameters(params): Parameters<FullyDefineSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        if !params.relations && !params.dimensions {
            return Err(McpError::invalid_params(
                "relations and dimensions can't both be false -- there'd be nothing to add",
                None,
            ));
        }
        let datum = |datum: &Option<String>, what: &str| -> Result<Option<proto::SketchRef>, McpError> {
            match datum.as_deref().map(str::trim) {
                None | Some("") => Ok(None),
                Some(id) => self
                    .sketch_ref(id)
                    .map(Some)
                    .map_err(|e| prefix_error(e, what)),
            }
        };
        let request = proto::FullyDefineSketchRequest {
            sketch_name: trimmed(params.sketch_name),
            skip_relations: !params.relations,
            skip_dimensions: !params.dimensions,
            scheme: match params.scheme {
                DimensionScheme::Baseline => proto::DimensionScheme::Baseline,
                DimensionScheme::Chain => proto::DimensionScheme::Chain,
                DimensionScheme::Ordinate => proto::DimensionScheme::Ordinate,
            }
            .into(),
            horizontal_datum: datum(&params.horizontal_datum, "horizontal_datum")?,
            vertical_datum: datum(&params.vertical_datum, "vertical_datum")?,
        };
        let reply = self
            .addin
            .fully_define_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let dims: Vec<String> = reply.added_dimensions.iter().map(sketch::dimension_text).collect();
        let mut text = if dims.is_empty() && reply.added_relations == 0 {
            format!("added nothing to {}.", reply.sketch_name)
        } else {
            let mut t = format!(
                "added {} and {} to {}",
                count_of(dims.len(), "dimension"),
                count_of(reply.added_relations.max(0) as usize, "relation"),
                reply.sketch_name
            );
            if !dims.is_empty() {
                t += &format!(" ({})", dims.join(", "));
            }
            t.push('.');
            t
        };
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        text += &sketch::state_text(&mut ids, &reply.sketch_name, reply.state.as_ref());
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Change the value of an existing dimension of the active part by its full \
                       name -- a sketch dimension like \"D1@Sketch1\" (inspect_sketch lists them) \
                       or a feature's, like \"D1@Boss-Extrude1\" -- and rebuild. In an assembly, \
                       this changes a distance or angle mate, e.g. \"D1@Distance1\" (inspect_model \
                       lists mates with their dimension names). value_mm for lengths, value_deg \
                       for angles. Requires no sketch open for editing."
    )]
    async fn set_dimension(
        &self,
        Parameters(params): Parameters<SetDimensionParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim().to_string();
        if name.is_empty() {
            return Err(McpError::invalid_params("give the dimension's name, e.g. D1@Sketch1", None));
        }
        let value = match (params.value_mm, params.value_deg) {
            (Some(v), None) => proto::set_dimension_value_request::Value::Length(positive_mm(v)?),
            (None, Some(d)) => proto::set_dimension_value_request::Value::Angle(angle_rad(d)?),
            _ => {
                return Err(McpError::invalid_params(
                    "give exactly one of value_mm (lengths) or value_deg (angles)",
                    None,
                ));
            }
        };
        let reply = self
            .addin
            .set_dimension_value(proto::SetDimensionValueRequest {
                name,
                value: Some(value),
            })
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = match &reply.dimension {
            Some(d) => format!("set {}; the model was rebuilt.", sketch::dimension_text(d)),
            None => "set the dimension; the model was rebuilt.".to_string(),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Set the tolerance of an existing dimension -- in a drawing (the names \
                       inspect_drawing lists: \"RD1@Drawing View1\", or a model dimension like \
                       \"D1@Boss-Extrude1@block.Part\"), a part (\"D1@Sketch1\", \
                       \"D1@Boss-Extrude1\") or an assembly's mate dimension. tolerance = \
                       \"bilateral\" (upper_mm / lower_mm, signed deviations: 0.1 and -0.05 for \
                       40 +0.1/-0.05), \"limit\" (same deviations, shown as the two limits), \
                       \"symmetric\" (plus_minus_mm), \"fit\" / \"fit_with_tolerance\" / \
                       \"fit_tolerance_only\" (hole_fit like \"H7\" and/or shaft_fit like \"g6\"), \
                       \"basic\", \"min\", \"max\" or \"none\" to remove it. Angle dimensions take \
                       the _deg fields instead. Optionally decimals / tolerance_decimals (places \
                       shown). A model dimension shown in a drawing is the model's own: its \
                       tolerance changes in the model too. If SolidWorks refuses part of it, the \
                       old tolerance is put back. Replies with the dimension as inspect_drawing \
                       shows it, including the resulting limits."
    )]
    async fn set_dimension_tolerance(
        &self,
        Parameters(params): Parameters<tolerance::SetToleranceParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = tolerance::request(&params)?;
        let reply = self
            .addin
            .set_dimension_tolerance(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = tolerance::reply_json(&request, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "SolidWorks Hole Wizard: add standard holes sized from a hole standard -- \
                       screw clearance or drilled holes, counterbores, countersinks or tapped \
                       (threaded) holes, e.g. kind=\"counterbore\", size=\"M5\". Usual way, as \
                       done by hand: sketch_point the hole centers on the face, locate them with \
                       dimension_sketch, then call this with that sketch_name -- the sketch \
                       becomes the hole feature's placement sketch, so its dimensions keep \
                       driving the holes. Quick alternative: `on` a planar face id from \
                       inspect_model plus [x, y, z] model-mm `locations` (projected onto the \
                       face; none = one hole mid-face), then dimension the placement sketch the \
                       reply names. Blind to depth_mm, or through_all. All the holes are one \
                       feature. Standard sizes can be overridden (diameter_mm, counterbore_*, \
                       countersink_*, thread_depth_mm). For a non-standard round hole, \
                       sketch_circle + cut_extrude_sketch is simpler. Requires an open part with \
                       no sketch being edited."
    )]
    async fn hole_wizard(
        &self,
        Parameters(params): Parameters<hole::HoleWizardParams>,
    ) -> Result<CallToolResult, McpError> {
        let face_persist_id = match hole::face_id(&params) {
            None => Vec::new(),
            Some(on) if Kind::of_id(on) == Some(Kind::Face) => self.resolve_id(on)?,
            Some(on) => {
                return Err(McpError::invalid_params(
                    format!("`on` must be a face id like \"face:4\" from inspect_model, got {on:?}"),
                    None,
                ));
            }
        };
        let mut params = params;
        let inch = units::is_inch(self.active_units().as_ref());
        params.standard.get_or_insert_with(|| hole::default_standard(&params.size, inch));
        let request = hole::request(&params, face_persist_id)?;
        let reply = self
            .addin
            .hole_wizard(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = hole::reply_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Mate two components of the active assembly, the way the Mate command \
                       does: coincident (faces flush / planes together), concentric (pin in \
                       hole: round faces, circular edges or axes share an axis), parallel, \
                       perpendicular, tangent, distance (distance_mm apart), angle (angle_deg), \
                       or lock (two components held as they are). A limit mate is a distance \
                       or angle mate with a range: min_mm / max_mm (a slider's stroke) or \
                       min_deg / max_deg (how far a lid opens), starting at distance_mm / \
                       angle_deg (default the minimum); sweep_mate then runs it through the \
                       range checking for collisions. `entities` = two ids from \
                       inspect_model on the assembly, on different components: faces or edges \
                       (what=\"faces\" within=\"component:N\" lists a component's faces, in \
                       assembly coordinates), or reference planes/axes as \"feature:N\" (a \
                       component's own Front/Top/Right planes come from what=\"features\" \
                       within=\"component:N\"; the assembly's from its overview). alignment: \
                       \"closest\" (default), \"aligned\" or \"anti_aligned\" (face against \
                       face). SolidWorks then moves the components that aren't fixed. If the \
                       mate is rejected, conflicts with other mates or can't be solved, it's \
                       removed again and the reason reported. Reports where the mated \
                       components ended up; render_views shows the result. Requires an open \
                       assembly with no sketch being edited."
    )]
    async fn add_mate(
        &self,
        Parameters(params): Parameters<assembly::AddMateParams>,
    ) -> Result<CallToolResult, McpError> {
        let entities = assembly::check(&params)?;
        let persist_ids = [
            self.resolve_id(&entities[0]).map_err(|e| prefix_error(e, "entity 1"))?,
            self.resolve_id(&entities[1]).map_err(|e| prefix_error(e, "entity 2"))?,
        ];
        let request = assembly::request(&params, persist_ids);
        let reply = self
            .addin
            .add_mate(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = assembly::reply_text(&mut ids, &params, &entities, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Insert a part, or an assembly as a subassembly, into the active assembly: `file` = an absolute .SLDPRT / .SLDASM path (save a new part with save_document first), or `copy_of` = a component already in it (\"component:N\") for another instance with the same file and configuration. `at` lists where each new component's own origin goes (origin_mm, assembly millimeters) and optionally which way its X / Z axes point; one component per entry, so the same part several times = several entries. Default: one at the assembly origin. The first component of an empty assembly is fixed, the others float until mated (`fixed` overrides). Reports the new components' ids and placements; mate them with add_mate. Requires an open assembly with no sketch being edited; create_assembly makes one."
    )]
    async fn insert_components(
        &self,
        Parameters(params): Parameters<component::InsertComponentsParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = component::request(&params, |id| self.resolve_id(id))?;
        let reply = self
            .addin
            .insert_components(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = component::reply_text(&mut ids, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Replace components of the active assembly with another part or assembly file, like SolidWorks' Replace Components: `components` = the \"component:N\" ids to replace (top-level ones), `file` = the new absolute .SLDPRT / .SLDASM path, `all_instances` = true to also replace every other instance of the same file(s). Each replacement keeps the old component's place in the tree and its position. SolidWorks re-attaches each mate to the matching face / edge / plane of the new file where it can find one (same names or geometry, e.g. a revised version of the part); the replacement stays even when it can't. Reports the new component ids, which mates came through, and the unresolved ones with the reference each lost -- fix those with edit_mate (new entities) or delete_features. `configuration` picks the new file's configuration (default: the same name as before if it has one, else its active one). Requires an open assembly with no sketch being edited."
    )]
    async fn replace_components(
        &self,
        Parameters(params): Parameters<component::ReplaceComponentsParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = component::replace_request(&params, |id| self.resolve_id(id))?;
        let reply = self
            .addin
            .replace_components(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = component::replace_reply_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Fix or float components of the active assembly, like SolidWorks' Fix / Float: `components` = the \"component:N\" ids (inspect_model what=\"components\" lists them and whether each is fixed), `fixed` = true to fix them in place (they can't move; mates move the others to them), false to float them (free to be moved by mates and other tools). Components already in that state are left alone. After the change SolidWorks re-solves the mates: a floated component may move to satisfy its mates, and fixing one can make mates fail (they'd have to move it) -- both reported, the change is kept. Top-level components only: for a part inside a subassembly, fix / float the whole subassembly, or open its file (open_document) and do it there. A rigid subassembly's parts show as fixed exactly when the subassembly is. Requires an open assembly with no sketch being edited."
    )]
    async fn fix_components(
        &self,
        Parameters(params): Parameters<component::FixComponentsParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = component::fix_request(&params, |id| self.resolve_id(id))?;
        let reply = self
            .addin
            .fix_components(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = component::fix_reply_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Move a component of the active assembly while SolidWorks solves its mates, like dragging it: give `translate_mm` (assembly millimeters) and / or a turn (`axis` direction + `angle_deg`, about an axis through `center_mm`, default the component's bounding-box center; the turn happens first). The component goes as far as its mates let it -- the parts of the motion they forbid are dropped -- and components mated to it may follow. The reply gives the requested and the actual pose (origin and turn), the solver's outcome (reached the requested pose / held back by its mates, and how far off / didn't move at all), the other components that moved, and mates that now fail. `restore`=true puts every component back afterwards, to check whether the mates allow a motion (\"does the slider reach 50 mm?\", \"can this lever turn 90 degrees?\") without changing the assembly. Long turns go to the solver in steps (default one per 15 degrees, `steps` to change) so linkages follow the motion. `check_collisions`=true runs an interference check before the move and after every step and reports what comes to overlap on the way (first contact, how long, the biggest overlap); `stop_at_collision`=true stops at the first contact and goes back a step (\"move it until it hits\"). A limit mate holds its component at the mate's current value here (SolidWorks' solver doesn't drag along its range): use sweep_mate for motion along a limit mate. The component must float: fixed ones are refused (fix_components fixed=false first), and so are components inside a subassembly (move the whole subassembly). Requires an open assembly with no sketch being edited."
    )]
    async fn move_component(
        &self,
        Parameters(params): Parameters<component::MoveComponentParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = component::move_request(&params, |id| self.resolve_id(id))?;
        let reply = self
            .addin
            .move_component(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = component::move_reply_text(&mut ids, &params, &request, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Run a distance or angle mate of the active assembly through a range of values and check for collisions along the way, like dragging a mechanism through its travel: a limit mate over its own range by default (min..max), any distance / angle mate over from_mm..to_mm or from_deg..to_deg. At each of the steps + 1 values SolidWorks sets the mate's value, re-solves the assembly and runs an interference check; the reply lists the components that come to overlap (between which values they first touch, how long they overlap, the biggest overlap and where), the ones already overlapping before (left out), what the mate moved, and any value SolidWorks refuses (beyond a limit) or can't solve. Puts everything back afterwards unless restore=false; stop_at_collision=true stops at the first contact (\"how far does the lid open before it hits the hinge?\"). This is how to move along a limit mate: move_component and inspect_mobility treat one as held at its current value. `mate` = \"feature:N\" from inspect_model what=\"mates\" or a name like \"LimitAngle1\". About a second per value (a rebuild and an interference check) plus a few seconds per call; stops after 45 s and says how far it got, so keep steps to about 40 per call. Requires an open assembly with no sketch being edited."
    )]
    async fn sweep_mate(
        &self,
        Parameters(params): Parameters<motion::SweepMateParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            motion::sweep_request(&ids, &params)?
        };
        let reply = self
            .addin
            .sweep_mate(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = motion::sweep_reply_text(&mut ids, &request, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Mobility inspection of the active assembly: each component's remaining degrees of freedom -- how many translations and rotations its mates still leave, and which: the directions it can slide along, the axes it can turn about (direction + a point on the axis, assembly mm; a screw's advance per turn). Use it to find under-defined components before adding mates, or to check that a mechanism moves the way it should (a hinge turns about its pin, a slider only slides). How: every component is nudged a little along and about the assembly X/Y/Z while SolidWorks solves its mates, as dragging it would, then put back -- nothing changes. Also reports which components drag others along (`drags_along` / `dragged_by`: a follower can move only by moving what it's mated to) and SolidWorks' own under / fully / over defined status. `components` narrows it to some top-level \"component:N\" ids (default: all; a subassembly counts as one rigid component); fixed components have 0. Takes under a second per component SolidWorks doesn't already call fully defined; a very big assembly may need several calls (the reply names any left unchecked). Limits: motion only possible beyond the nudge (a gap, a slot's far end) isn't seen; a limit mate counts as fixed at its current value (SolidWorks' solver keeps it there; sweep_mate drives it through its range). Requires an open assembly with no sketch being edited."
    )]
    async fn inspect_mobility(
        &self,
        Parameters(params): Parameters<mobility::InspectMobilityParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = mobility::request(&params, |id| self.resolve_id(id))?;
        let reply = self
            .addin
            .inspect_mobility(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            mobility::reply_json(&mut ids, &request, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Check an assembly for interference, like SolidWorks' Interference \
                       Detection: which components' solids overlap, by how much (volume in \
                       mm³) and where (the box around each overlap, assembly mm), biggest \
                       first. Works on the active assembly, or with a drawing active on the \
                       assembly its views show (`view` picks one if there are several), so a \
                       drawing can be checked before it's issued. `components` narrows it to \
                       interferences involving those (ids from inspect_model, or names like \
                       \"bracket-1\"; a subassembly covers its parts). Components are checked \
                       where they are now (the saved / solved pose); for collisions along a \
                       motion use move_component check_collisions=true or sweep_mate. \
                       include_touching also lists faces that only touch; fasteners in \
                       their holes are flagged, as SolidWorks recognises them. Changes \
                       nothing, though SolidWorks marks the assembly modified afterwards. \
                       Requires no sketch open for editing."
    )]
    async fn check_interference(
        &self,
        Parameters(params): Parameters<interference::CheckInterferenceParams>,
    ) -> Result<CallToolResult, McpError> {
        let wanted = params.wanted(|id| self.resolve_id(id))?;
        let reply = self
            .addin
            .check_interference(params.request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            interference::render(&mut ids, &params, &wanted, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Delete features of the active part or assembly, like selecting them in \
                       the FeatureManager tree and pressing Delete: sketches, extrudes, cuts, \
                       holes, fillets, chamfers, patterns, reference planes/axes, ..., and in an \
                       assembly its mates (and assembly features). `features` = \"feature:N\" \
                       ids from inspect_model (what=\"features\", or what=\"mates\" in an \
                       assembly) or feature-tree names like \"Cut-Extrude1\". If other features \
                       depend on them (a fillet on the extrude's edges, a sketch on its face, \
                       the extrude made from a sketch), nothing is deleted and they're named, \
                       unless children=\"delete\" (delete those too) or \"keep\" (leave them; \
                       they lose those references). An extrude's or cut's own sketch stays in \
                       the tree unless delete_absorbed=true; a hole's own sketches always go \
                       with it. The default planes, origin and \
                       components can't be deleted, nor a component's features from the \
                       assembly (open the part). Rebuilds, and reports what's gone and what now \
                       fails to rebuild. To remove lines/arcs/points from a sketch, use \
                       delete_sketch_entities. Requires no sketch open for editing."
    )]
    async fn delete_features(
        &self,
        Parameters(params): Parameters<delete::DeleteFeaturesParams>,
    ) -> Result<CallToolResult, McpError> {
        let features = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            delete::feature_targets(&ids, &params.features)?
        };
        let reply = self
            .addin
            .delete_features(proto::DeleteFeaturesRequest {
                features,
                children: proto::ChildFeatures::from(params.children).into(),
                delete_absorbed: params.delete_absorbed,
            })
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "feature", &params.features))?;
        let text = delete::features_reply_text(&params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Delete entities from a sketch of the active part: segments (lines, \
                       arcs, circles, splines, ...) and standalone points, by the sketch ids \
                       inspect_sketch and the sketch_* tools report (\"line:3\", \"point:7\"), \
                       all in one sketch. Their dimensions and relations go with them, and a \
                       segment's end/center points go with the segment (they can't be deleted \
                       on their own). Everything is checked before anything is deleted. \
                       Reports what went, the sketch's contours and definition state, and \
                       features (e.g. the sketch's extrude) that now fail to rebuild. To delete \
                       a whole sketch, use delete_features with its name. Requires no sketch \
                       open for editing."
    )]
    async fn delete_sketch_entities(
        &self,
        Parameters(params): Parameters<delete::DeleteSketchEntitiesParams>,
    ) -> Result<CallToolResult, McpError> {
        let items = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            delete::sketch_items(&ids, &params.entities)?
        };
        let reply = self
            .addin
            .delete_sketch_entities(proto::DeleteSketchEntitiesRequest {
                sketch_name: trimmed(params.sketch_name.clone()),
                items,
            })
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "entity", &params.entities))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = delete::sketch_reply_text(&mut ids, &params.entities, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Mirror entities of a sketch of the active part about a line of the same \
                       sketch (usually a centerline: sketch_line with construction=true). \
                       entities = sketch ids from inspect_sketch or the sketch_* tools (\"line:3\", \
                       \"arc:1\", \"point:7\"); the originals stay and each copy gets a symmetric \
                       relation to its original, so dimensioning one half defines both. Copies \
                       share the originals' end points on the axis, and a line that ends on the \
                       axis at a right angle is extended across it instead of copied, so \
                       mirroring an open half profile whose ends touch the centerline closes it. \
                       Reports the new and extended entities' ids, the sketch's contours and \
                       definition state, and features that now fail to rebuild. Requires no \
                       sketch open for editing."
    )]
    async fn mirror_sketch_entities(
        &self,
        Parameters(params): Parameters<mirror::MirrorSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            mirror::sketch_request(&ids, &params)?
        };
        let reply = self
            .addin
            .mirror_sketch_entities(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "entity", &mirror::given_entities(&params)))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = mirror::sketch_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Copy entities of a sketch of the active part in a row or grid inside the \
                       same sketch, e.g. a row of slots or a grid of hole circles to cut in one \
                       go. entities = sketch ids from inspect_sketch or the sketch_* tools \
                       (\"circle:1\", \"line:3\", \"point:7\"); count instances (the originals \
                       included) spacing_mm apart along angle_deg (degrees counterclockwise from \
                       the sketch x axis, default 0), and for a grid count2 rows spacing2_mm \
                       apart along angle2_deg (default 90). Reports the new entities' ids, the \
                       sketch's contours and definition state, and features that now fail to \
                       rebuild. To copy features of the part, use linear_pattern_features. \
                       Requires no sketch open for editing."
    )]
    async fn linear_pattern_sketch_entities(
        &self,
        Parameters(params): Parameters<pattern::LinearSketchPatternParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pattern::linear_sketch_request(&ids, &params)?
        };
        let reply = self
            .addin
            .pattern_sketch_entities(request)
            .await
            .map_err(|status| {
                delete::with_legend(self.addin_error(status), "entity", &pattern::given_entities(&params.entities))
            })?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let how = pattern::linear_sketch_how(&params);
        let text = pattern::sketch_text(&mut ids, &params.entities, &how, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Copy entities of a sketch of the active part round a point inside the \
                       same sketch, e.g. bolt-hole circles on a pitch circle. entities = sketch \
                       ids from inspect_sketch or the sketch_* tools (\"circle:1\", \"line:3\", \
                       \"point:7\"); center = [x, y] in sketch coordinates (mm, default the \
                       sketch origin); count = instances including the originals, spread evenly \
                       all the way round unless angle_deg (the total) or spacing_deg (between \
                       neighbours) says otherwise, counterclockwise unless reverse_direction. \
                       Reports the new entities' ids, the sketch's contours and definition \
                       state, and features that now fail to rebuild. To copy features of the \
                       part, use circular_pattern_features. Requires no sketch open for editing."
    )]
    async fn circular_pattern_sketch_entities(
        &self,
        Parameters(params): Parameters<pattern::CircularSketchPatternParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pattern::circular_sketch_request(&ids, &params)?
        };
        let reply = self
            .addin
            .pattern_sketch_entities(request)
            .await
            .map_err(|status| {
                delete::with_legend(self.addin_error(status), "entity", &pattern::given_entities(&params.entities))
            })?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let how = pattern::circular_sketch_how(&params);
        let text = pattern::sketch_text(&mut ids, &params.entities, &how, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Find out what's wrong with the active part's or assembly's feature tree, \
                       e.g. after deleting a feature or mate, or when inspect_model shows \
                       features with errors. Rebuilds (unless rebuild=false), then lists every \
                       feature that fails or warns, every sketch with dangling, broken or \
                       over-defining relations/dimensions or a missing sketch plane, and every \
                       mate whose mated face/edge/plane is gone (with which one and on which \
                       component). Each problem says what's wrong, its parents and children, \
                       and whether it's a root cause (nothing it depends on fails -- usually what \
                       referenced the deleted geometry, with a suggested fix) or only fails \
                       because of a parent (`caused_by`); root causes come first. Sketch entity \
                       ids work with inspect_sketch / delete_sketch_entities, feature ids with \
                       delete_features. Changes nothing apart from the rebuild. Requires no \
                       sketch open for editing."
    )]
    async fn diagnose_model(
        &self,
        Parameters(params): Parameters<diagnose::DiagnoseModelParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .diagnose_model(params.request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            diagnose::render(&mut ids, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Change an existing boss-extrude or cut-extrude of the active part, like \
                       Edit Feature: end_condition (\"blind\" to depth_mm, \"through_all\", \
                       \"mid_plane\", \"up_to_next\", for cuts \"through_all_both\"), depth_mm, \
                       reverse_direction, a draft (draft_deg, draft_outward), a second direction \
                       (both_directions, end_condition2, depth2_mm), and for cuts \
                       flip_side_to_cut. `feature` = \"feature:N\" from inspect_model or a name \
                       like \"Cut-Extrude1\". Only what's given changes; with nothing else given \
                       it just reports the extrude's current settings and dimension names. If \
                       SolidWorks rejects the change or the extrude fails to rebuild with it (e.g. \
                       a cut reversed into empty space), the old settings are put back and the \
                       reason reported. Reports before/after and other features that now fail. \
                       To change the profile, edit its sketch with the sketch tools; for just a \
                       depth, set_dimension on D1@<feature> works too. Requires no sketch open for \
                       editing."
    )]
    async fn edit_extrude(
        &self,
        Parameters(params): Parameters<edit::EditExtrudeParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            edit::extrude_request(&ids, &params)?
        };
        let reply = self
            .addin
            .edit_extrude(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = edit::extrude_reply_text(&params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Change an existing mate of the active assembly, like Edit Mate: its kind \
                       (e.g. coincident -> distance with distance_mm), the two entities it mates \
                       (as in add_mate), alignment (\"aligned\"/\"anti_aligned\"/\"closest\" -- \
                       turns a component around), flip (distance/angle), distance_mm, angle_deg, \
                       limits (min_mm / max_mm or min_deg / max_deg: a distance / angle mate \
                       becomes a limit mate; remove_limits=true makes it plain again) or \
                       lock_rotation (concentric). `mate` = \"feature:N\" from inspect_model \
                       what=\"mates\" or a name like \"Coincident1\". Only what's given changes. \
                       A new kind replaces the mate with one named after the kind (Coincident1 -> \
                       Distance1), and the mate's id changes with every edit: use the one in the \
                       reply. SolidWorks then re-solves and moves the components that aren't fixed. If \
                       the change is rejected, over-defines the assembly or can't be solved, the \
                       mate is edited back and the reason reported. Reports before/after, the \
                       mated entities and where the components ended up. For only a distance/angle \
                       value, set_dimension on the mate's dimension works too. Requires an open \
                       assembly with no sketch being edited."
    )]
    async fn edit_mate(
        &self,
        Parameters(params): Parameters<edit::EditMateParams>,
    ) -> Result<CallToolResult, McpError> {
        let entities = edit::check_mate(&params)?;
        let mut persist_ids = Vec::new();
        for (i, entity) in entities.iter().enumerate() {
            persist_ids.push(
                self.resolve_id(entity)
                    .map_err(|e| prefix_error(e, &format!("entity {}", i + 1)))?,
            );
        }
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            edit::mate_request(&ids, &params, persist_ids)?
        };
        let reply = self
            .addin
            .edit_mate(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "entity", &entities))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = edit::mate_reply_text(&mut ids, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
    #[tool(
        description = "Add a reference plane to the active part, like Insert > Reference Geometry \
                       > Plane: from one to three references, each with how the plane relates to \
                       it. References are model geometry (inspect_model ids: \"face:3\", \
                       \"edge:7\", \"vertex:2\" from an edge's start_vertex/end_vertex, \
                       \"feature:1\" for a plane/axis/point), sketch entities (\"line:3\", \
                       \"point:7\", from the sketch_* tools or inspect_sketch), \"origin\", \
                       \"front\"/\"top\"/\"right\", or a feature name like \"Plane1\". Common \
                       cases: offset [{\"entity\": \"top\", \"offset_mm\": 20}]; through an edge \
                       at an angle to a face [{\"entity\": \"face:3\", \"angle_deg\": 30}, \
                       {\"entity\": \"edge:5\"}]; through three points; normal to a sketch line / \
                       edge at a point [{\"entity\": \"line:2\", \"relation\": \"perpendicular\"}, \
                       {\"entity\": \"point:3\"}]; midway between two faces (both \"mid_plane\"); \
                       tangent to a cylinder at a line on it. Reports the plane's normal and a \
                       point on it; sketch on it with the sketch tools' on=\"feature:N\". If \
                       SolidWorks can't make the plane from the references (or it fails to \
                       rebuild), nothing is added. Requires a part with no sketch open."
    )]
    async fn create_reference_plane(
        &self,
        Parameters(params): Parameters<refgeom::ReferencePlaneParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            refgeom::plane_request(&ids, &params)?
        };
        let given: Vec<String> = params.references.iter().map(|r| r.entity.clone()).collect();
        self.reference_geometry(request, &refgeom::plane_from_text(&params), &given).await
    }

    #[tool(
        description = "Add a reference axis to the active part, like Insert > Reference Geometry \
                       > Axis, from one or two references: a straight edge or sketch line; a \
                       cylindrical or conical face (its axis, e.g. through a hole); two planes or \
                       planar faces (where they meet); two vertices / sketch points / \"origin\"; \
                       or a point and a face/plane (through the point, normal to it). References \
                       as in create_reference_plane (inspect_model ids, sketch entity ids, \
                       \"origin\", \"front\"/\"top\"/\"right\", feature names). Reports the axis' \
                       direction and a point on it; its \"feature:N\" id works as a reference for \
                       planes, points and coordinate systems, and in add_mate. Requires a part \
                       with no sketch open."
    )]
    async fn create_reference_axis(
        &self,
        Parameters(params): Parameters<refgeom::ReferenceAxisParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            refgeom::axis_request(&ids, &params)?
        };
        let refs: Vec<&str> = params.references.iter().map(String::as_str).collect();
        self.reference_geometry(request, &refgeom::from_text(&refs), &params.references).await
    }

    #[tool(
        description = "Add reference points to the active part, like Insert > Reference Geometry \
                       > Point: at the center of a circular edge or sketch arc/circle \
                       (kind=\"center\"), the center of a face (\"face_center\"), a vertex or \
                       sketch point projected onto a face/plane (\"projection\"), where two \
                       edges/sketch segments cross or an edge meets a face/plane \
                       (\"intersection\"), at a vertex or sketch point (\"at\"), or along an edge \
                       / sketch segment (\"along\": distance_mm or percent from one end, count \
                       for several -- alone, count points spread evenly). kind can usually be left \
                       out: it follows from the references. References as in \
                       create_reference_plane. Reports each point's position; their \"feature:N\" \
                       ids work as references for planes, axes and coordinate systems. Requires a \
                       part with no sketch open."
    )]
    async fn create_reference_point(
        &self,
        Parameters(params): Parameters<refgeom::ReferencePointParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            refgeom::point_request(&ids, &params)?
        };
        let refs: Vec<&str> = params.references.iter().map(String::as_str).collect();
        self.reference_geometry(request, &refgeom::from_text(&refs), &params.references).await
    }

    #[tool(
        description = "Add a coordinate system to the active part, like Insert > Reference \
                       Geometry > Coordinate System: `origin` at a vertex, sketch point or \
                       reference point (default: the part origin), and up to two of x_axis / \
                       y_axis / z_axis, each following an edge, sketch line or axis, a plane or \
                       planar face's normal, or pointing at a vertex/point; flip_x/y/z reverse \
                       one. References as in create_reference_plane. Reports the origin and axes \
                       in model coordinates. Requires a part with no sketch open."
    )]
    async fn create_coordinate_system(
        &self,
        Parameters(params): Parameters<refgeom::CoordinateSystemParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            refgeom::coordinate_system_request(&ids, &params)?
        };
        let given: Vec<String> = [&params.origin, &params.x_axis, &params.y_axis, &params.z_axis]
            .iter()
            .filter_map(|r| (*r).clone())
            .collect();
        let refs: Vec<&str> = given.iter().map(String::as_str).collect();
        self.reference_geometry(request, &refgeom::from_text(&refs), &[]).await
    }
}

impl SolidWorksTools {
    /// Shared by revolve_sketch and cut_revolve_sketch.
    async fn revolve(&self, params: revolve::RevolveParams, cut: bool) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            revolve::request(&ids, &params, cut)?
        };
        let reply = self
            .addin
            .revolve_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = revolve::reply_text(&mut ids, &params, &reply, cut)
            + &sketch_status_note(&reply.sketch_name, reply.sketch_status());
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// Shared by sweep_sketch and cut_sweep_sketch.
    async fn sweep(&self, params: sweep::SweepParams, cut: bool) -> Result<CallToolResult, McpError> {
        let request = sweep::request(&params, cut)?;
        let reply = self
            .addin
            .sweep_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut text = sweep::reply_text(&params, &reply, cut);
        if !reply.sketch_name.is_empty() {
            text += &sketch_status_note(&reply.sketch_name, reply.sketch_status());
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// Shared by loft_sketch and cut_loft_sketch.
    async fn loft(&self, params: loft::LoftParams, cut: bool) -> Result<CallToolResult, McpError> {
        let request = loft::request(&params, cut)?;
        let reply = self
            .addin
            .loft_sketch(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut text = loft::reply_text(&request, &reply);
        for (name, status) in request.profiles.iter().zip(reply.profile_status()) {
            text += &sketch_status_note(name, status);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// Shared by the reference geometry tools: `from` describes the
    /// references for the reply, `given` numbers them for error messages.
    async fn reference_geometry(
        &self,
        request: proto::CreateReferenceGeometryRequest,
        from: &str,
        given: &[String],
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .create_reference_geometry(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "reference", given))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = refgeom::reply_text(&mut ids, &reply, from);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

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
        text.push('.');
        {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            text += &sketch::items_text(&mut ids, &sketch.items);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// Shared by the sketch_* tools: draws `entities` (meters) into the target sketch and
    /// describes the result. `what` names what was drawn, e.g. "3 lines".
    async fn draw_sketch(
        &self,
        target: SketchTarget,
        model_space: bool,
        entities: Vec<proto::SketchEntity>,
        what: String,
    ) -> Result<CallToolResult, McpError> {
        let sketch_name = target
            .sketch_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let on = target.on.as_deref().map(str::trim).filter(|on| !on.is_empty());
        if sketch_name.is_some() && (on.is_some() || target.plane.is_some()) {
            return Err(McpError::invalid_params(
                "give either sketch_name (to add to an existing sketch) or plane/on (for a new \
                 one), not both",
                None,
            ));
        }
        let plane = target.plane.unwrap_or_default();

        let on_persist_id = match on {
            Some(id) => self.resolve_id(id)?,
            None => Vec::new(),
        };
        let request = proto::AddSketchEntitiesRequest {
            sketch_name: sketch_name.unwrap_or_default().to_string(),
            plane: if sketch_name.is_some() {
                proto::Plane::Unspecified
            } else {
                proto::Plane::from(plane)
            }
            .into(),
            on_persist_id,
            model_space,
            entities,
        };
        let result = self
            .addin
            .add_sketch_entities(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let place = match (sketch_name, on) {
            (Some(_), _) => String::new(),
            (None, Some(id)) => format!(" (new sketch on {id})"),
            (None, None) => format!(" (new sketch on the {plane:?} plane)"),
        };
        let mut text = format!("drew {what} in {}{place}.", result.sketch_name);
        {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            text += &sketch::items_text(&mut ids, &result.items);
        }
        if let Some(contours) = &result.contours {
            text += &format!(
                " {} now has {} closed and {} open contour(s)",
                result.sketch_name, contours.closed, contours.open
            );
            text += if contours.closed > 0 && contours.open == 0 {
                "; ready to extrude."
            } else if contours.open > 0 {
                "; extrude_sketch needs closed ones, so connect the open ends if you mean to \
                 extrude it."
            } else {
                "."
            };
        }
        if let Some(frame) = &result.frame
            && let (Some(origin), Some(x), Some(y), Some(n)) =
                (&frame.origin, &frame.x_axis, &frame.y_axis, &frame.normal)
        {
            text += &format!(
                " Sketch axes in model space: origin {} mm, X along {}, Y along {}, normal {} \
                 (a boss extrudes this way unless reversed).",
                inspect::point_mm(origin),
                inspect::direction(x),
                inspect::direction(y),
                inspect::direction(n)
            );
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// The wire refs for a dimension's or relation's entities.
    fn sketch_refs(&self, entities: &[String], max: usize) -> Result<Vec<proto::SketchRef>, McpError> {
        if entities.is_empty() || entities.len() > max {
            return Err(McpError::invalid_params(format!("give 1 to {max} entities"), None));
        }
        entities.iter().map(|e| self.sketch_ref(e)).collect()
    }

    /// One dimension/relation entity: a sketch entity id, "origin", or a model edge/face id.
    fn sketch_ref(&self, entity: &str) -> Result<proto::SketchRef, McpError> {
        let entity = entity.trim();
        if entity.eq_ignore_ascii_case("origin") {
            return Ok(RefTarget::Origin.into_proto());
        }
        match Kind::of_id(entity) {
            Some(Kind::Edge | Kind::Face) => Ok(RefTarget::Model(self.resolve_id(entity)?).into_proto()),
            Some(kind) if SKETCH_KINDS.contains(&kind) => {
                let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                sketch::decode_ref(&ids, entity)
                    .map(|r| RefTarget::Item(r).into_proto())
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            ids.unknown_id(
                                entity,
                                "inspect_sketch and the sketch_* tools",
                                "call inspect_sketch for current ones",
                            ),
                            None,
                        )
                    })
            }
            Some(_) => Err(McpError::invalid_params(
                format!(
                    "{entity} can't be used here -- give sketch entities (\"line:3\", \
                     \"point:7\"), \"origin\", or a model edge/face id"
                ),
                None,
            )),
            None => Err(McpError::invalid_params(
                format!(
                    "{entity:?} isn't an id -- give sketch entity ids like \"line:3\" (from \
                     inspect_sketch), \"origin\", or edge/face ids from inspect_model"
                ),
                None,
            )),
        }
    }

    /// What a render_views highlight refers to: a short id, or else a
    /// feature name.
    fn highlight_target(&self, highlight: &str) -> Result<render::Target, McpError> {
        if !IdRegistry::looks_like_id(highlight) {
            if highlight.is_empty() {
                return Err(McpError::invalid_params("empty highlight", None));
            }
            return Ok(render::Target::FeatureName(highlight.to_string()));
        }
        let persist_id = self.resolve_id(highlight)?;
        let kind = highlight.split(':').next().unwrap_or_default();
        Ok(match kind {
            "face" => render::Target::Face(persist_id),
            "edge" => render::Target::Edge(persist_id),
            "body" => render::Target::Body(persist_id),
            "component" => render::Target::Component(persist_id),
            "vertex" => {
                return Err(McpError::invalid_params(
                    format!("{highlight} is a vertex; render_views can't highlight those -- highlight an edge at it"),
                    None,
                ));
            }
            _ => render::Target::Feature(persist_id),
        })
    }

    /// Points the id registry at the active document, so the ids the tool
    /// call about to run mints and resolves are that document's. `rename`:
    /// the active document is the one the registry has, under a new file
    /// (Save As), so its ids carry over.
    async fn track_document(&self, rename: bool) {
        let active = self.addin.get_active_document(Some(DOCUMENT_CHECK_TIMEOUT)).await;
        *self.units.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            active.as_ref().ok().and_then(|reply| reply.document.as_ref()).and_then(|doc| doc.units.clone());
        match active {
            Ok(reply) => {
                let (key, name) = files::document_key(&reply);
                let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if rename {
                    ids.rename_document(&key, &name);
                } else {
                    ids.set_document(&key, &name);
                }
            }
            // The tool call itself then says what's wrong with the add-in.
            Err(status) => tracing::debug!("couldn't check the active document: {status}"),
        }
    }

    /// The active document's units as of this tool call's start.
    fn active_units(&self) -> Option<proto::DocumentUnits> {
        self.units.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// The persist ID behind an id handed out by inspect_model.
    fn resolve_id(&self, id: &str) -> Result<Vec<u8>, McpError> {
        let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        ids.persist_id(id).ok_or_else(|| {
            McpError::invalid_params(
                ids.unknown_id(id, "inspect_model", "call inspect_model for current ones"),
                None,
            )
        })
    }

    /// add_sheet / activate_sheet / edit_sheet: the RPC, then the drawing
    /// cache brought up to date with the sheet's views.
    async fn setup_sheet(&self, request: proto::SetupSheetRequest) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .setup_sheet(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        if let Some(sheet) = &reply.sheet {
            let moved = request.scale_numerator.is_some() || request.first_angle.is_some();
            let mut cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.sheet_changed(&sheet.views, moved);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(sheet::reply_text(&request, &reply))]))
    }

    /// create_drawing_view / edit_drawing_view: the RPC; a new view without
    /// at_mm, made in the sheet's middle, is then moved clear of the other
    /// views. The drawing cache forgets the redrawn views' edges.
    async fn setup_drawing_view(&self, request: proto::SetupDrawingViewRequest) -> Result<CallToolResult, McpError> {
        let mut reply = self
            .addin
            .setup_drawing_view(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let asked_at = request.position.as_ref().map(|p| glam::DVec2::new(p.x, p.y));
        let mut moved_to = None;
        if request.create
            && request.position.is_none()
            && let Some(spot) = reply.sheet.as_ref().and_then(|s| view::free_spot(s, &reply.view_name))
        {
            let moved = self
                .addin
                .setup_drawing_view(proto::SetupDrawingViewRequest {
                    view_name: reply.view_name.clone(),
                    position: Some(proto::Vec3 { x: spot.x, y: spot.y, z: 0.0 }),
                    ..Default::default()
                })
                .await;
            // The view is made either way; a failed move leaves it in the
            // middle, and the reply says what it overlaps.
            match moved {
                Ok(mut moved) => {
                    moved.created = true;
                    // The sheet scale SolidWorks may have changed while making it.
                    moved.sheet_scale_before = reply.sheet_scale_before.take();
                    reply = moved;
                    moved_to = Some(spot);
                }
                Err(status) => tracing::warn!("moving the new view {} clear failed: {status}", reply.view_name),
            }
        }
        if let Some(sheet) = &reply.sheet {
            let mut cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let rescaled = reply.sheet_scale_before.as_ref().is_some_and(|b| {
                b.numerator != sheet.scale_numerator || b.denominator != sheet.scale_denominator
            });
            if view::redraws(&request) {
                let mut names = view::redrawn_views(&reply);
                if request.view_name != reply.view_name {
                    names.push(request.view_name.clone());
                }
                cache.views_changed(&names);
            }
            cache.sheet_changed(&sheet.views, rescaled);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(view::reply_text(&request, &reply, asked_at, moved_to))]))
    }

    /// add_drawing_note / edit_drawing_annotation: the RPC; the drawing
    /// cache takes the sheet's view outlines.
    async fn setup_drawing_annotation(
        &self,
        request: proto::SetupDrawingAnnotationRequest,
        remarks: Vec<String>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .setup_drawing_annotation(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        if let Some(sheet) = &reply.sheet {
            let mut cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.sheet_changed(&sheet.views, false);
        }
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            note::setup_reply(&mut ids, &request, &reply, &remarks)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// add_geometric_tolerance / add_datum_symbol: SetupDrawingAnnotation in
    /// a drawing, AddModelAnnotation in a part or assembly; `remarks` go in
    /// the reply.
    async fn add_gdt(
        &self,
        symbol: proto::GdtSymbol,
        place: &gdt::PlaceParams,
        remarks: Vec<String>,
    ) -> Result<CallToolResult, McpError> {
        let active = self
            .addin
            .get_active_document(Some(DOCUMENT_CHECK_TIMEOUT))
            .await
            .map_err(|status| self.addin_error(status))?;
        let Some(document) = active.document else {
            return Err(McpError::invalid_params(
                "no document is open in SolidWorks -- open the part, assembly or drawing first",
                None,
            ));
        };
        if document.r#type() == proto::DocumentType::Drawing {
            let (request, mut placed) = {
                let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                gdt::drawing_request(&ids, &cache, symbol, place)?
            };
            placed.extend(remarks);
            return self.setup_drawing_annotation(request, placed).await;
        }
        let (request, mut placed) = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            gdt::model_request(&ids, symbol, place)?
        };
        placed.extend(remarks);
        let remarks = placed;
        let reply = self
            .addin
            .add_model_annotation(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            gdt::model_reply(&mut ids, &request, &reply, &remarks)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// inspect_title_block / fill_title_block: the sheet's title block
    /// fields with the drawing's and the sheet model's property values.
    async fn title_block_fields(&self, sheet_name: &str) -> Result<serde_json::Value, McpError> {
        let info = self
            .addin
            .inspect_drawing(sheet_format::fields_request())
            .await
            .map_err(|status| self.addin_error(status))?;
        let sheet = sheet_format::find_sheet(&info, sheet_name)?;
        let fields = sheet_format::fields(sheet);
        let drawing = self
            .addin
            .custom_properties(proto::CustomPropertiesRequest::default())
            .await
            .map_err(|status| self.addin_error(status))?;
        let model = if fields.iter().any(|f| f.of == sheet_format::PropertyOf::Model) && !sheet.views.is_empty() {
            self.addin
                .custom_properties(proto::CustomPropertiesRequest { sheet_name: sheet.name.clone(), ..Default::default() })
                .await
                .ok()
        } else {
            None
        };
        Ok(serde_json::json!({
            "sheet": sheet.name,
            "sheet_format": sheet.sheet_format,
            "model": model.as_ref().and_then(|m| m.document.as_ref()).map(|d| d.title.clone()),
            "fields": sheet_format::fields_json(&fields, &drawing.properties, model.as_ref()),
        }))
    }

    /// Turns a gRPC failure into an MCP error with a message that tells the
    /// model (and the user) what to actually do about it.
    fn addin_error(&self, status: tonic::Status) -> McpError {
        let message = match status.code() {
            Code::Unavailable => format!(
                "can't reach the sld_mcp add-in{} -- open SolidWorks with the \
                 sld_mcp add-in enabled ({})",
                self.addin.endpoint().map(|at| format!(" at {at}")).unwrap_or_default(),
                status.message()
            ),
            Code::Unauthenticated => format!(
                "the sld_mcp add-in{} refused the call: {}",
                self.addin.endpoint().map(|at| format!(" at {at}")).unwrap_or_default(),
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
    // Written by hand to check which document is active first: ids belong to
    // a document (see IdRegistry).
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.track_document(false).await;
        self.tool_router.call(ToolCallContext::new(self, request, context)).await
    }

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
                 the ids it returns let other tools target specific faces. Sketch with \
                 create_rectangle_sketch / create_square_sketch or, for any other profile, \
                 sketch_line, sketch_arc, sketch_circle, sketch_spline and sketch_point \
                 (pass sketch_name to keep adding to one sketch), then extrude_sketch or \
                 cut_extrude_sketch, or for round parts revolve_sketch / cut_revolve_sketch \
                 about a centerline (sketch_line with construction=true), or sweep_sketch / \
                 cut_sweep_sketch to run a profile (or a circle) along a path sketch, or                  loft_sketch / cut_loft_sketch to blend through profiles on several planes; \
                 fillet_edges / chamfer_edges round or bevel edges (by edge or face id); \
                 mirror_features copies features or bodies about a plane, and \
                 mirror_sketch_entities mirrors sketch geometry about a centerline; \
                 linear_pattern_features / circular_pattern_features copy features or bodies \
                 in a row, grid or round an axis, and linear_pattern_sketch_entities / \
                 circular_pattern_sketch_entities do the same inside a sketch. \
                 hole_wizard adds standard clearance, counterbore, \
                 countersink or tapped holes at the points of a (dimensioned) sketch, or at \
                 locations on a face. To make a sketch fully defined: the drawing tools \
                 report entity ids (\"line:1\", \"point:2\"; inspect_sketch lists them all), \
                 dimension_sketch adds driving dimensions and add_sketch_relations adds \
                 relations for the sizes and alignments that matter, then fully_define_sketch \
                 locks whatever is left. set_dimension changes a dimension later. render_views \
                 draws the part (optionally highlighting ids) to check its shape. \
                 Assemblies: inspect_model lists the components and mates and, within a \
                 component, its faces, edges and planes (in assembly coordinates); add_mate \
                 mates two components by those ids, and render_views shows where they ended \
                 up; inspect_mobility reports each component's remaining degrees of freedom \
                 (what it can still slide along / turn about); check_interference finds \
                 components that overlap (also from a drawing \
                 of the assembly). Motion: move_component drags a component while its mates \
                 solve (check_collisions=true reports what it hits on the way); add_mate \
                 min_mm / max_mm or min_deg / max_deg makes a limit mate, and sweep_mate runs a \
                 limit (or distance / angle) mate through its range checking for collisions. inspect_annotations reads the model's MBD / PMI (DimXpert dimensions, \
                 tolerances, datums, feature control frames); add_datum_symbol and \
                 add_geometric_tolerance add datums and feature control frames, to a part, \
                 assembly or drawing. Drawings: inspect_drawing lists \
                 the sheets and views and, for one view, its visible edges with ids and \
                 sheet positions; add_drawing_dimensions dimensions those edges, \
                 insert_model_items brings in the model's own dimensions, \
                 set_dimension_tolerance tolerances a dimension (drawing or model), and \
                 render_drawing shows the sheet as printed to check the layout. To undo or \
                 rework: delete_features removes features (sketches, extrudes, cuts, holes, \
                 fillets, planes, mates, ...) by inspect_model id or name, refusing unless told \
                 what to do with features that depend on them; delete_sketch_entities removes \
                 lines, arcs, points, ... from a sketch. Reference geometry: \
                 create_reference_plane / _axis / _point and create_coordinate_system build \
                 planes, axes, points and coordinate systems from faces, edges, vertices, sketch \
                 entities, the origin or the default planes; sketch on a new plane with \
                 on=\"feature:N\". Lengths are in millimeters, angles in \
                 degrees, whatever the document's units: for an inch (imperial) part, \
                 assembly or drawing give create_part / create_assembly / create_drawing \
                 units=\"ips\" (or change an open one with document_units) and keep passing \
                 mm -- SolidWorks then shows dimensions, tolerances and sheets in inches."
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

    let addin = AddinClient::new();
    let service = SolidWorksTools::new(addin).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sketch_points_convert_to_meters() {
        let mut points = PointConverter::default();
        let p = points.convert(&[10.0, -2.5], "point").unwrap();
        assert_eq!((p.x, p.y, p.z), (0.01, -0.0025, 0.0));
        assert!(!points.model_space());

        let mut points = PointConverter::default();
        let p = points.convert(&[1.0, 2.0, 3.0], "point").unwrap();
        assert_eq!((p.x, p.y, p.z), (0.001, 0.002, 0.003));
        assert!(points.model_space());
    }

    #[test]
    fn sketch_points_reject_bad_input() {
        let mut points = PointConverter::default();
        assert!(points.convert(&[1.0], "point").is_err());
        assert!(points.convert(&[1.0, 2.0, 3.0, 4.0], "point").is_err());
        assert!(points.convert(&[f64::NAN, 0.0], "point").is_err());

        // One call can't mix sketch and model points.
        let mut points = PointConverter::default();
        points.convert(&[0.0, 0.0], "start").unwrap();
        assert!(points.convert(&[0.0, 0.0, 0.0], "end").is_err());
    }

    #[test]
    fn dimension_values_check_units() {
        let spec = |kind, value_mm, value_deg| DimensionSpec {
            kind,
            entities: vec!["line:1".into()],
            value_mm,
            value_deg,
        };
        assert_eq!(dimension_value(&spec(DimensionKind::Length, Some(50.0), None)).unwrap(), Some(0.05));
        assert_eq!(dimension_value(&spec(DimensionKind::Length, None, None)).unwrap(), None);
        let angle = dimension_value(&spec(DimensionKind::Angle, None, Some(90.0))).unwrap().unwrap();
        assert!((angle - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        // Wrong unit for the kind, both units, or out of range.
        assert!(dimension_value(&spec(DimensionKind::Angle, Some(90.0), None)).is_err());
        assert!(dimension_value(&spec(DimensionKind::Diameter, None, Some(10.0))).is_err());
        assert!(dimension_value(&spec(DimensionKind::Length, Some(1.0), Some(1.0))).is_err());
        assert!(dimension_value(&spec(DimensionKind::Length, Some(-1.0), None)).is_err());
        assert!(dimension_value(&spec(DimensionKind::Angle, None, Some(360.0))).is_err());
    }
}
