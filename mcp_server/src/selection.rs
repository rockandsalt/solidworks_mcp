//! selection: what the user has selected in SolidWorks, as ids the other
//! tools take, or selecting entities so the user sees them highlighted.
//! Details in docs/notes/selection.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use glam::DVec2;

use crate::addin::proto;
use crate::drawing::{self, sheet_mm};
use crate::inspect::{self, DRAWING_KINDS, IdRegistry, Kind, point_mm};
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
    /// feature's name ("Boss-Extrude1"). In a drawing: inspect_drawing's view entity ids
    /// ("view_edge:3"), or the model's faces, edges, vertices and features (inspect_model ids
    /// of a model a view shows, or feature names) to select where the views draw them; one
    /// sheet at a time. Leave out to just read the selection.
    #[serde(default)]
    pub select: Vec<String>,
    /// Add `select` to what's already selected instead of replacing it.
    #[serde(default)]
    pub append: bool,
    /// Clear the selection (and select nothing).
    #[serde(default)]
    pub clear: bool,
}

/// What every call is refused for, in any document.
pub fn check(params: &SelectionParams) -> Result<(), McpError> {
    if params.clear && (params.append || !params.select.is_empty()) {
        return Err(invalid(
            "clear empties the selection; give it alone, or give select without clear",
        ));
    }
    if params.append && params.select.is_empty() {
        return Err(invalid(
            "append adds `select` to the selection; give what to select",
        ));
    }
    if params.select.len() > MAX_SELECT {
        return Err(invalid(format!(
            "at most {MAX_SELECT} entities, got {}",
            params.select.len()
        )));
    }
    Ok(())
}

pub fn request(
    ids: &IdRegistry,
    params: &SelectionParams,
) -> Result<proto::SelectionRequest, McpError> {
    check(params)?;
    let mut select = Vec::new();
    for (i, entity) in params.select.iter().enumerate() {
        let what = format!("select[{i}]");
        let entity = entity.trim();
        if Kind::of_id(entity).is_some_and(|k| DRAWING_KINDS.contains(&k)) {
            return Err(invalid(format!(
                "{what}: {entity} is a drawing's view entity, and the active document isn't a drawing -- \
                 activate the drawing first, or select the model's own entities (inspect_drawing's model_id)"
            )));
        }
        let r = if matches!(Kind::of_id(entity), Some(Kind::Body | Kind::Component)) {
            let persist_id = ids.persist_id(entity).ok_or_else(|| {
                invalid(format!(
                    "{what}: {}",
                    ids.unknown_id(
                        entity,
                        "inspect_model",
                        "call inspect_model for current ones"
                    )
                ))
            })?;
            proto::GeometryRef {
                target: Some(proto::geometry_ref::Target::ModelPersistId(persist_id)),
            }
        } else {
            geometry_ref(ids, entity, &what)?.0
        };
        select.push(r);
    }
    Ok(proto::SelectionRequest {
        select,
        append: params.append,
        clear: params.clear,
        ..Default::default()
    })
}

/// Something to select in a drawing: a view entity by its id, or a model
/// entity to find in the views that show its model.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawingPick {
    View(proto::DrawingEntityRef),
    /// An inspect_model id (face, edge, vertex, feature) of the model
    /// document `key` (`files::document_key`).
    ModelId {
        id: String,
        key: String,
        kind: Kind,
        persist_id: Vec<u8>,
    },
    /// A feature by name, in whatever views' models have it.
    FeatureName(String),
}

