//! Assemblies: placing component geometry in assembly space, describing
//! components and mates for the model, and the add_mate tool.
//!
//! The add-in reports a component's faces, edges, bodies and mesh in the
//! component's own coordinates (when `local_geometry` is set) together with
//! the component's placement; everything shown to the model is moved into
//! assembly space here, so coordinates from different components can be
//! compared directly.

use std::collections::HashMap;

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use glam::{BVec3, DAffine3, DMat3, DVec3};

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, direction, mm, point_mm, round};

// ---- Placement -------------------------------------------------------------

fn dvec3(v: &proto::Vec3) -> DVec3 {
    DVec3::new(v.x, v.y, v.z)
}

fn to_proto(v: DVec3) -> proto::Vec3 {
    proto::Vec3 { x: v.x, y: v.y, z: v.z }
}

/// Where a component sits in the assembly: the rigid transform from its own
/// coordinates to assembly space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement(DAffine3);

impl Placement {
    /// The placement to apply to a component's geometry; None when its
    /// geometry is already in assembly space or the placement is missing.
    pub fn of(component: &proto::ComponentInfo) -> Option<Placement> {
        if !component.local_geometry {
            return None;
        }
        Self::from_component(component)
    }

    /// The component's placement whether or not its geometry needs it.
    fn from_component(component: &proto::ComponentInfo) -> Option<Placement> {
        // The add-in reports where the component's unit axes land.
        // SolidWorks components aren't scaled, so normalize them into a
        // rotation.
        let axis = |v: &Option<proto::Vec3>| v.as_ref().map(dvec3)?.try_normalize();
        let rotation = DMat3::from_cols(
            axis(&component.x_axis)?,
            axis(&component.y_axis)?,
            axis(&component.z_axis)?,
        );
        let origin = dvec3(component.origin.as_ref()?);
        Some(Placement(DAffine3::from_mat3_translation(rotation, origin)))
    }

    pub fn point(&self, p: DVec3) -> DVec3 {
        self.0.transform_point3(p)
    }

    pub fn vector(&self, v: DVec3) -> DVec3 {
        self.0.transform_vector3(v)
    }

    fn place_point(&self, v: &mut Option<proto::Vec3>) {
        if let Some(v) = v {
            *v = to_proto(self.point(dvec3(v)));
        }
    }

    fn place_direction(&self, v: &mut Option<proto::Vec3>) {
        if let Some(v) = v {
            *v = to_proto(self.0.transform_vector3(dvec3(v)));
        }
    }

    /// The axis-aligned box around the placed box.
    fn place_box(&self, b: &mut Option<proto::Box3>) {
        let Some(b) = b else { return };
        let (Some(lo), Some(hi)) = (b.min.as_ref().map(dvec3), b.max.as_ref().map(dvec3)) else {
            return;
        };
        let (mut new_lo, mut new_hi) = (DVec3::INFINITY, DVec3::NEG_INFINITY);
        for corner in 0..8 {
            let p = DVec3::select(BVec3::new(corner & 1 != 0, corner & 2 != 0, corner & 4 != 0), hi, lo);
            let q = self.point(p);
            new_lo = new_lo.min(q);
            new_hi = new_hi.max(q);
        }
        b.min = Some(to_proto(new_lo));
        b.max = Some(to_proto(new_hi));
    }

    fn place_floats(&self, values: &mut [f32], is_point: bool) {
        for p in values.chunks_exact_mut(3) {
            let v = DVec3::new(p[0].into(), p[1].into(), p[2].into());
            let q = if is_point { self.point(v) } else { self.0.transform_vector3(v) };
            (p[0], p[1], p[2]) = (q.x as f32, q.y as f32, q.z as f32);
        }
    }

    /// Whether the component is turned relative to the assembly.
    fn is_rotated(&self) -> bool {
        !self.0.matrix3.abs_diff_eq(DMat3::IDENTITY, 1e-9)
    }
}

/// Placements by component persist id, for the components that need one.
fn placements(components: &[proto::ComponentInfo]) -> HashMap<Vec<u8>, Placement> {
    components
        .iter()
        .filter_map(|c| Some((c.persist_id.clone(), Placement::of(c)?)))
        .collect()
}

