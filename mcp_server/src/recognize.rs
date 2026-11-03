//! recognize_features: what a body is made of, read from inspect_model's
//! faces and edges -- holes (simple, counterbore, countersink; through or
//! blind), cylindrical bosses, fillets and rounds, chamfers, and the planar
//! faces by direction. Meant for imported bodies, which have no feature
//! history, but works on any. All geometry is worked out here; the add-in
//! only lists faces and edges (docs/notes/files.md, "Importing").

use std::collections::{BTreeMap, HashMap};

use rmcp::schemars;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{self, IdRegistry, Kind, dot, mm, norm, round, sub, xyz};
use proto::SurfaceType as S;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RecognizeFeaturesParams {
    /// Look at one body, or in an assembly one component: an id from inspect_model
    /// ("body:1", "component:2"), or (parts only) a feature name such as "Imported1" for the
    /// faces it made. Default: every body of the active part (every component in an assembly).
    #[serde(default)]
    pub within: Option<String>,
}

/// Lengths closer than this (m) are the same: imported geometry is only
/// approximately where it was (an IGES round trip moved a volume by 0.04 mm^3).
const TOL: f64 = 1e-5;
/// cos(0.5 degrees): directions this close are the same.
const PARALLEL_COS: f64 = 0.999_962;
/// A round covering at least this much of a turn is a full one.
const FULL_TURN: f64 = std::f64::consts::TAU * 0.99;
/// Partial cylinders up to this angle are fillets / rounds; wider ones are
/// rounded ends (a slot's end is half a turn).
const FILLET_MAX_SPAN: f64 = 179.0 * std::f64::consts::PI / 180.0;
/// Faces listed as unrecognized at most.
const MAX_OTHER: usize = 50;

type V = [f64; 3];

fn scale(v: V, s: f64) -> V {
    [v[0] * s, v[1] * s, v[2] * s]
}

fn add(a: V, b: V) -> V {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn cross(a: V, b: V) -> V {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn unit(v: V) -> Option<V> {
    let n = norm(v);
    (n > 0.0 && n.is_finite()).then(|| scale(v, 1.0 / n))
}

fn parallel(a: V, b: V) -> bool {
    dot(a, b).abs() >= PARALLEL_COS
}

/// Angle between two unit vectors, radians.
fn angle(a: V, b: V) -> f64 {
    dot(a, b).clamp(-1.0, 1.0).acos()
}

#[derive(Debug, Clone, Copy)]
struct Axis {
    o: V,
    a: V,
}

impl Axis {
    fn of(f: &proto::FaceInfo) -> Option<Axis> {
        Some(Axis { o: xyz(f.origin.as_ref()?), a: unit(xyz(f.axis.as_ref()?))? })
    }

    fn t(&self, p: V) -> f64 {
        dot(sub(p, self.o), self.a)
    }

    fn point(&self, t: f64) -> V {
        add(self.o, scale(self.a, t))
    }

    fn distance(&self, p: V) -> f64 {
        norm(sub(sub(p, self.o), scale(self.a, self.t(p))))
    }

    fn same_line(&self, other: &Axis) -> bool {
        parallel(self.a, other.a) && self.distance(other.o) < TOL
    }

    /// The span of an edge along the axis: a circle's center, else its ends.
    fn edge_t(&self, e: &proto::EdgeInfo) -> Option<(f64, f64)> {
        if e.curve_type() == proto::CurveType::Circle
            && let Some(c) = &e.center
        {
            let t = self.t(xyz(c));
            return Some((t, t));
        }
        let (s, end) = (self.t(xyz(e.start.as_ref()?)), self.t(xyz(e.end.as_ref()?)));
        Some((s.min(end), s.max(end)))
    }
}

/// Faces and edges, and which meet where. Keys are (component, persist id):
/// two instances of a part share persist ids.
struct Topology<'a> {
    faces: &'a [proto::FaceInfo],
    edges: &'a [proto::EdgeInfo],
    index: HashMap<(&'a [u8], &'a [u8]), usize>,
    face_edges: Vec<Vec<usize>>,
}

impl<'a> Topology<'a> {
    fn new(reply: &'a proto::InspectModelResponse) -> Self {
        let index: HashMap<_, _> = reply
            .faces
            .iter()
            .enumerate()
            .map(|(i, f)| ((f.component_persist_id.as_slice(), f.persist_id.as_slice()), i))
            .collect();
        let mut face_edges = vec![Vec::new(); reply.faces.len()];
        for (e, edge) in reply.edges.iter().enumerate() {
            for pid in &edge.face_persist_ids {
                if let Some(&f) = index.get(&(edge.component_persist_id.as_slice(), pid.as_slice())) {
                    face_edges[f].push(e);
                }
            }
        }
        Topology { faces: &reply.faces, edges: &reply.edges, index, face_edges }
    }

    /// The faces at an edge that are listed; None for one that isn't
    /// (outside `within`).
    fn edge_faces(&self, e: usize) -> impl Iterator<Item = Option<usize>> + '_ {
        let edge = &self.edges[e];
        edge.face_persist_ids
            .iter()
            .map(move |pid| self.index.get(&(edge.component_persist_id.as_slice(), pid.as_slice())).copied())
    }

    fn neighbours(&self, f: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self.face_edges[f]
            .iter()
            .flat_map(|&e| self.edge_faces(e).flatten())
            .filter(|&g| g != f)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn adjacent(&self, f: usize, g: usize) -> bool {
        self.face_edges[f].iter().any(|&e| self.edge_faces(e).any(|h| h == Some(g)))
    }

    fn body(&self, f: usize) -> (&[u8], &[u8]) {
        (&self.faces[f].component_persist_id, &self.faces[f].body_persist_id)
    }

    fn normal(&self, f: usize) -> Option<V> {
        unit(xyz(self.faces[f].normal.as_ref()?))
    }
}

struct UnionFind(Vec<usize>);

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind((0..n).collect())
    }

    fn find(&mut self, i: usize) -> usize {
        let parent = self.0[i];
        if parent == i {
            return i;
        }
        let root = self.find(parent);
        self.0[i] = root;
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        self.0[a] = b;
    }

    /// Members of each set, sets in order of their first member.
    fn sets(&mut self, members: &[usize]) -> Vec<Vec<usize>> {
        let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut order = Vec::new();
        for &m in members {
            let root = self.find(m);
            if !by_root.contains_key(&root) {
                order.push(root);
            }
            by_root.entry(root).or_default().push(m);
        }
        order.into_iter().map(|r| by_root.remove(&r).unwrap_or_default()).collect()
    }
}

