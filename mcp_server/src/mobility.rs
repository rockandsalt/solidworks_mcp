//! inspect_mobility: the remaining degrees of freedom of an assembly's
//! components -- how many translations and rotations their mates leave,
//! and along / about what.
//!
//! SolidWorks' own GetRemainingDOFs answers "unavailable" for every
//! component (seen 2026-10-25), so the add-in measures instead: it nudges
//! each component a little along and about the assembly axes with
//! SetTransformAndSolve2 (as dragging it would) and reports where the
//! solver let it, and anything mated to it, go. Each motion a component
//! made -- in its own probes, or dragged along by another's -- is a twist
//! (velocity at its box center, angular velocity) its mates allow. This
//! module spans those twists: the span's dimension is the component's
//! degrees of freedom, the part without rotation its free translations, and
//! the rest rotation axes (direction + a point on them, + pitch when
//! turning also advances it, as a screw).

use std::collections::HashMap;

use glam::{DMat3, DQuat, DVec3};
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, round};

/// Probe sizes: small enough that the motion is the mates' first-order
/// freedom, big enough to stand out from solver noise.
const DEFAULT_STEP_MM: f64 = 1.0;
const DEFAULT_STEP_DEG: f64 = 1.0;
const MAX_STEP_MM: f64 = 100.0;
const MAX_STEP_DEG: f64 = 30.0;
/// The add-in stops starting components after this long, well inside the
/// 60 s request timeout; the rest are reported unprobed.
const TIME_BUDGET_S: f64 = 30.0;
const MAX_COMPONENTS: usize = 100;
/// A component moved less than this (mm, degrees) didn't move: the solver
/// leaves sub-micron noise on components it didn't really move (as
/// component::moves).
const MOVED_MM: f64 = 1e-3;
const MOVED_DEG: f64 = 0.01;
/// Nor did one that moved less than this share of the nudge: the solver
/// holds some mates only within micrometers and snaps them back on any
/// solve (two components locked to each other moved 3 um, seen 2026-10-25).
const MOVED_SHARE: f64 = 0.01;
/// Each motion counts as a unit twist; a direction of the span whose
/// eigenvalue (sum of squared alignments) is below this is noise, e.g. the
/// second-order drift of a finite turn.
const RANK_EIGEN: f64 = 0.05;
/// A span direction whose rotation part is below this (squared) is a pure
/// translation.
const TRANSLATION_EIGEN: f64 = 1e-4;
/// Turning less than this far along the axis per radian isn't a screw.
const PITCH_MM_PER_RAD: f64 = 0.01;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectMobilityParams {
    /// The components to report, by id ("component:3" from inspect_model what="components"):
    /// top-level components (a subassembly moves as one). Default: every top-level component.
    /// Probing all of them also catches motion a component only has together with another
    /// (a follower tied to a slider).
    #[serde(default)]
    pub components: Option<Vec<String>>,
    /// How far each translation nudge goes, in mm. Default 1. Bigger can step over a nearby
    /// limit; smaller for tiny parts.
    #[serde(default)]
    pub step_mm: Option<f64>,
    /// How far each rotation nudge turns, in degrees. Default 1.
    #[serde(default)]
    pub step_deg: Option<f64>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// The InspectMobility request, with component ids resolved by `resolve`.
pub fn request(
    params: &InspectMobilityParams,
    mut resolve: impl FnMut(&str) -> Result<Vec<u8>, McpError>,
) -> Result<proto::InspectMobilityRequest, McpError> {
    let step_mm = params.step_mm.unwrap_or(DEFAULT_STEP_MM);
    if !(step_mm > 0.0 && step_mm <= MAX_STEP_MM) {
        return Err(invalid(format!("step_mm must be more than 0 and at most {MAX_STEP_MM}, got {step_mm}")));
    }
    let step_deg = params.step_deg.unwrap_or(DEFAULT_STEP_DEG);
    if !(step_deg > 0.0 && step_deg <= MAX_STEP_DEG) {
        return Err(invalid(format!("step_deg must be more than 0 and at most {MAX_STEP_DEG}, got {step_deg}")));
    }
    let components = params.components.as_deref().unwrap_or_default();
    if components.len() > MAX_COMPONENTS {
        return Err(invalid(format!("at most {MAX_COMPONENTS} components per call, got {}", components.len())));
    }
    let mut persist_ids = Vec::new();
    for id in components.iter().map(|id| id.trim()) {
        if Kind::of_id(id) != Some(Kind::Component) {
            return Err(invalid(format!(
                "components takes component ids like \"component:2\" (inspect_model lists them), got {id:?}"
            )));
        }
        persist_ids.push(resolve(id)?);
    }
    Ok(proto::InspectMobilityRequest {
        component_persist_ids: persist_ids,
        step: step_mm / MM_PER_M,
        step_angle: step_deg.to_radians(),
        time_budget: TIME_BUDGET_S,
    })
}

// ---- Twists -------------------------------------------------------------

fn vec(v: &Option<proto::Vec3>) -> DVec3 {
    v.as_ref().map(|v| DVec3::new(v.x, v.y, v.z)).unwrap_or_default()
}

/// Origin (meters) and rotation (the axes as columns).
type Frame = (DVec3, DMat3);

fn info_frame(c: &proto::ComponentInfo) -> Frame {
    (vec(&c.origin), DMat3::from_cols(vec(&c.x_axis), vec(&c.y_axis), vec(&c.z_axis)))
}

fn placement_frame(p: &proto::ComponentPlacement) -> Frame {
    (vec(&p.origin), DMat3::from_cols(vec(&p.x_axis), vec(&p.y_axis), vec(&p.z_axis)))
}

/// A small rigid motion: `v` how far the reference point went (mm), `w`
/// the turn as axis * angle (radians).
#[derive(Debug, Clone, Copy)]
struct Twist {
    v: DVec3,
    w: DVec3,
}

/// The motion from `from` to `to`, seen at `center` (meters): the twist
/// that, kept up for unit time, makes exactly this motion (SE(3)'s log),
/// so a finite turn about an axis away from the center gives that axis
/// rather than its chord.
fn twist(from: Frame, to: Frame, center: DVec3) -> Twist {
    let turn = to.1 * from.1.transpose();
    let (axis, angle) = DQuat::from_mat3(&turn).normalize().to_axis_angle();
    let angle = if angle > std::f64::consts::PI { angle - std::f64::consts::TAU } else { angle };
    let w = axis * angle;
    let moved = (turn * (center - from.0) + to.0 - center) * MM_PER_M;
    if angle.abs() < 1e-9 {
        return Twist { v: moved, w };
    }
    // moved = V v with V = I + (1 - cos a) / a^2 [w]x + (a - sin a) / a^3 [w]x^2.
    let cross = DMat3::from_cols(DVec3::new(0.0, w.z, -w.y), DVec3::new(-w.z, 0.0, w.x), DVec3::new(w.y, -w.x, 0.0));
    let a = angle.abs();
    let v_of = DMat3::IDENTITY + cross * ((1.0 - a.cos()) / (a * a)) + cross * cross * ((a - a.sin()) / (a * a * a));
    Twist { v: v_of.inverse() * moved, w }
}

fn is_motion(t: &Twist) -> bool {
    t.v.length() >= MOVED_MM || t.w.length().to_degrees() >= MOVED_DEG
}

// ---- The span -----------------------------------------------------------

/// Eigenvalues and unit eigenvectors of a symmetric matrix (cyclic Jacobi),
/// largest first.
fn symmetric_eigen(mut a: Vec<Vec<f64>>) -> Vec<(f64, Vec<f64>)> {
    let n = a.len();
    let mut v: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect()).collect();
    for _ in 0..100 {
        let off: f64 = (0..n).flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j))).map(|(i, j)| a[i][j] * a[i][j]).sum();
        if off < 1e-24 {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let (akp, akq) = (a[k][p], a[k][q]);
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..n {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for row in v.iter_mut() {
                    let (vkp, vkq) = (row[p], row[q]);
                    row[p] = c * vkp - s * vkq;
                    row[q] = s * vkp + c * vkq;
                }
            }
        }
    }
    let mut out: Vec<(f64, Vec<f64>)> = (0..n).map(|j| (a[j][j], v.iter().map(|row| row[j]).collect())).collect();
    out.sort_by(|x, y| y.0.total_cmp(&x.0));
    out
}

