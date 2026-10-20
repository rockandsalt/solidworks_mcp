//! diagnose_model: what's wrong with the active part's or assembly's feature
//! tree, as JSON for the model.
//!
//! The add-in lists every feature that fails or warns on rebuild, every
//! sketch with dangling / conflicting relations or a missing sketch plane,
//! and every mate that lost an entity, each with its parents. This module
//! works out which problems are root causes (none of their parents is a
//! problem -- typically the features that referenced something deleted)
//! and which only fail because a parent does, names the error codes, and
//! suggests a fix for the root causes.

use std::collections::HashSet;

use rmcp::schemars;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::delete::{feature_error_name, type_label};
use crate::inspect::{IdRegistry, Kind};
use crate::sketch;

/// Most root causes named in the summary line.
const SUMMARY_ROOTS: usize = 10;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DiagnoseModelParams {
    /// Rebuild first (default true), so the errors are current. false reports them as of the
    /// last rebuild.
    #[serde(default)]
    pub rebuild: Option<bool>,
}

impl DiagnoseModelParams {
    pub fn request(&self) -> proto::DiagnoseModelRequest {
        proto::DiagnoseModelRequest { skip_rebuild: self.rebuild == Some(false) }
    }
}

fn relation_problem_name(p: proto::RelationProblem) -> &'static str {
    use proto::RelationProblem as P;
    match p {
        P::Dangling => "dangling",
        P::OverDefining => "over-defining",
        P::Broken => "broken external reference",
        P::Unspecified => "problem",
    }
}

/// swSketchRelationEntityTypes_e.
fn relation_entity_type_name(t: i32) -> &'static str {
    match t {
        1 => "sub-sketch",
        2 => "point",
        3 => "line",
        4 => "arc",
        5 => "ellipse",
        6 => "parabola",
        7 => "spline",
        8 => "hatch",
        9 => "text",
        10 => "plane",
        11 => "cylinder",
        12 => "sphere",
        13 => "surface",
        14 => "dimension",
        _ => "unknown",
    }
}

/// swConstraintType_e values that are dimensions rather than relations.
fn is_dimension(constraint_type: i32) -> bool {
    matches!(constraint_type, 1 | 2 | 3 | 15)
}

fn sketch_relation_json(ids: &mut IdRegistry, r: &proto::SketchRelationProblem) -> Value {
    let mut obj = Map::new();
    obj.insert("problem".into(), json!(relation_problem_name(r.problem())));
    if !r.dimension_name.is_empty() {
        obj.insert("dimension".into(), json!(r.dimension_name));
    } else if is_dimension(r.constraint_type) {
        obj.insert("type".into(), json!("dimension"));
    } else {
        obj.insert("type".into(), json!(sketch::relation_name(r.constraint_type)));
    }
    let mut entities: Vec<Value> = r
        .items
        .iter()
        .filter_map(|item| sketch::relation_entity_id(ids, item).map(Value::String))
        .collect();
    for _ in 0..r.external_count {
        entities.push(json!("external"));
    }
    for _ in 0..r.missing_count {
        entities.push(json!("missing"));
    }
    obj.insert("entities".into(), Value::Array(entities));
    if r.missing_count > 0 && !r.entity_types.is_empty() {
        let types: Vec<&str> = r.entity_types.iter().map(|t| relation_entity_type_name(*t)).collect();
        obj.insert("between".into(), json!(types));
    }
    Value::Object(obj)
}

fn sketch_json(ids: &mut IdRegistry, s: &proto::SketchDiagnosis) -> Value {
    let mut obj = Map::new();
    obj.insert("status".into(), json!(sketch::status_name(s.status())));
    if s.plane_missing {
        obj.insert("plane_missing".into(), json!(true));
    }
    if !s.relations.is_empty() {
        let relations = s.relations.iter().map(|r| sketch_relation_json(ids, r)).collect();
        obj.insert("relations".into(), Value::Array(relations));
    }
    if !s.dangling_dimensions.is_empty() {
        obj.insert("dangling_dimensions".into(), json!(s.dangling_dimensions));
    }
    Value::Object(obj)
}

