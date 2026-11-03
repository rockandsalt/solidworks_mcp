//! Editing fillets, feature patterns and sweeps: the edit_fillet, edit_pattern and
//! edit_sweep tools' params, the wire requests built from them, and the text they
//! reply with.
//!
//! As edit_extrude (edit.rs): the feature by inspect_model id or feature-tree name,
//! only what's given changes, the definition is reported before and after, and the
//! add-in puts the old one back if the change doesn't take.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::delete;
use crate::edit::{error_text, feature_target};
use crate::inspect::{IdRegistry, Kind, MM_PER_M, mm, round};
use crate::mirror::{self, unique};
use crate::pattern;

/// The add-in's limits.
const MAX_EDGES: usize = 500;
const MAX_FEATURES: usize = 500;
const MAX_INSTANCES: u32 = 10000;
/// More turns than anyone means (as sweep_sketch).
const MAX_TWIST_DEG: f64 = 36_000.0;
/// Ids listed in a reply before "and N more".
const LISTED: usize = 20;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn positive_mm(value: Option<f64>, name: &str) -> Result<Option<f64>, McpError> {
    match value {
        Some(v) if !(v.is_finite() && v > 0.0) => Err(invalid(format!("{name} must be more than 0, got {v}"))),
        v => Ok(v.map(|v| v / MM_PER_M)),
    }
}

fn dimensions_text(dimensions: &[proto::DimensionInfo]) -> String {
    let names: Vec<&str> = dimensions.iter().map(|d| d.name.as_str()).collect();
    if names.is_empty() { String::new() } else { format!(" Its dimensions: {}.", names.join(", ")) }
}

fn listed(items: &[String]) -> String {
    let mut text = items.iter().take(LISTED).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > LISTED {
        text += &format!(" and {} more", items.len() - LISTED);
    }
    text
}

// ---- edit_fillet -------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditFilletParams {
    /// The fillet: a "feature:N" id from inspect_model or its name, e.g. "Fillet1".
    pub feature: String,
    /// New radius in mm.
    #[serde(default)]
    pub radius_mm: Option<f64>,
    /// true: also round the edges that run on tangentially from the given ones; false:
    /// only those.
    #[serde(default)]
    pub tangent_propagation: Option<bool>,
    /// Round these instead of what it rounds now: edge ids ("edge:7") or face ids ("face:3":
    /// all of its edges) from inspect_model. They must exist before the fillet: not made by
    /// it or a later feature. Not with add_edges.
    #[serde(default)]
    pub edges: Option<Vec<String>>,
    /// Round these as well (ids as for edges).
    #[serde(default)]
    pub add_edges: Vec<String>,
    /// Stop rounding these: ids from this tool's report of the fillet's edges and faces.
    #[serde(default)]
    pub remove_edges: Vec<String>,
}

impl EditFilletParams {
    fn changes_anything(&self) -> bool {
        self.radius_mm.is_some()
            || self.tangent_propagation.is_some()
            || self.edges.is_some()
            || !self.add_edges.is_empty()
            || !self.remove_edges.is_empty()
    }
}

/// Edge / face ids -> persist ids, once each.
fn entity_ids(ids: &IdRegistry, list: &[String], field: &str) -> Result<Vec<Vec<u8>>, McpError> {
    unique(list)
        .into_iter()
        .map(|e| {
            if !matches!(Kind::of_id(e), Some(Kind::Edge | Kind::Face)) {
                return Err(invalid(format!(
                    "{field}: {e:?} isn't an edge or face id -- give ids like \"edge:7\" or \"face:3\" \
                     from inspect_model (what=\"edges\" or \"faces\")"
                )));
            }
            ids.persist_id(e).ok_or_else(|| {
                invalid(format!("{field}: {}", ids.unknown_id(e, "inspect_model", "call inspect_model for current ones")))
            })
        })
        .collect()
}