#[derive(Debug, Clone)]
struct Rotation {
    axis: DVec3,
    /// A point on the axis, mm.
    through: DVec3,
    /// How far turning advances it along the axis, mm per radian (screw).
    pitch: f64,
}

#[derive(Debug, Clone, Default)]
struct Freedom {
    translations: Vec<DVec3>,
    rotations: Vec<Rotation>,
    /// Three rotations about one point (a ball joint), or none if
    /// translation makes the point arbitrary.
    pivot: Option<DVec3>,
}

impl Freedom {
    fn dof(&self) -> usize {
        self.translations.len() + self.rotations.len()
    }
}

/// The span of the twists (seen at `center`, mm): `scale` mm of motion
/// weighs as much as a radian of turn (the component's size).
fn freedom(twists: &[Twist], center: DVec3, scale: f64) -> Freedom {
    let units: Vec<[f64; 6]> = twists
        .iter()
        .filter(|t| is_motion(t))
        .filter_map(|t| {
            let s = [t.v.x / scale, t.v.y / scale, t.v.z / scale, t.w.x, t.w.y, t.w.z];
            let n = s.iter().map(|x| x * x).sum::<f64>().sqrt();
            (n > 0.0).then(|| s.map(|x| x / n))
        })
        .collect();
    let mut gram = vec![vec![0.0; 6]; 6];
    for u in &units {
        for i in 0..6 {
            for j in 0..6 {
                gram[i][j] += u[i] * u[j];
            }
        }
    }
    let basis: Vec<Vec<f64>> =
        symmetric_eigen(gram).into_iter().filter(|(l, _)| *l > RANK_EIGEN).map(|(_, v)| v).collect();
    if basis.is_empty() {
        return Freedom::default();
    }

    // Split the span: combinations without rotation are translations.
    let k = basis.len();
    let w = |b: &Vec<f64>| DVec3::new(b[3], b[4], b[5]);
    let v = |b: &Vec<f64>| DVec3::new(b[0], b[1], b[2]);
    let m: Vec<Vec<f64>> = (0..k).map(|i| (0..k).map(|j| w(&basis[i]).dot(w(&basis[j]))).collect()).collect();
    let combine = |u: &[f64], part: &dyn Fn(&Vec<f64>) -> DVec3| -> DVec3 {
        basis.iter().zip(u).map(|(b, c)| part(b) * *c).sum()
    };
    let mut translations: Vec<DVec3> = Vec::new();
    let mut turns: Vec<(DVec3, DVec3)> = Vec::new();
    for (mu, u) in symmetric_eigen(m) {
        if mu < TRANSLATION_EIGEN {
            translations.push(combine(&u, &v));
        } else {
            turns.push((combine(&u, &w), combine(&u, &v) * scale));
        }
    }
    // The span's own basis is arbitrary: say it in assembly axes where
    // they fit (a free block slides along X, Y, Z, not three skew ways).
    let translations = snap(&translations);
    let turns = snap_turns(&turns);

    // Each turn: its velocity minus what free translation explains, then
    // the screw axis it is.
    let rotations: Vec<Rotation> = turns
        .iter()
        .map(|&(w, v)| {
            let v = translations.iter().fold(v, |v, t| v - *t * v.dot(*t));
            let w2 = w.length_squared();
            let pitch = w.dot(v) / w2;
            let v = v - w * pitch;
            Rotation { axis: w / w2.sqrt(), through: center + w.cross(v) / w2, pitch }
        })
        .collect();

    // Three rotations: about one point (least squares: v = w x (center - q)).
    let pivot = (rotations.len() == 3 && translations.len() < 3).then(|| {
        let mut a = DMat3::ZERO;
        let mut b = DVec3::ZERO;
        for &(w, v) in &turns {
            let v = translations.iter().fold(v, |v, t| v - *t * v.dot(*t));
            a += DMat3::from_diagonal(DVec3::splat(w.length_squared())) - outer(w, w);
            b += (w.cross(center) - v).cross(w);
        }
        (a.determinant().abs() > 1e-12).then(|| a.inverse() * b)
    });
    Freedom { translations, rotations, pivot: pivot.flatten() }
}