pub fn mate_entity_json(ids: &mut IdRegistry, e: &proto::MateEntityState) -> Value {
    let mut obj = Map::new();
    if e.missing {
        obj.insert("missing".into(), json!(true));
    } else {
        obj.insert("kind".into(), json!(e.kind));
        let kind = match e.kind.as_str() {
            "face" => Some(Kind::Face),
            "edge" => Some(Kind::Edge),
            "component" => Some(Kind::Component),
            "plane" | "axis" | "point" | "coordinate system" => Some(Kind::Feature),
            _ => None,
        };
        if let Some(id) = kind.and_then(|k| ids.id_for(k, &e.persist_id)) {
            obj.insert("id".into(), json!(id));
        }
    }
    if let Some(id) = ids.id_for(Kind::Component, &e.component_persist_id) {
        obj.insert("component".into(), json!(id));
    }
    if !e.component_name.is_empty() {
        obj.insert("component_name".into(), json!(e.component_name));
    }
    Value::Object(obj)
}

fn is_mate(p: &proto::FeatureDiagnosis) -> bool {
    p.type_name.starts_with("Mate") && !matches!(p.type_name.as_str(), "MateGroup" | "MateReferenceGroupFeature")
}

/// An assembly's component: SolidWorks flags it (and What's Wrong lists it)
/// when its part fails to rebuild, rather than listing the part's features.
fn is_component(type_name: &str) -> bool {
    type_name == "Reference"
}

/// One phrase saying what's wrong with a problem feature.
fn what_is_wrong(p: &proto::FeatureDiagnosis) -> String {
    let mut found = Vec::new();
    if let Some(s) = &p.sketch {
        if s.plane_missing {
            found.push("its sketch plane/face is gone".to_string());
        }
        let count = |kind: proto::RelationProblem| s.relations.iter().filter(|r| r.problem() == kind).count();
        let dangling = count(proto::RelationProblem::Dangling) + s.dangling_dimensions.len();
        if dangling > 0 {
            found.push(plural(dangling, "dangling relation/dimension", "dangling relations/dimensions"));
        }
        let broken = count(proto::RelationProblem::Broken);
        if broken > 0 {
            found.push(plural(broken, "broken external reference", "broken external references"));
        }
        let over = count(proto::RelationProblem::OverDefining);
        if over > 0 {
            found.push(plural(over, "over-defining relation/dimension", "over-defining relations/dimensions"));
        }
    }
    let missing = p.mate_entities.iter().filter(|e| e.missing).count();
    if missing > 0 && !matches!(p.error_code, 43 | 48) {
        found.push(format!("{missing} mated entit{} missing", if missing == 1 { "y is" } else { "ies are" }));
    }

    let mut parts = Vec::new();
    if is_component(&p.type_name) && p.error_code == 1 {
        parts.push("its part has rebuild errors of its own".to_string());
    } else if p.error_code != 0 && !(p.error_code == 1 && !found.is_empty()) {
        // ("Unknown error" says nothing once something specific was found.)
        parts.push(feature_error_name(p.error_code));
    }
    parts.extend(found);
    if parts.is_empty() {
        if let Some(s) = &p.sketch {
            parts.push(format!("the sketch is {}", sketch::status_name(s.status())));
        } else {
            parts.push("SolidWorks lists it in What's Wrong".into());
        }
    }
    parts.join("; ")
}