/// A face as a cylinder or cone: the analytic ones as SolidWorks gives them,
/// and surfaces of revolution (how IGES brings them in) with a straight
/// profile.
#[derive(Debug, Clone, Copy)]
struct Revolved {
    cone: bool,
    axis: Axis,
    /// Radius where the axis point is (a cone's own radius parameter).
    radius: f64,
    half_angle: f64,
}

fn revolved(topo: &Topology, f: usize) -> Option<Revolved> {
    let face = &topo.faces[f];
    let axis = Axis::of(face)?;
    match face.surface_type() {
        S::Cylinder => Some(Revolved { cone: false, axis, radius: face.radius, half_angle: 0.0 }),
        S::Cone => Some(Revolved { cone: true, axis, radius: face.radius, half_angle: face.half_angle }),
        S::Revolution => fit_revolution(topo, f, axis),
        _ => None,
    }
}

/// A surface of revolution whose points -- its edges' ends and its sample
/// point, as (along the axis, from the axis) -- lie on one straight profile:
/// at one distance (a cylinder) or a distance changing evenly along the axis
/// (a cone). Anything else (a torus-like fillet, a spline) is None.
fn fit_revolution(topo: &Topology, f: usize, axis: Axis) -> Option<Revolved> {
    let mut points: Vec<(f64, f64)> = Vec::new();
    for &e in &topo.face_edges[f] {
        let edge = &topo.edges[e];
        for p in [&edge.start, &edge.end].into_iter().flatten() {
            points.push((axis.t(xyz(p)), axis.distance(xyz(p))));
        }
    }
    if let Some(p) = &topo.faces[f].sample_point {
        points.push((axis.t(xyz(p)), axis.distance(xyz(p))));
    }
    let low = points.iter().copied().min_by(|a, b| a.0.total_cmp(&b.0))?;
    let high = points.iter().copied().max_by(|a, b| a.0.total_cmp(&b.0))?;
    if high.0 - low.0 < TOL {
        return None;
    }
    let slope = (high.1 - low.1) / (high.0 - low.0);
    if points.iter().any(|&(t, r)| (r - (low.1 + slope * (t - low.0))).abs() > TOL) {
        return None;
    }
    let radius = low.1 - slope * low.0;
    if slope.abs() * (high.0 - low.0) < TOL {
        return Some(Revolved { cone: false, axis, radius: low.1, half_angle: 0.0 });
    }
    Some(Revolved { cone: true, axis, radius, half_angle: slope.abs().atan() })
}

/// Coaxial cylinder or cone faces of one surface, joined where they meet
/// (SolidWorks and STEP split a full cylinder into halves).
struct Round {
    cone: bool,
    faces: Vec<usize>,
    axis: Axis,
    radius: f64,
    half_angle: f64,
    t0: f64,
    t1: f64,
    /// Radii at the ends (from its circular edges; a cylinder's own radius).
    r_min: f64,
    r_max: f64,
    /// How much of a turn it covers, radians.
    span: f64,
    /// Material around it (a hole) rather than inside it; None if unknown.
    concave: Option<bool>,
}

impl Round {
    fn full(&self) -> bool {
        self.span >= FULL_TURN
    }

    fn length(&self) -> f64 {
        self.t1 - self.t0
    }
}

fn rounds(topo: &Topology) -> Vec<Round> {
    let faces = topo.faces;
    let shapes: Vec<Option<Revolved>> = (0..faces.len()).map(|f| revolved(topo, f)).collect();
    let candidates: Vec<usize> = (0..faces.len()).filter(|&f| shapes[f].is_some()).collect();
    let mut sets = UnionFind::new(faces.len());
    for (i, &f) in candidates.iter().enumerate() {
        for &g in &candidates[i + 1..] {
            let (Some(a), Some(b)) = (shapes[f], shapes[g]) else { continue };
            let same = a.cone == b.cone
                && topo.body(f) == topo.body(g)
                && (a.radius - b.radius).abs() < TOL
                && (a.half_angle - b.half_angle).abs() < 1e-3
                && a.axis.same_line(&b.axis);
            if same && topo.adjacent(f, g) {
                sets.union(f, g);
            }
        }
    }
    sets.sets(&candidates)
        .into_iter()
        .filter_map(|members| {
            let shape = shapes[members[0]]?;
            let (axis, cone) = (shape.axis, shape.cone);
            let (mut t0, mut t1) = (f64::MAX, f64::MIN);
            let (mut r_min, mut r_max) = (f64::MAX, f64::MIN);
            for &f in &members {
                for &e in &topo.face_edges[f] {
                    let edge = &topo.edges[e];
                    if let Some((a, b)) = axis.edge_t(edge) {
                        t0 = t0.min(a);
                        t1 = t1.max(b);
                    }
                    // Its rims: circles, or in IGES imports intersection curves.
                    for p in [&edge.start, &edge.end].into_iter().flatten() {
                        let r = axis.distance(xyz(p));
                        r_min = r_min.min(r);
                        r_max = r_max.max(r);
                    }
                }
            }
            if !cone || r_min > r_max {
                (r_min, r_max) = (shape.radius, shape.radius);
            }
            // A cone with one rim (a drill point) runs to its apex, which has
            // no edge: on the side its sample point is.
            if cone
                && t1 - t0 < TOL
                && shape.half_angle.tan() > 0.0
                && let Some(p) = &faces[members[0]].sample_point
            {
                let height = r_max / shape.half_angle.tan();
                if axis.t(xyz(p)) > t0 { t1 = t0 + height } else { t0 -= height }
                r_min = 0.0;
            }
            if t1 - t0 < TOL {
                return None;
            }
            let slant = (t1 - t0) / shape.half_angle.cos().max(1e-6);
            let mean_r = (r_min + r_max) / 2.0;
            let area: f64 = members.iter().map(|&f| faces[f].area).sum();
            let span = if mean_r > 0.0 { area / (mean_r * slant) } else { 0.0 };
            let concave = members.iter().find_map(|&f| inspect::points_at_axis(&faces[f]));
            Some(Round {
                cone,
                faces: members,
                axis,
                radius: shape.radius,
                half_angle: shape.half_angle,
                t0,
                t1,
                r_min,
                r_max,
                span,
                concave,
            })
        })
        .collect()
}