/// Moves every component face, edge and body in an InspectModel reply into
/// assembly space, in place. Parts (no components) are left as they are.
pub fn place_inspect_reply(reply: &mut proto::InspectModelResponse) {
    let placements = placements(&reply.components);
    if placements.is_empty() {
        return;
    }
    for face in &mut reply.faces {
        let Some(p) = placements.get(&face.component_persist_id) else { continue };
        p.place_box(&mut face.r#box);
        // Plane normals, axes and sample normals are directions; origins
        // (points on planes, axes, centers) and samples are points.
        p.place_direction(&mut face.normal);
        p.place_direction(&mut face.axis);
        p.place_direction(&mut face.sample_normal);
        p.place_point(&mut face.origin);
        p.place_point(&mut face.sample_point);
    }
    for edge in &mut reply.edges {
        let Some(p) = placements.get(&edge.component_persist_id) else { continue };
        p.place_point(&mut edge.start);
        p.place_point(&mut edge.end);
        p.place_point(&mut edge.center);
        p.place_direction(&mut edge.axis);
    }
    for body in &mut reply.bodies {
        let Some(p) = placements.get(&body.component_persist_id) else { continue };
        p.place_box(&mut body.r#box);
        if let Some(mass) = &mut body.mass {
            p.place_point(&mut mass.center_of_mass);
        }
    }
}

/// Moves a GetMesh reply's component faces and edges into assembly space.
pub fn place_mesh(mesh: &mut proto::GetMeshResponse) {
    let placements = placements(&mesh.components);
    if placements.is_empty() {
        return;
    }
    for face in &mut mesh.faces {
        let Some(p) = placements.get(&face.component_persist_id) else { continue };
        p.place_floats(&mut face.triangles, true);
        p.place_floats(&mut face.normals, false);
    }
    for edge in &mut mesh.edges {
        let Some(p) = placements.get(&edge.component_persist_id) else { continue };
        p.place_floats(&mut edge.points, true);
    }
}

// ---- Rendering components and mates ------------------------------------------

/// "C:\models\bracket.SLDPRT" -> "bracket.SLDPRT".
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

fn id_value(id: Option<String>) -> Value {
    id.map_or(Value::Null, Value::String)
}

pub fn component_json(ids: &mut IdRegistry, c: &proto::ComponentInfo) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), id_value(ids.id_for(Kind::Component, &c.persist_id)));
    obj.insert("name".into(), json!(c.name));
    if !c.file.is_empty() {
        obj.insert("file".into(), json!(file_name(&c.file)));
    }
    if !c.configuration.is_empty() {
        obj.insert("configuration".into(), json!(c.configuration));
    }
    obj.insert("fixed".into(), json!(c.fixed));
    match c.state() {
        proto::ComponentState::Lightweight => {
            obj.insert("state".into(), json!("lightweight (resolve it to see its geometry)"));
        }
        proto::ComponentState::Suppressed => {
            obj.insert("state".into(), json!("suppressed"));
        }
        _ => {}
    }
    if c.hidden {
        obj.insert("hidden".into(), json!(true));
    }
    if c.subassembly {
        obj.insert("subassembly".into(), json!(true));
    }
    if !c.parent_persist_id.is_empty() {
        obj.insert("parent".into(), id_value(ids.id_for(Kind::Component, &c.parent_persist_id)));
    }
    if let Some(origin) = &c.origin {
        obj.insert("origin_mm".into(), point_mm(origin));
    }
    // Axes only when rotated: most components sit square to the assembly.
    let rotated = Placement::from_component(c).is_some_and(|p| p.is_rotated());
    if rotated && let (Some(x), Some(y), Some(z)) = (&c.x_axis, &c.y_axis, &c.z_axis) {
        obj.insert(
            "axes".into(),
            json!({ "x": direction(x), "y": direction(y), "z": direction(z) }),
        );
    }
    if let Some(b) = &c.r#box
        && let (Some(lo), Some(hi)) = (&b.min, &b.max)
    {
        obj.insert("bbox_mm".into(), json!({ "min": point_mm(lo), "max": point_mm(hi) }));
    }
    Value::Object(obj)
}

pub fn mate_type_name(t: proto::MateType) -> &'static str {
    use proto::MateType as M;
    match t {
        M::Coincident => "coincident",
        M::Concentric => "concentric",
        M::Perpendicular => "perpendicular",
        M::Parallel => "parallel",
        M::Tangent => "tangent",
        M::Distance => "distance",
        M::Angle => "angle",
        M::Lock => "lock",
        M::Other | M::Unspecified => "other",
    }
}

pub fn alignment_name(a: proto::MateAlignment) -> Option<&'static str> {
    use proto::MateAlignment as A;
    match a {
        A::Aligned => Some("aligned"),
        A::AntiAligned => Some("anti_aligned"),
        A::Closest => Some("closest"),
        A::Unspecified => None,
    }
}

/// swFeatureError_e values that concern mates.
pub fn mate_error_name(code: i32) -> String {
    match code {
        38 | 39 | 41 | 44 => "the mated entities don't suit this mate type".into(),
        42 => "unknown tangency".into(),
        43 | 48 => "a mated entity is missing".into(),
        45 => "a mated entity failed".into(),
        46 => "over-defined: conflicts with other mates or fixed components".into(),
        47 => "can't be solved (ill-defined)".into(),
        code => format!("SolidWorks error {code}"),
    }
}

