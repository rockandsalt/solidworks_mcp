//! The drawing tools: inspect_drawing, add_drawing_dimensions and
//! insert_model_items.
//!
//! The add-in reports each view's visible edges in the model's own space
//! plus where the model's origin and axes land on the sheet; projecting
//! them onto the sheet, telling what each edge looks like there (a circle
//! seen face-on or edge-on, a line seen end-on), minting ids and placing
//! dimension text all happen here.
//!
//! Sheet positions are millimeters from the sheet's lower-left corner;
//! sizes (lengths, diameters) are the model's true sizes, as a dimension
//! would show them.

use std::collections::HashMap;

use glam::{DVec2, DVec3};
use prost::Message as _;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::DimensionKind;
use crate::addin::proto;
use crate::assembly::Placement;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, curve_name, mm, round};
use crate::pmi;

pub const DEFAULT_ENTITY_LIMIT: usize = 300;
/// The add-in's limit per add_drawing_dimensions call.
pub const MAX_DIMENSIONS: usize = 200;

/// Gap between a view's outline (or the geometry) and dimension text placed
/// next to it, and between stacked dimensions: sheet meters.
const TEXT_GAP: f64 = 0.010;
const STACK_STEP: f64 = 0.007;
/// How far off axis a circle's axis may be and still count as seen face-on
/// (or edge-on): about 0.06 degrees.
const AXIS_TOLERANCE: f64 = 1e-3;
/// A straight edge shorter than this on the sheet is seen end-on.
const END_ON_LENGTH: f64 = 1e-6;

fn dvec3(v: &proto::Vec3) -> DVec3 {
    DVec3::new(v.x, v.y, v.z)
}

/// Sheet meters -> [x, y] in mm, to 0.01 mm.
pub fn sheet_mm(p: DVec2) -> Value {
    json!([round(p.x * MM_PER_M, 2), round(p.y * MM_PER_M, 2)])
}

/// How a view maps model space onto the sheet.
#[derive(Debug, Clone, Copy)]
pub struct ViewXform {
    origin: DVec3,
    axes: [DVec3; 3],
}

impl ViewXform {
    pub fn of(view: &proto::ViewInfo) -> Option<ViewXform> {
        Some(ViewXform {
            origin: dvec3(view.model_origin.as_ref()?),
            axes: [
                dvec3(view.model_x.as_ref()?),
                dvec3(view.model_y.as_ref()?),
                dvec3(view.model_z.as_ref()?),
            ],
        })
    }

    fn apply(&self, p: DVec3) -> DVec3 {
        self.origin + p.x * self.axes[0] + p.y * self.axes[1] + p.z * self.axes[2]
    }

    /// Where a model point lands on the sheet.
    pub fn point(&self, p: DVec3) -> DVec2 {
        self.apply(p).truncate()
    }

    /// A model direction on the sheet (x, y) and toward the viewer (z),
    /// scaled by the view's scale.
    fn vector(&self, v: DVec3) -> DVec3 {
        v.x * self.axes[0] + v.y * self.axes[1] + v.z * self.axes[2]
    }

    /// Sheet length per model length.
    pub fn scale(&self) -> f64 {
        self.axes[0].length()
    }

    /// The model direction pointing out of the sheet at the viewer.
    pub fn toward_viewer_in_model(&self) -> DVec3 {
        DVec3::new(self.axes[0].z, self.axes[1].z, self.axes[2].z)
    }

    /// A model direction as it runs on the sheet.
    pub fn direction_on_sheet(&self, v: DVec3) -> DVec2 {
        self.vector(v).truncate()
    }
}

/// What an entity looks like on the sheet, in sheet meters.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A straight edge or silhouette, or a circle/arc seen edge-on.
    Line { a: DVec2, b: DVec2 },
    /// A full circle seen face-on; radius on the sheet.
    Circle { center: DVec2, radius: f64 },
    /// An arc seen face-on.
    Arc { center: DVec2, radius: f64, a: DVec2, b: DVec2 },
    /// A vertex, or a straight edge seen end-on.
    Point { p: DVec2 },
    /// Anything else (a circle seen at an angle, a spline, ...).
    Curve { a: DVec2, b: DVec2, center: Option<DVec2> },
}

impl Shape {
    /// One representative point: a line's middle, a circle's center, ...
    fn anchor(&self) -> DVec2 {
        match self {
            Shape::Line { a, b } => (*a + *b) / 2.0,
            Shape::Circle { center, .. } | Shape::Arc { center, .. } => *center,
            Shape::Point { p } => *p,
            Shape::Curve { a, b, center } => center.unwrap_or((*a + *b) / 2.0),
        }
    }

    fn is_straight(&self) -> bool {
        matches!(self, Shape::Line { .. })
    }

    fn is_round(&self) -> bool {
        matches!(self, Shape::Circle { .. } | Shape::Arc { .. })
    }
}

/// What add_drawing_dimensions needs to know about an id handed out by
/// inspect_drawing.
#[derive(Debug, Clone)]
pub struct EntityGeom {
    pub shape: Shape,
    /// The type word inspect_drawing showed, e.g. "circle_seen_edge_on".
    pub label: String,
    /// The view it's in.
    pub view: String,
}

/// Sheet geometry from the latest inspect_drawing calls, for checking and
/// placing dimensions.
#[derive(Debug, Default)]
pub struct Cache {
    entities: HashMap<String, EntityGeom>,
    /// View outlines (min, max), sheet meters.
    outlines: HashMap<String, (DVec2, DVec2)>,
}

impl Cache {
    /// A sheet was set up again: `views` are its views as they are now.
    /// With `moved` (a new scale or projection) the entities' sheet
    /// geometry is stale, so it's forgotten until the next inspect_drawing.
    pub fn sheet_changed(&mut self, views: &[proto::ViewInfo], moved: bool) {
        for view in views {
            if let Some(b) = &view.outline
                && let (Some(lo), Some(hi)) = (&b.min, &b.max)
            {
                self.outlines.insert(view.name.clone(), (DVec2::new(lo.x, lo.y), DVec2::new(hi.x, hi.y)));
            }
        }
        if moved {
            self.entities.clear();
        }
    }

    /// Views were moved, turned, rescaled or renamed: their entities' sheet
    /// geometry is stale until the next inspect_drawing.
    pub fn views_changed(&mut self, names: &[String]) {
        self.entities.retain(|_, g| !names.contains(&g.view));
        for name in names {
            self.outlines.remove(name);
        }
    }

    /// The sheet geometry inspect_drawing gave for a view entity id.
    pub fn entity(&self, id: &str) -> Option<&EntityGeom> {
        self.entities.get(id.trim())
    }

    /// A point to go through for `id`: a circle's center, a line's middle, ...
    pub fn entity_anchor(&self, id: &str) -> Option<DVec2> {
        self.entity(id).map(|g| g.shape.anchor())
    }

    /// The other entities of `view` inspect_drawing gave geometry for.
    pub fn others_in<'a>(&'a self, view: &'a str, id: &'a str) -> impl Iterator<Item = &'a EntityGeom> + 'a {
        self.entities.iter().filter(move |(k, g)| g.view == view && k.as_str() != id.trim()).map(|(_, g)| g)
    }

    /// A view's outline (min, max), sheet meters.
    pub fn outline(&self, view: &str) -> Option<(DVec2, DVec2)> {
        self.outlines.get(view).copied()
    }
}

// ---- Ids --------------------------------------------------------------------

fn kind_for(kind: proto::ViewEntityKind) -> Kind {
    match kind {
        proto::ViewEntityKind::Vertex => Kind::ViewVertex,
        proto::ViewEntityKind::Silhouette => Kind::Silhouette,
        _ => Kind::ViewEdge,
    }
}

fn entity_ref(view: &str, e: &proto::ViewEntity) -> proto::DrawingEntityRef {
    proto::DrawingEntityRef {
        view_name: view.to_string(),
        kind: e.kind,
        persist_id: e.persist_id.clone(),
        component_persist_id: e.component_persist_id.clone(),
        ordinal: e.ordinal,
    }
}

/// The id for an entity of a view.
pub fn id_for(ids: &mut IdRegistry, r: &proto::DrawingEntityRef) -> Option<String> {
    ids.id_for(kind_for(r.kind()), &r.encode_to_vec())
}