struct Hole {
    rounds: Vec<usize>,
    /// Planar faces inside it: a counterbore's shoulder, a blind hole's floor.
    planes: Vec<usize>,
}

/// Full concave rounds on one axis that meet, directly or across a planar
/// face lying wholly inside them, are one hole.
fn holes(topo: &Topology, rounds: &[Round]) -> Vec<Hole> {
    let candidates: Vec<usize> =
        (0..rounds.len()).filter(|&r| rounds[r].concave == Some(true) && rounds[r].full()).collect();
    let mut round_of: HashMap<usize, usize> = HashMap::new();
    for &r in &candidates {
        for &f in &rounds[r].faces {
            round_of.insert(f, r);
        }
    }
    let mut sets = UnionFind::new(rounds.len());
    for (i, &r) in candidates.iter().enumerate() {
        for &s in &candidates[i + 1..] {
            if rounds[r].axis.same_line(&rounds[s].axis)
                && rounds[r].faces.iter().any(|&f| rounds[s].faces.iter().any(|&g| topo.adjacent(f, g)))
            {
                sets.union(r, s);
            }
        }
    }
    let mut inner: Vec<(usize, usize)> = Vec::new(); // (plane face, a round it's inside)
    for f in 0..topo.faces.len() {
        if topo.faces[f].surface_type() != S::Plane {
            continue;
        }
        let Some(n) = topo.normal(f) else { continue };
        let around: Option<Vec<usize>> = topo.neighbours(f).iter().map(|g| round_of.get(g).copied()).collect();
        let Some(around) = around.filter(|a| !a.is_empty()) else { continue };
        // Every edge must be shared within the listing too (else it borders
        // something outside `within`).
        if topo.face_edges[f].iter().any(|&e| topo.edge_faces(e).any(|g| g.is_none())) {
            continue;
        }
        let axis = rounds[around[0]].axis;
        // No bigger than the hole's widest circle.
        let widest = around.iter().map(|&r| rounds[r].r_max).fold(0.0, f64::max);
        let fits = topo.faces[f].area <= std::f64::consts::PI * widest * widest * (1.0 + 1e-3);
        if fits && parallel(n, axis.a) && around.iter().all(|&r| rounds[r].axis.same_line(&axis)) {
            for &r in &around[1..] {
                sets.union(around[0], r);
            }
            inner.push((f, around[0]));
        }
    }
    sets.sets(&candidates)
        .into_iter()
        .map(|members| {
            let planes = inner
                .iter()
                .filter(|(_, r)| members.contains(r))
                .map(|&(f, _)| f)
                .collect();
            Hole { rounds: members, planes }
        })
        .collect()
}

/// How the rounds of a hole line up from its entry.
struct HoleShape {
    through: bool,
    entry: V,
    /// Into the material.
    direction: V,
    depth: f64,
    /// (round, from, to) in depth from the entry, in order.
    steps: Vec<(usize, f64, f64)>,
}

fn hole_shape(topo: &Topology, rounds: &[Round], hole: &Hole) -> Option<HoleShape> {
    let axis = rounds[hole.rounds[0]].axis;
    let along = |r: usize| {
        let o = axis.t(rounds[r].axis.point(rounds[r].t0));
        let p = axis.t(rounds[r].axis.point(rounds[r].t1));
        (o.min(p), o.max(p))
    };
    let t0 = hole.rounds.iter().map(|&r| along(r).0).fold(f64::MAX, f64::min);
    let t1 = hole.rounds.iter().map(|&r| along(r).1).fold(f64::MIN, f64::max);
    let inside: Vec<usize> = hole
        .rounds
        .iter()
        .flat_map(|&r| rounds[r].faces.iter().copied())
        .chain(hole.planes.iter().copied())
        .collect();
    // An end is open where an edge there meets a face outside the hole.
    let open_at = |end: f64| {
        hole.rounds.iter().flat_map(|&r| rounds[r].faces.iter()).any(|&f| {
            topo.face_edges[f].iter().any(|&e| {
                axis.edge_t(&topo.edges[e]).is_some_and(|(a, b)| (a - end).abs() < TOL || (b - end).abs() < TOL)
                    && topo.edge_faces(e).any(|g| g.is_none_or(|g| !inside.contains(&g)))
            })
        })
    };
    let widest_at = |end: f64| {
        hole.rounds
            .iter()
            .filter(|&&r| {
                let (a, b) = along(r);
                (a - end).abs() < TOL || (b - end).abs() < TOL
            })
            .map(|&r| rounds[r].r_max)
            .fold(0.0, f64::max)
    };
    let (open0, open1) = (open_at(t0), open_at(t1));
    let from_low = match (open0, open1) {
        (true, true) => widest_at(t0) >= widest_at(t1) - TOL,
        (true, false) => true,
        (false, true) => false,
        (false, false) => return None, // a closed cavity, not a hole
    };
    let (entry_t, sign) = if from_low { (t0, 1.0) } else { (t1, -1.0) };
    let mut steps: Vec<(usize, f64, f64)> = hole
        .rounds
        .iter()
        .map(|&r| {
            let (a, b) = along(r);
            let (a, b) = ((a - entry_t) * sign, (b - entry_t) * sign);
            (r, a.min(b), a.max(b))
        })
        .collect();
    steps.sort_by(|x, y| (x.1 + x.2).total_cmp(&(y.1 + y.2)));
    let through = open0 && open1;
    // A blind hole's depth stops where its drill point starts, as the Hole
    // Wizard measures it.
    let depth = match steps.last() {
        Some(&(r, from, _)) if !through && rounds[r].cone && steps.len() > 1 => from,
        _ => t1 - t0,
    };
    Some(HoleShape { through, entry: axis.point(entry_t), direction: scale(axis.a, sign), depth, steps })
}

