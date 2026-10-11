//! Turns InspectModel replies into compact JSON for the model, and keeps the
//! short IDs ("face:12") it hands out in place of SolidWorks persist IDs.
//!
//! Persist IDs are opaque byte strings, often a few hundred bytes each --
//! too long to put in front of the model for every face. Each one gets a
//! short ID the first time it's seen, and the same bytes always map back to
//! the same short ID for the life of this process. Tools that take an ID
//! translate it back to the bytes before calling the add-in, which then
//! resolves them against the current model.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::addin::proto;

/// SolidWorks API lengths are in meters; tools take and report millimeters.
pub const MM_PER_M: f64 = 1000.0;

/// What an ID refers to; only used as its readable prefix.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Feature,
    Body,
    Face,
    Edge,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Feature => "feature",
            Kind::Body => "body",
            Kind::Face => "face",
            Kind::Edge => "edge",
        }
    }
}

#[derive(Default)]
pub struct IdRegistry {
    by_bytes: HashMap<Vec<u8>, String>,
    by_id: HashMap<String, Vec<u8>>,
    next: HashMap<&'static str, u32>,
}

impl IdRegistry {
    /// The short ID for a persist ID, minting one if it's new. None for an
    /// empty persist ID (SolidWorks couldn't reference the entity).
    pub fn id_for(&mut self, kind: Kind, persist_id: &[u8]) -> Option<String> {
        if persist_id.is_empty() {
            return None;
        }
        if let Some(id) = self.by_bytes.get(persist_id) {
            return Some(id.clone());
        }
        let counter = self.next.entry(kind.prefix()).or_insert(0);
        *counter += 1;
        let id = format!("{}:{}", kind.prefix(), counter);
        self.by_bytes.insert(persist_id.to_vec(), id.clone());
        self.by_id.insert(id.clone(), persist_id.to_vec());
        Some(id)
    }

    /// The persist ID behind a short ID handed out earlier.
    pub fn persist_id(&self, id: &str) -> Option<Vec<u8>> {
        self.by_id.get(id.trim()).cloned()
    }

    /// Whether `s` looks like one of our IDs (as opposed to, say, a feature
    /// name), whether or not it's known.
    pub fn looks_like_id(s: &str) -> bool {
        let s = s.trim();
        [Kind::Feature, Kind::Body, Kind::Face, Kind::Edge]
            .iter()
            .any(|kind| {
                s.strip_prefix(kind.prefix())
                    .and_then(|rest| rest.strip_prefix(':'))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
    }
}

// ---- Filters ---------------------------------------------------------------

/// Face/edge filters applied here rather than in the add-in, which stays a
/// thin layer over the SolidWorks API.
#[derive(Debug, Default)]
pub struct Filters {
    pub surface_type: Option<proto::SurfaceType>,
    /// Planar faces whose outward normal points this way (any length).
    pub normal: Option<[f64; 3]>,
    pub curve_type: Option<proto::CurveType>,
    pub offset: usize,
    pub limit: usize,
}

/// cos(1 degree): how closely a face normal must match the requested one.
const NORMAL_TOLERANCE_COS: f64 = 0.999_847_7;

impl Filters {
    fn keep_face(&self, face: &proto::FaceInfo) -> bool {
        if let Some(wanted) = self.surface_type
            && face.surface_type() != wanted
        {
            return false;
        }
        if let Some(wanted) = self.normal {
            if face.surface_type() != proto::SurfaceType::Plane {
                return false;
            }
            let Some(n) = face.normal.as_ref() else {
                return false;
            };
            let n = [n.x, n.y, n.z];
            let (nl, wl) = (norm(n), norm(wanted));
            if nl == 0.0 || wl == 0.0 || dot(n, wanted) / (nl * wl) < NORMAL_TOLERANCE_COS {
                return false;
            }
        }
        true
    }

    fn keep_edge(&self, edge: &proto::EdgeInfo) -> bool {
        self.curve_type.is_none_or(|wanted| edge.curve_type() == wanted)
    }
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

// ---- Number formatting -----------------------------------------------------

/// Rounds to `places` decimals and turns -0 into 0, so the JSON stays short.
fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    let r = (value * scale).round() / scale;
    if r == 0.0 { 0.0 } else { r }
}

/// Meters -> millimeters, to 0.1 micron.
fn mm(m: f64) -> f64 {
    round(m * MM_PER_M, 4)
}

fn mm2(m2: f64) -> f64 {
    round(m2 * MM_PER_M * MM_PER_M, 4)
}

fn mm3(m3: f64) -> f64 {
    round(m3 * MM_PER_M * MM_PER_M * MM_PER_M, 4)
}

/// A point in meters as [x, y, z] in millimeters.
pub fn point_mm(v: &proto::Vec3) -> Value {
    json!([mm(v.x), mm(v.y), mm(v.z)])
}

/// A unitless direction.
pub fn direction(v: &proto::Vec3) -> Value {
    json!([round(v.x, 6), round(v.y, 6), round(v.z, 6)])
}

fn box_mm(b: &proto::Box3) -> Option<Value> {
    Some(json!({ "min": point_mm(b.min.as_ref()?), "max": point_mm(b.max.as_ref()?) }))
}

fn surface_name(t: proto::SurfaceType) -> &'static str {
    use proto::SurfaceType as S;
    match t {
        S::Plane => "plane",
        S::Cylinder => "cylinder",
        S::Cone => "cone",
        S::Sphere => "sphere",
        S::Torus => "torus",
        S::Bspline => "bspline",
        S::Blend => "blend",
        S::Offset => "offset",
        S::Extrusion => "extrusion",
        S::Revolution => "revolution",
        S::Unspecified => "other",
    }
}

fn curve_name(t: proto::CurveType) -> &'static str {
    use proto::CurveType as C;
    match t {
        C::Line => "line",
        C::Circle => "circle",
        C::Ellipse => "ellipse",
        C::Intersection => "intersection",
        C::Bspline => "bspline",
        C::Spcurve => "spcurve",
        C::Constparam => "constparam",
        C::Trimmed => "trimmed",
        C::Unspecified => "other",
    }
}