pub fn fillet_request(ids: &IdRegistry, p: &EditFilletParams) -> Result<proto::EditFilletRequest, McpError> {
    let feature = feature_target(ids, &p.feature, "fillet")?;
    if p.edges.is_some() && !p.add_edges.is_empty() {
        return Err(invalid("give edges (replacing what the fillet rounds) or add_edges, not both"));
    }
    let edges = entity_ids(ids, p.edges.as_deref().unwrap_or_default(), "edges")?;
    if p.edges.is_some() && edges.is_empty() {
        return Err(invalid("edges is empty -- give the edge or face ids to round, or delete the fillet"));
    }
    let add_edges = entity_ids(ids, &p.add_edges, "add_edges")?;
    let remove_edges = entity_ids(ids, &p.remove_edges, "remove_edges")?;
    if edges.len() + add_edges.len() + remove_edges.len() > MAX_EDGES {
        return Err(invalid(format!("at most {MAX_EDGES} edges and faces per call")));
    }
    Ok(proto::EditFilletRequest {
        feature: Some(feature),
        radius: positive_mm(p.radius_mm, "radius_mm")?,
        tangent_propagation: p.tangent_propagation,
        edges,
        add_edges,
        remove_edges,
    })
}

fn fillet_kind(fillet_type: i32) -> &'static str {
    match fillet_type {
        0 => "constant-radius fillet",
        1 => "variable-radius fillet",
        2 => "face fillet",
        3 => "full-round fillet",
        _ => "fillet",
    }
}

fn fillet_items(ids: &mut IdRegistry, d: &proto::FilletDefinition) -> Vec<String> {
    d.items
        .iter()
        .filter_map(|item| ids.id_for(if item.face { Kind::Face } else { Kind::Edge }, &item.persist_id))
        .collect()
}

fn fillet_definition_text(ids: &mut IdRegistry, d: &proto::FilletDefinition) -> String {
    let mut text = format!("{} mm radius", mm(d.radius));
    if d.multiple_radius {
        text += " (the default: it has a radius per edge)";
    }
    if d.tangent_propagation {
        text += ", tangent propagation";
    }
    let items = fillet_items(ids, d);
    text += &format!(" on {}", if items.is_empty() { "nothing SolidWorks hands out".into() } else { listed(&items) });
    if d.other_items > 0 {
        text += &format!(" (and {} loops or features)", d.other_items);
    }
    text
}

pub fn fillet_reply_text(ids: &mut IdRegistry, params: &EditFilletParams, reply: &proto::EditFilletResponse) -> String {
    let Some(before) = &reply.before else {
        return "SolidWorks didn't describe the fillet.".into();
    };
    let Some(after) = &reply.after else {
        let mut text = format!(
            "{} ({}): {}.",
            before.name,
            fillet_kind(before.fillet_type),
            fillet_definition_text(ids, before)
        );
        text += &error_text(before.error_code, before.error_is_warning);
        text += &dimensions_text(&before.dimensions);
        text += &format!(
            " The edge/face ids are as the model is before {0} (the rounded edges themselves are \
             gone after it): give them to remove_edges. Edges for edges / add_edges come from \
             inspect_model and must exist before {0}; to pick ones it or a later feature hides, \
             rollback before={0:?} and inspect_model first.",
            before.name
        );
        if !params.changes_anything() {
            text += " Nothing was changed: give radius_mm, tangent_propagation, edges, add_edges or \
                     remove_edges to change it.";
        }
        return text;
    };
    let mut text = format!(
        "changed {} ({}): was {}; now {}.",
        after.name,
        fillet_kind(after.fillet_type),
        fillet_definition_text(ids, before),
        fillet_definition_text(ids, after)
    );
    text += &error_text(after.error_code, after.error_is_warning);
    text += &delete::problems_text(&reply.new_errors);
    text += " The model was rebuilt; face and edge ids next to the fillet may be out of date -- \
             inspect_model again.";
    text
}

