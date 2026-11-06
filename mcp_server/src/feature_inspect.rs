//! inspect_feature: one feature's definition, as JSON for the model.
//!
//! The add-in reads the definition through the interface SolidWorks hands out
//! for the feature's kind (extrude, revolve, fillet, ...) plus the feature's own
//! dimensions; this module names the enums, converts to mm / degrees and hands
//! out ids for the edges and faces it uses.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::delete::{feature_error_name, type_label};
use crate::edit::{feature_target, raw_end_condition_name};
use crate::feature_edit::fillet_kind;
use crate::hole::fastener_text;
use crate::inspect::{IdRegistry, Kind, mm, round};
use crate::sketch::dimension_json;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectFeatureParams {
    /// The feature: a "feature:N" id from inspect_model or its feature-tree name, e.g.
    /// "Revolve1".
    pub feature: String,
}

pub fn request(ids: &IdRegistry, p: &InspectFeatureParams) -> Result<proto::InspectFeatureRequest, McpError> {
    Ok(proto::InspectFeatureRequest { feature: Some(feature_target(ids, &p.feature, "feature")?) })
}

fn deg(radians: f64) -> f64 {
    round(radians.to_degrees(), 4)
}

/// Entities the definition uses, as ids; `used` notes that ids were handed out.
struct Entities<'a> {
    ids: &'a mut IdRegistry,
    used: bool,
}

impl Entities<'_> {
    fn id(&mut self, kind: Kind, persist_id: &[u8]) -> Option<String> {
        let id = self.ids.id_for(kind, persist_id)?;
        self.used = true;
        Some(id)
    }

    fn faces(&mut self, list: &[Vec<u8>]) -> Vec<String> {
        list.iter().filter_map(|p| self.id(Kind::Face, p)).collect()
    }

    fn items(&mut self, items: &[proto::FilletItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| self.id(if i.face { Kind::Face } else { Kind::Edge }, &i.persist_id))
            .collect()
    }

    /// A face or edge by id, anything else (a plane, an axis, ...) by what the add-in calls it.
    fn reference(&mut self, what: &str, persist_id: &[u8]) -> String {
        let kind = match what {
            "face" => Some(Kind::Face),
            "edge" => Some(Kind::Edge),
            _ => None,
        };
        kind.and_then(|k| self.id(k, persist_id)).unwrap_or_else(|| what.to_string())
    }

    fn definition_ref(&mut self, r: &Option<proto::DefinitionRef>) -> String {
        r.as_ref().map_or_else(|| "nothing".into(), |r| self.reference(&r.what, &r.persist_id))
    }
}

fn insert_if(obj: &mut Map<String, Value>, key: &str, on: bool) {
    if on {
        obj.insert(key.into(), json!(true));
    }
}

fn insert_list(obj: &mut Map<String, Value>, key: &str, list: Vec<String>) {
    if !list.is_empty() {
        obj.insert(key.into(), json!(list));
    }
}

fn direction_json(d: &proto::ExtrudeDirection) -> Value {
    use proto::EndCondition as E;
    let mut obj = Map::new();
    let condition = match d.end_condition() {
        E::Blind => "blind".into(),
        E::ThroughAll => "through_all".into(),
        E::MidPlane => "mid_plane".into(),
        E::UpToNext => "up_to_next".into(),
        E::ThroughAllBoth => "through_all_both".into(),
        E::Other | E::Unspecified => raw_end_condition_name(d.raw_end_condition),
    };
    obj.insert("end_condition".into(), json!(condition));
    if d.depth > 0.0 {
        obj.insert("depth_mm".into(), json!(mm(d.depth)));
    }
    if d.draft_angle > 0.0 {
        obj.insert("draft_deg".into(), json!(deg(d.draft_angle)));
        insert_if(&mut obj, "draft_outward", d.draft_outward);
    }
    Value::Object(obj)
}

fn extrude_json(d: &proto::ExtrudeDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    insert_if(&mut obj, "cut", d.cut);
    insert_if(&mut obj, "thin", d.thin);
    if !d.sketch_name.is_empty() {
        obj.insert("sketch".into(), json!(d.sketch_name));
    }
    obj.insert("direction1".into(), direction_json(&d.direction1.clone().unwrap_or_default()));
    if let Some(d2) = &d.direction2 {
        obj.insert("direction2".into(), direction_json(d2));
    }
    insert_if(&mut obj, "reverse_direction", d.reverse_direction);
    insert_if(&mut obj, "flip_side_to_cut", d.flip_side_to_cut);
    obj
}