fn outer(a: DVec3, b: DVec3) -> DMat3 {
    DMat3::from_cols(a * b.x, a * b.y, a * b.z)
}

/// An orthonormal basis of the directions' span, made of the assembly axes'
/// shadows on it, the best-fitting first: X, Y, Z themselves when they lie
/// in it.
fn snap(dirs: &[DVec3]) -> Vec<DVec3> {
    let mut span: Vec<DVec3> = Vec::new();
    for d in dirs {
        let d = span.iter().fold(*d, |d, s| d - *s * d.dot(*s));
        if let Some(d) = d.try_normalize().filter(|_| d.length() > 1e-6) {
            span.push(d);
        }
    }
    let shadow = |a: DVec3| span.iter().map(|s| *s * a.dot(*s)).sum::<DVec3>();
    let mut axes = [DVec3::X, DVec3::Y, DVec3::Z].map(shadow);
    // Stable, ties (to rounding) in X, Y, Z order.
    axes.sort_by(|a, b| round(b.length(), 9).total_cmp(&round(a.length(), 9)));
    let mut out: Vec<DVec3> = Vec::new();
    for a in axes {
        let a = out.iter().fold(a, |a, o| a - *o * a.dot(*o));
        if out.len() < span.len() && a.length() > 1e-6 {
            out.push(a.normalize());
        }
    }
    out
}

/// The turns (angular, linear velocity) recombined so their axes are
/// `snap`'s of their angular velocities.
fn snap_turns(turns: &[(DVec3, DVec3)]) -> Vec<(DVec3, DVec3)> {
    let ws: Vec<DVec3> = turns.iter().map(|t| t.0).collect();
    // Least squares: the combination a of the turns' w closest to d.
    let mut gram = DMat3::IDENTITY;
    for (i, wi) in ws.iter().enumerate() {
        for (j, wj) in ws.iter().enumerate() {
            gram.col_mut(j)[i] = wi.dot(*wj);
        }
    }
    let Some(inverse) = (gram.determinant().abs() > 1e-12).then(|| gram.inverse()) else {
        return turns.to_vec();
    };
    snap(&ws)
        .into_iter()
        .map(|d| {
            let mut b = DVec3::ZERO;
            for (i, wi) in ws.iter().enumerate() {
                b[i] = wi.dot(d);
            }
            let a = inverse * b;
            turns.iter().enumerate().fold((DVec3::ZERO, DVec3::ZERO), |(w, v), (i, t)| (w + t.0 * a[i], v + t.1 * a[i]))
        })
        .collect()
}

// ---- The reply ----------------------------------------------------------

/// A direction for people: rounded, and pointing the "positive" way (its
/// biggest component positive), since a free direction has no sign.
fn direction(d: DVec3) -> DVec3 {
    let biggest = [d.x, d.y, d.z].into_iter().max_by(|a, b| a.abs().total_cmp(&b.abs())).unwrap_or(0.0);
    if biggest < 0.0 { -d } else { d }
}

fn dir_json(d: DVec3) -> Value {
    let d = direction(d);
    json!([round(d.x, 4), round(d.y, 4), round(d.z, 4)])
}

fn mm_json(p: DVec3) -> Value {
    json!([round(p.x, 3), round(p.y, 3), round(p.z, 3)])
}

fn dir_text(d: DVec3) -> String {
    dir_json(d).to_string()
}

fn mm_text(p: DVec3) -> String {
    mm_json(p).to_string()
}