/// `select` in a drawing: view entity ids as they are, model ids and
/// feature names to look for in the views.
pub fn drawing_picks(
    ids: &IdRegistry,
    params: &SelectionParams,
) -> Result<Vec<DrawingPick>, McpError> {
    let mut picks = Vec::new();
    for (i, entity) in params.select.iter().enumerate() {
        let what = format!("select[{i}]");
        let entity = entity.trim();
        let pick = match Kind::of_id(entity) {
            Some(k) if DRAWING_KINDS.contains(&k) => DrawingPick::View(
                drawing::decode_ref(ids, entity)
                    .map_err(|e| invalid(format!("{what}: {}", e.message)))?,
            ),
            Some(kind @ (Kind::Face | Kind::Edge | Kind::Vertex | Kind::Feature)) => {
                let (key, bytes) = ids.lookup(entity).ok_or_else(|| {
                    invalid(format!(
                        "{what}: {}",
                        ids.unknown_id(
                            entity,
                            "inspect_model or inspect_drawing (model_id)",
                            "call one for current ones"
                        )
                    ))
                })?;
                DrawingPick::ModelId {
                    id: entity.to_string(),
                    key: key.to_string(),
                    kind,
                    persist_id: bytes.to_vec(),
                }
            }
            Some(_) => {
                return Err(invalid(format!(
                    "{what}: {entity} isn't drawn as such in a view -- in a drawing, select view entity ids \
                     (inspect_drawing), or a model's faces, edges, vertices and features (inspect_model ids, or \
                     feature names) to select where the views show them"
                )));
            }
            None if entity.eq_ignore_ascii_case("origin")
                || crate::refgeom::standard_plane(entity).is_some() =>
            {
                return Err(invalid(format!(
                    "{what}: {entity:?} names a part's or assembly's plane or origin, which a drawing view \
                     doesn't list -- select view entities, or model faces, edges and features"
                )));
            }
            None => DrawingPick::FeatureName(entity.to_string()),
        };
        picks.push(pick);
    }
    Ok(picks)
}