fn revolve_json(e: &mut Entities, d: &proto::RevolveDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    insert_if(&mut obj, "cut", d.cut);
    insert_if(&mut obj, "thin", d.thin);
    if !d.sketch_name.is_empty() {
        obj.insert("sketch".into(), json!(d.sketch_name));
    }
    obj.insert("axis".into(), json!(e.definition_ref(&d.axis)));
    // swRevolveType_e; 3..5 are 0..2 all the way round.
    let full = (3..=5).contains(&d.revolve_type);
    let how = match d.revolve_type % 3 {
        0 => "one direction",
        1 => "mid plane",
        _ => "two directions",
    };
    obj.insert("type".into(), json!(how));
    obj.insert("angle_deg".into(), json!(if full { 360.0 } else { deg(d.angle) }));
    if d.revolve_type == 2 {
        obj.insert("angle2_deg".into(), json!(deg(d.angle2)));
    }
    insert_if(&mut obj, "reverse_direction", d.reverse_direction);
    obj
}

/// swTwistControlConstantTwistAlongPath.
const CONSTANT_TWIST: i32 = 8;

fn sweep_json(d: &proto::SweepDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    insert_if(&mut obj, "cut", d.cut);
    if d.profile.is_empty() {
        obj.insert("circle_diameter_mm".into(), json!(mm(d.circle_diameter)));
    } else {
        obj.insert("profile".into(), json!(d.profile));
    }
    obj.insert("path".into(), json!(d.path));
    if d.twist_control == CONSTANT_TWIST {
        obj.insert("twist_deg".into(), json!(deg(d.twist_angle)));
    }
    if d.guide_curves > 0 {
        obj.insert("guide_curves".into(), json!(d.guide_curves));
    }
    obj
}

/// swTangencyType_e.
fn tangency_name(t: i32) -> String {
    match t {
        0 => "none".into(),
        1 => "normal to profile".into(),
        2 => "direction vector".into(),
        3 => "all faces".into(),
        10 => "minimum twist".into(),
        t => format!("tangency {t}"),
    }
}

fn loft_json(d: &proto::LoftDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    insert_if(&mut obj, "cut", d.cut);
    insert_if(&mut obj, "thin", d.thin);
    obj.insert("profiles".into(), json!(d.profiles));
    insert_list(&mut obj, "guide_curves", d.guide_curves.clone());
    insert_if(&mut obj, "closed", d.closed);
    obj.insert("start_tangency".into(), json!(tangency_name(d.start_tangency)));
    obj.insert("end_tangency".into(), json!(tangency_name(d.end_tangency)));
    obj
}

fn fillet_json(e: &mut Entities, d: &proto::FilletDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("kind".into(), json!(fillet_kind(d.fillet_type)));
    obj.insert("radius_mm".into(), json!(mm(d.radius)));
    insert_if(&mut obj, "multiple_radius", d.multiple_radius);
    insert_if(&mut obj, "tangent_propagation", d.tangent_propagation);
    obj.insert("edges".into(), json!(e.items(&d.items)));
    if d.other_items > 0 {
        obj.insert("loops_or_features".into(), json!(d.other_items));
    }
    obj
}

fn chamfer_json(e: &mut Entities, d: &proto::ChamferDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    // swChamferType_e.
    let kind = match d.chamfer_type {
        1 => "angle-distance",
        2 => "distance-distance",
        3 => "vertex",
        16 => "equal distance",
        _ => "chamfer",
    };
    obj.insert("kind".into(), json!(kind));
    if d.chamfer_type == 3 {
        let distances: Vec<f64> = d.vertex_distances.iter().map(|v| mm(*v)).collect();
        obj.insert("distances_mm".into(), json!(distances));
        if let Some(v) = e.id(Kind::Vertex, &d.vertex_persist_id) {
            obj.insert("vertex".into(), json!(v));
        }
    } else {
        obj.insert("distance_mm".into(), json!(mm(d.distance)));
        if d.chamfer_type == 2 {
            obj.insert("distance2_mm".into(), json!(mm(d.distance2)));
        }
        if d.chamfer_type == 1 {
            obj.insert("angle_deg".into(), json!(deg(d.angle)));
        }
        obj.insert("edges".into(), json!(e.items(&d.items)));
    }
    insert_if(&mut obj, "tangent_propagation", d.tangent_propagation);
    obj
}