/// "translates along [1,0,0]; turns about [0,0,1] through [5,0,0] mm".
fn freedom_text(f: &Freedom) -> String {
    let mut parts = Vec::new();
    match f.translations.as_slice() {
        [] => {}
        [t] => parts.push(format!("translates along {}", dir_text(*t))),
        [a, b] => parts.push(format!("translates in the plane normal to {}", dir_text(a.cross(*b)))),
        _ => parts.push("translates in any direction".to_string()),
    }
    if f.rotations.len() == 3 {
        parts.push(match f.pivot {
            Some(p) => format!("turns about any axis through {} mm (a ball joint)", mm_text(p)),
            None => "turns about any axis".to_string(),
        });
    } else {
        for r in &f.rotations {
            let mut text = format!("turns about {} through {} mm", dir_text(r.axis), mm_text(r.through));
            if r.pitch.abs() >= PITCH_MM_PER_RAD {
                text += &format!(
                    ", advancing {} mm per turn (a screw)",
                    round(r.pitch * std::f64::consts::TAU, 3)
                );
            }
            parts.push(text);
        }
    }
    parts.join("; ")
}

fn constrained_name(s: proto::ConstrainedStatus) -> Option<&'static str> {
    use proto::ConstrainedStatus as S;
    Some(match s {
        S::Under => "under defined",
        S::Fully => "fully defined",
        S::Over => "over defined",
        S::NoSolution => "no solution",
        S::InvalidSolution => "invalid solution",
        S::AutosolveOff => "auto-solve off",
        S::Unknown | S::Unspecified => return None,
    })
}

/// Half the diagonal of the component's box, mm (at least 1): how much
/// translation weighs as much as a radian of turn.
fn size_mm(c: &proto::ComponentInfo) -> f64 {
    let half = c
        .r#box
        .as_ref()
        .map(|b| (vec(&b.max) - vec(&b.min)).length() * MM_PER_M / 2.0)
        .filter(|h| h.is_finite() && *h > 0.0)
        .unwrap_or(10.0);
    half.max(1.0)
}

