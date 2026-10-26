//! Motion ranges: the sweep_mate tool (a distance or angle mate driven
//! through a range of values, a limit mate over its limits by default) and
//! the collision report it shares with move_component's check_collisions.
//!
//! The add-in runs Interference Detection before the motion and at every
//! step and hands back each step's interferences; this module sorts them
//! into the pairs of components that come to overlap along the way (first
//! contact, how far it lasts, the biggest overlap), apart from the ones
//! that already overlapped before anything moved.

use std::collections::BTreeMap;

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::assembly;
use crate::delete;
use crate::edit;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, mm, mm3, point_mm, round};

/// Default number of steps of a sweep (21 values).
const DEFAULT_STEPS: u32 = 20;
const MAX_STEPS: u32 = 360;
/// How long the add-in may keep stepping (a sweep position -- set the value,
/// rebuild, check -- took ~0.8 s, a move step with its check ~0.5 s, seen
/// 2026-10-25; the gRPC call gives up after 60 s).
pub const TIME_BUDGET_S: f64 = 45.0;
/// Pairs listed per reply; the rest are counted.
const LISTED: usize = 12;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

// ---- Collisions along a motion -------------------------------------------------

/// Which components an interference is between, order-independent.
fn pair_key(i: &proto::Interference) -> Vec<Vec<u8>> {
    let mut key: Vec<Vec<u8>> = i
        .component_persist_ids
        .iter()
        .zip(&i.component_names)
        .map(|(pid, name)| if pid.is_empty() { [b"name:", name.as_bytes()].concat() } else { pid.clone() })
        .collect();
    key.sort();
    key
}

fn pair_names(ids: &mut IdRegistry, i: &proto::Interference) -> String {
    let names: Vec<String> = i
        .component_names
        .iter()
        .enumerate()
        .map(|(k, name)| {
            let pid = i.component_persist_ids.get(k).map(Vec::as_slice).unwrap_or_default();
            match ids.id_for(Kind::Component, pid) {
                Some(id) => format!("{name} ({id})"),
                None => name.clone(),
            }
        })
        .collect();
    names.join(" and ")
}

/// A pair that overlaps at some step, and where.
struct Collision {
    names: String,
    at: Vec<usize>,  // indexes into the steps
    max_volume: f64,
    max_at: usize,
    max_box: Option<proto::Box3>,
}

/// Consecutive runs of step indexes: [2, 3, 4, 7] -> [(2, 4), (7, 7)].
fn runs(at: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &k in at {
        match out.last_mut() {
            Some((_, end)) if *end + 1 == k => *end = k,
            _ => out.push((k, k)),
        }
    }
    out
}