fn shell_json(e: &mut Entities, d: &proto::ShellDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("thickness_mm".into(), json!(mm(d.thickness)));
    insert_if(&mut obj, "outward", d.outward);
    let removed = e.faces(&d.removed_faces);
    if removed.is_empty() {
        obj.insert("closed".into(), json!(true));
    } else {
        obj.insert("removed_faces".into(), json!(removed));
    }
    let walls: Vec<Value> = d
        .walls
        .iter()
        .filter_map(|w| Some(json!({"face": e.id(Kind::Face, &w.persist_id)?, "thickness_mm": mm(w.thickness)})))
        .collect();
    if !walls.is_empty() {
        obj.insert("walls".into(), json!(walls));
    }
    obj
}

fn draft_json(e: &mut Entities, d: &proto::DraftDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    // swDraftType_e.
    let kind = match d.draft_type {
        0 => "neutral plane",
        1 => "parting line",
        3 => "step",
        _ => "draft",
    };
    obj.insert("kind".into(), json!(kind));
    obj.insert("angle_deg".into(), json!(deg(d.angle)));
    insert_if(&mut obj, "reverse_direction", d.reverse_direction);
    if d.neutral_plane.is_some() {
        obj.insert("neutral_plane".into(), json!(e.definition_ref(&d.neutral_plane)));
    }
    obj.insert("faces".into(), json!(e.faces(&d.faces)));
    // swDraftFacePropagationType_e, named as draft_faces takes it.
    let propagation = match d.propagation {
        0 => "none",
        1 => "tangent",
        2 => "all",
        3 => "inner",
        4 => "outer",
        _ => "other",
    };
    obj.insert("propagation".into(), json!(propagation));
    obj
}

fn rib_json(d: &proto::RibDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    if !d.sketch_name.is_empty() {
        obj.insert("sketch".into(), json!(d.sketch_name));
    }
    obj.insert("thickness_mm".into(), json!(mm(d.thickness)));
    // As rib_sketch's `side`.
    let side = if d.two_sided {
        "both"
    } else if d.reverse_thickness {
        "other"
    } else {
        "one"
    };
    obj.insert("side".into(), json!(side));
    insert_if(&mut obj, "normal_to_sketch", d.normal_to_sketch);
    insert_if(&mut obj, "flip_material", d.flip_material);
    if d.draft_angle > 0.0 {
        obj.insert("draft_deg".into(), json!(deg(d.draft_angle)));
        insert_if(&mut obj, "draft_outward", d.draft_outward);
    }
    obj
}

fn mirror_json(e: &mut Entities, d: &proto::MirrorDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("plane".into(), json!(e.definition_ref(&d.plane)));
    insert_list(&mut obj, "features", d.features.clone());
    if d.bodies > 0 {
        obj.insert("bodies".into(), json!(d.bodies));
    }
    if d.faces > 0 {
        obj.insert("faces".into(), json!(d.faces));
    }
    insert_if(&mut obj, "geometry_pattern", d.geometry_pattern);
    obj
}

fn pattern_side_json(e: &mut Entities, s: &proto::PatternSide, circular: bool) -> Value {
    let mut obj = Map::new();
    obj.insert(if circular { "axis" } else { "along" }.into(), json!(e.reference(&s.what, &s.persist_id)));
    obj.insert("count".into(), json!(s.count));
    if circular {
        obj.insert("spacing_deg".into(), json!(deg(s.spacing)));
    } else {
        obj.insert("spacing_mm".into(), json!(mm(s.spacing)));
    }
    insert_if(&mut obj, "reverse_direction", s.reverse);
    Value::Object(obj)
}

fn pattern_json(e: &mut Entities, d: &proto::PatternDefinition) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("kind".into(), json!(if d.circular { "circular" } else { "linear" }));
    insert_list(&mut obj, "features", d.features.clone());
    if d.bodies > 0 {
        obj.insert("bodies".into(), json!(d.bodies));
    }
    let side1 = d.direction1.clone().unwrap_or_default();
    obj.insert("direction1".into(), pattern_side_json(e, &side1, d.circular));
    if let Some(s2) = &d.direction2 {
        obj.insert("direction2".into(), pattern_side_json(e, s2, false));
    }
    if d.circular && d.equal_spacing {
        // Spacing is then the total angle, spread evenly.
        obj.insert("equal_spacing".into(), json!(true));
    }
    obj
}

