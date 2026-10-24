//! The drawing annotation tools: inspect_drawing_annotations,
//! add_drawing_note, edit_drawing_annotation and delete_drawing_annotations.
//!
//! An annotation of a drawing belongs to a view (it moves with the view),
//! to a sheet, or to a sheet's format (the border / title block layer).
//! Whichever it belongs to, SolidWorks positions it in sheet coordinates --
//! the sheet format's are the sheet's -- so a position means the same spot
//! on the paper in all three. A view's own coordinates are millimeters at
//! the model's size from the model origin as the view shows it, x to the
//! view's right: what stays put on the view when the view moves or is
//! rescaled. Converting between them, choosing leader arrow tips on the
//! entities and a spot for the text, and checking where things ended up
//! happen here.

use std::collections::HashMap;

use glam::DVec2;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::drawing::{self, Cache, Shape, annotation_json, exit_distance, owner_text, sheet_mm};
use crate::inspect::{IdRegistry, MM_PER_M, round};
use crate::pmi;

pub const DEFAULT_LIMIT: usize = 100;
/// The add-in's limits.
pub const MAX_LEADERS: usize = 50;
pub const MAX_DELETED: usize = 500;

/// A text spot chosen for a note with leaders: this far past the view's
/// outline, along the direction from the view's middle to the first arrow.
const TEXT_CLEARANCE: f64 = 0.010;
/// Arrow tips given further than this from their entity are moved onto it,
/// and the reply says so.
const TIP_SLACK: f64 = 5e-4;
/// A view's note further than this from the view's outline is probably
/// misplaced: the reply says so.
const FAR_FROM_VIEW: f64 = 0.050;
/// SolidWorks' usual note height, for guessing a text's size before it's
/// made.
pub(crate) const DEFAULT_TEXT_HEIGHT: f64 = 0.0035;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

fn mm2(p: DVec2) -> String {
    format!("[{}, {}]", round(p.x * MM_PER_M, 2), round(p.y * MM_PER_M, 2))
}

// ---- Coordinates ----------------------------------------------------------------

/// A view's own coordinates: `origin` is where the model origin lands on
/// the sheet, `scale` sheet length per model length, `angle` the view's
/// rotation on the sheet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewFrame {
    origin: DVec2,
    scale: f64,
    angle: f64,
}

impl ViewFrame {
    pub fn of(view: &proto::ViewInfo) -> Option<ViewFrame> {
        let origin = view.model_origin.as_ref()?;
        let scale = view.scale_numerator / view.scale_denominator;
        (scale.is_finite() && scale > 0.0).then(|| ViewFrame {
            origin: DVec2::new(origin.x, origin.y),
            scale,
            angle: view.angle,
        })
    }

    /// Sheet meters -> the view's (meters at the model's size).
    pub fn to_view(&self, sheet: DVec2) -> DVec2 {
        DVec2::from_angle(-self.angle).rotate(sheet - self.origin) / self.scale
    }

    pub fn to_sheet(&self, view: DVec2) -> DVec2 {
        self.origin + DVec2::from_angle(self.angle).rotate(view * self.scale)
    }
}

/// The frames of a sheet's views, by name.
pub fn frames(sheet: &proto::SheetInfo) -> HashMap<String, ViewFrame> {
    sheet.views.iter().filter_map(|v| Some((v.name.clone(), ViewFrame::of(v)?))).collect()
}

pub fn outline(view: &proto::ViewInfo) -> Option<(DVec2, DVec2)> {
    let b = view.outline.as_ref()?;
    let (lo, hi) = (b.min.as_ref()?, b.max.as_ref()?);
    Some((DVec2::new(lo.x, lo.y), DVec2::new(hi.x, hi.y)))
}

pub(crate) fn point(at_mm: [f64; 2], what: &str) -> Result<DVec2, McpError> {
    let p = DVec2::new(at_mm[0], at_mm[1]) / MM_PER_M;
    if p.is_finite() { Ok(p) } else { Err(invalid(format!("{what}: give [x, y] in mm"))) }
}

pub(crate) fn sheet_point(p: DVec2) -> proto::DrawingPoint {
    proto::DrawingPoint { x: p.x, y: p.y, view_name: String::new() }
}

// ---- Names ------------------------------------------------------------------------

/// Where a note goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// In a drawing view: it moves with the view. Notes with leaders to a view's edges belong to
    /// that view.
    View,
    /// On the sheet: it stays where it is on the paper when views move.
    Sheet,
    /// In the sheet format, the layer of the border and title block: shown and hidden with the
    /// format. For title block entries.
    SheetFormat,
}

