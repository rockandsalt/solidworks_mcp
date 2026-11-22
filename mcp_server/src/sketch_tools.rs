//! The sketch tool and the sketch dimension / relation tools: their
//! params (millimeters and degrees, turned into the wire's meters and
//! radians here), and the SolidWorksTools steps they share (drawing into a
//! sketch, a rectangle sketch, the refs dimensions and relations take).

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{self, Kind, MM_PER_M, SKETCH_KINDS};
use crate::sketch::{self, RefTarget};
use crate::{SolidWorksTools, angle_rad, count_of, positive_mm};

/// One of the three default reference planes every part starts with.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SketchPlane {
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
pub type Point = Vec<f64>;

/// Which sketch the sketch tools draw in.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SketchTarget {
    /// Add to this existing sketch, e.g. "Sketch1" or "3DSketch1". Omit for a new sketch.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// New sketch: the default plane to sketch on. Defaults to front.
    #[serde(default)]
    pub plane: Option<SketchPlane>,
    /// New sketch: on this planar face or reference plane instead ("face:4", "feature:2");
    /// [x, y, z] points then place geometry without knowing the sketch axes.
    #[serde(default)]
    pub on: Option<String>,
}

/// Which sketch the sketch tool draws in, which unlike the other sketch tools can be a new 3D one.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DrawTarget {
    /// New sketch: a 3D sketch, with no plane: [x, y, z] points go where they say. For a
    /// sweep path or loft guide that doesn't lie in a plane; can't be extruded.
    #[serde(default)]
    pub sketch_3d: bool,
    #[serde(flatten)]
    pub sketch: SketchTarget,
}

/// What the sketch tool draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SketchShape {
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
    pub fn name(self) -> &'static str {
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
pub struct SketchParams {
    pub shape: SketchShape,
    /// line, spline, point.
    #[serde(default)]
    pub points: Option<Vec<Point>>,
    /// line, spline: join the last point back to the first.
    #[serde(default)]
    pub closed: bool,
    /// circle, polygon, ellipse, rectangle (default: the origin, or the middle of the `on`
    /// face); arc, slot: the center of the arc.
    #[serde(default)]
    pub center: Option<Point>,
    /// arc: where it starts; slot: one end's center.
    #[serde(default)]
    pub start: Option<Point>,
    /// arc, slot: the other end. With `center` only its direction from the center counts.
    #[serde(default)]
    pub end: Option<Point>,
    /// arc, slot: a point on the arc between start and end, instead of `center`.
    #[serde(default)]
    pub through: Option<Point>,
    /// arc, slot with `center`: clockwise, seen from the side the sketch normal points to.
    #[serde(default)]
    pub clockwise: bool,
    /// circle.
    #[serde(default)]
    pub radius_mm: Option<f64>,
    /// circle.
    #[serde(default)]
    pub diameter_mm: Option<f64>,
    /// slot (twice the end radius), rectangle.
    #[serde(default)]
    pub width_mm: Option<f64>,
    /// rectangle. Defaults to width_mm: a square.
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// polygon: 3 to 40.
    #[serde(default)]
    pub sides: Option<i32>,
    /// polygon: from the center to a corner.
    #[serde(default)]
    pub corner_radius_mm: Option<f64>,
    /// polygon: from the center to the middle of a side (half across flats).
    #[serde(default)]
    pub flat_radius_mm: Option<f64>,
    /// polygon: toward a corner (a side's middle with flat_radius_mm); ellipse: the long axis.
    /// Counterclockwise from sketch X. Defaults to 0.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// ellipse: from the center to the end of the long axis.
    #[serde(default)]
    pub major_radius_mm: Option<f64>,
    /// ellipse: from the center to the end of the short axis.
    #[serde(default)]
    pub minor_radius_mm: Option<f64>,
    /// slot: add driving dimensions of its width and length.
    #[serde(default)]
    pub add_dimensions: bool,
    /// Construction geometry: reference only, ignored by features.
    #[serde(default)]
    pub construction: bool,
    #[serde(flatten)]
    pub target: DrawTarget,
}