// ---- Rendering -------------------------------------------------------------

fn insert_id(obj: &mut Map<String, Value>, key: &str, id: Option<String>) {
    obj.insert(key.to_string(), id.map_or(Value::Null, Value::String));
}

fn feature_json(ids: &mut IdRegistry, f: &proto::FeatureInfo) -> Value {
    let mut obj = Map::new();
    insert_id(&mut obj, "id", ids.id_for(Kind::Feature, &f.persist_id));
    obj.insert("name".into(), json!(f.name));
    obj.insert("type".into(), json!(f.type_name));
    if f.suppressed {
        obj.insert("suppressed".into(), json!(true));
    }
    if !f.parents.is_empty() {
        obj.insert("parents".into(), json!(f.parents));
    }
    if !f.children.is_empty() {
        obj.insert("children".into(), json!(f.children));
    }
    Value::Object(obj)
}

fn body_json(ids: &mut IdRegistry, b: &proto::BodyInfo) -> Value {
    let mut obj = Map::new();
    insert_id(&mut obj, "id", ids.id_for(Kind::Body, &b.persist_id));
    obj.insert("name".into(), json!(b.name));
    obj.insert("solid".into(), json!(b.solid));
    if let Some(bbox) = b.r#box.as_ref().and_then(box_mm) {
        obj.insert("bbox_mm".into(), bbox);
    }
    if let Some(mass) = &b.mass {
        obj.insert("volume_mm3".into(), json!(mm3(mass.volume)));
        obj.insert("area_mm2".into(), json!(mm2(mass.surface_area)));
        if let Some(c) = &mass.center_of_mass {
            obj.insert("centroid_mm".into(), point_mm(c));
        }
    }
    obj.insert("faces".into(), json!(b.face_count));
    obj.insert("edges".into(), json!(b.edge_count));
    Value::Object(obj)
}

