mod addin;
mod assembly;
mod center_mark;
mod component;
mod configuration;
mod delete;
mod design_table;
mod diagnose;
mod discovery;
mod draft;
mod drawing;
mod drawing_image;
mod edit;
mod equation;
mod feature_edit;
mod feature_inspect;
mod feature_order;
mod files;
mod fillet;
mod gdt;
mod helix;
mod hole;
mod inspect;
mod interference;
mod loft;
mod macro_run;
mod measure;
mod mirror;
mod mobility;
mod motion;
mod note;
mod offset;
mod pattern;
mod pmi;
mod properties;
mod recognize;
mod references;
mod refgeom;
mod render;
mod revolve;
mod rib;
mod sheet;
mod sheet_format;
mod shell;
mod sketch_edit;
mod sketch;
mod sweep;
mod table;
mod tolerance;
mod tool_schema;
mod toolsets;
mod derived_view;
mod undo;
mod units;
mod view;
mod visibility;

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

/// A point for the sketch tools, in millimeters: [x, y] in the sketch's own 2D coordinates,
/// or [x, y, z] in model coordinates, projected onto the sketch plane (a 3D sketch takes only
/// these, as they are). All points in one call must use the same form.
type Point = Vec<f64>;

/// Which sketch the sketch tools draw in.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct SketchTarget {
    /// Add to this existing sketch, e.g. "Sketch1" or "3DSketch1". Omit for a new sketch.
    #[serde(default)]
    sketch_name: Option<String>,
    /// New sketch: the default plane to sketch on. Defaults to front.
    #[serde(default)]
    plane: Option<SketchPlane>,
    /// New sketch: on this planar face or reference plane instead ("face:4", "feature:2");
    /// [x, y, z] points then place geometry without knowing the sketch axes.
    #[serde(default)]
    on: Option<String>,
}

/// Which sketch the sketch tool draws in, which unlike the other sketch tools can be a new 3D one.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct DrawTarget {
    /// New sketch: a 3D sketch, with no plane: [x, y, z] points go where they say. For a
    /// sweep path or loft guide that doesn't lie in a plane; can't be extruded.
    #[serde(default)]
    sketch_3d: bool,
    #[serde(flatten)]
    sketch: SketchTarget,
}

/// What the sketch tool draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum SketchShape {
    /// Straight lines through `points`: 2 make one line, more a chain.
    Line,
    /// By `center` and radius_mm or diameter_mm.
    Circle,
    /// From `start` to `end`, round `center` or through a `through` point (3D sketch: through only).
    Arc,
    /// A smooth curve through `points`.
    Spline,
    /// Sketch points at `points`.
    Point,
    /// Round-ended slot width_mm wide between the end centers `start` and `end`; curved along an
    /// arc given by `center` or `through`. Reports the construction line / arc down its middle.
    Slot,
    /// Regular polygon of `sides` round `center`, sized by corner_radius_mm or flat_radius_mm.
    /// Reports a construction circle through its corners (or touching its sides): a diameter
    /// dimension on it sizes the polygon.
    Polygon,
    /// By `center`, major_radius_mm and minor_radius_mm.
    Ellipse,
    /// width_mm x height_mm round `center`, width along sketch X, with width and height
    /// dimensions; always a new 2D sketch.
    Rectangle,
}

impl SketchShape {
    fn name(self) -> &'static str {
        match self {
            SketchShape::Line => "line",
            SketchShape::Circle => "circle",
            SketchShape::Arc => "arc",
            SketchShape::Spline => "spline",
            SketchShape::Point => "point",
            SketchShape::Slot => "slot",
            SketchShape::Polygon => "polygon",
            SketchShape::Ellipse => "ellipse",
            SketchShape::Rectangle => "rectangle",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SketchParams {
    shape: SketchShape,
    /// line, spline, point.
    #[serde(default)]
    points: Option<Vec<Point>>,
    /// line, spline: join the last point back to the first.
    #[serde(default)]
    closed: bool,
    /// circle, polygon, ellipse, rectangle (default: the origin, or the middle of the `on`
    /// face); arc, slot: the center of the arc.
    #[serde(default)]
    center: Option<Point>,
    /// arc: where it starts; slot: one end's center.
    #[serde(default)]
    start: Option<Point>,
    /// arc, slot: the other end. With `center` only its direction from the center counts.
    #[serde(default)]
    end: Option<Point>,
    /// arc, slot: a point on the arc between start and end, instead of `center`.
    #[serde(default)]
    through: Option<Point>,
    /// arc, slot with `center`: clockwise, seen from the side the sketch normal points to.
    #[serde(default)]
    clockwise: bool,
    /// circle.
    #[serde(default)]
    radius_mm: Option<f64>,
    /// circle.
    #[serde(default)]
    diameter_mm: Option<f64>,
    /// slot (twice the end radius), rectangle.
    #[serde(default)]
    width_mm: Option<f64>,
    /// rectangle. Defaults to width_mm: a square.
    #[serde(default)]
    height_mm: Option<f64>,
    /// polygon: 3 to 40.
    #[serde(default)]
    sides: Option<i32>,
    /// polygon: from the center to a corner.
    #[serde(default)]
    corner_radius_mm: Option<f64>,
    /// polygon: from the center to the middle of a side (half across flats).
    #[serde(default)]
    flat_radius_mm: Option<f64>,
    /// polygon: toward a corner (a side's middle with flat_radius_mm); ellipse: the long axis.
    /// Counterclockwise from sketch X. Defaults to 0.
    #[serde(default)]
    angle_deg: Option<f64>,
    /// ellipse: from the center to the end of the long axis.
    #[serde(default)]
    major_radius_mm: Option<f64>,
    /// ellipse: from the center to the end of the short axis.
    #[serde(default)]
    minor_radius_mm: Option<f64>,
    /// slot: add driving dimensions of its width and length.
    #[serde(default)]
    add_dimensions: bool,
    /// Construction geometry: reference only, ignored by features.
    #[serde(default)]
    construction: bool,
    #[serde(flatten)]
    target: DrawTarget,
}

impl SketchParams {
    /// Refuses parameters the shape doesn't take, rather than ignoring them.
    fn check_applies(&self) -> Result<(), McpError> {
        use SketchShape::*;
        let all_but_rectangle = &[Line, Circle, Arc, Spline, Point, Slot, Polygon, Ellipse][..];
        let given: [(&str, bool, &[SketchShape]); 21] = [
            ("points", self.points.is_some(), &[Line, Spline, Point]),
            ("closed", self.closed, &[Line, Spline]),
            ("center", self.center.is_some(), &[Circle, Arc, Slot, Polygon, Ellipse, Rectangle]),
            ("start", self.start.is_some(), &[Arc, Slot]),
            ("end", self.end.is_some(), &[Arc, Slot]),
            ("through", self.through.is_some(), &[Arc, Slot]),
            ("clockwise", self.clockwise, &[Arc, Slot]),
            ("radius_mm", self.radius_mm.is_some(), &[Circle]),
            ("diameter_mm", self.diameter_mm.is_some(), &[Circle]),
            ("width_mm", self.width_mm.is_some(), &[Slot, Rectangle]),
            ("height_mm", self.height_mm.is_some(), &[Rectangle]),
            ("sides", self.sides.is_some(), &[Polygon]),
            ("corner_radius_mm", self.corner_radius_mm.is_some(), &[Polygon]),
            ("flat_radius_mm", self.flat_radius_mm.is_some(), &[Polygon]),
            ("angle_deg", self.angle_deg.is_some(), &[Polygon, Ellipse]),
            ("major_radius_mm", self.major_radius_mm.is_some(), &[Ellipse]),
            ("minor_radius_mm", self.minor_radius_mm.is_some(), &[Ellipse]),
            ("add_dimensions", self.add_dimensions, &[Slot]),
            ("construction", self.construction, &[Line, Circle, Arc, Spline, Slot, Polygon, Ellipse]),
            // A circle goes to the add-in, which says why a 3D sketch can't take one.
            ("sketch_3d", self.target.sketch_3d, &[Line, Circle, Arc, Spline, Point]),
            ("sketch_name", self.target.sketch.sketch_name.is_some(), all_but_rectangle),
        ];
        match given.iter().find(|(_, given, shapes)| *given && !shapes.contains(&self.shape)) {
            Some((name, _, _)) => Err(McpError::invalid_params(
                format!("shape \"{}\" doesn't take {name}", self.shape.name()),
                None,
            )),
            None => Ok(()),
        }
    }

    /// A parameter the shape needs.
    fn needs<'a, T>(&self, value: &'a Option<T>, name: &str) -> Result<&'a T, McpError> {
        value.as_ref().ok_or_else(|| {
            McpError::invalid_params(format!("shape \"{}\" needs {name}", self.shape.name()), None)
        })
    }