impl SketchParams {
    /// Refuses parameters the shape doesn't take, rather than ignoring them.
    pub fn check_applies(&self) -> Result<(), McpError> {
        use SketchShape::*;
        let all_but_rectangle = &[Line, Circle, Arc, Spline, Point, Slot, Polygon, Ellipse][..];
        let given: [(&str, bool, &[SketchShape]); 21] = [
            ("points", self.points.is_some(), &[Line, Spline, Point]),
            ("closed", self.closed, &[Line, Spline]),
            (
                "center",
                self.center.is_some(),
                &[Circle, Arc, Slot, Polygon, Ellipse, Rectangle],
            ),
            ("start", self.start.is_some(), &[Arc, Slot]),
            ("end", self.end.is_some(), &[Arc, Slot]),
            ("through", self.through.is_some(), &[Arc, Slot]),
            ("clockwise", self.clockwise, &[Arc, Slot]),
            ("radius_mm", self.radius_mm.is_some(), &[Circle]),
            ("diameter_mm", self.diameter_mm.is_some(), &[Circle]),
            ("width_mm", self.width_mm.is_some(), &[Slot, Rectangle]),
            ("height_mm", self.height_mm.is_some(), &[Rectangle]),
            ("sides", self.sides.is_some(), &[Polygon]),
            (
                "corner_radius_mm",
                self.corner_radius_mm.is_some(),
                &[Polygon],
            ),
            ("flat_radius_mm", self.flat_radius_mm.is_some(), &[Polygon]),
            ("angle_deg", self.angle_deg.is_some(), &[Polygon, Ellipse]),
            (
                "major_radius_mm",
                self.major_radius_mm.is_some(),
                &[Ellipse],
            ),
            (
                "minor_radius_mm",
                self.minor_radius_mm.is_some(),
                &[Ellipse],
            ),
            ("add_dimensions", self.add_dimensions, &[Slot]),
            (
                "construction",
                self.construction,
                &[Line, Circle, Arc, Spline, Slot, Polygon, Ellipse],
            ),
            // A circle goes to the add-in, which says why a 3D sketch can't take one.
            (
                "sketch_3d",
                self.target.sketch_3d,
                &[Line, Circle, Arc, Spline, Point],
            ),
            (
                "sketch_name",
                self.target.sketch.sketch_name.is_some(),
                all_but_rectangle,
            ),
        ];
        match given
            .iter()
            .find(|(_, given, shapes)| *given && !shapes.contains(&self.shape))
        {
            Some((name, _, _)) => Err(McpError::invalid_params(
                format!("shape \"{}\" doesn't take {name}", self.shape.name()),
                None,
            )),
            None => Ok(()),
        }
    }

