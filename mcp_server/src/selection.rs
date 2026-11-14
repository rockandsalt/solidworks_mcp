//! selection: what the user has selected in SolidWorks, as ids the other
//! tools take, or selecting entities so the user sees them highlighted.
//! Details in docs/notes/selection.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{self, IdRegistry, Kind, point_mm};
use crate::refgeom::geometry_ref;

/// Most entities one call selects.
const MAX_SELECT: usize = 200;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SelectionParams {
    /// Select these in SolidWorks first (highlighted, as if clicked), replacing what's selected:
    /// inspect_model ids of faces, edges, vertices, features, bodies or components ("face:7",
    /// "feature:3"), sketch entity ids ("line:3"), "origin", "front" / "top" / "right", or a
    /// feature's name ("Boss-Extrude1"). Leave out to just read the selection.
    #[serde(default)]
    pub select: Vec<String>,
    /// Add `select` to what's already selected instead of replacing it.
    #[serde(default)]
    pub append: bool,
    /// Clear the selection (and select nothing).
    #[serde(default)]
    pub clear: bool,
}

pub fn request(ids: &IdRegistry, params: &SelectionParams) -> Result<proto::SelectionRequest, McpError> {
    if params.clear && (params.append || !params.select.is_empty()) {
        return Err(invalid("clear empties the selection; give it alone, or give select without clear"));
    }
    if params.append && params.select.is_empty() {
        return Err(invalid("append adds `select` to the selection; give what to select"));
    }
    if params.select.len() > MAX_SELECT {
        return Err(invalid(format!("at most {MAX_SELECT} entities, got {}", params.select.len())));
    }
    let mut select = Vec::new();
    for (i, entity) in params.select.iter().enumerate() {
        let what = format!("select[{i}]");
        let entity = entity.trim();
        let r = if matches!(Kind::of_id(entity), Some(Kind::Body | Kind::Component)) {
            let persist_id = ids.persist_id(entity).ok_or_else(|| {
                invalid(format!(
                    "{what}: {}",
                    ids.unknown_id(entity, "inspect_model", "call inspect_model for current ones")
                ))
            })?;
            proto::GeometryRef { target: Some(proto::geometry_ref::Target::ModelPersistId(persist_id)) }
        } else {
            geometry_ref(ids, entity, &what)?.0
        };
        select.push(r);
    }
    Ok(proto::SelectionRequest { select, append: params.append, clear: params.clear })
}

/// One selected item: inspect_model's JSON for it (with its id), where it
/// was clicked, and the mark it was selected with if not 0.
fn item_json(ids: &mut IdRegistry, components: &[proto::ComponentInfo], item: &proto::SelectedItem) -> Value {
    use proto::selected_item::Item;
    let mut value = match &item.item {
        Some(Item::Face(f)) => inspect::face_json(ids, f),
        Some(Item::Edge(e)) => inspect::edge_json(ids, e),
        Some(Item::Feature(f)) => inspect::feature_json(ids, f),
        Some(Item::Body(b)) => inspect::body_json(ids, b),
        Some(Item::Vertex(v)) => {
            let mut obj = Map::new();
            obj.insert("id".into(), ids.id_for(Kind::Vertex, &v.persist_id).map_or(Value::Null, Value::String));
            inspect::insert_component(ids, &mut obj, &v.component_persist_id);
            if let Some(p) = &v.position {
                obj.insert("at_mm".into(), point_mm(p));
            }
            Value::Object(obj)
        }
        Some(Item::ComponentPersistId(pid)) => match components.iter().find(|c| &c.persist_id == pid) {
            Some(c) => crate::assembly::component_json(ids, c),
            None => json!({ "id": ids.id_for(Kind::Component, pid) }),
        },
        Some(Item::SketchItem(s)) => {
            let mut value = crate::sketch::item_json(ids, s, item.sketch_3d);
            if let Value::Object(obj) = &mut value
                && !item.sketch_name.is_empty()
            {
                obj.insert("sketch".into(), json!(item.sketch_name));
            }
            value
        }
        None => json!({
            "id": null,
            "note": format!(
                "something the tools don't take (a dimension, note, ...: swSelectType_e {})",
                item.select_type
            ),
        }),
    };
    if let Value::Object(obj) = &mut value {
        // Exactly [0, 0, 0]: selected without a click (through the API).
        // Clicks land at screen precision, never exactly on the origin.
        if let Some(p) = item.point.as_ref().filter(|p| (p.x, p.y, p.z) != (0.0, 0.0, 0.0)) {
            obj.insert("picked_at_mm".into(), point_mm(p));
        }
        if item.mark != 0 {
            obj.insert("mark".into(), json!(item.mark));
        }
    }
    value
}