// ---- edit_pattern ------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditPatternParams {
    /// The pattern: a "feature:N" id from inspect_model or its name, e.g. "LPattern1" or
    /// "CirPattern1".
    pub feature: String,
    /// Instances (along the first direction), the original included; at least 2.
    #[serde(default)]
    pub count: Option<u32>,
    /// Linear patterns: from one instance to the next along the first direction, in mm.
    #[serde(default)]
    pub spacing_mm: Option<f64>,
    /// Circular patterns: spread the instances evenly over this many degrees (360: all the
    /// way round). Not with spacing_deg.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Circular patterns: the angle from one instance to the next, in degrees, instead.
    #[serde(default)]
    pub spacing_deg: Option<f64>,
    /// true: pattern the other way along the direction / round the axis.
    #[serde(default)]
    pub reverse_direction: Option<bool>,
    /// Linear patterns: a new first direction, as for linear_pattern_features (a straight
    /// edge, a sketch line, a reference axis, or a planar face or plane along its normal).
    #[serde(default)]
    pub direction: Option<String>,
    /// Circular patterns: a new axis, as for circular_pattern_features (a reference axis, a
    /// straight or circular edge, a sketch line, a cylindrical face).
    #[serde(default)]
    pub axis: Option<String>,
    /// Linear patterns: instances along the second direction, the original row included; 1
    /// removes the second direction. Adding one takes direction2, count2 and spacing2_mm.
    #[serde(default)]
    pub count2: Option<u32>,
    /// Linear patterns: from one row to the next along the second direction, in mm.
    #[serde(default)]
    pub spacing2_mm: Option<f64>,
    /// Linear patterns: true goes the other way along the second direction.
    #[serde(default)]
    pub reverse_direction2: Option<bool>,
    /// Linear patterns: a new second direction (same kinds of reference as direction).
    #[serde(default)]
    pub direction2: Option<String>,
    /// Copy these features instead of the current ones: feature-tree names
    /// ("Cut-Extrude1") or inspect_model ids. Not for body patterns.
    #[serde(default)]
    pub features: Option<Vec<String>>,
}

impl EditPatternParams {
    fn changes_anything(&self) -> bool {
        self.count.is_some()
            || self.spacing_mm.is_some()
            || self.angle_deg.is_some()
            || self.spacing_deg.is_some()
            || self.reverse_direction.is_some()
            || self.direction.is_some()
            || self.axis.is_some()
            || self.count2.is_some()
            || self.spacing2_mm.is_some()
            || self.reverse_direction2.is_some()
            || self.direction2.is_some()
            || self.features.is_some()
    }
}

fn instances(count: Option<u32>, name: &str, lowest: u32) -> Result<Option<i32>, McpError> {
    match count {
        Some(n) if !(lowest..=MAX_INSTANCES).contains(&n) => {
            Err(invalid(format!("{name} must be {lowest} to {MAX_INSTANCES} (the original included), got {n}")))
        }
        n => Ok(n.map(|n| n as i32)),
    }
}