fn helix_json(d: &proto::HelixFeatureDefinition) -> Map<String, Value> {
    use proto::HelixDefinition as H;
    let mut obj = Map::new();
    let defined_by = match d.defined_by() {
        H::PitchAndRevolutions => "pitch and revolutions",
        H::HeightAndRevolutions => "height and revolutions",
        H::HeightAndPitch => "height and pitch",
        H::Spiral => "spiral",
        H::Unspecified => "other",
    };
    obj.insert("defined_by".into(), json!(defined_by));
    obj.insert("height_mm".into(), json!(mm(d.height)));
    obj.insert("pitch_mm".into(), json!(mm(d.pitch)));
    obj.insert("revolutions".into(), json!(round(d.revolutions, 4)));
    obj.insert("start_angle_deg".into(), json!(deg(d.start_angle)));
    insert_if(&mut obj, "clockwise", d.clockwise);
    insert_if(&mut obj, "reverse_direction", d.reverse_direction);
    if d.taper_angle > 0.0 {
        obj.insert("taper_deg".into(), json!(deg(d.taper_angle)));
        insert_if(&mut obj, "taper_outward", d.taper_outward);
    }
    insert_if(&mut obj, "variable_pitch", d.variable_pitch);
    obj
}

fn hole_json(d: &proto::HoleSpec) -> Map<String, Value> {
    let mut obj = Map::new();
    if !d.size.is_empty() {
        obj.insert("size".into(), json!(d.size));
    }
    let fastener = fastener_text(d.standard, d.fastener_type);
    if !fastener.is_empty() {
        obj.insert("fastener".into(), json!(fastener));
    }
    insert_if(&mut obj, "through_all", d.through_all);
    for (key, value) in [
        ("diameter_mm", d.diameter),
        ("depth_mm", d.depth),
        ("counterbore_diameter_mm", d.counterbore_diameter),
        ("counterbore_depth_mm", d.counterbore_depth),
        ("countersink_diameter_mm", d.countersink_diameter),
        ("tap_drill_diameter_mm", d.tap_drill_diameter),
        ("thread_diameter_mm", d.thread_diameter),
        ("thread_depth_mm", d.thread_depth),
    ] {
        if value > 0.0 {
            obj.insert(key.into(), json!(mm(value)));
        }
    }
    if d.countersink_angle > 0.0 {
        obj.insert("countersink_angle_deg".into(), json!(deg(d.countersink_angle)));
    }
    obj
}

