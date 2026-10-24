//! The geometric tolerancing tools: add_geometric_tolerance (a feature
//! control frame) and add_datum_symbol (a datum feature symbol), on a part
//! or assembly (PMI: AddModelAnnotation) or in a drawing
//! (SetupDrawingAnnotation with `gdt`).
//!
//! Characteristics and modifiers take the names inspect_annotations prints,
//! so a frame read can be written back; they're turned into SolidWorks'
//! enum values (swGtolGeomCharSymbol_e, swGtolMatCondition_e) here, and the
//! basic rules of GD&T (form tolerances take no datums, orientation and
//! runout need one, ...) are checked before anything reaches SolidWorks.

use glam::{DVec2, DVec3};
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::Value;

use crate::addin::proto;
use crate::drawing::Cache;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, round};
use crate::note;
use crate::pmi;

/// The longest tolerance text SolidWorks is sent (the add-in's limit).
const MAX_TOLERANCE_TEXT: usize = 64;
/// Arrows further than this from where they were asked to go are reported.
const ARROW_SLACK: f64 = 5e-4;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

fn mm3(p: DVec3) -> String {
    format!("[{}, {}, {}]", round(p.x * MM_PER_M, 2), round(p.y * MM_PER_M, 2), round(p.z * MM_PER_M, 2))
}

// ---- Names ------------------------------------------------------------------------

/// The geometric characteristic a frame controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Characteristic {
    // Form: never with datums.
    Straightness,
    Flatness,
    #[serde(alias = "roundness")]
    Circularity,
    Cylindricity,
    // Profile: datums optional.
    #[serde(alias = "line_profile", alias = "profile_line")]
    ProfileOfALine,
    #[serde(alias = "surface_profile", alias = "profile_surface", alias = "profile")]
    ProfileOfASurface,
    // Orientation: at least one datum.
    Angularity,
    Perpendicularity,
    Parallelism,
    // Location.
    #[serde(alias = "true_position")]
    Position,
    #[serde(alias = "coaxiality")]
    Concentricity,
    Symmetry,
    // Runout: at least one datum.
    #[serde(alias = "runout")]
    CircularRunout,
    TotalRunout,
}

impl Characteristic {
    /// swGtolGeomCharSymbol_e.
    fn sw(self) -> i32 {
        match self {
            Characteristic::Symmetry => 13,
            Characteristic::Straightness => 14,
            Characteristic::Flatness => 15,
            Characteristic::Circularity => 16,
            Characteristic::Cylindricity => 17,
            Characteristic::ProfileOfALine => 18,
            Characteristic::ProfileOfASurface => 19,
            Characteristic::Angularity => 20,
            Characteristic::Perpendicularity => 21,
            Characteristic::Parallelism => 22,
            Characteristic::Position => 23,
            Characteristic::Concentricity => 24,
            Characteristic::CircularRunout => 25,
            Characteristic::TotalRunout => 26,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Characteristic::Straightness => "straightness",
            Characteristic::Flatness => "flatness",
            Characteristic::Circularity => "circularity",
            Characteristic::Cylindricity => "cylindricity",
            Characteristic::ProfileOfALine => "profile_of_a_line",
            Characteristic::ProfileOfASurface => "profile_of_a_surface",
            Characteristic::Angularity => "angularity",
            Characteristic::Perpendicularity => "perpendicularity",
            Characteristic::Parallelism => "parallelism",
            Characteristic::Position => "position",
            Characteristic::Concentricity => "concentricity",
            Characteristic::Symmetry => "symmetry",
            Characteristic::CircularRunout => "circular_runout",
            Characteristic::TotalRunout => "total_runout",
        }
    }

    fn is_form(self) -> bool {
        matches!(
            self,
            Characteristic::Straightness
                | Characteristic::Flatness
                | Characteristic::Circularity
                | Characteristic::Cylindricity
        )
    }

