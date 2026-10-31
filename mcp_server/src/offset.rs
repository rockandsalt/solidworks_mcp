//! The offset tool (offset_sketch_entities): its parameters, the sketch entity or model
//! edge and face references, where the new entities go, and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::SketchTarget;
use crate::addin::proto;
use crate::delete::problems_text;
use crate::inspect::{IdRegistry, Kind, MM_PER_M};
use crate::mirror::{sketch_ref, unique};
use crate::sketch;

/// The add-in's limit.
const MAX_ENTITIES: usize = 1000;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OffsetSide {
    /// Away from a closed contour's inside; on an open chain, the right-hand side going from
    /// start to end of the first entity.
    #[default]
    Outward,
    /// The other side.
    Inward,
    /// Both sides.
    Both,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OffsetCaps {
    #[default]
    None,
    Arcs,
    Lines,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OffsetParams {
    /// How far to offset, in mm, more than 0. Use side for the direction.
    pub distance_mm: f64,
    /// What to offset, in the order given. Sketch entity ids ("line:3", "arc:1", "circle:2")
    /// from inspect_sketch or the sketch_* tools, all in one sketch; or model edge and face
    /// ids ("edge:4", "face:2") from inspect_model. Not both kinds in one call. Omit, with
    /// sketch_name, for every entity of that sketch except construction geometry.
    #[serde(default)]
    pub entities: Vec<String>,
    /// "outward" (the default: away from the inside of each closed contour), "inward" or
    /// "both". On an open chain of sketch entities, outward is the right-hand side going from
    /// the start to the end of the first entity, and inward the left.
    #[serde(default)]
    pub side: OffsetSide,
    /// Also offset what's connected to the given entities: a whole contour from one of its
    /// sketch entities, or for model edges what's tangent to them. Defaults to false.
    #[serde(default)]
    pub chain: bool,
    /// With side "both", on sketch entities: close the two offsets of an open chain with
    /// "arcs" or "lines" -- e.g. a line into a slot. Defaults to "none".
    #[serde(default)]
    pub caps: OffsetCaps,
    /// Sketch entities only: make the new entities construction geometry. Defaults to false.
    #[serde(default)]
    pub construction: bool,
    /// Sketch entities only: turn the originals into construction geometry, e.g. to keep just
    /// the offset for a feature. Defaults to false.
    #[serde(default)]
    pub base_construction: bool,
    /// Sketch entities only: add a driving dimension of the distance, which set_dimension can
    /// change later. Defaults to true; model edges and faces get none.
    #[serde(default)]
    pub add_dimension: Option<bool>,
    /// Sketch entities: sketch_name is their sketch, by default the one the ids belong to.
    /// Model edges and faces: draw into sketch_name, or a new sketch on `plane` or `on`, which
    /// defaults to the face when a single face is given.
    #[serde(flatten)]
    pub(crate) target: SketchTarget,
}

fn model_id(ids: &IdRegistry, entity: &str, what: &str) -> Result<Vec<u8>, McpError> {
    ids.persist_id(entity).ok_or_else(|| {
        invalid(format!(
            "{what}: {}",
            ids.unknown_id(entity, "inspect_model", "call inspect_model for current ones")
        ))
    })
}

/// A new sketch's place on the wire: (plane, on_persist_id).
fn new_sketch_place(
    ids: &IdRegistry,
    params: &OffsetParams,
    entities: &[&str],
) -> Result<(proto::Plane, Vec<u8>), McpError> {
    let on = params.target.on.as_deref().map(str::trim).filter(|on| !on.is_empty());
    match (on, params.target.plane, entities) {
        (Some(_), Some(_), _) => Err(invalid("give plane or on for the new sketch, not both")),
        (Some(on), None, _) => Ok((proto::Plane::Unspecified, model_id(ids, on, "on")?)),
        (None, Some(plane), _) => Ok((proto::Plane::from(plane), Vec::new())),
        (None, None, [face]) if Kind::of_id(face) == Some(Kind::Face) => {
            Ok((proto::Plane::Unspecified, model_id(ids, face, "entity 1")?))
        }
        _ => Err(invalid(
            "say where the offset of model edges goes: sketch_name for an existing sketch, or plane or \
             on for a new one",
        )),
    }
}

pub fn request(ids: &IdRegistry, params: &OffsetParams) -> Result<proto::OffsetSketchEntitiesRequest, McpError> {
    if !(params.distance_mm.is_finite() && params.distance_mm > 0.0) {
        return Err(invalid(format!(
            "distance_mm must be more than 0, got {} (use side for the direction)",
            params.distance_mm
        )));
    }
    let entities = unique(&params.entities);
    if entities.len() > MAX_ENTITIES {
        return Err(invalid(format!("at most {MAX_ENTITIES} entities per call")));
    }
    let sketch_name = params.target.sketch_name.as_deref().map(str::trim).unwrap_or_default().to_string();
    if entities.is_empty() && sketch_name.is_empty() {
        return Err(invalid("give the entities to offset, or sketch_name to offset a whole sketch"));
    }
    let model = entities.iter().filter(|e| matches!(Kind::of_id(e), Some(Kind::Edge | Kind::Face))).count();
    if model > 0 && model < entities.len() {
        return Err(invalid(
            "give sketch entities or model edges and faces, not both -- offset them in two calls",
        ));
    }
    let side = match params.side {
        OffsetSide::Outward => proto::OffsetSide::Outward,
        OffsetSide::Inward => proto::OffsetSide::Inward,
        OffsetSide::Both => proto::OffsetSide::Both,
    };
    let caps = match params.caps {
        OffsetCaps::None => proto::OffsetCaps::None,
        OffsetCaps::Arcs => proto::OffsetCaps::Arcs,
        OffsetCaps::Lines => proto::OffsetCaps::Lines,
    };
    if caps != proto::OffsetCaps::None && side != proto::OffsetSide::Both {
        return Err(invalid("caps only go with side \"both\": they close the two offsets of an open chain"));
    }

    let mut wire = proto::OffsetSketchEntitiesRequest {
        distance: params.distance_mm / MM_PER_M,
        side: side.into(),
        chain: params.chain,
        sketch_name,
        ..Default::default()
    };
    let has_place = params.target.plane.is_some() || params.target.on.as_deref().is_some_and(|on| !on.trim().is_empty());
    if model > 0 {
        if params.caps_or_construction() {
            return Err(invalid(
                "caps, construction and base_construction only go with sketch entities -- SolidWorks \
                 has no such options for model edges and faces",
            ));
        }
        if params.add_dimension == Some(true) {
            return Err(invalid(
                "add_dimension only goes with sketch entities -- SolidWorks adds no dimension for model \
                 edges and faces (dimension_sketch can)",
            ));
        }
        wire.model_persist_ids = entities
            .iter()
            .enumerate()
            .map(|(i, e)| model_id(ids, e, &format!("entity {}", i + 1)))
            .collect::<Result<_, _>>()?;
        if wire.sketch_name.is_empty() {
            let (plane, on) = new_sketch_place(ids, params, &entities)?;
            wire.plane = plane.into();
            wire.on_persist_id = on;
        } else if has_place {
            return Err(invalid("give either sketch_name (an existing sketch) or plane/on (a new one), not both"));
        }
        return Ok(wire);
    }

    if has_place {
        return Err(invalid(
            "plane and on are for offsetting model edges or faces into a new sketch -- sketch entities \
             are offset inside their own sketch",
        ));
    }
    wire.items = entities
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if Kind::of_id(e) == Some(Kind::SketchPoint) {
                return Err(invalid(format!(
                    "entity {}: {e} is a point -- only lines, arcs, circles and splines can be offset",
                    i + 1
                )));
            }
            sketch_ref(ids, e, &format!("entity {}", i + 1), "the feature tools")
        })
        .collect::<Result<_, _>>()?;
    wire.caps = caps.into();
    wire.offset_construction = params.construction;
    wire.base_construction = params.base_construction;
    wire.add_dimension = params.add_dimension.unwrap_or(true);
    Ok(wire)
}