pub fn reply_json(ids: &mut IdRegistry, request: &proto::InspectMobilityRequest, reply: &proto::InspectMobilityResponse) -> Value {
    // Where everything started, and each component's motions: from its own
    // probes and from the ones that dragged it along.
    let start: HashMap<&[u8], (&proto::ComponentInfo, DVec3)> = reply
        .components
        .iter()
        .chain(&reply.others)
        .filter_map(|m| Some((m.component.as_ref()?.persist_id.as_slice(), (m.component.as_ref()?, vec(&m.center)))))
        .collect();
    let step_mm = request.step * MM_PER_M;
    let step_rad = request.step_angle;
    // (moved, nudged, which nudge, motion)
    let mut motions: Vec<(&[u8], &[u8], (i32, bool), Twist)> = Vec::new();
    for m in &reply.components {
        let Some(c) = &m.component else { continue };
        for probe in &m.probes {
            for moved in &probe.moved {
                let (Some((info, center)), Some(p)) = (start.get(moved.persist_id.as_slice()), &moved.placement) else {
                    continue;
                };
                let t = twist(info_frame(info), placement_frame(p), *center);
                if t.v.length() < MOVED_SHARE * step_mm && t.w.length() < MOVED_SHARE * step_rad {
                    continue;
                }
                motions.push((&moved.persist_id, &c.persist_id, (probe.axis, probe.negative), t));
            }
        }
    }
    let mut twists: HashMap<&[u8], Vec<Twist>> = HashMap::new();
    let mut drags: HashMap<&[u8], Vec<&[u8]>> = HashMap::new();
    let mut dragged_by: HashMap<&[u8], Vec<&[u8]>> = HashMap::new();
    for (i, &(moved, nudged, nudge, t)) in motions.iter().enumerate() {
        if moved != nudged {
            // Dragged along the same way by a nudge along another axis or the
            // other way isn't a response to the nudge: the solver re-solving
            // a mate it holds only within a tolerance, the same snap on every
            // solve (a lock-mated pair, seen 2026-10-25).
            let scale = start.get(moved).map(|(c, _)| size_mm(c)).unwrap_or(10.0);
            let size = |t: &Twist| (t.v / scale).length() + t.w.length();
            let repeated = motions.iter().enumerate().any(|(j, &(m2, _, nudge2, t2))| {
                j != i && m2 == moved && nudge2 != nudge && {
                    let d = Twist { v: t.v - t2.v, w: t.w - t2.w };
                    size(&d) < 0.2 * size(&t)
                }
            });
            if repeated {
                continue;
            }
            push_once(drags.entry(nudged).or_default(), moved);
            push_once(dragged_by.entry(moved).or_default(), nudged);
        }
        twists.entry(moved).or_default().push(t);
    }
    let label = |ids: &mut IdRegistry, pid: &[u8]| {
        let name = start.get(pid).map(|(c, _)| c.name.as_str()).unwrap_or("?");
        match ids.id_for(Kind::Component, pid) {
            Some(id) => format!("{name} ({id})"),
            None => name.to_string(),
        }
    };

    let mut components = Vec::new();
    let mut counts = (0, 0, 0); // fixed, can't move, can move
    let mut not_probed = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for m in &reply.components {
        let Some(c) = &m.component else { continue };
        let pid = c.persist_id.as_slice();
        let mut obj = Map::new();
        obj.insert("id".into(), json!(ids.id_for(Kind::Component, pid)));
        obj.insert("name".into(), json!(c.name));
        if c.subassembly {
            obj.insert("subassembly".into(), json!(true));
        }
        if let Some(s) = constrained_name(m.constrained()) {
            obj.insert("solidworks_status".into(), json!(s));
        }
        if c.fixed {
            counts.0 += 1;
            obj.insert("fixed".into(), json!(true));
            obj.insert("dof".into(), json!(0));
            obj.insert("free".into(), json!("nothing: fixed in place"));
            components.push(Value::Object(obj));
            continue;
        }
        if c.state() == proto::ComponentState::Suppressed {
            obj.insert("free".into(), json!("not checked: suppressed"));
            components.push(Value::Object(obj));
            continue;
        }
        if !m.probed {
            not_probed.push(label(ids, pid));
            obj.insert("free".into(), json!("not checked: ran out of time"));
            components.push(Value::Object(obj));
            continue;
        }

        let f = freedom(twists.get(pid).map(Vec::as_slice).unwrap_or_default(), vec(&m.center) * MM_PER_M, size_mm(c));
        obj.insert("dof".into(), json!(f.dof()));
        obj.insert("translations".into(), json!(f.translations.len()));
        obj.insert("rotations".into(), json!(f.rotations.len()));
        if f.dof() == 0 {
            counts.1 += 1;
            let why = if m.constrained() == proto::ConstrainedStatus::Fully { "its mates fully define it" } else { "its mates hold it in place" };
            obj.insert("free".into(), json!(format!("nothing: {why}")));
        } else {
            counts.2 += 1;
            obj.insert("free".into(), json!(freedom_text(&f)));
            if !f.translations.is_empty() {
                obj.insert("translate_along".into(), json!(f.translations.iter().map(|t| dir_json(*t)).collect::<Vec<_>>()));
            }
            if !f.rotations.is_empty() {
                let rotations: Vec<Value> = f
                    .rotations
                    .iter()
                    .map(|r| {
                        let mut o = json!({ "axis": dir_json(r.axis), "through_mm": mm_json(r.through) });
                        if r.pitch.abs() >= PITCH_MM_PER_RAD {
                            // Along the axis as dir_json shows it.
                            let sign = if direction(r.axis).dot(r.axis) < 0.0 { -1.0 } else { 1.0 };
                            o["pitch_mm_per_turn"] = json!(round(sign * r.pitch * std::f64::consts::TAU, 3));
                        }
                        o
                    })
                    .collect();
                obj.insert("rotate_about".into(), json!(rotations));
            }
        }
        if let Some(others) = drags.get(pid) {
            obj.insert("drags_along".into(), json!(others.iter().map(|o| label(ids, o)).collect::<Vec<_>>()));
        }
        if let Some(others) = dragged_by.get(pid) {
            obj.insert("dragged_by".into(), json!(others.iter().map(|o| label(ids, o)).collect::<Vec<_>>()));
        }
        match m.constrained() {
            proto::ConstrainedStatus::Under if f.dof() == 0 => notes.push(format!(
                "{}: SolidWorks calls it under defined, but no nudge moved it -- it may only move \
                 together with components that weren't probed, or the mates hold it at a dead point.",
                label(ids, pid)
            )),
            proto::ConstrainedStatus::Fully if f.dof() > 0 => notes.push(format!(
                "{}: SolidWorks calls it fully defined (against what it's mated to), yet it moves: \
                 {}.",
                label(ids, pid),
                if dragged_by.contains_key(pid) {
                    "together with the components that drag it"
                } else {
                    "check its mates"
                }
            )),
            proto::ConstrainedStatus::Over | proto::ConstrainedStatus::NoSolution | proto::ConstrainedStatus::InvalidSolution => {
                notes.push(format!(
                    "{}: its mates conflict ({}); the result reflects how SolidWorks resolved them -- \
                     diagnose_model shows the failing mates.",
                    label(ids, pid),
                    constrained_name(m.constrained()).unwrap_or_default()
                ))
            }
            _ => {}
        }
        components.push(Value::Object(obj));
    }

    let mut out = Map::new();
    out.insert(
        "summary".into(),
        json!(format!(
            "{} component{}: {} fixed, {} held in place by mates, {} can still move",
            components.len(),
            if components.len() == 1 { "" } else { "s" },
            counts.0,
            counts.1,
            counts.2
        )),
    );
    out.insert("components".into(), json!(components));
    out.insert(
        "probe".into(),
        json!({
            "step_mm": round(request.step * MM_PER_M, 4),
            "step_deg": round(request.step_angle.to_degrees(), 4),
            "method": "each component nudged +- along and about the assembly X/Y/Z (turns about its \
                       bounding-box center) while SolidWorks solves its mates, then put back; \
                       dof = independent motions it made. Directions are assembly coordinates.",
        }),
    );
    if reply.out_of_time {
        notes.push(format!(
            "Ran out of time before checking {} -- ask about them on their own (components=[...]).",
            not_probed.join(", ")
        ));
    }
    if !reply.restored {
        notes.push(format!(
            "SolidWorks didn't put these back exactly (more than 0.01 mm off; inspect_model shows \
             where they are now): {}. Mates it solves only within a tolerance, like a lock mate \
             between two floating components, can shift a little when it re-solves them.",
            reply.not_restored.join(", ")
        ));
    }
    if !notes.is_empty() {
        out.insert("notes".into(), json!(notes));
    }
    Value::Object(out)
}