/// The ref behind a view entity id, or a message saying what's wrong.
pub fn decode_ref(ids: &IdRegistry, id: &str) -> Result<proto::DrawingEntityRef, McpError> {
    let id = id.trim();
    match Kind::of_id(id) {
        Some(Kind::ViewEdge | Kind::ViewVertex | Kind::Silhouette) => {}
        Some(_) => {
            return Err(McpError::invalid_params(
                format!(
                    "{id} isn't an entity of a drawing view -- give ids from inspect_drawing with a \
                     view (\"view_edge:3\", \"silhouette:1\", \"view_vertex:2\")"
                ),
                None,
            ));
        }
        None => {
            return Err(McpError::invalid_params(
                format!("{id:?} isn't an id -- give view entity ids like \"view_edge:3\" from inspect_drawing"),
                None,
            ));
        }
    }
    ids.persist_id(id)
        .and_then(|bytes| proto::DrawingEntityRef::decode(bytes.as_slice()).ok())
        .ok_or_else(|| {
            McpError::invalid_params(
                ids.unknown_id(id, "inspect_drawing", "call inspect_drawing with the view for current ones"),
                None,
            )
        })
}

// ---- Projection -------------------------------------------------------------

/// Component placements of an assembly view (only those whose geometry
/// needs one).
fn placements(view: &proto::ViewInfo) -> HashMap<Vec<u8>, Placement> {
    view.components
        .iter()
        .filter_map(|c| Some((c.persist_id.clone(), Placement::of(c)?)))
        .collect()
}

/// Model-space (of the view's model) points and directions of one entity.
struct Mapper<'a> {
    xform: ViewXform,
    placement: Option<&'a Placement>,
}

impl Mapper<'_> {
    fn point(&self, p: &proto::Vec3) -> DVec2 {
        let p = dvec3(p);
        self.xform.point(self.placement.map_or(p, |pl| pl.point(p)))
    }

    fn vector(&self, v: &proto::Vec3) -> DVec3 {
        let v = dvec3(v);
        self.xform.vector(self.placement.map_or(v, |pl| pl.vector(v)))
    }
}

/// An edge's look on the sheet and the JSON describing it.
fn edge_shape(m: &Mapper, edge: &proto::EdgeInfo) -> Option<(Shape, Map<String, Value>)> {
    let mut obj = Map::new();
    let (start, end) = (m.point(edge.start.as_ref()?), m.point(edge.end.as_ref()?));
    let curve = edge.curve_type();
    let scale = m.xform.scale();
    if curve == proto::CurveType::Circle
        && let (Some(center), Some(axis)) = (edge.center.as_ref(), edge.axis.as_ref())
    {
        let full = (start - end).length() < END_ON_LENGTH;
        let c = m.point(center);
        let a = m.vector(axis);
        let across = a.truncate().length() / scale.max(f64::MIN_POSITIVE);
        let along = a.z.abs() / scale.max(f64::MIN_POSITIVE);
        let size = |obj: &mut Map<String, Value>| {
            if full {
                obj.insert("diameter_mm".into(), json!(mm(2.0 * edge.radius)));
            } else {
                obj.insert("radius_mm".into(), json!(mm(edge.radius)));
            }
        };
        let word = if full { "circle" } else { "arc" };
        let shape = if across < AXIS_TOLERANCE {
            // Seen face-on: a true circle on the sheet.
            obj.insert("type".into(), json!(word));
            obj.insert("center".into(), sheet_mm(c));
            if !full {
                obj.insert("start".into(), sheet_mm(start));
                obj.insert("end".into(), sheet_mm(end));
            }
            size(&mut obj);
            let radius = edge.radius * scale;
            if full {
                Shape::Circle { center: c, radius }
            } else {
                Shape::Arc { center: c, radius, a: start, b: end }
            }
        } else if along < AXIS_TOLERANCE {
            // Seen edge-on: a straight line across the circle's diameter
            // (for arcs, between its ends).
            obj.insert("type".into(), json!(format!("{word}_seen_edge_on")));
            let (a_end, b_end) = if full {
                let d = a.truncate().perp().normalize() * edge.radius * scale;
                (c - d, c + d)
            } else {
                (start, end)
            };
            obj.insert("start".into(), sheet_mm(a_end));
            obj.insert("end".into(), sheet_mm(b_end));
            size(&mut obj);
            Shape::Line { a: a_end, b: b_end }
        } else {
            obj.insert("type".into(), json!(format!("{word}_seen_at_angle")));
            obj.insert("center".into(), sheet_mm(c));
            size(&mut obj);
            Shape::Curve { a: start, b: end, center: Some(c) }
        };
        return Some((shape, obj));
    }
    if curve == proto::CurveType::Line {
        if (end - start).length() < END_ON_LENGTH {
            obj.insert("type".into(), json!("line_seen_end_on"));
            obj.insert("at".into(), sheet_mm(start));
            obj.insert("length_mm".into(), json!(mm(edge.length)));
            return Some((Shape::Point { p: start }, obj));
        }
        obj.insert("type".into(), json!("line"));
        obj.insert("start".into(), sheet_mm(start));
        obj.insert("end".into(), sheet_mm(end));
        obj.insert("length_mm".into(), json!(mm(edge.length)));
        return Some((Shape::Line { a: start, b: end }, obj));
    }
    obj.insert("type".into(), json!(curve_name(curve)));
    obj.insert("start".into(), sheet_mm(start));
    obj.insert("end".into(), sheet_mm(end));
    if edge.length > 0.0 {
        obj.insert("length_mm".into(), json!(mm(edge.length)));
    }
    Some((Shape::Curve { a: start, b: end, center: None }, obj))
}

fn entity_shape(m: &Mapper, e: &proto::ViewEntity) -> Option<(Shape, Map<String, Value>)> {
    match e.kind() {
        proto::ViewEntityKind::Edge => edge_shape(m, e.edge.as_ref()?),
        proto::ViewEntityKind::Vertex => {
            let p = m.point(e.start.as_ref()?);
            let mut obj = Map::new();
            obj.insert("type".into(), json!("vertex"));
            obj.insert("at".into(), sheet_mm(p));
            Some((Shape::Point { p }, obj))
        }
        proto::ViewEntityKind::Silhouette => {
            let (a, b) = (m.point(e.start.as_ref()?), m.point(e.end.as_ref()?));
            let mut obj = Map::new();
            obj.insert("type".into(), json!("silhouette"));
            obj.insert("start".into(), sheet_mm(a));
            obj.insert("end".into(), sheet_mm(b));
            let scale = m.xform.scale();
            if scale > 0.0 {
                obj.insert("length_mm".into(), json!(mm((b - a).length() / scale)));
            }
            Some((Shape::Line { a, b }, obj))
        }
        proto::ViewEntityKind::Unspecified => None,
    }
}

// ---- Names ------------------------------------------------------------------

fn view_type_name(t: i32) -> &'static str {
    match t {
        1 => "sheet",
        2 => "section",
        3 => "detail",
        4 => "projected",
        5 => "auxiliary",
        6 => "standard",
        7 => "named",
        8 => "relative",
        9 => "detached",
        10 => "alternate_position",
        _ => "other",
    }
}

pub fn paper_name(size: i32) -> Option<&'static str> {
    Some(match size {
        0 => "A (letter) landscape",
        1 => "A (letter) portrait",
        2 => "B",
        3 => "C",
        4 => "D",
        5 => "E",
        6 => "A4 landscape",
        7 => "A4 portrait",
        8 => "A3",
        9 => "A2",
        10 => "A1",
        11 => "A0",
        12 => "user defined",
        _ => return None,
    })
}

pub fn display_mode_name(mode: i32) -> Option<&'static str> {
    Some(match mode {
        0 => "wireframe",
        1 => "hidden_lines_visible",
        2 => "hidden_lines_removed",
        3 => "shaded",
        4..=6 => "draft_quality",
        7 => "shaded_with_edges",
        _ => return None,
    })
}

/// A number without needless decimals: 2, 1.5, 0.333.
fn short_number(x: f64) -> String {
    let r = round(x, 3);
    if r.fract() == 0.0 { format!("{}", r as i64) } else { format!("{r}") }
}