    /// Controls that mean nothing without a datum to relate to.
    fn needs_datum(self) -> bool {
        matches!(
            self,
            Characteristic::Angularity
                | Characteristic::Perpendicularity
                | Characteristic::Parallelism
                | Characteristic::Concentricity
                | Characteristic::Symmetry
                | Characteristic::CircularRunout
                | Characteristic::TotalRunout
        )
    }
}

/// A material condition modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub enum Modifier {
    /// Maximum material condition, (M).
    #[serde(rename = "MMC", alias = "mmc", alias = "M")]
    Mmc,
    /// Least material condition, (L).
    #[serde(rename = "LMC", alias = "lmc", alias = "L")]
    Lmc,
    /// Regardless of feature size, (S) (ASME Y14.5-1982 style; the default without a modifier).
    #[serde(rename = "RFS", alias = "rfs", alias = "S")]
    Rfs,
}

impl Modifier {
    /// swGtolMatCondition_e.
    fn sw(self) -> i32 {
        match self {
            Modifier::Mmc => 1,
            Modifier::Rfs => 2,
            Modifier::Lmc => 3,
        }
    }
}

/// A tolerance value: a number of mm, or the text to show ("0.05", "0.1/25").
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ToleranceValue {
    Mm(f64),
    Text(String),
}

// ---- Params -----------------------------------------------------------------------

/// One frame (row) of a feature control frame.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct FrameParams {
    /// What's controlled: form (straightness, flatness, circularity, cylindricity -- no datums),
    /// profile (profile_of_a_line, profile_of_a_surface), orientation (angularity,
    /// perpendicularity, parallelism -- need a datum), location (position, concentricity,
    /// symmetry), runout (circular_runout, total_runout -- need a datum).
    pub characteristic: Characteristic,
    /// The tolerance zone's size in mm, e.g. 0.05 (or a text such as "0.1/25").
    pub tolerance: ToleranceValue,
    /// A cylindrical zone: shows the diameter sign before the value (position of a hole,
    /// straightness of an axis).
    #[serde(default)]
    pub diameter_zone: bool,
    /// The tolerance's material condition: "MMC" (M), "LMC" (L) or "RFS" (S).
    #[serde(default)]
    pub modifier: Option<Modifier>,
    /// Datum references in order of precedence, at most 3: "A", "B", "C"; a material condition
    /// in parentheses, "B(M)" / "B(L)" / "B(S)"; a common datum as "A-B".
    #[serde(default)]
    pub datums: Vec<String>,
}

