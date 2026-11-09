//! The mirror tools (mirror_features, mirror_sketch_entities): their parameters,
//! the plane / line references, and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::delete::problems_text;
use crate::inspect::{IdRegistry, Kind, SKETCH_KINDS};
use crate::refgeom::{self, Shape};
use crate::sketch;

/// The add-in's limits.
const MAX_FEATURES: usize = 500;
const MAX_SKETCH_ENTITIES: usize = 1000;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MirrorFeaturesParams {
    /// What to mirror about: "front", "top" or "right" (the default planes), a planar face
    /// id ("face:3") from inspect_model, or a reference plane ("feature:4", or its name such
    /// as "Plane1").
    pub plane: String,
    /// Features to mirror, by feature-tree name ("Boss-Extrude1") or inspect_model id
    /// ("feature:4"): copies of them are built on the other side of the plane. Not with
    /// bodies.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whole solid bodies to mirror, by inspect_model id ("body:1"), e.g. to make the other
    /// half of a symmetric part about one of its faces. Not with features.
    #[serde(default)]
    pub bodies: Vec<String>,
    /// Bodies only: merge each mirrored body with the bodies it touches into one (SolidWorks'
    /// default). false keeps it a separate body. Defaults to true.
    #[serde(default)]
    pub merge: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MirrorSketchParams {
    /// Sketch entity ids to mirror ("line:3", "arc:1", "point:7") from inspect_sketch or the
    /// sketch tool, all in one sketch.
    pub entities: Vec<String>,
    /// The line of the same sketch to mirror about ("line:5"), usually a centerline drawn
    /// with the sketch tool, construction=true.
    pub axis: String,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

/// The strings, trimmed, without repeats.
pub(crate) fn unique(list: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for s in list.iter().map(|s| s.trim()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn plane_ref(ids: &IdRegistry, plane: &str) -> Result<proto::GeometryRef, McpError> {
    let (r, shape) = refgeom::geometry_ref(ids, plane, "plane")?;
    match shape {
        Shape::Surface | Shape::Feature => Ok(r),
        _ => Err(invalid(format!(
            "plane: {} isn't a plane -- give \"front\", \"top\" or \"right\", a planar face id \
             (\"face:3\") or a reference plane (\"Plane1\")",
            plane.trim()
        ))),
    }
}

/// A feature name or id -> its wire target. `sketch_tools` names the tools for sketch entities.
pub(crate) fn feature_target(
    ids: &IdRegistry,
    feature: &str,
    what: &str,
    sketch_tools: &str,
) -> Result<proto::FeatureTarget, McpError> {
    use proto::feature_target::Target;
    let target = match Kind::of_id(feature) {
        None => Target::Name(feature.to_string()),
        Some(Kind::Feature) => Target::PersistId(ids.persist_id(feature).ok_or_else(|| {
            invalid(format!(
                "{what}: {}",
                ids.unknown_id(
                    feature,
                    "inspect_model",
                    "call inspect_model for current ones, or give the feature's name"
                )
            ))
        })?),
        Some(Kind::Body) => {
            return Err(invalid(format!("{what}: {feature} is a body -- pass bodies in `bodies`")));
        }
        Some(kind) if SKETCH_KINDS.contains(&kind) => {
            return Err(invalid(format!(
                "{what}: {feature} is a sketch entity -- use {sketch_tools} for sketch entities"
            )));
        }
        Some(_) => {
            return Err(invalid(format!(
                "{what}: {feature} isn't a feature -- give feature names or ids from inspect_model \
                 (what=\"features\")"
            )));
        }
    };
    Ok(proto::FeatureTarget { target: Some(target) })
}

pub(crate) fn body_id(ids: &IdRegistry, body: &str, what: &str) -> Result<Vec<u8>, McpError> {
    if Kind::of_id(body) != Some(Kind::Body) {
        return Err(invalid(format!(
            "{what}: {body:?} isn't a body id -- give ids like \"body:1\" from inspect_model \
             (what=\"bodies\"), or pass features in `features`"
        )));
    }
    ids.persist_id(body).ok_or_else(|| {
        invalid(format!(
            "{what}: {}",
            ids.unknown_id(body, "inspect_model", "call inspect_model for current ones")
        ))
    })
}

pub fn features_request(
    ids: &IdRegistry,
    params: &MirrorFeaturesParams,
) -> Result<proto::MirrorFeaturesRequest, McpError> {
    let features = unique(&params.features);
    let bodies = unique(&params.bodies);
    match (features.is_empty(), bodies.is_empty()) {
        (true, true) => return Err(invalid("give the features or the bodies to mirror")),
        (false, false) => {
            return Err(invalid(
                "give features or bodies, not both -- mirror the features in one call and the bodies \
                 in another",
            ));
        }
        _ => {}
    }
    if features.len() + bodies.len() > MAX_FEATURES {
        return Err(invalid(format!("at most {MAX_FEATURES} features or bodies per call")));
    }
    if params.merge.is_some() && bodies.is_empty() {
        return Err(invalid(
            "merge only applies to mirrored bodies -- mirrored features always join the body they're on",
        ));
    }
    let features = features
        .iter()
        .enumerate()
        .map(|(i, f)| feature_target(ids, f, &format!("feature {}", i + 1), "mirror_sketch_entities"))
        .collect::<Result<_, _>>()?;
    let bodies = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| body_id(ids, b, &format!("body {}", i + 1)))
        .collect::<Result<_, _>>()?;
    Ok(proto::MirrorFeaturesRequest {
        plane: Some(plane_ref(ids, &params.plane)?),
        features,
        bodies,
        merge: params.merge.unwrap_or(true),
    })
}

/// The given features / bodies, trimmed and once each, for error legends.
pub fn given(params: &MirrorFeaturesParams) -> Vec<String> {
    let list = if params.features.is_empty() { &params.bodies } else { &params.features };
    unique(list).into_iter().map(str::to_string).collect()
}

/// "created Mirror1: Boss-Extrude1 mirrored about Right Plane. The part has 1 solid body."
pub fn features_text(params: &MirrorFeaturesParams, reply: &proto::MirrorFeaturesResponse) -> String {
    let bodies = match reply.body_count {
        1 => "1 solid body".to_string(),
        n => format!("{n} solid bodies"),
    };
    // A face by the id it was given; planes by name.
    let plane = params.plane.trim();
    let plane = if Kind::of_id(plane) == Some(Kind::Face) { plane } else { reply.plane.as_str() };
    format!(
        "created {}: {} mirrored about {plane}. The part has {bodies}. The mirrored copies' faces and \
         edges have no ids yet, and faces the mirror joined may have new ones: inspect_model again \
         before using face or edge ids.",
        reply.feature_name, reply.mirrored
    )
}

/// A sketch entity id -> its wire ref. `feature_tool` names the tool for features and bodies.
pub(crate) fn sketch_ref(
    ids: &IdRegistry,
    entity: &str,
    what: &str,
    feature_tool: &str,
) -> Result<proto::SketchItemRef, McpError> {
    match Kind::of_id(entity) {
        Some(kind) if SKETCH_KINDS.contains(&kind) => sketch::decode_ref(ids, entity).ok_or_else(|| {
            invalid(format!(
                "{what}: {}",
                ids.unknown_id(entity, "inspect_sketch and the sketch tool", "call inspect_sketch for current ones")
            ))
        }),
        _ if entity.eq_ignore_ascii_case("origin") => {
            Err(invalid(format!("{what}: the sketch origin isn't an entity that can be copied or used here")))
        }
        _ => Err(invalid(format!(
            "{what}: {entity:?} isn't a sketch entity id -- give ids like \"line:3\" or \"point:7\" \
             from inspect_sketch (for features or bodies, use {feature_tool})"
        ))),
    }
}

pub fn sketch_request(
    ids: &IdRegistry,
    params: &MirrorSketchParams,
) -> Result<proto::MirrorSketchEntitiesRequest, McpError> {
    let entities = unique(&params.entities);
    if entities.is_empty() || entities.len() > MAX_SKETCH_ENTITIES {
        return Err(invalid(format!("give between 1 and {MAX_SKETCH_ENTITIES} sketch entities")));
    }
    let axis = params.axis.trim();
    if Kind::of_id(axis) != Some(Kind::Line) {
        return Err(invalid(format!(
            "axis: {axis:?} isn't a sketch line -- give the id of a line or centerline of the same \
             sketch (\"line:5\")"
        )));
    }
    let axis = sketch_ref(ids, axis, "axis", "mirror_features")?;
    let items = entities
        .iter()
        .enumerate()
        .map(|(i, e)| sketch_ref(ids, e, &format!("entity {}", i + 1), "mirror_features"))
        .collect::<Result<Vec<_>, _>>()?;
    if items.contains(&axis) {
        return Err(invalid("entities include the axis -- the line to mirror about isn't mirrored itself"));
    }
    Ok(proto::MirrorSketchEntitiesRequest {
        sketch_name: params.sketch_name.as_deref().map(str::trim).unwrap_or_default().to_string(),
        items,
        axis: Some(axis),
    })
}

/// The given entities, trimmed and once each, for error legends.
pub fn given_entities(params: &MirrorSketchParams) -> Vec<String> {
    unique(&params.entities).into_iter().map(str::to_string).collect()
}

/// "mirrored line:1, line:2 about line:5 in Sketch1. New: line:6 (point:7 -> point:8), ...
/// Sketch1 now has 1 closed and 0 open contours. ..."
pub fn sketch_text(ids: &mut IdRegistry, params: &MirrorSketchParams, reply: &proto::MirrorSketchEntitiesResponse) -> String {
    let new: Vec<String> = reply.items.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    let extended: Vec<String> = reply.extended.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    let mut text = format!(
        "mirrored {} about {} in {}.",
        unique(&params.entities).join(", "),
        params.axis.trim(),
        reply.sketch_name
    );
    if !new.is_empty() {
        text += &format!(
            " New: {}. Each copy is held symmetric to its original about the line.",
            new.join(", ")
        );
    }
    if !extended.is_empty() {
        text += &format!(
            " Extended across the axis instead of copied, since they end on it at a right angle \
             (each is held at its midpoint on the axis): {}.",
            extended.join(", ")
        );
    }
    if let Some(c) = &reply.contours {
        text += &format!(
            " {} now has {} closed and {} open contours.",
            reply.sketch_name,
            c.closed.max(0),
            c.open.max(0)
        );
    }
    text += &sketch::state_text(ids, &reply.sketch_name, reply.state.as_ref());
    text += &problems_text(&reply.new_errors);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> IdRegistry {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        ids
    }

    fn line(ids: &mut IdRegistry, id1: i64) -> (String, proto::SketchItemRef) {
        let r = proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point: false, id0: 0, id1, ..Default::default() };
        (sketch::ref_id(ids, Kind::Line, &r).unwrap(), r)
    }

    fn features(plane: &str, features: &[&str], bodies: &[&str]) -> MirrorFeaturesParams {
        MirrorFeaturesParams {
            plane: plane.into(),
            features: features.iter().map(|f| f.to_string()).collect(),
            bodies: bodies.iter().map(|b| b.to_string()).collect(),
            merge: None,
        }
    }

    #[test]
    fn features_by_name_or_id_about_a_default_plane_or_face() {
        let mut ids = registry();
        let feature = ids.id_for(Kind::Feature, b"feat").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let r = features_request(&ids, &features("Right", &["Boss-Extrude1", &feature, " Boss-Extrude1 "], &[])).unwrap();
        assert_eq!(r.features.len(), 2);
        assert!(r.bodies.is_empty() && r.merge);
        assert!(matches!(
            r.plane.unwrap().target,
            Some(proto::geometry_ref::Target::StandardPlane(p)) if p == proto::Plane::Right as i32
        ));
        let r = features_request(&ids, &features(&face, &["Cut-Extrude1"], &[])).unwrap();
        assert!(matches!(r.plane.unwrap().target, Some(proto::geometry_ref::Target::ModelPersistId(p)) if p == b"face"));
    }

    #[test]
    fn bodies_with_or_without_merge() {
        let mut ids = registry();
        let body = ids.id_for(Kind::Body, b"body").unwrap();
        let r = features_request(&ids, &features("front", &[], &[&body])).unwrap();
        assert_eq!(r.bodies, vec![b"body".to_vec()]);
        assert!(r.merge);
        let mut p = features("front", &[], &[&body]);
        p.merge = Some(false);
        assert!(!features_request(&ids, &p).unwrap().merge);
    }

    #[test]
    fn feature_refusals() {
        let mut ids = registry();
        let body = ids.id_for(Kind::Body, b"body").unwrap();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let vertex = ids.id_for(Kind::Vertex, b"vertex").unwrap();
        let (line, _) = line(&mut ids, 1);
        assert!(features_request(&ids, &features("front", &[], &[])).is_err());
        assert!(features_request(&ids, &features("front", &["Boss-Extrude1"], &[&body])).is_err());
        for bad in [&body, &line, &edge, "feature:99"] {
            assert!(features_request(&ids, &features("front", &[bad], &[])).is_err(), "{bad}");
        }
        for bad in ["Boss-Extrude1", "feature:1", &edge] {
            assert!(features_request(&ids, &features("front", &[], &[bad])).is_err(), "{bad}");
        }
        for bad in [&edge, &vertex, &line, "origin", ""] {
            assert!(features_request(&ids, &features(bad, &["Boss-Extrude1"], &[])).is_err(), "{bad}");
        }
        let mut p = features("front", &["Boss-Extrude1"], &[]);
        p.merge = Some(true);
        assert!(features_request(&ids, &p).is_err());
    }

    #[test]
    fn sketch_entities_about_a_line() {
        let mut ids = registry();
        let (a, ar) = line(&mut ids, 1);
        let (b, br) = line(&mut ids, 2);
        let (axis, axis_ref) = line(&mut ids, 3);
        let params = MirrorSketchParams { entities: vec![a.clone(), b.clone(), a.clone()], axis: axis.clone(), sketch_name: None };
        let r = sketch_request(&ids, &params).unwrap();
        assert_eq!(r.items, vec![ar, br]);
        assert_eq!(r.axis, Some(axis_ref));
        assert!(r.sketch_name.is_empty());

        let bad = |entities: Vec<String>, axis: &str| MirrorSketchParams { entities, axis: axis.into(), sketch_name: None };
        assert!(sketch_request(&ids, &bad(vec![], &axis)).is_err());
        assert!(sketch_request(&ids, &bad(vec![a.clone(), axis.clone()], &axis)).is_err());
        assert!(sketch_request(&ids, &bad(vec![a.clone()], "line:99")).is_err());
        assert!(sketch_request(&ids, &bad(vec![a.clone()], "front")).is_err());
        assert!(sketch_request(&ids, &bad(vec!["Boss-Extrude1".into()], &axis)).is_err());
        assert!(sketch_request(&ids, &bad(vec!["origin".into()], &axis)).is_err());
    }

    #[test]
    fn replies() {
        let reply = proto::MirrorFeaturesResponse {
            feature_name: "Mirror1".into(),
            mirrored: "Boss-Extrude2".into(),
            plane: "Right Plane".into(),
            body_count: 1,
        };
        let p = features("right", &["Boss-Extrude2"], &[]);
        assert!(features_text(&p, &reply).starts_with("created Mirror1: Boss-Extrude2 mirrored about Right Plane. The part has 1 solid body."));
        let reply = proto::MirrorFeaturesResponse { plane: "face".into(), body_count: 2, ..reply };
        let p = features("face:3", &["Boss-Extrude2"], &[]);
        assert!(features_text(&p, &reply).contains("mirrored about face:3. The part has 2 solid bodies."));

        let mut ids = registry();
        let (a, _) = line(&mut ids, 1);
        let (axis, _) = line(&mut ids, 3);
        let (_, new_ref) = line(&mut ids, 4);
        let reply = proto::MirrorSketchEntitiesResponse {
            sketch_name: "Sketch1".into(),
            items: vec![proto::SketchItem {
                r#ref: Some(new_ref.clone()),
                r#type: proto::SketchItemType::Line as i32,
                ..Default::default()
            }],
            contours: Some(proto::SketchContours { closed: 1, open: 0 }),
            state: None,
            new_errors: vec![],
            extended: vec![proto::SketchItem {
                r#ref: Some(proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point: false, id0: 0, id1: 1, ..Default::default() }),
                r#type: proto::SketchItemType::Line as i32,
                ..Default::default()
            }],
        };
        let params = MirrorSketchParams { entities: vec![a.clone()], axis: axis.clone(), sketch_name: None };
        let text = sketch_text(&mut ids, &params, &reply);
        let new_id = sketch::ref_id(&mut ids, Kind::Line, &new_ref).unwrap();
        assert!(text.starts_with(&format!("mirrored {a} about {axis} in Sketch1. New: {new_id}.")), "{text}");
        assert!(text.contains("Sketch1 now has 1 closed and 0 open contours."), "{text}");
        assert!(text.contains(&format!("(each is held at its midpoint on the axis): {a}.")), "{text}");
    }
}