pub fn scale_text(numerator: f64, denominator: f64) -> Option<String> {
    (numerator > 0.0 && denominator > 0.0)
        .then(|| format!("{}:{}", short_number(numerator), short_number(denominator)))
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Which model axis points along a sheet direction ("+X", "-Z", ...), if
/// one does.
fn axis_along(xform: &ViewXform, sheet_dir: DVec3) -> Option<String> {
    let scale = xform.scale();
    if scale <= 0.0 {
        return None;
    }
    for (name, axis) in ["X", "Y", "Z"].iter().zip(xform.axes) {
        let c = axis.dot(sheet_dir) / scale;
        if c > 1.0 - AXIS_TOLERANCE {
            return Some(format!("+{name}"));
        }
        if c < -(1.0 - AXIS_TOLERANCE) {
            return Some(format!("-{name}"));
        }
    }
    None
}

/// "+Y points right, +Z up and +X toward the viewer", or for a view that
/// isn't axis-aligned, where the model's axes point on the sheet.
pub fn axes_text(view: &proto::ViewInfo) -> Option<String> {
    let x = ViewXform::of(view)?;
    let named: Vec<Option<String>> = [DVec3::X, DVec3::Y, DVec3::Z].iter().map(|d| axis_along(&x, *d)).collect();
    if let [Some(right), Some(up), Some(toward)] = named.as_slice() {
        return Some(format!("{right} points right, {up} up and {toward} toward the viewer"));
    }
    let scale = x.scale().max(f64::MIN_POSITIVE);
    let axis = |v: DVec3| format!("[{}, {}]", round(v.x / scale, 3), round(v.y / scale, 3));
    Some(format!(
        "the model's X, Y, Z axes point along {}, {}, {} on the sheet",
        axis(x.axes[0]),
        axis(x.axes[1]),
        axis(x.axes[2])
    ))
}

pub fn annotation_counts(types: &[i32]) -> Value {
    let mut counts = Map::new();
    for t in types {
        let key = pmi::type_name(*t);
        let n = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(key.into(), json!(n + 1));
    }
    Value::Object(counts)
}

/// A drawing annotation: what inspect_annotations shows, plus where it is on
/// the sheet and the view entities it's attached to.
pub fn annotation_json(ids: &mut IdRegistry, view: Option<&str>, a: &proto::AnnotationInfo) -> Value {
    let mut obj = pmi::annotation_fields(a);
    // Dimensions in a drawing are driven by the model (or are reference
    // dimensions); saying so on each is noise.
    obj.remove("driven");
    if let Some(owner) = owner_text(a) {
        obj.insert("in".into(), json!(owner));
    }
    let mut attached = Vec::new();
    let mut others = 0;
    for e in &a.attached {
        let kind = match e.kind() {
            proto::AttachedKind::Edge => proto::ViewEntityKind::Edge,
            proto::AttachedKind::Vertex => proto::ViewEntityKind::Vertex,
            _ => {
                others += 1;
                continue;
            }
        };
        let id = view.filter(|_| !e.persist_id.is_empty()).and_then(|view| {
            let r = proto::DrawingEntityRef {
                view_name: view.to_string(),
                kind: kind as i32,
                persist_id: e.persist_id.clone(),
                component_persist_id: e.component_persist_id.clone(),
                ordinal: 0,
            };
            id_for(ids, &r)
        });
        match id {
            Some(id) => attached.push(Value::String(id)),
            None => others += 1,
        }
    }
    if !attached.is_empty() {
        obj.insert("attached".into(), Value::Array(attached));
    }
    if others > 0 {
        obj.insert("attached_other".into(), json!(others));
    }
    if let Some(p) = &a.position {
        obj.insert("position".into(), sheet_mm(DVec2::new(p.x, p.y)));
    }
    Value::Object(obj)
}

/// Where a drawing annotation belongs: "view Drawing View1", "sheet",
/// "sheet_format"; None outside drawings.
pub fn owner_text(a: &proto::AnnotationInfo) -> Option<String> {
    match a.owner() {
        proto::AnnotationOwner::View => Some(format!("view {}", a.owner_name)),
        proto::AnnotationOwner::Sheet => Some("sheet".to_string()),
        proto::AnnotationOwner::SheetFormat => Some("sheet_format".to_string()),
        proto::AnnotationOwner::Unspecified => None,
    }
}

// ---- inspect_drawing ----------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectDrawingParams {
    /// A view to look at closely, e.g. "Drawing View1": lists its visible edges (with ids
    /// for add_drawing_dimensions and where they are on the sheet) and its dimensions and
    /// other annotations. Without it, only the sheets and views are listed.
    #[serde(default)]
    pub view: Option<String>,
    /// With `view`: also list its visible vertices (for point-to-point dimensions).
    #[serde(default)]
    pub include_vertices: bool,
    /// Without `view`: list the annotations (dimensions, notes, ...) of every view, not just
    /// how many there are.
    #[serde(default)]
    pub list_annotations: bool,
    /// With `view`: how many entities to list. Defaults to 300.
    #[serde(default)]
    pub limit: Option<usize>,
    /// With `view`: skip this many entities first, for paging. Defaults to 0.
    #[serde(default)]
    pub offset: Option<usize>,
}

pub fn inspect_request(params: &InspectDrawingParams) -> proto::InspectDrawingRequest {
    let view = params.view.as_deref().map(str::trim).filter(|v| !v.is_empty());
    proto::InspectDrawingRequest {
        entity_views: view.map(|v| vec![v.to_string()]).unwrap_or_default(),
        include_vertices: params.include_vertices,
        include_annotations: view.is_some() || params.list_annotations,
        // Listed apart from the sheet's own only by inspect_drawing_annotations.
        skip_sheet_format: true,
        include_tables: false,
    }
}

