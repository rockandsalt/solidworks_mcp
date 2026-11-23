//! check_clearance: component pairs closer than a clearance. SolidWorks'
//! Clearance Verification has no API, so the pairs whose boxes are closer than
//! the clearance (a box gap is never more than the real gap) are measured one
//! by one with the Measure RPC, which gives the minimum distance between two
//! components.

use std::collections::{HashMap, HashSet};

use rmcp::schemars;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, round};

/// At most this many pairs are measured per call (one Measure each).
pub const MAX_PAIRS: usize = 300;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CheckClearanceParams {
    /// Report component pairs closer than this, in mm (e.g. 2 = parts must stay 2 mm apart).
    pub clearance_mm: f64,
    /// Only pairs with at least one of these components ("component:3", from inspect_model);
    /// defaults to every pair.
    #[serde(default)]
    pub components: Vec<String>,
    /// Also check hidden components. Defaults to false.
    #[serde(default)]
    pub include_hidden: bool,
}

/// Two components worth measuring: indexes into the component list, and the
/// gap between their boxes in meters.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub a: usize,
    pub b: usize,
    pub box_gap: f64,
}

/// The distance between two boxes (0 when they touch or overlap).
fn box_gap(a: &proto::Box3, b: &proto::Box3) -> Option<f64> {
    let (amin, amax, bmin, bmax) = (
        a.min.as_ref()?,
        a.max.as_ref()?,
        b.min.as_ref()?,
        b.max.as_ref()?,
    );
    let axis = |a0: f64, a1: f64, b0: f64, b1: f64| (a0 - b1).max(b0 - a1).max(0.0);
    let d = [
        axis(amin.x, amax.x, bmin.x, bmax.x),
        axis(amin.y, amax.y, bmin.y, bmax.y),
        axis(amin.z, amax.z, bmin.z, bmax.z),
    ];
    Some(d.iter().map(|v| v * v).sum::<f64>().sqrt())
}

/// Hidden itself or inside a hidden subassembly.
fn hidden(c: &proto::ComponentInfo, by_pid: &HashMap<&[u8], &proto::ComponentInfo>) -> bool {
    let mut at = Some(c);
    for _ in 0..64 {
        let Some(component) = at else { break };
        if component.hidden {
            return true;
        }
        at = by_pid.get(component.parent_persist_id.as_slice()).copied();
    }
    false
}