/// A suggested fix for a root-cause problem, naming this server's tools
/// where one fits.
fn fix_hint(p: &proto::FeatureDiagnosis) -> Option<String> {
    if let Some(s) = &p.sketch {
        if s.plane_missing {
            return Some(format!(
                "the face or plane {} was sketched on is gone. No tool can move a sketch to a new \
                 plane yet: in SolidWorks right-click it > Edit Sketch Plane, or delete_features it \
                 and sketch again on a current face",
                p.name
            ));
        }
        if s.relations.iter().any(|r| r.problem() == proto::RelationProblem::Dangling)
            || !s.dangling_dimensions.is_empty()
        {
            return Some(
                "these relations/dimensions point at geometry that's gone. No tool deletes or \
                 re-attaches relations yet: in SolidWorks use Display/Delete Relations (filter \
                 Dangling); or delete_sketch_entities the entities they're on and redraw them"
                    .into(),
            );
        }
        if s.relations.iter().any(|r| r.problem() == proto::RelationProblem::OverDefining) {
            return Some(format!(
                "relations/dimensions conflict: inspect_sketch {} to see them, then remove the \
                 extra ones",
                p.name
            ));
        }
    }
    if is_component(&p.type_name) {
        return Some(format!(
            "the part {} uses fails to rebuild; SolidWorks lists the component, not that part's \
             features. No tool opens a component's part yet: open it in SolidWorks (right-click > \
             Open Part) and run diagnose_model there",
            p.name
        ));
    }
    if is_mate(p) {
        return Some(match p.error_code {
            46 => "it conflicts with other mates: delete_features this mate (or the one it fights)".into(),
            47 => "the solver can't satisfy it from where the components are: delete_features it \
                   and add_mate again, maybe with another alignment"
                .into(),
            _ if p.mate_entities.iter().any(|e| e.missing) || matches!(p.error_code, 43 | 45 | 48) => {
                "a mated face/edge/plane is gone: delete_features this mate and add_mate again \
                 with current ids from inspect_model"
                    .into()
            }
            _ => return None,
        });
    }
    let parents = if p.parents.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = p.parents.iter().map(|f| f.name.as_str()).collect();
        format!(" (it uses {})", names.join(", "))
    };
    Some(match p.error_code {
        10..=19 | 52..=68 => format!(
            "the edges/faces it rounds are gone{parents}: delete_features it and make it again on \
             current edges, or edit it in SolidWorks to pick new ones"
        ),
        31 => format!("its end condition's face/vertex is gone or out of reach{parents}"),
        33..=37 | 72 => format!(
            "its sketch profile isn't a valid closed contour any more: inspect_sketch to see the \
             contours, then close gaps with sketch_line or remove stray segments with \
             delete_sketch_entities{parents}"
        ),
        51 => "a sketch entity referenced model geometry (converted/offset edge, or a relation to \
               an edge) that's gone"
            .into(),
        71 => format!("the cut no longer touches the body{parents}: the material it cut is gone or its sketch moved"),
        73 => format!("items it uses are missing{parents}, e.g. a pattern's seed features or a hole's placement points"),
        1 => format!("most often a reference to deleted geometry{parents}"),
        _ => return None,
    })
}

/// The names of problems that count as failing for root-cause purposes.
fn failing_names(problems: &[proto::FeatureDiagnosis]) -> HashSet<&str> {
    problems.iter().map(|p| p.name.as_str()).collect()
}

/// The problem's parents that are problems too. A failing component doesn't
/// explain a mate's problem (its part's errors needn't touch the mated
/// faces), so a mate's components don't count.
fn caused_by<'a>(p: &'a proto::FeatureDiagnosis, failing: &HashSet<&str>) -> Vec<&'a str> {
    p.parents
        .iter()
        .filter(|f| !(is_mate(p) && is_component(&f.type_name)))
        .map(|f| f.name.as_str())
        .filter(|name| *name != p.name && failing.contains(name))
        .collect()
}

fn problem_json(
    ids: &mut IdRegistry,
    p: &proto::FeatureDiagnosis,
    causes: &[&str],
) -> Value {
    let mut obj = Map::new();
    if let Some(id) = ids.id_for(Kind::Feature, &p.persist_id) {
        obj.insert("id".into(), json!(id));
    }
    obj.insert("name".into(), json!(p.name));
    obj.insert("type".into(), json!(p.type_name));
    let label = type_label(&p.type_name);
    if label != p.type_name {
        obj.insert("kind".into(), json!(label));
    }
    if let Some(id) = ids.id_for(Kind::Component, &p.component_persist_id) {
        obj.insert("component".into(), json!(id));
    }
    let key = if p.error_code != 0 && p.warning { "warning" } else { "problem" };
    obj.insert(key.into(), json!(what_is_wrong(p)));
    if p.error_code != 0 {
        obj.insert("error_code".into(), json!(p.error_code));
    }
    if p.suppressed {
        obj.insert("suppressed".into(), json!(true));
    }
    if causes.is_empty() {
        obj.insert("root_cause".into(), json!(true));
        if let Some(fix) = fix_hint(p) {
            obj.insert("fix".into(), json!(fix));
        }
    } else {
        obj.insert("caused_by".into(), json!(causes));
    }
    if !p.parents.is_empty() {
        let parents: Vec<&str> = p.parents.iter().map(|f| f.name.as_str()).collect();
        obj.insert("parents".into(), json!(parents));
    }
    if !p.children.is_empty() {
        let children: Vec<&str> = p.children.iter().map(|f| f.name.as_str()).collect();
        obj.insert("children".into(), json!(children));
    }
    if let Some(s) = &p.sketch {
        obj.insert("sketch".into(), sketch_json(ids, s));
    }
    if !p.mate_entities.is_empty() && (is_mate(p) || p.mate_entities.iter().any(|e| e.missing)) {
        let entities = p.mate_entities.iter().map(|e| mate_entity_json(ids, e)).collect();
        obj.insert("mated_entities".into(), Value::Array(entities));
    }
    Value::Object(obj)
}

