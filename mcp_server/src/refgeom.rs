//! The reference geometry tools (create_reference_plane / _axis / _point,
//! create_coordinate_system): their parameters, turning reference strings
//! into wire refs, filling in what can be inferred, and the reply text.
//!
//! A reference is anything the Reference Geometry commands accept: model
//! faces / edges / vertices and reference features by inspect_model id,
//! sketch entities by their sketch ids ("line:3", "point:7"), "origin", the
//! default planes by name ("front", "top", "right"), or a reference
//! feature's name ("Plane1").

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::addin::proto;
use crate::inspect::{self, IdRegistry, Kind, MM_PER_M, SKETCH_KINDS, mm, round};
use crate::sketch;

/// Most references a plane takes (SolidWorks' first / second / third).
const MAX_PLANE_REFERENCES: usize = 3;
/// Most points one along-a-curve call makes (the add-in's limit).
const MAX_POINTS: u32 = 100;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// How a reference plane relates to one of its references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlaneRelation {
    /// Through it: a point, a line / edge, or a plane / planar face (the plane lies on it).
    Coincident,
    /// Parallel to a plane / planar face (through another reference).
    Parallel,
    /// Perpendicular to a plane / face, or normal to a line / edge / curve (with a point on
    /// it as a second, coincident reference: the plane normal to a path at that point).
    Perpendicular,
    /// Offset from a plane / planar face by offset_mm.
    Distance,
    /// At angle_deg to a plane / planar face, rotated about a line / edge given as another,
    /// coincident reference.
    Angle,
    /// Tangent to a cylindrical, conical or other curved face.
    Tangent,
    /// Halfway between two planes / planar faces: give both with mid_plane.
    MidPlane,
}

impl PlaneRelation {
    /// As the tool takes it.
    fn key(self) -> &'static str {
        match self {
            PlaneRelation::Coincident => "coincident",
            PlaneRelation::Parallel => "parallel",
            PlaneRelation::Perpendicular => "perpendicular",
            PlaneRelation::Distance => "distance",
            PlaneRelation::Angle => "angle",
            PlaneRelation::Tangent => "tangent",
            PlaneRelation::MidPlane => "mid_plane",
        }
    }

    fn name(self) -> &'static str {
        match self {
            PlaneRelation::Coincident => "through",
            PlaneRelation::Parallel => "parallel to",
            PlaneRelation::Perpendicular => "perpendicular to",
            PlaneRelation::Distance => "offset from",
            PlaneRelation::Angle => "at an angle to",
            PlaneRelation::Tangent => "tangent to",
            PlaneRelation::MidPlane => "midway between",
        }
    }
}

impl From<PlaneRelation> for proto::PlaneConstraint {
    fn from(r: PlaneRelation) -> Self {
        use proto::PlaneConstraint as C;
        match r {
            PlaneRelation::Coincident => C::Coincident,
            PlaneRelation::Parallel => C::Parallel,
            PlaneRelation::Perpendicular => C::Perpendicular,
            PlaneRelation::Distance => C::Distance,
            PlaneRelation::Angle => C::Angle,
            PlaneRelation::Tangent => C::Tangent,
            PlaneRelation::MidPlane => C::MidPlane,
        }
    }
}