/// "8" or "8.25": millimeters for text.
fn num(m: f64) -> String {
    format!("{}", round(m * crate::inspect::MM_PER_M, 3))
}

fn deg(radians: f64) -> f64 {
    round(radians.to_degrees(), 2)
}

fn point_json(p: V) -> Value {
    json!([mm(p[0]), mm(p[1]), mm(p[2])])
}

fn dir_json(d: V) -> Value {
    json!([round(d[0], 6), round(d[1], 6), round(d[2], 6)])
}

struct Recognizer<'a> {
    topo: Topology<'a>,
    rounds: Vec<Round>,
    used: Vec<bool>,
}

impl Recognizer<'_> {
    fn ids(&self, ids: &mut IdRegistry, faces: &[usize]) -> Value {
        json!(faces.iter().filter_map(|&f| ids.id_for(Kind::Face, &self.topo.faces[f].persist_id)).collect::<Vec<_>>())
    }

    fn take(&mut self, faces: &[usize]) {
        for &f in faces {
            self.used[f] = true;
        }
    }

    fn body_id(&self, ids: &mut IdRegistry, f: usize) -> Value {
        ids.id_for(Kind::Body, &self.topo.faces[f].body_persist_id).map_or(Value::Null, Value::String)
    }
}

/// The recognize_features reply for an InspectModel reply with features,
/// faces and edges.
pub fn render(ids: &mut IdRegistry, reply: &proto::InspectModelResponse) -> Value {
    let topo = Topology::new(reply);
    let rounds = rounds(&topo);
    let mut r = Recognizer { used: vec![false; topo.faces.len()], topo, rounds };
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }
    let imported: Vec<&str> = reply
        .features
        .iter()
        .filter(|f| inspect::is_imported_type(&f.type_name))
        .map(|f| f.name.as_str())
        .collect();
    if !imported.is_empty() {
        out.insert("imported".into(), json!(imported));
    }
    let mut summary: Vec<String> = Vec::new();

    // ---- Holes
    let mut hole_list = Vec::new();
    let mut hole_lines: BTreeMap<String, usize> = BTreeMap::new();
    for hole in holes(&r.topo, &r.rounds) {
        let Some(shape) = hole_shape(&r.topo, &r.rounds, &hole) else { continue };
        let cylinders: Vec<usize> = shape.steps.iter().map(|s| s.0).filter(|&i| !r.rounds[i].cone).collect();
        let Some(diameter) = cylinders.iter().map(|&i| 2.0 * r.rounds[i].radius).reduce(f64::min) else {
            continue; // only cones: a conical pocket
        };
        let mut obj = Map::new();
        let first = shape.steps[0].0;
        let kind = if r.rounds[first].cone {
            obj.insert(
                "countersink".into(),
                json!({ "diameter_mm": mm(2.0 * r.rounds[first].r_max),
                        "angle_deg": deg(2.0 * r.rounds[first].half_angle) }),
            );
            "countersink"
        } else if 2.0 * r.rounds[first].radius > diameter + TOL {
            obj.insert(
                "counterbore".into(),
                json!({ "diameter_mm": mm(2.0 * r.rounds[first].radius), "depth_mm": mm(shape.steps[0].2) }),
            );
            "counterbore"
        } else {
            "simple"
        };
        let last = shape.steps[shape.steps.len() - 1].0;
        if !shape.through && r.rounds[last].cone {
            obj.insert("drill_point_angle_deg".into(), json!(deg(2.0 * r.rounds[last].half_angle)));
        }
        obj.insert("kind".into(), json!(kind));
        obj.insert("diameter_mm".into(), json!(mm(diameter)));
        obj.insert("through".into(), json!(shape.through));
        obj.insert("depth_mm".into(), json!(mm(shape.depth)));
        obj.insert("entry_mm".into(), point_json(shape.entry));
        obj.insert("direction".into(), dir_json(shape.direction));
        if shape.steps.len() > 1 {
            let steps: Vec<Value> = shape
                .steps
                .iter()
                .map(|&(i, from, to)| {
                    let round = &r.rounds[i];
                    let mut s = Map::new();
                    s.insert("surface".into(), json!(if round.cone { "cone" } else { "cylinder" }));
                    if round.cone {
                        s.insert("diameters_mm".into(), json!([mm(2.0 * round.r_max), mm(2.0 * round.r_min)]));
                    } else {
                        s.insert("diameter_mm".into(), json!(mm(2.0 * round.radius)));
                    }
                    s.insert("from_mm".into(), json!(mm(from)));
                    s.insert("to_mm".into(), json!(mm(to)));
                    Value::Object(s)
                })
                .collect();
            obj.insert("steps".into(), json!(steps));
        }
        let faces: Vec<usize> = hole
            .rounds
            .iter()
            .flat_map(|&i| r.rounds[i].faces.iter().copied())
            .chain(hole.planes.iter().copied())
            .collect();
        obj.insert("faces".into(), r.ids(ids, &faces));
        obj.insert("body".into(), r.body_id(ids, faces[0]));
        r.take(&faces);
        let mut line = format!("Ø{} {kind} hole", num(diameter));
        line += &if shape.through { ", through".to_string() } else { format!(", {} deep", num(shape.depth)) };
        if let Some(Value::Object(c)) = obj.get("counterbore") {
            line += &format!(" (counterbore Ø{} x {})", c["diameter_mm"], c["depth_mm"]);
        }
        if let Some(Value::Object(c)) = obj.get("countersink") {
            line += &format!(" (countersink Ø{} x {}°)", c["diameter_mm"], c["angle_deg"]);
        }
        *hole_lines.entry(line).or_default() += 1;
        hole_list.push(Value::Object(obj));
    }
    summary.extend(hole_lines.iter().map(|(line, n)| format!("{n} x {line}")));

    // ---- Bosses, fillets, rounded ends, conical chamfers
    let mut bosses = Vec::new();
    let mut fillets: BTreeMap<(i64, &str), Vec<usize>> = BTreeMap::new();
    let mut arcs = Vec::new();
    let mut chamfers = Vec::new();
    for i in 0..r.rounds.len() {
        let round = &r.rounds[i];
        if round.faces.iter().any(|&f| r.used[f]) {
            continue;
        }
        let faces = round.faces.clone();
        if !round.cone && round.full() && round.concave == Some(false) {
            bosses.push(json!({
                "diameter_mm": mm(2.0 * round.radius),
                "length_mm": mm(round.length()),
                "axis": dir_json(round.axis.a),
                "from_mm": point_json(round.axis.point(round.t0)),
                "to_mm": point_json(round.axis.point(round.t1)),
                "faces": r.ids(ids, &faces),
                "body": r.body_id(ids, faces[0]),
            }));
        } else if !round.cone && !round.full() && round.span < FILLET_MAX_SPAN {
            let kind = match round.concave {
                Some(true) => "fillet",
                Some(false) => "round",
                None => "blend",
            };
            fillets.entry(((round.radius * 1e7).round() as i64, kind)).or_default().extend(&faces);
        } else if !round.cone && !round.full() {
            arcs.push(json!({
                "radius_mm": mm(round.radius),
                "span_deg": deg(round.span),
                "concave": round.concave,
                "axis": dir_json(round.axis.a),
                "axis_point_mm": point_json(round.axis.point(round.t0)),
                "length_mm": mm(round.length()),
                "faces": r.ids(ids, &faces),
            }));
        } else if round.cone && round.concave == Some(false) && round.r_max - round.r_min > TOL {
            chamfers.push(json!({
                "kind": "conical",
                "axial_mm": mm(round.length()),
                "radial_mm": mm(round.r_max - round.r_min),
                "angle_deg": deg(round.half_angle),
                "faces": r.ids(ids, &faces),
            }));
        } else {
            continue;
        }
        r.take(&faces);
    }
    // Tori are fillets around a circular edge (minor radius = fillet radius).
    for f in 0..r.topo.faces.len() {
        let face = &r.topo.faces[f];
        if r.used[f] || face.surface_type() != S::Torus {
            continue;
        }
        let kind = match torus_convex(face) {
            Some(true) => "round",
            Some(false) => "fillet",
            None => "blend",
        };
        fillets.entry(((face.minor_radius * 1e7).round() as i64, kind)).or_default().push(f);
        r.used[f] = true;
    }
    if !bosses.is_empty() {
        summary.push(format!("{} cylindrical boss(es)/shaft(s)", bosses.len()));
    }
    let fillet_list: Vec<Value> = fillets
        .iter()
        .map(|(&(radius, kind), faces)| {
            summary.push(format!("{kind}s R{}: {} face(s)", num(radius as f64 / 1e7), faces.len()));
            json!({ "radius_mm": mm(radius as f64 / 1e7), "kind": kind, "faces": r.ids(ids, faces) })
        })
        .collect();
    if !arcs.is_empty() {
        summary.push(format!("{} rounded end(s) (partial cylinders, e.g. slot ends)", arcs.len()));
    }

    // ---- Planar chamfers: a narrow face between two planes, leaning on both
    for f in 0..r.topo.faces.len() {
        if r.used[f] || r.topo.faces[f].surface_type() != S::Plane {
            continue;
        }
        if let Some((mut obj, a, b)) = planar_chamfer(&r.topo, f) {
            obj.insert("kind".into(), json!("planar"));
            obj.insert("faces".into(), r.ids(ids, &[f]));
            // legs_mm[0] lies on between[0], legs_mm[1] on between[1].
            obj.insert("between".into(), r.ids(ids, &[a, b]));
            chamfers.push(Value::Object(obj));
            r.used[f] = true;
        }
    }
    if !chamfers.is_empty() {
        summary.push(format!("{} chamfer(s)", chamfers.len()));
    }

    // ---- Planar faces by direction
    let mut directions: Vec<(V, Vec<(f64, usize)>)> = Vec::new();
    for f in 0..r.topo.faces.len() {
        if r.used[f] || r.topo.faces[f].surface_type() != S::Plane {
            continue;
        }
        let (Some(n), Some(o)) = (r.topo.normal(f), r.topo.faces[f].origin.as_ref()) else { continue };
        let at = |d: V| dot(xyz(o), d);
        match directions.iter_mut().find(|(d, _)| parallel(*d, n)) {
            Some((d, faces)) => faces.push((at(*d), f)),
            None => directions.push((n, vec![(at(n), f)])),
        }
        r.used[f] = true;
    }
    let plane_count: usize = directions.iter().map(|(_, faces)| faces.len()).sum();
    if plane_count > 0 {
        summary.push(format!("{plane_count} planar face(s) in {} direction(s)", directions.len()));
    }
    let planes: Vec<Value> = directions
        .iter_mut()
        .map(|(d, faces)| {
            faces.sort_by(|a, b| a.0.total_cmp(&b.0));
            let levels: Vec<Value> = faces
                .iter()
                .map(|&(at, f)| {
                    let facing = r.topo.normal(f).is_some_and(|n| dot(n, *d) > 0.0);
                    json!({ "at_mm": mm(at), "facing": if facing { "+" } else { "-" },
                            "face": ids.id_for(Kind::Face, &r.topo.faces[f].persist_id) })
                })
                .collect();
            json!({ "direction": dir_json(*d), "faces": levels })
        })
        .collect();

    let other: Vec<usize> = (0..r.topo.faces.len()).filter(|&f| !r.used[f]).collect();
    if !other.is_empty() {
        summary.push(format!("{} other face(s) not recognized", other.len()));
    }
    if reply.truncated {
        summary.push("the model has more faces or edges than the add-in lists at once: narrow it with `within`".into());
    }

    out.insert("summary".into(), json!(summary));
    for (key, list) in [
        ("holes", hole_list),
        ("bosses", bosses),
        ("fillets", fillet_list),
        ("rounded_ends", arcs),
        ("chamfers", chamfers),
        ("planes", planes),
    ] {
        if !list.is_empty() {
            out.insert(key.into(), Value::Array(list));
        }
    }
    if !other.is_empty() {
        let shown: Vec<usize> = other.iter().copied().take(MAX_OTHER).collect();
        out.insert("other_faces".into(), r.ids(ids, &shown));
    }
    Value::Object(out)
}