fn mate_value(m: &proto::MateInfo, obj: &mut Map<String, Value>) {
    if let Some(dim) = &m.dimension
        && !dim.name.is_empty()
    {
        if dim.r#type() == proto::DimensionType::Angular {
            obj.insert("angle_deg".into(), json!(round(dim.value.to_degrees(), 4)));
        } else {
            obj.insert("distance_mm".into(), json!(mm(dim.value)));
        }
        obj.insert("dimension".into(), json!(dim.name));
    }
    if let Some(limits) = &m.limits {
        if m.r#type() == proto::MateType::Angle {
            obj.insert("min_deg".into(), json!(round(limits.min.to_degrees(), 4)));
            obj.insert("max_deg".into(), json!(round(limits.max.to_degrees(), 4)));
        } else {
            obj.insert("min_mm".into(), json!(mm(limits.min)));
            obj.insert("max_mm".into(), json!(mm(limits.max)));
        }
    }
}

/// ", limits 0.0 .. 40.0 mm" for a limit mate; empty otherwise.
pub fn limits_text(m: &proto::MateInfo) -> String {
    let Some(limits) = &m.limits else {
        return String::new();
    };
    if m.r#type() == proto::MateType::Angle {
        format!(
            ", limits {} .. {} deg",
            round(limits.min.to_degrees(), 4),
            round(limits.max.to_degrees(), 4)
        )
    } else {
        format!(", limits {} .. {} mm", mm(limits.min), mm(limits.max))
    }
}

pub fn mate_json(ids: &mut IdRegistry, m: &proto::MateInfo) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), id_value(ids.id_for(Kind::Feature, &m.persist_id)));
    obj.insert("name".into(), json!(m.name));
    let kind = m.r#type();
    obj.insert(
        "type".into(),
        if kind == proto::MateType::Other || kind == proto::MateType::Unspecified {
            json!(m.type_name)
        } else {
            json!(mate_type_name(kind))
        },
    );
    if let Some(a) = alignment_name(m.alignment()) {
        obj.insert("alignment".into(), json!(a));
    }
    if m.flipped {
        obj.insert("flipped".into(), json!(true));
    }
    let components: Vec<Value> = m
        .component_persist_ids
        .iter()
        .zip(&m.component_names)
        .map(|(pid, name)| {
            if pid.is_empty() {
                json!("(assembly)")
            } else {
                json!(format!(
                    "{} ({})",
                    name,
                    ids.id_for(Kind::Component, pid).unwrap_or_default()
                ))
            }
        })
        .collect();
    obj.insert("components".into(), Value::Array(components));
    mate_value(m, &mut obj);
    if m.suppressed {
        obj.insert("suppressed".into(), json!(true));
    }
    if m.error_code != 0 {
        let key = if m.error_is_warning { "warning" } else { "error" };
        obj.insert(key.into(), json!(mate_error_name(m.error_code)));
    }
    Value::Object(obj)
}

// ---- add_mate ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MateKind {
    /// Faces/planes flush (coplanar), edges/axes collinear, points together.
    Coincident,
    /// Two round faces, circular edges or axes share an axis (pin in hole).
    Concentric,
    Parallel,
    Perpendicular,
    /// A curved face touching a face (cylinder on plane, ball in socket, ...).
    Tangent,
    /// Faces/planes a set distance apart (distance_mm), or anywhere between min_mm and
    /// max_mm (a limit mate).
    Distance,
    /// Faces/planes/edges at a set angle (angle_deg), or anywhere between min_deg and
    /// max_deg (a limit mate).
    Angle,
    /// Locks two components together as they are (entities = two component ids).
    Lock,
}

impl MateKind {
    fn name(self) -> &'static str {
        match self {
            MateKind::Coincident => "coincident",
            MateKind::Concentric => "concentric",
            MateKind::Parallel => "parallel",
            MateKind::Perpendicular => "perpendicular",
            MateKind::Tangent => "tangent",
            MateKind::Distance => "distance",
            MateKind::Angle => "angle",
            MateKind::Lock => "lock",
        }
    }
}

