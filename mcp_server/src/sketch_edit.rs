//! Tools that change existing sketch entities or bring others in: sketch_fillet, sketch_chamfer,
//! trim_sketch_entity, extend_sketch_entity and convert_entities. Parameters, requests and replies.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::SketchTarget;
use crate::addin::proto;
use crate::delete::problems_text;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, SKETCH_KINDS};
use crate::mirror::{sketch_ref, unique};
use crate::sketch;

/// The add-in's limits.
const MAX_CORNERS: usize = 200;
const MAX_ENTITIES: usize = 1000;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn trimmed_name(name: &Option<String>) -> String {
    name.as_deref().map(str::trim).unwrap_or_default().to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SketchFilletParams {
    /// Radius in mm, more than 0.
    pub radius_mm: f64,
    /// Which corners: sketch point ids ("point:3"), each where two entities meet; or exactly two
    /// entity ids ("line:1", "arc:2") for the corner between them, which also works when they
    /// don't touch yet (they're extended to it). inspect_sketch and the sketch_* tools give ids.
    pub entities: Vec<String>,
    /// The sketch, e.g. "Sketch1". Defaults to the one the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SketchChamferParams {
    /// How far along the entities the chamfer starts, in mm, more than 0. With second_distance_mm
    /// or angle_deg it's the distance along the first of the two entities.
    pub distance_mm: f64,
    /// A different distance along the second entity, in mm. Needs two entity ids.
    #[serde(default)]
    pub second_distance_mm: Option<f64>,
    /// The chamfer's angle from the first entity, in degrees, instead of a second distance. Needs
    /// two entity ids.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Which corners: sketch point ids ("point:3"), each where two entities meet; or exactly two
    /// entity ids ("line:1", "line:2") that touch, for the corner between them.
    pub entities: Vec<String>,
    /// The sketch, e.g. "Sketch1". Defaults to the one the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TrimParams {
    /// The entity to trim: a sketch id such as "line:3", "arc:1", "circle:2" or "ellipse:1".
    pub entity: String,
    /// Removes the piece of the entity at this point, between whatever crosses it on either side
    /// (the whole entity if nothing does). [x, y] in sketch coordinates or [x, y, z] in model
    /// coordinates, in mm. Give this or corner_with.
    #[serde(default)]
    pub at: Option<Vec<f64>>,
    /// Instead of `at`: another entity. Both grow until they meet in a corner. They must not cross
    /// each other already.
    #[serde(default)]
    pub corner_with: Option<String>,
    /// The sketch, e.g. "Sketch1". Defaults to the one the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExtendParams {
    /// The entity to extend: a sketch id such as "line:3" or "arc:1".
    pub entity: String,
    /// A point nearest the end to extend, e.g. that end itself. [x, y] in sketch coordinates or
    /// [x, y, z] in model coordinates, in mm.
    pub at: Vec<f64>,
    /// The sketch, e.g. "Sketch1". Defaults to the one the id belongs to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConvertLoops {
    /// The outer loop and the holes.
    #[default]
    All,
    /// The outer loop only.
    Outer,
    /// The holes only.
    Inner,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConvertParams {
    /// What to convert, in the order given: model edge and face ids ("edge:4", "face:2") from
    /// inspect_model, and entity ids of other sketches ("line:3", "arc:1") from inspect_sketch.
    pub entities: Vec<String>,
    /// With faces: "all" (the default: outer loop and holes), "outer" or "inner" (the holes).
    #[serde(default)]
    pub loops: ConvertLoops,
    /// New sketch only: make it a 3D sketch, where edges keep their place in space instead of
    /// being projected, e.g. for a sweep path along a model's edges. Defaults to false.
    #[serde(default)]
    pub sketch_3d: bool,
    /// sketch_name draws into an existing sketch (2D or 3D). Otherwise a new sketch on `plane` or
    /// `on`, which defaults to the face when a single face is given.
    #[serde(flatten)]
    pub(crate) target: SketchTarget,
}

fn item_refs(ids: &IdRegistry, entities: &[&str]) -> Result<Vec<proto::SketchItemRef>, McpError> {
    entities
        .iter()
        .enumerate()
        .map(|(i, e)| sketch_ref(ids, e, &format!("entity {}", i + 1), "fillet_edges or chamfer_edges"))
        .collect()
}

fn corner_refs(ids: &IdRegistry, entities: &[String]) -> Result<Vec<proto::SketchItemRef>, McpError> {
    let entities = unique(entities);
    if entities.is_empty() {
        return Err(invalid("give the corners: sketch point ids, or two entity ids"));
    }
    if entities.len() > MAX_CORNERS {
        return Err(invalid(format!("at most {MAX_CORNERS} corners per call")));
    }
    let points = entities.iter().filter(|e| Kind::of_id(e) == Some(Kind::SketchPoint)).count();
    if points > 0 && points < entities.len() {
        return Err(invalid("give sketch points or two entities, not both"));
    }
    if points == 0 && entities.len() != 2 {
        return Err(invalid(
            "two entities make one corner -- give exactly two, or the corners' sketch point ids (\"point:3\")",
        ));
    }
    item_refs(ids, &entities)
}

pub fn fillet_request(ids: &IdRegistry, params: &SketchFilletParams) -> Result<proto::SketchCornersRequest, McpError> {
    if !(params.radius_mm.is_finite() && params.radius_mm > 0.0) {
        return Err(invalid(format!("radius_mm must be more than 0, got {}", params.radius_mm)));
    }
    Ok(proto::SketchCornersRequest {
        sketch_name: trimmed_name(&params.sketch_name),
        items: corner_refs(ids, &params.entities)?,
        kind: Some(proto::sketch_corners_request::Kind::FilletRadius(params.radius_mm / MM_PER_M)),
    })
}

pub fn chamfer_request(ids: &IdRegistry, params: &SketchChamferParams) -> Result<proto::SketchCornersRequest, McpError> {
    if !(params.distance_mm.is_finite() && params.distance_mm > 0.0) {
        return Err(invalid(format!("distance_mm must be more than 0, got {}", params.distance_mm)));
    }
    let (kind, second) = match (params.second_distance_mm, params.angle_deg) {
        (Some(_), Some(_)) => return Err(invalid("give second_distance_mm or angle_deg, not both")),
        (Some(d), None) => {
            if !(d.is_finite() && d > 0.0) {
                return Err(invalid(format!("second_distance_mm must be more than 0, got {d}")));
            }
            (proto::SketchChamferKind::Distances, d / MM_PER_M)
        }
        (None, Some(a)) => {
            if !(a.is_finite() && a > 0.0 && a < 180.0) {
                return Err(invalid(format!("angle_deg must be between 0 and 180, got {a}")));
            }
            (proto::SketchChamferKind::Angle, a.to_radians())
        }
        (None, None) => (proto::SketchChamferKind::Equal, 0.0),
    };
    let items = corner_refs(ids, &params.entities)?;
    if kind != proto::SketchChamferKind::Equal && items.iter().any(|r| r.point) {
        return Err(invalid(
            "a chamfer with two different sides needs the two entity ids, to say which is which, not a sketch point",
        ));
    }
    Ok(proto::SketchCornersRequest {
        sketch_name: trimmed_name(&params.sketch_name),
        items,
        kind: Some(proto::sketch_corners_request::Kind::Chamfer(proto::SketchChamfer {
            kind: kind.into(),
            distance: params.distance_mm / MM_PER_M,
            second,
        })),
    })
}

/// A point as (wire point in meters, whether it's in model coordinates).
fn at_point(at: &[f64]) -> Result<(proto::Vec3, bool), McpError> {
    if !matches!(at.len(), 2 | 3) {
        return Err(invalid(format!(
            "at has {} coordinates; give [x, y] in sketch coordinates or [x, y, z] in model coordinates",
            at.len()
        )));
    }
    if !at.iter().all(|v| v.is_finite()) {
        return Err(invalid("at must be finite"));
    }
    Ok((
        proto::Vec3 {
            x: at[0] / MM_PER_M,
            y: at[1] / MM_PER_M,
            z: at.get(2).copied().unwrap_or(0.0) / MM_PER_M,
        },
        at.len() == 3,
    ))
}

fn segment_ref(ids: &IdRegistry, entity: &str, what: &str) -> Result<proto::SketchItemRef, McpError> {
    let entity = entity.trim();
    if Kind::of_id(entity) == Some(Kind::SketchPoint) {
        return Err(invalid(format!("{what}: {entity} is a point -- give a line, arc, circle, ellipse or spline")));
    }
    sketch_ref(ids, entity, what, "fillet_edges or chamfer_edges")
}

pub fn trim_request(ids: &IdRegistry, params: &TrimParams) -> Result<proto::TrimSketchEntityRequest, McpError> {
    let item = Some(segment_ref(ids, &params.entity, "entity")?);
    let sketch_name = trimmed_name(&params.sketch_name);
    match (&params.at, &params.corner_with) {
        (Some(at), None) => {
            let (at, model_space) = at_point(at)?;
            Ok(proto::TrimSketchEntityRequest {
                sketch_name,
                item,
                kind: proto::SketchTrimKind::Piece.into(),
                at: Some(at),
                model_space,
                other: None,
            })
        }
        (None, Some(other)) => {
            if other.trim() == params.entity.trim() {
                return Err(invalid("corner_with is the entity itself -- a corner needs two different entities"));
            }
            Ok(proto::TrimSketchEntityRequest {
                sketch_name,
                item,
                kind: proto::SketchTrimKind::Corner.into(),
                at: None,
                model_space: false,
                other: Some(segment_ref(ids, other, "corner_with")?),
            })
        }
        _ => Err(invalid("give exactly one of at (remove the piece there) or corner_with (meet in a corner)")),
    }
}

pub fn extend_request(ids: &IdRegistry, params: &ExtendParams) -> Result<proto::TrimSketchEntityRequest, McpError> {
    let (at, model_space) = at_point(&params.at)?;
    Ok(proto::TrimSketchEntityRequest {
        sketch_name: trimmed_name(&params.sketch_name),
        item: Some(segment_ref(ids, &params.entity, "entity")?),
        kind: proto::SketchTrimKind::Extend.into(),
        at: Some(at),
        model_space,
        other: None,
    })
}

pub fn convert_request(ids: &IdRegistry, params: &ConvertParams) -> Result<proto::ConvertEntitiesRequest, McpError> {
    let entities = unique(&params.entities);
    if entities.is_empty() {
        return Err(invalid("give the entities to convert"));
    }
    if entities.len() > MAX_ENTITIES {
        return Err(invalid(format!("at most {MAX_ENTITIES} entities per call")));
    }
    let mut wire = proto::ConvertEntitiesRequest {
        sketch_name: trimmed_name(&params.target.sketch_name),
        sketch_3d: params.sketch_3d,
        loops: match params.loops {
            ConvertLoops::All => proto::ConvertLoops::All,
            ConvertLoops::Outer => proto::ConvertLoops::Outer,
            ConvertLoops::Inner => proto::ConvertLoops::Inner,
        }
        .into(),
        ..Default::default()
    };
    if params.loops != ConvertLoops::All && !entities.iter().any(|e| Kind::of_id(e) == Some(Kind::Face)) {
        return Err(invalid("loops only goes with faces -- give a face id, or leave it out"));
    }
    for (i, entity) in entities.iter().enumerate() {
        let what = format!("entity {}", i + 1);
        match Kind::of_id(entity) {
            Some(Kind::Edge | Kind::Face) => {
                wire.model_persist_ids.push(ids.persist_id(entity).ok_or_else(|| {
                    invalid(format!(
                        "{what}: {}",
                        ids.unknown_id(entity, "inspect_model", "call inspect_model for current ones")
                    ))
                })?);
            }
            Some(Kind::SketchPoint) => {
                return Err(invalid(format!("{what}: {entity} is a point -- only segments can be converted")));
            }
            Some(kind) if SKETCH_KINDS.contains(&kind) => wire.items.push(sketch_ref(ids, entity, &what, "offset_sketch_entities")?),
            _ => {
                return Err(invalid(format!(
                    "{what}: {entity:?} can't be converted -- give model edges and faces (\"edge:4\", \"face:2\") or \
                     entities of other sketches (\"line:3\")"
                )));
            }
        }
    }

    let on = params.target.on.as_deref().map(str::trim).filter(|on| !on.is_empty());
    let has_place = on.is_some() || params.target.plane.is_some();
    if !wire.sketch_name.is_empty() {
        if has_place || params.sketch_3d {
            return Err(invalid("give either sketch_name (an existing sketch) or where to put a new one, not both"));
        }
        return Ok(wire);
    }
    if params.sketch_3d {
        if has_place {
            return Err(invalid("a 3D sketch isn't on a plane or face -- drop plane/on or sketch_3d"));
        }
        return Ok(wire);
    }
    match (on, params.target.plane, entities.as_slice()) {
        (Some(_), Some(_), _) => return Err(invalid("give plane or on for the new sketch, not both")),
        (Some(on), None, _) => {
            wire.on_persist_id = ids.persist_id(on).ok_or_else(|| {
                invalid(format!("on: {}", ids.unknown_id(on, "inspect_model", "call inspect_model for current ones")))
            })?;
        }
        (None, Some(plane), _) => wire.plane = proto::Plane::from(plane).into(),
        (None, None, [face]) if Kind::of_id(face) == Some(Kind::Face) => {
            wire.on_persist_id = wire.model_persist_ids[0].clone();
        }
        _ => {
            return Err(invalid(
                "say where the converted entities go: sketch_name for an existing sketch, plane or on for a new one, or \
                 sketch_3d",
            ));
        }
    }
    Ok(wire)
}

/// The given entities, trimmed and once each, for error legends.
pub fn given(entities: &[String]) -> Vec<String> {
    unique(entities).into_iter().map(str::to_string).collect()
}

/// The ids of what's gone (a sketch entity already handed out has one; others aren't worth one).
fn deleted_ids(ids: &IdRegistry, deleted: &[proto::SketchItemRef]) -> Vec<String> {
    use prost::Message as _;
    deleted.iter().filter_map(|r| ids.existing_id(&r.encode_to_vec())).collect()
}

/// "New: arc:1 (...). Now: line:1 (...), .... Gone: line:3. D1@Sketch1 = 3 mm. Sketch1 now has ..."
pub fn edit_text(ids: &mut IdRegistry, reply: &proto::SketchEditResponse) -> String {
    let mut text = String::new();
    let new: Vec<String> = reply.items.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    if !new.is_empty() {
        text += &format!(" New: {}.", new.join(", "));
    }
    let changed: Vec<String> = reply.changed.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    if !changed.is_empty() {
        text += &format!(" Changed: {}.", changed.join(", "));
    }
    let gone = deleted_ids(ids, &reply.deleted);
    if !gone.is_empty() {
        text += &format!(" Gone: {}.", gone.join(", "));
    }
    if !reply.dimensions.is_empty() {
        let dims: Vec<String> = reply.dimensions.iter().map(sketch::dimension_text).collect();
        text += &format!(" Dimensions: {} (set_dimension changes them).", dims.join(", "));
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

/// "converted edge:1 into Sketch2 (new sketch). New: line:3 (point:7 -> point:8), ... "
pub fn convert_text(ids: &mut IdRegistry, params: &ConvertParams, reply: &proto::ConvertEntitiesResponse) -> String {
    let given = unique(&params.entities);
    let new_sketch = match (reply.created_sketch, reply.is_3d) {
        (true, true) => " (new 3D sketch)",
        (true, false) => " (new sketch)",
        _ => "",
    };
    let mut text = format!("converted {} into {}{new_sketch}.", given.join(", "), reply.sketch_name);
    let new: Vec<String> = reply.items.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    if !new.is_empty() {
        text += &format!(" New: {}.", new.join(", "));
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

    fn item(ids: &mut IdRegistry, kind: Kind, id1: i64) -> (String, proto::SketchItemRef) {
        let r = proto::SketchItemRef {
            sketch_persist_id: b"sk".to_vec(),
            point: kind == Kind::SketchPoint,
            id0: 0,
            id1,
            ..Default::default()
        };
        (sketch::ref_id(ids, kind, &r).unwrap(), r)
    }

    fn fillet(entities: &[&str]) -> SketchFilletParams {
        SketchFilletParams {
            radius_mm: 3.0,
            entities: entities.iter().map(|e| e.to_string()).collect(),
            sketch_name: None,
        }
    }

    fn chamfer(entities: &[&str]) -> SketchChamferParams {
        SketchChamferParams {
            distance_mm: 3.0,
            second_distance_mm: None,
            angle_deg: None,
            entities: entities.iter().map(|e| e.to_string()).collect(),
            sketch_name: None,
        }
    }

    fn convert(entities: &[&str]) -> ConvertParams {
        ConvertParams {
            entities: entities.iter().map(|e| e.to_string()).collect(),
            loops: ConvertLoops::default(),
            sketch_3d: false,
            target: SketchTarget { sketch_name: None, plane: None, on: None },
        }
    }

    #[test]
    fn corners_are_points_or_two_entities() {
        let mut ids = registry();
        let (a, ar) = item(&mut ids, Kind::Line, 1);
        let (b, br) = item(&mut ids, Kind::Arc, 2);
        let (c, _) = item(&mut ids, Kind::Line, 3);
        let (p, pr) = item(&mut ids, Kind::SketchPoint, 4);
        let (q, qr) = item(&mut ids, Kind::SketchPoint, 5);

        let r = fillet_request(&ids, &fillet(&[&p, &q, &p])).unwrap();
        assert_eq!(r.items, vec![pr, qr]);
        assert_eq!(r.kind, Some(proto::sketch_corners_request::Kind::FilletRadius(0.003)));
        assert_eq!(fillet_request(&ids, &fillet(&[&a, &b])).unwrap().items, vec![ar, br]);

        assert!(fillet_request(&ids, &fillet(&[])).is_err());
        assert!(fillet_request(&ids, &fillet(&[&a])).is_err());
        assert!(fillet_request(&ids, &fillet(&[&a, &b, &c])).is_err());
        assert!(fillet_request(&ids, &fillet(&[&a, &p])).is_err());
        assert!(fillet_request(&ids, &fillet(&["line:99", &a])).is_err());
        assert!(fillet_request(&ids, &fillet(&["edge:1", "edge:2"])).is_err());
        for bad in [0.0, -1.0, f64::NAN] {
            let mut f = fillet(&[&p]);
            f.radius_mm = bad;
            assert!(fillet_request(&ids, &f).is_err(), "{bad}");
        }
    }

    #[test]
    fn chamfers() {
        let mut ids = registry();
        let (a, _) = item(&mut ids, Kind::Line, 1);
        let (b, _) = item(&mut ids, Kind::Line, 2);
        let (p, _) = item(&mut ids, Kind::SketchPoint, 3);
        let wire = |r: proto::SketchCornersRequest| match r.kind {
            Some(proto::sketch_corners_request::Kind::Chamfer(c)) => c,
            other => panic!("{other:?}"),
        };

        let c = wire(chamfer_request(&ids, &chamfer(&[&p])).unwrap());
        assert_eq!(c.kind(), proto::SketchChamferKind::Equal);
        assert!((c.distance - 0.003).abs() < 1e-12);

        let mut params = chamfer(&[&a, &b]);
        params.second_distance_mm = Some(5.0);
        let c = wire(chamfer_request(&ids, &params).unwrap());
        assert_eq!(c.kind(), proto::SketchChamferKind::Distances);
        assert!((c.second - 0.005).abs() < 1e-12);

        let mut params = chamfer(&[&a, &b]);
        params.angle_deg = Some(30.0);
        let c = wire(chamfer_request(&ids, &params).unwrap());
        assert_eq!(c.kind(), proto::SketchChamferKind::Angle);
        assert!((c.second - 30f64.to_radians()).abs() < 1e-12);

        // A point can't say which side is which.
        let mut params = chamfer(&[&p]);
        params.second_distance_mm = Some(5.0);
        assert!(chamfer_request(&ids, &params).is_err());
        let mut params = chamfer(&[&a, &b]);
        params.second_distance_mm = Some(5.0);
        params.angle_deg = Some(30.0);
        assert!(chamfer_request(&ids, &params).is_err());
        for bad in [0.0, 180.0, f64::NAN] {
            let mut params = chamfer(&[&a, &b]);
            params.angle_deg = Some(bad);
            assert!(chamfer_request(&ids, &params).is_err(), "{bad}");
        }
    }

    #[test]
    fn trims_and_extends() {
        let mut ids = registry();
        let (a, ar) = item(&mut ids, Kind::Line, 1);
        let (b, br) = item(&mut ids, Kind::Circle, 2);
        let (p, _) = item(&mut ids, Kind::SketchPoint, 3);

        let r = trim_request(&ids, &TrimParams { entity: a.clone(), at: Some(vec![1.0, 2.0]), corner_with: None, sketch_name: None }).unwrap();
        assert_eq!(r.kind(), proto::SketchTrimKind::Piece);
        assert_eq!(r.item.as_ref(), Some(&ar));
        assert!(!r.model_space && r.other.is_none());
        assert!((r.at.unwrap().y - 0.002).abs() < 1e-12);

        let r = trim_request(&ids, &TrimParams { entity: a.clone(), at: Some(vec![1.0, 2.0, 3.0]), corner_with: None, sketch_name: None }).unwrap();
        assert!(r.model_space && (r.at.unwrap().z - 0.003).abs() < 1e-12);

        let r = trim_request(&ids, &TrimParams { entity: a.clone(), at: None, corner_with: Some(b.clone()), sketch_name: Some(" Sketch1 ".into()) }).unwrap();
        assert_eq!(r.kind(), proto::SketchTrimKind::Corner);
        assert_eq!(r.other, Some(br));
        assert_eq!(r.sketch_name, "Sketch1");
        assert!(r.at.is_none());

        let r = extend_request(&ids, &ExtendParams { entity: a.clone(), at: vec![0.0, 0.0], sketch_name: None }).unwrap();
        assert_eq!(r.kind(), proto::SketchTrimKind::Extend);

        let t = |entity: &str, at: Option<Vec<f64>>, corner: Option<String>| {
            trim_request(&ids, &TrimParams { entity: entity.into(), at, corner_with: corner, sketch_name: None }).is_err()
        };
        assert!(t(&a, None, None));
        assert!(t(&a, Some(vec![0.0, 0.0]), Some(b.clone())));
        assert!(t(&a, Some(vec![0.0]), None));
        assert!(t(&a, Some(vec![f64::NAN, 0.0]), None));
        assert!(t(&p, Some(vec![0.0, 0.0]), None));
        assert!(t(&a, None, Some(a.clone())));
        assert!(t(&a, None, Some(p.clone())));
        assert!(t("line:99", Some(vec![0.0, 0.0]), None));
        assert!(extend_request(&ids, &ExtendParams { entity: p, at: vec![0.0, 0.0], sketch_name: None }).is_err());
    }

    #[test]
    fn conversions_go_where_they_are_told() {
        let mut ids = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let (line, line_ref) = item(&mut ids, Kind::Line, 1);
        let (point, _) = item(&mut ids, Kind::SketchPoint, 2);

        // A single face is its own place, and brings its holes.
        let r = convert_request(&ids, &convert(&[&face])).unwrap();
        assert_eq!(r.on_persist_id, b"face");
        assert_eq!(r.loops(), proto::ConvertLoops::All);
        let mut p = convert(&[&face]);
        p.loops = ConvertLoops::Inner;
        assert_eq!(convert_request(&ids, &p).unwrap().loops(), proto::ConvertLoops::Inner);
        let mut p = convert(&[&edge]);
        p.loops = ConvertLoops::Outer;
        p.target.sketch_name = Some("Sketch2".into());
        assert!(convert_request(&ids, &p).is_err());

        // Anything else has to say.
        assert!(convert_request(&ids, &convert(&[&edge])).is_err());
        assert!(convert_request(&ids, &convert(&[&edge, &face])).is_err());

        let mut p = convert(&[&edge, &line]);
        p.target.plane = Some(crate::SketchPlane::Top);
        let r = convert_request(&ids, &p).unwrap();
        assert_eq!(r.plane(), proto::Plane::Top);
        assert_eq!(r.model_persist_ids, vec![b"edge".to_vec()]);
        assert_eq!(r.items, vec![line_ref]);

        let mut p = convert(&[&edge]);
        p.target.on = Some(face.clone());
        assert_eq!(convert_request(&ids, &p).unwrap().on_persist_id, b"face");

        let mut p = convert(&[&edge]);
        p.sketch_3d = true;
        let r = convert_request(&ids, &p).unwrap();
        assert!(r.sketch_3d && r.on_persist_id.is_empty());
        assert_eq!(r.plane(), proto::Plane::Unspecified);
        p.target.plane = Some(crate::SketchPlane::Top);
        assert!(convert_request(&ids, &p).is_err());

        let mut p = convert(&[&edge]);
        p.target.sketch_name = Some("Sketch2".into());
        assert_eq!(convert_request(&ids, &p).unwrap().sketch_name, "Sketch2");
        p.sketch_3d = true;
        assert!(convert_request(&ids, &p).is_err());
        p.sketch_3d = false;
        p.target.on = Some(face.clone());
        assert!(convert_request(&ids, &p).is_err());

        let mut p = convert(&[&edge]);
        p.target.on = Some(face);
        p.target.plane = Some(crate::SketchPlane::Top);
        assert!(convert_request(&ids, &p).is_err());

        for bad in [point.as_str(), "edge:99", "line:99", "origin", "Boss-Extrude1"] {
            let mut p = convert(&[bad]);
            p.target.sketch_name = Some("Sketch2".into());
            assert!(convert_request(&ids, &p).is_err(), "{bad}");
        }
        assert!(convert_request(&ids, &convert(&[])).is_err());
    }

    #[test]
    fn replies() {
        let mut ids = registry();
        let (line, line_ref) = item(&mut ids, Kind::Line, 1);
        let (gone, gone_ref) = item(&mut ids, Kind::Line, 2);
        let arc_ref = proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), id1: 3, ..Default::default() };
        let reply = proto::SketchEditResponse {
            sketch_name: "Sketch1".into(),
            items: vec![proto::SketchItem {
                r#ref: Some(arc_ref.clone()),
                r#type: proto::SketchItemType::Arc as i32,
                ..Default::default()
            }],
            changed: vec![proto::SketchItem {
                r#ref: Some(line_ref),
                r#type: proto::SketchItemType::Line as i32,
                ..Default::default()
            }],
            deleted: vec![gone_ref],
            dimensions: vec![proto::DimensionInfo {
                name: "D1@Sketch1".into(),
                r#type: proto::DimensionType::Radius as i32,
                value: 0.003,
                ..Default::default()
            }],
            contours: Some(proto::SketchContours { closed: 1, open: 0 }),
            state: None,
            new_errors: vec![],
        };
        let text = edit_text(&mut ids, &reply);
        let arc = sketch::ref_id(&mut ids, Kind::Arc, &arc_ref).unwrap();
        assert!(text.contains(&format!("New: {arc}.")), "{text}");
        assert!(text.contains(&format!("Changed: {line}.")), "{text}");
        assert!(text.contains(&format!("Gone: {gone}.")), "{text}");
        assert!(text.contains("D1@Sketch1 = 3 mm"), "{text}");
        assert!(text.contains("Sketch1 now has 1 closed and 0 open contours."), "{text}");

        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let converted = proto::ConvertEntitiesResponse {
            sketch_name: "3DSketch1".into(),
            created_sketch: true,
            is_3d: true,
            ..Default::default()
        };
        let text = convert_text(&mut ids, &convert(&[&edge]), &converted);
        assert!(text.starts_with(&format!("converted {edge} into 3DSketch1 (new 3D sketch).")), "{text}");
    }
}