/// Whether a torus face is convex (material inside its tube), from its
/// sample point and outward normal.
fn torus_convex(f: &proto::FaceInfo) -> Option<bool> {
    let axis = Axis::of(f)?;
    let p = xyz(f.sample_point.as_ref()?);
    let n = xyz(f.sample_normal.as_ref()?);
    let radial = unit(sub(sub(p, axis.o), scale(axis.a, axis.t(p))))?;
    // The center of the tube's cross-section nearest p.
    let ring = add(axis.o, scale(radial, f.radius));
    let d = dot(n, sub(p, ring));
    (d != 0.0).then_some(d > 0.0)
}

/// A planar face that is a chamfer: it meets two planar faces along lines,
/// leans on both (its normal between theirs), and is narrow. Gives its legs
/// on each, measured from the corner it replaced, and those two faces.
fn planar_chamfer(topo: &Topology, f: usize) -> Option<(Map<String, Value>, usize, usize)> {
    let nc = topo.normal(f)?;
    let mut sides: Vec<(usize, V, f64)> = Vec::new(); // (face, normal, shared line length)
    for &e in &topo.face_edges[f] {
        let edge = &topo.edges[e];
        if edge.curve_type() != proto::CurveType::Line {
            continue;
        }
        for g in topo.edge_faces(e).flatten() {
            if g != f
                && topo.faces[g].surface_type() == S::Plane
                && let Some(n) = topo.normal(g)
            {
                sides.push((g, n, edge.length));
            }
        }
    }
    for (i, &(a, na, la)) in sides.iter().enumerate() {
        for &(b, nb, lb) in &sides[i + 1..] {
            let (alpha, beta, phi) = (angle(nc, na), angle(nc, nb), angle(na, nb));
            let leaning = |x: f64| x > 5f64.to_radians() && x < 85f64.to_radians();
            let coplanar = cross(na, nb);
            if !(leaning(alpha) && leaning(beta)) || (alpha + beta - phi).abs() > 1e-3 {
                continue;
            }
            if unit(coplanar).is_some_and(|c| dot(c, nc).abs() > 1e-3) {
                continue;
            }
            // Its width across, from its area and the length of the edges it
            // shares; a chamfer is narrow next to them.
            let length = la.min(lb);
            let width = topo.faces[f].area / length;
            if width > length / 2.0 {
                continue;
            }
            let mut obj = Map::new();
            obj.insert("width_mm".into(), json!(mm(width)));
            obj.insert(
                "legs_mm".into(),
                json!([mm(width * beta.sin() / phi.sin()), mm(width * alpha.sin() / phi.sin())]),
            );
            obj.insert("length_mm".into(), json!(mm(length)));
            return Some((obj, a, b));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> Option<proto::Vec3> {
        Some(proto::Vec3 { x, y, z })
    }

    struct Builder(proto::InspectModelResponse);

    impl Builder {
        fn face(&mut self, f: proto::FaceInfo) -> Vec<u8> {
            let pid = vec![1, self.0.faces.len() as u8];
            self.0.faces.push(proto::FaceInfo { persist_id: pid.clone(), body_persist_id: vec![9], ..f });
            pid
        }

        fn plane(&mut self, n: [f64; 3], o: [f64; 3], area: f64) -> Vec<u8> {
            self.face(proto::FaceInfo {
                surface_type: S::Plane as i32,
                normal: v(n[0], n[1], n[2]),
                origin: v(o[0], o[1], o[2]),
                area,
                ..Default::default()
            })
        }

        /// A Z-axis cylinder of radius r from z0 to z1 at (x, y), in `parts`
        /// faces, concave (a hole) or not.
        fn cylinder(&mut self, at: [f64; 2], r: f64, hole: bool, z0: f64, z1: f64, parts: usize) -> Vec<Vec<u8>> {
            let n = if hole { -1.0 } else { 1.0 };
            (0..parts)
                .map(|_| {
                    self.face(proto::FaceInfo {
                        surface_type: S::Cylinder as i32,
                        origin: v(at[0], at[1], 0.0),
                        axis: v(0.0, 0.0, 1.0),
                        radius: r,
                        area: std::f64::consts::TAU * r * (z1 - z0) / parts as f64,
                        sample_point: v(at[0] + r, at[1], (z0 + z1) / 2.0),
                        sample_normal: v(n, 0.0, 0.0),
                        ..Default::default()
                    })
                })
                .collect()
        }

        fn circle(&mut self, at: [f64; 3], r: f64, faces: &[&Vec<u8>]) {
            self.0.edges.push(proto::EdgeInfo {
                curve_type: proto::CurveType::Circle as i32,
                center: v(at[0], at[1], at[2]),
                axis: v(0.0, 0.0, 1.0),
                radius: r,
                face_persist_ids: faces.iter().map(|f| (*f).clone()).collect(),
                ..Default::default()
            });
        }

        fn line(&mut self, a: [f64; 3], b: [f64; 3], faces: &[&Vec<u8>]) {
            self.0.edges.push(proto::EdgeInfo {
                curve_type: proto::CurveType::Line as i32,
                start: v(a[0], a[1], a[2]),
                end: v(b[0], b[1], b[2]),
                length: norm(sub(b, a)),
                face_persist_ids: faces.iter().map(|f| (*f).clone()).collect(),
                ..Default::default()
            });
        }
    }

    /// A 10 mm plate (z 0..0.01) with an 8 mm through hole in two halves, a
    /// blind 5 mm hole 6 deep with a flat floor, and a counterbored 6 mm
    /// through hole (11 mm x 3 deep).
    fn plate() -> proto::InspectModelResponse {
        let mut b = Builder(proto::InspectModelResponse::default());
        let top = b.plane([0.0, 0.0, 1.0], [0.0, 0.0, 0.01], 0.004);
        let bottom = b.plane([0.0, 0.0, -1.0], [0.0, 0.0, 0.0], 0.004);
        let through = b.cylinder([0.0, 0.0], 0.004, true, 0.0, 0.01, 2);
        for z in [0.0, 0.01] {
            let cap = if z == 0.0 { &bottom } else { &top };
            for half in &through {
                b.circle([0.0, 0.0, z], 0.004, &[half, cap]);
            }
        }
        b.line([0.004, 0.0, 0.0], [0.004, 0.0, 0.01], &[&through[0], &through[1]]);
        b.line([-0.004, 0.0, 0.0], [-0.004, 0.0, 0.01], &[&through[0], &through[1]]);

        let blind = b.cylinder([0.02, 0.0], 0.0025, true, 0.004, 0.01, 1).remove(0);
        let floor = b.plane([0.0, 0.0, 1.0], [0.02, 0.0, 0.004], std::f64::consts::PI * 0.0025 * 0.0025);
        b.circle([0.02, 0.0, 0.01], 0.0025, &[&blind, &top]);
        b.circle([0.02, 0.0, 0.004], 0.0025, &[&blind, &floor]);

        let cbore = b.cylinder([-0.02, 0.0], 0.0055, true, 0.007, 0.01, 1).remove(0);
        let shoulder = b.plane([0.0, 0.0, 1.0], [-0.02, 0.0, 0.007], 7e-5);
        let bore = b.cylinder([-0.02, 0.0], 0.003, true, 0.0, 0.007, 1).remove(0);
        b.circle([-0.02, 0.0, 0.01], 0.0055, &[&cbore, &top]);
        b.circle([-0.02, 0.0, 0.007], 0.0055, &[&cbore, &shoulder]);
        b.circle([-0.02, 0.0, 0.007], 0.003, &[&bore, &shoulder]);
        b.circle([-0.02, 0.0, 0.0], 0.003, &[&bore, &bottom]);

        // A 4 mm hole 5 deep from the top with a 118 degree drill point.
        let drilled = b.cylinder([0.0, 0.02], 0.002, true, 0.005, 0.01, 1).remove(0);
        let half = 59f64.to_radians();
        let tip = 0.002 / half.tan();
        let point = b.face(proto::FaceInfo {
            surface_type: S::Cone as i32,
            origin: v(0.0, 0.02, 0.005),
            axis: v(0.0, 0.0, 1.0),
            radius: 0.002,
            half_angle: half,
            area: std::f64::consts::PI * 0.002 * (0.002 / half.sin()),
            sample_point: v(0.001, 0.02, 0.005 - tip / 2.0),
            sample_normal: v(-half.cos(), 0.0, half.sin()),
            ..Default::default()
        });
        b.circle([0.0, 0.02, 0.01], 0.002, &[&drilled, &top]);
        b.circle([0.0, 0.02, 0.005], 0.002, &[&drilled, &point]);
        b.0
    }

    #[test]
    fn holes_are_recognized() {
        let mut ids = IdRegistry::default();
        let json = render(&mut ids, &plate());
        let holes = json["holes"].as_array().unwrap();
        assert_eq!(holes.len(), 4, "{json:#}");
        let drilled = holes.iter().find(|h| h["diameter_mm"] == json!(4.0)).unwrap();
        assert_eq!(drilled["through"], json!(false), "{json:#}");
        assert_eq!(drilled["depth_mm"], json!(5.0), "to the drill point's shoulder");
        assert_eq!(drilled["drill_point_angle_deg"], json!(118.0));
        assert_eq!(drilled["entry_mm"], json!([0.0, 20.0, 10.0]));
        assert!(json.get("other_faces").is_none(), "{json:#}");
        let by_dia = |d: f64| holes.iter().find(|h| h["diameter_mm"] == json!(d)).unwrap();
        let through = by_dia(8.0);
        assert_eq!((through["kind"].clone(), through["through"].clone()), (json!("simple"), json!(true)));
        assert_eq!(through["faces"].as_array().unwrap().len(), 2, "both halves");
        let blind = by_dia(5.0);
        assert_eq!((blind["through"].clone(), blind["depth_mm"].clone()), (json!(false), json!(6.0)));
        assert_eq!(blind["entry_mm"], json!([20.0, 0.0, 10.0]));
        assert_eq!(blind["direction"], json!([0.0, 0.0, -1.0]));
        let cbore = by_dia(6.0);
        assert_eq!(cbore["kind"], json!("counterbore"));
        assert_eq!(cbore["counterbore"], json!({"diameter_mm": 11.0, "depth_mm": 3.0}));
        assert_eq!(cbore["entry_mm"], json!([-20.0, 0.0, 10.0]));
        assert_eq!(cbore["through"], json!(true));
        let summary = json["summary"].to_string();
        assert!(summary.contains("1 x Ø8 simple hole, through"), "{summary}");
        assert!(summary.contains("1 x Ø5 simple hole, 6 deep"), "{summary}");
        // Top and bottom are left as planes; floor and shoulder belong to holes.
        let planes = json["planes"].as_array().unwrap();
        assert_eq!(planes.len(), 1);
        assert_eq!(planes[0]["faces"].as_array().unwrap().len(), 2, "{json:#}");
    }

    #[test]
    fn revolution_faces_are_fitted() {
        // A counterbore as IGES brings it in: surfaces of revolution about Z
        // with the axis point far off, rims as closed intersection curves.
        let mut b = Builder(proto::InspectModelResponse::default());
        let top = b.plane([0.0, 0.0, 1.0], [0.0, 0.0, 0.01], 0.004);
        let bottom = b.plane([0.0, 0.0, -1.0], [0.0, 0.0, 0.0], 0.004);
        let shoulder = b.plane([0.0, 0.0, 1.0], [0.0, 0.0, 0.007], 7e-5);
        let revolution = |b: &mut Builder, r: f64, z0: f64, z1: f64| {
            b.face(proto::FaceInfo {
                surface_type: S::Revolution as i32,
                origin: v(0.0, 0.0, -1.0),
                axis: v(0.0, 0.0, 1.0),
                area: std::f64::consts::TAU * r * (z1 - z0),
                sample_point: v(0.0, r, (z0 + z1) / 2.0),
                sample_normal: v(0.0, -1.0, 0.0),
                ..Default::default()
            })
        };
        let cbore = revolution(&mut b, 0.0055, 0.007, 0.01);
        let bore = revolution(&mut b, 0.003, 0.0, 0.007);
        let rim = |b: &mut Builder, r: f64, z: f64, faces: &[&Vec<u8>]| {
            b.0.edges.push(proto::EdgeInfo {
                curve_type: proto::CurveType::Intersection as i32,
                start: v(r, 0.0, z),
                end: v(r, 0.0, z),
                face_persist_ids: faces.iter().map(|f| (*f).clone()).collect(),
                ..Default::default()
            });
        };
        rim(&mut b, 0.0055, 0.01, &[&cbore, &top]);
        rim(&mut b, 0.0055, 0.007, &[&cbore, &shoulder]);
        rim(&mut b, 0.003, 0.007, &[&bore, &shoulder]);
        rim(&mut b, 0.003, 0.0, &[&bore, &bottom]);
        let mut ids = IdRegistry::default();
        let json = render(&mut ids, &b.0);
        let hole = &json["holes"][0];
        assert_eq!(hole["kind"], json!("counterbore"), "{json:#}");
        assert_eq!(hole["diameter_mm"], json!(6.0));
        assert_eq!(hole["counterbore"], json!({"diameter_mm": 11.0, "depth_mm": 3.0}));
        assert_eq!(hole["entry_mm"], json!([0.0, 0.0, 10.0]));
    }

    #[test]
    fn bosses_fillets_and_chamfers() {
        let mut b = Builder(proto::InspectModelResponse::default());
        // A 6 mm pin standing on a plate.
        let top = b.plane([0.0, 0.0, 1.0], [0.0, 0.0, 0.0], 0.004);
        let pin = b.cylinder([0.0, 0.0], 0.003, false, 0.0, 0.02, 1).remove(0);
        let end = b.plane([0.0, 0.0, 1.0], [0.0, 0.0, 0.02], 2.8e-5);
        b.circle([0.0, 0.0, 0.0], 0.003, &[&pin, &top]);
        b.circle([0.0, 0.0, 0.02], 0.003, &[&pin, &end]);
        // A quarter round, R2, 30 long, along Y at x = z = 0.05.
        let s = std::f64::consts::FRAC_1_SQRT_2;
        let round = b.face(proto::FaceInfo {
            surface_type: S::Cylinder as i32,
            origin: v(0.048, 0.0, 0.048),
            axis: v(0.0, 1.0, 0.0),
            radius: 0.002,
            area: std::f64::consts::FRAC_PI_2 * 0.002 * 0.03,
            sample_point: v(0.048 + 0.002 * s, 0.015, 0.048 + 0.002 * s),
            sample_normal: v(s, 0.0, s),
            ..Default::default()
        });
        b.line([0.048, 0.0, 0.05], [0.048, 0.03, 0.05], &[&round]);
        b.line([0.05, 0.0, 0.048], [0.05, 0.03, 0.048], &[&round]);
        // A 45 degree chamfer, 1 mm legs, 30 long, between +X and +Z faces.
        let side = b.plane([1.0, 0.0, 0.0], [0.1, 0.0, 0.0], 0.0003);
        let lid = b.plane([0.0, 0.0, 1.0], [0.0, 0.0, 0.1], 0.0003);
        let chamfer = b.plane([s, 0.0, s], [0.1, 0.0, 0.099], 0.03 * 0.001 * 2f64.sqrt());
        b.line([0.1, 0.0, 0.099], [0.1, 0.03, 0.099], &[&chamfer, &side]);
        b.line([0.099, 0.0, 0.1], [0.099, 0.03, 0.1], &[&chamfer, &lid]);
        let mut ids = IdRegistry::default();
        let json = render(&mut ids, &b.0);
        let boss = &json["bosses"][0];
        assert_eq!((boss["diameter_mm"].clone(), boss["length_mm"].clone()), (json!(6.0), json!(20.0)), "{json:#}");
        assert_eq!(json["fillets"][0]["radius_mm"], json!(2.0), "{json:#}");
        assert_eq!(json["fillets"][0]["kind"], json!("round"));
        let c = &json["chamfers"][0];
        assert_eq!(c["kind"], json!("planar"), "{json:#}");
        assert_eq!(c["legs_mm"], json!([1.0, 1.0]));
        assert!(json.get("holes").is_none());
    }
}