pub fn render(ids: &mut IdRegistry, reply: &proto::InspectFeatureResponse) -> Value {
    use proto::inspect_feature_response::Definition as D;
    let mut obj = Map::new();
    obj.insert("feature".into(), json!(reply.name));
    obj.insert("type".into(), json!(type_label(&reply.type_name)));
    obj.insert("type_name".into(), json!(reply.type_name));
    insert_if(&mut obj, "suppressed", reply.suppressed);
    if reply.error_code != 0 {
        let key = if reply.error_is_warning { "warning" } else { "error" };
        obj.insert(key.into(), json!(feature_error_name(reply.error_code)));
    }

    let mut e = Entities { ids, used: false };
    let (definition, edit_with) = match &reply.definition {
        Some(D::Extrude(d)) => (extrude_json(d), Some("edit_extrude")),
        Some(D::Revolve(d)) => (revolve_json(&mut e, d), None),
        Some(D::Sweep(d)) => (sweep_json(d), Some("edit_sweep")),
        Some(D::Loft(d)) => (loft_json(d), None),
        Some(D::Fillet(d)) => (fillet_json(&mut e, d), (d.fillet_type == 0).then_some("edit_fillet")),
        Some(D::Chamfer(d)) => (chamfer_json(&mut e, d), None),
        Some(D::Shell(d)) => (shell_json(&mut e, d), None),
        Some(D::Draft(d)) => (draft_json(&mut e, d), None),
        Some(D::Rib(d)) => (rib_json(d), None),
        Some(D::Mirror(d)) => (mirror_json(&mut e, d), None),
        Some(D::Pattern(d)) => (pattern_json(&mut e, d), Some("edit_pattern")),
        Some(D::Helix(d)) => (helix_json(d), None),
        Some(D::Hole(d)) => (hole_json(d), None),
        None => (Map::new(), None),
    };
    if reply.definition.is_some() {
        obj.insert("definition".into(), Value::Object(definition));
    }
    // Its own: SolidWorks also hands out others, e.g. a loft its profile plane's offset.
    let own: Vec<&proto::DimensionInfo> = reply
        .dimensions
        .iter()
        .filter(|d| d.name.split('@').nth(1).is_none_or(|f| f == reply.name))
        .collect();
    // A pattern's count and a helix's turns come as OTHER: plain numbers, not meters.
    let dimensions: Vec<Value> = own
        .iter()
        .map(|d| match d.r#type() {
            proto::DimensionType::Other => json!({"name": d.name, "type": "other", "value": round(d.value, 6)}),
            _ => dimension_json(d),
        })
        .collect();
    obj.insert("dimensions".into(), json!(dimensions));

    let mut notes = Vec::new();
    if reply.definition.is_none() {
        notes.push("SolidWorks' definition of this kind of feature isn't read; its dimensions are.".to_string());
    }
    if !own.is_empty() {
        notes.push("set_dimension changes a dimension.".into());
    }
    if let Some(tool) = edit_with {
        notes.push(format!("{tool} changes the other settings."));
    }
    if e.used {
        notes.push(format!(
            "Edge and face ids are as the model is just before {} (ones it consumes are gone \
             after it); to look at them, rollback before={:?} and inspect_model.",
            reply.name, reply.name
        ));
    }
    if !notes.is_empty() {
        obj.insert("note".into(), json!(notes.join(" ")));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_takes_names_and_feature_ids() {
        let mut ids = IdRegistry::default();
        let feature = ids.id_for(Kind::Feature, b"feat").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        use proto::feature_target::Target;
        let by_name = request(&ids, &InspectFeatureParams { feature: " Revolve1 ".into() }).unwrap();
        assert_eq!(by_name.feature.unwrap().target, Some(Target::Name("Revolve1".into())));
        let by_id = request(&ids, &InspectFeatureParams { feature: feature.clone() }).unwrap();
        assert_eq!(by_id.feature.unwrap().target, Some(Target::PersistId(b"feat".to_vec())));
        assert!(request(&ids, &InspectFeatureParams { feature: face }).is_err());
        assert!(request(&ids, &InspectFeatureParams { feature: "".into() }).is_err());
    }

    #[test]
    fn revolve_in_degrees_with_its_axis() {
        let mut ids = IdRegistry::default();
        let reply = proto::InspectFeatureResponse {
            name: "Revolve1".into(),
            type_name: "Revolution".into(),
            definition: Some(proto::inspect_feature_response::Definition::Revolve(proto::RevolveDefinition {
                sketch_name: "Sketch1".into(),
                axis: Some(proto::DefinitionRef { what: "edge".into(), persist_id: b"edge".to_vec() }),
                revolve_type: 2,
                angle: std::f64::consts::FRAC_PI_2,
                angle2: std::f64::consts::FRAC_PI_4,
                ..Default::default()
            })),
            dimensions: vec![proto::DimensionInfo {
                name: "D1@Revolve1".into(),
                r#type: proto::DimensionType::Angular.into(),
                value: std::f64::consts::FRAC_PI_2,
                driven: false,
            }],
            ..Default::default()
        };
        let json = render(&mut ids, &reply);
        let d = &json["definition"];
        assert_eq!(json["type"], "revolve");
        assert_eq!(d["type"], "two directions");
        assert_eq!(d["angle_deg"], 90.0);
        assert_eq!(d["angle2_deg"], 45.0);
        assert_eq!(d["axis"], ids.existing_id(b"edge").unwrap());
        assert_eq!(json["dimensions"][0]["value_deg"], 90.0);
        assert!(json["note"].as_str().unwrap().contains("rollback before=\"Revolve1\""));
    }

    #[test]
    fn shell_closed_or_open_and_unknown_kinds() {
        let mut ids = IdRegistry::default();
        use proto::inspect_feature_response::Definition as D;
        let shell = |removed: Vec<Vec<u8>>| proto::InspectFeatureResponse {
            name: "Shell1".into(),
            type_name: "Shell".into(),
            definition: Some(D::Shell(proto::ShellDefinition {
                thickness: 0.002,
                removed_faces: removed,
                walls: vec![proto::ShellWall { persist_id: b"wall".to_vec(), thickness: 0.004 }],
                ..Default::default()
            })),
            ..Default::default()
        };
        let closed = render(&mut ids, &shell(vec![]));
        assert_eq!(closed["definition"]["closed"], true);
        assert_eq!(closed["definition"]["thickness_mm"], 2.0);
        assert_eq!(closed["definition"]["walls"][0]["thickness_mm"], 4.0);
        let open = render(&mut ids, &shell(vec![b"top".to_vec()]));
        assert_eq!(open["definition"]["removed_faces"][0], ids.existing_id(b"top").unwrap());
        assert!(open["definition"].get("closed").is_none());

        let other = render(
            &mut ids,
            &proto::InspectFeatureResponse {
                name: "Thicken1".into(),
                type_name: "Thicken".into(),
                error_code: 1,
                ..Default::default()
            },
        );
        assert!(other.get("definition").is_none());
        assert!(other["error"].is_string());
        assert!(other["note"].as_str().unwrap().contains("isn't read"));
    }
}