/// What the symbol is attached to and where it goes, in a part / assembly or a drawing.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PlaceParams {
    /// What it points at, with a leader: in a part or assembly a face or edge from inspect_model
    /// ("face:7", "edge:3"); in a drawing a view entity from inspect_drawing ("view_edge:4",
    /// "silhouette:1", "view_vertex:2"), and it belongs to that view.
    #[serde(default)]
    pub on: Option<String>,
    /// Drawings, frames only, instead of `on`: a dimension it sits on, by the name
    /// inspect_drawing lists ("RD1@Drawing View1") -- the feature of size the dimension measures
    /// (a hole's diameter). In a part, use the feature's face (a hole's cylindrical face).
    #[serde(default)]
    pub dimension: Option<String>,
    /// Where the leader's arrow touches `on`: a part's [x, y, z] model mm (on the face/edge); a
    /// drawing's [x, y] sheet mm. Default: a face's middle in a part, an edge's middle in a
    /// drawing.
    #[serde(default)]
    pub arrow_at_mm: Option<Vec<f64>>,
    /// Where the symbol goes: a part's [x, y, z] model mm (SolidWorks keeps it in its annotation
    /// view's plane), a drawing's [x, y] sheet mm (the upper-left corner). Default: a frame 10 mm
    /// out from its arrow (in a drawing outside the view), a datum symbol where SolidWorks puts
    /// it (just off its entity).
    #[serde(default)]
    pub at_mm: Option<Vec<f64>>,
    /// Drawings, instead of at_mm: [x, y] in the view's own coordinates (mm at the model's size
    /// from the model origin as the view shows it).
    #[serde(default)]
    pub at_view_mm: Option<[f64; 2]>,
    /// Drawings: the view a free frame (no `on` / `dimension`) goes in; without it, the sheet.
    #[serde(default)]
    pub view: Option<String>,
    /// Drawings: the sheet a free frame goes on (default: the active one).
    #[serde(default)]
    pub sheet: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddGeometricToleranceParams {
    #[serde(flatten)]
    pub frame: FrameParams,
    /// A second frame (row) under the first: with composite=true a composite frame (one
    /// characteristic, the lower row refining the upper, e.g. a pattern's position); otherwise
    /// two single-segment frames stacked (e.g. flatness under a perpendicularity).
    #[serde(default)]
    pub second_frame: Option<FrameParams>,
    /// The two frames form a composite frame (same characteristic: position or profile).
    #[serde(default)]
    pub composite: bool,
    /// A datum feature symbol shown on the frame itself, e.g. "D": the toleranced feature is
    /// also datum D.
    #[serde(default)]
    pub datum_feature: Option<String>,
    /// A projected tolerance zone of this height in mm (shown as (P) and the height).
    #[serde(default)]
    pub projected_zone_mm: Option<f64>,
    #[serde(flatten)]
    pub place: PlaceParams,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AddDatumSymbolParams {
    /// The datum letter, e.g. "A". Default: the next letter SolidWorks picks. Each datum feature
    /// gets its own letter; I, O and Q are best avoided (they look like 1, 0).
    #[serde(default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub place: PlaceParams,
}

// ---- Symbols ----------------------------------------------------------------------

fn tolerance_text(value: &ToleranceValue, what: &str) -> Result<String, McpError> {
    let t = match value {
        ToleranceValue::Mm(v) => {
            if !(v.is_finite() && *v > 0.0) {
                return Err(invalid(format!("{what}: the tolerance must be above 0 mm")));
            }
            // Shortest form that reads back the same: 0.05, not 0.05000000000000000277.
            format!("{}", round(*v, 6))
        }
        ToleranceValue::Text(t) => t.trim().to_string(),
    };
    if t.is_empty() || t.len() > MAX_TOLERANCE_TEXT || t.contains(['<', '>']) {
        return Err(invalid(format!("{what}: give the tolerance in mm, e.g. 0.05")));
    }
    Ok(t)
}

/// "B(M)" -> ("B", MMC). Letters, digits and "-" (common datums) only.
fn datum_ref(text: &str, what: &str) -> Result<(String, i32), McpError> {
    let t = text.trim();
    let (label, condition) = match t.strip_suffix(')').and_then(|r| r.rsplit_once('(')) {
        Some((label, m)) => {
            let condition = match m.trim().to_ascii_uppercase().as_str() {
                "M" | "MMC" => Modifier::Mmc.sw(),
                "L" | "LMC" => Modifier::Lmc.sw(),
                "S" | "RFS" => Modifier::Rfs.sw(),
                other => return Err(invalid(format!("{what}: unknown material condition ({other}); use (M), (L) or (S)"))),
            };
            (label.trim(), condition)
        }
        None => (t, 0),
    };
    if label.is_empty() || label.len() > 16 || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(invalid(format!("{what}: a datum reference is a letter such as \"A\", \"B(M)\" or \"A-B\", not {text:?}")));
    }
    Ok((label.to_string(), condition))
}