fn present(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub fn pattern_request(ids: &IdRegistry, p: &EditPatternParams) -> Result<proto::EditPatternRequest, McpError> {
    let feature = feature_target(ids, &p.feature, "pattern")?;
    let (spacing, equal_spacing) = match (p.spacing_mm, p.angle_deg, p.spacing_deg) {
        (None, None, None) => (None, None),
        (Some(s), None, None) => (positive_mm(Some(s), "spacing_mm")?, None),
        (None, Some(a), None) => {
            if !(a.is_finite() && a > 0.0 && a <= 360.0) {
                return Err(invalid(format!("angle_deg must be more than 0 and at most 360, got {a}")));
            }
            (Some(a.to_radians()), Some(true))
        }
        (None, None, Some(s)) => {
            if !(s.is_finite() && s > 0.0 && s < 360.0) {
                return Err(invalid(format!("spacing_deg must be more than 0 and less than 360, got {s}")));
            }
            (Some(s.to_radians()), Some(false))
        }
        _ => {
            return Err(invalid(
                "give one of spacing_mm (linear patterns), angle_deg or spacing_deg (circular patterns)",
            ));
        }
    };
    let direction = match (present(&p.direction), present(&p.axis)) {
        (Some(_), Some(_)) => return Err(invalid("give direction (linear patterns) or axis (circular), not both")),
        (Some(d), None) => Some(pattern::direction_ref(ids, d, "direction")?),
        (None, Some(a)) => Some(pattern::axis_ref(ids, a)?),
        (None, None) => None,
    };
    let direction2 = present(&p.direction2).map(|d| pattern::direction_ref(ids, d, "direction2")).transpose()?;
    if p.count2 == Some(1) && (p.spacing2_mm.is_some() || p.reverse_direction2.is_some() || direction2.is_some()) {
        return Err(invalid("count2 = 1 removes the second direction; give nothing else for it"));
    }
    let features = match &p.features {
        None => Vec::new(),
        Some(list) => {
            let list = unique(list);
            if list.is_empty() {
                return Err(invalid("features is empty -- give the features to copy, or leave it out to keep them"));
            }
            if list.len() > MAX_FEATURES {
                return Err(invalid(format!("at most {MAX_FEATURES} features per call")));
            }
            list.iter()
                .enumerate()
                .map(|(i, f)| {
                    mirror::feature_target(
                        ids,
                        f,
                        &format!("feature {}", i + 1),
                        "linear_pattern_sketch_entities / circular_pattern_sketch_entities",
                    )
                })
                .collect::<Result<_, _>>()?
        }
    };
    Ok(proto::EditPatternRequest {
        feature: Some(feature),
        count: instances(p.count, "count", 2)?,
        spacing,
        reverse: p.reverse_direction,
        equal_spacing,
        direction: Some(direction.unwrap_or_default()),
        count2: instances(p.count2, "count2", 1)?,
        spacing2: positive_mm(p.spacing2_mm, "spacing2_mm")?,
        reverse2: p.reverse_direction2,
        direction2: Some(direction2.unwrap_or_default()),
        features,
    })
}

/// A direction / axis as the reply names it: faces and edges by id, the rest as the
/// add-in calls them.
fn side_name(ids: &mut IdRegistry, s: &proto::PatternSide) -> String {
    let kind = match s.what.as_str() {
        "face" => Some(Kind::Face),
        "edge" => Some(Kind::Edge),
        _ => None,
    };
    kind.and_then(|k| ids.id_for(k, &s.persist_id)).unwrap_or_else(|| s.what.clone())
}

fn reversed(r: bool) -> &'static str {
    if r { " (reversed)" } else { "" }
}

fn pattern_definition_text(ids: &mut IdRegistry, d: &proto::PatternDefinition) -> String {
    let s1 = d.direction1.clone().unwrap_or_default();
    let mut text = if d.circular {
        let degrees = pattern::number(s1.spacing.to_degrees());
        let how = if !d.equal_spacing {
            format!("{degrees} degrees apart")
        } else if s1.spacing.to_degrees() >= 360.0 - 1e-6 {
            "evenly all the way round".into()
        } else {
            format!("spread evenly over {degrees} degrees")
        };
        format!("{} instances round {}{}, {how}", s1.count, side_name(ids, &s1), reversed(s1.reverse))
    } else {
        format!(
            "{} instances along {}{}, {} mm apart",
            s1.count,
            side_name(ids, &s1),
            reversed(s1.reverse),
            mm(s1.spacing)
        )
    };
    if let Some(s2) = &d.direction2 {
        text += &format!(
            "; times {} along {}{}, {} mm apart ({} in all)",
            s2.count,
            side_name(ids, s2),
            reversed(s2.reverse),
            mm(s2.spacing),
            s1.count * s2.count
        );
    }
    text
}