impl From<MateKind> for proto::MateType {
    fn from(kind: MateKind) -> Self {
        match kind {
            MateKind::Coincident => proto::MateType::Coincident,
            MateKind::Concentric => proto::MateType::Concentric,
            MateKind::Parallel => proto::MateType::Parallel,
            MateKind::Perpendicular => proto::MateType::Perpendicular,
            MateKind::Tangent => proto::MateType::Tangent,
            MateKind::Distance => proto::MateType::Distance,
            MateKind::Angle => proto::MateType::Angle,
            MateKind::Lock => proto::MateType::Lock,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MateAlign {
    /// Whichever is nearer to how the components sit now.
    #[default]
    Closest,
    /// Normals/axes point the same way (faces side by side, facing the same way).
    Aligned,
    /// Normals/axes point opposite ways (faces against each other).
    AntiAligned,
}

impl From<MateAlign> for proto::MateAlignment {
    fn from(a: MateAlign) -> Self {
        match a {
            MateAlign::Closest => proto::MateAlignment::Closest,
            MateAlign::Aligned => proto::MateAlignment::Aligned,
            MateAlign::AntiAligned => proto::MateAlignment::AntiAligned,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddMateParams {
    /// coincident, concentric, parallel, perpendicular, tangent, distance, angle or lock.
    pub kind: MateKind,
    /// The two things to mate, as ids from inspect_model on the assembly, on two different
    /// components: faces or edges ("face:12", "edge:4"), or reference planes/axes ("feature:3"
    /// -- a component's own planes come from inspect_model what="features" within="component:N",
    /// the assembly's from its overview). For kind="lock": two component ids ("component:2").
    pub entities: Vec<String>,
    /// Which way the faces' normals (or axes) end up: "closest" (default; least movement),
    /// "aligned" (same direction) or "anti_aligned" (opposite, e.g. face against face).
    #[serde(default)]
    pub alignment: Option<MateAlign>,
    /// kind="distance" only: the gap between the two entities, in millimeters (0 or more).
    /// For a limit mate: where it starts (default min_mm).
    #[serde(default)]
    pub distance_mm: Option<f64>,
    /// kind="angle" only: the angle between the two entities, 0 to 180 degrees. For a limit
    /// mate: where it starts (default min_deg).
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// kind="distance": make it a limit mate, the gap free to move between min_mm and
    /// max_mm (give both; 0 <= min_mm <= max_mm), e.g. a slider's stroke.
    #[serde(default)]
    pub min_mm: Option<f64>,
    /// kind="distance": the limit mate's largest gap, in millimeters. Give with min_mm.
    #[serde(default)]
    pub max_mm: Option<f64>,
    /// kind="angle": make it a limit mate, the angle free to move between min_deg and
    /// max_deg (give both; 0..180), e.g. how far a lid opens.
    #[serde(default)]
    pub min_deg: Option<f64>,
    /// kind="angle": the limit mate's largest angle, in degrees. Give with min_deg.
    #[serde(default)]
    pub max_deg: Option<f64>,
    /// kind="distance"/"angle": put the gap/angle on the other side.
    #[serde(default)]
    pub flip: bool,
    /// kind="concentric" only: also stop the component spinning about the shared axis.
    #[serde(default)]
    pub lock_rotation: bool,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// A limit mate's (min, max), both given or neither; 0 <= min <= max <= `top`.
fn limit_pair(min: Option<f64>, max: Option<f64>, min_name: &str, max_name: &str, top: f64) -> Result<Option<(f64, f64)>, McpError> {
    match (min, max) {
        (None, None) => Ok(None),
        (Some(lo), Some(hi)) => {
            if !(lo.is_finite() && hi.is_finite() && 0.0 <= lo && lo <= hi && hi <= top) {
                let range = if top.is_finite() { format!(" (0 to {top})") } else { " (0 or more)".into() };
                return Err(invalid(format!(
                    "{min_name} / {max_name} must satisfy 0 <= {min_name} <= {max_name}{range}, got {lo} / {hi}"
                )));
            }
            Ok(Some((lo, hi)))
        }
        _ => Err(invalid(format!("a limit mate needs both {min_name} and {max_name}"))),
    }
}

/// Checks the parameters that don't need the registry and returns the two
/// entity ids, trimmed. The caller resolves them to persist ids.
pub fn check(params: &AddMateParams) -> Result<[String; 2], McpError> {
    let kind = params.kind;
    let entities: Vec<String> = params.entities.iter().map(|e| e.trim().to_string()).collect();
    let [a, b] = <[String; 2]>::try_from(entities)
        .map_err(|_| invalid("a mate takes exactly two entities"))?;
    if a == b {
        return Err(invalid("the two entities are the same one"));
    }
    for e in [&a, &b] {
        let ok = match (kind, Kind::of_id(e)) {
            (MateKind::Lock, Some(Kind::Component)) => true,
            (MateKind::Lock, _) => {
                return Err(invalid(format!(
                    "a lock mate takes two component ids like \"component:2\" (inspect_model \
                     lists them), got {e:?}"
                )));
            }
            (_, Some(Kind::Face | Kind::Edge | Kind::Feature)) => true,
            (_, Some(Kind::Component)) => {
                return Err(invalid(format!(
                    "{e} is a whole component -- only kind=\"lock\" takes components; mate a face, \
                     edge or reference plane of it instead"
                )));
            }
            _ => false,
        };
        if !ok {
            return Err(invalid(format!(
                "{e:?} can't be mated -- give face/edge ids or reference plane/axis feature ids \
                 from inspect_model on the assembly"
            )));
        }
    }

    let distance_limits = limit_pair(params.min_mm, params.max_mm, "min_mm", "max_mm", f64::INFINITY)?;
    let angle_limits = limit_pair(params.min_deg, params.max_deg, "min_deg", "max_deg", 180.0)?;
    if distance_limits.is_some() && kind != MateKind::Distance {
        return Err(invalid("only distance mates take min_mm / max_mm (limit angles: kind=\"angle\" with min_deg / max_deg)"));
    }
    if angle_limits.is_some() && kind != MateKind::Angle {
        return Err(invalid("only angle mates take min_deg / max_deg (limit distances: kind=\"distance\" with min_mm / max_mm)"));
    }
    match (kind, params.distance_mm) {
        (MateKind::Distance, None) if distance_limits.is_none() => {
            return Err(invalid("a distance mate needs distance_mm (or min_mm and max_mm for a limit mate)"));
        }
        (MateKind::Distance, Some(d)) if !(d.is_finite() && d >= 0.0) => {
            return Err(invalid(format!("distance_mm must be 0 or more, got {d}")));
        }
        (MateKind::Distance, _) | (_, None) => {}
        (_, Some(_)) => return Err(invalid("only distance mates take distance_mm")),
    }
    match (kind, params.angle_deg) {
        (MateKind::Angle, None) if angle_limits.is_none() => {
            return Err(invalid("an angle mate needs angle_deg (or min_deg and max_deg for a limit mate)"));
        }
        (MateKind::Angle, Some(d)) if !(d.is_finite() && (0.0..=180.0).contains(&d)) => {
            return Err(invalid(format!("angle_deg must be 0 to 180, got {d}")));
        }
        (MateKind::Angle, _) | (_, None) => {}
        (_, Some(_)) => return Err(invalid("only angle mates take angle_deg")),
    }
    for (value, limits, unit) in [
        (params.distance_mm, distance_limits, "mm"),
        (params.angle_deg, angle_limits, "deg"),
    ] {
        if let (Some(v), Some((lo, hi))) = (value, limits)
            && !(lo..=hi).contains(&v)
        {
            return Err(invalid(format!(
                "the starting value {v} {unit} isn't within the limits {lo} .. {hi} {unit}"
            )));
        }
    }
    if params.lock_rotation && kind != MateKind::Concentric {
        return Err(invalid("lock_rotation only applies to concentric mates"));
    }
    if params.flip && !matches!(kind, MateKind::Distance | MateKind::Angle) {
        return Err(invalid(
            "flip only applies to distance and angle mates -- use alignment to turn a component \
             around",
        ));
    }
    if params.alignment.is_some() && kind == MateKind::Lock {
        return Err(invalid("a lock mate has no alignment"));
    }
    Ok([a, b])
}

pub fn request(params: &AddMateParams, persist_ids: [Vec<u8>; 2]) -> proto::AddMateRequest {
    proto::AddMateRequest {
        r#type: proto::MateType::from(params.kind).into(),
        entity_persist_ids: persist_ids.to_vec(),
        alignment: proto::MateAlignment::from(params.alignment.unwrap_or_default()).into(),
        flip: params.flip,
        distance: params.distance_mm.or(params.min_mm).map_or(0.0, |d| d / MM_PER_M),
        angle: params.angle_deg.or(params.min_deg).map_or(0.0, f64::to_radians),
        lock_rotation: params.lock_rotation,
        limits: match (params.kind, params.min_mm, params.max_mm, params.min_deg, params.max_deg) {
            (MateKind::Distance, Some(lo), Some(hi), _, _) => {
                Some(proto::MateLimits { min: lo / MM_PER_M, max: hi / MM_PER_M })
            }
            (MateKind::Angle, _, _, Some(lo), Some(hi)) => {
                Some(proto::MateLimits { min: lo.to_radians(), max: hi.to_radians() })
            }
            _ => None,
        },
    }
}

/// Where a component ended up, in words.
pub fn placement_text(c: &proto::ComponentInfo) -> String {
    let mut text = c.name.clone();
    if c.fixed {
        text += " (fixed)";
    }
    if let Some(origin) = &c.origin {
        text += &format!(" has its origin at {} mm", point_mm(origin));
        if let (Some(x), Some(y), Some(z)) = (&c.x_axis, &c.y_axis, &c.z_axis) {
            text += &format!(
                ", its X/Y/Z axes along {} / {} / {}",
                direction(x),
                direction(y),
                direction(z)
            );
        }
    }
    text
}

pub fn reply_text(
    ids: &mut IdRegistry,
    params: &AddMateParams,
    entities: &[String; 2],
    reply: &proto::AddMateResponse,
) -> String {
    let Some(mate) = &reply.mate else {
        return format!("added a {} mate.", params.kind.name());
    };
    let mate_id = ids.id_for(Kind::Feature, &mate.persist_id).unwrap_or_default();
    let mut text = format!("added {} ({mate_id}): {}", mate.name, params.kind.name());
    if let Some(a) = alignment_name(mate.alignment()) {
        text += &format!(", {}", a.replace('_', "-"));
    }
    if mate.flipped {
        text += ", flipped";
    }
    let mut value = Map::new();
    mate_value(mate, &mut value);
    if let Some(d) = value.get("distance_mm") {
        text += &format!(", {d} mm");
    } else if let Some(a) = value.get("angle_deg") {
        text += &format!(", {a} deg");
    }
    text += &limits_text(mate);
    let names: Vec<&str> = mate.component_names.iter().map(String::as_str).filter(|n| !n.is_empty()).collect();
    text += &format!(", between {} and {}", entities[0], entities[1]);
    if !names.is_empty() {
        text += &format!(" ({})", names.join(" / "));
    }
    text.push('.');
    if mate.error_code != 0 {
        text += &format!(" SolidWorks flags it: {}.", mate_error_name(mate.error_code));
    }
    if !reply.components.is_empty() {
        let places: Vec<String> = reply.components.iter().map(placement_text).collect();
        text += &format!(" After solving: {}.", places.join("; "));
        text += " Earlier inspect_model coordinates of moved components are out of date (their ids \
                 still work).";
    }
    if let Some(dim) = &mate.dimension
        && !dim.name.is_empty()
    {
        if mate.limits.is_some() {
            text += &format!(
                " Its value is where it is within the limits: set_dimension name=\"{}\" moves it \
                 there, sweep_mate mate=\"{mate_id}\" runs it through the range checking for \
                 collisions (move_component keeps a limit mate at its value).",
                dim.name
            );
        } else {
            text += &format!(" Change its value later with set_dimension name=\"{}\".", dim.name);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> Option<proto::Vec3> {
        Some(proto::Vec3 { x, y, z })
    }

    /// A component moved by (0.1, 0, 0) m and turned 90 degrees about Z:
    /// its X axis points along assembly +Y, its Y along -X.
    fn turned(local: bool) -> proto::ComponentInfo {
        proto::ComponentInfo {
            persist_id: b"c1".to_vec(),
            name: "bracket-1".into(),
            origin: v(0.1, 0.0, 0.0),
            x_axis: v(0.0, 1.0, 0.0),
            y_axis: v(-1.0, 0.0, 0.0),
            z_axis: v(0.0, 0.0, 1.0),
            local_geometry: local,
            ..Default::default()
        }
    }

    fn close(a: &proto::Vec3, b: [f64; 3]) -> bool {
        (a.x - b[0]).abs() < 1e-12 && (a.y - b[1]).abs() < 1e-12 && (a.z - b[2]).abs() < 1e-12
    }

    #[test]
    fn places_faces_edges_and_bodies() {
        let mut reply = proto::InspectModelResponse {
            components: vec![turned(true)],
            faces: vec![proto::FaceInfo {
                component_persist_id: b"c1".to_vec(),
                normal: v(1.0, 0.0, 0.0),
                origin: v(0.01, 0.0, 0.0),
                r#box: Some(proto::Box3 { min: v(0.0, 0.0, 0.0), max: v(0.02, 0.01, 0.005) }),
                ..Default::default()
            }],
            edges: vec![proto::EdgeInfo {
                component_persist_id: b"c1".to_vec(),
                start: v(0.0, 0.01, 0.0),
                axis: v(0.0, 1.0, 0.0),
                ..Default::default()
            }],
            bodies: vec![proto::BodyInfo {
                component_persist_id: b"other".to_vec(),
                r#box: Some(proto::Box3 { min: v(0.0, 0.0, 0.0), max: v(1.0, 1.0, 1.0) }),
                ..Default::default()
            }],
            ..Default::default()
        };
        place_inspect_reply(&mut reply);
        let f = &reply.faces[0];
        assert!(close(f.normal.as_ref().unwrap(), [0.0, 1.0, 0.0]));
        assert!(close(f.origin.as_ref().unwrap(), [0.1, 0.01, 0.0]));
        let b = f.r#box.as_ref().unwrap();
        assert!(close(b.min.as_ref().unwrap(), [0.09, 0.0, 0.0]));
        assert!(close(b.max.as_ref().unwrap(), [0.1, 0.02, 0.005]));
        let e = &reply.edges[0];
        assert!(close(e.start.as_ref().unwrap(), [0.09, 0.0, 0.0]));
        assert!(close(e.axis.as_ref().unwrap(), [-1.0, 0.0, 0.0]));
        // Not one of the listed components: untouched.
        let b = reply.bodies[0].r#box.as_ref().unwrap();
        assert!(close(b.max.as_ref().unwrap(), [1.0, 1.0, 1.0]));
    }

    #[test]
    fn leaves_assembly_space_geometry_alone() {
        let mut reply = proto::InspectModelResponse {
            components: vec![turned(false)],
            faces: vec![proto::FaceInfo {
                component_persist_id: b"c1".to_vec(),
                normal: v(1.0, 0.0, 0.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        place_inspect_reply(&mut reply);
        assert!(close(reply.faces[0].normal.as_ref().unwrap(), [1.0, 0.0, 0.0]));
    }

    #[test]
    fn places_mesh() {
        let mut mesh = proto::GetMeshResponse {
            components: vec![turned(true)],
            faces: vec![proto::FaceMesh {
                component_persist_id: b"c1".to_vec(),
                triangles: vec![0.0, 0.0, 0.0, 0.01, 0.0, 0.0, 0.0, 0.01, 0.0],
                normals: vec![0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
                ..Default::default()
            }],
            edges: vec![proto::EdgePolyline {
                component_persist_id: b"c1".to_vec(),
                points: vec![0.01, 0.0, 0.0, 0.02, 0.0, 0.0],
                ..Default::default()
            }],
            ..Default::default()
        };
        place_mesh(&mut mesh);
        let t = &mesh.faces[0].triangles;
        assert!((t[3] - 0.1).abs() < 1e-6 && (t[4] - 0.01).abs() < 1e-6);
        assert!((t[6] - 0.09).abs() < 1e-6 && t[7].abs() < 1e-6);
        let n = &mesh.faces[0].normals;
        assert!(n[3].abs() < 1e-6 && (n[4] - 1.0).abs() < 1e-6);
        assert!((mesh.edges[0].points[4] - 0.02).abs() < 1e-6);
    }

    #[test]
    fn describes_components_and_mates() {
        let mut ids = IdRegistry::default();
        let mut c = turned(true);
        c.file = r"C:\models\bracket.SLDPRT".into();
        c.fixed = false;
        let json = component_json(&mut ids, &c);
        assert_eq!(json["id"], "component:1");
        assert_eq!(json["file"], "bracket.SLDPRT");
        assert_eq!(json["origin_mm"], json!([100.0, 0.0, 0.0]));
        assert_eq!(json["axes"]["x"], json!([0.0, 1.0, 0.0]));

        // Square to the assembly: no axes.
        let square = proto::ComponentInfo {
            persist_id: b"c2".to_vec(),
            origin: v(0.0, 0.0, 0.05),
            x_axis: v(1.0, 0.0, 0.0),
            y_axis: v(0.0, 1.0, 0.0),
            z_axis: v(0.0, 0.0, 1.0),
            ..Default::default()
        };
        assert!(component_json(&mut ids, &square).get("axes").is_none());

        let mate = proto::MateInfo {
            persist_id: b"m1".to_vec(),
            name: "Distance1".into(),
            r#type: proto::MateType::Distance as i32,
            alignment: proto::MateAlignment::AntiAligned as i32,
            component_persist_ids: vec![b"c1".to_vec(), Vec::new()],
            component_names: vec!["bracket-1".into(), String::new()],
            dimension: Some(proto::DimensionInfo {
                name: "D1@Distance1".into(),
                r#type: proto::DimensionType::Linear as i32,
                value: 0.0125,
                driven: false,
            }),
            error_code: 46,
            ..Default::default()
        };
        let json = mate_json(&mut ids, &mate);
        assert_eq!(json["type"], "distance");
        assert_eq!(json["alignment"], "anti_aligned");
        assert_eq!(json["distance_mm"], 12.5);
        assert_eq!(json["components"], json!(["bracket-1 (component:1)", "(assembly)"]));
        assert!(json["error"].as_str().unwrap().contains("over-defined"));
    }

    fn params(kind: MateKind, entities: &[&str]) -> AddMateParams {
        AddMateParams {
            kind,
            entities: entities.iter().map(|e| e.to_string()).collect(),
            alignment: None,
            distance_mm: None,
            angle_deg: None,
            min_mm: None,
            max_mm: None,
            min_deg: None,
            max_deg: None,
            flip: false,
            lock_rotation: false,
        }
    }

    #[test]
    fn checks_limit_mates() {
        let mut p = params(MateKind::Distance, &["face:1", "face:2"]);
        p.min_mm = Some(0.0);
        assert!(check(&p).is_err(), "max_mm missing");
        p.max_mm = Some(40.0);
        assert!(check(&p).is_ok(), "no distance_mm: starts at min_mm");
        let r = request(&p, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(r.distance, 0.0);
        let limits = r.limits.unwrap();
        assert!((limits.min - 0.0).abs() < 1e-15 && (limits.max - 0.04).abs() < 1e-15);
        p.distance_mm = Some(50.0);
        assert!(check(&p).is_err(), "start outside the limits");
        p.distance_mm = Some(25.0);
        assert!((request(&p, [b"a".to_vec(), b"b".to_vec()]).distance - 0.025).abs() < 1e-15);
        p.min_mm = Some(45.0);
        assert!(check(&p).is_err(), "min above max");
        p.min_mm = Some(-1.0);
        assert!(check(&p).is_err(), "negative");

        let mut p = params(MateKind::Angle, &["face:1", "face:2"]);
        p.min_deg = Some(10.0);
        p.max_deg = Some(200.0);
        assert!(check(&p).is_err(), "past 180");
        p.max_deg = Some(110.0);
        let r = request(&p, [b"a".to_vec(), b"b".to_vec()]);
        assert!((r.angle - 10f64.to_radians()).abs() < 1e-15);
        assert!((r.limits.unwrap().max - 110f64.to_radians()).abs() < 1e-15);
        p.min_mm = Some(0.0);
        p.max_mm = Some(1.0);
        assert!(check(&p).is_err(), "mm limits on an angle mate");

        let mut p = params(MateKind::Coincident, &["face:1", "face:2"]);
        p.min_deg = Some(0.0);
        p.max_deg = Some(10.0);
        assert!(check(&p).is_err());
        assert!(request(&params(MateKind::Coincident, &["face:1", "face:2"]), [vec![], vec![]]).limits.is_none());
    }

    #[test]
    fn describes_limit_mates() {
        let mut ids = IdRegistry::default();
        let mate = proto::MateInfo {
            persist_id: b"m".to_vec(),
            name: "LimitAngle1".into(),
            r#type: proto::MateType::Angle.into(),
            limits: Some(proto::MateLimits { min: 0.0, max: std::f64::consts::FRAC_PI_4 }),
            ..Default::default()
        };
        let json = mate_json(&mut ids, &mate);
        assert_eq!(json["min_deg"], 0.0);
        assert_eq!(json["max_deg"], 45.0);
        assert!(json.get("min_mm").is_none());
        assert_eq!(limits_text(&mate), ", limits 0 .. 45 deg");
    }

    #[test]
    fn checks_mate_params() {
        assert!(check(&params(MateKind::Coincident, &["face:1", " feature:2 "])).is_ok());
        assert!(check(&params(MateKind::Coincident, &["face:1"])).is_err());
        assert!(check(&params(MateKind::Coincident, &["face:1", "face:1"])).is_err());
        assert!(check(&params(MateKind::Coincident, &["face:1", "component:1"])).is_err());
        assert!(check(&params(MateKind::Coincident, &["face:1", "line:1"])).is_err());
        assert!(check(&params(MateKind::Lock, &["component:1", "component:2"])).is_ok());
        assert!(check(&params(MateKind::Lock, &["face:1", "component:2"])).is_err());

        let mut p = params(MateKind::Distance, &["face:1", "face:2"]);
        assert!(check(&p).is_err());
        p.distance_mm = Some(5.0);
        assert!(check(&p).is_ok());
        p.distance_mm = Some(-1.0);
        assert!(check(&p).is_err());

        let mut p = params(MateKind::Coincident, &["face:1", "face:2"]);
        p.distance_mm = Some(5.0);
        assert!(check(&p).is_err());
        let mut p = params(MateKind::Angle, &["face:1", "face:2"]);
        p.angle_deg = Some(200.0);
        assert!(check(&p).is_err());
        p.angle_deg = Some(90.0);
        let r = request(&p, [b"a".to_vec(), b"b".to_vec()]);
        assert!((r.angle - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert_eq!(r.alignment(), proto::MateAlignment::Closest);

        let mut p = params(MateKind::Parallel, &["face:1", "face:2"]);
        p.lock_rotation = true;
        assert!(check(&p).is_err());
    }
}