impl Place {
    fn sw(self) -> proto::AnnotationOwner {
        match self {
            Place::View => proto::AnnotationOwner::View,
            Place::Sheet => proto::AnnotationOwner::Sheet,
            Place::SheetFormat => proto::AnnotationOwner::SheetFormat,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeaderKind {
    /// No leader lines (the attachments stay).
    None,
    Straight,
    /// With a short horizontal shoulder at the text.
    Bent,
    /// The text underlined, the leader from the line's end.
    Underlined,
}

impl LeaderKind {
    fn sw(self) -> i32 {
        match self {
            LeaderKind::None => 0,
            LeaderKind::Straight => 1,
            LeaderKind::Bent => 2,
            LeaderKind::Underlined => 3,
        }
    }
}

fn leader_name(style: i32) -> &'static str {
    match style & 0xFF {
        0 => "none",
        1 => "straight",
        2 => "bent",
        3 => "underlined",
        4 => "spline",
        _ => "other",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Arrow {
    /// Filled arrowhead (the usual one).
    Closed,
    Open,
    Dot,
    Slash,
    /// Arrowhead as wide as it is long.
    Wide,
    /// No arrowhead.
    None,
}

impl Arrow {
    fn sw(self) -> i32 {
        match self {
            Arrow::Open => 0,
            Arrow::Closed => 1,
            Arrow::Slash => 2,
            Arrow::Dot => 3,
            Arrow::Wide => 5,
            Arrow::None => 10,
        }
    }
}

fn arrow_name(arrow: i32) -> Option<&'static str> {
    Some(match arrow {
        0 => "open",
        1 => "closed",
        2 => "slash",
        3 => "dot",
        4 => "origin",
        5 => "wide",
        6 => "iso_wide",
        7 => "rus",
        8 => "closed_top",
        9 => "closed_bottom",
        10 => "none",
        11 => "shoulder",
        12 => "smart",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Justification {
    Left,
    Center,
    Right,
}

impl Justification {
    fn sw(self) -> i32 {
        match self {
            Justification::Left => 1,
            Justification::Center => 2,
            Justification::Right => 3,
        }
    }
}

fn justification_name(j: i32) -> Option<&'static str> {
    Some(match j {
        1 => "left",
        2 => "center",
        3 => "right",
        _ => return None,
    })
}

/// A frame drawn around the text (SolidWorks' balloon styles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Border {
    None,
    Box,
    Circle,
    Triangle,
    Hexagon,
    Diamond,
    Pentagon,
    Square,
    Underline,
}

impl Border {
    fn sw(self) -> i32 {
        match self {
            Border::None => 0,
            Border::Circle => 1,
            Border::Triangle => 2,
            Border::Hexagon => 3,
            Border::Box => 4,
            Border::Diamond => 5,
            Border::Pentagon => 6,
            Border::Underline => 10,
            Border::Square => 11,
        }
    }
}

fn border_name(b: i32) -> Option<&'static str> {
    Some(match b {
        0 => "none",
        1 => "circle",
        2 => "triangle",
        3 => "hexagon",
        4 => "box",
        5 => "diamond",
        6 => "pentagon",
        7 => "split_circle",
        10 => "underline",
        11 => "square",
        _ => return None,
    })
}

// ---- Params -----------------------------------------------------------------------

/// How a note looks; unset = SolidWorks' / the note's own.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoteStyleParams {
    /// Text height in mm on the paper (SolidWorks' default is usually 3.5).
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// A font installed on the machine, e.g. "Arial".
    #[serde(default)]
    pub font: Option<String>,
    #[serde(default)]
    pub bold: Option<bool>,
    #[serde(default)]
    pub italic: Option<bool>,
    #[serde(default)]
    pub underline: Option<bool>,
    /// How the lines of a multi-line note line up.
    #[serde(default)]
    pub justification: Option<Justification>,
    /// Turns the text counterclockwise, degrees.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// A frame around the text: box, circle, ... or none.
    #[serde(default)]
    pub border: Option<Border>,
    /// The drawing layer to put it on (an existing one, e.g. "FORMAT"); "" for none.
    #[serde(default)]
    pub layer: Option<String>,
    /// The leader lines' shape: straight, bent (with a shoulder at the text), underlined, or
    /// none.
    #[serde(default)]
    pub leader: Option<LeaderKind>,
    /// Every leader's arrowhead: closed (filled), open, dot, slash, wide, none.
    #[serde(default)]
    pub arrow: Option<Arrow>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AddDrawingNoteParams {
    /// The note's text; "\n" starts a new line.
    pub text: String,
    /// Where the note belongs: "view" (moves with the view; the default when `view` or
    /// `attach_to` is given), "sheet" (stays put on the paper; the default otherwise),
    /// "sheet_format" (the border / title block layer).
    #[serde(default, rename = "in")]
    pub place: Option<Place>,
    /// The view the note goes in, e.g. "Drawing View1".
    #[serde(default)]
    pub view: Option<String>,
    /// The sheet of a sheet or sheet-format note. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Where the text's upper-left corner goes: [x, y] sheet mm from the sheet's lower-left
    /// corner (render_drawing's rulers). Default: next to the first leader's arrow outside the
    /// view, just above the view for a view note, or the sheet's middle.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// Instead of at_mm, for a view note: [x, y] in the view's own coordinates -- mm at the
    /// model's size from the model origin as the view shows it, x to the view's right, y up
    /// (for a 1:2 view, 10 view mm = 5 sheet mm).
    #[serde(default)]
    pub at_view_mm: Option<[f64; 2]>,
    /// Leaders: one to each of these entities of one view (ids from inspect_drawing with the
    /// view: "view_edge:3", "silhouette:1", "view_vertex:2"). The note then belongs to that view.
    #[serde(default)]
    pub attach_to: Vec<String>,
    /// Where each leader's arrow touches its entity, [x, y] sheet mm, one per attach_to entry
    /// (moved onto the entity if off it). Default: a line's middle, a circle's upper right.
    #[serde(default)]
    pub arrow_at_mm: Vec<[f64; 2]>,
    #[serde(flatten)]
    pub style: NoteStyleParams,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditDrawingAnnotationParams {
    /// The annotation's name as inspect_drawing_annotations lists it, e.g. "DetailItem12" or
    /// "RD1@Drawing View1".
    pub name: String,
    /// Only look for it on this sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Only look for it in this view; also the view at_view_mm is measured in.
    #[serde(default)]
    pub view: Option<String>,
    /// Notes: the new text ("\n" starts a new line).
    #[serde(default)]
    pub text: Option<String>,
    /// Move the text's upper-left corner (a dimension's text) to [x, y] sheet mm.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// Instead, [x, y] in the coordinates of `view` (the view it's in; mm at the model's size
    /// from the model origin).
    #[serde(default)]
    pub at_view_mm: Option<[f64; 2]>,
    /// Instead, shift it by [dx, dy] sheet mm.
    #[serde(default)]
    pub move_by_mm: Option<[f64; 2]>,
    /// Put it in this view so that it moves with it (it stays where it is on the paper). A
    /// sheet or sheet-format note is made again in the view under a new name, which the reply
    /// gives (SolidWorks only moves a view's annotations between views). A view's annotation
    /// can't be taken back onto the sheet: delete it and add it again.
    #[serde(default)]
    pub attach_to_view: Option<String>,
    /// Notes: replace the leaders with one to each of these entities of the view the note is in
    /// (or goes into with attach_to_view). The note is made again (same text, look and place)
    /// under a new name, which the reply gives.
    #[serde(default)]
    pub attach_to: Option<Vec<String>>,
    /// With attach_to: where each leader's arrow touches its entity, [x, y] sheet mm.
    #[serde(default)]
    pub arrow_at_mm: Option<Vec<[f64; 2]>>,
    /// Notes: back to the document's note font (instead of height_mm / font / bold / italic /
    /// underline). It also sets the text's angle back to 0: a turned note has its own font.
    #[serde(default)]
    pub document_font: bool,
    #[serde(flatten)]
    pub style: NoteStyleParams,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DeleteDrawingAnnotationsParams {
    /// Annotation names as inspect_drawing_annotations lists them (notes, dimensions, ...).
    pub names: Vec<String>,
    /// Only look for them on this sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Only look for them in this view.
    #[serde(default)]
    pub view: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectDrawingAnnotationsParams {
    /// Only this sheet. Default: every sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Only the annotations in this view.
    #[serde(default)]
    pub view: Option<String>,
    /// Only those that belong to: "view" (any view), "sheet" or "sheet_format".
    #[serde(default, rename = "in")]
    pub place: Option<Place>,
    /// Only this kind: note (balloons included), balloon, dimension, table, datum, gtol, surface_finish, center_mark, ...
    #[serde(default)]
    pub kind: Option<String>,
    /// Only those whose text contains this (any case).
    #[serde(default)]
    pub contains: Option<String>,
    /// Also list the sheet format's annotations (the title block's notes; otherwise only
    /// counted, unless in="sheet_format").
    #[serde(default)]
    pub include_sheet_format: bool,
    /// How many to list. Defaults to 100.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Skip this many first, for paging.
    #[serde(default)]
    pub offset: Option<usize>,
}

// ---- Leaders ----------------------------------------------------------------------

/// The point of `shape` nearest `p` (circles and arcs: on the circle);
/// None for curves, whose shape isn't known well enough.
fn project(shape: &Shape, p: DVec2) -> Option<DVec2> {
    match shape {
        Shape::Line { a, b } => {
            let d = *b - *a;
            let t = if d.length_squared() > 0.0 { ((p - *a).dot(d) / d.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
            Some(*a + d * t)
        }
        Shape::Circle { center, radius } | Shape::Arc { center, radius, .. } => {
            let v = p - *center;
            let dir = if v.length() > 1e-12 { v.normalize() } else { DVec2::X };
            Some(*center + dir * *radius)
        }
        Shape::Point { p: q } => Some(*q),
        Shape::Curve { .. } => None,
    }
}

/// Spots an arrow could go on an entity, best first: a line's middle and
/// then further out, a circle's upper right and then round it, an arc's
/// middle, the point itself.
fn tip_candidates(shape: &Shape) -> Vec<DVec2> {
    match shape {
        Shape::Line { a, b } => [0.5, 0.35, 0.65, 0.2, 0.8, 0.1, 0.9].iter().map(|t| *a + (*b - *a) * *t).collect(),
        Shape::Circle { center, radius } => (0..8)
            .map(|k| *center + DVec2::from_angle((45.0 + 90.0 * k as f64 / 2.0).to_radians()) * *radius)
            .collect(),
        Shape::Arc { center, radius, a, b } => {
            let m = (*a + *b) / 2.0 - *center;
            vec![if m.length() > 1e-9 { *center + m.normalize() * *radius } else { *a }]
        }
        Shape::Point { p } => vec![*p],
        Shape::Curve { .. } => Vec::new(),
    }
}

/// Where an arrow goes by default: the first candidate spot no other
/// entity of the view passes through (SolidWorks attaches the leader to
/// whatever is picked there), else the first.
fn default_tip(shape: &Shape, others: &[&Shape]) -> Option<DVec2> {
    let candidates = tip_candidates(shape);
    let clear = |p: &DVec2| others.iter().all(|o| project(o, *p).is_none_or(|q| q.distance(*p) > 2.0 * TIP_SLACK));
    candidates.iter().find(|p| clear(p)).or(candidates.first()).copied()
}

/// A rough size of the text on the paper: [width, height], sheet meters.
pub(crate) fn text_size(text: &str, height: f64) -> DVec2 {
    let lines: Vec<&str> = text.split('\n').collect();
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
    DVec2::new(longest * height * 0.75, lines.len() as f64 * height * 1.5)
}

/// Where to put the upper-left corner of a note's text whose first leader
/// points at `tip`: out of the view's outline along the direction from the
/// outline's middle to the tip, the text on the far side of that point.
pub(crate) fn text_spot(tip: DVec2, outline: Option<(DVec2, DVec2)>, size: DVec2) -> DVec2 {
    let (dir, past) = match outline {
        Some((lo, hi)) => {
            let middle = (lo + hi) / 2.0;
            let away = tip - middle;
            let dir = if away.length() > 1e-9 { away.normalize() } else { DVec2::new(1.0, 1.0).normalize() };
            (dir, exit_distance(tip, dir, lo, hi))
        }
        None => (DVec2::new(1.0, 1.0).normalize(), 0.0),
    };
    let at = tip + dir * (past + TEXT_CLEARANCE);
    // The corner given is the text's upper left: text to the left of `at`
    // ends there, text above it sits on it.
    DVec2::new(if dir.x < 0.0 { at.x - size.x } else { at.x }, if dir.y > 0.0 { at.y + size.y } else { at.y })
}

/// What the leaders of a note will be: entity refs, arrow tips (all or
/// none), the view, and remarks for the reply.
pub(crate) struct Leaders {
    pub refs: Vec<proto::DrawingEntityRef>,
    pub tips: Vec<DVec2>,
    pub view: Option<String>,
    pub remarks: Vec<String>,
}

pub(crate) fn leaders(ids: &IdRegistry, cache: &Cache, attach_to: &[String], arrows: &[[f64; 2]]) -> Result<Leaders, McpError> {
    if attach_to.len() > MAX_LEADERS {
        return Err(invalid(format!("at most {MAX_LEADERS} leaders per note")));
    }
    if !arrows.is_empty() && !attach_to.is_empty() && arrows.len() != attach_to.len() {
        return Err(invalid(format!(
            "arrow_at_mm: give one point per attach_to entity ({} for {})",
            arrows.len(),
            attach_to.len()
        )));
    }
    let mut out = Leaders { refs: Vec::new(), tips: Vec::new(), view: None, remarks: Vec::new() };
    for id in attach_to {
        let r = drawing::decode_ref(ids, id)?;
        match &out.view {
            Some(v) if *v != r.view_name => {
                return Err(invalid(format!(
                    "a note's leaders all go to one view's entities: {id} is in {}, the others in {v}",
                    r.view_name
                )));
            }
            _ => out.view = Some(r.view_name.clone()),
        }
        out.refs.push(r);
    }
    let shapes: Vec<Option<&Shape>> = attach_to.iter().map(|id| cache.entity(id).map(|g| &g.shape)).collect();
    if !arrows.is_empty() {
        for (i, given) in arrows.iter().enumerate() {
            let p = point(*given, "arrow_at_mm")?;
            let on = shapes.get(i).copied().flatten().and_then(|s| project(s, p));
            match (on, attach_to.get(i)) {
                (Some(q), Some(id)) if q.distance(p) > TIP_SLACK => {
                    out.remarks.push(format!(
                        "arrow {} was moved {} mm onto {id}, to {} mm",
                        i + 1,
                        round(q.distance(p) * MM_PER_M, 2),
                        mm2(q)
                    ));
                    out.tips.push(q);
                }
                _ => out.tips.push(p),
            }
        }
    } else if !attach_to.is_empty() {
        let tips: Option<Vec<DVec2>> = attach_to
            .iter()
            .zip(&shapes)
            .zip(&out.refs)
            .map(|((id, s), r)| {
                let others: Vec<&Shape> = cache.others_in(&r.view_name, id).map(|g| &g.shape).collect();
                s.and_then(|s| default_tip(s, &others))
            })
            .collect();
        match tips {
            Some(tips) => out.tips = tips,
            None => out.remarks.push(
                "the arrows are where SolidWorks put them (an end of each edge): inspect_drawing the view first \
                 for edge middles, or give arrow_at_mm"
                    .to_string(),
            ),
        }
    }
    Ok(out)
}

// ---- Requests ---------------------------------------------------------------------

fn style(params: &NoteStyleParams, document_font: bool) -> Result<proto::NoteStyle, McpError> {
    let mut s = proto::NoteStyle { document_font, ..Default::default() };
    if let Some(h) = params.height_mm {
        if !(h.is_finite() && h > 0.0 && h < 1000.0) {
            return Err(invalid("height_mm: give the text height in mm, above 0"));
        }
        s.text_height = Some(h / MM_PER_M);
    }
    if let Some(font) = params.font.as_deref().map(str::trim) {
        if font.is_empty() {
            return Err(invalid("font: give a font name, e.g. \"Arial\""));
        }
        s.font = Some(font.to_string());
    }
    s.bold = params.bold;
    s.italic = params.italic;
    s.underline = params.underline;
    if document_font
        && (s.text_height.is_some() || s.font.is_some() || s.bold.is_some() || s.italic.is_some() || s.underline.is_some())
    {
        return Err(invalid(
            "document_font puts the note back on the document's font: give it alone, or height_mm / font / bold / \
             italic / underline",
        ));
    }
    s.justification = params.justification.map(Justification::sw);
    if let Some(a) = params.angle_deg {
        if !a.is_finite() {
            return Err(invalid("angle_deg must be a number"));
        }
        // The angle is part of a note's own text format (SolidWorks).
        if document_font && a != 0.0 {
            return Err(invalid(
                "a turned note has its own font in SolidWorks: document_font sets the angle back to 0",
            ));
        }
        s.angle = Some(a.to_radians());
    }
    s.border = params.border.map(Border::sw);
    Ok(s)
}

fn has_style(s: &proto::NoteStyle) -> bool {
    s.text_height.is_some()
        || s.font.is_some()
        || s.bold.is_some()
        || s.italic.is_some()
        || s.underline.is_some()
        || s.document_font
        || s.justification.is_some()
        || s.angle.is_some()
        || s.border.is_some()
}

/// The add_drawing_note request, and remarks for the reply.
pub fn add_request(
    ids: &IdRegistry,
    cache: &Cache,
    params: &AddDrawingNoteParams,
) -> Result<(proto::SetupDrawingAnnotationRequest, Vec<String>), McpError> {
    if params.text.trim().is_empty() {
        return Err(invalid("text: give the note's text"));
    }
    let view = text(params.view.as_deref());
    let leaders = leaders(ids, cache, &params.attach_to, &params.arrow_at_mm)?;
    if params.attach_to.is_empty() && !params.arrow_at_mm.is_empty() {
        return Err(invalid("arrow_at_mm goes with attach_to: the entities the leaders point at"));
    }
    if let Some(lv) = &leaders.view
        && !view.is_empty()
        && *lv != view
    {
        return Err(invalid(format!(
            "the leaders go to {lv}'s entities, so the note belongs to {lv}, not {view}"
        )));
    }
    let note_view = leaders.view.clone().unwrap_or(view.clone());
    let place = params.place.unwrap_or(if note_view.is_empty() { Place::Sheet } else { Place::View });
    match place {
        Place::View if note_view.is_empty() => {
            return Err(invalid("in=\"view\": say which view (view=\"Drawing View1\")"));
        }
        Place::Sheet | Place::SheetFormat if leaders.view.is_some() => {
            return Err(invalid(format!(
                "a note with leaders to {}'s entities belongs to that view (it moves with it); leave `in` out",
                note_view
            )));
        }
        Place::Sheet | Place::SheetFormat if !view.is_empty() => {
            return Err(invalid(format!(
                "a {} note doesn't belong to a view: leave `view` out, or use in=\"view\"",
                if place == Place::Sheet { "sheet" } else { "sheet format" }
            )));
        }
        _ => {}
    }
    if params.style.leader.is_some_and(|l| l != LeaderKind::None) && leaders.refs.is_empty() {
        return Err(invalid("a leader needs something to point at: give attach_to"));
    }
    let mut remarks = leaders.remarks;
    let position = match (params.at_mm, params.at_view_mm) {
        (Some(_), Some(_)) => return Err(invalid("give at_mm (sheet) or at_view_mm (view), not both")),
        (Some(at), None) => Some(sheet_point(point(at, "at_mm")?)),
        (None, Some(at)) => {
            if place != Place::View {
                return Err(invalid(
                    "at_view_mm is for notes in a view; a sheet note stays put on the paper: give at_mm",
                ));
            }
            let p = point(at, "at_view_mm")?;
            Some(proto::DrawingPoint { x: p.x, y: p.y, view_name: note_view.clone() })
        }
        (None, None) => match leaders.tips.first() {
            Some(tip) => {
                let height = params.style.height_mm.map(|h| h / MM_PER_M).unwrap_or(DEFAULT_TEXT_HEIGHT);
                let at = text_spot(*tip, cache.outline(&note_view), text_size(&params.text, height));
                remarks.push("the text was placed next to the first arrow, outside the view; pass at_mm to choose".into());
                Some(sheet_point(at))
            }
            None => None,
        },
    };
    let request = proto::SetupDrawingAnnotationRequest {
        sheet_name: text(params.sheet.as_deref()),
        view_name: if place == Place::View { note_view } else { String::new() },
        owner: place.sw() as i32,
        text: Some(params.text.clone()),
        position,
        leader_to: leaders.refs,
        arrow_points: leaders.tips.into_iter().map(sheet_point).collect(),
        leader_style: params.style.leader.map(LeaderKind::sw),
        arrow: params.style.arrow.map(Arrow::sw),
        style: Some(style(&params.style, false)?),
        layer: params.style.layer.as_ref().map(|l| l.trim().to_string()),
        ..Default::default()
    };
    Ok((request, remarks))
}

/// The edit_drawing_annotation request, and remarks for the reply.
pub fn edit_request(
    ids: &IdRegistry,
    cache: &Cache,
    params: &EditDrawingAnnotationParams,
) -> Result<(proto::SetupDrawingAnnotationRequest, Vec<String>), McpError> {
    let name = text(Some(&params.name));
    if name.is_empty() {
        return Err(invalid("name: give the annotation's name, e.g. \"DetailItem12\""));
    }
    let view = text(params.view.as_deref());
    let attach_to = params.attach_to.as_deref().unwrap_or_default();
    let arrows = params.arrow_at_mm.as_deref().unwrap_or_default();
    if params.attach_to.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("attach_to: give the entities, or leave it out (leader=\"none\" hides the leaders)"));
    }
    if !arrows.is_empty() && attach_to.is_empty() {
        return Err(invalid(
            "arrow_at_mm goes with attach_to: a leader's arrow is placed where its entity is picked, so give the              entities again with the new points",
        ));
    }
    let leaders = leaders(ids, cache, attach_to, arrows)?;
    let target = text(params.attach_to_view.as_deref());
    if let Some(lv) = &leaders.view
        && !target.is_empty()
        && *lv != target
    {
        return Err(invalid(format!(
            "the leaders go to {lv}'s entities, so the note goes in {lv}, not {target}"
        )));
    }
    let mut request = proto::SetupDrawingAnnotationRequest {
        name: name.clone(),
        sheet_name: text(params.sheet.as_deref()),
        view_name: view.clone(),
        attach_to_view: target,
        text: params.text.clone(),
        leader_to: leaders.refs,
        arrow_points: leaders.tips.into_iter().map(sheet_point).collect(),
        leader_style: params.style.leader.map(LeaderKind::sw),
        arrow: params.style.arrow.map(Arrow::sw),
        style: Some(style(&params.style, params.document_font)?),
        layer: params.style.layer.as_ref().map(|l| l.trim().to_string()),
        ..Default::default()
    };
    if params.text.as_ref().is_some_and(|t| t.trim().is_empty()) {
        return Err(invalid("text: give the new text (delete_drawing_annotations removes a note)"));
    }
    match (params.at_mm, params.at_view_mm, params.move_by_mm) {
        (None, None, None) => {}
        (Some(at), None, None) => request.position = Some(sheet_point(point(at, "at_mm")?)),
        (None, Some(at), None) => {
            if view.is_empty() {
                return Err(invalid("at_view_mm: give `view`, the view the annotation is in"));
            }
            let p = point(at, "at_view_mm")?;
            request.position = Some(proto::DrawingPoint { x: p.x, y: p.y, view_name: view.clone() });
        }
        (None, None, Some(by)) => {
            let d = point(by, "move_by_mm")?;
            request.move_by = Some(proto::Vec3 { x: d.x, y: d.y, z: 0.0 });
        }
        _ => return Err(invalid("give one of at_mm, at_view_mm and move_by_mm")),
    }
    let changes = request.text.is_some()
        || request.position.is_some()
        || request.move_by.is_some()
        || !request.attach_to_view.is_empty()
        || !request.leader_to.is_empty()
        || !request.arrow_points.is_empty()
        || request.leader_style.is_some()
        || request.arrow.is_some()
        || request.layer.is_some()
        || request.style.as_ref().is_some_and(has_style);
    if !changes {
        return Err(invalid(
            "nothing to change: give text, at_mm / at_view_mm / move_by_mm, attach_to_view, attach_to, \
             arrow_at_mm, leader, arrow, a style (height_mm, font, bold, ...), document_font or layer",
        ));
    }
    Ok((request, leaders.remarks))
}

pub fn delete_request(params: &DeleteDrawingAnnotationsParams) -> Result<proto::DeleteDrawingAnnotationsRequest, McpError> {
    let names: Vec<String> = params.names.iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
    if names.is_empty() || names.len() > MAX_DELETED {
        return Err(invalid(format!("names: give 1 to {MAX_DELETED} annotation names")));
    }
    Ok(proto::DeleteDrawingAnnotationsRequest {
        names,
        sheet_name: text(params.sheet.as_deref()),
        view_name: text(params.view.as_deref()),
    })
}

pub fn inspect_request(params: &InspectDrawingAnnotationsParams) -> proto::InspectDrawingRequest {
    proto::InspectDrawingRequest {
        include_annotations: true,
        skip_sheet_format: !(params.include_sheet_format || params.place == Some(Place::SheetFormat)),
        ..Default::default()
    }
}

// ---- Replies ----------------------------------------------------------------------

fn box_of(b: &proto::Box3) -> Option<(DVec2, DVec2)> {
    let (lo, hi) = (b.min.as_ref()?, b.max.as_ref()?);
    let (lo, hi) = (DVec2::new(lo.x, lo.y), DVec2::new(hi.x, hi.y));
    (lo.is_finite() && hi.is_finite() && (hi - lo).length() > 0.0).then_some((lo, hi))
}

/// The box around an annotation's text (and leaders) on the sheet: a
/// note's extent, else its leaders' points and its position.
fn extent(a: &proto::AnnotationInfo) -> Option<(DVec2, DVec2)> {
    if let Some(b) = a.note.as_ref().and_then(|n| n.extent.as_ref()).and_then(box_of) {
        return Some(b);
    }
    None
}

/// One annotation as the drawing annotation tools show it: what
/// inspect_drawing shows plus where it belongs, its position in sheet mm
/// and (in a view) the view's coordinates, its look, box and leaders.
pub fn annotation_entry(ids: &mut IdRegistry, frames: &HashMap<String, ViewFrame>, a: &proto::AnnotationInfo) -> Value {
    let in_view = (a.owner() == proto::AnnotationOwner::View).then_some(a.owner_name.as_str());
    let mut obj = match annotation_json(ids, in_view, a) {
        Value::Object(o) => o,
        _ => Map::new(),
    };
    obj.remove("position");
    let frame = in_view.and_then(|v| frames.get(v));
    if let Some(p) = &a.position {
        let p = DVec2::new(p.x, p.y);
        obj.insert("at_mm".into(), sheet_mm(p));
        if let Some(f) = frame {
            obj.insert("at_view_mm".into(), sheet_mm(f.to_view(p)));
        }
    }
    if let Some((lo, hi)) = extent(a) {
        obj.insert("box_mm".into(), json!([sheet_mm(lo), sheet_mm(hi)]));
    }
    if let Some(n) = &a.note {
        let mut s = Map::new();
        if n.document_font {
            s.insert("font".into(), json!("document"));
        } else if !n.font.is_empty() {
            s.insert("font".into(), json!(n.font));
        }
        if n.text_height > 0.0 {
            s.insert("height_mm".into(), json!(round(n.text_height * MM_PER_M, 2)));
        }
        for (key, on) in [("bold", n.bold), ("italic", n.italic), ("underline", n.underline)] {
            if on {
                s.insert(key.into(), json!(true));
            }
        }
        if let Some(j) = justification_name(n.justification).filter(|j| *j != "left") {
            s.insert("justification".into(), json!(j));
        }
        if n.angle.abs() > 1e-9 {
            s.insert("angle_deg".into(), json!(round(n.angle.to_degrees(), 2)));
        }
        if let Some(b) = border_name(n.border).filter(|b| *b != "none") {
            s.insert("border".into(), json!(b));
        }
        obj.insert("style".into(), Value::Object(s));
        if !n.linked_text.is_empty() {
            obj.insert("linked_text".into(), json!(n.linked_text));
        }
    }
    if let Some(b) = &a.balloon {
        obj.insert("type".into(), json!("balloon"));
        obj.insert("balloon".into(), crate::table::balloon_json(b, a.note.as_ref().is_some_and(|n| n.border == 7)));
    }
    if !a.layer.is_empty() {
        obj.insert("layer".into(), json!(a.layer));
    }
    if !a.leaders.is_empty() {
        obj.insert("leader".into(), json!(leader_name(a.leader_style)));
        let smart = a.note.as_ref().is_some_and(|n| n.smart_arrows);
        let list: Vec<Value> = a
            .leaders
            .iter()
            .map(|l| {
                let mut e = Map::new();
                if let Some(tip) = l.points.last() {
                    let tip = DVec2::new(tip.x, tip.y);
                    e.insert("arrow_at_mm".into(), sheet_mm(tip));
                    if let Some(f) = frame {
                        e.insert("arrow_at_view_mm".into(), sheet_mm(f.to_view(tip)));
                    }
                }
                match arrow_name(l.arrow) {
                    Some(_) if smart => {
                        e.insert("arrow".into(), json!("smart"));
                    }
                    Some(name) => {
                        e.insert("arrow".into(), json!(name));
                    }
                    None => {}
                }
                Value::Object(e)
            })
            .collect();
        obj.insert("leaders".into(), Value::Array(list));
    }
    Value::Object(obj)
}

/// What looks wrong about where an annotation ended up on `sheet`.
fn placement_warnings(sheet: &proto::SheetInfo, a: &proto::AnnotationInfo) -> Vec<String> {
    let mut out = Vec::new();
    let Some((lo, hi)) = extent(a).or_else(|| a.position.as_ref().map(|p| (DVec2::new(p.x, p.y), DVec2::new(p.x, p.y))))
    else {
        return out;
    };
    let slack = 1e-4;
    if sheet.width > 0.0
        && (lo.x < -slack || lo.y < -slack || hi.x > sheet.width + slack || hi.y > sheet.height + slack)
    {
        out.push(format!(
            "it reaches past the sheet's edges ({:.0} x {:.0} mm)",
            sheet.width * MM_PER_M,
            sheet.height * MM_PER_M
        ));
    }
    let overlaps = |v: &proto::ViewInfo| {
        outline(v).is_some_and(|(vlo, vhi)| lo.x < vhi.x && vlo.x < hi.x && lo.y < vhi.y && vlo.y < hi.y)
    };
    match a.owner() {
        proto::AnnotationOwner::Sheet | proto::AnnotationOwner::SheetFormat => {
            let over: Vec<&str> = sheet.views.iter().filter(|v| overlaps(v)).map(|v| v.name.as_str()).collect();
            if !over.is_empty() {
                out.push(format!(
                    "it's over {} but belongs to the {}: it stays put when the view moves (attach_to_view makes it \
                     follow)",
                    over.join(", "),
                    if a.owner() == proto::AnnotationOwner::Sheet { "sheet" } else { "sheet format" }
                ));
            }
        }
        proto::AnnotationOwner::View => {
            if let Some((vlo, vhi)) = sheet.views.iter().find(|v| v.name == a.owner_name).and_then(outline) {
                let gap = DVec2::ZERO.max(vlo - hi).max(lo - vhi).length();
                if gap > FAR_FROM_VIEW {
                    out.push(format!(
                        "it belongs to {} but sits {:.0} mm from its outline -- was it meant for the sheet, or for \
                         another view?",
                        a.owner_name,
                        gap * MM_PER_M
                    ));
                }
            }
        }
        proto::AnnotationOwner::Unspecified => {}
    }
    out
}

fn first_text(a: &proto::AnnotationInfo) -> String {
    // A frame's or datum symbol's raw texts are SolidWorks' symbol codes.
    if let Some(g) = a.gtol.as_ref().filter(|g| !g.frames.is_empty()) {
        return g.frames.iter().map(pmi::frame_callout).collect::<Vec<_>>().join(" / ");
    }
    if !a.datum_label.trim().is_empty() {
        return format!("datum {}", a.datum_label.trim());
    }
    let t = a.text.first().map(|t| t.replace("\r\n", " / ").replace('\n', " / ")).unwrap_or_default();
    if t.chars().count() > 40 { format!("{}...", t.chars().take(40).collect::<String>()) } else { t }
}

/// The reply to add_drawing_note / edit_drawing_annotation.
pub fn setup_reply(
    ids: &mut IdRegistry,
    request: &proto::SetupDrawingAnnotationRequest,
    reply: &proto::SetupDrawingAnnotationResponse,
    remarks: &[String],
) -> String {
    let Some(a) = &reply.annotation else { return "done".to_string() };
    let sheet = reply.sheet.clone().unwrap_or_default();
    let frames = frames(&sheet);
    let kind = pmi::type_name(a.r#type);
    let mut text = if reply.replaced.is_empty() {
        format!("{} {kind} {}", if reply.created { "added" } else { "changed" }, a.name)
    } else {
        let why = if request.leader_to.is_empty() {
            "SolidWorks doesn't move a sheet note into a view, so it was made again there"
        } else {
            "its leaders were made again"
        };
        format!("changed note {} -- {why}: it's now {} ({} was deleted) --", reply.replaced, a.name, reply.replaced)
    };
    let words = first_text(a);
    if !words.is_empty() && a.r#type == 6 {
        text += &format!(" \"{words}\"");
    }
    text += &match a.owner() {
        proto::AnnotationOwner::View => format!(" in {} on {} (it moves with the view)", a.owner_name, sheet.name),
        proto::AnnotationOwner::Sheet => format!(" on sheet {}", sheet.name),
        proto::AnnotationOwner::SheetFormat => {
            format!(" in the sheet format of {} (the title block layer: shown and hidden with the format)", sheet.name)
        }
        proto::AnnotationOwner::Unspecified => String::new(),
    };
    if let Some(p) = &a.position {
        let p = DVec2::new(p.x, p.y);
        text += &format!(": text's upper-left corner at {} sheet mm", mm2(p));
        if a.owner() == proto::AnnotationOwner::View
            && let Some(f) = frames.get(&a.owner_name)
        {
            text += &format!(" ({} mm in {}'s coordinates)", mm2(f.to_view(p)), a.owner_name);
        }
    }
    if let Some((lo, hi)) = extent(a) {
        text += &format!(
            ", box x {:.1}..{:.1}, y {:.1}..{:.1} mm",
            lo.x * MM_PER_M,
            hi.x * MM_PER_M,
            lo.y * MM_PER_M,
            hi.y * MM_PER_M
        );
    }
    if !a.leaders.is_empty() {
        let tips: Vec<String> =
            a.leaders.iter().filter_map(|l| l.points.last()).map(|p| mm2(DVec2::new(p.x, p.y))).collect();
        text += &format!("; {} {} leader(s), arrows at {} mm", a.leaders.len(), leader_name(a.leader_style), tips.join(", "));
    }
    if sheet.name != reply.active_sheet && !reply.active_sheet.is_empty() {
        text += &format!(" ({} stays the active sheet)", reply.active_sheet);
    }
    text += ".";

    let mut notes: Vec<String> = remarks.to_vec();
    // Where it was asked to go, on the sheet (a view's coordinates through
    // the view as it is now).
    let asked = request.position.as_ref().and_then(|asked| {
        let at = DVec2::new(asked.x, asked.y);
        if asked.view_name.is_empty() { Some(at) } else { frames.get(&asked.view_name).map(|f| f.to_sheet(at)) }
    });
    if let (Some(asked), Some(p)) = (asked, &a.position)
        && asked.distance(DVec2::new(p.x, p.y)) > TIP_SLACK
    {
        notes.push(format!("it's at {} sheet mm, not {} sheet mm", mm2(DVec2::new(p.x, p.y)), mm2(asked)));
    }
    // Arrows go where their entity was picked (SolidWorks numbers the
    // leaders in its own order, so they're matched by place). A datum
    // symbol has no leader: its triangle sits where it was picked.
    let tips: Vec<DVec2> = a.leaders.iter().filter_map(|l| l.points.last()).map(|p| DVec2::new(p.x, p.y)).collect();
    let datum = a.r#type == 2;
    for (i, asked) in request.arrow_points.iter().enumerate().filter(|_| !datum) {
        let asked = DVec2::new(asked.x, asked.y);
        if !tips.iter().any(|t| t.distance(asked) <= TIP_SLACK) {
            notes.push(format!(
                "no arrow is at {} mm (attach_to entry {}): SolidWorks put that leader's arrow at an end of its entity",
                mm2(asked),
                i + 1
            ));
        }
    }
    notes.extend(placement_warnings(&sheet, a));
    if !notes.is_empty() {
        let notes = notes.join("; ");
        text += &format!("\nNote: {notes}{}", if notes.ends_with(['.', '?']) { "" } else { "." });
    }
    text += &format!("\n{}", annotation_entry(ids, &frames, a));
    text
}

pub fn delete_reply(reply: &proto::DeleteDrawingAnnotationsResponse) -> String {
    let list: Vec<String> = reply
        .deleted
        .iter()
        .map(|a| {
            let words = first_text(a);
            let words = if words.is_empty() { String::new() } else { format!(" \"{words}\"") };
            let place = owner_text(a).map(|o| format!(", {}", o.replace('_', " "))).unwrap_or_default();
            format!("{} ({}{words}{place})", a.name, pmi::type_name(a.r#type))
        })
        .collect();
    format!("deleted {} annotation(s): {}", list.len(), list.join(", "))
}

/// The inspect_drawing_annotations JSON.
pub fn render_inspect(
    ids: &mut IdRegistry,
    reply: &proto::InspectDrawingResponse,
    params: &InspectDrawingAnnotationsParams,
) -> Result<Value, McpError> {
    let want_sheet = text(params.sheet.as_deref());
    let want_view = text(params.view.as_deref());
    let kind = text(params.kind.as_deref()).to_ascii_lowercase();
    let contains = text(params.contains.as_deref()).to_lowercase();
    let with_format = params.include_sheet_format || params.place == Some(Place::SheetFormat);
    let sheets: Vec<&proto::SheetInfo> = reply
        .sheets
        .iter()
        .filter(|s| want_sheet.is_empty() || s.name.eq_ignore_ascii_case(&want_sheet))
        .collect();
    if sheets.is_empty() {
        let names: Vec<&str> = reply.sheets.iter().map(|s| s.name.as_str()).collect();
        return Err(invalid(format!("the drawing has no sheet {want_sheet:?} (its sheets: {})", names.join(", "))));
    }
    if !want_view.is_empty() && !sheets.iter().any(|s| s.views.iter().any(|v| v.name == want_view)) {
        return Err(invalid(format!("no view {want_view:?} -- inspect_drawing lists the views")));
    }

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let mut seen = 0usize;
    let mut shown = 0usize;
    let mut hidden_format = 0usize;
    let mut out_sheets = Vec::new();
    for sheet in sheets {
        let frames = frames(sheet);
        let mut list: Vec<&proto::AnnotationInfo> = Vec::new();
        for v in &sheet.views {
            if want_view.is_empty() || v.name == want_view {
                list.extend(v.annotations.iter());
            }
        }
        if want_view.is_empty() {
            list.extend(sheet.annotations.iter());
        }
        let mut entries = Vec::new();
        for a in list {
            let place = match a.owner() {
                proto::AnnotationOwner::View => Place::View,
                proto::AnnotationOwner::SheetFormat => Place::SheetFormat,
                _ => Place::Sheet,
            };
            if params.place.is_some_and(|p| p != place)
                || (!kind.is_empty()
                    && pmi::type_name(a.r#type) != kind
                    && !(kind == "balloon" && a.balloon.is_some()))
                || (!contains.is_empty() && !a.text.iter().any(|t| t.to_lowercase().contains(&contains)))
            {
                continue;
            }
            if place == Place::SheetFormat && !with_format {
                continue;
            }
            seen += 1;
            if seen <= offset || shown >= limit {
                continue;
            }
            shown += 1;
            entries.push(annotation_entry(ids, &frames, a));
        }
        let views: Vec<Value> = sheet
            .views
            .iter()
            .filter(|v| want_view.is_empty() || v.name == want_view)
            .map(|v| {
                let mut e = Map::new();
                e.insert("name".into(), json!(v.name));
                if let Some((lo, hi)) = outline(v) {
                    e.insert("outline".into(), json!([sheet_mm(lo), sheet_mm(hi)]));
                }
                if let Some(f) = ViewFrame::of(v) {
                    e.insert("view_origin_mm".into(), sheet_mm(f.origin));
                    if let Some(scale) = drawing::scale_text(v.scale_numerator, v.scale_denominator) {
                        e.insert("scale".into(), json!(scale));
                    }
                    if f.angle.abs() > 1e-9 {
                        e.insert("angle_deg".into(), json!(round(f.angle.to_degrees(), 2)));
                    }
                }
                Value::Object(e)
            })
            .collect();
        let mut s = Map::new();
        s.insert("name".into(), json!(sheet.name));
        if sheet.active {
            s.insert("active".into(), json!(true));
        }
        if sheet.width > 0.0 {
            s.insert("size_mm".into(), json!([round(sheet.width * MM_PER_M, 1), round(sheet.height * MM_PER_M, 1)]));
        }
        s.insert("views".into(), Value::Array(views));
        s.insert("annotations".into(), Value::Array(entries));
        // The add-in only counts them unless asked for.
        if !with_format && want_view.is_empty() && params.place.is_none() {
            hidden_format += sheet
                .sheet_format_annotation_types
                .iter()
                .filter(|t| kind.is_empty() || pmi::type_name(**t) == kind)
                .count();
        }
        if !with_format && !sheet.sheet_format_annotation_types.is_empty() && want_view.is_empty() {
            s.insert(
                "sheet_format_annotation_counts".into(),
                drawing::annotation_counts(&sheet.sheet_format_annotation_types),
            );
        }
        out_sheets.push(Value::Object(s));
    }

    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
    }
    out.insert(
        "coordinates".into(),
        json!(
            "at_mm / arrow_at_mm / box_mm / outline / view_origin_mm: sheet mm from the sheet's lower-left corner \
             (render_drawing's rulers; the sheet format uses the same). at_view_mm / arrow_at_view_mm: the view's own \
             coordinates, mm at the model's size from the view's model origin (view_origin_mm on the sheet), x to the \
             view's right. at_mm is the text's upper-left corner. `in`: what it belongs to -- a view (moves with it), \
             the sheet, or the sheet format (title block layer)"
        ),
    );
    out.insert("sheets".into(), Value::Array(out_sheets));
    let mut notes = Vec::new();
    if seen > offset + shown {
        notes.push(format!(
            "showing {}-{} of {seen}; pass offset={} for more",
            offset + 1,
            offset + shown,
            offset + shown
        ));
    }
    if hidden_format > 0 {
        notes.push(format!(
            "{hidden_format} sheet format annotation(s) (title block) not listed: include_sheet_format=true or \
             in=\"sheet_format\""
        ));
    }
    if reply.truncated {
        notes.push("the drawing has more annotations than the add-in reports at once".to_string());
    }
    if !notes.is_empty() {
        out.insert("note".into(), json!(notes.join("; ")));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(name: &str, origin: [f64; 2], scale: f64, angle: f64) -> proto::ViewInfo {
        proto::ViewInfo {
            name: name.into(),
            scale_numerator: 1.0,
            scale_denominator: 1.0 / scale,
            angle,
            model_origin: Some(proto::Vec3 { x: origin[0], y: origin[1], z: 0.0 }),
            outline: Some(proto::Box3 {
                min: Some(proto::Vec3 { x: origin[0] - 0.03, y: origin[1] - 0.02, z: 0.0 }),
                max: Some(proto::Vec3 { x: origin[0] + 0.03, y: origin[1] + 0.02, z: 0.0 }),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn view_frames_go_both_ways() {
        // A 1:2 view with its model origin at (300, 152.5) mm: 10 view mm
        // right is 5 sheet mm right.
        let f = ViewFrame::of(&view("V", [0.3, 0.1525], 0.5, 0.0)).unwrap();
        let p = f.to_sheet(DVec2::new(0.01, -0.004));
        assert!((p - DVec2::new(0.305, 0.1505)).length() < 1e-12, "{p}");
        assert!((f.to_view(p) - DVec2::new(0.01, -0.004)).length() < 1e-12);
        // Turned 90 degrees: the view's x is the sheet's y.
        let f = ViewFrame::of(&view("V", [0.1, 0.1], 1.0, std::f64::consts::FRAC_PI_2)).unwrap();
        assert!((f.to_sheet(DVec2::new(0.01, 0.0)) - DVec2::new(0.1, 0.11)).length() < 1e-12);
        // No transform reported: no frame.
        assert!(ViewFrame::of(&proto::ViewInfo { scale_denominator: 1.0, ..Default::default() }).is_none());
    }

    #[test]
    fn tips_land_on_the_entity() {
        let line = Shape::Line { a: DVec2::new(0.1, 0.1), b: DVec2::new(0.14, 0.1) };
        let near = |a: Option<DVec2>, b: DVec2| a.is_some_and(|a| a.distance(b) < 1e-12);
        assert!(near(default_tip(&line, &[]), DVec2::new(0.12, 0.1)));
        // Something else crosses the middle: the next spot along.
        let across = Shape::Line { a: DVec2::new(0.117, 0.1), b: DVec2::new(0.123, 0.1) };
        assert!(near(default_tip(&line, &[&across]), DVec2::new(0.114, 0.1)));
        assert!(near(project(&line, DVec2::new(0.13, 0.105)), DVec2::new(0.13, 0.1)));
        assert!(near(project(&line, DVec2::new(0.2, 0.1)), DVec2::new(0.14, 0.1)));
        let circle = Shape::Circle { center: DVec2::new(0.1, 0.1), radius: 0.005 };
        let tip = default_tip(&circle, &[]).unwrap();
        assert!((tip.distance(DVec2::new(0.1, 0.1)) - 0.005).abs() < 1e-12 && tip.x > 0.1 && tip.y > 0.1);
        assert!((project(&circle, DVec2::new(0.1, 0.2)).unwrap() - DVec2::new(0.1, 0.105)).length() < 1e-12);
        assert_eq!(project(&Shape::Curve { a: DVec2::ZERO, b: DVec2::X, center: None }, DVec2::Y), None);
    }

    #[test]
    fn text_goes_outside_the_view() {
        // A tip on the top edge of a view spanning x 100..140, y 140..160:
        // the text goes above the outline, its corner on the far side.
        let outline = Some((DVec2::new(0.1, 0.14), DVec2::new(0.14, 0.16)));
        let size = DVec2::new(0.02, 0.005);
        let at = text_spot(DVec2::new(0.12, 0.16), outline, size);
        assert!(at.y - size.y >= 0.16 + TEXT_CLEARANCE - 1e-12, "{at}");
        // Left edge: the text ends left of the outline.
        let at = text_spot(DVec2::new(0.1, 0.15), outline, size);
        assert!(at.x + size.x <= 0.1 - TEXT_CLEARANCE + 1e-12, "{at}");
    }

    #[test]
    fn add_requests_say_where_the_note_belongs() {
        let ids = IdRegistry::default();
        let cache = Cache::default();
        let add = |v: serde_json::Value| add_request(&ids, &cache, &serde_json::from_value(v).unwrap());
        // Sheet by default; a view note when a view is given.
        let (r, _) = add(json!({"text": "NOTE 1", "at_mm": [50, 250]})).unwrap();
        assert_eq!((r.owner(), r.view_name.as_str()), (proto::AnnotationOwner::Sheet, ""));
        assert_eq!(r.position, Some(proto::DrawingPoint { x: 0.05, y: 0.25, view_name: String::new() }));
        let (r, _) = add(json!({"text": "A", "view": "Drawing View2", "at_view_mm": [10, -4]})).unwrap();
        assert_eq!((r.owner(), r.view_name.as_str()), (proto::AnnotationOwner::View, "Drawing View2"));
        assert_eq!(r.position.unwrap().view_name, "Drawing View2");
        let (r, _) = add(json!({"text": "T", "in": "sheet_format", "sheet": "Sheet2", "at_mm": [300, 20],
            "height_mm": 5, "bold": true, "angle_deg": 90, "border": "box", "justification": "center"}))
        .unwrap();
        assert_eq!(r.owner(), proto::AnnotationOwner::SheetFormat);
        let s = r.style.unwrap();
        assert_eq!((s.text_height, s.bold, s.border, s.justification), (Some(0.005), Some(true), Some(4), Some(2)));
        assert!((s.angle.unwrap() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        // Refused: coordinates of the wrong frame, a view for a sheet note,
        // a view note without a view, both positions, a leader to nothing.
        for bad in [
            json!({"text": "A", "at_view_mm": [1, 2]}),
            json!({"text": "A", "in": "sheet", "view": "Drawing View1"}),
            json!({"text": "A", "in": "view"}),
            json!({"text": "A", "view": "V", "at_mm": [1, 2], "at_view_mm": [1, 2]}),
            json!({"text": "A", "leader": "straight"}),
            json!({"text": " "}),
            json!({"text": "A", "arrow_at_mm": [[1, 2]]}),
            json!({"text": "A", "attach_to": ["face:1"]}),
        ] {
            assert!(add(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn edit_requests_need_one_position_and_a_change() {
        let ids = IdRegistry::default();
        let cache = Cache::default();
        let edit = |v: serde_json::Value| edit_request(&ids, &cache, &serde_json::from_value(v).unwrap());
        assert!(edit(json!({"name": "DetailItem3"})).is_err());
        assert!(edit(json!({"name": "DetailItem3", "at_mm": [1, 2], "move_by_mm": [1, 0]})).is_err());
        assert!(edit(json!({"name": "DetailItem3", "at_view_mm": [1, 2]})).is_err());
        assert!(edit(json!({"name": "DetailItem3", "document_font": true, "bold": true})).is_err());
        assert!(edit(json!({"name": "DetailItem3", "document_font": true, "angle_deg": 15})).is_err());
        let (r, _) = edit(json!({"name": " DetailItem3 ", "move_by_mm": [10, -5]})).unwrap();
        assert_eq!(r.name, "DetailItem3");
        assert_eq!(r.move_by, Some(proto::Vec3 { x: 0.01, y: -0.005, z: 0.0 }));
        let (r, _) = edit(json!({"name": "N", "view": "Drawing View1", "at_view_mm": [0, 25], "arrow": "dot"})).unwrap();
        assert_eq!(r.position.unwrap().view_name, "Drawing View1");
        assert_eq!(r.arrow, Some(3));
        let (r, _) = edit(json!({"name": "N", "layer": "", "attach_to_view": "Drawing View2"})).unwrap();
        assert_eq!((r.layer.as_deref(), r.attach_to_view.as_str()), (Some(""), "Drawing View2"));
    }

    fn note(owner: proto::AnnotationOwner, owner_name: &str, lo: [f64; 2], hi: [f64; 2]) -> proto::AnnotationInfo {
        proto::AnnotationInfo {
            name: "DetailItem9".into(),
            r#type: 6,
            text: vec!["HELLO".into()],
            owner: owner as i32,
            owner_name: owner_name.into(),
            position: Some(proto::Vec3 { x: lo[0], y: hi[1], z: 0.0 }),
            note: Some(proto::NoteInfo {
                text_height: 0.0035,
                document_font: true,
                justification: 1,
                smart_arrows: true,
                extent: Some(proto::Box3 {
                    min: Some(proto::Vec3 { x: lo[0], y: lo[1], z: 0.0 }),
                    max: Some(proto::Vec3 { x: hi[0], y: hi[1], z: 0.0 }),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn sheet() -> proto::SheetInfo {
        proto::SheetInfo {
            name: "Sheet1".into(),
            width: 0.42,
            height: 0.297,
            views: vec![view("Drawing View1", [0.12, 0.15], 1.0, 0.0)],
            ..Default::default()
        }
    }

    #[test]
    fn warnings_catch_misplaced_notes() {
        let s = sheet();
        // A sheet note over a view doesn't follow it.
        let w = placement_warnings(&s, &note(proto::AnnotationOwner::Sheet, "Sheet1", [0.11, 0.14], [0.13, 0.145]));
        assert!(w.len() == 1 && w[0].contains("over Drawing View1 but belongs to the sheet"), "{w:?}");
        // A view note far from its view.
        let w = placement_warnings(&s, &note(proto::AnnotationOwner::View, "Drawing View1", [0.3, 0.25], [0.32, 0.255]));
        assert!(w.len() == 1 && w[0].contains("sits 1"), "{w:?}");
        // Off the sheet.
        let w = placement_warnings(&s, &note(proto::AnnotationOwner::Sheet, "Sheet1", [0.41, 0.01], [0.44, 0.02]));
        assert!(w.len() == 1 && w[0].contains("past the sheet's edges"), "{w:?}");
        // Fine.
        let w = placement_warnings(&s, &note(proto::AnnotationOwner::View, "Drawing View1", [0.1, 0.18], [0.12, 0.185]));
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn entries_show_both_frames_and_the_owner() {
        let mut ids = IdRegistry::default();
        let s = sheet();
        let mut a = note(proto::AnnotationOwner::View, "Drawing View1", [0.1, 0.18], [0.12, 0.185]);
        a.leader_style = 2049;
        a.leaders = vec![proto::AnnotationLeader {
            points: vec![proto::Vec3 { x: 0.1, y: 0.18, z: 0.0 }, proto::Vec3 { x: 0.11, y: 0.16, z: 0.0 }],
            arrow: 1,
        }];
        let e = annotation_entry(&mut ids, &frames(&s), &a);
        assert_eq!(e["in"], json!("view Drawing View1"));
        assert_eq!(e["at_mm"], json!([100.0, 185.0]));
        assert_eq!(e["at_view_mm"], json!([-20.0, 35.0]));
        assert_eq!(e["leader"], json!("straight"));
        assert_eq!(e["leaders"][0]["arrow_at_view_mm"], json!([-10.0, 10.0]));
        assert_eq!(e["leaders"][0]["arrow"], json!("smart"));
        assert_eq!(e["style"], json!({"font": "document", "height_mm": 3.5}));
        assert!(e.get("position").is_none() && e.get("imported").is_none(), "{e}");

        let reply = proto::SetupDrawingAnnotationResponse {
            annotation: Some(a),
            created: true,
            sheet: Some(s),
            active_sheet: "Sheet1".into(),
            ..Default::default()
        };
        let text = setup_reply(&mut ids, &proto::SetupDrawingAnnotationRequest::default(), &reply, &[]);
        assert!(
            text.starts_with(
                "added note DetailItem9 \"HELLO\" in Drawing View1 on Sheet1 (it moves with the view): text's \
                 upper-left corner at [100, 185] sheet mm ([-20, 35] mm in Drawing View1's coordinates)"
            ),
            "{text}"
        );
        assert!(text.contains("1 straight leader(s), arrows at [110, 160] mm"), "{text}");
    }
}