/// The collision part of a motion's reply: pairs of components that came
/// to overlap, with `label` naming a step's position along the motion
/// ("15 deg", "12.5 mm along") and `start` the position before it.
pub fn collisions_text(
    ids: &mut IdRegistry,
    start: &[proto::Interference],
    steps: &[proto::MotionStep],
    label: impl Fn(&proto::MotionStep) -> String,
    start_label: &str,
) -> String {
    let before: Vec<Vec<Vec<u8>>> = start.iter().map(pair_key).collect();
    let mut found: BTreeMap<Vec<Vec<u8>>, Collision> = BTreeMap::new();
    let mut order: Vec<Vec<Vec<u8>>> = Vec::new();
    for (k, step) in steps.iter().enumerate() {
        for i in &step.interferences {
            let key = pair_key(i);
            if before.contains(&key) {
                continue;
            }
            let entry = found.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                Collision { names: pair_names(ids, i), at: Vec::new(), max_volume: -1.0, max_at: k, max_box: None }
            });
            if entry.at.last() != Some(&k) {
                entry.at.push(k);
            }
            if i.volume > entry.max_volume {
                entry.max_volume = i.volume;
                entry.max_at = k;
                entry.max_box = i.r#box;
            }
        }
    }

    let mut text = String::new();
    if found.is_empty() {
        text += &format!(
            "\nCollisions: none -- no components came to overlap at any of the {} position{} checked.",
            steps.len(),
            if steps.len() == 1 { "" } else { "s" }
        );
    } else {
        text += "\nCollisions (components that came to overlap):";
        for key in order.iter().take(LISTED) {
            let c = &found[key];
            let ranges: Vec<String> = runs(&c.at)
                .iter()
                .map(|&(a, b)| {
                    if a == b {
                        format!("at {}", label(&steps[a]))
                    } else {
                        format!("from {} to {}", label(&steps[a]), label(&steps[b]))
                    }
                })
                .collect();
            let first = c.at[0];
            let contact = if first == 0 {
                format!("already at the first position checked ({})", label(&steps[0]))
            } else {
                format!("between {} and {}", label(&steps[first - 1]), label(&steps[first]))
            };
            text += &format!(
                "\n- {}: overlap {}; first contact {}; most {} mm³ at {}",
                c.names,
                ranges.join(" and "),
                contact,
                mm3(c.max_volume.max(0.0)),
                label(&steps[c.max_at])
            );
            if let Some(b) = &c.max_box
                && let (Some(lo), Some(hi)) = (&b.min, &b.max)
            {
                text += &format!(", in {} .. {} mm", point_mm(lo), point_mm(hi));
            }
            text.push('.');
        }
        if order.len() > LISTED {
            text += &format!("\n- ... and {} more pairs.", order.len() - LISTED);
        }
        text += "\nBetween two positions checked the motion isn't seen: use more steps (or a narrower \
                 range) to find the contact more closely.";
    }
    if !start.is_empty() {
        let mut pairs: Vec<String> = start
            .iter()
            .take(LISTED)
            .map(|i| format!("{} ({} mm³)", pair_names(ids, i), mm3(i.volume)))
            .collect();
        if start.len() > LISTED {
            pairs.push(format!("{} more", start.len() - LISTED));
        }
        text += &format!(
            "\nAlready overlapping {start_label} (left out above): {}.",
            pairs.join("; ")
        );
    }
    if steps.iter().any(|s| s.truncated) {
        text += "\nSome positions had more interferences than the add-in lists; only the first ones are counted.";
    }
    text
}

// ---- sweep_mate --------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SweepMateParams {
    /// The mate to drive: a distance or angle mate (limit mate or not), as a "feature:N" id
    /// from inspect_model what="mates" or its name, e.g. "LimitAngle1".
    pub mate: String,
    /// Distance mates: where the sweep starts, in millimeters. Default: a limit mate's
    /// min_mm; for a plain distance mate its current distance.
    #[serde(default)]
    pub from_mm: Option<f64>,
    /// Distance mates: where it ends, in millimeters. Default: a limit mate's max_mm; a plain
    /// distance mate needs it.
    #[serde(default)]
    pub to_mm: Option<f64>,
    /// Angle mates: where the sweep starts, 0 to 180 degrees. Default: a limit mate's
    /// min_deg; for a plain angle mate its current angle.
    #[serde(default)]
    pub from_deg: Option<f64>,
    /// Angle mates: where it ends, 0 to 180 degrees. Default: a limit mate's max_deg; a plain
    /// angle mate needs it.
    #[serde(default)]
    pub to_deg: Option<f64>,
    /// How many equal steps the range is cut into; the values at both ends and between them
    /// are checked (steps + 1 positions). Default 20. 1..360. Each position takes about a
    /// second (a rebuild and an interference check); the call stops after 45 s and says how
    /// far it got.
    #[serde(default)]
    pub steps: Option<u32>,
    /// true: stop at the first position where components come to overlap, and go back to
    /// the position before it ("how far can it open before it hits?"). Default false: the
    /// whole range is checked.
    #[serde(default)]
    pub stop_at_collision: bool,
    /// Afterwards, put the mate's value and every component back where they were. Default
    /// true: a check that changes nothing. false keeps the mate at the end of the sweep (or
    /// where stop_at_collision stopped it).
    #[serde(default)]
    pub restore: Option<bool>,
}