/// What reference strings can be, as the parameter docs put it.
const REFERENCE_DOC: &str = "inspect_model ids of faces, edges, vertices (\"vertex:N\", from an \
    edge's start_vertex/end_vertex) or reference planes/axes/points (\"feature:N\"); sketch \
    entity ids (\"line:3\", \"arc:1\", \"point:7\" from the sketch_* tools or inspect_sketch); \
    \"origin\"; \"front\", \"top\" or \"right\" for the default planes; or a reference \
    feature's name such as \"Plane1\"";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlaneReference {
    /// What the plane is defined from: an inspect_model id of a face, edge, vertex or
    /// reference plane/axis/point ("face:3", "edge:7", "vertex:2", "feature:1"); a sketch
    /// entity id ("line:3", "point:7"); "origin"; "front" / "top" / "right" (the default
    /// planes); or a reference feature's name ("Plane1").
    pub entity: String,
    /// How the plane relates to it: "coincident" (through it), "parallel", "perpendicular",
    /// "distance" (offset by offset_mm), "angle" (by angle_deg, about another reference that is a
    /// line/edge), "tangent" or "mid_plane". Defaults to "distance" with offset_mm, "angle"
    /// with angle_deg, else "coincident".
    #[serde(default)]
    pub relation: Option<PlaneRelation>,
    /// relation "distance": how far from the plane/face, in millimeters (more than 0).
    #[serde(default)]
    pub offset_mm: Option<f64>,
    /// relation "angle": the angle in degrees (more than 0).
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Put the plane on the other side of this reference (the offset / angle the other way, or
    /// the normal flipped). Defaults to false.
    #[serde(default)]
    pub flip: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReferencePlaneParams {
    /// One to three references, combined as in SolidWorks' Plane command. E.g. offset:
    /// [{"entity": "top", "offset_mm": 20}]; at an angle about an edge: [{"entity": "face:3",
    /// "angle_deg": 30}, {"entity": "edge:5"}]; through three points: [{"entity": "vertex:1"},
    /// {"entity": "vertex:2"}, {"entity": "point:4"}]; normal to a sketch path at its end:
    /// [{"entity": "line:2", "relation": "perpendicular"}, {"entity": "point:3"}]; between two
    /// faces: both with "relation": "mid_plane".
    pub references: Vec<PlaneReference>,
    /// Name for the new plane, e.g. "Mounting Plane". Defaults to SolidWorks' "PlaneN".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReferenceAxisParams {
    /// One or two references, as SolidWorks' Axis command takes them: a straight edge or
    /// sketch line; a cylindrical or conical face ("face:N", its axis); two planes / planar
    /// faces (where they meet); two points / vertices / "origin"; or a point and a face or
    /// plane (through the point, normal to it). Each is an inspect_model id, a sketch entity
    /// id ("line:3", "point:7"), "origin", "front"/"top"/"right", or a feature name.
    pub references: Vec<String>,
    /// Name for the new axis. Defaults to SolidWorks' "AxisN".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    /// The center of a circular edge, or of a sketch arc or circle.
    Center,
    /// The center of area of a face.
    FaceCenter,
    /// A vertex or sketch point projected onto a face or plane (point first, then the face).
    Projection,
    /// Where two edges / sketch segments cross, or where an edge meets a face or plane.
    Intersection,
    /// On an edge or sketch segment: distance_mm or percent from one of its ends (count > 1:
    /// every distance_mm / percent after that too), or count points spread evenly.
    Along,
    /// At a vertex or sketch point. (SolidWorks has no point at a bare vertex: it's made at the
    /// end of one of the vertex's edges, so that edge is its parent.)
    At,
}

impl From<PointKind> for proto::ReferencePointKind {
    fn from(k: PointKind) -> Self {
        use proto::ReferencePointKind as K;
        match k {
            PointKind::Center => K::ArcCenter,
            PointKind::FaceCenter => K::FaceCenter,
            PointKind::Projection => K::Projection,
            PointKind::Intersection => K::Intersection,
            PointKind::Along => K::AlongCurve,
            PointKind::At => K::AtPoint,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReferencePointParams {
    /// One or two references: an edge or sketch segment, a face, a vertex or sketch point,
    /// "origin", a plane ("front"/"top"/"right", "feature:N", a name) -- see `kind`.
    pub references: Vec<String>,
    /// "center", "face_center", "projection", "intersection", "along" or "at". Defaults from
    /// the references: distance_mm/percent/count -> "along"; one face -> "face_center"; one
    /// edge, arc or circle -> "center"; one vertex / sketch point -> "at"; a point and a face
    /// or plane -> "projection"; any other two -> "intersection".
    #[serde(default)]
    pub kind: Option<PointKind>,
    /// kind "along": distance from one end of the curve, in millimeters. Which end is
    /// SolidWorks' choice: check the reported position (for the other end, use the length minus
    /// this).
    #[serde(default)]
    pub distance_mm: Option<f64>,
    /// kind "along": position as a percentage of the curve's length (0-100), from one end as
    /// for distance_mm.
    #[serde(default)]
    pub percent: Option<f64>,
    /// kind "along": how many points. Alone: spread evenly over the curve; with distance_mm /
    /// percent: one every distance_mm / percent. Defaults to 1. At most 100.
    #[serde(default)]
    pub count: Option<u32>,
    /// Name for the new point (several: Name, Name2, ...). Defaults to SolidWorks' "PointN".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CoordinateSystemParams {
    /// Where its origin goes: a vertex, sketch point or reference point. Defaults to the part
    /// origin.
    #[serde(default)]
    pub origin: Option<String>,
    /// What its X axis follows: an edge, sketch line or axis (along it), a plane or planar
    /// face (along its normal), or a vertex / point (pointing at it). Give at most two of
    /// x_axis/y_axis/z_axis; the third follows. Unset axes keep the model's directions.
    #[serde(default)]
    pub x_axis: Option<String>,
    /// What its Y axis follows; see x_axis.
    #[serde(default)]
    pub y_axis: Option<String>,
    /// What its Z axis follows; see x_axis.
    #[serde(default)]
    pub z_axis: Option<String>,
    /// Reverse the X axis. Defaults to false.
    #[serde(default)]
    pub flip_x: bool,
    /// Reverse the Y axis. Defaults to false.
    #[serde(default)]
    pub flip_y: bool,
    /// Reverse the Z axis. Defaults to false.
    #[serde(default)]
    pub flip_z: bool,
    /// Name for the new coordinate system. Defaults to SolidWorks' "Coordinate SystemN".
    #[serde(default)]
    pub name: Option<String>,
}

/// What a reference string names, as far as its form tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A vertex, sketch point or the origin.
    Point,
    /// A face or a default plane.
    Surface,
    /// An edge (straight or curved; the id doesn't say).
    Edge,
    /// A sketch arc or circle.
    Round,
    /// A sketch line, spline or other segment.
    Segment,
    /// A feature id or name: a plane, axis, point or coordinate system.
    Feature,
}

fn standard_plane(s: &str) -> Option<proto::Plane> {
    let s = s.to_ascii_lowercase();
    let s = s.strip_suffix(" plane").unwrap_or(&s);
    match s {
        "front" => Some(proto::Plane::Front),
        "top" => Some(proto::Plane::Top),
        "right" => Some(proto::Plane::Right),
        _ => None,
    }
}

/// One reference string -> its wire ref and shape. `what` names it in errors.
fn geometry_ref(ids: &IdRegistry, entity: &str, what: &str) -> Result<(proto::GeometryRef, Shape), McpError> {
    use proto::geometry_ref::Target;
    let entity = entity.trim();
    if entity.is_empty() {
        return Err(invalid(format!("{what} is empty")));
    }
    let (target, shape) = if entity.eq_ignore_ascii_case("origin") {
        (Target::Origin(true), Shape::Point)
    } else if let Some(plane) = standard_plane(entity) {
        (Target::StandardPlane(plane.into()), Shape::Surface)
    } else {
        match Kind::of_id(entity) {
            None => (Target::FeatureName(entity.to_string()), Shape::Feature),
            Some(kind @ (Kind::Face | Kind::Edge | Kind::Vertex | Kind::Feature)) => {
                let persist_id = ids.persist_id(entity).ok_or_else(|| {
                    invalid(format!(
                        "{what}: {}",
                        ids.unknown_id(entity, "inspect_model", "call inspect_model for current ones")
                    ))
                })?;
                let shape = match kind {
                    Kind::Face => Shape::Surface,
                    Kind::Edge => Shape::Edge,
                    Kind::Vertex => Shape::Point,
                    _ => Shape::Feature,
                };
                (Target::ModelPersistId(persist_id), shape)
            }
            Some(kind) if SKETCH_KINDS.contains(&kind) => {
                let item = sketch::decode_ref(ids, entity).ok_or_else(|| {
                    invalid(format!(
                        "{what}: {}",
                        ids.unknown_id(
                            entity,
                            "inspect_sketch and the sketch_* tools",
                            "call inspect_sketch for current ones"
                        )
                    ))
                })?;
                let shape = match kind {
                    Kind::SketchPoint => Shape::Point,
                    Kind::Arc | Kind::Circle => Shape::Round,
                    _ => Shape::Segment,
                };
                (Target::SketchItem(item), shape)
            }
            Some(_) => {
                return Err(invalid(format!(
                    "{what}: {entity} can't be a reference -- give {REFERENCE_DOC}"
                )));
            }
        }
    };
    Ok((proto::GeometryRef { target: Some(target) }, shape))
}

fn name(name: &Option<String>) -> String {
    name.as_deref().map(str::trim).unwrap_or_default().to_string()
}

fn request(geometry: proto::create_reference_geometry_request::Geometry, name: String) -> proto::CreateReferenceGeometryRequest {
    proto::CreateReferenceGeometryRequest { geometry: Some(geometry), name }
}

pub fn plane_request(
    ids: &IdRegistry,
    params: &ReferencePlaneParams,
) -> Result<proto::CreateReferenceGeometryRequest, McpError> {
    let n = params.references.len();
    if n == 0 || n > MAX_PLANE_REFERENCES {
        return Err(invalid(format!("a plane takes 1 to {MAX_PLANE_REFERENCES} references, got {n}")));
    }
    let mut references = Vec::new();
    for (i, r) in params.references.iter().enumerate() {
        let what = format!("reference {}", i + 1);
        let (geometry, _) = geometry_ref(ids, &r.entity, &what)?;
        let relation = match (r.relation, r.offset_mm, r.angle_deg) {
            (_, Some(_), Some(_)) => return Err(invalid(format!("{what}: give offset_mm or angle_deg, not both"))),
            (None | Some(PlaneRelation::Distance), Some(_), None) => PlaneRelation::Distance,
            (None | Some(PlaneRelation::Angle), None, Some(_)) => PlaneRelation::Angle,
            (Some(PlaneRelation::Distance), None, None) => {
                return Err(invalid(format!("{what}: relation \"distance\" needs offset_mm")));
            }
            (Some(PlaneRelation::Angle), None, None) => {
                return Err(invalid(format!("{what}: relation \"angle\" needs angle_deg")));
            }
            (Some(other), Some(_), None) | (Some(other), None, Some(_)) => {
                return Err(invalid(format!(
                    "{what}: offset_mm / angle_deg only go with relation \"distance\" / \"angle\", \
                     not \"{}\"",
                    other.key()
                )));
            }
            (relation, None, None) => relation.unwrap_or(PlaneRelation::Coincident),
        };
        let distance = match r.offset_mm {
            Some(d) if !(d.is_finite() && d > 0.0) => {
                return Err(invalid(format!(
                    "{what}: offset_mm must be more than 0, got {d} (use flip for the other side)"
                )));
            }
            Some(d) => d / MM_PER_M,
            None => 0.0,
        };
        let angle = match r.angle_deg {
            Some(a) if !(a.is_finite() && a > 0.0 && a < 360.0) => {
                return Err(invalid(format!(
                    "{what}: angle_deg must be more than 0 and less than 360, got {a} (use flip for the \
                     other way)"
                )));
            }
            Some(a) => a.to_radians(),
            None => 0.0,
        };
        references.push(proto::PlaneReference {
            r#ref: Some(geometry),
            constraint: proto::PlaneConstraint::from(relation).into(),
            distance,
            angle,
            flip: r.flip,
        });
    }
    let mids = params.references.iter().filter(|r| r.relation == Some(PlaneRelation::MidPlane)).count();
    if mids == 1 {
        return Err(invalid("a mid plane needs two references with relation \"mid_plane\""));
    }
    Ok(request(
        proto::create_reference_geometry_request::Geometry::Plane(proto::ReferencePlaneSpec { references }),
        name(&params.name),
    ))
}

pub fn axis_request(
    ids: &IdRegistry,
    params: &ReferenceAxisParams,
) -> Result<proto::CreateReferenceGeometryRequest, McpError> {
    let n = params.references.len();
    if n == 0 || n > 2 {
        return Err(invalid(format!("an axis takes 1 or 2 references, got {n}")));
    }
    let references = params
        .references
        .iter()
        .enumerate()
        .map(|(i, r)| geometry_ref(ids, r, &format!("reference {}", i + 1)).map(|(g, _)| g))
        .collect::<Result<_, _>>()?;
    Ok(request(
        proto::create_reference_geometry_request::Geometry::Axis(proto::ReferenceAxisSpec { references }),
        name(&params.name),
    ))
}

/// The point kind the references and options call for, when not given.
fn infer_point_kind(params: &ReferencePointParams, shapes: &[Shape]) -> Result<PointKind, McpError> {
    if params.distance_mm.is_some() || params.percent.is_some() || params.count.is_some() {
        return Ok(PointKind::Along);
    }
    use Shape as S;
    match shapes {
        [S::Surface] => Ok(PointKind::FaceCenter),
        [S::Edge | S::Round] => Ok(PointKind::Center),
        [S::Point] => Ok(PointKind::At),
        [S::Segment] => Err(invalid(
            "for a point on a sketch line or spline, give distance_mm, percent or count (kind \
             \"along\"); for its end, use the end point's id (kind \"at\")",
        )),
        [_] => Err(invalid(
            "can't tell what point to make from one feature -- give `kind` (\"at\" a reference \
             point, ...) or other references",
        )),
        [S::Point, S::Surface | S::Feature] | [S::Surface | S::Feature, S::Point] => Ok(PointKind::Projection),
        _ => Ok(PointKind::Intersection),
    }
}

pub fn point_request(
    ids: &IdRegistry,
    params: &ReferencePointParams,
) -> Result<proto::CreateReferenceGeometryRequest, McpError> {
    let n = params.references.len();
    if n == 0 || n > 2 {
        return Err(invalid(format!("a reference point takes 1 or 2 references, got {n}")));
    }
    let mut references = Vec::new();
    let mut shapes = Vec::new();
    for (i, r) in params.references.iter().enumerate() {
        let (g, shape) = geometry_ref(ids, r, &format!("reference {}", i + 1))?;
        references.push(g);
        shapes.push(shape);
    }
    let kind = match params.kind {
        Some(kind) => kind,
        None => infer_point_kind(params, &shapes)?,
    };
    // A projection takes the point first.
    if kind == PointKind::Projection && shapes[0] != Shape::Point && shapes.get(1) == Some(&Shape::Point) {
        references.swap(0, 1);
    }

    let mut spec = proto::ReferencePointSpec {
        kind: proto::ReferencePointKind::from(kind).into(),
        references,
        ..Default::default()
    };
    if kind == PointKind::Along {
        if n != 1 {
            return Err(invalid("a point along a curve takes one reference: the edge or sketch segment"));
        }
        let count = params.count.unwrap_or(1);
        if count == 0 || count > MAX_POINTS {
            return Err(invalid(format!("count must be 1 to {MAX_POINTS}, got {count}")));
        }
        spec.count = count as i32;
        spec.along = match (params.distance_mm, params.percent) {
            (Some(_), Some(_)) => return Err(invalid("give distance_mm or percent, not both")),
            (Some(d), None) => {
                if !(d.is_finite() && d > 0.0) {
                    return Err(invalid(format!("distance_mm must be more than 0, got {d}")));
                }
                spec.distance = d / MM_PER_M;
                proto::AlongCurve::Distance
            }
            (None, Some(p)) => {
                if !(p.is_finite() && (0.0..=100.0).contains(&p)) {
                    return Err(invalid(format!("percent must be 0 to 100, got {p}")));
                }
                spec.percent = p;
                proto::AlongCurve::Percent
            }
            (None, None) => proto::AlongCurve::Evenly,
        }
        .into();
    } else if params.distance_mm.is_some() || params.percent.is_some() || params.count.is_some() {
        return Err(invalid("distance_mm, percent and count only go with kind \"along\""));
    }
    Ok(request(proto::create_reference_geometry_request::Geometry::Point(spec), name(&params.name)))
}

pub fn coordinate_system_request(
    ids: &IdRegistry,
    params: &CoordinateSystemParams,
) -> Result<proto::CreateReferenceGeometryRequest, McpError> {
    let one = |r: &Option<String>, what: &str| -> Result<Option<proto::GeometryRef>, McpError> {
        r.as_deref().map(|r| geometry_ref(ids, r, what).map(|(g, _)| g)).transpose()
    };
    let spec = proto::CoordinateSystemSpec {
        origin: one(&params.origin, "origin")?,
        x_axis: one(&params.x_axis, "x_axis")?,
        y_axis: one(&params.y_axis, "y_axis")?,
        z_axis: one(&params.z_axis, "z_axis")?,
        flip_x: params.flip_x,
        flip_y: params.flip_y,
        flip_z: params.flip_z,
    };
    let axes = [&spec.x_axis, &spec.y_axis, &spec.z_axis].iter().filter(|a| a.is_some()).count();
    if axes == 3 {
        return Err(invalid("give at most two of x_axis, y_axis and z_axis: the third follows from them"));
    }
    Ok(request(
        proto::create_reference_geometry_request::Geometry::CoordinateSystem(spec),
        name(&params.name),
    ))
}

/// "[0, 0, 1]" for text.
fn dir_text(v: &proto::Vec3) -> String {
    format!("[{}, {}, {}]", round(v.x, 6), round(v.y, 6), round(v.z, 6))
}

fn mm_text(v: &proto::Vec3) -> String {
    format!("[{}, {}, {}] mm", mm(v.x), mm(v.y), mm(v.z))
}

/// Where a new reference feature ended up, in words.
fn where_text(f: &proto::FeatureInfo) -> String {
    let Some(r) = &f.reference else {
        return "(SolidWorks didn't say where it is)".into();
    };
    match (f.type_name.as_str(), &r.origin, &r.direction) {
        ("RefPlane", Some(o), Some(n)) => format!("plane with normal {} through {}", dir_text(n), mm_text(o)),
        ("RefAxis", Some(o), Some(d)) => {
            format!("axis along {} through {} ({} mm shown)", dir_text(d), mm_text(o), mm(r.length))
        }
        ("RefPoint", Some(o), _) => format!("point at {}", mm_text(o)),
        ("CoordSys", Some(o), _) => match (&r.x_axis, &r.y_axis, &r.z_axis) {
            (Some(x), Some(y), Some(z)) => format!(
                "coordinate system at {} with X {}, Y {}, Z {}",
                mm_text(o),
                dir_text(x),
                dir_text(y),
                dir_text(z)
            ),
            _ => format!("coordinate system at {}", mm_text(o)),
        },
        _ => "(SolidWorks didn't say where it is)".into(),
    }
}

/// The JSON a feature gets in inspect_model, for the reply.
fn feature_value(ids: &mut IdRegistry, f: &proto::FeatureInfo) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), ids.id_for(Kind::Feature, &f.persist_id).map_or(Value::Null, Value::String));
    obj.insert("name".into(), Value::String(f.name.clone()));
    obj.insert("type".into(), Value::String(f.type_name.clone()));
    if let Some(r) = &f.reference {
        inspect::reference_json(&mut obj, &f.type_name, r);
    }
    Value::Object(obj)
}

/// The reply: what was made, where it is, and how to use it.
pub fn reply_text(ids: &mut IdRegistry, reply: &proto::CreateReferenceGeometryResponse, from: &str) -> String {
    let mut lines = Vec::new();
    let mut first_id = None;
    for f in &reply.features {
        let id = ids.id_for(Kind::Feature, &f.persist_id);
        if first_id.is_none() {
            first_id = id.clone();
        }
        let mut line = format!(
            "Created {} ({}): {}",
            f.name,
            id.as_deref().unwrap_or("no id"),
            where_text(f)
        );
        if f.error_code != 0 {
            line += &format!(
                ". SolidWorks warns: {}",
                crate::delete::feature_error_name(f.error_code)
            );
        }
        lines.push(line);
    }
    let mut text = if lines.len() > 1 {
        let from = if from.is_empty() { String::new() } else { format!(" {from}") };
        format!("{} features{from}:\n{}.", lines.len(), lines.join(".\n"))
    } else if from.is_empty() {
        format!("{}.", lines.concat())
    } else {
        format!("{} -- {from}.", lines.concat())
    };
    let plane = reply.features.iter().any(|f| f.type_name == "RefPlane");
    if let (true, Some(id)) = (plane, &first_id) {
        text += &format!(
            "\nSketch on it with the sketch tools' on=\"{id}\" (the sketch normal is the plane's)."
        );
    } else if let Some(id) = &first_id {
        text += &format!(" Use {id} as a reference for other reference geometry or in add_mate.");
    }
    let json: Vec<Value> = reply.features.iter().map(|f| feature_value(ids, f)).collect();
    text += &format!("\n{}", Value::Array(json));
    text
}

/// "offset from Top Plane by 20 mm, through edge:4" for the plane reply.
pub fn plane_from_text(params: &ReferencePlaneParams) -> String {
    params
        .references
        .iter()
        .map(|r| {
            let relation = match (r.relation, r.offset_mm, r.angle_deg) {
                (_, Some(_), _) => PlaneRelation::Distance,
                (_, _, Some(_)) => PlaneRelation::Angle,
                (relation, _, _) => relation.unwrap_or(PlaneRelation::Coincident),
            };
            let mut text = format!("{} {}", relation.name(), r.entity.trim());
            if let Some(d) = r.offset_mm {
                text += &format!(" by {d} mm");
            }
            if let Some(a) = r.angle_deg {
                text += &format!(" by {a}°");
            }
            if r.flip {
                text += " (flipped)";
            }
            text
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// "from edge:3, origin" for the other replies.
pub fn from_text(references: &[&str]) -> String {
    let refs: Vec<&str> = references.iter().map(|r| r.trim()).filter(|r| !r.is_empty()).collect();
    if refs.is_empty() { String::new() } else { format!("from {}", refs.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> IdRegistry {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        ids
    }

    fn plane_ref(entity: &str) -> PlaneReference {
        PlaneReference { entity: entity.into(), relation: None, offset_mm: None, angle_deg: None, flip: false }
    }

    #[test]
    fn plane_relations_follow_the_values() {
        let mut ids = registry();
        let face = ids.id_for(Kind::Face, b"face-bytes").unwrap();
        let params = ReferencePlaneParams {
            references: vec![
                PlaneReference { offset_mm: Some(20.0), ..plane_ref("top") },
                PlaneReference { angle_deg: Some(30.0), flip: true, ..plane_ref(&face) },
                plane_ref("origin"),
            ],
            name: Some(" Mount ".into()),
        };
        let request = plane_request(&ids, &params).unwrap();
        assert_eq!(request.name, "Mount");
        let Some(proto::create_reference_geometry_request::Geometry::Plane(spec)) = request.geometry else {
            panic!("not a plane");
        };
        let r = &spec.references;
        assert_eq!(r[0].constraint(), proto::PlaneConstraint::Distance);
        assert!((r[0].distance - 0.02).abs() < 1e-12);
        assert_eq!(
            r[0].r#ref.as_ref().unwrap().target,
            Some(proto::geometry_ref::Target::StandardPlane(proto::Plane::Top.into()))
        );
        assert_eq!(r[1].constraint(), proto::PlaneConstraint::Angle);
        assert!((r[1].angle - 30f64.to_radians()).abs() < 1e-12 && r[1].flip);
        assert_eq!(
            r[1].r#ref.as_ref().unwrap().target,
            Some(proto::geometry_ref::Target::ModelPersistId(b"face-bytes".to_vec()))
        );
        assert_eq!(r[2].constraint(), proto::PlaneConstraint::Coincident);
        assert_eq!(r[2].r#ref.as_ref().unwrap().target, Some(proto::geometry_ref::Target::Origin(true)));
    }

    #[test]
    fn plane_rejects_mismatched_values() {
        let ids = registry();
        let bad = |r: PlaneReference| plane_request(&ids, &ReferencePlaneParams { references: vec![r], name: None });
        assert!(bad(PlaneReference { relation: Some(PlaneRelation::Distance), ..plane_ref("top") }).is_err());
        assert!(bad(PlaneReference { relation: Some(PlaneRelation::Parallel), offset_mm: Some(5.0), ..plane_ref("top") }).is_err());
        assert!(bad(PlaneReference { offset_mm: Some(-5.0), ..plane_ref("top") }).is_err());
        assert!(bad(PlaneReference { relation: Some(PlaneRelation::MidPlane), ..plane_ref("top") }).is_err());
        assert!(bad(plane_ref("body:1")).is_err());
        assert!(bad(plane_ref("face:99")).is_err());
        assert!(plane_request(&ids, &ReferencePlaneParams { references: vec![], name: None }).is_err());
    }

    #[test]
    fn names_and_plane_words_are_references() {
        let ids = registry();
        let (g, shape) = geometry_ref(&ids, "Right Plane", "r").unwrap();
        assert_eq!(g.target, Some(proto::geometry_ref::Target::StandardPlane(proto::Plane::Right.into())));
        assert_eq!(shape, Shape::Surface);
        let (g, shape) = geometry_ref(&ids, " Plane1 ", "r").unwrap();
        assert_eq!(g.target, Some(proto::geometry_ref::Target::FeatureName("Plane1".into())));
        assert_eq!(shape, Shape::Feature);
    }

    fn point_params(references: &[&str]) -> ReferencePointParams {
        ReferencePointParams {
            references: references.iter().map(|r| r.to_string()).collect(),
            kind: None,
            distance_mm: None,
            percent: None,
            count: None,
            name: None,
        }
    }

    fn point_spec(request: proto::CreateReferenceGeometryRequest) -> proto::ReferencePointSpec {
        match request.geometry {
            Some(proto::create_reference_geometry_request::Geometry::Point(spec)) => spec,
            _ => panic!("not a point"),
        }
    }

    #[test]
    fn point_kind_is_inferred() {
        let mut ids = registry();
        let face = ids.id_for(Kind::Face, b"f").unwrap();
        let edge = ids.id_for(Kind::Edge, b"e").unwrap();
        let vertex = ids.id_for(Kind::Vertex, b"v").unwrap();
        use proto::ReferencePointKind as K;
        let kind = |refs: &[&str]| point_spec(point_request(&ids, &point_params(refs)).unwrap()).kind();
        assert_eq!(kind(&[&face]), K::FaceCenter);
        assert_eq!(kind(&[&edge]), K::ArcCenter);
        assert_eq!(kind(&[&vertex]), K::AtPoint);
        assert_eq!(kind(&["origin"]), K::AtPoint);
        assert_eq!(kind(&[&edge, &face]), K::Intersection);
        // A projection takes the point first, whichever order it's given in.
        let spec = point_spec(point_request(&ids, &point_params(&["front", &vertex])).unwrap());
        assert_eq!(spec.kind(), K::Projection);
        assert_eq!(spec.references[0].target, Some(proto::geometry_ref::Target::ModelPersistId(b"v".to_vec())));
    }

    #[test]
    fn points_along_a_curve() {
        let mut ids = registry();
        let edge = ids.id_for(Kind::Edge, b"e").unwrap();
        let spec = point_spec(
            point_request(&ids, &ReferencePointParams { percent: Some(25.0), ..point_params(&[&edge]) }).unwrap(),
        );
        assert_eq!(spec.kind(), proto::ReferencePointKind::AlongCurve);
        assert_eq!(spec.along(), proto::AlongCurve::Percent);
        assert_eq!((spec.percent, spec.count), (25.0, 1));
        let spec =
            point_spec(point_request(&ids, &ReferencePointParams { count: Some(5), ..point_params(&[&edge]) }).unwrap());
        assert_eq!((spec.along(), spec.count), (proto::AlongCurve::Evenly, 5));
        let spec = point_spec(
            point_request(&ids, &ReferencePointParams { distance_mm: Some(10.0), ..point_params(&[&edge]) }).unwrap(),
        );
        assert_eq!(spec.along(), proto::AlongCurve::Distance);
        assert!((spec.distance - 0.01).abs() < 1e-12);
        assert!(point_request(&ids, &ReferencePointParams { count: Some(0), ..point_params(&[&edge]) }).is_err());
        assert!(point_request(&ids, &ReferencePointParams { percent: Some(150.0), ..point_params(&[&edge]) }).is_err());
        assert!(
            point_request(
                &ids,
                &ReferencePointParams { kind: Some(PointKind::Center), count: Some(2), ..point_params(&[&edge]) }
            )
            .is_err()
        );
    }

    #[test]
    fn coordinate_systems_take_two_axes_at_most() {
        let ids = registry();
        let params = |x: Option<&str>, y: Option<&str>, z: Option<&str>| CoordinateSystemParams {
            origin: None,
            x_axis: x.map(String::from),
            y_axis: y.map(String::from),
            z_axis: z.map(String::from),
            flip_x: false,
            flip_y: true,
            flip_z: false,
            name: None,
        };
        let request = coordinate_system_request(&ids, &params(Some("front"), None, Some("top"))).unwrap();
        let Some(proto::create_reference_geometry_request::Geometry::CoordinateSystem(spec)) = request.geometry else {
            panic!("not a coordinate system");
        };
        assert!(spec.origin.is_none() && spec.y_axis.is_none() && spec.flip_y);
        assert!(coordinate_system_request(&ids, &params(Some("front"), Some("top"), Some("right"))).is_err());
    }

    #[test]
    fn reply_says_where_and_how_to_use_it() {
        let mut ids = registry();
        let reply = proto::CreateReferenceGeometryResponse {
            features: vec![proto::FeatureInfo {
                persist_id: b"plane".to_vec(),
                name: "Plane1".into(),
                type_name: "RefPlane".into(),
                reference: Some(proto::ReferenceGeometry {
                    origin: Some(proto::Vec3 { x: 0.0, y: 0.0, z: 0.02 }),
                    direction: Some(proto::Vec3 { x: 0.0, y: 0.0, z: 1.0 }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        };
        let text = reply_text(&mut ids, &reply, "offset from top by 20 mm");
        assert!(text.starts_with("Created Plane1 (feature:1): plane with normal [0, 0, 1] through [0, 0, 20] mm"));
        assert!(text.contains("on=\"feature:1\""));
        assert!(text.contains("\"point_on_plane_mm\":[0.0,0.0,20.0]"));
    }
}