fn push_once<'a>(list: &mut Vec<&'a [u8]>, pid: &'a [u8]) {
    if !list.contains(&pid) {
        list.push(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEG: f64 = 1f64.to_radians();

    /// The finite motion "turn by `angle` about `axis` through `point`, then
    /// translate by `shift`" (mm), applied to a component at `origin` (mm),
    /// seen at `center` (mm).
    fn motion(origin: DVec3, center: DVec3, axis: DVec3, point: DVec3, angle: f64, shift: DVec3) -> Twist {
        let turn = DMat3::from_axis_angle(axis.normalize(), angle);
        let from = (origin / MM_PER_M, DMat3::IDENTITY);
        let to = ((turn * (origin - point) + point + shift) / MM_PER_M, turn);
        twist(from, to, center / MM_PER_M)
    }

    fn shift(d: DVec3) -> Twist {
        motion(DVec3::ZERO, DVec3::ZERO, DVec3::Z, DVec3::ZERO, 0.0, d)
    }

    fn turn(center: DVec3, axis: DVec3, point: DVec3, angle: f64) -> Twist {
        motion(DVec3::ZERO, center, axis, point, angle, DVec3::ZERO)
    }

    fn near(a: DVec3, b: DVec3, tol: f64) -> bool {
        (a - b).length() < tol
    }

    fn same_line(a: DVec3, b: DVec3) -> bool {
        a.normalize().cross(b.normalize()).length() < 1e-6
    }

    #[test]
    fn eigen_of_a_symmetric_matrix() {
        let e = symmetric_eigen(vec![vec![2.0, 1.0], vec![1.0, 2.0]]);
        assert!((e[0].0 - 3.0).abs() < 1e-12 && (e[1].0 - 1.0).abs() < 1e-12, "{e:?}");
        assert!((e[0].1[0].abs() - 0.5f64.sqrt()).abs() < 1e-12 && (e[0].1[0] - e[0].1[1]).abs() < 1e-12, "{e:?}");
    }

    #[test]
    fn a_free_component_has_six() {
        let c = DVec3::new(5.0, -100.0, 0.0);
        let mut t = Vec::new();
        for axis in [DVec3::X, DVec3::Y, DVec3::Z] {
            for s in [1.0, -1.0] {
                t.push(shift(axis * s));
                t.push(turn(c, axis, c, s * DEG));
            }
        }
        let f = freedom(&t, c, 22.0);
        assert_eq!((f.translations.len(), f.rotations.len()), (3, 3), "{f:?}");
        assert_eq!(freedom_text(&f), "translates in any direction; turns about any axis");
    }

    #[test]
    fn a_face_on_a_plane_slides_and_spins() {
        let c = DVec3::new(5.0, -200.0, 0.0);
        // As SolidWorks answered: the turns about X / Y it can't make leave a
        // sub-micron x drift; about Z it turns.
        let t = vec![
            shift(DVec3::X),
            shift(-DVec3::Y),
            turn(c, DVec3::Z, c, DEG),
            turn(c, DVec3::Z, c, -DEG),
            shift(DVec3::new(0.0008, 0.0, 0.0)),
        ];
        let f = freedom(&t, c, 22.0);
        assert_eq!((f.translations.len(), f.rotations.len()), (2, 1), "{f:?}");
        assert!(same_line(f.rotations[0].axis, DVec3::Z) && near(f.rotations[0].through, c, 1e-6), "{f:?}");
        assert_eq!(
            freedom_text(&f),
            "translates in the plane normal to [0.0,0.0,1.0]; turns about [0.0,0.0,1.0] through [5.0,-200.0,0.0] mm"
        );
    }

    #[test]
    fn free_directions_come_out_along_the_assembly_axes() {
        // A skewed basis of all of space, and of the XY plane.
        let skew = [DVec3::new(1.0, 1.0, 0.0), DVec3::new(0.0, 1.0, 1.0), DVec3::new(1.0, 0.0, 1.0)];
        let got = snap(&skew);
        assert!(near(got[0], DVec3::X, 1e-12) && near(got[1], DVec3::Y, 1e-12) && near(got[2], DVec3::Z, 1e-12), "{got:?}");
        let got = snap(&[DVec3::new(1.0, 1.0, 0.0), DVec3::new(1.0, -2.0, 0.0)]);
        assert!(near(got[0], DVec3::X, 1e-12) && near(got[1], DVec3::Y, 1e-12), "{got:?}");
        // A tilted plane: the axes' shadows on it.
        let got = snap(&[DVec3::new(1.0, 0.0, 1.0)]);
        assert_eq!(got.len(), 1);
        assert!(same_line(got[0], DVec3::new(1.0, 0.0, 1.0)), "{got:?}");
        // Turns about skewed axes through one point: X / Y / Z through it.
        let c = DVec3::new(10.0, 0.0, 0.0);
        let t: Vec<Twist> = skew.iter().map(|a| turn(c, *a, DVec3::ZERO, DEG)).collect();
        let f = freedom(&t, c, 10.0);
        let axes: Vec<DVec3> = f.rotations.iter().map(|r| r.axis).collect();
        assert!(same_line(axes[0], DVec3::X) && same_line(axes[1], DVec3::Y) && same_line(axes[2], DVec3::Z), "{f:?}");
        assert!(near(f.pivot.unwrap(), DVec3::ZERO, 1e-6), "{f:?}");
    }

    #[test]
    fn a_hinge_turns_about_its_own_axis() {
        // The pin through (0, 0, 0) along Z, the component's center 10 mm off it.
        let c = DVec3::new(10.0, 0.0, 0.0);
        let t = vec![turn(c, DVec3::Z, DVec3::ZERO, DEG), turn(c, DVec3::Z, DVec3::ZERO, -DEG)];
        let f = freedom(&t, c, 15.0);
        assert_eq!((f.translations.len(), f.rotations.len()), (0, 1), "{f:?}");
        let r = &f.rotations[0];
        assert!(same_line(r.axis, DVec3::Z), "{f:?}");
        // On the pin's axis.
        let off = r.through - DVec3::Z * r.through.z;
        assert!(off.length() < 1e-6, "{f:?}");
        assert!(r.pitch.abs() < PITCH_MM_PER_RAD, "{f:?}");
    }

    #[test]
    fn a_ball_joint_turns_about_its_center() {
        let c = DVec3::new(10.0, 5.0, 0.0);
        let q = DVec3::ZERO;
        let t: Vec<Twist> = [DVec3::X, DVec3::Y, DVec3::Z].iter().map(|a| turn(c, *a, q, DEG)).collect();
        let f = freedom(&t, c, 12.0);
        assert_eq!((f.translations.len(), f.rotations.len()), (0, 3), "{f:?}");
        assert!(near(f.pivot.unwrap(), q, 1e-6), "{f:?}");
        let text = freedom_text(&f);
        assert!(text.contains("turns about any axis through [0.0,0.0,0.0] mm (a ball joint)"), "{text}");
    }

    #[test]
    fn a_screw_advances_as_it_turns() {
        // 2 mm per turn along Z.
        let c = DVec3::new(0.0, 0.0, 5.0);
        let pitch = 2.0 / std::f64::consts::TAU;
        let t = vec![
            motion(DVec3::ZERO, c, DVec3::Z, c, DEG, DVec3::Z * pitch * DEG),
            motion(DVec3::ZERO, c, DVec3::Z, c, -DEG, -DVec3::Z * pitch * DEG),
        ];
        let f = freedom(&t, c, 5.0);
        assert_eq!((f.translations.len(), f.rotations.len()), (0, 1), "{f:?}");
        assert!((f.rotations[0].pitch.abs() * std::f64::consts::TAU - 2.0).abs() < 1e-6, "{f:?}");
        let text = freedom_text(&f);
        assert!(text.ends_with("advancing 2 mm per turn (a screw)"), "{text}");
    }

    #[test]
    fn concentric_slides_and_spins_noise_aside() {
        let c = DVec3::new(115.0, 0.0, 0.0);
        let t = vec![
            shift(DVec3::X),
            shift(-DVec3::X),
            turn(c, DVec3::X, c, DEG),
            turn(c, DVec3::X, c, -DEG),
            // What the blocked turns about Y / Z left: a tiny slide along X.
            shift(DVec3::new(-0.0023, 0.0, 0.0)),
            // Solver noise.
            shift(DVec3::new(0.0, 1e-7, 0.0)),
        ];
        let f = freedom(&t, c, 16.0);
        assert_eq!((f.translations.len(), f.rotations.len()), (1, 1), "{f:?}");
        assert!(same_line(f.translations[0], DVec3::X) && same_line(f.rotations[0].axis, DVec3::X), "{f:?}");
        assert_eq!(
            freedom_text(&f),
            "translates along [1.0,0.0,0.0]; turns about [1.0,0.0,0.0] through [115.0,0.0,0.0] mm"
        );
        assert_eq!(freedom(&[shift(DVec3::new(1e-4, 0.0, 0.0))], c, 16.0).dof(), 0);
    }

    fn v(x: f64, y: f64, z: f64) -> Option<proto::Vec3> {
        Some(proto::Vec3 { x, y, z })
    }

    fn info(pid: &[u8], name: &str, fixed: bool, o: [f64; 3]) -> proto::ComponentInfo {
        proto::ComponentInfo {
            persist_id: pid.to_vec(),
            name: name.into(),
            fixed,
            origin: v(o[0] / MM_PER_M, o[1] / MM_PER_M, o[2] / MM_PER_M),
            x_axis: v(1.0, 0.0, 0.0),
            y_axis: v(0.0, 1.0, 0.0),
            z_axis: v(0.0, 0.0, 1.0),
            ..Default::default()
        }
    }

    fn moved(pid: &[u8], o: [f64; 3]) -> proto::MovedComponent {
        proto::MovedComponent {
            persist_id: pid.to_vec(),
            placement: Some(proto::ComponentPlacement {
                origin: v(o[0] / MM_PER_M, o[1] / MM_PER_M, o[2] / MM_PER_M),
                x_axis: v(1.0, 0.0, 0.0),
                y_axis: v(0.0, 1.0, 0.0),
                z_axis: v(0.0, 0.0, 1.0),
            }),
        }
    }

    fn probe(axis: i32, negative: bool, moved: Vec<proto::MovedComponent>) -> proto::MobilityProbe {
        proto::MobilityProbe { axis, negative, refused: false, moved }
    }

    fn mobility(
        c: proto::ComponentInfo,
        constrained: proto::ConstrainedStatus,
        probes: Vec<proto::MobilityProbe>,
    ) -> proto::ComponentMobility {
        proto::ComponentMobility {
            probed: !c.fixed,
            center: c.origin.clone(),
            component: Some(c),
            constrained: constrained.into(),
            probes,
        }
    }

    #[test]
    fn request_checks_params() {
        let resolve = |id: &str| -> Result<Vec<u8>, McpError> { Ok(id.as_bytes().to_vec()) };
        let r = request(&InspectMobilityParams::default(), resolve).unwrap();
        assert!(r.component_persist_ids.is_empty());
        assert!((r.step - 0.001).abs() < 1e-15 && (r.step_angle - DEG).abs() < 1e-15);
        let p = |components: Option<Vec<&str>>, step_mm, step_deg| InspectMobilityParams {
            components: components.map(|c| c.into_iter().map(String::from).collect()),
            step_mm,
            step_deg,
        };
        assert!(request(&p(Some(vec!["face:1"]), None, None), resolve).is_err());
        assert!(request(&p(None, Some(0.0), None), resolve).is_err());
        assert!(request(&p(None, None, Some(90.0)), resolve).is_err());
        let r = request(&p(Some(vec![" component:2 "]), Some(0.5), Some(2.0)), resolve).unwrap();
        assert_eq!(r.component_persist_ids, vec![b"component:2".to_vec()]);
        assert!((r.step - 0.0005).abs() < 1e-15);
    }

    #[test]
    fn a_follower_gets_the_motion_its_slider_drags_it_through() {
        use proto::ConstrainedStatus as S;
        let mut ids = IdRegistry::default();
        let request = request(&InspectMobilityParams::default(), |id| Ok(id.as_bytes().to_vec())).unwrap();
        // The slider slides along X and drags the follower; the follower on
        // its own only slides along Y (SolidWorks won't push the slider).
        let reply = proto::InspectMobilityResponse {
            components: vec![
                mobility(info(b"b", "base-1", true, [0.0; 3]), S::Fully, vec![]),
                mobility(
                    info(b"s", "slider-1", false, [0.0, 100.0, 0.0]),
                    S::Under,
                    vec![
                        probe(0, false, vec![moved(b"s", [1.0, 100.0, 0.0]), moved(b"f", [1.0, 150.0, 0.0])]),
                        probe(0, true, vec![moved(b"s", [-1.0, 100.0, 0.0]), moved(b"f", [-1.0, 150.0, 0.0])]),
                        probe(4, false, vec![moved(b"s", [0.0008, 100.0, 0.0]), moved(b"p", [0.02, -450.0, 0.0])]),
                    ],
                ),
                mobility(
                    info(b"f", "follower-1", false, [0.0, 150.0, 0.0]),
                    S::Under,
                    vec![
                        probe(1, false, vec![moved(b"f", [0.0, 151.0, 0.0])]),
                        probe(1, true, vec![moved(b"f", [0.0, 149.0, 0.0])]),
                    ],
                ),
                // Every solve also snaps a lock-mated pair: by micrometers, or
                // by the same 20 um whatever the nudge -- not freedom.
                mobility(
                    info(b"x", "full-1", false, [0.0, 200.0, 0.0]),
                    S::Under,
                    vec![
                        probe(0, false, vec![moved(b"p", [0.003, -450.0, 0.0])]),
                        probe(1, true, vec![moved(b"p", [0.02, -450.0, 0.0])]),
                    ],
                ),
                mobility(info(b"p", "pair-1", false, [0.0, -450.0, 0.0]), S::Fully, vec![]),
            ],
            others: vec![],
            restored: true,
            not_restored: vec![],
            out_of_time: false,
        };
        let json = reply_json(&mut ids, &request, &reply);
        let text = json.to_string();
        let cs = json["components"].as_array().unwrap();
        assert_eq!(json["summary"], "5 components: 1 fixed, 2 held in place by mates, 2 can still move", "{text}");
        assert!(cs[3].get("drags_along").is_none() && cs[4].get("dragged_by").is_none(), "{text}");
        assert_eq!(cs[4]["free"], "nothing: its mates fully define it", "{text}");
        assert_eq!((cs[0]["dof"].as_u64(), cs[0]["fixed"].as_bool()), (Some(0), Some(true)), "{text}");
        assert_eq!(cs[1]["dof"], 1, "{text}");
        assert_eq!(cs[1]["free"], "translates along [1.0,0.0,0.0]", "{text}");
        assert_eq!(cs[1]["drags_along"], json!(["follower-1 (component:3)"]), "{text}");
        assert_eq!((cs[2]["dof"].as_u64(), cs[2]["translations"].as_u64()), (Some(2), Some(2)), "{text}");
        assert_eq!(cs[2]["free"], "translates in the plane normal to [0.0,0.0,1.0]", "{text}");
        assert_eq!(cs[2]["dragged_by"], json!(["slider-1 (component:2)"]), "{text}");
        assert_eq!(cs[3]["dof"], 0, "{text}");
        let note = json["notes"][0].as_str().unwrap();
        assert!(note.starts_with("full-1 (component:4): SolidWorks calls it under defined"), "{text}");
    }
}