impl OffsetParams {
    fn caps_or_construction(&self) -> bool {
        !matches!(self.caps, OffsetCaps::None) || self.construction || self.base_construction
    }
}

/// The given entities, trimmed and once each, for error legends.
pub fn given(params: &OffsetParams) -> Vec<String> {
    unique(&params.entities).into_iter().map(str::to_string).collect()
}

fn side_text(side: OffsetSide) -> &'static str {
    match side {
        OffsetSide::Outward => "outward",
        OffsetSide::Inward => "inward",
        OffsetSide::Both => "to both sides",
    }
}

/// "offset line:1, line:2 by 5 mm outward in Sketch1. New: line:3 (point:7 -> point:8), ...
/// Sketch1 now has 2 closed and 0 open contours. ..."
pub fn reply_text(ids: &mut IdRegistry, params: &OffsetParams, reply: &proto::OffsetSketchEntitiesResponse) -> String {
    let given = unique(&params.entities);
    let what = if given.is_empty() { "the whole sketch".to_string() } else { given.join(", ") };
    let new_sketch = if reply.created_sketch { " (new sketch)" } else { "" };
    let mut text = format!(
        "offset {what} by {} mm {} in {}{new_sketch}.",
        params.distance_mm,
        side_text(params.side),
        reply.sketch_name
    );
    let new: Vec<String> = reply.items.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    if !new.is_empty() {
        text += &format!(" New: {}.", new.join(", "));
    }
    let model = given.iter().any(|e| matches!(Kind::of_id(e), Some(Kind::Edge | Kind::Face)));
    if given.iter().any(|e| Kind::of_id(e) == Some(Kind::Face)) {
        text += " A face offsets its outer loop only: give a hole's edge ids to offset the hole too.";
    }
    if !model && params.add_dimension.unwrap_or(true) {
        text += " The distance is a dimension (inspect_sketch lists it, set_dimension changes it).";
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

    fn params(entities: &[&str]) -> OffsetParams {
        OffsetParams {
            distance_mm: 5.0,
            entities: entities.iter().map(|e| e.to_string()).collect(),
            side: OffsetSide::default(),
            chain: false,
            caps: OffsetCaps::default(),
            construction: false,
            base_construction: false,
            add_dimension: None,
            target: SketchTarget { sketch_name: None, plane: None, on: None },
        }
    }

    #[test]
    fn sketch_entities_with_a_dimension_by_default() {
        let mut ids = registry();
        let (a, ar) = line(&mut ids, 1);
        let (b, br) = line(&mut ids, 2);
        let r = request(&ids, &params(&[&a, &b, &a])).unwrap();
        assert_eq!(r.items, vec![ar, br]);
        assert!((r.distance - 0.005).abs() < 1e-12);
        assert_eq!(r.side(), proto::OffsetSide::Outward);
        assert!(r.add_dimension && r.model_persist_ids.is_empty() && r.sketch_name.is_empty());

        let mut p = params(&[&a]);
        p.side = OffsetSide::Both;
        p.caps = OffsetCaps::Arcs;
        p.construction = true;
        p.base_construction = true;
        p.add_dimension = Some(false);
        let r = request(&ids, &p).unwrap();
        assert_eq!(r.side(), proto::OffsetSide::Both);
        assert_eq!(r.caps(), proto::OffsetCaps::Arcs);
        assert!(r.offset_construction && r.base_construction && !r.add_dimension);
    }

    #[test]
    fn a_whole_sketch_by_name() {
        let ids = registry();
        let mut p = params(&[]);
        p.target.sketch_name = Some(" Sketch1 ".into());
        let r = request(&ids, &p).unwrap();
        assert_eq!(r.sketch_name, "Sketch1");
        assert!(r.items.is_empty() && r.model_persist_ids.is_empty());
    }

    #[test]
    fn a_single_face_gets_a_new_sketch_on_itself() {
        let mut ids = registry();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let r = request(&ids, &params(&[&face])).unwrap();
        assert_eq!(r.model_persist_ids, vec![b"face".to_vec()]);
        assert_eq!(r.on_persist_id, b"face");
        assert_eq!(r.plane(), proto::Plane::Unspecified);
        assert!(!r.add_dimension);
    }

    #[test]
    fn edges_go_where_they_are_told() {
        let mut ids = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        assert!(request(&ids, &params(&[&edge])).is_err());
        assert!(request(&ids, &params(&[&edge, &face])).is_err());

        let mut p = params(&[&edge]);
        p.target.on = Some(face.clone());
        let r = request(&ids, &p).unwrap();
        assert_eq!(r.on_persist_id, b"face");

        let mut p = params(&[&edge]);
        p.target.plane = Some(crate::SketchPlane::Top);
        assert_eq!(request(&ids, &p).unwrap().plane(), proto::Plane::Top);

        let mut p = params(&[&edge]);
        p.target.sketch_name = Some("Sketch2".into());
        let r = request(&ids, &p).unwrap();
        assert_eq!(r.sketch_name, "Sketch2");
        assert!(r.on_persist_id.is_empty());
        assert_eq!(r.plane(), proto::Plane::Unspecified);

        p.target.on = Some(face);
        assert!(request(&ids, &p).is_err());
    }

    #[test]
    fn refusals() {
        let mut ids = registry();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let (a, _) = line(&mut ids, 1);
        let point = {
            let r = proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point: true, id0: 0, id1: 9, ..Default::default() };
            sketch::ref_id(&mut ids, Kind::SketchPoint, &r).unwrap()
        };
        assert!(request(&ids, &params(&[])).is_err());
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut p = params(&[&a]);
            p.distance_mm = bad;
            assert!(request(&ids, &p).is_err(), "{bad}");
        }
        for bad in [&point, "line:99", "edge:99", "face:99", "Boss-Extrude1", "origin"] {
            let mut p = params(&[bad]);
            p.target.sketch_name = Some("Sketch1".into());
            assert!(request(&ids, &p).is_err(), "{bad}");
        }
        assert!(request(&ids, &params(&[&a, &face])).is_err());

        let mut p = params(&[&a]);
        p.caps = OffsetCaps::Lines;
        assert!(request(&ids, &p).is_err());
        let mut p = params(&[&a]);
        p.target.plane = Some(crate::SketchPlane::Top);
        assert!(request(&ids, &p).is_err());

        let model = |set: &dyn Fn(&mut OffsetParams)| {
            let mut p = params(&[&face]);
            set(&mut p);
            request(&ids, &p).is_err()
        };
        assert!(model(&|p| p.construction = true));
        assert!(model(&|p| p.base_construction = true));
        assert!(model(&|p| p.add_dimension = Some(true)));
        assert!(model(&|p| {
            p.side = OffsetSide::Both;
            p.caps = OffsetCaps::Arcs;
        }));
        assert!(!model(&|p| p.add_dimension = Some(false)));
    }

    #[test]
    fn replies() {
        let mut ids = registry();
        let (a, _) = line(&mut ids, 1);
        let (_, new_ref) = line(&mut ids, 2);
        let item = proto::SketchItem {
            r#ref: Some(new_ref.clone()),
            r#type: proto::SketchItemType::Line as i32,
            ..Default::default()
        };
        let reply = proto::OffsetSketchEntitiesResponse {
            sketch_name: "Sketch1".into(),
            created_sketch: false,
            items: vec![item.clone()],
            contours: Some(proto::SketchContours { closed: 2, open: 0 }),
            state: None,
            new_errors: vec![],
        };
        let text = reply_text(&mut ids, &params(&[&a]), &reply);
        let new_id = sketch::ref_id(&mut ids, Kind::Line, &new_ref).unwrap();
        assert!(text.starts_with(&format!("offset {a} by 5 mm outward in Sketch1. New: {new_id}.")), "{text}");
        assert!(text.contains("is a dimension") && text.contains("Sketch1 now has 2 closed and 0 open contours."), "{text}");

        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let mut p = params(&[&face]);
        p.side = OffsetSide::Inward;
        let reply = proto::OffsetSketchEntitiesResponse { created_sketch: true, ..reply };
        let text = reply_text(&mut ids, &p, &reply);
        assert!(text.starts_with(&format!("offset {face} by 5 mm inward in Sketch1 (new sketch).")), "{text}");
        assert!(text.contains("outer loop only") && !text.contains("is a dimension"), "{text}");

        let mut p = params(&[]);
        p.side = OffsetSide::Both;
        assert!(reply_text(&mut ids, &p, &reply).starts_with("offset the whole sketch by 5 mm to both sides"));
    }
}