    /// The entities a 2D or 3D shape draws (meters), and what to call them in the reply.
    fn entities(&self, points: &mut PointConverter) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let construction = self.construction;
        let one = |geometry| vec![sketch_entity(construction, geometry)];
        Ok(match self.shape {
            SketchShape::Line => {
                let given = self.needs(&self.points, "points")?;
                let n = given.len();
                if n < 2 {
                    return Err(McpError::invalid_params("give at least 2 points", None));
                }
                if self.closed && n < 3 {
                    return Err(McpError::invalid_params("a closed chain needs at least 3 points", None));
                }
                if n > MAX_SKETCH_ITEMS {
                    return Err(McpError::invalid_params(format!("at most {MAX_SKETCH_ITEMS} points per call"), None));
                }
                let mut vertices = points.convert_all(given, "point")?;
                if self.closed {
                    vertices.push(vertices[0]);
                }
                let entities = vertices
                    .windows(2)
                    .map(|pair| {
                        sketch_entity(
                            construction,
                            proto::sketch_entity::Geometry::Line(proto::SketchLine {
                                start: Some(pair[0]),
                                end: Some(pair[1]),
                            }),
                        )
                    })
                    .collect::<Vec<_>>();
                let what = count_of(entities.len(), "line");
                (entities, what)
            }
            SketchShape::Circle => {
                let radius_mm = match (self.radius_mm, self.diameter_mm) {
                    (Some(r), None) => r,
                    (None, Some(d)) => d / 2.0,
                    _ => {
                        return Err(McpError::invalid_params("give exactly one of radius_mm or diameter_mm", None));
                    }
                };
                if !(radius_mm.is_finite() && radius_mm > 0.0) {
                    return Err(McpError::invalid_params(
                        "the radius/diameter must be a positive number of millimeters",
                        None,
                    ));
                }
                let center = points.convert(self.needs(&self.center, "center")?, "center")?;
                let geometry = proto::sketch_entity::Geometry::Circle(proto::SketchCircle {
                    center: Some(center),
                    radius: radius_mm / MM_PER_M,
                });
                (one(geometry), format!("a circle of radius {radius_mm} mm"))
            }
            SketchShape::Arc => {
                let start = points.convert(self.needs(&self.start, "start")?, "start")?;
                let end = points.convert(self.needs(&self.end, "end")?, "end")?;
                let (center, through) = match (&self.center, &self.through) {
                    (Some(c), None) => (Some(points.convert(c, "center")?), None),
                    (None, Some(t)) => {
                        if self.clockwise {
                            return Err(McpError::invalid_params(
                                "clockwise only applies with `center`; a 3-point arc's direction comes \
                                 from its `through` point",
                                None,
                            ));
                        }
                        (None, Some(points.convert(t, "through")?))
                    }
                    _ => return Err(McpError::invalid_params("give exactly one of center or through", None)),
                };
                let geometry = proto::sketch_entity::Geometry::Arc(proto::SketchArc {
                    start: Some(start),
                    end: Some(end),
                    center,
                    through,
                    clockwise: self.clockwise,
                });
                (one(geometry), "an arc".to_string())
            }
            SketchShape::Spline => {
                let given = self.needs(&self.points, "points")?;
                let n = given.len();
                let min = if self.closed { 3 } else { 2 };
                if n < min {
                    return Err(McpError::invalid_params(format!("give at least {min} points"), None));
                }
                if n > MAX_SKETCH_ITEMS {
                    return Err(McpError::invalid_params(format!("at most {MAX_SKETCH_ITEMS} points per spline"), None));
                }
                let geometry = proto::sketch_entity::Geometry::Spline(proto::SketchSpline {
                    points: points.convert_all(given, "point")?,
                    closed: self.closed,
                });
                let what = format!("a {}spline through {n} points", if self.closed { "closed " } else { "" });
                (one(geometry), what)
            }
            SketchShape::Point => {
                let given = self.needs(&self.points, "points")?;
                let n = given.len();
                if n == 0 {
                    return Err(McpError::invalid_params("give at least 1 point", None));
                }
                if n > MAX_SKETCH_ITEMS {
                    return Err(McpError::invalid_params(format!("at most {MAX_SKETCH_ITEMS} points per call"), None));
                }
                let entities = points
                    .convert_all(given, "point")?
                    .into_iter()
                    .map(|position| {
                        sketch_entity(
                            false,
                            proto::sketch_entity::Geometry::Point(proto::SketchPointEntity { position: Some(position) }),
                        )
                    })
                    .collect::<Vec<_>>();
                (entities, count_of(n, "point"))
            }
            SketchShape::Slot => {
                let width_mm = *self.needs(&self.width_mm, "width_mm")?;
                if !(width_mm.is_finite() && width_mm > 0.0) {
                    return Err(McpError::invalid_params("width_mm must be a positive number of millimeters", None));
                }
                if self.center.is_some() && self.through.is_some() {
                    return Err(McpError::invalid_params("give center or through for a curved slot, not both", None));
                }
                if self.clockwise && self.center.is_none() {
                    return Err(McpError::invalid_params(
                        "clockwise only applies with `center`; a slot through a point has its direction from \
                         that point",
                        None,
                    ));
                }
                let start = points.convert(self.needs(&self.start, "start")?, "start")?;
                let end = points.convert(self.needs(&self.end, "end")?, "end")?;
                let center = self.center.as_ref().map(|c| points.convert(c, "center")).transpose()?;
                let through = self.through.as_ref().map(|t| points.convert(t, "through")).transpose()?;
                let what = format!(
                    "a {}slot {width_mm} mm wide",
                    if center.is_some() || through.is_some() { "curved " } else { "" }
                );
                let geometry = proto::sketch_entity::Geometry::Slot(proto::SketchSlot {
                    start: Some(start),
                    end: Some(end),
                    center,
                    through,
                    clockwise: self.clockwise,
                    width: width_mm / MM_PER_M,
                    add_dimensions: self.add_dimensions,
                });
                (one(geometry), what)
            }
            SketchShape::Polygon => {
                let sides = *self.needs(&self.sides, "sides")?;
                let (corner_radius, angle, inscribed) =
                    polygon_shape(sides, self.corner_radius_mm, self.flat_radius_mm, self.angle_deg.unwrap_or(0.0))?;
                let center = points.convert(self.needs(&self.center, "center")?, "center")?;
                let geometry = proto::sketch_entity::Geometry::Polygon(proto::SketchPolygon {
                    center: Some(center),
                    sides,
                    corner_radius,
                    angle,
                    inscribed,
                });
                (one(geometry), format!("a polygon with {sides} sides"))
            }
            SketchShape::Ellipse => {
                let major = *self.needs(&self.major_radius_mm, "major_radius_mm")?;
                let minor = *self.needs(&self.minor_radius_mm, "minor_radius_mm")?;
                if !(major.is_finite() && minor.is_finite() && minor > 0.0 && major > minor) {
                    return Err(McpError::invalid_params(
                        format!(
                            "major_radius_mm must be more than minor_radius_mm, and that more than 0, got {major} \
                             and {minor} (equal radii are a circle: use shape \"circle\")"
                        ),
                        None,
                    ));
                }
                let angle_deg = self.angle_deg.unwrap_or(0.0);
                if !angle_deg.is_finite() {
                    return Err(McpError::invalid_params("angle_deg must be finite", None));
                }
                let center = points.convert(self.needs(&self.center, "center")?, "center")?;
                let geometry = proto::sketch_entity::Geometry::Ellipse(proto::SketchEllipse {
                    center: Some(center),
                    major_radius: major / MM_PER_M,
                    minor_radius: minor / MM_PER_M,
                    angle: angle_deg.to_radians(),
                });
                (one(geometry), format!("an ellipse of {major} x {minor} mm radii"))
            }
            SketchShape::Rectangle => unreachable!("a rectangle is its own request"),
        })
    }
}

/// Most entities or spline points one sketch call may draw (the add-in's limit).
const MAX_SKETCH_ITEMS: usize = 1000;

/// A polygon as the add-in draws it: from the center to a corner, in the direction of one
/// (radians), and whether its construction circle touches the sides.
fn polygon_shape(
    sides: i32,
    corner_radius_mm: Option<f64>,
    flat_radius_mm: Option<f64>,
    angle_deg: f64,
) -> Result<(f64, f64, bool), McpError> {
    if !(3..=40).contains(&sides) {
        return Err(McpError::invalid_params(format!("a polygon needs 3 to 40 sides, got {sides}"), None));
    }
    if !angle_deg.is_finite() {
        return Err(McpError::invalid_params("angle_deg must be finite", None));
    }
    let half_side = std::f64::consts::PI / f64::from(sides);
    let (radius, angle, inscribed) = match (corner_radius_mm, flat_radius_mm) {
        (Some(r), None) => (r, angle_deg.to_radians(), false),
        (None, Some(r)) => (r / half_side.cos(), angle_deg.to_radians() + half_side, true),
        _ => {
            return Err(McpError::invalid_params(
                "give exactly one of corner_radius_mm or flat_radius_mm",
                None,
            ));
        }
    };
    if !(radius.is_finite() && radius > 0.0) {
        return Err(McpError::invalid_params(
            "the radius must be a positive number of millimeters",
            None,
        ));
    }
    Ok((radius / MM_PER_M, angle, inscribed))
}

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
    /// In millimeters; with mid_plane the total, split evenly across both sides of the sketch
    /// plane. Needed unless through_all.
    #[serde(default)]
    depth_mm: Option<f64>,
    /// Remove material instead (cut-extrude).
    #[serde(default)]
    cut: bool,
    /// Cuts: through the whole part instead of to a depth.
    #[serde(default)]
    through_all: bool,
    /// The sketch, e.g. "Sketch1". Defaults to the newest sketch no feature uses yet.
    #[serde(default)]
    sketch_name: Option<String>,
    /// The other way: a boss against the sketch normal, a cut along it (and only that way).
    /// Ignored with mid_plane.
    #[serde(default)]
    reverse_direction: bool,
    /// Symmetric to both sides of the sketch plane.
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
    /// Views to draw, in order, as a grid: a standard view's name, or {"direction": [x, y, z]}
    /// to look from any direction (model axes, from the part toward the eye; optional "up").
    /// Defaults to ["top", "isometric", "front", "right"] in a 2x2 grid: top above front and
    /// right beside it, like a third-angle drawing; with `section`, straight at the cut and at
    /// an angle.
    #[serde(default)]
    views: Option<Vec<render::ViewSpec>>,
    /// Entities to color in, each an id from inspect_model ("face:7", "edge:3", "body:1",
    /// "feature:5", or in an assembly "component:2") or a feature name ("Cut-Extrude1", for the
    /// faces it made). Up to 6; each gets its own color, reported in the result.
    #[serde(default)]
    highlight: Vec<String>,
    /// Entities to leave out of this picture (ids or feature names, as for highlight), e.g. a
    /// housing to see inside an assembly. Nothing is hidden in SolidWorks.
    #[serde(default)]
    hide: Vec<String>,
    /// Entities to draw see-through (ids or feature names), or ["others"] for everything not
    /// highlighted.
    #[serde(default)]
    transparent: Vec<String>,
    /// Frame these (ids or feature names) instead of the whole model, to zoom in on them.
    #[serde(default)]
    zoom_to: Vec<String>,
    /// Cut the model with a plane; the cut material is drawn solid (tan, or a darker
    /// highlight color).
    #[serde(default)]
    section: Option<SectionParams>,
    /// Size of each view in pixels (square), 256 to 1024. Defaults to 512.
    #[serde(default)]
    size_px: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SectionParams {
    /// The plane's normal, model axes; everything on the side it points to is cut away.
    normal: [f32; 3],
    /// A point on the plane, model mm. Defaults to the middle of what's shown.
    #[serde(default)]
    point_mm: Option<[f32; 3]>,
}

/// render_views' id lists, trimmed, as given: what its result names.
struct PickNames {
    highlight: Vec<String>,
    hide: Vec<String>,
    transparent: Vec<String>,
    zoom_to: Vec<String>,
    /// `transparent: ["others"]`.
    others: bool,
}

/// render_views' tiles: `views`, else the default grid, or with a section
/// the two section views.
fn render_cameras(
    views: Option<&[render::ViewSpec]>,
    section_normal: Option<render::Vec3>,
) -> Result<Vec<render::Camera>, McpError> {
    match (views, section_normal) {
        (None, None) => Ok(render::View::DEFAULT_GRID.map(render::Camera::standard).to_vec()),
        (None, Some(normal)) => Ok(render::Camera::section_views(normal).to_vec()),
        (Some(views), _) if views.is_empty() || views.len() > MAX_VIEWS => Err(McpError::invalid_params(
            format!("give between 1 and {MAX_VIEWS} views"),
            None,
        )),
        (Some(views), _) => views
            .iter()
            .map(|&view| render::Camera::from_spec(view))
            .collect::<Result<_, _>>()
            .map_err(|err| McpError::invalid_params(err, None)),
    }
}

/// As square as possible: 1-3 in a row, 4 as 2x2, 5-6 as 3x2, ...
fn grid_columns(views: usize) -> usize {
    match views {
        n @ 1..=3 => n,
        4 => 2,
        _ => 3,
    }
}

/// "through (x, y, z) mm, normal (x, y, z)", for render_views' messages.
fn describe_plane(plane: &render::Plane) -> String {
    let (p, n) = (plane.point, plane.normal);
    format!(
        "through ({:.1}, {:.1}, {:.1}) mm, normal ({:.2}, {:.2}, {:.2})",
        p.x, p.y, p.z, n.x, n.y, n.z
    )
}

/// The section plane through `section.point_mm`, or the middle of `model`;
/// refused if it cuts away all of `model`.
fn section_plane(
    section: &SectionParams,
    normal: render::Vec3,
    model: &render::Model,
) -> Result<render::Plane, McpError> {
    let point = section.point_mm.map_or_else(
        || model.bounding_box().map_or(render::Vec3::ZERO, |b| b.center()),
        render::Vec3::from_array,
    );
    let plane = render::Plane { point, normal };
    if !plane.keeps_any(model) {
        return Err(McpError::invalid_params(
            format!(
                "the section plane {} cuts away everything that's shown: the side its normal \
                 points to is removed",
                describe_plane(&plane)
            ),
            None,
        ));
    }
    Ok(plane)
}