/// The view entities each pick stands for, from an InspectDrawing reply
/// listing the views that could show them, all on one sheet: the active
/// one if anything is found there, else the first that has something.
/// A pick found nowhere (hidden, in no view), or only on another sheet, is
/// refused.
pub fn resolve_picks(
    picks: &[DrawingPick],
    reply: &proto::InspectDrawingResponse,
) -> Result<Vec<proto::DrawingEntityRef>, McpError> {
    let found = |pick: &DrawingPick, sheet: &proto::SheetInfo| -> Vec<proto::DrawingEntityRef> {
        let mut refs = Vec::new();
        for view in &sheet.views {
            let model =
                (!view.model_file.is_empty()).then(|| crate::files::file_key(&view.model_file));
            for e in &view.entities {
                if pick_matches(pick, model.as_deref(), e) {
                    refs.push(drawing::entity_ref(&view.name, e));
                }
            }
        }
        refs
    };
    let mut sheets: Vec<&proto::SheetInfo> = reply.sheets.iter().collect();
    sheets.sort_by_key(|s| !s.active);
    let models = picks
        .iter()
        .filter(|p| !matches!(p, DrawingPick::View(_)))
        .collect::<Vec<_>>();
    let Some(sheet) = sheets
        .iter()
        .find(|s| models.iter().any(|p| !found(p, s).is_empty()))
    else {
        let names: Vec<String> = models.iter().map(|p| pick_label(p)).collect();
        return Err(invalid(format!(
            "{} isn't drawn in any view of the drawing {NOT_DRAWN}",
            names.join(", ")
        )));
    };
    let mut refs: Vec<proto::DrawingEntityRef> = Vec::new();
    for pick in picks {
        let matched = match pick {
            DrawingPick::View(r) => vec![r.clone()],
            _ => found(pick, sheet),
        };
        if matched.is_empty() {
            let elsewhere: Vec<&str> = sheets
                .iter()
                .filter(|s| !found(pick, s).is_empty())
                .map(|s| s.name.as_str())
                .collect();
            let label = pick_label(pick);
            return Err(invalid(if elsewhere.is_empty() {
                format!("{label} isn't drawn in any view of the drawing {NOT_DRAWN}")
            } else {
                format!(
                    "{label} is drawn on {}, the others on {}: select one sheet's at a time",
                    elsewhere.join(", "),
                    sheet.name
                )
            }));
        }
        for r in matched {
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
    }
    Ok(refs)
}

/// Why a model entity may not be found in the views.
const NOT_DRAWN: &str = "(hidden, cut away, or no view shows its model; or its id is from before the model was \
                         first saved, which gives faces and edges new ids: inspect_model it again, or take \
                         inspect_drawing's model_id) -- inspect_drawing lists what each view shows";

fn pick_label(pick: &DrawingPick) -> String {
    match pick {
        DrawingPick::ModelId { id, .. } => id.clone(),
        DrawingPick::FeatureName(name) => format!("feature {name:?}"),
        DrawingPick::View(r) => r.view_name.clone(),
    }
}

/// Whether view entity `e` of a view showing the model with key `model`
/// draws what `pick` names: the entity itself (edges, vertices; faces by
/// their silhouettes and the edges around them) or one made by the feature.
fn pick_matches(pick: &DrawingPick, model: Option<&str>, e: &proto::ViewEntity) -> bool {
    use proto::ViewEntityKind as K;
    match pick {
        DrawingPick::View(_) => false,
        DrawingPick::FeatureName(name) => e.features.iter().any(|f| &f.name == name),
        DrawingPick::ModelId {
            key,
            kind,
            persist_id,
            ..
        } => {
            if model != Some(key.as_str()) {
                return false;
            }
            match kind {
                Kind::Edge => e.kind() == K::Edge && &e.model_persist_id == persist_id,
                Kind::Vertex => e.kind() == K::Vertex && &e.model_persist_id == persist_id,
                Kind::Face => match e.kind() {
                    K::Silhouette => &e.model_persist_id == persist_id,
                    K::Edge => e.model_face_persist_ids.contains(persist_id),
                    _ => false,
                },
                Kind::Feature => e.features.iter().any(|f| &f.persist_id == persist_id),
                _ => false,
            }
        }
    }
}

/// The view entities to select when every pick is one (nothing to look
/// for in the views); None otherwise.
pub fn view_refs(picks: &[DrawingPick]) -> Option<Vec<proto::DrawingEntityRef>> {
    picks
        .iter()
        .map(|p| match p {
            DrawingPick::View(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// InspectDrawing for the views (and the models they show), without
/// entities: where to look for model picks.
pub fn overview_request() -> proto::InspectDrawingRequest {
    proto::InspectDrawingRequest {
        skip_sheet_format: true,
        ..Default::default()
    }
}

/// InspectDrawing for the entities of the views that could draw `picks`
/// (vertices only when a vertex is asked for); None if no view could.
pub fn detail_request(
    picks: &[DrawingPick],
    overview: &proto::InspectDrawingResponse,
) -> Option<proto::InspectDrawingRequest> {
    let entity_views = views_for_picks(picks, overview);
    let include_vertices = picks.iter().any(|p| {
        matches!(
            p,
            DrawingPick::ModelId {
                kind: Kind::Vertex,
                ..
            }
        )
    });
    (!entity_views.is_empty()).then(|| proto::InspectDrawingRequest {
        entity_views,
        include_vertices,
        ..overview_request()
    })
}

/// The SelectionRequest selecting `refs`, view entities of a drawing.
pub fn drawing_request(
    params: &SelectionParams,
    refs: Vec<proto::DrawingEntityRef>,
) -> Result<proto::SelectionRequest, McpError> {
    if refs.len() > MAX_SELECT {
        return Err(invalid(format!(
            "that's {} view entities, more than {MAX_SELECT} at a time: select fewer",
            refs.len()
        )));
    }
    Ok(proto::SelectionRequest {
        append: params.append,
        clear: params.clear,
        select_view_entities: refs,
        ..Default::default()
    })
}

/// The views to list entities of for `picks`: those showing a model a
/// model id is from, and with a feature name every view with a model.
fn views_for_picks(picks: &[DrawingPick], overview: &proto::InspectDrawingResponse) -> Vec<String> {
    let any_name = picks
        .iter()
        .any(|p| matches!(p, DrawingPick::FeatureName(_)));
    let keys: Vec<&str> = picks
        .iter()
        .filter_map(|p| match p {
            DrawingPick::ModelId { key, .. } => Some(key.as_str()),
            _ => None,
        })
        .collect();
    overview
        .sheets
        .iter()
        .flat_map(|s| &s.views)
        .filter(|v| !v.model_file.is_empty())
        .filter(|v| any_name || keys.contains(&crate::files::file_key(&v.model_file).as_str()))
        .map(|v| v.name.clone())
        .collect()
}

/// One selected item: inspect_model's JSON for it in a part or assembly,
/// inspect_drawing's in a drawing (with its id), where it was clicked, and
/// the mark it was selected with if not 0.
fn item_json(
    ids: &mut IdRegistry,
    cache: &mut drawing::Cache,
    reply: &proto::SelectionResponse,
    item: &proto::SelectedItem,
) -> Value {
    let in_drawing = is_drawing(reply);
    let mut value = if in_drawing {
        drawing_item_json(ids, cache, reply, item)
    } else {
        model_item_json(ids, reply, item)
    };
    if let Value::Object(obj) = &mut value {
        // Exactly [0, 0, 0]: selected without a click (through the API).
        // Clicks land at screen precision, never exactly on the origin.
        if let Some(p) = item
            .point
            .as_ref()
            .filter(|p| (p.x, p.y, p.z) != (0.0, 0.0, 0.0))
        {
            let at = if in_drawing {
                sheet_mm(DVec2::new(p.x, p.y))
            } else {
                point_mm(p)
            };
            obj.insert("picked_at_mm".into(), at);
        }
        if item.mark != 0 {
            obj.insert("mark".into(), json!(item.mark));
        }
    }
    value
}

fn is_drawing(reply: &proto::SelectionResponse) -> bool {
    reply
        .document
        .as_ref()
        .is_some_and(|d| d.r#type() == proto::DocumentType::Drawing)
}

/// A part's or assembly's selected item as inspect_model gives it.
fn model_item_json(
    ids: &mut IdRegistry,
    reply: &proto::SelectionResponse,
    item: &proto::SelectedItem,
) -> Value {
    use proto::selected_item::Item;
    match &item.item {
        Some(Item::Face(f)) => inspect::face_json(ids, f),
        Some(Item::Edge(e)) => inspect::edge_json(ids, e),
        Some(Item::Feature(f)) => inspect::feature_json(ids, f),
        Some(Item::Body(b)) => inspect::body_json(ids, b),
        Some(Item::Vertex(v)) => {
            let mut obj = Map::new();
            obj.insert(
                "id".into(),
                ids.id_for(Kind::Vertex, &v.persist_id)
                    .map_or(Value::Null, Value::String),
            );
            inspect::insert_component(ids, &mut obj, &v.component_persist_id);
            if let Some(p) = &v.position {
                obj.insert("at_mm".into(), point_mm(p));
            }
            Value::Object(obj)
        }
        Some(Item::ComponentPersistId(pid)) => {
            match reply.components.iter().find(|c| &c.persist_id == pid) {
                Some(c) => crate::assembly::component_json(ids, c),
                None => json!({ "id": ids.id_for(Kind::Component, pid) }),
            }
        }
        Some(Item::SketchItem(s)) => {
            let mut value = crate::sketch::item_json(ids, s, item.sketch_3d);
            if let Value::Object(obj) = &mut value
                && !item.sketch_name.is_empty()
            {
                obj.insert("sketch".into(), json!(item.sketch_name));
            }
            value
        }
        _ => json!({
            "id": null,
            "note": format!(
                "something the tools don't take (a dimension, note, ...: swSelectType_e {})",
                item.select_type
            ),
        }),
    }
}

/// A drawing's selected item: a view entity, annotation or view as
/// inspect_drawing gives it, or a feature picked under a view in the tree
/// as inspect_model gives it in the view's model (with that model's id).
/// Items in a view say which (`view`), and `model` when they carry its ids.
fn drawing_item_json(
    ids: &mut IdRegistry,
    cache: &mut drawing::Cache,
    reply: &proto::SelectionResponse,
    item: &proto::SelectedItem,
) -> Value {
    use proto::selected_item::Item;
    let view = reply.views.iter().find(|v| v.name == item.view_name);
    let model_file = view
        .map(|v| v.model_file.as_str())
        .filter(|f| !f.is_empty());
    let mut value = match &item.item {
        Some(Item::ViewEntity(e)) => view.and_then(|v| drawing::selected_entity_json(ids, cache, v, e)).unwrap_or_else(|| {
            json!({
                "id": null,
                "note": format!("no sheet geometry for it: call inspect_drawing with view={:?}", item.view_name),
            })
        }),
        Some(Item::Annotation(a)) => drawing::annotation_json(ids, Some(&item.view_name), a),
        Some(Item::DrawingView(name)) => json!({ "drawing_view": name }),
        Some(Item::Feature(f)) => match model_file {
            Some(file) => ids.in_document(&crate::files::file_key(file), drawing::file_name(file), |ids| {
                inspect::feature_json(ids, f)
            }),
            None => json!({ "id": null, "name": f.name }),
        },
        _ => {
            let place = if item.view_name.is_empty() { "the drawing" } else { item.view_name.as_str() };
            json!({
                "id": null,
                "note": format!(
                    "something in {place} the tools don't name (a sheet, a table, ...: swSelectType_e {}) -- \
                     inspect_drawing lists the views' edges and annotations",
                    item.select_type
                ),
            })
        }
    };
    if let Value::Object(obj) = &mut value
        && !item.view_name.is_empty()
    {
        obj.insert("view".into(), json!(item.view_name));
        // Model ids (model_id, a feature's id) are that model's.
        if let Some(file) = model_file
            && (obj.contains_key("model_id") || matches!(item.item, Some(Item::Feature(_))))
        {
            obj.insert("model".into(), json!(drawing::file_name(file)));
        }
    }
    value
}

/// The selection JSON. Drawing views' sheet geometry goes into `cache`, so
/// add_drawing_dimensions takes the ids without an inspect_drawing first.
pub fn render(
    ids: &mut IdRegistry,
    cache: &mut drawing::Cache,
    reply: &proto::SelectionResponse,
) -> Value {
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }
    cache.sheet_changed(&reply.views, false);
    let selected: Vec<Value> = reply
        .items
        .iter()
        .map(|item| item_json(ids, cache, reply, item))
        .collect();
    if selected.is_empty() {
        let find = if is_drawing(reply) {
            "inspect_drawing"
        } else {
            "inspect_model"
        };
        out.insert(
            "note".into(),
            json!(format!("nothing is selected in SolidWorks -- ask the user to pick it there, or find it with {find}")),
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
        let both = SelectionParams {
            select: vec!["front".into()],
            clear: true,
            ..Default::default()
        };
        assert!(request(&ids, &both).is_err());
        let append = SelectionParams {
            append: true,
            ..Default::default()
        };
        assert!(request(&ids, &append).is_err());
        assert!(request(&ids, &SelectionParams::default()).is_ok());
    }

    #[test]
    fn names_and_planes_go_through() {
        let ids = IdRegistry::default();
        let params = SelectionParams {
            select: vec!["Boss-Extrude1".into(), "top".into()],
            ..Default::default()
        };
        let r = request(&ids, &params).unwrap();
        assert_eq!(r.select.len(), 2);
        assert!(!r.append && !r.clear);
    }

    #[test]
    fn unknown_ids_are_refused() {
        let ids = IdRegistry::default();
        let params = SelectionParams {
            select: vec!["face:99".into()],
            ..Default::default()
        };
        assert!(request(&ids, &params).is_err());
    }

    #[test]
    fn view_entity_ids_are_refused_outside_drawings() {
        let mut ids = IdRegistry::default();
        let r = proto::DrawingEntityRef {
            view_name: "Drawing View1".into(),
            kind: proto::ViewEntityKind::Edge as i32,
            persist_id: vec![7],
            ..Default::default()
        };
        let id = drawing::id_for(&mut ids, &r).unwrap();
        let params = SelectionParams {
            select: vec![id],
            ..Default::default()
        };
        let error = request(&ids, &params).unwrap_err();
        assert!(
            error.message.contains("isn't a drawing"),
            "{}",
            error.message
        );
    }

    #[test]
    fn view_picks_go_straight_to_the_addin() {
        let r = proto::DrawingEntityRef {
            view_name: "V1".into(),
            persist_id: vec![7],
            ..Default::default()
        };
        let refs = view_refs(&[DrawingPick::View(r.clone())]).unwrap();
        assert_eq!(refs, vec![r.clone()]);
        let mixed = [
            DrawingPick::View(r),
            DrawingPick::FeatureName("Boss-Extrude1".into()),
        ];
        assert!(view_refs(&mixed).is_none());
        let params = SelectionParams {
            append: true,
            ..Default::default()
        };
        let request = drawing_request(&params, refs).unwrap();
        assert!(
            request.append && request.select.is_empty() && request.select_view_entities.len() == 1
        );
        let many = vec![proto::DrawingEntityRef::default(); MAX_SELECT + 1];
        assert!(drawing_request(&params, many).is_err());
    }

    #[test]
    fn model_picks_list_only_the_views_of_their_model() {
        let overview = drawing_of_block();
        let edge = [model_pick("edge:1", Kind::Edge, b"e1")];
        let request = detail_request(&edge, &overview).unwrap();
        assert_eq!(
            request.entity_views,
            vec!["V2".to_string(), "V1".to_string()]
        );
        assert!(!request.include_vertices && request.skip_sheet_format);
        let vertex = [model_pick("vertex:1", Kind::Vertex, b"v1")];
        assert!(detail_request(&vertex, &overview).unwrap().include_vertices);
        let mut other = model_pick("edge:1", Kind::Edge, b"e1");
        if let DrawingPick::ModelId { key, .. } = &mut other {
            *key = crate::files::file_key("c:\\d\\other.sldprt");
        }
        assert!(detail_request(&[other], &overview).is_none());
    }

    fn drawing_document() -> Option<proto::Document> {
        Some(proto::Document {
            title: "Draw".into(),
            r#type: proto::DocumentType::Drawing as i32,
            ..Default::default()
        })
    }

    #[test]
    fn unknown_drawing_items_point_to_inspect_drawing() {
        let reply = proto::SelectionResponse {
            document: drawing_document(),
            items: vec![proto::SelectedItem {
                select_type: 99,
                ..Default::default()
            }],
            ..Default::default()
        };
        let v = render(
            &mut IdRegistry::default(),
            &mut drawing::Cache::default(),
            &reply,
        );
        let note = v["selected"][0]["note"].as_str().unwrap();
        assert!(
            note.contains("the drawing") && note.contains("inspect_drawing"),
            "{v}"
        );
        assert!(v["selected"][0].get("view").is_none(), "{v}");
    }

    #[test]
    fn drawing_items_say_their_view_and_sheet_pick_point() {
        let mut ids = IdRegistry::default();
        let mut cache = drawing::Cache::default();
        let reply = proto::SelectionResponse {
            items: vec![
                proto::SelectedItem {
                    view_name: "Drawing View1".into(),
                    point: Some(proto::Vec3 {
                        x: 0.1,
                        y: 0.125,
                        z: 0.0,
                    }),
                    item: Some(proto::selected_item::Item::ViewEntity(
                        proto::ViewEntity::default(),
                    )),
                    ..Default::default()
                },
                proto::SelectedItem {
                    item: Some(proto::selected_item::Item::DrawingView(
                        "Drawing View2".into(),
                    )),
                    ..Default::default()
                },
            ],
            document: drawing_document(),
            ..Default::default()
        };
        let v = render(&mut ids, &mut cache, &reply);
        // No view described: no sheet geometry, so no id.
        let e = &v["selected"][0];
        assert!(
            e["id"].is_null() && e["note"].as_str().unwrap().contains("inspect_drawing"),
            "{v}"
        );
        assert_eq!(e["view"], "Drawing View1");
        assert_eq!(e["picked_at_mm"], json!([100.0, 125.0]));
        assert_eq!(v["selected"][1]["drawing_view"], "Drawing View2");
    }

    fn view_entity(
        kind: proto::ViewEntityKind,
        pid: &[u8],
        model: &[u8],
        faces: &[&[u8]],
        feature: &str,
    ) -> proto::ViewEntity {
        proto::ViewEntity {
            kind: kind as i32,
            persist_id: pid.to_vec(),
            model_persist_id: model.to_vec(),
            model_face_persist_ids: faces.iter().map(|f| f.to_vec()).collect(),
            features: vec![proto::ModelFeatureRef {
                name: feature.into(),
                persist_id: feature.as_bytes().to_vec(),
            }],
            ..Default::default()
        }
    }

    /// Sheet1 (active): a view of block.SLDPRT with an edge (model "e1",
    /// on faces "f1" / "f2") and a silhouette of face "f3"; Sheet2: a view
    /// of it with edge "e2".
    fn drawing_of_block() -> proto::InspectDrawingResponse {
        use proto::ViewEntityKind as K;
        let view = |name: &str, entities| proto::ViewInfo {
            name: name.into(),
            model_file: "C:\\d\\Block.SLDPRT".into(),
            entities,
            ..Default::default()
        };
        proto::InspectDrawingResponse {
            sheets: vec![
                proto::SheetInfo {
                    name: "Sheet2".into(),
                    views: vec![view(
                        "V2",
                        vec![view_entity(
                            K::Edge,
                            b"d3",
                            b"e2",
                            &[b"f2"],
                            "Boss-Extrude1",
                        )],
                    )],
                    ..Default::default()
                },
                proto::SheetInfo {
                    name: "Sheet1".into(),
                    active: true,
                    views: vec![view(
                        "V1",
                        vec![
                            view_entity(K::Edge, b"d1", b"e1", &[b"f1", b"f2"], "Boss-Extrude1"),
                            view_entity(K::Silhouette, b"d2", b"f3", &[b"f3"], "Boss-Extrude2"),
                        ],
                    )],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn model_pick(id: &str, kind: Kind, pid: &[u8]) -> DrawingPick {
        DrawingPick::ModelId {
            id: id.into(),
            key: crate::files::file_key("c:\\d\\block.sldprt"),
            kind,
            persist_id: pid.to_vec(),
        }
    }

    #[test]
    fn model_picks_find_what_the_views_draw() {
        let reply = drawing_of_block();
        let views = |refs: Vec<proto::DrawingEntityRef>| -> Vec<(String, Vec<u8>)> {
            refs.into_iter()
                .map(|r| (r.view_name, r.persist_id))
                .collect()
        };
        // An edge; a face by its silhouette and the edges around it.
        let r = resolve_picks(&[model_pick("edge:1", Kind::Edge, b"e1")], &reply).unwrap();
        assert_eq!(views(r), vec![("V1".to_string(), b"d1".to_vec())]);
        let r = resolve_picks(&[model_pick("face:3", Kind::Face, b"f3")], &reply).unwrap();
        assert_eq!(views(r), vec![("V1".to_string(), b"d2".to_vec())]);
        // A feature by id or name: on the active sheet first.
        let r = resolve_picks(
            &[model_pick("feature:1", Kind::Feature, b"Boss-Extrude1")],
            &reply,
        )
        .unwrap();
        assert_eq!(views(r), vec![("V1".to_string(), b"d1".to_vec())]);
        let r = resolve_picks(&[DrawingPick::FeatureName("Boss-Extrude2".into())], &reply).unwrap();
        assert_eq!(views(r), vec![("V1".to_string(), b"d2".to_vec())]);
        // Only on Sheet2: that sheet.
        let r = resolve_picks(&[model_pick("edge:2", Kind::Edge, b"e2")], &reply).unwrap();
        assert_eq!(views(r), vec![("V2".to_string(), b"d3".to_vec())]);
        // Across sheets, or nowhere: refused.
        let both = [
            model_pick("edge:1", Kind::Edge, b"e1"),
            model_pick("edge:2", Kind::Edge, b"e2"),
        ];
        assert!(
            resolve_picks(&both, &reply)
                .unwrap_err()
                .message
                .contains("Sheet2")
        );
        let hidden = resolve_picks(&[model_pick("edge:9", Kind::Edge, b"e9")], &reply).unwrap_err();
        assert!(hidden.message.contains("isn't drawn"), "{}", hidden.message);
        // Another model's ids don't match the same bytes.
        let mut other = model_pick("edge:1", Kind::Edge, b"e1");
        if let DrawingPick::ModelId { key, .. } = &mut other {
            *key = crate::files::file_key("c:\\d\\other.sldprt");
        }
        assert!(resolve_picks(&[other], &reply).is_err());
    }

    #[test]
    fn drawing_picks_take_other_documents_ids_and_names() {
        let mut ids = IdRegistry::default();
        let key = crate::files::file_key("C:\\d\\Block.SLDPRT");
        ids.set_document(&key, "Block.SLDPRT");
        let edge = ids.id_for(Kind::Edge, b"e1").unwrap();
        ids.set_document("file:c:\\d\\draw.slddrw", "Draw.SLDDRW");
        let params = SelectionParams {
            select: vec![edge.clone(), "Boss-Extrude1".into()],
            ..Default::default()
        };
        let picks = drawing_picks(&ids, &params).unwrap();
        assert_eq!(
            picks[0],
            DrawingPick::ModelId {
                id: edge,
                key,
                kind: Kind::Edge,
                persist_id: b"e1".to_vec()
            }
        );
        assert_eq!(picks[1], DrawingPick::FeatureName("Boss-Extrude1".into()));
        for refused in ["top", "origin", "body:1", "face:99"] {
            let params = SelectionParams {
                select: vec![refused.into()],
                ..Default::default()
            };
            assert!(drawing_picks(&ids, &params).is_err(), "{refused}");
        }
    }

    #[test]
    fn drawing_features_get_their_models_ids() {
        let mut ids = IdRegistry::default();
        ids.set_document("file:c:\\d\\draw.slddrw", "Draw.SLDDRW");
        let reply = proto::SelectionResponse {
            items: vec![proto::SelectedItem {
                view_name: "V1".into(),
                item: Some(proto::selected_item::Item::Feature(proto::FeatureInfo {
                    persist_id: b"boss".to_vec(),
                    name: "Boss-Extrude1".into(),
                    ..Default::default()
                })),
                ..Default::default()
            }],
            views: vec![proto::ViewInfo {
                name: "V1".into(),
                model_file: "C:\\d\\Block.SLDPRT".into(),
                ..Default::default()
            }],
            document: drawing_document(),
            ..Default::default()
        };
        let v = render(&mut ids, &mut drawing::Cache::default(), &reply);
        let f = &v["selected"][0];
        assert_eq!(
            (f["name"].as_str(), f["model"].as_str(), f["view"].as_str()),
            (Some("Boss-Extrude1"), Some("Block.SLDPRT"), Some("V1")),
            "{v}"
        );
        let id = f["id"].as_str().unwrap();
        // The model's id: unknown in the drawing, known once the model is active.
        assert!(ids.persist_id(id).is_none());
        ids.set_document(
            &crate::files::file_key("C:\\d\\Block.SLDPRT"),
            "Block.SLDPRT",
        );
        assert_eq!(ids.persist_id(id), Some(b"boss".to_vec()));
    }

    #[test]
    fn empty_selection_says_so() {
        let mut ids = IdRegistry::default();
        let v = render(
            &mut ids,
            &mut drawing::Cache::default(),
            &proto::SelectionResponse::default(),
        );
        assert!(v["note"].as_str().unwrap().contains("nothing is selected"));
        assert_eq!(v["selected"], json!([]));
    }

    #[test]
    fn faces_get_ids_and_pick_points() {
        let mut ids = IdRegistry::default();
        let reply = proto::SelectionResponse {
            items: vec![proto::SelectedItem {
                point: Some(proto::Vec3 {
                    x: 0.01,
                    y: 0.02,
                    z: 0.0,
                }),
                item: Some(proto::selected_item::Item::Face(proto::FaceInfo {
                    persist_id: vec![1, 2, 3],
                    feature_name: "Boss-Extrude1".into(),
                    ..Default::default()
                })),
                ..Default::default()
            }],
            ..Default::default()
        };
        let v = render(&mut ids, &mut drawing::Cache::default(), &reply);
        let face = &v["selected"][0];
        let id = face["id"].as_str().unwrap();
        assert!(id.starts_with("face:"));
        assert_eq!(ids.persist_id(id), Some(vec![1, 2, 3]));
        assert_eq!(face["feature"], json!("Boss-Extrude1"));
        assert_eq!(face["picked_at_mm"], json!([10.0, 20.0, 0.0]));
        // Selected through the API: SolidWorks says [0, 0, 0], left out.
        let mut api = reply.clone();
        api.items[0].point = Some(proto::Vec3::default());
        let v = render(&mut ids, &mut drawing::Cache::default(), &api);
        assert!(v["selected"][0].get("picked_at_mm").is_none(), "{v}");
    }
}