pub fn sweep_request(ids: &IdRegistry, p: &SweepMateParams) -> Result<proto::SweepMateRequest, McpError> {
    let mm_range = p.from_mm.is_some() || p.to_mm.is_some();
    let deg_range = p.from_deg.is_some() || p.to_deg.is_some();
    if mm_range && deg_range {
        return Err(invalid(
            "give the range in millimeters (from_mm / to_mm, distance mates) or degrees (from_deg / \
             to_deg, angle mates), not both",
        ));
    }
    for (name, v, top) in [
        ("from_mm", p.from_mm, f64::INFINITY),
        ("to_mm", p.to_mm, f64::INFINITY),
        ("from_deg", p.from_deg, 180.0),
        ("to_deg", p.to_deg, 180.0),
    ] {
        if let Some(v) = v
            && !(v.is_finite() && 0.0 <= v && v <= top)
        {
            return Err(invalid(format!("{name} must be 0 to {top}, got {v}")));
        }
    }
    let steps = p.steps.unwrap_or(DEFAULT_STEPS);
    if !(1..=MAX_STEPS).contains(&steps) {
        return Err(invalid(format!("steps must be 1..{MAX_STEPS}, got {steps}")));
    }
    let si = |mm: Option<f64>, deg: Option<f64>| mm.map(|v| v / MM_PER_M).or(deg.map(f64::to_radians));
    Ok(proto::SweepMateRequest {
        mate: Some(edit::feature_target(ids, &p.mate, "mate")?),
        from: si(p.from_mm, p.from_deg),
        to: si(p.to_mm, p.to_deg),
        angular: deg_range,
        steps: steps as i32,
        stop_at_collision: p.stop_at_collision,
        restore: p.restore.unwrap_or(true),
        time_budget: TIME_BUDGET_S,
    })
}

/// A mate value in words: "12.5 mm", "30 deg".
fn value_text(angular: bool, v: f64) -> String {
    if angular {
        format!("{} deg", round(v.to_degrees(), 4))
    } else {
        format!("{} mm", mm(v))
    }
}