fn pattern_label(d: &proto::PatternDefinition) -> String {
    let kind = if d.circular { "circular pattern" } else { "linear pattern" };
    let of = if !d.features.is_empty() {
        d.features.join(", ")
    } else {
        match d.bodies {
            1 => "1 body".into(),
            n => format!("{n} bodies"),
        }
    };
    format!("{} ({kind} of {of})", d.name)
}

pub fn pattern_reply_text(ids: &mut IdRegistry, params: &EditPatternParams, reply: &proto::EditPatternResponse) -> String {
    let Some(before) = &reply.before else {
        return "SolidWorks didn't describe the pattern.".into();
    };
    let Some(after) = &reply.after else {
        let mut text = format!("{}: {}.", pattern_label(before), pattern_definition_text(ids, before));
        text += &error_text(before.error_code, before.error_is_warning);
        text += &dimensions_text(&before.dimensions);
        if !params.changes_anything() {
            text += " Nothing was changed: give count, spacing_mm / angle_deg / spacing_deg, \
                     reverse_direction, direction / axis, count2, ... or features to change it.";
        }
        return text;
    };
    let mut text = format!(
        "changed {}: was {}; now {}.",
        pattern_label(after),
        pattern_definition_text(ids, before),
        pattern_definition_text(ids, after)
    );
    text += &error_text(after.error_code, after.error_is_warning);
    text += &delete::problems_text(&reply.new_errors);
    let bodies = match reply.body_count {
        1 => "1 solid body".to_string(),
        n => format!("{n} solid bodies"),
    };
    text += &format!(
        " The part has {bodies}. The copies' faces and edges have new ids: inspect_model again \
         before using face or edge ids."
    );
    text
}

// ---- edit_sweep --------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditSweepParams {
    /// The sweep or cut-sweep: a "feature:N" id from inspect_model or its name, e.g.
    /// "Sweep1" or "Cut-Sweep1".
    pub feature: String,
    /// A new path: the feature-tree name of a sketch (one open or closed chain of curves),
    /// a 3D sketch or a helix, e.g. "Sketch3".
    #[serde(default)]
    pub path: Option<String>,
    /// A new profile: the feature-tree name of a sketch holding one closed contour across the
    /// path. Replaces a circular profile. Not with circle_diameter_mm.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// Sweep a circle of this diameter (mm) centered on the path instead of a profile
    /// sketch, or resize the circle.
    #[serde(default)]
    pub circle_diameter_mm: Option<f64>,
    /// Twist the profile by this many degrees from the start of the path to its end; 0: the
    /// profile follows the path without turning. Not with a circular profile.
    #[serde(default)]
    pub twist_deg: Option<f64>,
}

impl EditSweepParams {
    fn changes_anything(&self) -> bool {
        present(&self.path).is_some()
            || present(&self.sketch_name).is_some()
            || self.circle_diameter_mm.is_some()
            || self.twist_deg.is_some()
    }
}

fn feature_name(name: Option<&str>, field: &str) -> Result<String, McpError> {
    match name {
        Some(n) if Kind::of_id(n).is_some() => Err(invalid(format!(
            "{field}: give the feature-tree name (e.g. \"Sketch1\"), not an id like {n}"
        ))),
        n => Ok(n.unwrap_or_default().to_string()),
    }
}