/// render_views' result lines for what `names` asked for: each one's color
/// or what was done with it, or that it isn't visible.
fn describe_picks(names: &PickNames, matched: &render::Matched) -> String {
    let mut text = String::new();
    if !names.zoom_to.is_empty() && matched.zoom.iter().any(|&n| n > 0) {
        text += "\nZoomed in: tiles frame what zoom_to names.";
    }
    if names.others {
        text += "\nEverything not highlighted is transparent.";
    }
    let not_visible = "not visible (deleted, hidden, or on a hidden body?)";
    for ((name, count), (color, _)) in names.highlight.iter().zip(&matched.highlight).zip(render::HIGHLIGHTS) {
        text += &match count {
            0 => format!("\n{name}: {not_visible}"),
            _ => format!("\n{name}: {color}"),
        };
    }
    for (list, counts, done) in [
        (&names.hide, &matched.hide, "left out"),
        (&names.transparent, &matched.transparent, "transparent"),
        (&names.zoom_to, &matched.zoom, "framed"),
    ] {
        for (name, count) in list.iter().zip(counts) {
            text += &match count {
                0 => format!("\n{name}: {not_visible}"),
                _ => format!("\n{name}: {done}"),
            };
        }
    }
    text
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
    /// What it measures: sketch entity ids from inspect_sketch or the sketch tool ("line:3",
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
    /// 3D sketches only: lines along the model's X axis.
    #[serde(rename = "along_x")]
    AlongX,
    /// 3D sketches only: lines along the model's Y axis.
    #[serde(rename = "along_y")]
    AlongY,
    /// 3D sketches only: lines along the model's Z axis.
    #[serde(rename = "along_z")]
    AlongZ,
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
            RelationKind::AlongX => "along_x",
            RelationKind::AlongY => "along_y",
            RelationKind::AlongZ => "along_z",
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
            RelationKind::AlongX => proto::RelationKind::AlongX,
            RelationKind::AlongY => proto::RelationKind::AlongY,
            RelationKind::AlongZ => proto::RelationKind::AlongZ,
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
    /// Only in these configurations (names from the configurations tool). Default: SolidWorks'
    /// choice -- every configuration while the dimension has one value for all, else only the
    /// active one.
    #[serde(default)]
    configurations: Option<Vec<String>>,
    /// true: in every configuration.
    #[serde(default)]
    all_configurations: Option<bool>,
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
    /// The router's tools with slimmed schemas and annotations. tools/list
    /// sends, and calls may use, only those `filter` allows.
    tools: Arc<Vec<rmcp::model::Tool>>,
    /// Which tools are listed and may be called (--modes, --inspect-only).
    filter: Arc<toolsets::Filter>,
    addin: AddinClient,
    /// Short IDs handed out by inspect_model, shared by every tool call.
    ids: Arc<Mutex<IdRegistry>>,
    renderer: Arc<render::Renderer>,
    /// The last mesh render_views got, unplaced; the add-in confirms it's
    /// still good (GetMeshResponse.key) instead of tessellating again.
    mesh: Arc<Mutex<Option<proto::GetMeshResponse>>>,
    /// Sheet geometry of the drawing view entities inspect_drawing listed,
    /// for checking and placing drawing dimensions.
    drawings: Arc<Mutex<drawing::Cache>>,
    /// The active document's units, as of the tool call's start (None:
    /// nothing open, or the add-in didn't answer).
    units: Arc<Mutex<Option<proto::DocumentUnits>>>,
}

#[tool_router]
impl SolidWorksTools {
    fn new(addin: AddinClient, filter: toolsets::Filter) -> Self {
        let tool_router = Self::tool_router();
        let mut tools = tool_router.list_all();
        for tool in &mut tools {
            tool.annotations = toolsets::annotations(&tool.name);
        }
        Self {
            tools: Arc::new(tool_schema::slim_tools(tools)),
            filter: Arc::new(filter),
            tool_router,
            addin,
            ids: Arc::default(),
            renderer: Arc::default(),
            mesh: Arc::default(),
            drawings: Arc::default(),
            units: Arc::default(),
        }
    }