fn frame(p: &FrameParams, what: &str) -> Result<proto::GtolFrame, McpError> {
    let c = p.characteristic;
    if p.datums.len() > 3 {
        return Err(invalid(format!("{what}: at most 3 datums (primary, secondary, tertiary)")));
    }
    if c.is_form() && !p.datums.is_empty() {
        return Err(invalid(format!(
            "{what}: {} is a form tolerance and never references datums -- leave datums out",
            c.name()
        )));
    }
    if c.needs_datum() && p.datums.is_empty() {
        return Err(invalid(format!("{what}: {} needs a datum reference (datums: [\"A\"])", c.name())));
    }
    let mut out = proto::GtolFrame {
        symbol: c.sw(),
        tolerance1: tolerance_text(&p.tolerance, what)?,
        tolerance1_diameter: p.diameter_zone,
        tolerance1_condition: p.modifier.map_or(0, Modifier::sw),
        ..Default::default()
    };
    for (i, d) in p.datums.iter().enumerate() {
        let (label, condition) = datum_ref(d, &format!("{what}, datum {}", i + 1))?;
        if out.datums.contains(&label) {
            return Err(invalid(format!("{what}: datum {label} is referenced twice")));
        }
        out.datums.push(label);
        out.datum_conditions.push(condition);
    }
    Ok(out)
}

/// The frame add_geometric_tolerance asks for.
pub fn gtol(params: &AddGeometricToleranceParams) -> Result<proto::GdtSymbol, McpError> {
    let mut g = proto::AnnotationGtol { composite: params.composite, ..Default::default() };
    g.frames.push(frame(&params.frame, "frame")?);
    if let Some(second) = &params.second_frame {
        g.frames.push(frame(second, "second_frame")?);
        if params.composite && second.characteristic != params.frame.characteristic {
            return Err(invalid(
                "a composite frame has one characteristic for both rows: give second_frame the same one (or composite=false \
                 for two separate frames)",
            ));
        }
    }
    if params.composite {
        if params.second_frame.is_none() {
            return Err(invalid("composite needs second_frame: the lower row of the composite frame"));
        }
        if !matches!(params.frame.characteristic, Characteristic::Position | Characteristic::ProfileOfASurface | Characteristic::ProfileOfALine) {
            return Err(invalid("composite frames are for position and profile"));
        }
    }
    if let Some(d) = params.datum_feature.as_deref().map(str::trim) {
        if d.is_empty() || d.len() > 8 || !d.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(invalid("datum_feature: a datum letter such as \"D\""));
        }
        g.datum_label = d.to_string();
    }
    if let Some(h) = params.projected_zone_mm {
        if !(h.is_finite() && h > 0.0) {
            return Err(invalid("projected_zone_mm: the zone's height in mm, above 0"));
        }
        g.projected_zone_height = format!("{}", round(h, 6));
    }
    Ok(proto::GdtSymbol { kind: Some(proto::gdt_symbol::Kind::Gtol(g)) })
}

/// The symbol add_datum_symbol asks for.
pub fn datum(params: &AddDatumSymbolParams) -> Result<proto::GdtSymbol, McpError> {
    let label = text(params.label.as_deref());
    if !label.is_empty() && (label.len() > 8 || !label.chars().all(|c| c.is_ascii_alphanumeric())) {
        return Err(invalid("label: a datum letter such as \"A\""));
    }
    Ok(proto::GdtSymbol { kind: Some(proto::gdt_symbol::Kind::Datum(proto::DatumSymbol { label })) })
}

/// Remarks on a datum letter that is legal but unwise.
fn label_remarks(symbol: &proto::GdtSymbol) -> Vec<String> {
    match &symbol.kind {
        Some(proto::gdt_symbol::Kind::Datum(d)) if ["I", "O", "Q"].contains(&d.label.as_str()) => {
            vec![format!("datum letter {} is easily misread (ASME Y14.5 leaves out I, O and Q)", d.label)]
        }
        _ => Vec::new(),
    }
}

/// The callout of a frame row as asked, for the reply text.
fn callout_of(symbol: &proto::GdtSymbol) -> String {
    match &symbol.kind {
        Some(proto::gdt_symbol::Kind::Gtol(g)) => g.frames.iter().map(pmi::frame_callout).collect::<Vec<_>>().join(" / "),
        Some(proto::gdt_symbol::Kind::Datum(d)) => format!("datum {}", if d.label.is_empty() { "?" } else { &d.label }),
        None => String::new(),
    }
}

// ---- Parts and assemblies -----------------------------------------------------------