fn plural(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 { format!("1 {singular}") } else { format!("{n} {plural}") }
}

pub fn render(ids: &mut IdRegistry, reply: &proto::DiagnoseModelResponse) -> Value {
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }

    let failing = failing_names(&reply.problems);
    let causes: Vec<Vec<&str>> = reply.problems.iter().map(|p| caused_by(p, &failing)).collect();
    // Root causes first, each group in tree order.
    let mut order: Vec<usize> = (0..reply.problems.len()).collect();
    order.sort_by_key(|&i| !causes[i].is_empty());
    let roots: Vec<&proto::FeatureDiagnosis> =
        order.iter().filter(|&&i| causes[i].is_empty()).map(|&i| &reply.problems[i]).collect();
    let downstream = reply.problems.len() - roots.len();

    let checked = format!(
        "checked {} and {}",
        plural(reply.checked_features.max(0) as usize, "feature", "features"),
        plural(reply.checked_sketches.max(0) as usize, "sketch", "sketches")
    );
    let summary = if reply.problems.is_empty() {
        format!("no rebuild errors, warnings or sketch problems ({checked})")
    } else {
        let named: Vec<String> = roots
            .iter()
            .take(SUMMARY_ROOTS)
            .map(|p| format!("{} ({})", p.name, what_is_wrong(p)))
            .collect();
        let more = if roots.len() > SUMMARY_ROOTS {
            format!(" and {} more", roots.len() - SUMMARY_ROOTS)
        } else {
            String::new()
        };
        let mut text = format!(
            "{}; root cause{}: {}{more}.",
            plural(reply.problems.len(), "problem", "problems"),
            if roots.len() == 1 { "" } else { "s" },
            named.join("; ")
        );
        if downstream > 0 {
            text += &format!(
                " {} only because of {} -- fix the root causes first.",
                plural(downstream, "more feature fails", "more features fail"),
                if roots.len() == 1 { "it" } else { "those" }
            );
        }
        text += &format!(" ({checked})");
        text
    };
    out.insert("summary".into(), json!(summary));
    if !reply.rebuilt {
        out.insert(
            "note".into(),
            json!("the model wasn't rebuilt first; errors are as of the last rebuild"),
        );
    }
    if !reply.problems.is_empty() {
        let problems = order.iter().map(|&i| problem_json(ids, &reply.problems[i], &causes[i])).collect();
        out.insert("problems".into(), Value::Array(problems));
    }
    if reply.truncated {
        out.insert(
            "warning".into(),
            json!("there are more problems than the add-in reports at once; fix the root causes and run again"),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(name: &str, type_name: &str) -> proto::FeatureRef {
        proto::FeatureRef { name: name.into(), type_name: type_name.into() }
    }

    fn reply() -> proto::DiagnoseModelResponse {
        let sketch = proto::FeatureDiagnosis {
            persist_id: b"sk2".to_vec(),
            name: "Sketch2".into(),
            type_name: "ProfileFeature".into(),
            error_code: 1,
            parents: vec![],
            children: vec![feature("Cut-Extrude1", "ICE")],
            sketch: Some(proto::SketchDiagnosis {
                status: proto::DefinitionStatus::Under as i32,
                relations: vec![proto::SketchRelationProblem {
                    problem: proto::RelationProblem::Dangling as i32,
                    constraint_type: 9,
                    items: vec![proto::SketchItemRef {
                        sketch_persist_id: b"sk2".to_vec(),
                        point: true,
                        id0: 4,
                        id1: 1,
                        ..Default::default()
                    }],
                    entity_types: vec![2, 3],
                    missing_count: 1,
                    ..Default::default()
                }],
                dangling_dimensions: vec!["D2@Sketch2".into()],
                plane_missing: false,
            }),
            ..Default::default()
        };
        let cut = proto::FeatureDiagnosis {
            persist_id: b"cut1".to_vec(),
            name: "Cut-Extrude1".into(),
            type_name: "ICE".into(),
            error_code: 34,
            parents: vec![feature("Sketch2", "ProfileFeature"), feature("Boss-Extrude1", "Extrusion")],
            ..Default::default()
        };
        let mate = proto::FeatureDiagnosis {
            persist_id: b"mate".to_vec(),
            name: "Coincident3".into(),
            type_name: "MateCoincident".into(),
            error_code: 48,
            mate_entities: vec![
                proto::MateEntityState {
                    kind: "face".into(),
                    persist_id: b"face-a".to_vec(),
                    component_persist_id: b"comp-a".to_vec(),
                    component_name: "plate-1".into(),
                    ..Default::default()
                },
                proto::MateEntityState {
                    missing: true,
                    component_name: "bracket-1".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        proto::DiagnoseModelResponse {
            document: Some(proto::Document { title: "Part1".into(), r#type: 0, path: String::new() }),
            rebuilt: true,
            problems: vec![cut, sketch, mate],
            checked_features: 12,
            checked_sketches: 3,
            truncated: false,
        }
    }

    #[test]
    fn root_causes_come_first_and_downstream_points_at_them() {
        let mut ids = IdRegistry::default();
        let json = render(&mut ids, &reply());
        let problems = json["problems"].as_array().unwrap();
        assert_eq!(problems[0]["name"], "Sketch2");
        assert_eq!(problems[0]["root_cause"], true);
        assert_eq!(problems[1]["name"], "Coincident3");
        assert_eq!(problems[2]["name"], "Cut-Extrude1");
        assert_eq!(problems[2]["caused_by"], json!(["Sketch2"]));
        assert!(problems[2].get("fix").is_none());
        let summary = json["summary"].as_str().unwrap();
        assert!(summary.starts_with("3 problems; root causes: Sketch2 ("), "{summary}");
        assert!(summary.contains("1 more feature fails only because of those"), "{summary}");
        assert!(summary.ends_with("(checked 12 features and 3 sketches)"), "{summary}");
        assert!(json.get("note").is_none());
    }

    #[test]
    fn sketch_and_mate_details() {
        let mut ids = IdRegistry::default();
        let json = render(&mut ids, &reply());
        let sketch = &json["problems"][0];
        assert_eq!(sketch["kind"], "sketch");
        let rel = &sketch["sketch"]["relations"][0];
        assert_eq!(rel["problem"], "dangling");
        assert_eq!(rel["type"], "coincident");
        assert_eq!(rel["entities"], json!(["point:1", "missing"]));
        assert_eq!(rel["between"], json!(["point", "line"]));
        assert_eq!(sketch["sketch"]["dangling_dimensions"], json!(["D2@Sketch2"]));
        assert_eq!(sketch["problem"], "2 dangling relations/dimensions");
        assert!(sketch["fix"].as_str().unwrap().contains("Display/Delete Relations"));

        let mate = &json["problems"][1];
        assert_eq!(mate["problem"], "a mated entity is missing");
        assert_eq!(mate["mated_entities"][0]["id"], "face:1");
        assert_eq!(mate["mated_entities"][0]["component"], "component:1");
        assert_eq!(mate["mated_entities"][1], json!({"missing": true, "component_name": "bracket-1"}));
        assert!(mate["fix"].as_str().unwrap().contains("add_mate"));
    }

    #[test]
    fn a_failing_component_is_its_own_root_cause_and_not_the_mates() {
        let component = proto::FeatureDiagnosis {
            persist_id: b"comp-a".to_vec(),
            name: "block-1".into(),
            type_name: "Reference".into(),
            error_code: 1,
            whats_wrong: true,
            ..Default::default()
        };
        let mut reply = reply();
        reply.problems[2].parents = vec![feature("block-1", "Reference"), feature("Mates", "MateGroup")];
        reply.problems.insert(0, component);
        let mut ids = IdRegistry::default();
        let json = render(&mut ids, &reply);
        let block = &json["problems"][0];
        assert_eq!(block["problem"], "its part has rebuild errors of its own");
        assert!(block["fix"].as_str().unwrap().contains("Open Part"), "{block}");
        let mate = json["problems"].as_array().unwrap().iter().find(|p| p["name"] == "Coincident3").unwrap();
        assert_eq!(mate["root_cause"], true, "{mate}");
        assert!(mate["fix"].as_str().unwrap().contains("add_mate"));
    }

    #[test]
    fn clean_model() {
        let mut ids = IdRegistry::default();
        let reply = proto::DiagnoseModelResponse {
            checked_features: 1,
            checked_sketches: 0,
            ..Default::default()
        };
        let json = render(&mut ids, &reply);
        assert_eq!(json["summary"], "no rebuild errors, warnings or sketch problems (checked 1 feature and 0 sketches)");
        assert!(json.get("problems").is_none());
        assert!(json["note"].as_str().unwrap().contains("wasn't rebuilt"));
    }

    #[test]
    fn rebuild_defaults_on() {
        assert!(!DiagnoseModelParams::default().request().skip_rebuild);
        assert!(DiagnoseModelParams { rebuild: Some(false) }.request().skip_rebuild);
    }
}