pub fn render(ids: &mut IdRegistry, reply: &proto::SelectionResponse) -> Value {
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }
    let selected: Vec<Value> = reply.items.iter().map(|item| item_json(ids, &reply.components, item)).collect();
    if selected.is_empty() {
        out.insert(
            "note".into(),
            json!("nothing is selected in SolidWorks -- ask the user to pick it there, or find it with inspect_model"),
        );
    }
    out.insert("selected".into(), Value::Array(selected));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_contradictions() {
        let ids = IdRegistry::default();
        let both = SelectionParams { select: vec!["front".into()], clear: true, ..Default::default() };
        assert!(request(&ids, &both).is_err());
        let append = SelectionParams { append: true, ..Default::default() };
        assert!(request(&ids, &append).is_err());
        assert!(request(&ids, &SelectionParams::default()).is_ok());
    }

    #[test]
    fn names_and_planes_go_through() {
        let ids = IdRegistry::default();
        let params = SelectionParams { select: vec!["Boss-Extrude1".into(), "top".into()], ..Default::default() };
        let r = request(&ids, &params).unwrap();
        assert_eq!(r.select.len(), 2);
        assert!(!r.append && !r.clear);
    }

    #[test]
    fn unknown_ids_are_refused() {
        let ids = IdRegistry::default();
        let params = SelectionParams { select: vec!["face:99".into()], ..Default::default() };
        assert!(request(&ids, &params).is_err());
    }

    #[test]
    fn empty_selection_says_so() {
        let mut ids = IdRegistry::default();
        let v = render(&mut ids, &proto::SelectionResponse::default());
        assert!(v["note"].as_str().unwrap().contains("nothing is selected"));
        assert_eq!(v["selected"], json!([]));
    }

    #[test]
    fn faces_get_ids_and_pick_points() {
        let mut ids = IdRegistry::default();
        let reply = proto::SelectionResponse {
            items: vec![proto::SelectedItem {
                point: Some(proto::Vec3 { x: 0.01, y: 0.02, z: 0.0 }),
                item: Some(proto::selected_item::Item::Face(proto::FaceInfo {
                    persist_id: vec![1, 2, 3],
                    feature_name: "Boss-Extrude1".into(),
                    ..Default::default()
                })),
                ..Default::default()
            }],
            ..Default::default()
        };
        let v = render(&mut ids, &reply);
        let face = &v["selected"][0];
        let id = face["id"].as_str().unwrap();
        assert!(id.starts_with("face:"));
        assert_eq!(ids.persist_id(id), Some(vec![1, 2, 3]));
        assert_eq!(face["feature"], json!("Boss-Extrude1"));
        assert_eq!(face["picked_at_mm"], json!([10.0, 20.0, 0.0]));
        // Selected through the API: SolidWorks says [0, 0, 0], left out.
        let mut api = reply.clone();
        api.items[0].point = Some(proto::Vec3::default());
        let v = render(&mut ids, &api);
        assert!(v["selected"][0].get("picked_at_mm").is_none(), "{v}");
    }
}