pub fn sweep_reply_text(ids: &mut IdRegistry, request: &proto::SweepMateRequest, reply: &proto::SweepMateResponse) -> String {
    let Some(mate) = &reply.mate else {
        return "SolidWorks didn't report the mate".into();
    };
    let angular = mate.r#type() == proto::MateType::Angle;
    let mate_id = ids.id_for(Kind::Feature, &mate.persist_id).unwrap_or_default();
    let v = |x: f64| value_text(angular, x);
    let current = mate.dimension.as_ref().map_or(0.0, |d| d.value);
    let steps = request.steps.max(1) as usize;

    let mut text = format!(
        "Swept {} ({mate_id}, {}{}) from {} to {} in {} step{} (every {}; it was at {}).",
        mate.name,
        assembly::mate_type_name(mate.r#type()),
        assembly::limits_text(mate),
        v(reply.from),
        v(reply.to),
        steps,
        if steps == 1 { "" } else { "s" },
        v((reply.to - reply.from).abs() / steps as f64),
        v(current)
    );
    if reply.refused_step > 0 {
        let at = reply.from + (reply.to - reply.from) * (reply.refused_step - 1) as f64 / steps as f64;
        text += &format!("\nStopped at {}: {}.", v(at), reply.failure);
    }
    if reply.out_of_time {
        text += &format!(
            "\nOut of time after {} of {} positions; the rest of the range wasn't checked (fewer steps \
             or a narrower range).",
            reply.steps.len(),
            steps + 1
        );
    }
    text += &collisions_text(ids, &reply.start_interferences, &reply.steps, |s| v(s.value), "before the sweep");
    if reply.backed_off {
        let back = match reply.collision_step {
            1 => format!("where it was ({})", v(current)),
            k => reply.steps.get(k as usize - 2).map_or_else(|| v(current), |s| v(s.value)),
        };
        text += &format!("\nStopped at the first collision and went back to {back}.");
    }

    let moved: Vec<String> = reply
        .moved
        .iter()
        .filter_map(|m| m.after.as_ref())
        .map(|c| format!("{} ({})", c.name, ids.id_for(Kind::Component, &c.persist_id).unwrap_or_default()))
        .collect();
    if !moved.is_empty() {
        text += &format!("\nMoved by the sweep: {}.", moved.join(", "));
    } else if reply.steps.len() > 1 {
        text += "\nNothing moved: the mate's components may be fixed, or held by other mates.";
    }

    if request.restore {
        if reply.restored {
            text += &format!("\nRestored: {} is back at {} and every component where it was.", mate.name, v(current));
        } else {
            text += &format!(
                "\nNot back where they were: {} -- they're where the sweep left them.",
                reply.not_restored.join(", ")
            );
        }
    } else {
        let end = reply.steps.iter().rev().find(|s| !reply.backed_off || s.step < reply.collision_step);
        text += &format!(
            "\n{} is left at {} (restore=true would have put it back).",
            mate.name,
            end.map_or_else(|| v(current), |s| v(s.value))
        );
        if !moved.is_empty() {
            text += " Earlier inspect_model coordinates of the moved components are out of date.";
        }
    }
    text += &delete::problems_text(&reply.new_errors);
    text
}

/// Where step `s` of a move_component motion is: that fraction of the
/// requested translation / turn.
pub fn move_step_label(request: &proto::MoveComponentRequest, s: &proto::MotionStep) -> String {
    let f = s.step as f64 / request.steps.max(1) as f64;
    let t = request.translation.as_ref().map_or(0.0, |t| (t.x * t.x + t.y * t.y + t.z * t.z).sqrt());
    let mut parts = Vec::new();
    if t > 0.0 {
        parts.push(format!("{} mm", mm(f * t)));
    }
    if request.rotation_angle != 0.0 {
        parts.push(format!("{} deg", round((f * request.rotation_angle).to_degrees().abs(), 3)));
    }
    let at = s.placement.as_ref().and_then(|p| p.origin.as_ref()).map(point_mm);
    match at {
        Some(origin) => format!("{} (step {}, origin at {} mm)", parts.join(" + "), s.step, origin),
        None => format!("{} (step {})", parts.join(" + "), s.step),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlap(a: &[u8], b: &[u8], volume_mm3: f64) -> proto::Interference {
        proto::Interference {
            component_persist_ids: vec![a.to_vec(), b.to_vec()],
            component_names: vec![
                String::from_utf8_lossy(a).into_owned(),
                String::from_utf8_lossy(b).into_owned(),
            ],
            volume: volume_mm3 * 1e-9,
            ..Default::default()
        }
    }

    fn step(k: i32, value: f64, interferences: Vec<proto::Interference>) -> proto::MotionStep {
        proto::MotionStep { step: k, value, interferences, ..Default::default() }
    }

    #[test]
    fn collisions_by_pair_with_first_contact() {
        let mut ids = IdRegistry::default();
        let start = vec![overlap(b"base", b"pin", 3.0)];
        let steps = vec![
            step(1, 0.0, vec![overlap(b"pin", b"base", 3.0)]),
            step(2, 10.0, vec![overlap(b"pin", b"base", 3.0)]),
            step(3, 20.0, vec![overlap(b"base", b"pin", 3.0), overlap(b"lid", b"base", 50.0)]),
            step(4, 30.0, vec![overlap(b"base", b"lid", 120.0)]),
            step(5, 40.0, vec![]),
            step(6, 50.0, vec![overlap(b"lid", b"base", 10.0)]),
        ];
        let text = collisions_text(&mut ids, &start, &steps, |s| format!("{} deg", s.value), "before");
        assert!(text.contains("- lid (component:1) and base (component:2): overlap from 20 deg to 30 deg and at 50 deg"), "{text}");
        assert!(text.contains("first contact between 10 deg and 20 deg"), "{text}");
        assert!(text.contains("most 120"), "{text}");
        assert!(!text.contains("- base (component:2) and pin"), "the pair overlapping before isn't a collision: {text}");
        assert!(text.contains("Already overlapping before (left out above): base (component:2) and pin (component:3) (3 mm³)"), "{text}");
    }

    #[test]
    fn no_collisions() {
        let mut ids = IdRegistry::default();
        let steps = vec![step(1, 0.0, vec![]), step(2, 1.0, vec![])];
        let text = collisions_text(&mut ids, &[], &steps, |s| format!("{}", s.value), "before");
        assert!(text.contains("none") && text.contains("2 positions"), "{text}");
        let text = collisions_text(&mut ids, &[], &[step(1, 0.0, vec![overlap(b"a", b"b", 1.0)])], |_| "x".into(), "before");
        assert!(text.contains("already at the first position checked"), "{text}");
    }

    #[test]
    fn sweep_request_checks_the_range() {
        let ids = IdRegistry::default();
        let p = SweepMateParams { mate: "LimitAngle1".into(), ..Default::default() };
        let r = sweep_request(&ids, &p).unwrap();
        assert_eq!((r.from, r.to, r.steps, r.restore, r.angular), (None, None, 20, true, false));
        let p = SweepMateParams { mate: "Distance1".into(), to_mm: Some(40.0), steps: Some(8), restore: Some(false), ..Default::default() };
        let r = sweep_request(&ids, &p).unwrap();
        assert_eq!((r.from, r.to, r.steps, r.restore), (None, Some(0.04), 8, false));
        let p = SweepMateParams { mate: "Angle1".into(), from_deg: Some(10.0), to_deg: Some(90.0), ..Default::default() };
        let r = sweep_request(&ids, &p).unwrap();
        assert!(r.angular && (r.to.unwrap() - std::f64::consts::FRAC_PI_2).abs() < 1e-15);
        for bad in [
            SweepMateParams { mate: "x".into(), from_mm: Some(1.0), to_deg: Some(9.0), ..Default::default() },
            SweepMateParams { mate: "x".into(), to_deg: Some(200.0), ..Default::default() },
            SweepMateParams { mate: "x".into(), to_mm: Some(-1.0), ..Default::default() },
            SweepMateParams { mate: "x".into(), steps: Some(0), ..Default::default() },
            SweepMateParams { mate: "face:3".into(), ..Default::default() },
            SweepMateParams { mate: " ".into(), ..Default::default() },
        ] {
            assert!(sweep_request(&ids, &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sweep_text() {
        let mut ids = IdRegistry::default();
        let request = sweep_request(&ids, &SweepMateParams { mate: "LimitAngle1".into(), steps: Some(2), ..Default::default() }).unwrap();
        let reply = proto::SweepMateResponse {
            mate: Some(proto::MateInfo {
                persist_id: b"m".to_vec(),
                name: "LimitAngle1".into(),
                r#type: proto::MateType::Angle.into(),
                limits: Some(proto::MateLimits { min: 0.0, max: std::f64::consts::FRAC_PI_2 }),
                dimension: Some(proto::DimensionInfo { value: 0.1, ..Default::default() }),
                ..Default::default()
            }),
            from: 0.0,
            to: std::f64::consts::FRAC_PI_2,
            steps: vec![
                step(1, 0.0, vec![]),
                step(2, std::f64::consts::FRAC_PI_4, vec![overlap(b"lid", b"box", 7.0)]),
                step(3, std::f64::consts::FRAC_PI_2, vec![overlap(b"lid", b"box", 9.0)]),
            ],
            collision_step: 2,
            restored: true,
            ..Default::default()
        };
        let text = sweep_reply_text(&mut ids, &request, &reply);
        assert!(text.starts_with("Swept LimitAngle1 (feature:1, angle, limits 0 .. 90 deg) from 0 deg to 90 deg in 2 steps (every 45 deg"), "{text}");
        assert!(text.contains("first contact between 0 deg and 45 deg"), "{text}");
        assert!(text.contains("Restored: LimitAngle1 is back at"), "{text}");
    }
}