    #[tool(
        description = "The active document: its name, type, file (or that it was never saved) and \
                       units."
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
        description = "Read or change how the active document shows units: unit system, decimal \
                       places, drafting standard. The tools always take and report mm and degrees. \
                       Inch documents also get inch defaults (ANSI Inch holes, ANSI B sheets, ASME \
                       borders and title blocks)."
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
        description = "Save the active document (or `document`, another open one) with no \
                       dialogs: to its own file, or Save As an \
                       absolute path whose extension decides -- .SLDPRT / .SLDASM / .SLDDRW \
                       (copy=true keeps working on the original), or an export: STEP, IGES, \
                       Parasolid, SAT, STL, 3MF; PDF (a page per sheet), DXF, DWG for drawings. \
                       Saving an assembly or drawing saves the changed parts it uses. A part with \
                       rebuild errors is saved, and the reply says so."
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
        // Save As gives the document another file, and so another key; another
        // document saved leaves the active one's alone.
        self.track_document(request.document.is_empty()).await;
        let text = files::save_reply_text(&request, &reply)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Open a part, assembly or drawing by absolute path and make it active, with \
                       no dialogs (an open one is just made active). A STEP, IGES or Parasolid \
                       file is imported into a new unsaved part or assembly: bodies without \
                       history, which recognize_features reads. Reports problems such as missing \
                       components. Ids from another document don't apply."
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
        description = "Close the active or a named open document, saving nothing. One with unsaved \
                       changes is refused unless discard_changes. A part an open assembly or \
                       drawing uses stays loaded for it. The reply says which document is active \
                       afterwards."
    )]
    async fn close_document(
        &self,
        Parameters(params): Parameters<files::CloseDocumentParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = files::close_request(&params)?;
        let reply = self
            .addin
            .close_document(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(files::close_reply_text(&reply))]))
    }

    #[tool(
        description = "List the files a part, assembly or drawing uses, every level: \
                       subassemblies, parts, a drawing's models, a derived part's base part. \
                       Gives where SolidWorks finds each one, and flags files that are missing or \
                       were found somewhere other than their saved path. Works on an open document \
                       or on a file on disk, which it doesn't open. With where_used_in it also \
                       lists the files in a folder that use the document. Check this before \
                       renaming, moving or copying files."
    )]
    async fn file_references(
        &self,
        Parameters(params): Parameters<references::FileReferencesParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = references::run(&self.addin, &params, |status| self.addin_error(status)).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text(reply.to_string())]))
    }

    #[tool(
        description = "Take back the last steps in the active document, like Ctrl+Z; the reply \
                       says which features went or came back and how the volume changed. An edit \
                       of an existing sketch doesn't undo reliably (use delete_sketch_entities). \
                       No redo; hiding a component can't be undone."
    )]
    async fn undo(&self, Parameters(params): Parameters<undo::UndoParams>) -> Result<CallToolResult, McpError> {
        let request = undo::request(&params)?;
        let reply = self.addin.undo(request).await.map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(undo::reply_text(&reply))]))
    }

    #[tool(
        description = "Escape hatch: run a VBA macro inside SolidWorks with the whole API and full \
                       access to this computer; prefer the other tools. `code` has a `Sub main()` \
                       and starts with `Set swApp = Application.SldWorks`; early binding, enum \
                       names and out parameters work; the API is in meters and radians. Report \
                       back with `McpPrint value` (one line per call; MsgBox and Debug.Print don't \
                       reach you). An error stops it and gives its message, not its line; what it \
                       did stays. 60 s at most (a timeout doesn't stop what is running); dialogs \
                       it opens are closed and listed. inspect_model ids don't exist here: find \
                       things by name or position."
    )]
    async fn run_macro(&self, Parameters(params): Parameters<macro_run::RunMacroParams>) -> Result<CallToolResult, McpError> {
        let text = macro_run::run(&self.addin, &params).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Hide or show bodies, components, sketches and reference geometry of the \
                       active part or assembly. Hidden things stay in the model, its mass and the \
                       features built on them; render_views leaves them out. Hide a feature's body \
                       by its body id; a body in an assembly goes with its component."
    )]
    async fn set_visibility(
        &self,
        Parameters(params): Parameters<visibility::SetVisibilityParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            visibility::request(&ids, &params)?
        };
        let reply = self
            .addin
            .set_visibility(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(visibility::reply_text(&params, &reply))]))
    }

    #[tool(
        description = "List, create, activate or delete configurations (variants) of the active \
                       part or assembly. The list gives the active one, what each suppresses and \
                       the dimensions that differ. Values per configuration: set_dimension \
                       `configurations`; suppression: set_suppression."
    )]
    async fn configurations(
        &self,
        Parameters(params): Parameters<configuration::ConfigurationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .configurations(configuration::request(&params)?)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(configuration::configurations_text(&params, &reply))]))
    }

    #[tool(
        description = "Suppress or unsuppress features, mates and top-level components of the \
                       active part or assembly, in the active, named or all configurations. \
                       SolidWorks suppresses what depends on a feature too (named in the reply); a \
                       suppressed component is unloaded with its mates. Read back per \
                       configuration; reports features that now fail."
    )]
    async fn set_suppression(
        &self,
        Parameters(params): Parameters<configuration::SetSuppressionParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            configuration::suppression_request(&ids, &params)?
        };
        let reply = self
            .addin
            .set_suppression(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(configuration::suppression_text(&params, &reply))]))
    }

    #[tool(
        description = "Move the rollback bar of the active part or assembly `before` or `after` a \
                       feature, or neither to roll forward. Rolled-back features are left out of \
                       the model; new features go in at the bar, the way to add something early in \
                       the history. Saving keeps the bar."
    )]
    async fn rollback(
        &self,
        Parameters(params): Parameters<feature_order::RollbackParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_order::rollback_request(&ids, &params)?
        };
        let reply = self.addin.rollback(request).await.map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(feature_order::rollback_text(&params, &reply))]))
    }

    #[tool(
        description = "Move a feature of the active part or assembly `before` or `after` another \
                       in the tree, with what it absorbed. Order changes the result (a fillet \
                       before a cut). SolidWorks refuses to put a feature above what it's built on \
                       or below what's built on it, naming those. Reports its new neighbours and \
                       features that now fail."
    )]
    async fn reorder_feature(
        &self,
        Parameters(params): Parameters<feature_order::ReorderParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_order::reorder_request(&ids, &params)?
        };
        let reply = self.addin.reorder_feature(request).await.map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(feature_order::reorder_text(&reply))]))
    }

    #[tool(
        description = "List, set or delete equations and global variables of the active part or \
                       assembly: a variable (\"Width\" = \"50\") or a dimension driven by an \
                       expression (\"D1@Boss-Extrude1\" = \"\\\"Width\\\" / 2\"). Names in double \
                       quotes; plain numbers in the document's units, or with units (\"25mm\", \
                       \"30deg\"); trigonometry in degrees. All or nothing; rebuilds and lists \
                       every equation's value. Driven dimensions can't be set with set_dimension. \
                       Values per configuration: design_table."
    )]
    async fn equations(
        &self,
        Parameters(params): Parameters<equation::EquationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let listing = self
            .addin
            .equations(proto::EquationsRequest::default())
            .await
            .map_err(|status| self.addin_error(status))?;
        let request = equation::request(&params, &listing.equations)?;
        let reply = if params.changes_anything() {
            self.addin.equations(request.clone()).await.map_err(|status| self.addin_error(status))?
        } else {
            listing
        };
        let unit = self.active_units().map_or("millimeters", |u| units::length_name(&u));
        Ok(CallToolResult::success(vec![ContentBlock::text(equation::equations_text(&params, &request, &reply, unit))]))
    }

    #[tool(
        description = "Read, write or delete the design table of the active part or assembly: \
                       SolidWorks' Excel table with a row per configuration setting dimensions, \
                       suppression, global variables or properties (a family of parts). Writing \
                       creates the configurations the rows name; configurations without a row keep \
                       their values. Dimensions a table drives can't be changed with \
                       set_dimension. Needs Microsoft Excel."
    )]
    async fn design_table(
        &self,
        Parameters(params): Parameters<design_table::DesignTableParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = design_table::request(&params)?;
        // SolidWorks reports a column or value it can't use in a modal dialog.
        let (reply, dialogs) = macro_run::closing_dialogs(self.addin.design_table(request)).await;
        let reply = reply.map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(design_table::reply_text(&params, &reply, &dialogs))]))
    }

    #[tool(
        description = "Create a new, empty part from the default template and make it active. \
                       `units`, `decimals` and `drafting_standard` set how SolidWorks shows it."
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
        description = "Create a new, empty assembly from the default template and make it active; \
                       insert_components fills it. `units`, `decimals` and `drafting_standard` set \
                       how SolidWorks shows it."
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
        description = "Create a new drawing from a template and make it active. `template`, \
                       `sheet_format` (border and title block) and paper size; an unknown template \
                       or format name lists the ones there are. Reports the sheet. It has no \
                       views."
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
        description = "Draw in a sketch of the active part: `shape` line, circle, arc, spline, \
                       point, slot, polygon, ellipse or rectangle. Into a new sketch on `plane` or \
                       `on` a planar face or reference plane, into the existing `sketch_name` \
                       (combining shapes into one profile), or a new 3D sketch (sketch_3d). Points \
                       are [x, y] in sketch or [x, y, z] in model coordinates (projected onto the \
                       sketch plane), mm. Ends that meet are joined. Reports the new entities' \
                       ids, the sketch's closed and open contours (an extrude needs closed ones) \
                       and its axes in model space."
    )]
    async fn sketch(&self, Parameters(params): Parameters<SketchParams>) -> Result<CallToolResult, McpError> {
        params.check_applies()?;
        if params.shape == SketchShape::Rectangle {
            let width_mm = *params.needs(&params.width_mm, "width_mm")?;
            let target = &params.target.sketch;
            return self
                .rectangle_sketch(
                    target.plane.unwrap_or_default(),
                    target.on.as_deref(),
                    params.center.as_deref(),
                    width_mm,
                    params.height_mm.unwrap_or(width_mm),
                )
                .await;
        }
        let mut points = PointConverter::default();
        let (entities, what) = params.entities(&mut points)?;
        self.draw_sketch(params.target, points.model_space(), entities, what).await
    }

    #[tool(
        description = "Round sketch corners with arcs of one radius (Sketch Fillet), at corner \
                       points or between two entities (extended to meet if needed). Lines and arcs \
                       shorten; what was dimensioned to the corner stays on a point there. Adds a \
                       radius dimension. Reports the new arcs, what changed, the contours, the \
                       definition state and features that now fail. All or nothing."
    )]
    async fn sketch_fillet(
        &self,
        Parameters(params): Parameters<sketch_edit::SketchFilletParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sketch_edit::fillet_request(&ids, &params)?
        };
        let reply = self.addin.sketch_corners(request).await.map_err(|status| {
            delete::with_legend(self.addin_error(status), "entity", &sketch_edit::given(&params.entities))
        })?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = format!(
            "filleted {} with radius {} mm in {}.{}",
            sketch_edit::given(&params.entities).join(", "),
            params.radius_mm,
            reply.sketch_name,
            sketch_edit::edit_text(&mut ids, &reply)
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Cut sketch corners with a straight line (Sketch Chamfer), at corner points \
                       or between two touching entities; with two entities the sides may differ. \
                       Adds dimensions and keeps the sharp corner as a point. Reports the new \
                       lines, what changed, the contours, the definition state and features that \
                       now fail. All or nothing."
    )]
    async fn sketch_chamfer(
        &self,
        Parameters(params): Parameters<sketch_edit::SketchChamferParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sketch_edit::chamfer_request(&ids, &params)?
        };
        let reply = self.addin.sketch_corners(request).await.map_err(|status| {
            delete::with_legend(self.addin_error(status), "entity", &sketch_edit::given(&params.entities))
        })?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = format!(
            "chamfered {} in {}.{}",
            sketch_edit::given(&params.entities).join(", "),
            reply.sketch_name,
            sketch_edit::edit_text(&mut ids, &reply)
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Trim a sketch entity: remove the piece at `at` between whatever crosses it \
                       (all of it if nothing does), or with corner_with grow it and another into a \
                       corner. A piece from a line's middle leaves two lines, a trimmed circle is \
                       an arc with a new id: use the reply's ids. One piece per call."
    )]
    async fn trim_sketch_entity(
        &self,
        Parameters(params): Parameters<sketch_edit::TrimParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sketch_edit::trim_request(&ids, &params)?
        };
        let legend = [params.entity.trim().to_string()];
        let reply = self
            .addin
            .trim_sketch_entity(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "entity", &legend))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = format!(
            "trimmed {} in {}.{}",
            params.entity.trim(),
            reply.sketch_name,
            sketch_edit::edit_text(&mut ids, &reply)
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Extend the end of a sketch line or arc nearest `at` to the next entity in \
                       its way; again goes on to the one after. Reports what changed, the \
                       contours, the definition state and features that now fail."
    )]
    async fn extend_sketch_entity(
        &self,
        Parameters(params): Parameters<sketch_edit::ExtendParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sketch_edit::extend_request(&ids, &params)?
        };
        let legend = [params.entity.trim().to_string()];
        let reply = self
            .addin
            .trim_sketch_entity(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "entity", &legend))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = format!(
            "extended {} in {}.{}",
            params.entity.trim(),
            reply.sketch_name,
            sketch_edit::edit_text(&mut ids, &reply)
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Project model edges and faces, or other sketches' entities, into a sketch \
                       (Convert Entities): the new entities follow what they came from and need no \
                       dimensions. A face brings its loops, an edge only itself. A single face \
                       defaults to a new sketch on itself; in a new 3D sketch edges keep their \
                       place in space (a sweep path). Reports the new ids, the contours, the \
                       definition state and features that now fail."
    )]
    async fn convert_entities(
        &self,
        Parameters(params): Parameters<sketch_edit::ConvertParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sketch_edit::convert_request(&ids, &params)?
        };
        let reply = self.addin.convert_entities(request).await.map_err(|status| {
            delete::with_legend(self.addin_error(status), "entity", &sketch_edit::given(&params.entities))
        })?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = sketch_edit::convert_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Extrude a closed sketch of the active part into a solid, merging with any \
                       body, or with cut=true remove material to a depth or through_all. A cut \
                       goes against the sketch normal (into the part, for a sketch on its face); \
                       if that removes nothing it goes the other way and says so."
    )]
    async fn extrude_sketch(
        &self,
        Parameters(params): Parameters<ExtrudeSketchParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.through_all && !params.cut {
            return Err(McpError::invalid_params("through_all is for cuts (cut=true)", None));
        }
        let depth_mm = match (params.through_all, params.depth_mm) {
            (true, _) => 0.0,
            (false, Some(d)) if d.is_finite() && d > 0.0 => d,
            (false, Some(d)) => {
                return Err(McpError::invalid_params(
                    format!("depth must be a positive number of millimeters, got {d}"),
                    None,
                ));
            }
            (false, None) if params.cut => {
                return Err(McpError::invalid_params("give depth_mm, or set through_all", None));
            }
            (false, None) => return Err(McpError::invalid_params("give depth_mm", None)),
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
            cut: params.cut,
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
        let (verb, flipped) = match (params.cut, result.direction_flipped) {
            (false, _) => ("extruding", ""),
            (true, false) => ("cutting with", ""),
            (true, true) => (
                "cutting with",
                " It cut along the sketch normal (reversed): against it there was nothing to remove.",
            ),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "created {} by {verb} {} {how}.{flipped}{}",
            result.feature_name,
            result.sketch_name,
            sketch_status_note(&result.sketch_name, result.sketch_status())
        ))]))
    }

    #[tool(
        description = "Revolve a closed sketch of the active part about an axis into a solid (a \
                       shaft, knob, flange), or with cut=true remove material (a groove, a turned \
                       recess). Draw the half profile beside a construction line in the same \
                       sketch, or give `axis`; it may touch the axis, not cross it. A partial-turn \
                       cut that would remove nothing goes the other way and says so."
    )]
    async fn revolve_sketch(
        &self,
        Parameters(params): Parameters<revolve::RevolveParams>,
    ) -> Result<CallToolResult, McpError> {
        self.revolve(params).await
    }

    #[tool(
        description = "Sweep a closed profile sketch of the active part along a `path` sketch into \
                       a solid (a handle, pipe, spring), or with cut=true remove material (a \
                       groove along a curve, a thread-like cut). Draw the path, then the profile \
                       in its own sketch on a plane across the path's start; or circle_diameter_mm \
                       sweeps a round section with no profile sketch."
    )]
    async fn sweep_sketch(
        &self,
        Parameters(params): Parameters<sweep::SweepParams>,
    ) -> Result<CallToolResult, McpError> {
        self.sweep(params).await
    }

    #[tool(
        description = "Loft through profile sketches of the active part, in order, into a solid (a \
                       square-to-round transition, a bottle neck, a pyramid), or with cut=true \
                       remove material (a tapered pocket). Each profile is one closed contour in \
                       its own sketch on its own plane, e.g. Front and a reference plane offset \
                       from it."
    )]
    async fn loft_sketch(
        &self,
        Parameters(params): Parameters<loft::LoftParams>,
    ) -> Result<CallToolResult, McpError> {
        self.loft(params).await
    }

    #[tool(
        description = "Round edges of the active part (or all edges of given faces) with a \
                       constant-radius fillet; tangent edges too unless tangent_propagation=false. \
                       Usually after the extrudes and cuts that make the edges."
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
        description = "Bevel edges of the active part (or all edges of given faces) with a \
                       chamfer: equal distances, angle-distance or distance-distance. Tangent \
                       edges are beveled too unless tangent_propagation=false."
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
        description = "Make a helix or flat spiral curve in the active part from a sketch holding \
                       one circle: the circle sets the diameter, its plane the axis. Give two of \
                       pitch_mm, height_mm and revolutions (spiral: pitch_mm and revolutions). Not \
                       solid: a sweep path for a spring, coil or thread. The reply says where it \
                       starts and ends."
    )]
    async fn create_helix(
        &self,
        Parameters(params): Parameters<helix::HelixParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = helix::request(&params)?;
        let reply = self
            .addin
            .create_helix(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(helix::reply_text(&params, &reply))]))
    }

    #[tool(
        description = "Hollow a solid body of the active part into walls of even thickness (an \
                       enclosure, a tray), removing open_faces as openings, or closed with a \
                       cavity. One body per call. Usually after the extrudes and outside fillets, \
                       before features inside the cavity."
    )]
    async fn shell_body(
        &self,
        Parameters(params): Parameters<shell::ShellParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            shell::request(&ids, &params)?
        };
        let reply = self
            .addin
            .shell_body(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(shell::reply_text(&params, &reply))]))
    }

    #[tool(
        description = "Make a rib: a thin wall of the active part grown from a sketch until it \
                       meets the body, e.g. a web across a tray. The sketch, usually an open line \
                       or chain where the rib's free edge goes, may stop short of the walls, but \
                       its ends must run into material. Grows within the sketch plane (or \
                       normal_to_sketch); if the default way makes nothing it goes the other way \
                       and says so."
    )]
    async fn rib_sketch(
        &self,
        Parameters(params): Parameters<rib::RibParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = rib::request(&params)?;
        let reply = self
            .addin
            .rib_sketch(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            rib::reply_text(&params, &reply) + &sketch_status_note(&reply.sketch_name, reply.sketch_status()),
        )]))
    }

    #[tool(
        description = "Taper faces of the active part about a neutral plane (draft), so a molded \
                       or cast part leaves its mold. The reply says whether the body grew or \
                       shrank (flip leans the other way). For the sides of one extrude, \
                       edit_extrude draft_deg is simpler."
    )]
    async fn draft_faces(
        &self,
        Parameters(params): Parameters<draft::DraftParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            draft::request(&ids, &params)?
        };
        let reply = self
            .addin
            .draft_faces(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(draft::reply_text(&params, &reply))]))
    }

    #[tool(
        description = "Mirror features of the active part about a plane (copies on the other \
                       side), or whole bodies, e.g. to finish a half-modelled part about its cut \
                       face (merged unless merge=false)."
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
                       direction2, spacing_mm apart. Which way an edge points isn't obvious: if \
                       the copies go the wrong way, edit_pattern reverse_direction=true."
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
                       holes round a flange. count includes the original; spread evenly all the \
                       way round unless angle_deg or spacing_deg."
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
        description = "Look at the active part or assembly: the feature tree (failing features \
                       carry an error; diagnose_model explains), bodies, faces with geometry, \
                       edges, and in an assembly components and mates. Every entity has an id \
                       (\"face:7\") other tools take, valid across edits while it exists; `within` \
                       narrows to a body, feature, face or component. mm, in model (or assembly) \
                       space."
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
        description = "Recognize what the active part's bodies are made of, for imported geometry \
                       with no history (works on any body): holes (counterbore, countersink, \
                       steps, depth, direction), bosses, fillets, rounded ends, chamfers, and the \
                       planar faces by direction and position. Every item gives its face ids. mm, \
                       degrees, model space."
    )]
    async fn recognize_features(
        &self,
        Parameters(params): Parameters<recognize::RecognizeFeaturesParams>,
    ) -> Result<CallToolResult, McpError> {
        let (scope_persist_id, scope_feature_name) = match params.within.as_deref().map(str::trim) {
            None | Some("") => (Vec::new(), String::new()),
            Some(within) if IdRegistry::looks_like_id(within) => (self.resolve_id(within)?, String::new()),
            Some(feature_name) => (Vec::new(), feature_name.to_string()),
        };
        let request = proto::InspectModelRequest {
            include_features: true,
            include_faces: true,
            include_edges: true,
            scope_persist_id,
            scope_feature_name,
            ..Default::default()
        };
        let mut reply = self
            .addin
            .inspect_model(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        assembly::place_inspect_reply(&mut reply);
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            recognize::render(&mut ids, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Read the 3D annotations (MBD / PMI) of the active part or assembly: \
                       dimensions with tolerances, datum symbols, feature control frames with a \
                       one-line callout, datum targets, surface finish symbols, notes, each with \
                       the face / edge ids it's attached to. Feature-driving dimensions only if \
                       asked."
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
                       or assembly (3D PMI / MBD) or drawing, with a leader to `on`, on a size \
                       `dimension` (drawings), or free. The frame is read back: the reply gives \
                       its callout (\"position | dia 0.1 (M) | A | B | C\"), its name (for \
                       delete_drawing_annotations, parts too) and where it went."
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
        description = "Add a datum feature symbol (the boxed letter on a triangle that makes a \
                       face, edge or feature of size datum A, B, ...) to the active part, assembly \
                       or drawing, for add_geometric_tolerance to reference. A hole's or shaft's \
                       cylindrical face makes its axis the datum. A letter another datum symbol \
                       has is refused."
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
        description = "Add a surface finish (texture) symbol to the active drawing, part or \
                       assembly: with a leader to `on`, standing on it (no_leader), or free at \
                       at_mm (a drawing's general finish). Texts show as written. Drawn by the \
                       document's drafting standard. delete_drawing_annotations removes it."
    )]
    async fn add_surface_finish_symbol(
        &self,
        Parameters(params): Parameters<gdt::AddSurfaceFinishParams>,
    ) -> Result<CallToolResult, McpError> {
        let symbol = gdt::surface_finish(&params)?;
        self.add_gdt(symbol, &params.place, Vec::new()).await
    }

    #[tool(
        description = "Add a weld symbol to the active drawing, part or assembly: a leader to the \
                       joint (`on`) and a reference line at at_mm, with the weld on the arrow \
                       side, the other side or both, drawn as `standard` places them. \
                       delete_drawing_annotations removes it."
    )]
    async fn add_weld_symbol(
        &self,
        Parameters(params): Parameters<gdt::AddWeldSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        let units = self.active_units();
        let (symbol, remarks) = gdt::weld(&params, units.as_ref())?;
        self.add_gdt(symbol, &params.place, remarks).await
    }

    #[tool(
        description = "Add center marks to a view of the active drawing: one per circle or arc in \
                       `on`, or with `group` one over them all; without `on`, every hole (or \
                       `throughout`) of `view`. Circles already marked are left alone -- \
                       SolidWorks may mark them when a view is made, so check the view's \
                       annotations first. Reports each mark's name and centers."
    )]
    async fn add_center_marks(
        &self,
        Parameters(params): Parameters<center_mark::AddCenterMarksParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            center_mark::marks_request(&ids, &cache, &params)?
        };
        let reply = self
            .addin
            .add_center_marks(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            center_mark::reply(&mut ids, "center mark(s)", &request.view_name, &params.on, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Add centerlines to a view of the active drawing: midway between each pair \
                       of parallel edges or silhouettes in `between`, or without it SolidWorks' \
                       own on the view's cylinders."
    )]
    async fn add_centerlines(
        &self,
        Parameters(params): Parameters<center_mark::AddCenterlinesParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            center_mark::lines_request(&ids, &cache, &params)?
        };
        let reply = self
            .addin
            .add_centerlines(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let asked: Vec<String> = params.between.iter().map(|[a, b]| format!("{a} / {b}")).collect();
        let text = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            center_mark::reply(&mut ids, "centerline(s)", &request.view_name, &asked, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Look at the active drawing: its sheets (paper, scale, projection) and views \
                       (type, orientation, scale, model, outline on the sheet, which model axes \
                       point right / up). With `view`, also its visible edges with ids \
                       (\"view_edge:3\"; \"silhouette:1\" for a curved face's outline) and sheet \
                       positions, and its dimensions and annotations. Sheet mm from the lower-left \
                       corner; sizes are the model's."
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
                       inspect_drawing lists for a view. They show the model's sizes and don't \
                       change it; insert_model_items brings in the model's own, driving dimensions \
                       instead. Text goes at at_mm or by default outside the view next to what it \
                       measures. All of a call's dimensions are added, or none."
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
        description = "Insert the model's own dimensions (those marked for drawing by default) \
                       into drawing views where they sit in its sketches, optionally also hole \
                       callouts, datums, frames, symbols, notes and cosmetic threads. Unlike \
                       add_drawing_dimensions they stay linked: changing one changes the part. \
                       Skips dimensions shown in another view. Reports what each view gained."
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
        description = "A PNG of a sheet of the active drawing as printed, with rulers in sheet mm, \
                       to check the layout: overlapping or crowded text, dimensions crossing \
                       views, things off the sheet. `view` or `region_mm` zooms in. About 1-2k \
                       tokens per picture."
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
        description = "Add a sheet to the active drawing and make it active. It copies the active \
                       sheet's paper size, sheet format, scale and projection unless given. It has \
                       no views."
    )]
    async fn add_sheet(
        &self,
        Parameters(params): Parameters<sheet::AddSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_sheet(sheet::add_request(&params)?).await
    }

    #[tool(
        description = "Make another sheet of the active drawing the active one (the one SolidWorks \
                       shows and the drawing tools default to). Reports its setup and views."
    )]
    async fn activate_sheet(
        &self,
        Parameters(params): Parameters<sheet::ActivateSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_sheet(sheet::activate_request(&params)?).await
    }

    #[tool(
        description = "Change a sheet of the active drawing (default: the active one): paper size, \
                       sheet format, scale, projection, name. Views keep their positions; the \
                       reply lists any now past the edges. Changes apply together, or not at all."
    )]
    async fn edit_sheet(
        &self,
        Parameters(params): Parameters<sheet::EditSheetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_sheet(sheet::edit_request(&params)?).await
    }

    #[tool(
        description = "Add a view to a sheet of the active drawing: `type` model (a part or \
                       assembly), or from an existing `view` projected, section, detail or \
                       auxiliary. Reports the view's outline and which model axes point right / up \
                       on the sheet; inspect_drawing with the view then lists its edges."
    )]
    async fn create_drawing_view(
        &self,
        Parameters(params): Parameters<derived_view::DrawingViewParams>,
    ) -> Result<CallToolResult, McpError> {
        use derived_view::ViewRequest;
        let params = match params.split()? {
            ViewRequest::Model(params) => return self.setup_drawing_view(view::create_request(&params)?).await,
            derived => derived,
        };
        let drawing = self.inspect_views().await?;
        let plan = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match params {
                ViewRequest::Model(_) => unreachable!("made above"),
                ViewRequest::Projected(params) => derived_view::projected_plan(&drawing, &params)?,
                ViewRequest::Section(params) => derived_view::section_plan(&drawing, &ids, &cache, &params)?,
                ViewRequest::Detail(params) => derived_view::detail_plan(&drawing, &cache, &params)?,
                ViewRequest::Auxiliary(params) => derived_view::auxiliary_plan(&drawing, &ids, &cache, &params)?,
            }
        };
        self.derive_drawing_view(plan).await
    }

    #[tool(
        description = "Change a view of the active drawing on any sheet: model, configuration, \
                       orientation (views projected from it turn with it), position (aligned views \
                       slide only along their alignment), scale, display, name. Changes apply \
                       together, or not at all."
    )]
    async fn edit_drawing_view(
        &self,
        Parameters(params): Parameters<view::EditDrawingViewParams>,
    ) -> Result<CallToolResult, McpError> {
        self.setup_drawing_view(view::edit_request(&params)?).await
    }

    #[tool(
        description = "Crop a view of the active drawing to a circle or rectangle (sheet mm), \
                       replacing any crop it has; `remove` takes it off. A detail or broken view \
                       can't be cropped."
    )]
    async fn crop_view(
        &self,
        Parameters(params): Parameters<derived_view::CropViewParams>,
    ) -> Result<CallToolResult, McpError> {
        self.shape_drawing_view(derived_view::crop_request(&params)?).await
    }

    #[tool(
        description = "Break a view of the active drawing: hide ranges of a long part and close \
                       the gaps; dimensions keep the true lengths. Ranges are in the sheet \
                       coordinates inspect_drawing lists for the view's entities, which don't move \
                       when it's broken. Adds to its breaks; `remove` takes them all off. A \
                       cropped view can't be broken."
    )]
    async fn break_view(
        &self,
        Parameters(params): Parameters<derived_view::BreakViewParams>,
    ) -> Result<CallToolResult, McpError> {
        self.shape_drawing_view(derived_view::break_request(&params)?).await
    }

    #[tool(
        description = "List the annotations of the active drawing: name (for \
                       edit_drawing_annotation / delete_drawing_annotations), whether it belongs \
                       to a view, the sheet or the sheet format, text, position on the sheet and \
                       in its view, box, style, layer and leaders with what they're attached to."
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
        description = "Add a note to the active drawing: in a view (moving with it), on the sheet, \
                       or in the sheet format (title block layer); with leaders to view entities \
                       if `attach_to`. The reply says where it ended up and flags notes over a \
                       view they don't belong to, far from their view, or off the sheet."
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
        description = "Change an annotation of the active drawing by the name \
                       inspect_drawing_annotations lists: move it, change its layer, put it in a \
                       view; for notes also text, font, border and leaders. Title block notes too. \
                       Changes apply together, or not at all."
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
        description = "Delete annotations of the active drawing (notes, dimensions, balloons, \
                       frames, symbols, center marks, centerlines, tables, title block notes) by \
                       the names inspect_drawing_annotations lists, or of the active part or \
                       assembly by the names inspect_annotations lists. Nothing is deleted if a \
                       name isn't found."
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
        description = "List the tables of the active drawing (BOMs, hole, general and revision \
                       tables): name, kind, position and box, columns and every row's cells; BOM \
                       rows name their components."
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
        description = "Add a native table to the active drawing: a bill of materials of an \
                       assembly view (kept up to date by SolidWorks; use it instead of a parts \
                       list of notes), a hole table, a general table of text, or the sheet's \
                       revision table (only the first sheet's lists revisions; later sheets' say \
                       \"See Sheet1\"). Placed clear of views, tables and the title block unless \
                       at_mm."
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
                       lists: cells, rows, revisions, column widths, title, layer, position. All \
                       or nothing. Delete one with delete_drawing_annotations."
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
        description = "Add BOM balloons to an assembly view of the active drawing, each showing a \
                       component's item number from the drawing's BOM (add_drawing_table kind bom \
                       first); use these, not numbered notes. Default: every component of the view \
                       (Auto Balloon); or `components` by name, or `attach_to` view entities. \
                       Balloons are notes: edit_drawing_annotation / delete_drawing_annotations \
                       work on them."
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
        description = "Draw a border into a sheet's format (behind the views on every print): a \
                       frame, zones with labels, centering marks, with defaults by ISO 5457 or \
                       ASME Y14.1. On its own layer: drawing it again replaces it. Then \
                       draw_title_block."
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
        description = "Draw a title block into a sheet's format: rows of cells, each a label and a \
                       value showing a custom property (updating by itself) or fixed text. In the \
                       border's lower-right corner by default. On its own layer: drawing it again \
                       replaces it. Fill it with fill_title_block."
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
        description = "List a sheet's title block fields: each custom property its notes show, \
                       whose it is (drawing or model), its value and the note."
    )]
    async fn inspect_title_block(
        &self,
        Parameters(params): Parameters<sheet_format::InspectTitleBlockParams>,
    ) -> Result<CallToolResult, McpError> {
        let json = self.title_block_fields(params.sheet.as_deref().unwrap_or("").trim()).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Fill in a sheet's title block: values for the fields inspect_title_block \
                       lists, each where the title block reads it from (drawing or model; save the \
                       model too). Names it doesn't show are refused unless allow_unlisted; \
                       properties linked to SolidWorks values unless replace_links. Part number, \
                       revision, material by meaning: drawing_properties."
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
        description = "Read and set custom properties of the active document, an open `document`, \
                       or the model a drawing `sheet` takes $PRPSHEET values from; also the part \
                       number BOMs list. Properties linked to what SolidWorks works out (Material \
                       = \"SW-Material@part\") are kept unless replace_links. Lists every property \
                       afterwards. Physical material: part_material."
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
        description = "Set the properties a drawing shows by meaning -- part number, revision, \
                       description, material text -- each where the sheet's title block reads it \
                       from (drawing or model, under its note's name), else the usual property; \
                       plus any other drawing / model properties. Nothing given: lists both. \
                       Physical material: part_material. Save the model too."
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
        description = "Read, assign, search or remove a part's physical material from the material \
                       databases (\"AISI 1020\", \"6061-T6 (SS)\"), which sets density, mass, \
                       appearance and hatch: the active part, an open `document`, or a drawing \
                       sheet's part. Not the Material text a title block shows: the reply says \
                       whether that follows this material; link_property makes it. Reports density \
                       and mass."
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
        description = "A PNG of the active part or assembly, shaded, in orthographic views at one \
                       scale, to check the shape after edits (which side a cut went, where a \
                       component is) and, with `highlight`, which face or edge an id is. To look \
                       inside: `section`, `hide`, `transparent`; `zoom_to` for small details. Read \
                       sizes from inspect_model, not the picture."
    )]
    async fn render_views(
        &self,
        Parameters(params): Parameters<RenderViewsParams>,
    ) -> Result<CallToolResult, McpError> {
        let section_normal = match &params.section {
            None => None,
            Some(section) => Some(
                render::Vec3::from_array(section.normal)
                    .try_normalize()
                    .ok_or_else(|| McpError::invalid_params("section normal has no length", None))?,
            ),
        };
        let views = render_cameras(params.views.as_deref(), section_normal)?;
        let columns = grid_columns(views.len());
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
        let (picks, names) = self.render_picks(&params)?;

        let mut mesh = self.mesh().await?;
        assembly::place_mesh(&mut mesh);
        let (model, matched) = render::model_from_mesh(&mesh, &picks);
        if model.faces.is_empty() {
            return Err(McpError::invalid_params(
                if mesh.faces.is_empty() {
                    "the active document has nothing to draw yet (no visible bodies or components) \
                     -- create some geometry first"
                } else {
                    "`hide` leaves nothing to draw"
                },
                None,
            ));
        }
        let triangles = model.triangle_count();
        let edge_count = model.edges.len();
        let section = params
            .section
            .as_ref()
            .zip(section_normal)
            .map(|(section, normal)| section_plane(section, normal, &model))
            .transpose()?;

        let request = render::Request {
            model,
            views: views.clone(),
            columns,
            tile_px,
            section,
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
                    Some(axes) => format!("{} ({axes})", v.label),
                    None => v.label.clone(),
                })
                .collect();
            text += &format!("\n  row {}: {}", row + 1, tiles.join(" | "));
        }
        if let Some(plane) = section {
            text += &format!(
                "\nSection {}: the side the normal points to is cut away; cut material is tan \
                 (highlighted: its color, darker).",
                describe_plane(&plane)
            );
        }
        text += &describe_picks(&names, &matched);
        if mesh.truncated {
            text += "\nwarning: the model is too detailed to draw completely; some faces or edges are missing";
        }
        Ok(CallToolResult::success(vec![
            ContentBlock::image(BASE64.encode(&rendered.png), "image/png"),
            ContentBlock::text(text),
        ]))
    }

    #[tool(
        description = "Look at one sketch: every segment and point with its id (\"line:3\"; end \
                       points and centers are points) and geometry (sketch mm, model mm in a 3D \
                       sketch), its dimensions and relations, and what can still move."
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
        description = "Look at one feature of the active part as Edit Feature shows it: extrude, \
                       revolve, sweep, loft, fillet, chamfer, shell, draft, rib, mirror, pattern, \
                       helix or Hole Wizard settings, and for every feature its dimensions \
                       (\"D1@Revolve1\" for set_dimension). Edge and face ids are of the model \
                       just before the feature."
    )]
    async fn inspect_feature(
        &self,
        Parameters(params): Parameters<feature_inspect::InspectFeatureParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_inspect::request(&ids, &params)?
        };
        let reply = self
            .addin
            .inspect_feature(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let json = {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_inspect::render(&mut ids, &reply)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(json.to_string())]))
    }

    #[tool(
        description = "Add dimensions to a sketch, to give it exact sizes and positions: with \
                       value_mm / value_deg the geometry is driven to it, without the current size \
                       is locked. All of a call's dimensions are added, or none (e.g. if one would \
                       over-define). Reports the new dimension names and what can still move."
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
        description = "Add geometric relations between sketch entities (or with \"origin\" or \
                       model edges / faces); they can move geometry to satisfy them. All of a \
                       call's relations are added, or none. Reports what in the sketch can still \
                       move."
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
        description = "Fully define a sketch (Fully Define Sketch): adds the relations and \
                       dimensions still missing, locking the geometry where it is, measured from \
                       the origin or the given datums. Best after dimension_sketch / \
                       add_sketch_relations set what matters. Reports what it added and whether \
                       the sketch is now fully defined."
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
        description = "Change a dimension of the active part (\"D1@Sketch1\", \
                       \"D1@Boss-Extrude1\") or an assembly's mate (\"D1@Distance1\") and rebuild. \
                       With several configurations, SolidWorks picks where unless `configurations` \
                       or all_configurations; the reply gives each one's value. Dimensions driven \
                       by equations or a design table are refused."
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
        let (scope, configurations) = configuration::scope(
            params.configurations.as_deref(),
            params.all_configurations,
            proto::ConfigurationScope::Unspecified,
        )?;
        let reply = self
            .addin
            .set_dimension_value(proto::SetDimensionValueRequest {
                name,
                value: Some(value),
                scope,
                configurations,
            })
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut text = match &reply.dimension {
            Some(d) => format!("set {}; the model was rebuilt.", sketch::dimension_text(d)),
            None => "set the dimension; the model was rebuilt.".to_string(),
        };
        text += &configuration::values_text(&reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Set the tolerance of a dimension in a drawing, part or assembly: bilateral, \
                       limit, symmetric, fits, basic, min, max or none; angles take the _deg \
                       fields. A model dimension shown in a drawing is the model's: its tolerance \
                       changes there too. A refused change is put back. Replies with the dimension \
                       and its resulting limits."
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
        description = "Add Hole Wizard holes: clearance or drilled, counterbore, countersink or \
                       tapped, sized from a hole standard (kind=\"counterbore\", size=\"M5\"). At \
                       the points of a sketch (sketch_name; its dimensions keep locating the \
                       holes), or `on` a face at model `locations`. All the holes are one feature. \
                       For a non-standard round hole, a circle and a cut extrude is simpler."
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
        description = "Mate two components of the active assembly, like the Mate command; distance \
                       / angle mates with min and max are limit mates (sweep_mate runs them \
                       through their range). SolidWorks moves the components that aren't fixed. A \
                       mate that's rejected, conflicts or can't be solved is removed again and the \
                       reason given. Reports where the mated components ended up."
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
        description = "Insert parts or subassemblies into the active assembly from a saved `file`, \
                       or more instances of a component (`copy_of`), one per `at` entry. The first \
                       component of an empty assembly is fixed, the others float. Reports the new \
                       components' ids and placements; mate them with add_mate."
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
        description = "Replace top-level components of the active assembly with another file, in \
                       place. SolidWorks re-attaches mates to matching faces where it can; the \
                       reply lists the mates that came through and the unresolved ones with what \
                       each lost (fix with edit_mate or delete_features)."
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
        description = "Fix or float top-level components of the active assembly. SolidWorks then \
                       re-solves: a floated component may move, and fixing one can make mates fail \
                       -- both reported, the change kept. For a part in a subassembly, fix the \
                       subassembly or open its file."
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
        description = "Move a floating top-level component of the active assembly while its mates \
                       solve, like dragging it: a turn, then a translation. It goes as far as the \
                       mates allow, others may follow; the reply gives the asked and actual pose \
                       and what else moved. restore=true checks a motion without keeping it; \
                       check_collisions / stop_at_collision report what it hits. A limit mate \
                       holds at its value here: use sweep_mate along its range."
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
        description = "Run a distance or angle mate of the active assembly through a range (a \
                       limit mate's own by default), checking for collisions at each step: the \
                       reply gives what comes to overlap and between which values, what moved, and \
                       values SolidWorks refuses. Puts everything back unless restore=false. The \
                       way to move along a limit mate. About a second per step; stops after 45 s, \
                       so keep to about 40 steps."
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
        description = "Report each component's remaining degrees of freedom in the active \
                       assembly: the directions it can slide along and axes it can turn about, \
                       which components drag others along, and SolidWorks' defined status. Each is \
                       nudged while the mates solve and put back, so nothing changes. Misses \
                       motion only possible beyond the nudge; a limit mate counts as fixed at its \
                       value (sweep_mate drives it). Under a second per component."
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
        description = "Check an assembly for interference: which components' solids overlap, by \
                       how much (mm³) and where, biggest first. On the active assembly, or from a \
                       drawing on the assembly its views show. Components as they sit now; for \
                       collisions along a motion use move_component check_collisions or \
                       sweep_mate. Fasteners in their holes are flagged. SolidWorks marks the \
                       assembly modified."
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
        description = "Measure in the active part or assembly, like the Measure tool. One entity: \
                       an edge's length or radius, a face's area and perimeter. More: minimum and \
                       center distances, normal distance and angle of parallel planes, parallel / \
                       perpendicular, closest points and X / Y / Z offsets. mm, degrees. Overlap: \
                       check_interference."
    )]
    async fn measure(&self, Parameters(params): Parameters<measure::MeasureParams>) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            measure::measure_request(&ids, &params)?
        };
        let reply = self.addin.measure(request).await.map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(measure::measure_json(&params, &reply).to_string())]))
    }

    #[tool(
        description = "Mass properties of the active part or assembly: mass, volume, area, \
                       density, center of mass, principal moments and axes, and the inertia tensor \
                       at the center of mass and at the origin or a coordinate system (products of \
                       inertia as SolidWorks shows them, Pxy = +∫xy dm). Mass comes from the \
                       materials (none: 1000 kg/m³) and mass overrides. kg, mm."
    )]
    async fn mass_properties(
        &self,
        Parameters(params): Parameters<measure::MassPropertiesParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            measure::mass_request(&ids, &params)?
        };
        let reply = self.addin.get_mass_properties(request).await.map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(measure::mass_json(&params, &reply).to_string())]))
    }

    #[tool(
        description = "Delete features of the active part or assembly (sketches, extrudes, holes, \
                       fillets, patterns, reference geometry, mates, ...), like Delete in the \
                       feature tree. If other features depend on them nothing is deleted and \
                       they're named, unless `children` says otherwise. The default planes, origin \
                       and components can't be deleted. Reports what's gone and what now fails. \
                       Sketch geometry: delete_sketch_entities."
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
        description = "Delete segments and points from a sketch, with their dimensions and \
                       relations (a segment's end and center points go with it). Reports what \
                       went, the contours, the definition state and features that now fail. A \
                       whole sketch: delete_features."
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
        description = "Mirror sketch entities about a line of their sketch, usually a construction \
                       centerline; each copy gets a symmetric relation, so dimensioning one half \
                       defines both. A line ending square on the axis is extended across it, so \
                       mirroring an open half profile closes it. Reports the new and extended ids, \
                       the contours, the definition state and features that now fail."
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
        description = "Offset sketch entities by a distance (Offset Entities), the copies \
                       following through an offset relation; or model edges and faces into a \
                       sketch (a face: its outer loop). Reports the new ids, the contours, the \
                       definition state and features that now fail; check an open chain's side \
                       with inspect_sketch. Refuses a distance that leaves no shape."
    )]
    async fn offset_sketch_entities(
        &self,
        Parameters(params): Parameters<offset::OffsetParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            offset::request(&ids, &params)?
        };
        let reply = self
            .addin
            .offset_sketch_entities(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "entity", &offset::given(&params)))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = offset::reply_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Copy sketch entities in a row or grid inside their sketch, e.g. hole \
                       circles to cut in one go. Reports the new entities' ids, the contours, the \
                       definition state and features that now fail."
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
        description = "Copy sketch entities round a point inside their sketch, e.g. hole circles \
                       on a pitch circle. count includes the originals. Reports the new entities' \
                       ids, the contours, the definition state and features that now fail."
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
                       e.g. after a delete: rebuilds, then lists every failing feature, broken \
                       sketch relation or dimension, and mate whose reference is gone, with its \
                       parents and children. Root causes come first, with a suggested fix; the \
                       rest say what they're `caused_by`."
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
        description = "Change a boss- or cut-extrude of the active part (Edit Feature): end \
                       condition, depth, direction, draft, a second direction, side to cut. Only \
                       what's given changes; nothing given reports its settings. If it's rejected \
                       or fails to rebuild the old settings are put back and the reason given. \
                       Reports before / after and features that now fail."
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
        description = "Change a mate of the active assembly (Edit Mate): kind, entities, \
                       alignment, flip, value, limits, lock_rotation. Only what's given changes. A \
                       new kind or any edit gives the mate a new id (and a new kind a new name): \
                       use the reply's. A change that's rejected or can't be solved is undone and \
                       the reason given. Reports before / after and where the components went."
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
        description = "Change a constant-radius fillet of the active part: radius, tangent \
                       propagation, edges. Feature alone reports it, with its edges and faces as \
                       ids of the model before the fillet. New edges must exist before the fillet \
                       (to pick ones it hides: rollback, inspect_model, rollback to the end). A \
                       rejected change is put back. Reports before / after and features that now \
                       fail."
    )]
    async fn edit_fillet(
        &self,
        Parameters(params): Parameters<feature_edit::EditFilletParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_edit::fillet_request(&ids, &params)?
        };
        let reply = self
            .addin
            .edit_fillet(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = feature_edit::fillet_reply_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Change a linear or circular feature pattern of the active part: counts, \
                       spacing, directions or axis, a second direction, the features copied. Only \
                       what's given changes; nothing given reports it. A rejected change is put \
                       back. Reports before / after and features that now fail."
    )]
    async fn edit_pattern(
        &self,
        Parameters(params): Parameters<feature_edit::EditPatternParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_edit::pattern_request(&ids, &params)?
        };
        let features = params.features.clone().unwrap_or_default();
        let reply = self
            .addin
            .edit_pattern(request)
            .await
            .map_err(|status| delete::with_legend(self.addin_error(status), "feature", &features))?;
        let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = feature_edit::pattern_reply_text(&mut ids, &params, &reply);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Change a sweep or cut-sweep of the active part: path, profile sketch or \
                       circle, twist. Only what's given changes; nothing given reports it. Shapes: \
                       edit the sketches. A rejected change is put back. Reports before / after \
                       and features that now fail."
    )]
    async fn edit_sweep(
        &self,
        Parameters(params): Parameters<feature_edit::EditSweepParams>,
    ) -> Result<CallToolResult, McpError> {
        let request = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            feature_edit::sweep_request(&ids, &params)?
        };
        let reply = self
            .addin
            .edit_sweep(request)
            .await
            .map_err(|status| self.addin_error(status))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(feature_edit::sweep_reply_text(&params, &reply))]))
    }
    #[tool(
        description = "Add a reference plane, axis, point or coordinate system to the active part \
                       from faces, edges, vertices, sketch entities, the origin or the default \
                       planes. Reports where it is and its \"feature:N\" id, a reference for more \
                       reference geometry and add_mate; sketch on a plane with the sketch tool's \
                       on=\"feature:N\". Adds nothing if SolidWorks can't make it."
    )]
    async fn create_reference_geometry(
        &self,
        Parameters(params): Parameters<refgeom::ReferenceGeometryParams>,
    ) -> Result<CallToolResult, McpError> {
        let (request, from, given) = {
            let ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            refgeom::geometry_request(&ids, params)?
        };
        self.reference_geometry(request, &from, &given).await
    }
}

