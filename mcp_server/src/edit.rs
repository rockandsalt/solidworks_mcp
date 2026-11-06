//! Editing features: the edit_extrude and edit_mate tools' params, the wire
//! requests built from them, and the text they reply with.
//!
//! Both take the feature by inspect_model id ("feature:4") or feature-tree
//! name, change only what's given, and report the definition before and
//! after. The add-in puts the old definition back if SolidWorks rejects the
//! change or the feature fails to rebuild with it.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::assembly::{self, MateAlign, MateKind};
use crate::delete;
use crate::diagnose;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, mm, round};

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// The wire target for one feature: a "feature:N" id this server handed out,
/// or else a feature-tree name.
pub fn feature_target(ids: &IdRegistry, feature: &str, noun: &str) -> Result<proto::FeatureTarget, McpError> {
    use proto::feature_target::Target;
    let feature = feature.trim();
    if feature.is_empty() {
        return Err(invalid(format!("give the {noun}'s id or name")));
    }
    let target = match Kind::of_id(feature) {
        None => Target::Name(feature.to_string()),
        Some(Kind::Feature) => Target::PersistId(ids.persist_id(feature).ok_or_else(|| {
            invalid(ids.unknown_id(
                feature,
                "inspect_model",
                &format!("call inspect_model for current ones, or give the {noun}'s name"),
            ))
        })?),
        Some(_) => {
            return Err(invalid(format!(
                "{feature} isn't a feature -- give the {noun} as a \"feature:N\" id from \
                 inspect_model or by its feature-tree name"
            )));
        }
    };
    Ok(proto::FeatureTarget { target: Some(target) })
}

// ---- edit_extrude ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndCondition {
    /// To a depth (depth_mm).
    Blind,
    /// Through the whole part.
    ThroughAll,
    /// Symmetric about the sketch plane, depth_mm in total. Direction 1 only.
    MidPlane,
    /// Up to the next face it meets.
    UpToNext,
    /// Cuts, direction 1 only: through the whole part on both sides.
    ThroughAllBoth,
}

impl EndCondition {
    fn uses_depth(self) -> bool {
        matches!(self, EndCondition::Blind | EndCondition::MidPlane)
    }
}