fn model_point(v: &[f64], what: &str) -> Result<proto::Vec3, McpError> {
    match v {
        [x, y, z] if x.is_finite() && y.is_finite() && z.is_finite() => {
            Ok(proto::Vec3 { x: x / MM_PER_M, y: y / MM_PER_M, z: z / MM_PER_M })
        }
        [_, _] => Err(invalid(format!(
            "{what}: in a part or assembly give [x, y, z] model mm ([x, y] sheet mm is for drawings)"
        ))),
        _ => Err(invalid(format!("{what}: give [x, y, z] in mm"))),
    }
}

/// The AddModelAnnotation request.
pub fn model_request(
    ids: &IdRegistry,
    symbol: proto::GdtSymbol,
    place: &PlaceParams,
) -> Result<(proto::AddModelAnnotationRequest, Vec<String>), McpError> {
    if place.view.is_some() || place.sheet.is_some() || place.at_view_mm.is_some() {
        return Err(invalid("view, sheet and at_view_mm are for drawings; the active document is a part or assembly"));
    }
    let remarks = label_remarks(&symbol);
    let mut request = proto::AddModelAnnotationRequest { symbol: Some(symbol), ..Default::default() };
    let on = text(place.on.as_deref());
    if !text(place.dimension.as_deref()).is_empty() {
        // SolidWorks makes it, with nothing attached and nowhere to show
        // (seen 2026-10-24).
        return Err(invalid(
            "in a part or assembly a frame or datum symbol can't go on a dimension (SolidWorks doesn't place it there              through its API): give on = the feature's face, e.g. a hole's cylindrical face for its position or datum              axis",
        ));
    }
    if !on.is_empty() {
        match Kind::of_id(&on) {
            Some(Kind::Face | Kind::Edge) => {}
            Some(Kind::ViewEdge | Kind::ViewVertex | Kind::Silhouette) => {
                return Err(invalid(format!("{on} is a drawing view's entity; the active document is a part or assembly")));
            }
            _ => return Err(invalid(format!("on: a face or edge id from inspect_model (\"face:7\"), not {on:?}"))),
        }
        request.attach_to = ids
            .persist_id(&on)
            .ok_or_else(|| invalid(ids.unknown_id(&on, "inspect_model", "call inspect_model for current ones")))?;
    }
    if let Some(a) = &place.arrow_at_mm {
        if on.is_empty() {
            return Err(invalid("arrow_at_mm goes with on: the face or edge the leader points at"));
        }
        request.arrow_at = Some(model_point(a, "arrow_at_mm")?);
    }
    if let Some(a) = &place.at_mm {
        request.position = Some(model_point(a, "at_mm")?);
    }
    Ok((request, remarks))
}

fn vec3(v: &proto::Vec3) -> DVec3 {
    DVec3::new(v.x, v.y, v.z)
}