fn face_json(ids: &mut IdRegistry, f: &proto::FaceInfo) -> Value {
    use proto::SurfaceType as S;
    let mut obj = Map::new();
    insert_id(&mut obj, "id", ids.id_for(Kind::Face, &f.persist_id));
    insert_id(&mut obj, "body", ids.id_for(Kind::Body, &f.body_persist_id));
    if !f.feature_name.is_empty() {
        obj.insert("feature".into(), json!(f.feature_name));
    }
    let surface = f.surface_type();
    obj.insert("type".into(), json!(surface_name(surface)));
    obj.insert("area_mm2".into(), json!(mm2(f.area)));
    match surface {
        S::Plane => {
            if let Some(n) = &f.normal {
                obj.insert("normal".into(), direction(n));
            }
            if let Some(o) = &f.origin {
                obj.insert("point_on_plane_mm".into(), point_mm(o));
            }
        }
        S::Cylinder | S::Cone | S::Torus => {
            if let Some(a) = &f.axis {
                obj.insert("axis".into(), direction(a));
            }
            if let Some(o) = &f.origin {
                let key = if surface == S::Torus { "center_mm" } else { "axis_point_mm" };
                obj.insert(key.into(), point_mm(o));
            }
            obj.insert("radius_mm".into(), json!(mm(f.radius)));
            if surface == S::Torus {
                obj.insert("minor_radius_mm".into(), json!(mm(f.minor_radius)));
            }
            if surface == S::Cone {
                obj.insert("half_angle_deg".into(), json!(round(f.half_angle.to_degrees(), 4)));
            }
            if matches!(surface, S::Cylinder | S::Cone)
                && let Some(hole) = points_at_axis(f)
            {
                obj.insert("hole".into(), json!(hole));
            }
        }
        S::Sphere => {
            if let Some(o) = &f.origin {
                obj.insert("center_mm".into(), point_mm(o));
            }
            obj.insert("radius_mm".into(), json!(mm(f.radius)));
            if let Some(concave) = points_at_center(f) {
                obj.insert("concave".into(), json!(concave));
            }
        }
        _ => {
            // Freeform faces: give the model a point and outward direction.
            if let (Some(p), Some(n)) = (&f.sample_point, &f.sample_normal) {
                obj.insert("sample_point_mm".into(), point_mm(p));
                obj.insert("outward_normal".into(), direction(n));
            }
        }
    }
    if let Some(bbox) = f.r#box.as_ref().and_then(box_mm) {
        obj.insert("bbox_mm".into(), bbox);
    }
    obj.insert("edges".into(), json!(f.edge_count));
    Value::Object(obj)
}