impl From<EndCondition> for proto::EndCondition {
    fn from(c: EndCondition) -> Self {
        match c {
            EndCondition::Blind => proto::EndCondition::Blind,
            EndCondition::ThroughAll => proto::EndCondition::ThroughAll,
            EndCondition::MidPlane => proto::EndCondition::MidPlane,
            EndCondition::UpToNext => proto::EndCondition::UpToNext,
            EndCondition::ThroughAllBoth => proto::EndCondition::ThroughAllBoth,
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditExtrudeParams {
    /// The boss- or cut-extrude: a "feature:N" id from inspect_model or its name, e.g.
    /// "Boss-Extrude1" or "Cut-Extrude2".
    pub feature: String,
    /// New end condition: "blind" (to depth_mm), "through_all", "mid_plane" (depth_mm in
    /// total, half each side), "up_to_next", or for cuts "through_all_both".
    #[serde(default)]
    pub end_condition: Option<EndCondition>,
    /// New depth in millimeters (blind / mid_plane).
    #[serde(default)]
    pub depth_mm: Option<f64>,
    /// true: extrude/cut the other way along the sketch normal; false: the default way.
    #[serde(default)]
    pub reverse_direction: Option<bool>,
    /// Taper the sides while extruding, in degrees (0 removes the draft).
    #[serde(default)]
    pub draft_deg: Option<f64>,
    /// With a draft: taper outward (growing) instead of inward.
    #[serde(default)]
    pub draft_outward: Option<bool>,
    /// Cuts only: true removes the material outside the profile instead of inside.
    #[serde(default)]
    pub flip_side_to_cut: Option<bool>,
    /// true adds a second direction (the other side of the sketch plane; blind unless
    /// end_condition2 says otherwise), false removes it.
    #[serde(default)]
    pub both_directions: Option<bool>,
    /// Second direction's end condition: "blind", "through_all" or "up_to_next".
    #[serde(default)]
    pub end_condition2: Option<EndCondition>,
    /// Second direction's depth in millimeters (blind).
    #[serde(default)]
    pub depth2_mm: Option<f64>,
}

impl EditExtrudeParams {
    fn changes_anything(&self) -> bool {
        self.end_condition.is_some()
            || self.depth_mm.is_some()
            || self.reverse_direction.is_some()
            || self.draft_deg.is_some()
            || self.draft_outward.is_some()
            || self.flip_side_to_cut.is_some()
            || self.both_directions.is_some()
            || self.end_condition2.is_some()
            || self.depth2_mm.is_some()
    }
}

fn depth_m(value: Option<f64>, name: &str) -> Result<Option<f64>, McpError> {
    match value {
        Some(d) if !(d.is_finite() && d > 0.0) => {
            Err(invalid(format!("{name} must be a positive number of millimeters, got {d}")))
        }
        d => Ok(d.map(|d| d / MM_PER_M)),
    }
}

pub fn extrude_request(ids: &IdRegistry, p: &EditExtrudeParams) -> Result<proto::EditExtrudeRequest, McpError> {
    let feature = feature_target(ids, &p.feature, "extrude")?;
    if let Some(c) = p.end_condition
        && p.depth_mm.is_some()
        && !c.uses_depth()
    {
        return Err(invalid("depth_mm only goes with end_condition \"blind\" or \"mid_plane\""));
    }
    if let Some(c) = p.end_condition2 {
        if matches!(c, EndCondition::MidPlane | EndCondition::ThroughAllBoth) {
            return Err(invalid(
                "end_condition2 can be \"blind\", \"through_all\" or \"up_to_next\" (mid_plane and \
                 through_all_both are direction 1 end conditions)",
            ));
        }
        if p.depth2_mm.is_some() && !c.uses_depth() {
            return Err(invalid("depth2_mm only goes with end_condition2 \"blind\""));
        }
    }
    if p.both_directions == Some(false) && (p.end_condition2.is_some() || p.depth2_mm.is_some()) {
        return Err(invalid("end_condition2 / depth2_mm describe the second direction that both_directions=false removes"));
    }
    let draft = match p.draft_deg {
        Some(d) if !(d.is_finite() && (0.0..90.0).contains(&d)) => {
            return Err(invalid(format!("draft_deg must be 0 to under 90 degrees, got {d}")));
        }
        d => d.map(f64::to_radians),
    };
    Ok(proto::EditExtrudeRequest {
        feature: Some(feature),
        end_condition: p.end_condition.map_or(proto::EndCondition::Unspecified, Into::into).into(),
        depth: depth_m(p.depth_mm, "depth_mm")?,
        reverse_direction: p.reverse_direction,
        draft_angle: draft,
        draft_outward: p.draft_outward,
        flip_side_to_cut: p.flip_side_to_cut,
        both_directions: p.both_directions,
        end_condition2: p.end_condition2.map_or(proto::EndCondition::Unspecified, Into::into).into(),
        depth2: depth_m(p.depth2_mm, "depth2_mm")?,
    })
}

/// swEndConditions_e values EndCondition doesn't cover.
pub(crate) fn raw_end_condition_name(raw: i32) -> String {
    match raw {
        3 => "up to vertex".into(),
        4 => "up to surface".into(),
        5 => "offset from surface".into(),
        7 => "up to body".into(),
        10 => "up to selection".into(),
        raw => format!("end condition {raw}"),
    }
}

fn direction_text(d: &proto::ExtrudeDirection) -> String {
    use proto::EndCondition as E;
    let depth = format!("{} mm", mm(d.depth));
    let mut text = match d.end_condition() {
        E::Blind => format!("blind {depth}"),
        E::MidPlane => format!("mid plane {depth} in total"),
        E::ThroughAll => "through all".into(),
        E::UpToNext => "up to next".into(),
        E::ThroughAllBoth => "through all, both sides".into(),
        E::Other | E::Unspecified => {
            let name = raw_end_condition_name(d.raw_end_condition);
            if d.raw_end_condition == 5 { format!("{name} {depth}") } else { name }
        }
    };
    if d.draft_angle > 0.0 {
        let how = if d.draft_outward { "outward" } else { "inward" };
        text += &format!(", draft {} deg {how}", round(d.draft_angle.to_degrees(), 4));
    }
    text
}

fn definition_text(d: &proto::ExtrudeDefinition) -> String {
    let mut text = direction_text(d.direction1.as_ref().unwrap_or(&Default::default()));
    if d.reverse_direction {
        text += ", reversed";
    }
    if let Some(d2) = &d.direction2 {
        text += &format!("; second direction {}", direction_text(d2));
    }
    if d.flip_side_to_cut {
        text += "; removes the material outside the profile (flipped side to cut)";
    }
    text
}

fn extrude_label(d: &proto::ExtrudeDefinition) -> String {
    let what = if d.cut { "cut-extrude" } else { "boss-extrude" };
    let thin = if d.thin { "thin " } else { "" };
    if d.sketch_name.is_empty() {
        format!("{} ({thin}{what})", d.name)
    } else {
        format!("{} ({thin}{what} of {})", d.name, d.sketch_name)
    }
}

pub(crate) fn error_text(code: i32, warning: bool) -> String {
    if code == 0 {
        return String::new();
    }
    let how = if warning { "warns" } else { "fails to rebuild" };
    format!(" SolidWorks says it {how}: {}.", delete::feature_error_name(code))
}

pub fn extrude_reply_text(params: &EditExtrudeParams, reply: &proto::EditExtrudeResponse) -> String {
    let Some(before) = &reply.before else {
        return "SolidWorks didn't describe the extrude.".into();
    };
    let Some(after) = &reply.after else {
        let mut text = format!("{}: {}.", extrude_label(before), definition_text(before));
        text += &error_text(before.error_code, before.error_is_warning);
        let dims: Vec<&str> = before.dimensions.iter().map(|d| d.name.as_str()).collect();
        if !dims.is_empty() {
            text += &format!(" Its dimensions: {}.", dims.join(", "));
        }
        if !params.changes_anything() {
            text += " Nothing was changed: give end_condition, depth_mm, reverse_direction, \
                     draft_deg, both_directions, ... to change it.";
        }
        return text;
    };
    let mut text = format!(
        "changed {}: was {}; now {}.",
        extrude_label(after),
        definition_text(before),
        definition_text(after)
    );
    text += &error_text(after.error_code, after.error_is_warning);
    text += &delete::problems_text(&reply.new_errors);
    text += " The model was rebuilt; earlier body ids and coordinates of the changed geometry may \
             be out of date -- inspect_model again.";
    text
}

// ---- edit_mate ---------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditMateParams {
    /// The mate: a "feature:N" id from inspect_model what="mates", or its name, e.g.
    /// "Coincident1".
    pub mate: String,
    /// Change the mate type: coincident, concentric, parallel, perpendicular, tangent,
    /// distance (needs distance_mm), angle (needs angle_deg) or lock (needs two component
    /// entities).
    #[serde(default)]
    pub kind: Option<MateKind>,
    /// Mate these two instead, as in add_mate: face/edge ids or reference plane/axis
    /// "feature:N" ids on two different components (two "component:N" ids for lock). Omit to
    /// keep the mated entities.
    #[serde(default)]
    pub entities: Option<Vec<String>>,
    /// "closest", "aligned" or "anti_aligned" (flips which way the component faces).
    #[serde(default)]
    pub alignment: Option<MateAlign>,
    /// Distance/angle mates: put the gap/angle on the other side.
    #[serde(default)]
    pub flip: Option<bool>,
    /// Distance mates: the new gap in millimeters (0 or more).
    #[serde(default)]
    pub distance_mm: Option<f64>,
    /// Angle mates: the new angle, 0 to 180 degrees.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Concentric mates: stop (true) or allow (false) spinning about the shared axis.
    #[serde(default)]
    pub lock_rotation: Option<bool>,
    /// Distance mates: the smallest gap of a limit mate, in millimeters. Making a plain
    /// distance mate a limit mate takes min_mm and max_mm; a limit mate takes either.
    #[serde(default)]
    pub min_mm: Option<f64>,
    /// Distance mates: the largest gap of a limit mate, in millimeters.
    #[serde(default)]
    pub max_mm: Option<f64>,
    /// Angle mates: the smallest angle of a limit mate, 0 to 180 degrees (as min_mm).
    #[serde(default)]
    pub min_deg: Option<f64>,
    /// Angle mates: the largest angle of a limit mate, 0 to 180 degrees.
    #[serde(default)]
    pub max_deg: Option<f64>,
    /// true: make a limit mate a plain distance / angle mate, at its current value.
    #[serde(default)]
    pub remove_limits: bool,
}

/// Checks what doesn't need SolidWorks and returns the new entities' ids,
/// trimmed (empty: keep the mated ones). The caller resolves them.
pub fn check_mate(p: &EditMateParams) -> Result<Vec<String>, McpError> {
    let entities: Vec<String> = p.entities.iter().flatten().map(|e| e.trim().to_string()).collect();
    if p.entities.is_some() {
        if entities.len() != 2 {
            return Err(invalid("entities: give exactly two (or leave it out to keep the mated ones)"));
        }
        if entities[0] == entities[1] {
            return Err(invalid("the two entities are the same one"));
        }
        for e in &entities {
            let ok = match (p.kind, Kind::of_id(e)) {
                (Some(MateKind::Lock), Some(Kind::Component)) => true,
                (Some(MateKind::Lock), _) => {
                    return Err(invalid(format!(
                        "a lock mate takes two component ids like \"component:2\", got {e:?}"
                    )));
                }
                (_, Some(Kind::Face | Kind::Edge | Kind::Feature | Kind::Component)) => true,
                _ => false,
            };
            if !ok {
                return Err(invalid(format!(
                    "{e:?} can't be mated -- give face/edge ids or reference plane/axis feature ids \
                     from inspect_model on the assembly"
                )));
            }
        }
    }
    if let Some(d) = p.distance_mm
        && !(d.is_finite() && d >= 0.0)
    {
        return Err(invalid(format!("distance_mm must be 0 or more, got {d}")));
    }
    if let Some(a) = p.angle_deg
        && !(a.is_finite() && (0.0..=180.0).contains(&a))
    {
        return Err(invalid(format!("angle_deg must be 0 to 180, got {a}")));
    }
    let mm_limits = p.min_mm.is_some() || p.max_mm.is_some();
    let deg_limits = p.min_deg.is_some() || p.max_deg.is_some();
    if mm_limits && deg_limits {
        return Err(invalid("give min_mm / max_mm (distance mates) or min_deg / max_deg (angle mates), not both"));
    }
    for (name, v, top) in [
        ("min_mm", p.min_mm, f64::INFINITY),
        ("max_mm", p.max_mm, f64::INFINITY),
        ("min_deg", p.min_deg, 180.0),
        ("max_deg", p.max_deg, 180.0),
    ] {
        if let Some(v) = v
            && !(v.is_finite() && 0.0 <= v && v <= top)
        {
            return Err(invalid(format!("{name} must be 0 to {top}, got {v}")));
        }
    }
    if let (Some(lo), Some(hi)) = (p.min_mm.or(p.min_deg), p.max_mm.or(p.max_deg))
        && lo > hi
    {
        return Err(invalid(format!("the minimum ({lo}) is above the maximum ({hi})")));
    }
    if p.remove_limits && (mm_limits || deg_limits) {
        return Err(invalid("give new limits or remove_limits, not both"));
    }
    let limits_kind = if mm_limits {
        Some(MateKind::Distance)
    } else if deg_limits {
        Some(MateKind::Angle)
    } else {
        None
    };
    if let (Some(kind), Some(limits_kind)) = (p.kind, limits_kind)
        && kind != limits_kind
    {
        return Err(invalid(match kind {
            MateKind::Distance => "a distance mate's limits are min_mm / max_mm",
            MateKind::Angle => "an angle mate's limits are min_deg / max_deg",
            _ => "only distance and angle mates have limits",
        }));
    }
    if p.kind.is_none()
        && p.entities.is_none()
        && p.alignment.is_none()
        && p.flip.is_none()
        && p.distance_mm.is_none()
        && p.angle_deg.is_none()
        && p.lock_rotation.is_none()
        && !mm_limits
        && !deg_limits
        && !p.remove_limits
    {
        return Err(invalid(
            "nothing to change -- give kind, entities, alignment, flip, distance_mm, angle_deg, \
             min_mm / max_mm, min_deg / max_deg, remove_limits or lock_rotation (inspect_model \
             what=\"mates\" shows the mates as they are)",
        ));
    }
    Ok(entities)
}

pub fn mate_request(
    ids: &IdRegistry,
    p: &EditMateParams,
    entity_persist_ids: Vec<Vec<u8>>,
) -> Result<proto::EditMateRequest, McpError> {
    Ok(proto::EditMateRequest {
        mate: Some(feature_target(ids, &p.mate, "mate")?),
        r#type: p.kind.map_or(proto::MateType::Unspecified, Into::into).into(),
        entity_persist_ids,
        alignment: p.alignment.map_or(proto::MateAlignment::Unspecified, Into::into).into(),
        flip: p.flip,
        distance: p.distance_mm.map(|d| d / MM_PER_M),
        angle: p.angle_deg.map(f64::to_radians),
        lock_rotation: p.lock_rotation,
        min_limit: p.min_mm.map(|d| d / MM_PER_M).or(p.min_deg.map(f64::to_radians)),
        max_limit: p.max_mm.map(|d| d / MM_PER_M).or(p.max_deg.map(f64::to_radians)),
        remove_limits: p.remove_limits,
    })
}

pub fn mate_text(m: &proto::MateInfo) -> String {
    let kind = m.r#type();
    let mut text = if matches!(kind, proto::MateType::Other | proto::MateType::Unspecified) {
        m.type_name.clone()
    } else {
        assembly::mate_type_name(kind).to_string()
    };
    if let Some(a) = assembly::alignment_name(m.alignment()) {
        text += &format!(", {}", a.replace('_', "-"));
    }
    if m.flipped {
        text += ", flipped";
    }
    if let Some(dim) = &m.dimension
        && !dim.name.is_empty()
    {
        if dim.r#type() == proto::DimensionType::Angular {
            text += &format!(", {} deg", round(dim.value.to_degrees(), 4));
        } else {
            text += &format!(", {} mm", mm(dim.value));
        }
    }
    text + &assembly::limits_text(m)
}

pub fn entity_text(ids: &mut IdRegistry, e: &proto::MateEntityState) -> String {
    let json = diagnose::mate_entity_json(ids, e);
    if e.missing {
        return "(missing)".into();
    }
    let what = json["id"].as_str().map_or_else(|| e.kind.clone(), str::to_string);
    if e.component_name.is_empty() {
        format!("{what} (assembly)")
    } else {
        format!("{what} on {}", e.component_name)
    }
}

pub fn mate_reply_text(ids: &mut IdRegistry, reply: &proto::EditMateResponse) -> String {
    let (Some(before), Some(after)) = (&reply.before, &reply.after) else {
        return "edited the mate.".into();
    };
    let mate_id = ids.id_for(Kind::Feature, &after.persist_id).unwrap_or_default();
    let mut text = if before.name == after.name {
        format!("edited {} (now {mate_id})", after.name)
    } else {
        format!(
            "replaced {} with {} ({mate_id}; a new type makes a new mate, named after its type)",
            before.name, after.name
        )
    };
    text += &format!(": was {}; now {}", mate_text(before), mate_text(after));
    let entities: Vec<String> = reply.entities.iter().map(|e| entity_text(ids, e)).collect();
    if !entities.is_empty() {
        text += &format!(", between {}", entities.join(" and "));
    }
    text.push('.');
    if after.error_code != 0 {
        text += &format!(" SolidWorks flags it: {}.", assembly::mate_error_name(after.error_code));
    }
    if !reply.components.is_empty() {
        let places: Vec<String> = reply.components.iter().map(assembly::placement_text).collect();
        text += &format!(" After solving: {}.", places.join("; "));
        text += " Earlier inspect_model coordinates of moved components are out of date (their ids \
                 still work).";
    }
    text += &delete::problems_text(&reply.new_errors);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (IdRegistry, String) {
        let mut ids = IdRegistry::default();
        let feature = ids.id_for(Kind::Feature, b"cut-bytes").unwrap();
        (ids, feature)
    }

    #[test]
    fn extrude_request_converts_units_and_keeps_unset_fields_unset() {
        let (ids, feature) = registry();
        let p = EditExtrudeParams {
            feature: feature.clone(),
            end_condition: Some(EndCondition::Blind),
            depth_mm: Some(25.0),
            draft_deg: Some(2.0),
            ..Default::default()
        };
        let r = extrude_request(&ids, &p).unwrap();
        assert_eq!(r.end_condition(), proto::EndCondition::Blind);
        assert_eq!(r.depth, Some(0.025));
        assert!((r.draft_angle.unwrap() - 2f64.to_radians()).abs() < 1e-15);
        assert_eq!((r.reverse_direction, r.both_directions, r.depth2), (None, None, None));
        assert_eq!(r.end_condition2(), proto::EndCondition::Unspecified);
        assert_eq!(
            r.feature.unwrap().target,
            Some(proto::feature_target::Target::PersistId(b"cut-bytes".to_vec()))
        );
    }

    #[test]
    fn extrude_request_rejects_contradictions() {
        let (ids, _) = registry();
        let bad = [
            EditExtrudeParams { feature: "Cut-Extrude1".into(), end_condition: Some(EndCondition::ThroughAll), depth_mm: Some(5.0), ..Default::default() },
            EditExtrudeParams { feature: "Cut-Extrude1".into(), depth_mm: Some(0.0), ..Default::default() },
            EditExtrudeParams { feature: "Cut-Extrude1".into(), end_condition2: Some(EndCondition::MidPlane), ..Default::default() },
            EditExtrudeParams { feature: "Cut-Extrude1".into(), both_directions: Some(false), depth2_mm: Some(3.0), ..Default::default() },
            EditExtrudeParams { feature: "Cut-Extrude1".into(), draft_deg: Some(90.0), ..Default::default() },
            EditExtrudeParams { feature: "face:1".into(), depth_mm: Some(3.0), ..Default::default() },
            EditExtrudeParams { feature: "feature:99".into(), depth_mm: Some(3.0), ..Default::default() },
        ];
        for p in bad {
            assert!(extrude_request(&ids, &p).is_err(), "{p:?} accepted");
        }
    }

    fn direction(end: proto::EndCondition, depth: f64) -> Option<proto::ExtrudeDirection> {
        Some(proto::ExtrudeDirection {
            end_condition: end.into(),
            depth,
            ..Default::default()
        })
    }

    #[test]
    fn extrude_text_shows_before_and_after() {
        let before = proto::ExtrudeDefinition {
            name: "Cut-Extrude1".into(),
            cut: true,
            sketch_name: "Sketch2".into(),
            direction1: direction(proto::EndCondition::Blind, 0.003),
            ..Default::default()
        };
        let after = proto::ExtrudeDefinition {
            direction1: direction(proto::EndCondition::ThroughAll, 0.0),
            reverse_direction: true,
            ..before.clone()
        };
        let reply = proto::EditExtrudeResponse {
            before: Some(before.clone()),
            after: Some(after),
            new_errors: vec![],
        };
        let text = extrude_reply_text(&EditExtrudeParams::default(), &reply);
        assert!(
            text.starts_with("changed Cut-Extrude1 (cut-extrude of Sketch2): was blind 3 mm; now through all, reversed."),
            "{text}"
        );
        let reply = proto::EditExtrudeResponse { before: Some(before), after: None, new_errors: vec![] };
        let text = extrude_reply_text(&EditExtrudeParams::default(), &reply);
        assert!(text.starts_with("Cut-Extrude1 (cut-extrude of Sketch2): blind 3 mm."), "{text}");
        assert!(text.contains("Nothing was changed"), "{text}");
    }

    #[test]
    fn mate_checks() {
        let ok = EditMateParams { mate: "Coincident1".into(), distance_mm: Some(5.0), ..Default::default() };
        assert!(check_mate(&ok).unwrap().is_empty());
        let nothing = EditMateParams { mate: "Coincident1".into(), ..Default::default() };
        assert!(check_mate(&nothing).is_err());
        let one = EditMateParams { mate: "x".into(), entities: Some(vec!["face:1".into()]), ..Default::default() };
        assert!(check_mate(&one).is_err());
        let lock = EditMateParams {
            mate: "x".into(),
            kind: Some(MateKind::Lock),
            entities: Some(vec!["face:1".into(), "face:2".into()]),
            ..Default::default()
        };
        assert!(check_mate(&lock).is_err());
        let angle = EditMateParams { mate: "x".into(), angle_deg: Some(190.0), ..Default::default() };
        assert!(check_mate(&angle).is_err());
    }

    #[test]
    fn checks_mate_limits() {
        let ids = IdRegistry::default();
        let p = EditMateParams { mate: "Distance1".into(), min_mm: Some(5.0), max_mm: Some(40.0), ..Default::default() };
        assert!(check_mate(&p).is_ok());
        let r = mate_request(&ids, &p, vec![]).unwrap();
        assert_eq!((r.min_limit, r.max_limit), (Some(0.005), Some(0.04)));
        let p = EditMateParams { mate: "LimitAngle1".into(), max_deg: Some(90.0), ..Default::default() };
        let r = mate_request(&ids, &p, vec![]).unwrap();
        assert!(r.min_limit.is_none() && (r.max_limit.unwrap() - std::f64::consts::FRAC_PI_2).abs() < 1e-15);
        let p = EditMateParams { mate: "x".into(), remove_limits: true, ..Default::default() };
        assert!(check_mate(&p).is_ok() && mate_request(&ids, &p, vec![]).unwrap().remove_limits);

        for bad in [
            EditMateParams { mate: "x".into(), min_mm: Some(5.0), max_deg: Some(9.0), ..Default::default() },
            EditMateParams { mate: "x".into(), min_mm: Some(50.0), max_mm: Some(40.0), ..Default::default() },
            EditMateParams { mate: "x".into(), max_deg: Some(270.0), ..Default::default() },
            EditMateParams { mate: "x".into(), min_mm: Some(-1.0), ..Default::default() },
            EditMateParams { mate: "x".into(), min_mm: Some(1.0), remove_limits: true, ..Default::default() },
            EditMateParams { mate: "x".into(), kind: Some(MateKind::Angle), min_mm: Some(1.0), ..Default::default() },
            EditMateParams { mate: "x".into(), kind: Some(MateKind::Coincident), min_deg: Some(1.0), ..Default::default() },
        ] {
            assert!(check_mate(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn mate_text_shows_the_change() {
        let mut ids = IdRegistry::default();
        let before = proto::MateInfo {
            persist_id: b"m".to_vec(),
            name: "Coincident1".into(),
            r#type: proto::MateType::Coincident.into(),
            alignment: proto::MateAlignment::AntiAligned.into(),
            ..Default::default()
        };
        let after = proto::MateInfo {
            name: "Distance1".into(),
            r#type: proto::MateType::Distance.into(),
            dimension: Some(proto::DimensionInfo {
                name: "D1@Coincident1".into(),
                r#type: proto::DimensionType::Linear.into(),
                value: 0.005,
                driven: false,
            }),
            ..before.clone()
        };
        let reply = proto::EditMateResponse {
            before: Some(before),
            after: Some(after),
            entities: vec![proto::MateEntityState {
                kind: "face".into(),
                persist_id: b"f".to_vec(),
                component_name: "block-1".into(),
                ..Default::default()
            }],
            components: vec![],
            new_errors: vec![],
        };
        let text = mate_reply_text(&mut ids, &reply);
        assert!(
            text.starts_with("replaced Coincident1 with Distance1 (feature:1; a new type makes a new mate, named after its type): was coincident, anti-aligned; now distance, anti-aligned, 5 mm, between face:1 on block-1."),
            "{text}"
        );
    }

    #[test]
    fn mate_text_same_name() {
        let mut ids = IdRegistry::default();
        let m = proto::MateInfo {
            persist_id: b"m2".to_vec(),
            name: "Coincident2".into(),
            r#type: proto::MateType::Coincident.into(),
            ..Default::default()
        };
        let reply = proto::EditMateResponse { before: Some(m.clone()), after: Some(m), ..Default::default() };
        let text = mate_reply_text(&mut ids, &reply);
        assert!(text.starts_with("edited Coincident2 (now feature:1): was coincident; now coincident."), "{text}");
    }
}