pub fn sweep_request(ids: &IdRegistry, p: &EditSweepParams) -> Result<proto::EditSweepRequest, McpError> {
    let feature = feature_target(ids, &p.feature, "sweep")?;
    let sketch_name = feature_name(present(&p.sketch_name), "sketch_name")?;
    if p.circle_diameter_mm.is_some() && !sketch_name.is_empty() {
        return Err(invalid("give sketch_name (a profile sketch) or circle_diameter_mm, not both"));
    }
    if let Some(t) = p.twist_deg
        && !(t.is_finite() && t.abs() <= MAX_TWIST_DEG)
    {
        return Err(invalid(format!("twist_deg must be within +-{MAX_TWIST_DEG}, got {t}")));
    }
    if p.circle_diameter_mm.is_some() && p.twist_deg.is_some_and(|t| t != 0.0) {
        return Err(invalid("a circular profile can't be twisted -- drop twist_deg or give a profile sketch"));
    }
    Ok(proto::EditSweepRequest {
        feature: Some(feature),
        path: feature_name(present(&p.path), "path")?,
        sketch_name,
        circle_diameter: positive_mm(p.circle_diameter_mm, "circle_diameter_mm")?,
        twist_angle: p.twist_deg.map(f64::to_radians),
    })
}

/// swTwistControlConstantTwistAlongPath.
const CONSTANT_TWIST: i32 = 8;

fn sweep_definition_text(d: &proto::SweepDefinition) -> String {
    let profile = if d.profile.is_empty() {
        format!("a {} mm circle", mm(d.circle_diameter))
    } else {
        d.profile.clone()
    };
    let mut text = format!("{profile} along {}", d.path);
    if d.twist_control == CONSTANT_TWIST {
        text += &format!(", twisted {} degrees", round(d.twist_angle.to_degrees(), 4));
    }
    if d.guide_curves > 0 {
        text += &format!(", {} guide curves", d.guide_curves);
    }
    text
}

fn sweep_label(d: &proto::SweepDefinition) -> String {
    format!("{} ({})", d.name, if d.cut { "cut-sweep" } else { "sweep" })
}