    /// A parameter the shape needs.
    pub fn needs<'a, T>(&self, value: &'a Option<T>, name: &str) -> Result<&'a T, McpError> {
        value.as_ref().ok_or_else(|| {
            McpError::invalid_params(
                format!("shape \"{}\" needs {name}", self.shape.name()),
                None,
            )
        })
    }

    /// The entities a 2D or 3D shape draws (meters), and what to call them in the reply.
    pub fn entities(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        match self.shape {
            SketchShape::Line => self.line_entities(points),
            SketchShape::Circle => self.circle_entity(points),
            SketchShape::Arc => self.arc_entity(points),
            SketchShape::Spline => self.spline_entity(points),
            SketchShape::Point => self.point_entities(points),
            SketchShape::Slot => self.slot_entity(points),
            SketchShape::Polygon => self.polygon_entity(points),
            SketchShape::Ellipse => self.ellipse_entity(points),
            SketchShape::Rectangle => unreachable!("a rectangle is its own request"),
        }
    }

    /// One entity of the shape, construction or not.
    fn one(&self, geometry: proto::sketch_entity::Geometry) -> Vec<proto::SketchEntity> {
        vec![sketch_entity(self.construction, geometry)]
    }

    /// A polyline: one line per pair of points (closed: back to the first).
    fn line_entities(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let construction = self.construction;
        let given = self.needs(&self.points, "points")?;
        let n = given.len();
        if n < 2 {
            return Err(McpError::invalid_params("give at least 2 points", None));
        }
        if self.closed && n < 3 {
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
        Ok((entities, what))
    }

    /// A circle by radius or diameter.
    fn circle_entity(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let radius_mm = match (self.radius_mm, self.diameter_mm) {
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
        let center = points.convert(self.needs(&self.center, "center")?, "center")?;
        let geometry = proto::sketch_entity::Geometry::Circle(proto::SketchCircle {
            center: Some(center),
            radius: radius_mm / MM_PER_M,
        });
        Ok((
            self.one(geometry),
            format!("a circle of radius {radius_mm} mm"),
        ))
    }

    /// An arc by its center or a point it passes through.
    fn arc_entity(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
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
            _ => {
                return Err(McpError::invalid_params(
                    "give exactly one of center or through",
                    None,
                ));
            }
        };
        let geometry = proto::sketch_entity::Geometry::Arc(proto::SketchArc {
            start: Some(start),
            end: Some(end),
            center,
            through,
            clockwise: self.clockwise,
        });
        Ok((self.one(geometry), "an arc".to_string()))
    }

    /// A spline through the points.
    fn spline_entity(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let given = self.needs(&self.points, "points")?;
        let n = given.len();
        let min = if self.closed { 3 } else { 2 };
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
        let geometry = proto::sketch_entity::Geometry::Spline(proto::SketchSpline {
            points: points.convert_all(given, "point")?,
            closed: self.closed,
        });
        let what = format!(
            "a {}spline through {n} points",
            if self.closed { "closed " } else { "" }
        );
        Ok((self.one(geometry), what))
    }

    /// Sketch points.
    fn point_entities(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let given = self.needs(&self.points, "points")?;
        let n = given.len();
        if n == 0 {
            return Err(McpError::invalid_params("give at least 1 point", None));
        }
        if n > MAX_SKETCH_ITEMS {
            return Err(McpError::invalid_params(
                format!("at most {MAX_SKETCH_ITEMS} points per call"),
                None,
            ));
        }
        let entities = points
            .convert_all(given, "point")?
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
        Ok((entities, count_of(n, "point")))
    }

    /// A straight slot, or a curved one (center or through).
    fn slot_entity(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let width_mm = *self.needs(&self.width_mm, "width_mm")?;
        if !(width_mm.is_finite() && width_mm > 0.0) {
            return Err(McpError::invalid_params(
                "width_mm must be a positive number of millimeters",
                None,
            ));
        }
        if self.center.is_some() && self.through.is_some() {
            return Err(McpError::invalid_params(
                "give center or through for a curved slot, not both",
                None,
            ));
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
        let center = self
            .center
            .as_ref()
            .map(|c| points.convert(c, "center"))
            .transpose()?;
        let through = self
            .through
            .as_ref()
            .map(|t| points.convert(t, "through"))
            .transpose()?;
        let what = format!(
            "a {}slot {width_mm} mm wide",
            if center.is_some() || through.is_some() {
                "curved "
            } else {
                ""
            }
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
        Ok((self.one(geometry), what))
    }

    /// A regular polygon.
    fn polygon_entity(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
        let sides = *self.needs(&self.sides, "sides")?;
        let (corner_radius, angle, inscribed) = polygon_shape(
            sides,
            self.corner_radius_mm,
            self.flat_radius_mm,
            self.angle_deg.unwrap_or(0.0),
        )?;
        let center = points.convert(self.needs(&self.center, "center")?, "center")?;
        let geometry = proto::sketch_entity::Geometry::Polygon(proto::SketchPolygon {
            center: Some(center),
            sides,
            corner_radius,
            angle,
            inscribed,
        });
        Ok((self.one(geometry), format!("a polygon with {sides} sides")))
    }

    /// An ellipse, its long axis at angle_deg.
    fn ellipse_entity(
        &self,
        points: &mut PointConverter,
    ) -> Result<(Vec<proto::SketchEntity>, String), McpError> {
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
        Ok((
            self.one(geometry),
            format!("an ellipse of {major} x {minor} mm radii"),
        ))
    }
}

/// Most entities or spline points one sketch call may draw (the add-in's limit).
pub const MAX_SKETCH_ITEMS: usize = 1000;

/// A polygon as the add-in draws it: from the center to a corner, in the direction of one
/// (radians), and whether its construction circle touches the sides.
pub fn polygon_shape(
    sides: i32,
    corner_radius_mm: Option<f64>,
    flat_radius_mm: Option<f64>,
    angle_deg: f64,
) -> Result<(f64, f64, bool), McpError> {
    if !(3..=40).contains(&sides) {
        return Err(McpError::invalid_params(
            format!("a polygon needs 3 to 40 sides, got {sides}"),
            None,
        ));
    }
    if !angle_deg.is_finite() {
        return Err(McpError::invalid_params("angle_deg must be finite", None));
    }
    let half_side = std::f64::consts::PI / f64::from(sides);
    let (radius, angle, inscribed) = match (corner_radius_mm, flat_radius_mm) {
        (Some(r), None) => (r, angle_deg.to_radians(), false),
        (None, Some(r)) => (
            r / half_side.cos(),
            angle_deg.to_radians() + half_side,
            true,
        ),
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
pub struct PointConverter {
    /// Settled by the first point: Some(true) for model coordinates.
    pub model_space: Option<bool>,
}

impl PointConverter {
    pub fn convert(&mut self, point: &[f64], what: &str) -> Result<proto::Vec3, McpError> {
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
            return Err(McpError::invalid_params(
                format!("{what} must be finite"),
                None,
            ));
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

    pub fn convert_all(
        &mut self,
        points: &[Point],
        what: &str,
    ) -> Result<Vec<proto::Vec3>, McpError> {
        points
            .iter()
            .enumerate()
            .map(|(i, p)| self.convert(p, &format!("{what} {}", i + 1)))
            .collect()
    }

    pub fn model_space(&self) -> bool {
        self.model_space.unwrap_or(false)
    }
}
/// Flags an extruded, revolved, swept or lofted sketch that isn't fully defined.
pub fn sketch_status_note(sketch_name: &str, status: proto::DefinitionStatus) -> String {
    match status {
        proto::DefinitionStatus::Fully | proto::DefinitionStatus::Unspecified => String::new(),
        status => format!(
            " Note: {sketch_name} is {} (dimension_sketch / fully_define_sketch can fix that).",
            sketch::status_name(status)
        ),
    }
}
/// A dimension's value on the wire (meters, or radians for angles), checking the right unit
/// was used.
pub fn dimension_value(spec: &DimensionSpec) -> Result<Option<f64>, McpError> {
    match (
        spec.kind == DimensionKind::Angle,
        spec.value_mm,
        spec.value_deg,
    ) {
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

pub fn sketch_entity(
    construction: bool,
    geometry: proto::sketch_entity::Geometry,
) -> proto::SketchEntity {
    proto::SketchEntity {
        construction,
        geometry: Some(geometry),
    }
}
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectSketchParams {
    /// The sketch, e.g. "Sketch1". Defaults to the last sketch in the feature tree.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

/// What a dimension measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DimensionKind {
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
    pub fn name(self) -> &'static str {
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
pub struct DimensionSpec {
    /// length (1 line), distance (2 entities), horizontal / vertical (1 line, or 2 entities,
    /// measured along the sketch's X / Y axis), diameter / radius (1 circle or arc), angle
    /// (2 lines).
    pub kind: DimensionKind,
    /// What it measures: sketch entity ids from inspect_sketch or the sketch tool ("line:3",
    /// "point:7", "circle:1"), "origin" for the sketch origin, or an edge/face id from
    /// inspect_model ("edge:12") to measure from the model.
    pub entities: Vec<String>,
    /// Drive the dimension to this many millimeters, moving the geometry. Omit to keep the
    /// current size and just lock it. Not for angles.
    #[serde(default)]
    pub value_mm: Option<f64>,
    /// Angles only: drive the angle to this many degrees. Omit to keep the current angle.
    #[serde(default)]
    pub value_deg: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DimensionSketchParams {
    /// Dimensions to add to one sketch, applied together.
    pub dimensions: Vec<DimensionSpec>,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the entity ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

/// A geometric relation between sketch entities.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RelationKind {
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
    pub fn name(self) -> &'static str {
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
pub struct RelationSpec {
    pub kind: RelationKind,
    /// The entities, in order: sketch entity ids ("line:3", "point:7"), "origin", or an
    /// edge/face id from inspect_model.
    pub entities: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddSketchRelationsParams {
    /// Relations to add to one sketch, applied together.
    pub relations: Vec<RelationSpec>,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the entity ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

/// How fully_define_sketch lays out the dimensions it adds.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DimensionScheme {
    /// Every dimension from the datum (default).
    #[default]
    Baseline,
    /// Each dimension from the previous feature.
    Chain,
    /// Ordinate dimensions from the datum.
    Ordinate,
}

pub fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FullyDefineSketchParams {
    /// The sketch, e.g. "Sketch1". Defaults to the last sketch in the feature tree.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// Add relations (horizontal, vertical, coincident, tangent, ...) wherever the geometry
    /// already satisfies them. Defaults to true.
    #[serde(default = "default_true")]
    pub relations: bool,
    /// Add dimensions for whatever relations don't fix. Defaults to true.
    #[serde(default = "default_true")]
    pub dimensions: bool,
    /// How to lay out the dimensions: "baseline" (default), "chain" or "ordinate".
    #[serde(default)]
    pub scheme: DimensionScheme,
    /// What horizontal dimensions are measured from: "origin" (default), a sketch point or
    /// vertical line id, or a model edge id.
    #[serde(default)]
    pub horizontal_datum: Option<String>,
    /// What vertical dimensions are measured from: "origin" (default), a sketch point or
    /// horizontal line id, or a model edge id.
    #[serde(default)]
    pub vertical_datum: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetDimensionParams {
    /// The dimension's full name, e.g. "D1@Sketch1" (inspect_sketch lists them).
    pub name: String,
    /// New value in millimeters, for lengths, distances, diameters and radii.
    #[serde(default)]
    pub value_mm: Option<f64>,
    /// New value in degrees, for angles.
    #[serde(default)]
    pub value_deg: Option<f64>,
    /// Only in these configurations (names from the configurations tool). Default: SolidWorks'
    /// choice -- every configuration while the dimension has one value for all, else only the
    /// active one.
    #[serde(default)]
    pub configurations: Option<Vec<String>>,
    /// true: in every configuration.
    #[serde(default)]
    pub all_configurations: Option<bool>,
}

/// Most entities one dimension takes.
pub const MAX_DIMENSION_ENTITIES: usize = 3;
/// Most entities one relation takes (equal, parallel, ... across many entities at once;
/// the add-in's limit).
pub const MAX_RELATION_ENTITIES: usize = 32;
/// Most dimensions or relations per call (the add-in's limit).
pub const MAX_CONSTRAINTS: usize = 200;

impl SolidWorksTools {
    /// The sketch tool's rectangle: a new sketch of its own. Takes millimeters; `center` is
    /// a sketch [x, y] or a model [x, y, z] point.
    pub(crate) async fn rectangle_sketch(
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
            let dims: Vec<String> = sketch
                .dimensions
                .iter()
                .map(sketch::dimension_text)
                .collect();
            text += &format!(" Dimensions: {}.", dims.join(", "));
        }
        {
            let mut ids = self
                .ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            text += &sketch::items_text(&mut ids, &sketch.items);
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// Shared by the sketch tool: draws `entities` (meters) into the target sketch and
    /// describes the result. `what` names what was drawn, e.g. "3 lines".
    pub(crate) async fn draw_sketch(
        &self,
        target: DrawTarget,
        model_space: bool,
        entities: Vec<proto::SketchEntity>,
        what: String,
    ) -> Result<CallToolResult, McpError> {
        let DrawTarget {
            sketch_3d,
            sketch: target,
        } = target;
        let sketch_name = target
            .sketch_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let on = target
            .on
            .as_deref()
            .map(str::trim)
            .filter(|on| !on.is_empty());
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
            let mut ids = self
                .ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    pub(crate) fn sketch_refs(
        &self,
        entities: &[String],
        max: usize,
    ) -> Result<Vec<proto::SketchRef>, McpError> {
        if entities.is_empty() || entities.len() > max {
            return Err(McpError::invalid_params(
                format!("give 1 to {max} entities"),
                None,
            ));
        }
        entities.iter().map(|e| self.sketch_ref(e)).collect()
    }

    /// One dimension/relation entity: a sketch entity id, "origin", or a model edge/face id.
    pub(crate) fn sketch_ref(&self, entity: &str) -> Result<proto::SketchRef, McpError> {
        let entity = entity.trim();
        if entity.eq_ignore_ascii_case("origin") {
            return Ok(RefTarget::Origin.into_proto());
        }
        match Kind::of_id(entity) {
            Some(Kind::Edge | Kind::Face) => {
                Ok(RefTarget::Model(self.resolve_id(entity)?).into_proto())
            }
            Some(kind) if SKETCH_KINDS.contains(&kind) => {
                let ids = self
                    .ids
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch_params(args: serde_json::Value) -> SketchParams {
        serde_json::from_value(args).unwrap()
    }

    #[test]
    fn sketch_shapes_take_only_their_parameters() {
        let ok = |args| sketch_params(args).check_applies().is_ok();
        assert!(ok(
            serde_json::json!({"shape": "line", "points": [[0, 0], [1, 0]], "closed": true})
        ));
        assert!(ok(
            serde_json::json!({"shape": "rectangle", "width_mm": 10, "on": "face:1", "center": [0, 0, 5]})
        ));
        assert!(ok(
            serde_json::json!({"shape": "arc", "start": [0, 0, 0], "end": [1, 0, 0], "through": [0, 1, 0], "sketch_3d": true})
        ));
        assert!(!ok(
            serde_json::json!({"shape": "line", "points": [[0, 0], [1, 0]], "radius_mm": 3})
        ));
        assert!(!ok(
            serde_json::json!({"shape": "point", "points": [[0, 0]], "construction": true})
        ));
        assert!(!ok(
            serde_json::json!({"shape": "rectangle", "width_mm": 10, "sketch_name": "Sketch1"})
        ));
        assert!(!ok(
            serde_json::json!({"shape": "slot", "start": [0, 0], "end": [1, 0], "width_mm": 1, "sketch_3d": true})
        ));

        // What a shape needs, it asks for by name.
        let p = sketch_params(serde_json::json!({"shape": "polygon", "center": [0, 0]}));
        let error = p.entities(&mut PointConverter::default()).unwrap_err();
        assert!(error.message.contains("needs sides"), "{}", error.message);
        let p = sketch_params(
            serde_json::json!({"shape": "line", "points": [[0, 0], [10, 0], [10, 5]], "closed": true}),
        );
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
        assert_eq!(
            dimension_value(&spec(DimensionKind::Length, Some(50.0), None)).unwrap(),
            Some(0.05)
        );
        assert_eq!(
            dimension_value(&spec(DimensionKind::Length, None, None)).unwrap(),
            None
        );
        let angle = dimension_value(&spec(DimensionKind::Angle, None, Some(90.0)))
            .unwrap()
            .unwrap();
        assert!((angle - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        // Wrong unit for the kind, both units, or out of range.
        assert!(dimension_value(&spec(DimensionKind::Angle, Some(90.0), None)).is_err());
        assert!(dimension_value(&spec(DimensionKind::Diameter, None, Some(10.0))).is_err());
        assert!(dimension_value(&spec(DimensionKind::Length, Some(1.0), Some(1.0))).is_err());
        assert!(dimension_value(&spec(DimensionKind::Length, Some(-1.0), None)).is_err());
        assert!(dimension_value(&spec(DimensionKind::Angle, None, Some(360.0))).is_err());
    }
}