/// The part components (not subassemblies, not suppressed, shown unless
/// `include_hidden`) whose boxes are closer than `clearance` (meters), closest
/// first. With `only`, pairs with at least one of those persist ids.
pub fn candidates(
    components: &[proto::ComponentInfo],
    clearance: f64,
    only: &HashSet<Vec<u8>>,
    include_hidden: bool,
) -> Vec<Candidate> {
    let by_pid: HashMap<&[u8], &proto::ComponentInfo> = components
        .iter()
        .map(|c| (c.persist_id.as_slice(), c))
        .collect();
    let parts: Vec<usize> = (0..components.len())
        .filter(|&i| {
            let c = &components[i];
            !c.subassembly
                && c.state() != proto::ComponentState::Suppressed
                && c.r#box.is_some()
                && (include_hidden || !hidden(c, &by_pid))
        })
        .collect();
    let mut out = Vec::new();
    for (n, &a) in parts.iter().enumerate() {
        for &b in &parts[n + 1..] {
            let (ca, cb) = (&components[a], &components[b]);
            if !only.is_empty() && !only.contains(&ca.persist_id) && !only.contains(&cb.persist_id)
            {
                continue;
            }
            if let (Some(ba), Some(bb)) = (&ca.r#box, &cb.r#box)
                && let Some(gap) = box_gap(ba, bb)
                && gap < clearance
            {
                out.push(Candidate { a, b, box_gap: gap });
            }
        }
    }
    out.sort_by(|x, y| x.box_gap.total_cmp(&y.box_gap));
    out
}

/// One measured pair: the candidate and the gap Measure gave (meters), or
/// None when it gave none.
pub struct Measured {
    pub candidate: Candidate,
    pub gap: Option<f64>,
}

fn pair_json(ids: &mut IdRegistry, components: &[proto::ComponentInfo], m: &Measured) -> Value {
    let label = |ids: &mut IdRegistry, c: &proto::ComponentInfo| {
        let id = ids
            .id_for(Kind::Component, &c.persist_id)
            .unwrap_or_default();
        json!(format!("{} ({id})", c.name))
    };
    let mut obj = Map::new();
    let a = label(ids, &components[m.candidate.a]);
    let b = label(ids, &components[m.candidate.b]);
    obj.insert("components".into(), json!([a, b]));
    if let Some(gap) = m.gap {
        obj.insert("gap_mm".into(), json!(round(gap * MM_PER_M, 3)));
    }
    Value::Object(obj)
}

/// The reply: pairs closer than the clearance (smallest gap first), and the
/// ones that touch or overlap apart.
pub fn render(
    ids: &mut IdRegistry,
    components: &[proto::ComponentInfo],
    clearance: f64,
    measured: &[Measured],
    left_out: usize,
) -> Value {
    let mut closer: Vec<&Measured> = measured
        .iter()
        .filter(|m| m.gap.is_some_and(|g| g > 1e-9 && g < clearance))
        .collect();
    closer.sort_by(|x, y| x.gap.unwrap_or(0.0).total_cmp(&y.gap.unwrap_or(0.0)));
    let touching: Vec<Value> = measured
        .iter()
        .filter(|m| m.gap.is_some_and(|g| g <= 1e-9))
        .map(|m| pair_json(ids, components, m))
        .collect();
    let unmeasured = measured.iter().filter(|m| m.gap.is_none()).count();

    let mut out = Map::new();
    out.insert("clearance_mm".into(), json!(round(clearance * MM_PER_M, 3)));
    out.insert("pairs_measured".into(), json!(measured.len()));
    out.insert(
        "closer_than_clearance".into(),
        Value::Array(
            closer
                .iter()
                .map(|m| pair_json(ids, components, m))
                .collect(),
        ),
    );
    if !touching.is_empty() {
        out.insert("touching_or_overlapping".into(), Value::Array(touching));
        out.insert(
            "touching_note".into(),
            json!("gap 0: they touch or overlap; check_interference tells which overlap and by how much"),
        );
    }
    if unmeasured > 0 {
        out.insert(
            "not_measured".into(),
            json!(format!("{unmeasured} pair(s): SolidWorks gave no distance")),
        );
    }
    if left_out > 0 {
        out.insert(
            "warning".into(),
            json!(format!(
                "{left_out} more pair(s) have boxes closer than the clearance but weren't measured \
                 (at most {MAX_PAIRS} per call, closest boxes first); pass `components` to check the rest"
            )),
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

    /// A 10 mm cube starting at `x` mm along X.
    fn cube(pid: u8, name: &str, x: f64) -> proto::ComponentInfo {
        let x = x / 1000.0;
        proto::ComponentInfo {
            persist_id: vec![pid],
            name: name.into(),
            state: proto::ComponentState::Resolved.into(),
            r#box: Some(proto::Box3 {
                min: v(x, 0.0, 0.0),
                max: v(x + 0.01, 0.01, 0.01),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn box_gaps() {
        let a = cube(1, "a", 0.0).r#box.unwrap();
        let b = cube(2, "b", 13.0).r#box.unwrap();
        assert!((box_gap(&a, &b).unwrap() - 0.003).abs() < 1e-12);
        let c = cube(3, "c", 5.0).r#box.unwrap();
        assert_eq!(box_gap(&a, &c), Some(0.0));
    }

    #[test]
    fn candidates_are_close_boxes_of_parts_closest_first() {
        let mut sub = cube(9, "sub", 0.0);
        sub.subassembly = true;
        let mut far_hidden = cube(5, "hidden", 11.0);
        far_hidden.hidden = true;
        let components = vec![
            cube(1, "a", 0.0),
            cube(2, "b", 14.0), // 4 mm from a
            cube(3, "c", 11.0), // 1 mm from a, overlaps b
            cube(4, "far", 100.0),
            sub,
            far_hidden,
        ];
        let found = candidates(&components, 0.005, &HashSet::new(), false);
        let pairs: Vec<(usize, usize)> = found.iter().map(|c| (c.a, c.b)).collect();
        assert_eq!(pairs, [(1, 2), (0, 2), (0, 1)]);

        let with_hidden = candidates(&components, 0.005, &HashSet::new(), true);
        assert!(with_hidden.iter().any(|c| c.a == 0 && c.b == 5));

        let only: HashSet<Vec<u8>> = [vec![2u8]].into_iter().collect();
        let pairs: Vec<(usize, usize)> = candidates(&components, 0.005, &only, false)
            .iter()
            .map(|c| (c.a, c.b))
            .collect();
        assert_eq!(pairs, [(1, 2), (0, 1)]);
    }

    #[test]
    fn hidden_subassembly_hides_its_parts() {
        let mut sub = cube(9, "sub", 0.0);
        sub.subassembly = true;
        sub.hidden = true;
        let mut inside = cube(2, "sub/b", 12.0);
        inside.parent_persist_id = vec![9];
        let components = vec![cube(1, "a", 0.0), inside, sub];
        assert!(candidates(&components, 0.005, &HashSet::new(), false).is_empty());
    }

    #[test]
    fn render_sorts_and_separates_touching() {
        let components = vec![cube(1, "a", 0.0), cube(2, "b", 12.0), cube(3, "c", 10.0)];
        let measured = vec![
            Measured {
                candidate: Candidate {
                    a: 0,
                    b: 1,
                    box_gap: 0.002,
                },
                gap: Some(0.002),
            },
            Measured {
                candidate: Candidate {
                    a: 0,
                    b: 2,
                    box_gap: 0.0,
                },
                gap: Some(0.0),
            },
            Measured {
                candidate: Candidate {
                    a: 1,
                    b: 2,
                    box_gap: 0.0,
                },
                gap: Some(0.0011),
            },
        ];
        let mut ids = IdRegistry::default();
        let out = render(&mut ids, &components, 0.003, &measured, 0);
        assert_eq!(out["pairs_measured"], 3);
        let closer = out["closer_than_clearance"].as_array().unwrap();
        assert_eq!(closer.len(), 2);
        assert_eq!(closer[0]["gap_mm"], 1.1);
        assert_eq!(closer[1]["gap_mm"], 2.0);
        assert_eq!(out["touching_or_overlapping"].as_array().unwrap().len(), 1);
        assert!(out.get("warning").is_none());
    }
}