pub fn sweep_reply_text(params: &EditSweepParams, reply: &proto::EditSweepResponse) -> String {
    let Some(before) = &reply.before else {
        return "SolidWorks didn't describe the sweep.".into();
    };
    let Some(after) = &reply.after else {
        let mut text = format!("{}: {}.", sweep_label(before), sweep_definition_text(before));
        text += &error_text(before.error_code, before.error_is_warning);
        text += &dimensions_text(&before.dimensions);
        if !params.changes_anything() {
            text += " Nothing was changed: give path, sketch_name, circle_diameter_mm or twist_deg \
                     to change it; to change the profile's or the path's shape, edit their sketches \
                     with the sketch tools.";
        }
        return text;
    };
    let mut text = format!(
        "changed {}: was {}; now {}.",
        sweep_label(after),
        sweep_definition_text(before),
        sweep_definition_text(after)
    );
    text += &error_text(after.error_code, after.error_is_warning);
    text += &delete::problems_text(&reply.new_errors);
    text += " The model was rebuilt; earlier body, face and edge ids of the swept geometry may be \
             out of date -- inspect_model again.";
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (IdRegistry, String, String) {
        let mut ids = IdRegistry::default();
        let edge = ids.id_for(Kind::Edge, b"edge-bytes").unwrap();
        let face = ids.id_for(Kind::Face, b"face-bytes").unwrap();
        (ids, edge, face)
    }

    #[test]
    fn fillet_request_converts_and_checks() {
        let (ids, edge, face) = registry();
        let p = EditFilletParams {
            feature: "Fillet1".into(),
            radius_mm: Some(3.0),
            add_edges: vec![edge.clone(), format!(" {edge} "), face.clone()],
            ..Default::default()
        };
        let r = fillet_request(&ids, &p).unwrap();
        assert_eq!(r.radius, Some(0.003));
        assert_eq!(r.add_edges, vec![b"edge-bytes".to_vec(), b"face-bytes".to_vec()]);
        assert!(r.edges.is_empty() && r.remove_edges.is_empty() && r.tangent_propagation.is_none());

        for bad in [
            EditFilletParams { feature: "Fillet1".into(), radius_mm: Some(0.0), ..Default::default() },
            EditFilletParams { feature: "Fillet1".into(), edges: Some(vec![]), ..Default::default() },
            EditFilletParams {
                feature: "Fillet1".into(),
                edges: Some(vec![edge.clone()]),
                add_edges: vec![face.clone()],
                ..Default::default()
            },
            EditFilletParams { feature: "Fillet1".into(), add_edges: vec!["vertex:1".into()], ..Default::default() },
            EditFilletParams { feature: "Fillet1".into(), remove_edges: vec!["edge:99".into()], ..Default::default() },
        ] {
            assert!(fillet_request(&ids, &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn fillet_text_lists_items_by_id() {
        let (mut ids, edge, _) = registry();
        let before = proto::FilletDefinition {
            name: "Fillet1".into(),
            radius: 0.002,
            tangent_propagation: true,
            items: vec![proto::FilletItem { persist_id: b"edge-bytes".to_vec(), face: false }],
            ..Default::default()
        };
        let reply = proto::EditFilletResponse { before: Some(before.clone()), ..Default::default() };
        let text = fillet_reply_text(&mut ids, &EditFilletParams::default(), &reply);
        assert!(
            text.starts_with(&format!("Fillet1 (constant-radius fillet): 2 mm radius, tangent propagation on {edge}.")),
            "{text}"
        );
        assert!(text.contains("Nothing was changed"), "{text}");

        let after = proto::FilletDefinition {
            radius: 0.003,
            items: vec![
                proto::FilletItem { persist_id: b"edge-bytes".to_vec(), face: false },
                proto::FilletItem { persist_id: b"new-face".to_vec(), face: true },
            ],
            ..before.clone()
        };
        let reply = proto::EditFilletResponse { before: Some(before), after: Some(after), new_errors: vec![] };
        let text = fillet_reply_text(&mut ids, &EditFilletParams::default(), &reply);
        assert!(
            text.starts_with(&format!(
                "changed Fillet1 (constant-radius fillet): was 2 mm radius, tangent propagation on {edge}; \
                 now 3 mm radius, tangent propagation on {edge}, face:2."
            )),
            "{text}"
        );
    }

    #[test]
    fn pattern_request_converts_and_checks() {
        let (ids, edge, _) = registry();
        let p = EditPatternParams { feature: "CirPattern1".into(), count: Some(8), angle_deg: Some(180.0), ..Default::default() };
        let r = pattern_request(&ids, &p).unwrap();
        assert_eq!((r.count, r.equal_spacing), (Some(8), Some(true)));
        assert!((r.spacing.unwrap() - std::f64::consts::PI).abs() < 1e-12);
        assert!(r.direction.unwrap().target.is_none() && r.features.is_empty());

        let p = EditPatternParams {
            feature: "LPattern1".into(),
            spacing_mm: Some(12.5),
            direction2: Some(edge.clone()),
            count2: Some(3),
            spacing2_mm: Some(20.0),
            features: Some(vec!["Cut-Extrude2".into()]),
            ..Default::default()
        };
        let r = pattern_request(&ids, &p).unwrap();
        assert_eq!((r.spacing, r.equal_spacing, r.count2, r.spacing2), (Some(0.0125), None, Some(3), Some(0.02)));
        assert!(r.direction2.unwrap().target.is_some());
        assert_eq!(r.features.len(), 1);

        for bad in [
            EditPatternParams { feature: "x".into(), count: Some(1), ..Default::default() },
            EditPatternParams { feature: "x".into(), spacing_mm: Some(5.0), angle_deg: Some(90.0), ..Default::default() },
            EditPatternParams { feature: "x".into(), angle_deg: Some(400.0), ..Default::default() },
            EditPatternParams { feature: "x".into(), direction: Some(edge.clone()), axis: Some(edge.clone()), ..Default::default() },
            EditPatternParams { feature: "x".into(), count2: Some(1), spacing2_mm: Some(5.0), ..Default::default() },
            EditPatternParams { feature: "x".into(), features: Some(vec![]), ..Default::default() },
        ] {
            assert!(pattern_request(&ids, &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pattern_text_shows_before_and_after() {
        let (mut ids, edge, _) = registry();
        let side = |count, spacing| proto::PatternSide {
            what: "edge".into(),
            persist_id: b"edge-bytes".to_vec(),
            count,
            spacing,
            reverse: false,
        };
        let before = proto::PatternDefinition {
            name: "LPattern1".into(),
            features: vec!["Cut-Extrude1".into()],
            direction1: Some(side(3, 0.01)),
            ..Default::default()
        };
        let after = proto::PatternDefinition { direction1: Some(side(5, 0.008)), ..before.clone() };
        let reply = proto::EditPatternResponse {
            before: Some(before),
            after: Some(after),
            new_errors: vec![],
            body_count: 1,
        };
        let text = pattern_reply_text(&mut ids, &EditPatternParams::default(), &reply);
        assert!(
            text.starts_with(&format!(
                "changed LPattern1 (linear pattern of Cut-Extrude1): was 3 instances along {edge}, 10 mm \
                 apart; now 5 instances along {edge}, 8 mm apart."
            )),
            "{text}"
        );

        let circular = proto::PatternDefinition {
            name: "CirPattern1".into(),
            circular: true,
            equal_spacing: true,
            features: vec!["Cut-Extrude1".into()],
            direction1: Some(proto::PatternSide {
                what: "Axis1".into(),
                count: 6,
                spacing: std::f64::consts::TAU,
                ..Default::default()
            }),
            ..Default::default()
        };
        let reply = proto::EditPatternResponse { before: Some(circular), ..Default::default() };
        let text = pattern_reply_text(&mut ids, &EditPatternParams::default(), &reply);
        assert!(
            text.starts_with("CirPattern1 (circular pattern of Cut-Extrude1): 6 instances round Axis1, evenly all the way round."),
            "{text}"
        );
    }

    #[test]
    fn sweep_request_converts_and_checks() {
        let (ids, _, _) = registry();
        let p = EditSweepParams { feature: "Sweep1".into(), path: Some(" Sketch3 ".into()), twist_deg: Some(90.0), ..Default::default() };
        let r = sweep_request(&ids, &p).unwrap();
        assert_eq!(r.path, "Sketch3");
        assert!(r.sketch_name.is_empty() && r.circle_diameter.is_none());
        assert!((r.twist_angle.unwrap() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);

        for bad in [
            EditSweepParams { feature: "Sweep1".into(), path: Some("feature:3".into()), ..Default::default() },
            EditSweepParams { feature: "Sweep1".into(), circle_diameter_mm: Some(-1.0), ..Default::default() },
            EditSweepParams {
                feature: "Sweep1".into(),
                circle_diameter_mm: Some(4.0),
                sketch_name: Some("Sketch2".into()),
                ..Default::default()
            },
            EditSweepParams { feature: "Sweep1".into(), circle_diameter_mm: Some(4.0), twist_deg: Some(30.0), ..Default::default() },
        ] {
            assert!(sweep_request(&ids, &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sweep_text_shows_before_and_after() {
        let before = proto::SweepDefinition {
            name: "Sweep1".into(),
            profile: "Sketch2".into(),
            path: "Sketch1".into(),
            ..Default::default()
        };
        let after = proto::SweepDefinition {
            profile: String::new(),
            circle_diameter: 0.004,
            path: "Sketch3".into(),
            ..before.clone()
        };
        let reply = proto::EditSweepResponse { before: Some(before), after: Some(after), new_errors: vec![] };
        let text = sweep_reply_text(&EditSweepParams::default(), &reply);
        assert!(
            text.starts_with("changed Sweep1 (sweep): was Sketch2 along Sketch1; now a 4 mm circle along Sketch3."),
            "{text}"
        );
    }
}