fn view_json(
    ids: &mut IdRegistry,
    cache: &mut Cache,
    view: &proto::ViewInfo,
    params: &InspectDrawingParams,
) -> Value {
    let mut obj = Map::new();
    obj.insert("name".into(), json!(view.name));
    obj.insert("type".into(), json!(view_type_name(view.r#type)));
    if !view.orientation.is_empty() {
        obj.insert("orientation".into(), json!(view.orientation));
    }
    if !view.base_view.is_empty() {
        obj.insert("from_view".into(), json!(view.base_view));
    }
    if !view.label.is_empty() {
        obj.insert("label".into(), json!(view.label));
    }
    if view.cropped {
        obj.insert("cropped".into(), json!(true));
    }
    if !view.breaks.is_empty() {
        obj.insert("breaks".into(), crate::derived_view::breaks_json(view));
    }
    if view.broken {
        obj.insert("broken_note".into(), json!(crate::derived_view::BROKEN_NOTE));
    }
    if !view.model_file.is_empty() {
        obj.insert("model".into(), json!(file_name(&view.model_file)));
    }
    if !view.configuration.is_empty() {
        obj.insert("configuration".into(), json!(view.configuration));
    }
    if let Some(scale) = scale_text(view.scale_numerator, view.scale_denominator) {
        obj.insert("scale".into(), json!(scale));
    }
    if view.uses_parent_scale {
        obj.insert("scale_from".into(), json!("parent"));
    } else if view.uses_sheet_scale {
        obj.insert("scale_from".into(), json!("sheet"));
    }
    if let Some(b) = &view.outline
        && let (Some(lo), Some(hi)) = (&b.min, &b.max)
    {
        let (lo, hi) = (DVec2::new(lo.x, lo.y), DVec2::new(hi.x, hi.y));
        obj.insert("outline".into(), json!([sheet_mm(lo), sheet_mm(hi)]));
        cache.outlines.insert(view.name.clone(), (lo, hi));
    }
    let xform = ViewXform::of(view);
    if let Some(x) = &xform {
        let dirs = [
            ("sheet_right", DVec3::X),
            ("sheet_up", DVec3::Y),
            ("toward_viewer", DVec3::Z),
        ];
        let named: Vec<(&str, Option<String>)> = dirs.iter().map(|(k, d)| (*k, axis_along(x, *d))).collect();
        if named.iter().all(|(_, v)| v.is_some()) {
            for (k, v) in named {
                obj.insert(k.into(), json!(v));
            }
        } else {
            // Not an axis-aligned view (isometric, auxiliary, ...): how the
            // model's axes appear on the sheet.
            let scale = x.scale().max(f64::MIN_POSITIVE);
            let axis = |v: DVec3| json!([round(v.x / scale, 3), round(v.y / scale, 3)]);
            obj.insert(
                "model_axes_on_sheet".into(),
                json!({ "x": axis(x.axes[0]), "y": axis(x.axes[1]), "z": axis(x.axes[2]) }),
            );
        }
    }
    if let Some(mode) = display_mode_name(view.display_mode) {
        obj.insert("display".into(), json!(mode));
    }
    if view.uses_parent_display {
        obj.insert("display_from".into(), json!("parent"));
    }
    if !view.model_loaded && view.model_type() == proto::DocumentType::Unspecified {
        obj.insert(
            "note".into(),
            json!("the model isn't loaded (lightweight or detached view): its edges can't be listed or dimensioned"),
        );
    }
    obj.insert("annotation_counts".into(), annotation_counts(&view.annotation_types));

    if view.entities_listed {
        let placements = placements(view);
        let mut entities = Vec::new();
        if let Some(xform) = xform {
            for e in &view.entities {
                let mapper = Mapper { xform, placement: placements.get(&e.component_persist_id) };
                let Some((shape, mut fields)) = entity_shape(&mapper, e) else { continue };
                let Some(id) = id_for(ids, &entity_ref(&view.name, e)) else { continue };
                let label = fields.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                let mut entry = Map::new();
                entry.insert("id".into(), json!(id));
                entry.append(&mut fields);
                // The same edge's inspect_model id, when the part has been
                // inspected: ties the drawing back to the 3D model.
                if e.kind() == proto::ViewEntityKind::Edge
                    && let Some(model_id) = ids.existing_id(&e.persist_id)
                {
                    entry.insert("model_edge".into(), json!(model_id));
                }
                if let Some(c) = ids.id_for(Kind::Component, &e.component_persist_id) {
                    entry.insert("component".into(), json!(c));
                }
                cache.entities.insert(id, EntityGeom { shape, label, view: view.name.clone() });
                entities.push(Value::Object(entry));
            }
        }
        let total = entities.len();
        let offset = params.offset.unwrap_or(0);
        let limit = params.limit.unwrap_or(DEFAULT_ENTITY_LIMIT);
        let shown: Vec<Value> = entities.into_iter().skip(offset).take(limit).collect();
        let end = offset + shown.len();
        if end < total {
            obj.insert(
                "entities_note".into(),
                json!(format!(
                    "showing entities {}-{} of {total}; pass offset={end} for more",
                    offset + 1,
                    end
                )),
            );
        }
        if total == 0 {
            obj.insert(
                "entities_note".into(),
                json!("SolidWorks reported no visible edges for this view (is its model loaded and its sheet open?)"),
            );
        }
        obj.insert("entities".into(), Value::Array(shown));
    }
    if !view.annotations.is_empty() {
        let list: Vec<Value> = view
            .annotations
            .iter()
            .map(|a| annotation_json(ids, Some(&view.name), a))
            .collect();
        obj.insert("annotations".into(), Value::Array(list));
    }
    if !view.components.is_empty() && view.entities_listed {
        let components = view
            .components
            .iter()
            .map(|c| crate::assembly::component_json(ids, c))
            .collect();
        obj.insert("component_list".into(), Value::Array(components));
    }
    Value::Object(obj)
}

/// The inspect_drawing JSON; remembers the views' sheet geometry in `cache`.
pub fn render_inspect(
    ids: &mut IdRegistry,
    cache: &mut Cache,
    reply: &proto::InspectDrawingResponse,
    params: &InspectDrawingParams,
) -> Value {
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }
    out.insert(
        "units".into(),
        json!(
            "positions (start, end, center, at, outline, position) are sheet coordinates in mm \
             from the sheet's lower-left corner; sizes (length_mm, diameter_mm, radius_mm, \
             dimension values) are the model's true sizes"
        ),
    );
    let detail = params.view.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let mut sheets = Vec::new();
    let mut view_count = 0;
    for sheet in &reply.sheets {
        let mut obj = Map::new();
        obj.insert("name".into(), json!(sheet.name));
        if sheet.active {
            obj.insert("active".into(), json!(true));
        }
        if let Some(paper) = paper_name(sheet.paper_size) {
            obj.insert("paper".into(), json!(paper));
        }
        if sheet.width > 0.0 && sheet.height > 0.0 {
            obj.insert("size_mm".into(), json!([mm(sheet.width), mm(sheet.height)]));
        }
        if let Some(scale) = scale_text(sheet.scale_numerator, sheet.scale_denominator) {
            obj.insert("scale".into(), json!(scale));
        }
        obj.insert(
            "projection".into(),
            json!(if sheet.first_angle { "first_angle" } else { "third_angle" }),
        );
        if !sheet.sheet_format.is_empty() {
            obj.insert("sheet_format".into(), json!(sheet.sheet_format));
        }
        let views: Vec<Value> = sheet
            .views
            .iter()
            .map(|v| {
                let mut v = v.clone();
                // Annotations only for the view asked about (or all, with
                // list_annotations).
                if detail.is_some_and(|d| d != v.name) {
                    v.annotations.clear();
                }
                view_json(ids, cache, &v, params)
            })
            .collect();
        view_count += views.len();
        obj.insert("views".into(), Value::Array(views));
        // The sheet's own list holds its sheet format's (title block)
        // annotations too; the add-in counts them apart.
        if !sheet.annotation_types.is_empty() {
            obj.insert("sheet_annotation_counts".into(), annotation_counts(&sheet.annotation_types));
        }
        if !sheet.sheet_format_annotation_types.is_empty() {
            obj.insert(
                "sheet_format_annotation_counts".into(),
                annotation_counts(&sheet.sheet_format_annotation_types),
            );
        }
        let own: Vec<&proto::AnnotationInfo> =
            sheet.annotations.iter().filter(|a| a.owner() != proto::AnnotationOwner::SheetFormat).collect();
        if params.list_annotations && !own.is_empty() {
            let list: Vec<Value> = own.iter().map(|a| annotation_json(ids, None, a)).collect();
            obj.insert("sheet_annotations".into(), Value::Array(list));
        }
        sheets.push(Value::Object(obj));
    }
    out.insert("sheets".into(), Value::Array(sheets));

    let mut notes = Vec::new();
    if view_count == 0 {
        notes.push("the drawing has no views yet -- add views of a part or assembly in SolidWorks first".to_string());
    } else if detail.is_none() {
        notes.push(
            "pass view=\"<name>\" to list a view's edges (ids for add_drawing_dimensions) and its \
             dimensions; insert_model_items brings in the model's own dimensions"
                .to_string(),
        );
    }
    if reply.truncated {
        notes.push("the drawing has more entities or annotations than the add-in reports at once".to_string());
    }
    if !notes.is_empty() {
        out.insert("note".into(), json!(notes.join("; ")));
    }
    Value::Object(out)
}

// ---- add_drawing_dimensions -------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DrawingDimensionSpec {
    /// length (1 straight edge), distance (2 entities: between parallel edges, point to edge,
    /// or from a circle's center), horizontal / vertical (1 edge's extent, or the distance
    /// between 2 entities, along the sheet's x / y), diameter / radius (1 circle or arc seen
    /// face-on), angle (2 straight edges).
    pub kind: DimensionKind,
    /// What it measures: entity ids of one view from inspect_drawing ("view_edge:3",
    /// "silhouette:1", "view_vertex:2").
    pub entities: Vec<String>,
    /// Where the dimension text goes: [x, y] on the sheet, mm from its lower-left corner.
    /// Defaults to a spot outside the view on the side nearer the measured edges (above or
    /// below for horizontal, left or right for vertical), next to the circle for diameter /
    /// radius, inside the corner for angles.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddDrawingDimensionsParams {
    /// Dimensions to add, applied together (all or none).
    pub dimensions: Vec<DrawingDimensionSpec>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// Where a line meets another (infinite lines); None if parallel.
fn intersection(a1: DVec2, b1: DVec2, a2: DVec2, b2: DVec2) -> Option<DVec2> {
    let (d1, d2) = (b1 - a1, b2 - a2);
    let denom = d1.perp_dot(d2);
    if denom.abs() < 1e-12 * d1.length() * d2.length() {
        return None;
    }
    let t = (a2 - a1).perp_dot(d2) / denom;
    Some(a1 + d1 * t)
}

/// How far a ray from `from` along unit `dir` travels before leaving the
/// box (0 if it starts outside).
pub fn exit_distance(from: DVec2, dir: DVec2, lo: DVec2, hi: DVec2) -> f64 {
    let inside = from.cmpge(lo).all() && from.cmple(hi).all();
    if !inside {
        return 0.0;
    }
    let mut t = f64::INFINITY;
    for (f, d, l, h) in [(from.x, dir.x, lo.x, hi.x), (from.y, dir.y, lo.y, hi.y)] {
        if d > 1e-12 {
            t = t.min((h - f) / d);
        } else if d < -1e-12 {
            t = t.min((l - f) / d);
        }
    }
    if t.is_finite() { t.max(0.0) } else { 0.0 }
}

/// Text stacked outside a view: how many dimensions this call already put
/// on each side of each view.
#[derive(Debug, Default)]
pub struct Stacking {
    count: HashMap<(String, Side), usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    Above,
    Below,
    Left,
    Right,
}

impl Stacking {
    /// How many dimensions are already on that side; counts this one.
    fn next(&mut self, view: &str, side: Side) -> f64 {
        let k = self.count.entry((view.to_string(), side)).or_default();
        *k += 1;
        (*k - 1) as f64
    }
}

/// Default text position for a dimension, sheet meters.
fn place(
    kind: DimensionKind,
    shapes: &[&Shape],
    outline: Option<(DVec2, DVec2)>,
    view: &str,
    stacking: &mut Stacking,
) -> Result<DVec2, String> {
    let anchors: Vec<DVec2> = shapes.iter().map(|s| s.anchor()).collect();
    let mean = anchors.iter().copied().sum::<DVec2>() / anchors.len() as f64;
    // Away from the view's middle.
    let center = outline.map_or(mean, |(lo, hi)| (lo + hi) / 2.0);
    let away = |n: DVec2, from: DVec2| if n.dot(from - center) < 0.0 { -n } else { n };
    let points: Vec<DVec2> = shapes
        .iter()
        .flat_map(|s| match s {
            Shape::Line { a, b } => vec![*a, *b],
            Shape::Circle { center, radius } => {
                vec![*center + DVec2::splat(*radius), *center - DVec2::splat(*radius)]
            }
            other => vec![other.anchor()],
        })
        .collect();
    let max = points.iter().copied().fold(DVec2::NEG_INFINITY, DVec2::max);
    let min = points.iter().copied().fold(DVec2::INFINITY, DVec2::min);
    match kind {
        // Outside the view, on the side nearer to what's measured.
        DimensionKind::Horizontal => {
            if mean.y >= center.y {
                let top = outline.map_or(max.y, |(_, hi)| hi.y.max(max.y));
                let y = top + TEXT_GAP + stacking.next(view, Side::Above) * STACK_STEP;
                Ok(DVec2::new(mean.x, y))
            } else {
                let bottom = outline.map_or(min.y, |(lo, _)| lo.y.min(min.y));
                let y = bottom - TEXT_GAP - stacking.next(view, Side::Below) * STACK_STEP;
                Ok(DVec2::new(mean.x, y))
            }
        }
        DimensionKind::Vertical => {
            if mean.x >= center.x {
                let right = outline.map_or(max.x, |(_, hi)| hi.x.max(max.x));
                let x = right + TEXT_GAP + stacking.next(view, Side::Right) * STACK_STEP;
                Ok(DVec2::new(x, mean.y))
            } else {
                let left = outline.map_or(min.x, |(lo, _)| lo.x.min(min.x));
                let x = left - TEXT_GAP - stacking.next(view, Side::Left) * STACK_STEP;
                Ok(DVec2::new(x, mean.y))
            }
        }
        DimensionKind::Diameter | DimensionKind::Radius => match shapes[0] {
            Shape::Circle { center, radius } | Shape::Arc { center, radius, .. } => {
                let dir = away(DVec2::new(1.0, 1.0).normalize(), *center + DVec2::new(1e-9, 1e-9));
                Ok(*center + dir * (radius + TEXT_GAP))
            }
            _ => Err("needs a circle or arc seen face-on".into()),
        },
        DimensionKind::Angle => {
            let (Shape::Line { a: a1, b: b1 }, Shape::Line { a: a2, b: b2 }) = (shapes[0], shapes[1]) else {
                return Err("needs two straight edges".into());
            };
            let corner = intersection(*a1, *b1, *a2, *b2).ok_or("the edges are parallel")?;
            let (m1, m2) = ((*a1 + *b1) / 2.0, (*a2 + *b2) / 2.0);
            let bisector = ((m1 - corner).normalize_or_zero() + (m2 - corner).normalize_or_zero()).normalize_or_zero();
            let reach = (m1 - corner).length().min((m2 - corner).length());
            Ok(corner + bisector * (0.6 * reach).max(1.5 * TEXT_GAP))
        }
        DimensionKind::Length | DimensionKind::Distance => {
            let (from, dir) = match shapes {
                [Shape::Line { a, b }] => ((*a + *b) / 2.0, *b - *a),
                [_, _] if (anchors[1] - anchors[0]).length() > 1e-9 => (mean, anchors[1] - anchors[0]),
                // Two entities around the same point (e.g. concentric):
                // fall back to the first one's direction.
                [Shape::Line { a, b }, _] => (mean, *b - *a),
                _ => (mean, DVec2::X),
            };
            let n = away(dir.perp().normalize_or(DVec2::Y), from);
            // Out of the view, beside the geometry, so the text doesn't sit
            // on the part's lines.
            let inside = outline.map_or(0.0, |(lo, hi)| exit_distance(from, n, lo, hi));
            Ok(from + n * (inside + TEXT_GAP))
        }
    }
}

fn check_shapes(kind: DimensionKind, shapes: &[Option<&EntityGeom>]) -> Result<(), String> {
    let n = shapes.len();
    let ok_count = match kind {
        DimensionKind::Length | DimensionKind::Diameter | DimensionKind::Radius => n == 1,
        DimensionKind::Distance | DimensionKind::Angle => n == 2,
        DimensionKind::Horizontal | DimensionKind::Vertical => n == 1 || n == 2,
    };
    if !ok_count {
        let wanted = match kind {
            DimensionKind::Length | DimensionKind::Diameter | DimensionKind::Radius => "1 entity",
            DimensionKind::Distance | DimensionKind::Angle => "2 entities",
            DimensionKind::Horizontal | DimensionKind::Vertical => "1 or 2 entities",
        };
        return Err(format!("a {} dimension takes {wanted}", kind.name()));
    }
    let known: Vec<&EntityGeom> = shapes.iter().flatten().copied().collect();
    let describe = |g: &EntityGeom| g.label.replace('_', " ");
    match kind {
        DimensionKind::Length | DimensionKind::Angle => {
            if let Some(g) = known.iter().find(|g| !g.shape.is_straight()) {
                return Err(format!(
                    "a {} dimension needs straight edges, but one is a {} here",
                    kind.name(),
                    describe(g)
                ));
            }
        }
        DimensionKind::Diameter | DimensionKind::Radius => {
            if let Some(g) = known.first()
                && !g.shape.is_round()
            {
                let hint = if g.label.contains("edge_on") {
                    " (in this view it's seen edge-on: dimension it in a view that looks along its \
                     axis, or measure the distance between the two silhouette lines)"
                } else {
                    ""
                };
                return Err(format!(
                    "a {} dimension needs a circle or arc seen face-on, but that's a {}{hint}",
                    kind.name(),
                    describe(g)
                ));
            }
        }
        DimensionKind::Horizontal | DimensionKind::Vertical if n == 1 => {
            if let Some(g) = known.first()
                && matches!(g.shape, Shape::Point { .. })
            {
                return Err(format!(
                    "a single {} has no extent; give two entities",
                    describe(g)
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Builds the AddDrawingDimensions request (and a label per dimension for
/// the reply).
pub fn dimensions_request(
    ids: &IdRegistry,
    cache: &Cache,
    params: &AddDrawingDimensionsParams,
) -> Result<(proto::AddDrawingDimensionsRequest, Vec<String>), McpError> {
    let n = params.dimensions.len();
    if n == 0 || n > MAX_DIMENSIONS {
        return Err(invalid(format!("give between 1 and {MAX_DIMENSIONS} dimensions")));
    }
    let mut stacking = Stacking::default();
    let mut dimensions = Vec::with_capacity(n);
    let mut labels = Vec::with_capacity(n);
    for (i, spec) in params.dimensions.iter().enumerate() {
        let at = |message: String| invalid(format!("dimension {}: {message}", i + 1));
        let refs = spec
            .entities
            .iter()
            .map(|e| decode_ref(ids, e))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| at(e.message.to_string()))?;
        if refs.is_empty() {
            return Err(at("give the entities it measures".into()));
        }
        let view = refs[0].view_name.clone();
        if let Some(other) = refs.iter().find(|r| r.view_name != view) {
            return Err(at(format!(
                "all its entities must be in one view, but they're in {view} and {}",
                other.view_name
            )));
        }
        let trimmed: Vec<&str> = spec.entities.iter().map(|e| e.trim()).collect();
        if trimmed.len() == 2 && trimmed[0] == trimmed[1] {
            return Err(at("give two different entities".into()));
        }
        let geoms: Vec<Option<&EntityGeom>> = trimmed.iter().map(|e| cache.entities.get(*e)).collect();
        check_shapes(spec.kind, &geoms).map_err(at)?;

        let text = match spec.at_mm {
            Some([x, y]) if x.is_finite() && y.is_finite() => DVec2::new(x, y) / MM_PER_M,
            Some(_) => return Err(at("at_mm must be two finite numbers".into())),
            None => {
                let shapes: Option<Vec<&Shape>> = geoms.iter().map(|g| g.map(|g| &g.shape)).collect();
                let Some(shapes) = shapes else {
                    return Err(at(format!(
                        "no sheet position known for its entities -- call inspect_drawing with \
                         view=\"{view}\" first, or give at_mm"
                    )));
                };
                place(spec.kind, &shapes, cache.outlines.get(&view).copied(), &view, &mut stacking)
                    .map_err(|e| at(format!("can't place a {} dimension: {e}", spec.kind.name())))?
            }
        };
        dimensions.push(proto::DrawingDimension {
            kind: proto::DimensionKind::from(spec.kind).into(),
            refs,
            text_x: text.x,
            text_y: text.y,
        });
        labels.push(format!("{} of {}", spec.kind.name(), trimmed.join(" & ")));
    }
    Ok((proto::AddDrawingDimensionsRequest { dimensions }, labels))
}

/// "RD1@Drawing View1 = 50 mm" for a dimension annotation.
fn dimension_value_text(a: &proto::AnnotationInfo) -> String {
    let name = if a.name.is_empty() { "dimension" } else { a.name.as_str() };
    let Some(info) = a.dimension.as_ref().and_then(|d| d.dimension.as_ref()) else {
        return name.to_string();
    };
    let name = if info.name.is_empty() { name } else { info.name.as_str() };
    let angular = info.r#type() == proto::DimensionType::Angular;
    let value = pmi::value_of(info.value, angular);
    let unit = if angular { "deg" } else { "mm" };
    let symbol = match info.r#type() {
        proto::DimensionType::Diameter => "dia ",
        proto::DimensionType::Radius => "R",
        _ => "",
    };
    format!("{name} = {symbol}{value} {unit}")
}

pub fn dimensions_reply_text(reply: &proto::AddDrawingDimensionsResponse, labels: &[String]) -> String {
    let lines: Vec<String> = reply
        .dimensions
        .iter()
        .zip(labels)
        .map(|(d, label)| {
            let a = d.annotation.clone().unwrap_or_default();
            let mut line = format!("{} ({label}) in {}", dimension_value_text(&a), d.view_name);
            if let Some(p) = &a.position {
                line += &format!(", text at [{}, {}] mm", round(p.x * MM_PER_M, 1), round(p.y * MM_PER_M, 1));
            }
            line
        })
        .collect();
    format!(
        "added {} dimension(s):\n  {}\nThey measure the model (reference dimensions; they don't change it). \
         Move one by giving at_mm, or check the layout with inspect_drawing.",
        lines.len(),
        lines.join("\n  ")
    )
}

// ---- insert_model_items ---------------------------------------------------------

/// What insert_model_items brings in from the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelItem {
    /// The model's dimensions marked for drawing (sketch and feature dimensions; the default).
    Dimensions,
    /// Also the dimensions not marked for drawing.
    UnmarkedDimensions,
    /// Dimensions locating Hole Wizard holes.
    HoleWizardLocations,
    /// Hole Wizard holes' own size dimensions.
    HoleWizardProfiles,
    /// Hole callouts.
    HoleCallouts,
    /// Datum feature symbols.
    Datums,
    /// Datum targets.
    DatumTargets,
    /// Geometric tolerances (feature control frames).
    Gtols,
    /// Surface finish symbols.
    SurfaceFinish,
    /// Notes.
    Notes,
    /// Cosmetic threads.
    CosmeticThreads,
}

impl ModelItem {
    /// swInsertAnnotation_e.
    fn flag(self) -> i32 {
        match self {
            ModelItem::Dimensions => 32768,
            ModelItem::UnmarkedDimensions => 524288,
            ModelItem::HoleWizardLocations => 131072,
            ModelItem::HoleWizardProfiles => 65536,
            ModelItem::HoleCallouts => 1048576,
            ModelItem::Datums => 2,
            ModelItem::DatumTargets => 4,
            ModelItem::Gtols => 32,
            ModelItem::SurfaceFinish => 128,
            ModelItem::Notes => 64,
            ModelItem::CosmeticThreads => 1,
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InsertModelItemsParams {
    /// Views to insert into, all on one sheet, e.g. ["Drawing View1"]. Defaults to every
    /// view of the active sheet.
    #[serde(default)]
    pub views: Option<Vec<String>>,
    /// What to bring in. Defaults to ["dimensions"] (those marked for drawing, as SolidWorks'
    /// Model Items does).
    #[serde(default)]
    pub items: Option<Vec<ModelItem>>,
    /// Also insert dimensions already shown in another view.
    #[serde(default)]
    pub include_duplicates: bool,
    /// Also insert dimensions of features hidden in the view.
    #[serde(default)]
    pub include_hidden_features: bool,
}

pub fn model_items_request(params: &InsertModelItemsParams) -> proto::InsertModelItemsRequest {
    let items = params.items.clone().unwrap_or_else(|| vec![ModelItem::Dimensions]);
    proto::InsertModelItemsRequest {
        view_names: params
            .views
            .iter()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect(),
        types: items.iter().fold(0, |acc, item| acc | item.flag()),
        include_duplicates: params.include_duplicates,
        include_hidden_features: params.include_hidden_features,
    }
}

pub fn render_model_items(ids: &mut IdRegistry, reply: &proto::InsertModelItemsResponse) -> Value {
    let mut out = Map::new();
    out.insert("inserted".into(), json!(reply.inserted));
    let mut listed = 0;
    let views: Vec<Value> = reply
        .views
        .iter()
        .map(|v| {
            listed += v.annotations.len();
            let added: Vec<Value> = v
                .annotations
                .iter()
                .map(|a| annotation_json(ids, Some(&v.view_name), a))
                .collect();
            json!({ "view": v.view_name, "added": added })
        })
        .collect();
    out.insert("views".into(), Value::Array(views));
    let note = if reply.inserted == 0 && listed == 0 {
        "nothing was inserted: the model has no such items for these views (dimensions only come \
         in if the model's sketches/features have them and they're marked for drawing; try \
         items=[\"dimensions\", \"unmarked_dimensions\"]), or they're already shown in another \
         view (include_duplicates)"
    } else {
        "positions are sheet mm; use inspect_drawing with a view to see its edges and add more \
         dimensions with add_drawing_dimensions"
    };
    out.insert("note".into(), json!(note));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> Option<proto::Vec3> {
        Some(proto::Vec3 { x, y, z })
    }

    /// A front view at 1:2 with the model origin at (0.1, 0.1) on the sheet.
    fn front_view() -> proto::ViewInfo {
        proto::ViewInfo {
            name: "Drawing View1".into(),
            r#type: 6,
            orientation: "*Front".into(),
            model_file: r"C:\parts\bracket.SLDPRT".into(),
            model_type: proto::DocumentType::Part as i32,
            model_loaded: true,
            scale_numerator: 1.0,
            scale_denominator: 2.0,
            outline: Some(proto::Box3 { min: v(0.08, 0.08, 0.0), max: v(0.15, 0.13, 0.0) }),
            model_origin: v(0.1, 0.1, 0.0),
            model_x: v(0.5, 0.0, 0.0),
            model_y: v(0.0, 0.5, 0.0),
            model_z: v(0.0, 0.0, 0.5),
            display_mode: 2,
            ..Default::default()
        }
    }

    fn edge(pid: &[u8], curve: proto::CurveType, start: [f64; 3], end: [f64; 3]) -> proto::ViewEntity {
        proto::ViewEntity {
            kind: proto::ViewEntityKind::Edge as i32,
            persist_id: pid.to_vec(),
            edge: Some(proto::EdgeInfo {
                persist_id: pid.to_vec(),
                curve_type: curve as i32,
                start: v(start[0], start[1], start[2]),
                end: v(end[0], end[1], end[2]),
                length: 0.1,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn circle(pid: &[u8], center: [f64; 3], axis: [f64; 3], radius: f64) -> proto::ViewEntity {
        let mut e = edge(pid, proto::CurveType::Circle, [center[0] + radius, center[1], center[2]], [
            center[0] + radius,
            center[1],
            center[2],
        ]);
        let info = e.edge.as_mut().unwrap();
        info.center = v(center[0], center[1], center[2]);
        info.axis = v(axis[0], axis[1], axis[2]);
        info.radius = radius;
        e
    }

    fn reply_with(entities: Vec<proto::ViewEntity>) -> proto::InspectDrawingResponse {
        let mut view = front_view();
        view.entities_listed = true;
        view.entities = entities;
        view.annotation_types = vec![4, 4, 6];
        proto::InspectDrawingResponse {
            document: Some(proto::Document { title: "bracket - Sheet1".into(), r#type: 3, path: String::new(), ..Default::default() }),
            sheets: vec![proto::SheetInfo {
                name: "Sheet1".into(),
                active: true,
                paper_size: 8,
                width: 0.42,
                height: 0.297,
                scale_numerator: 1.0,
                scale_denominator: 2.0,
                first_angle: true,
                views: vec![view],
                ..Default::default()
            }],
            truncated: false,
        }
    }

    fn params(view: Option<&str>) -> InspectDrawingParams {
        InspectDrawingParams { view: view.map(Into::into), ..Default::default() }
    }

    #[test]
    fn projects_edges_onto_the_sheet() {
        let mut ids = IdRegistry::default();
        let mut cache = Cache::default();
        let reply = reply_with(vec![
            edge(b"top", proto::CurveType::Line, [0.0, 0.05, 0.0], [0.1, 0.05, 0.0]),
            edge(b"depth", proto::CurveType::Line, [0.0, 0.0, 0.0], [0.0, 0.0, 0.02]),
            circle(b"hole", [0.05, 0.025, 0.0], [0.0, 0.0, 1.0], 0.005),
            circle(b"side", [0.05, 0.025, 0.0], [1.0, 0.0, 0.0], 0.005),
        ]);
        let out = render_inspect(&mut ids, &mut cache, &reply, &params(Some("Drawing View1")));
        let sheet = &out["sheets"][0];
        assert_eq!(sheet["paper"], "A3");
        assert_eq!(sheet["size_mm"], json!([420.0, 297.0]));
        assert_eq!(sheet["projection"], "first_angle");
        let view = &sheet["views"][0];
        assert_eq!(view["scale"], "1:2");
        assert_eq!(view["model"], "bracket.SLDPRT");
        assert_eq!(view["sheet_right"], "+X");
        assert_eq!(view["sheet_up"], "+Y");
        assert_eq!(view["toward_viewer"], "+Z");
        assert_eq!(view["annotation_counts"], json!({ "dimension": 2, "note": 1 }));
        let e = view["entities"].as_array().unwrap();
        assert_eq!(e.len(), 4);
        assert_eq!(e[0]["id"], "view_edge:1");
        assert_eq!(e[0]["type"], "line");
        // Model (0, 50) -> sheet 100 + 25 = 125 mm up; 1:2.
        assert_eq!(e[0]["start"], json!([100.0, 125.0]));
        assert_eq!(e[0]["end"], json!([150.0, 125.0]));
        assert_eq!(e[0]["length_mm"], 100.0);
        assert_eq!(e[1]["type"], "line_seen_end_on");
        assert_eq!(e[2]["type"], "circle");
        assert_eq!(e[2]["center"], json!([125.0, 112.5]));
        assert_eq!(e[2]["diameter_mm"], 10.0);
        assert_eq!(e[3]["type"], "circle_seen_edge_on");
        assert_eq!(e[3]["start"], json!([125.0, 110.0]));
        assert_eq!(e[3]["end"], json!([125.0, 115.0]));
        assert!(matches!(cache.entities["view_edge:3"].shape, Shape::Circle { radius, .. } if (radius - 0.0025).abs() < 1e-12));

        // Ids are stable and decode back to the view's ref.
        let again = render_inspect(&mut ids, &mut cache, &reply, &params(Some("Drawing View1")));
        assert_eq!(again["sheets"][0]["views"][0]["entities"][0]["id"], "view_edge:1");
        let r = decode_ref(&ids, "view_edge:3").unwrap();
        assert_eq!(r.view_name, "Drawing View1");
        assert_eq!(r.persist_id, b"hole");
        assert!(decode_ref(&ids, "face:1").is_err());
        assert!(decode_ref(&ids, "view_edge:99").is_err());
    }

    #[test]
    fn overview_has_no_entities_and_says_how_to_get_them() {
        let mut ids = IdRegistry::default();
        let mut cache = Cache::default();
        let mut reply = reply_with(vec![]);
        reply.sheets[0].views[0].entities_listed = false;
        let out = render_inspect(&mut ids, &mut cache, &reply, &params(None));
        assert!(out["sheets"][0]["views"][0].get("entities").is_none());
        assert!(out["note"].as_str().unwrap().contains("view="));
        assert!(cache.outlines.contains_key("Drawing View1"));
    }

    #[test]
    fn isometric_views_report_axes_instead() {
        let mut view = front_view();
        let s = 0.5;
        view.model_x = v(s * 0.707, -s * 0.408, s * 0.577);
        view.model_y = v(0.0, s * 0.816, s * 0.577);
        view.model_z = v(-s * 0.707, -s * 0.408, s * 0.577);
        let mut ids = IdRegistry::default();
        let mut cache = Cache::default();
        let out = view_json(&mut ids, &mut cache, &view, &params(None));
        assert!(out.get("sheet_right").is_none());
        assert_eq!(out["model_axes_on_sheet"]["y"], json!([0.0, 0.816]));
    }

    #[test]
    fn assembly_components_are_placed_first() {
        let mut view = front_view();
        // A component moved 100 mm along X, its geometry in its own space.
        view.components = vec![proto::ComponentInfo {
            persist_id: b"comp".to_vec(),
            origin: v(0.1, 0.0, 0.0),
            x_axis: v(1.0, 0.0, 0.0),
            y_axis: v(0.0, 1.0, 0.0),
            z_axis: v(0.0, 0.0, 1.0),
            local_geometry: true,
            ..Default::default()
        }];
        let mut e = edge(b"e", proto::CurveType::Line, [0.0, 0.0, 0.0], [0.0, 0.01, 0.0]);
        e.component_persist_id = b"comp".to_vec();
        view.entities = vec![e];
        view.entities_listed = true;
        let mut ids = IdRegistry::default();
        let mut cache = Cache::default();
        let out = view_json(&mut ids, &mut cache, &view, &params(Some("Drawing View1")));
        assert_eq!(out["entities"][0]["start"], json!([150.0, 100.0]));
        assert_eq!(out["entities"][0]["component"], "component:1");
    }

    fn inspected() -> (IdRegistry, Cache) {
        let mut ids = IdRegistry::default();
        let mut cache = Cache::default();
        let reply = reply_with(vec![
            edge(b"top", proto::CurveType::Line, [0.0, 0.05, 0.0], [0.1, 0.05, 0.0]),
            edge(b"left", proto::CurveType::Line, [0.0, 0.0, 0.0], [0.0, 0.05, 0.0]),
            circle(b"hole", [0.05, 0.025, 0.0], [0.0, 0.0, 1.0], 0.005),
            circle(b"side", [0.05, 0.025, 0.0], [1.0, 0.0, 0.0], 0.005),
            edge(b"bottom", proto::CurveType::Line, [0.0, 0.0, 0.0], [0.1, 0.0, 0.0]),
        ]);
        render_inspect(&mut ids, &mut cache, &reply, &params(Some("Drawing View1")));
        (ids, cache)
    }

    fn spec(kind: DimensionKind, entities: &[&str], at_mm: Option<[f64; 2]>) -> DrawingDimensionSpec {
        DrawingDimensionSpec { kind, entities: entities.iter().map(|e| e.to_string()).collect(), at_mm }
    }

    #[test]
    fn places_dimensions_outside_the_view() {
        let (ids, cache) = inspected();
        let params = AddDrawingDimensionsParams {
            dimensions: vec![
                spec(DimensionKind::Horizontal, &["view_edge:1"], None),
                spec(DimensionKind::Horizontal, &["view_edge:2", "view_edge:3"], None),
                spec(DimensionKind::Vertical, &["view_edge:2"], None),
                spec(DimensionKind::Diameter, &["view_edge:3"], None),
                spec(DimensionKind::Distance, &["view_edge:1", "view_edge:5"], Some([200.0, 50.0])),
                spec(DimensionKind::Angle, &["view_edge:1", "view_edge:2"], None),
            ],
        };
        let (request, labels) = dimensions_request(&ids, &cache, &params).unwrap();
        let d = &request.dimensions;
        assert_eq!(d.len(), 6);
        assert_eq!(labels[0], "horizontal of view_edge:1");
        assert_eq!(d[0].refs[0].view_name, "Drawing View1");
        assert_eq!(d[0].kind, proto::DimensionKind::Horizontal as i32);
        // Above the outline (top 130 mm), stacked.
        assert!((d[0].text_y - 0.140).abs() < 1e-9 && (d[0].text_x - 0.125).abs() < 1e-9);
        assert!((d[1].text_y - 0.147).abs() < 1e-9);
        // The left edge's height: left of the outline (80 mm).
        assert!((d[2].text_x - 0.070).abs() < 1e-9);
        // Diameter: off the circle at 45 degrees, beyond its radius.
        assert!(d[3].text_x > 0.125 + 0.0025 && d[3].text_y > 0.1125 + 0.0025);
        assert!((d[4].text_x - 0.2).abs() < 1e-12 && (d[4].text_y - 0.05).abs() < 1e-12);
        // Angle text inside the corner between the two lines.
        assert!(d[5].text_x > 0.1 && d[5].text_y < 0.125);

        // Between two parallel edges: beside them, outside the view (right
        // edge 150 mm).
        let params = AddDrawingDimensionsParams {
            dimensions: vec![spec(DimensionKind::Distance, &["view_edge:1", "view_edge:5"], None)],
        };
        let (request, _) = dimensions_request(&ids, &cache, &params).unwrap();
        let d = &request.dimensions[0];
        assert!((d.text_x - 0.160).abs() < 1e-9 && (d.text_y - 0.1125).abs() < 1e-9);

        // The bottom edge's width goes below the view (bottom 80 mm).
        let params = AddDrawingDimensionsParams {
            dimensions: vec![spec(DimensionKind::Horizontal, &["view_edge:5"], None)],
        };
        let (request, _) = dimensions_request(&ids, &cache, &params).unwrap();
        assert!((request.dimensions[0].text_y - 0.070).abs() < 1e-9);
    }

    #[test]
    fn rejects_entities_that_dont_suit_the_kind() {
        let (ids, cache) = inspected();
        let one = |s| AddDrawingDimensionsParams { dimensions: vec![s] };
        let err = |s| dimensions_request(&ids, &cache, &one(s)).unwrap_err().message.to_string();
        assert!(err(spec(DimensionKind::Diameter, &["view_edge:4"], None)).contains("edge-on"));
        assert!(err(spec(DimensionKind::Diameter, &["view_edge:1"], None)).contains("circle or arc"));
        assert!(err(spec(DimensionKind::Length, &["view_edge:3"], None)).contains("straight"));
        assert!(err(spec(DimensionKind::Distance, &["view_edge:1"], None)).contains("2 entities"));
        assert!(err(spec(DimensionKind::Distance, &["view_edge:1", "view_edge:1"], None)).contains("different"));
        assert!(err(spec(DimensionKind::Angle, &["view_edge:1", "view_edge:5"], None)).contains("parallel"));
        assert!(err(spec(DimensionKind::Length, &["edge:1"], None)).contains("drawing view"));
        assert!(err(spec(DimensionKind::Length, &["top edge"], None)).contains("isn't an id"));
        assert!(err(spec(DimensionKind::Length, &["view_edge:1"], Some([f64::NAN, 0.0]))).contains("finite"));
    }

    #[test]
    fn model_items_flags_and_reply() {
        let request = model_items_request(&InsertModelItemsParams::default());
        assert_eq!(request.types, 32768);
        assert!(request.view_names.is_empty());
        let request = model_items_request(&InsertModelItemsParams {
            views: Some(vec![" Drawing View1 ".into(), "".into()]),
            items: Some(vec![ModelItem::Dimensions, ModelItem::HoleCallouts]),
            ..Default::default()
        });
        assert_eq!(request.types, 32768 | 1048576);
        assert_eq!(request.view_names, vec!["Drawing View1".to_string()]);

        let mut ids = IdRegistry::default();
        let reply = proto::InsertModelItemsResponse {
            inserted: 1,
            views: vec![proto::ViewAnnotations {
                view_name: "Drawing View1".into(),
                annotations: vec![proto::AnnotationInfo {
                    name: "D1@Sketch1".into(),
                    r#type: 4,
                    position: v(0.12, 0.14, 0.0),
                    attached: vec![proto::AttachedEntity {
                        kind: proto::AttachedKind::Edge as i32,
                        persist_id: b"top".to_vec(),
                        component_persist_id: vec![],
                    }],
                    dimension: Some(proto::AnnotationDimension {
                        dimension: Some(proto::DimensionInfo {
                            name: "D1@Sketch1@bracket.Part".into(),
                            r#type: proto::DimensionType::Linear as i32,
                            value: 0.1,
                            driven: true,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                components: vec![],
            }],
        };
        let out = render_model_items(&mut ids, &reply);
        let a = &out["views"][0]["added"][0];
        assert_eq!(a["value_mm"], 100.0);
        assert_eq!(a["position"], json!([120.0, 140.0]));
        assert_eq!(a["attached"], json!(["view_edge:1"]));
        assert!(a.get("driven").is_none());
    }

    #[test]
    fn reply_text_names_values() {
        let reply = proto::AddDrawingDimensionsResponse {
            dimensions: vec![proto::DrawingDimensionResult {
                view_name: "Drawing View1".into(),
                annotation: Some(proto::AnnotationInfo {
                    name: "RD1".into(),
                    r#type: 4,
                    position: v(0.125, 0.14, 0.0),
                    dimension: Some(proto::AnnotationDimension {
                        dimension: Some(proto::DimensionInfo {
                            name: "RD1@Drawing View1".into(),
                            r#type: proto::DimensionType::Diameter as i32,
                            value: 0.01,
                            driven: true,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }],
            components: vec![],
        };
        let text = dimensions_reply_text(&reply, &["diameter of view_edge:3".into()]);
        assert!(text.contains("RD1@Drawing View1 = dia 10 mm (diameter of view_edge:3) in Drawing View1"));
        assert!(text.contains("[125, 140]"));
    }
}