fn xyz(v: &proto::Vec3) -> [f64; 3] {
    [v.x, v.y, v.z]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// For a cylinder or cone: whether the face's outward normal points toward
/// the axis, i.e. the material is around the face (a hole) rather than
/// inside it (a boss). None without a sample, or if it's degenerate.
fn points_at_axis(f: &proto::FaceInfo) -> Option<bool> {
    let (p, n) = (xyz(f.sample_point.as_ref()?), xyz(f.sample_normal.as_ref()?));
    let (o, a) = (xyz(f.origin.as_ref()?), xyz(f.axis.as_ref()?));
    let a_len = norm(a);
    if a_len == 0.0 {
        return None;
    }
    let a = [a[0] / a_len, a[1] / a_len, a[2] / a_len];
    // Component of (p - o) perpendicular to the axis: from the axis out to p.
    let op = sub(p, o);
    let along = dot(op, a);
    let radial = sub(op, [a[0] * along, a[1] * along, a[2] * along]);
    let d = dot(n, radial);
    if norm(radial) == 0.0 || d == 0.0 {
        return None;
    }
    Some(d < 0.0)
}

/// For a sphere: whether the outward normal points at the center (a
/// spherical pocket rather than a ball).
fn points_at_center(f: &proto::FaceInfo) -> Option<bool> {
    let (p, n) = (xyz(f.sample_point.as_ref()?), xyz(f.sample_normal.as_ref()?));
    let d = dot(n, sub(p, xyz(f.origin.as_ref()?)));
    if d == 0.0 {
        return None;
    }
    Some(d < 0.0)
}

fn edge_json(ids: &mut IdRegistry, e: &proto::EdgeInfo) -> Value {
    let mut obj = Map::new();
    insert_id(&mut obj, "id", ids.id_for(Kind::Edge, &e.persist_id));
    insert_id(&mut obj, "body", ids.id_for(Kind::Body, &e.body_persist_id));
    let curve = e.curve_type();
    obj.insert("type".into(), json!(curve_name(curve)));
    obj.insert("length_mm".into(), json!(mm(e.length)));
    if let Some(p) = &e.start {
        obj.insert("start_mm".into(), point_mm(p));
    }
    if let Some(p) = &e.end {
        obj.insert("end_mm".into(), point_mm(p));
    }
    if curve == proto::CurveType::Circle {
        if let Some(c) = &e.center {
            obj.insert("center_mm".into(), point_mm(c));
        }
        if let Some(a) = &e.axis {
            obj.insert("axis".into(), direction(a));
        }
        obj.insert("radius_mm".into(), json!(mm(e.radius)));
    }
    let faces: Vec<Value> = e
        .face_persist_ids
        .iter()
        .filter_map(|f| ids.id_for(Kind::Face, f))
        .map(Value::String)
        .collect();
    obj.insert("faces".into(), Value::Array(faces));
    Value::Object(obj)
}

/// Applies offset/limit to the items that pass `keep`, rendering only those
/// shown, and records the counts so the model knows whether to page on.
fn page<T>(
    out: &mut Map<String, Value>,
    key: &str,
    items: &[T],
    keep: impl Fn(&T) -> bool,
    filters: &Filters,
    mut render: impl FnMut(&T) -> Value,
) {
    let matching: Vec<&T> = items.iter().filter(|item| keep(item)).collect();
    let shown: Vec<Value> = matching
        .iter()
        .skip(filters.offset)
        .take(filters.limit)
        .map(|item| render(item))
        .collect();
    let end = filters.offset + shown.len();
    if end < matching.len() {
        out.insert(
            format!("{key}_note"),
            json!(format!(
                "showing {key} {}-{} of {}; pass offset={} for more",
                filters.offset + 1,
                end,
                matching.len(),
                end
            )),
        );
    }
    out.insert(format!("{key}_matching"), json!(matching.len()));
    out.insert(key.to_string(), Value::Array(shown));
}

/// The JSON for one InspectModel reply.
pub fn render(ids: &mut IdRegistry, reply: &proto::InspectModelResponse, filters: &Filters) -> Value {
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }
    if !reply.features.is_empty() {
        let features = reply.features.iter().map(|f| feature_json(ids, f)).collect();
        out.insert("features".into(), Value::Array(features));
    }
    if !reply.bodies.is_empty() {
        let bodies = reply.bodies.iter().map(|b| body_json(ids, b)).collect();
        out.insert("bodies".into(), Value::Array(bodies));
    }
    if !reply.faces.is_empty() {
        page(&mut out, "faces", &reply.faces, |f| filters.keep_face(f), filters, |f| face_json(ids, f));
    }
    if !reply.edges.is_empty() {
        page(&mut out, "edges", &reply.edges, |e| filters.keep_edge(e), filters, |e| edge_json(ids, e));
    }
    if reply.truncated {
        out.insert(
            "warning".into(),
            json!("the part has more faces/edges than the add-in reports at once; narrow `within` to see the rest"),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> Option<proto::Vec3> {
        Some(proto::Vec3 { x, y, z })
    }

    #[test]
    fn ids_are_stable_and_reversible() {
        let mut ids = IdRegistry::default();
        let a = ids.id_for(Kind::Face, b"aaa").unwrap();
        let b = ids.id_for(Kind::Face, b"bbb").unwrap();
        let e = ids.id_for(Kind::Edge, b"ccc").unwrap();
        assert_eq!((a.as_str(), b.as_str(), e.as_str()), ("face:1", "face:2", "edge:1"));
        assert_eq!(ids.id_for(Kind::Face, b"aaa").unwrap(), "face:1");
        assert_eq!(ids.persist_id(" face:2 ").unwrap(), b"bbb");
        assert!(ids.persist_id("face:9").is_none());
        assert!(ids.id_for(Kind::Face, b"").is_none());
    }

    #[test]
    fn recognizes_id_syntax() {
        assert!(IdRegistry::looks_like_id("face:12"));
        assert!(IdRegistry::looks_like_id(" body:1"));
        assert!(!IdRegistry::looks_like_id("Boss-Extrude1"));
        assert!(!IdRegistry::looks_like_id("face:"));
        assert!(!IdRegistry::looks_like_id("face:x"));
    }

    #[test]
    fn filters_faces_by_normal_and_pages() {
        let plane = |n: Option<proto::Vec3>, id: &[u8]| proto::FaceInfo {
            persist_id: id.to_vec(),
            surface_type: proto::SurfaceType::Plane as i32,
            normal: n,
            area: 1e-4,
            ..Default::default()
        };
        let reply = proto::InspectModelResponse {
            faces: vec![
                plane(v(0.0, 0.0, 1.0), b"top"),
                plane(v(0.0, 0.0, -1.0), b"bottom"),
                plane(v(0.0, 0.0, 1.0), b"step"),
                // A hole along Z through (1, 2): outward normal points at the axis.
                // `reversed` is deliberately the misleading value.
                proto::FaceInfo {
                    persist_id: b"cyl".to_vec(),
                    surface_type: proto::SurfaceType::Cylinder as i32,
                    radius: 0.005,
                    origin: v(0.001, 0.002, 0.0),
                    axis: v(0.0, 0.0, 3.0),
                    sample_point: v(0.006, 0.002, 0.004),
                    sample_normal: v(-1.0, 0.0, 0.0),
                    reversed: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut ids = IdRegistry::default();
        let filters = Filters { normal: Some([0.0, 0.0, 2.0]), limit: 1, ..Default::default() };
        let out = render(&mut ids, &reply, &filters);
        assert_eq!(out["faces_matching"], 2);
        assert_eq!(out["faces"].as_array().unwrap().len(), 1);
        assert_eq!(out["faces"][0]["area_mm2"], 100.0);
        assert!(out["faces_note"].as_str().unwrap().contains("offset=1"));

        let filters = Filters {
            surface_type: Some(proto::SurfaceType::Cylinder),
            limit: 10,
            ..Default::default()
        };
        let out = render(&mut ids, &reply, &filters);
        assert_eq!(out["faces"][0]["radius_mm"], 5.0);
        assert_eq!(out["faces"][0]["hole"], true);
        assert!(out.get("faces_note").is_none());
    }

    #[test]
    fn tells_holes_from_bosses() {
        let cyl = |point: Option<proto::Vec3>, normal: Option<proto::Vec3>| proto::FaceInfo {
            surface_type: proto::SurfaceType::Cylinder as i32,
            origin: v(0.0, 0.0, 0.0),
            axis: v(1.0, 0.0, 0.0),
            sample_point: point,
            sample_normal: normal,
            ..Default::default()
        };
        // Point above the X axis; normal down (toward it) = hole, up = boss.
        assert_eq!(points_at_axis(&cyl(v(5.0, 0.0, 2.0), v(0.0, 0.0, -1.0))), Some(true));
        assert_eq!(points_at_axis(&cyl(v(5.0, 0.0, 2.0), v(0.0, 0.0, 1.0))), Some(false));
        // No tessellation sample: unknown, so the flag is left out.
        assert_eq!(points_at_axis(&cyl(None, None)), None);
        let mut ids = IdRegistry::default();
        let reply = proto::InspectModelResponse { faces: vec![cyl(None, None)], ..Default::default() };
        let out = render(&mut ids, &reply, &Filters { limit: 10, ..Default::default() });
        assert!(out["faces"][0].get("hole").is_none());

        let ball = |n| proto::FaceInfo {
            surface_type: proto::SurfaceType::Sphere as i32,
            origin: v(0.0, 0.0, 0.0),
            sample_point: v(0.0, 0.0, 1.0),
            sample_normal: n,
            ..Default::default()
        };
        assert_eq!(points_at_center(&ball(v(0.0, 0.0, -1.0))), Some(true));
        assert_eq!(points_at_center(&ball(v(0.0, 0.0, 1.0))), Some(false));
    }

    #[test]
    fn rounds_and_converts() {
        assert_eq!(mm(0.0123456789), 12.3457);
        assert_eq!(round(-0.00000001, 4), 0.0);
        assert!(round(-0.00000001, 4).is_sign_positive());
    }
}