impl SolidWorksTools {
    async fn revolve(&self, params: revolve::RevolveParams) -> Result<CallToolResult, McpError> {
        let cut = params.cut;
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

    async fn sweep(&self, params: sweep::SweepParams) -> Result<CallToolResult, McpError> {
        let cut = params.cut;
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

    async fn loft(&self, params: loft::LoftParams) -> Result<CallToolResult, McpError> {
        let cut = params.cut;
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

    /// `from` describes the references for the reply, `given` numbers them
    /// for error messages.
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

    /// The sketch tool's rectangle: a new sketch of its own. Takes millimeters; `center` is
    /// a sketch [x, y] or a model [x, y, z] point.
    async fn rectangle_sketch(
        &self,
        plane: SketchPlane,
        on: Option<&str>,
        center: Option<&[f64]>,
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
        let (center_x_mm, center_y_mm, center_mm) = match center {
            None => (0.0, 0.0, None),
            Some(&[x, y]) => (x, y, None),
            Some(&[x, y, z]) => (0.0, 0.0, Some([x, y, z])),
            Some(c) => {
                return Err(McpError::invalid_params(
                    format!(
                        "center has {} coordinates; give [x, y] in sketch coordinates or [x, y, z] in \
                         model coordinates",
                        c.len()
                    ),
                    None,
                ));
            }
        };
        if !center.unwrap_or_default().iter().all(|v| v.is_finite()) {
            return Err(McpError::invalid_params("center must be finite", None));
        }

        let on = on.map(str::trim).filter(|on| !on.is_empty());
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
            center_model: center_mm.map(|[x, y, z]| proto::Vec3 {
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
        if sketch.dimensions.len() < 2 {
            text += " SolidWorks didn't take a width and a height dimension on it.";
        }
        if !sketch.dimensions.is_empty() {
            let dims: Vec<String> = sketch.dimensions.iter().map(sketch::dimension_text).collect();
            text += &format!(" Dimensions: {}.", dims.join(", "));
        }
        {
            let mut ids = self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            text += &sketch::items_text(&mut ids, &sketch.items);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// Shared by the sketch tool: draws `entities` (meters) into the target sketch and
    /// describes the result. `what` names what was drawn, e.g. "3 lines".
    async fn draw_sketch(
        &self,
        target: DrawTarget,
        model_space: bool,
        entities: Vec<proto::SketchEntity>,
        what: String,
    ) -> Result<CallToolResult, McpError> {
        let DrawTarget { sketch_3d, sketch: target } = target;
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
        if sketch_3d {
            if sketch_name.is_some() {
                return Err(McpError::invalid_params(
                    "sketch_3d is for a new sketch; sketch_name alone adds to an existing one, 3D \
                     or not",
                    None,
                ));
            }
            if on.is_some() || target.plane.is_some() {
                return Err(McpError::invalid_params(
                    "a 3D sketch isn't on a plane or face -- drop plane/on or sketch_3d",
                    None,
                ));
            }
            if !model_space {
                return Err(McpError::invalid_params(
                    "a 3D sketch has no 2D coordinates -- give points as [x, y, z] in model \
                     coordinates",
                    None,
                ));
            }
        }
        let plane = target.plane.unwrap_or_default();

        let on_persist_id = match on {
            Some(id) => self.resolve_id(id)?,
            None => Vec::new(),
        };
        let request = proto::AddSketchEntitiesRequest {
            sketch_name: sketch_name.unwrap_or_default().to_string(),
            plane: if sketch_name.is_some() || sketch_3d {
                proto::Plane::Unspecified
            } else {
                proto::Plane::from(plane)
            }
            .into(),
            on_persist_id,
            model_space,
            entities,
            sketch_3d,
        };
        let result = self
            .addin
            .add_sketch_entities(request)
            .await
            .map_err(|status| self.addin_error(status))?;

        let place = match (sketch_name, on) {
            (Some(_), _) => String::new(),
            (None, _) if sketch_3d => " (new 3D sketch)".to_string(),
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
            text += if result.is_3d {
                "; a 3D sketch can be a sweep path or a loft guide curve, not extruded."
            } else if contours.closed > 0 && contours.open == 0 {
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
                                "inspect_sketch and the sketch tool",
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

    /// The active document's mesh, from the add-in or, when the add-in says
    /// it hasn't changed, from the last render (unplaced either way).
    async fn mesh(&self) -> Result<proto::GetMeshResponse, McpError> {
        let lock = || self.mesh.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut cached_key = lock().as_ref().map_or(0, |m| m.key);
        loop {
            let reply = self
                .addin
                .get_mesh(proto::GetMeshRequest {
                    include_edges: true,
                    cached_key,
                })
                .await
                .map_err(|status| self.addin_error(status))?;
            let mut cache = lock();
            if !reply.unchanged {
                *cache = Some(reply.clone());
                return Ok(reply);
            }
            match cache.as_ref() {
                Some(mesh) if mesh.key == reply.key => {
                    tracing::debug!("render_views: geometry unchanged, using the cached mesh");
                    return Ok(mesh.clone());
                }
                // Another render replaced the cache meanwhile: ask for all of it.
                _ if cached_key != 0 => cached_key = 0,
                _ => return Err(McpError::internal_error("the add-in sent an empty mesh", None)),
            }
        }
    }

    /// render_views' highlight / hide / transparent / zoom_to lists,
    /// resolved, and their names for the result.
    fn render_picks(&self, params: &RenderViewsParams) -> Result<(render::Picks, PickNames), McpError> {
        if params.highlight.len() > render::HIGHLIGHTS.len() {
            return Err(McpError::invalid_params(
                format!("at most {} highlights", render::HIGHLIGHTS.len()),
                None,
            ));
        }
        let trimmed = |list: &[String]| list.iter().map(|h| h.trim().to_string()).collect::<Vec<_>>();
        let (others, transparent): (Vec<String>, Vec<String>) = trimmed(&params.transparent)
            .into_iter()
            .partition(|t| t.eq_ignore_ascii_case("others"));
        let names = PickNames {
            highlight: trimmed(&params.highlight),
            hide: trimmed(&params.hide),
            transparent,
            zoom_to: trimmed(&params.zoom_to),
            others: !others.is_empty(),
        };
        let targets = |list: &[String]| list.iter().map(|h| self.highlight_target(h)).collect::<Result<Vec<_>, _>>();
        let picks = render::Picks {
            highlight: targets(&names.highlight)?,
            hide: targets(&names.hide)?,
            transparent: targets(&names.transparent)?,
            transparent_rest: names.others,
            zoom: targets(&names.zoom_to)?,
        };
        Ok((picks, names))
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

    /// The active drawing's sheets and views as they are now, for working
    /// out where a derived view goes.
    async fn inspect_views(&self) -> Result<proto::InspectDrawingResponse, McpError> {
        self.addin
            .inspect_drawing(proto::InspectDrawingRequest { skip_sheet_format: true, ..Default::default() })
            .await
            .map_err(|status| self.addin_error(status))
    }

    /// create_drawing_view's derived views: the RPC, then, unless placed by the
    /// caller, the new view is moved clear of the other views.
    async fn derive_drawing_view(&self, plan: derived_view::Plan) -> Result<CallToolResult, McpError> {
        let mut reply = self
            .addin
            .derive_drawing_view(plan.request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        let mut moved_to = None;
        if let Some(spot) = reply.sheet.as_ref().and_then(|s| plan.clear_spot(s, &reply.view_name)) {
            let moved = self
                .addin
                .setup_drawing_view(proto::SetupDrawingViewRequest {
                    view_name: reply.view_name.clone(),
                    position: Some(proto::Vec3 { x: spot.x, y: spot.y, z: 0.0 }),
                    ..Default::default()
                })
                .await;
            // The view is made either way; a failed move leaves it where it
            // is, and the reply says what it overlaps.
            match moved {
                Ok(moved) => {
                    reply = moved;
                    moved_to = Some(spot);
                }
                Err(status) => tracing::warn!("moving the new view {} clear failed: {status}", reply.view_name),
            }
        }
        if let Some(sheet) = &reply.sheet {
            let mut cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.sheet_changed(&sheet.views, false);
        }
        reply.created = true;
        let text = view::reply_text(&proto::SetupDrawingViewRequest::default(), &reply, None, moved_to);
        Ok(CallToolResult::success(vec![ContentBlock::text(plan.finish_text(text, &reply))]))
    }

    /// crop_view / break_view: the RPC; the view's entity positions are
    /// asked for again before use.
    async fn shape_drawing_view(&self, request: proto::ShapeDrawingViewRequest) -> Result<CallToolResult, McpError> {
        let reply = self
            .addin
            .shape_drawing_view(request.clone())
            .await
            .map_err(|status| self.addin_error(status))?;
        if let Some(sheet) = &reply.sheet {
            let mut cache = self.drawings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.sheet_changed(&sheet.views, false);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(derived_view::shape_reply_text(&request, &reply))]))
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
    // a document (see IdRegistry), and so a tool's failure is an isError
    // result the model sees and can correct, not a protocol error that some
    // clients hide. Unknown or unloaded tools stay protocol errors.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if !self.filter.allows(&request.name) {
            return Err(McpError::invalid_params(
                format!("tool {} isn't enabled on this server (--modes / --inspect-only)", request.name),
                None,
            ));
        }
        if !self.tool_router.has_route(&request.name) {
            return Err(McpError::invalid_params(format!("unknown tool {}", request.name), None));
        }
        self.track_document(false).await;
        let result = self.tool_router.call(ToolCallContext::new(self, request, context)).await;
        Ok(result.unwrap_or_else(|error| CallToolResult::error(vec![ContentBlock::text(error.message)]).into()))
    }

    // Written by hand because the one #[tool_handler] generates leaves out
    // ttlMs and cacheScope. Protocol 2026-07-28 requires both on tools/list,
    // and Claude Code rejects the whole tool list without them.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self.tools.iter().filter(|tool| self.filter.allows(&tool.name)).cloned().collect();
        Ok(ListToolsResult::with_all_items(tools)
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
            .with_instructions(INSTRUCTIONS)
    }
}

/// Cross-tool guidance, sent once in the initialize result.
const INSTRUCTIONS: &str = "Drives a running SolidWorks through its sld_mcp add-in, on localhost. Tools work \
                 on the active document; those that change a part or assembly need no sketch open \
                 for editing. Lengths are mm and angles degrees whatever the document's units \
                 (document_units only changes what SolidWorks shows; create_part / create_assembly \
                 / create_drawing take units=\"ips\" for an inch document). Ids: inspect_model \
                 gives the model's (\"face:7\", \"edge:3\", \"feature:2\", \"component:1\"), \
                 inspect_sketch and the sketch tool a sketch's (\"line:3\", \"point:7\"), \
                 inspect_drawing with a view that view's (\"view_edge:3\"); they belong to the \
                 active document and stay valid while the entity exists. Work in a loop: inspect, \
                 change by id, check with render_views / render_drawing (shapes) and inspect_model \
                 (sizes). A part: sketch closed profiles, then extrude_sketch, revolve_sketch, \
                 sweep_sketch or loft_sketch (cut=true removes material), then fillets, patterns, \
                 holes; dimension_sketch, add_sketch_relations and fully_define_sketch define a \
                 sketch, set_dimension changes sizes later. Assemblies: insert_components, \
                 add_mate, then inspect_mobility, move_component, check_interference. Drawings: \
                 create_drawing, create_drawing_view, add_drawing_dimensions or \
                 insert_model_items, then render_drawing. To rework: undo, delete_features, \
                 diagnose_model. When no tool fits, run_macro runs VBA.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio transport uses stdout for protocol messages, so all logging
    // must go to stderr -- anything on stdout would corrupt the stream.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let filter = toolsets::Filter::from_args(std::env::args().skip(1))?;
    let addin = AddinClient::new();
    let service = SolidWorksTools::new(addin, filter).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_in_exactly_one_toolset_entry() {
        let mut tools: Vec<String> =
            SolidWorksTools::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        let mut grouped: Vec<String> = toolsets::TOOLS.iter().map(|(name, ..)| name.to_string()).collect();
        tools.sort();
        grouped.sort();
        assert_eq!(tools, grouped);
    }

    fn sketch_params(args: serde_json::Value) -> SketchParams {
        serde_json::from_value(args).unwrap()
    }

    #[test]
    fn sketch_shapes_take_only_their_parameters() {
        let ok = |args| sketch_params(args).check_applies().is_ok();
        assert!(ok(serde_json::json!({"shape": "line", "points": [[0, 0], [1, 0]], "closed": true})));
        assert!(ok(serde_json::json!({"shape": "rectangle", "width_mm": 10, "on": "face:1", "center": [0, 0, 5]})));
        assert!(ok(serde_json::json!({"shape": "arc", "start": [0, 0, 0], "end": [1, 0, 0], "through": [0, 1, 0], "sketch_3d": true})));
        assert!(!ok(serde_json::json!({"shape": "line", "points": [[0, 0], [1, 0]], "radius_mm": 3})));
        assert!(!ok(serde_json::json!({"shape": "point", "points": [[0, 0]], "construction": true})));
        assert!(!ok(serde_json::json!({"shape": "rectangle", "width_mm": 10, "sketch_name": "Sketch1"})));
        assert!(!ok(serde_json::json!({"shape": "slot", "start": [0, 0], "end": [1, 0], "width_mm": 1, "sketch_3d": true})));

        // What a shape needs, it asks for by name.
        let p = sketch_params(serde_json::json!({"shape": "polygon", "center": [0, 0]}));
        let error = p.entities(&mut PointConverter::default()).unwrap_err();
        assert!(error.message.contains("needs sides"), "{}", error.message);
        let p = sketch_params(serde_json::json!({"shape": "line", "points": [[0, 0], [10, 0], [10, 5]], "closed": true}));
        let (entities, what) = p.entities(&mut PointConverter::default()).unwrap();
        assert_eq!((entities.len(), what.as_str()), (3, "3 lines"));
    }

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
    fn polygon_shapes() {
        // By a corner: as given, with the circle through the corners.
        let (r, a, inscribed) = polygon_shape(6, Some(10.0), None, 30.0).unwrap();
        assert!((r - 0.01).abs() < 1e-12 && (a - 30f64.to_radians()).abs() < 1e-12 && !inscribed);
        // By a side: the corner is half a side on from its middle, and further out.
        let (r, a, inscribed) = polygon_shape(6, None, Some(10.0), 0.0).unwrap();
        assert!((r - 0.01 / 30f64.to_radians().cos()).abs() < 1e-12);
        assert!((a - 30f64.to_radians()).abs() < 1e-12 && inscribed);

        assert!(polygon_shape(2, Some(10.0), None, 0.0).is_err());
        assert!(polygon_shape(41, Some(10.0), None, 0.0).is_err());
        assert!(polygon_shape(6, None, None, 0.0).is_err());
        assert!(polygon_shape(6, Some(10.0), Some(10.0), 0.0).is_err());
        assert!(polygon_shape(6, Some(0.0), None, 0.0).is_err());
        assert!(polygon_shape(6, Some(10.0), None, f64::NAN).is_err());
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