/// The reply to the tools on a part or assembly.
pub fn model_reply(
    ids: &mut IdRegistry,
    request: &proto::AddModelAnnotationRequest,
    reply: &proto::AddModelAnnotationResponse,
    remarks: &[String],
) -> String {
    let Some(a) = &reply.annotation else { return "done".to_string() };
    let json = pmi::annotation_json(ids, a);
    let mut text = format!("added {} {}", pmi::type_name(a.r#type), a.name);
    match (&a.gtol, a.datum_label.trim()) {
        (Some(g), _) if !g.frames.is_empty() => {
            text += &format!(" ({})", g.frames.iter().map(pmi::frame_callout).collect::<Vec<_>>().join(" / "));
        }
        (_, label) if !label.is_empty() => text += &format!(" (datum {label})"),
        _ => text += &format!(" ({})", request.symbol.as_ref().map(callout_of).unwrap_or_default()),
    }
    if let Some(Value::Array(on)) = json.get("attached") {
        let on: Vec<&str> = on.iter().filter_map(Value::as_str).collect();
        if !on.is_empty() {
            text += &format!(" on {}", on.join(", "));
        }
    }
    // Positions and leader points are in the annotation view's plane (the
    // arrow's face / edge point projected into it), so they're compared
    // across that plane only.
    let normal = reply.annotation_view_normal.as_ref().map(vec3).filter(|n| n.length() > 0.5).map(DVec3::normalize);
    let in_plane = |v: DVec3| normal.map_or(v, |n| v - n * v.dot(n));
    let tip = a.leaders.first().and_then(|l| l.points.last()).map(vec3);
    let mut notes = remarks.to_vec();
    match (request.arrow_at.as_ref().map(vec3), tip) {
        (Some(asked), Some(t)) if in_plane(asked - t).length() <= ARROW_SLACK => {
            text += &format!(", arrow at {} mm", mm3(asked));
        }
        (Some(asked), Some(t)) => {
            text += &format!(", arrow drawn at {} mm", mm3(t));
            notes.push(format!("the arrow isn't where asked ({} mm): SolidWorks picked the entity elsewhere", mm3(asked)));
        }
        (None, Some(t)) => text += &format!(", arrow drawn at {} mm", mm3(t)),
        _ => {}
    }
    if let Some(p) = &a.position {
        text += &format!(", text at {} mm", mm3(vec3(p)));
    }
    if !reply.annotation_view.is_empty() {
        text += &format!(
            "; in annotation view {}: drawn in that view's plane (points are where they show in it)",
            reply.annotation_view
        );
    }
    text += ".";
    if let (Some(asked), Some(p)) = (&request.position, &a.position) {
        let (asked, p) = (vec3(asked), vec3(p));
        if in_plane(asked - p).length() > ARROW_SLACK {
            notes.push(format!("the text is at {} mm, not {} mm as asked", mm3(p), mm3(asked)));
        } else if asked.distance(p) > ARROW_SLACK {
            notes.push(format!(
                "the text is at {} mm: SolidWorks keeps it in {}'s plane, so only its depth differs from {} mm",
                mm3(p),
                reply.annotation_view,
                mm3(asked)
            ));
        }
    }
    if !notes.is_empty() {
        text += &format!("\nNote: {}.", notes.join("; "));
    }
    text += &format!("\n{json}");
    text
}

// ---- Drawings ---------------------------------------------------------------------

/// The SetupDrawingAnnotation request for a frame or datum symbol in the
/// active drawing.
pub fn drawing_request(
    ids: &IdRegistry,
    cache: &Cache,
    symbol: proto::GdtSymbol,
    place: &PlaceParams,
) -> Result<(proto::SetupDrawingAnnotationRequest, Vec<String>), McpError> {
    let on = text(place.on.as_deref());
    let dimension = text(place.dimension.as_deref());
    let view = text(place.view.as_deref());
    if !on.is_empty() && !dimension.is_empty() {
        return Err(invalid("give on (a view entity) or dimension, not both"));
    }
    if !on.is_empty() && matches!(Kind::of_id(&on), Some(Kind::Face | Kind::Edge | Kind::Vertex)) {
        return Err(invalid(format!(
            "{on} is a model id; in a drawing give a view entity from inspect_drawing with the view (\"view_edge:4\")"
        )));
    }
    let is_datum = matches!(symbol.kind, Some(proto::gdt_symbol::Kind::Datum(_)));
    if is_datum && !dimension.is_empty() {
        // SolidWorks makes it attached to nothing, off the view (seen
        // 2026-10-24).
        return Err(invalid(
            "a datum symbol can't go on a dimension (SolidWorks leaves it attached to nothing): put it on the              feature's edge in the view (on = the hole's circle, a shaft's silhouette)",
        ));
    }
    if is_datum && on.is_empty() {
        return Err(invalid("a datum symbol goes on a view entity: give on (\"view_edge:4\")"));
    }
    let attach: Vec<String> = if on.is_empty() { Vec::new() } else { vec![on.clone()] };
    let arrows: Vec<[f64; 2]> = match &place.arrow_at_mm {
        None => Vec::new(),
        Some(_) if on.is_empty() => {
            return Err(invalid("arrow_at_mm goes with on: the view entity the leader points at"));
        }
        Some(a) => match a.as_slice() {
            [x, y] => vec![[*x, *y]],
            _ => return Err(invalid("arrow_at_mm: in a drawing give [x, y] sheet mm")),
        },
    };
    let leaders = note::leaders(ids, cache, &attach, &arrows)?;
    let mut remarks = label_remarks(&symbol);
    remarks.extend(leaders.remarks.iter().cloned());
    if let Some(lv) = &leaders.view
        && !view.is_empty()
        && *lv != view
    {
        return Err(invalid(format!("{on} is in {lv}, so the symbol belongs to {lv}, not {view}")));
    }
    if (!on.is_empty() || !dimension.is_empty()) && place.sheet.is_some() {
        return Err(invalid("sheet is for a free frame; one on an entity or dimension goes on that view's sheet"));
    }
    let owner_view = leaders.view.clone().unwrap_or(view.clone());
    let position = match (&place.at_mm, place.at_view_mm) {
        (Some(_), Some(_)) => return Err(invalid("give at_mm (sheet) or at_view_mm (view), not both")),
        (Some(at), None) => match at.as_slice() {
            [x, y] => Some(note::sheet_point(note::point([*x, *y], "at_mm")?)),
            _ => return Err(invalid("at_mm: in a drawing give [x, y] sheet mm")),
        },
        (None, Some(at)) => {
            let in_view = if owner_view.is_empty() { None } else { Some(owner_view.clone()) };
            let Some(v) = in_view else {
                return Err(invalid("at_view_mm needs a view: give view, on or dimension"));
            };
            let p = note::point(at, "at_view_mm")?;
            Some(proto::DrawingPoint { x: p.x, y: p.y, view_name: v })
        }
        (None, None) if !is_datum => match leaders.tips.first() {
            // A frame with a leader: outside the view, next to its arrow.
            Some(tip) => {
                let size = note::text_size(&callout_of(&symbol), note::DEFAULT_TEXT_HEIGHT);
                // A frame's cells are wider than the callout's letters.
                let size = DVec2::new(size.x * 1.3, size.y);
                remarks.push("the frame was placed next to its arrow, outside the view; pass at_mm to choose".into());
                Some(note::sheet_point(note::text_spot(*tip, cache.outline(&owner_view), size)))
            }
            None => None,
        },
        // A datum symbol's box: SolidWorks stood it at the view's middle
        // (seen 2026-10-24), so it goes out past the view from its triangle
        // (its position is the box's bottom middle).
        (None, None) => leaders.tips.first().map(|tip| {
            remarks.push("the datum symbol was placed outside the view, past its triangle; pass at_mm to choose".into());
            note::sheet_point(note::text_spot(*tip, cache.outline(&owner_view), DVec2::ZERO))
        }),
    };
    let request = proto::SetupDrawingAnnotationRequest {
        sheet_name: text(place.sheet.as_deref()),
        view_name: owner_view,
        position,
        leader_to: leaders.refs,
        arrow_points: leaders.tips.into_iter().map(note::sheet_point).collect(),
        gdt: Some(symbol),
        attach_to_dimension: dimension,
        ..Default::default()
    };
    Ok((request, remarks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_params(c: Characteristic, datums: &[&str]) -> FrameParams {
        FrameParams {
            characteristic: c,
            tolerance: ToleranceValue::Mm(0.05),
            diameter_zone: false,
            modifier: None,
            datums: datums.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn params(frame: FrameParams) -> AddGeometricToleranceParams {
        AddGeometricToleranceParams {
            frame,
            second_frame: None,
            composite: false,
            datum_feature: None,
            projected_zone_mm: None,
            place: PlaceParams::default(),
        }
    }

    fn gtol_of(s: &proto::GdtSymbol) -> &proto::AnnotationGtol {
        match &s.kind {
            Some(proto::gdt_symbol::Kind::Gtol(g)) => g,
            _ => panic!("not a frame"),
        }
    }

    #[test]
    fn position_frame_with_modifiers() {
        let mut f = frame_params(Characteristic::Position, &["A", "B(M)", "C"]);
        f.diameter_zone = true;
        f.modifier = Some(Modifier::Mmc);
        f.tolerance = ToleranceValue::Mm(0.1);
        let s = gtol(&params(f)).unwrap();
        let g = gtol_of(&s);
        let row = &g.frames[0];
        assert_eq!((row.symbol, row.tolerance1.as_str(), row.tolerance1_diameter, row.tolerance1_condition), (23, "0.1", true, 1));
        assert_eq!(row.datums, vec!["A", "B", "C"]);
        assert_eq!(row.datum_conditions, vec![0, 1, 0]);
        assert_eq!(pmi::frame_callout(row), "position | dia 0.1 (M) | A | B (M) | C");
    }

    #[test]
    fn gdt_rules() {
        // Form tolerances never take datums; orientation needs one.
        assert!(gtol(&params(frame_params(Characteristic::Flatness, &["A"]))).is_err());
        assert!(gtol(&params(frame_params(Characteristic::Perpendicularity, &[]))).is_err());
        assert!(gtol(&params(frame_params(Characteristic::Flatness, &[]))).is_ok());
        assert!(gtol(&params(frame_params(Characteristic::Position, &[]))).is_ok());
        // At most 3 datums, no repeats, sane letters.
        assert!(gtol(&params(frame_params(Characteristic::Position, &["A", "B", "C", "D"]))).is_err());
        assert!(gtol(&params(frame_params(Characteristic::Position, &["A", "A"]))).is_err());
        assert!(gtol(&params(frame_params(Characteristic::Position, &["A(X)"]))).is_err());
        assert!(gtol(&params(frame_params(Characteristic::Position, &["<MOD-MMC>"]))).is_err());
        let common = gtol(&params(frame_params(Characteristic::CircularRunout, &["A-B"]))).unwrap();
        assert_eq!(gtol_of(&common).frames[0].datums, vec!["A-B"]);
        // Tolerances: positive mm or a plain text.
        let mut f = frame_params(Characteristic::Flatness, &[]);
        f.tolerance = ToleranceValue::Mm(-0.1);
        assert!(gtol(&params(f.clone())).is_err());
        f.tolerance = ToleranceValue::Text("0.1/25".into());
        assert_eq!(gtol_of(&gtol(&params(f)).unwrap()).frames[0].tolerance1, "0.1/25");
    }

    #[test]
    fn composite_frames() {
        let mut p = params(frame_params(Characteristic::Position, &["A", "B", "C"]));
        p.composite = true;
        assert!(gtol(&p).is_err(), "composite without a second frame");
        p.second_frame = Some(frame_params(Characteristic::Flatness, &[]));
        assert!(gtol(&p).is_err(), "composite rows share a characteristic");
        p.second_frame = Some(frame_params(Characteristic::Position, &["A"]));
        let s = gtol(&p).unwrap();
        assert!(gtol_of(&s).composite);
        assert_eq!(gtol_of(&s).frames.len(), 2);
    }

    #[test]
    fn names_parse_from_json() {
        let p: AddGeometricToleranceParams = serde_json::from_value(serde_json::json!({
            "characteristic": "true_position", "tolerance": "0.2", "modifier": "M",
            "datums": ["A"], "on": "face:3", "arrow_at_mm": [1, 2, 3]
        }))
        .unwrap();
        assert_eq!(p.frame.characteristic, Characteristic::Position);
        assert_eq!(p.frame.modifier, Some(Modifier::Mmc));
        assert_eq!(p.frame.tolerance, ToleranceValue::Text("0.2".into()));
        assert_eq!(p.place.on.as_deref(), Some("face:3"));
        let d: AddDatumSymbolParams = serde_json::from_value(serde_json::json!({ "label": "B", "on": "view_edge:2" })).unwrap();
        assert_eq!(d.label.as_deref(), Some("B"));
    }
}
