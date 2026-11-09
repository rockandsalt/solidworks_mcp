//! Views made from other views and views' shapes: create_drawing_view's
//! projected, section, detail and auxiliary views over the DeriveDrawingView
//! RPC, crop_view and break_view over ShapeDrawingView.
//! Everything is sheet meters on the wire; which way a view looks and where
//! it goes is worked out here (docs/notes/drawings.md, "Derived views").

use glam::DVec2;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::addin::proto;
use crate::drawing::{self, Cache, Shape, sheet_mm};
use crate::inspect::{IdRegistry, MM_PER_M};
use crate::view::{self, Rect, find_view};

/// How far a section line reaches past what it crosses of the view's outline.
const LINE_OVERHANG: f64 = 0.003;
/// A straight edge this near a sheet axis gets a projected view, not an
/// auxiliary one: about 0.06 degrees.
const AXIS_TOLERANCE: f64 = 1e-3;
/// A view this far from where it was asked to go didn't go there.
const AT_SLACK: f64 = 0.001;
/// Letters drawings leave out, being too like digits and each other.
const SKIPPED_LETTERS: [char; 3] = ['I', 'O', 'Q'];

pub const BROKEN_NOTE: &str = "the view is shown broken: its entities' positions are where they'd be unbroken, \
    outline and render_drawing show it as it is; dimensions keep true lengths";

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

fn sheet_m(at: [f64; 2], what: &str) -> Result<DVec2, McpError> {
    if at.iter().all(|c| c.is_finite()) {
        Ok(DVec2::new(at[0], at[1]) / MM_PER_M)
    } else {
        Err(invalid(format!("{what}: give [x, y] in sheet mm")))
    }
}

fn vec3(p: DVec2) -> Option<proto::Vec3> {
    Some(proto::Vec3 { x: p.x, y: p.y, z: 0.0 })
}

/// Where a view goes, relative to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Right,
    Left,
    Above,
    Below,
}

impl Side {
    fn direction(self) -> DVec2 {
        match self {
            Side::Right => DVec2::X,
            Side::Left => -DVec2::X,
            Side::Above => DVec2::Y,
            Side::Below => -DVec2::Y,
        }
    }
}

/// The outline drawn around a detail or crop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outline {
    /// The circle or rectangle itself.
    Profile,
    /// Detail views only: SolidWorks' "Full outline".
    Full,
    Jagged,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DetailStyle {
    Standard,
    Broken,
    Leader,
    NoLeader,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BreakStyle {
    Straight,
    Zigzag,
    Curve,
    SmallZigzag,
    Jagged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BreakAxis {
    X,
    Y,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ProjectedViewParams {
    /// The view to project from, e.g. "Drawing View1".
    pub view: String,
    /// Where it goes relative to `view`. Default: right.
    #[serde(default)]
    pub side: Option<Side>,
    /// Its center, [x, y] sheet mm, instead of `side`. It stays aligned with `view`.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// The view's name. Default: SolidWorks' next "Drawing ViewN".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SectionViewParams {
    /// The view to cut, e.g. "Drawing View1".
    pub view: String,
    /// A view entity for the line to go through, from inspect_drawing with the view: a
    /// circle's center, a line's middle. Not with through_mm.
    #[serde(default)]
    pub through: Option<String>,
    /// A point for the line to go through, [x, y] sheet mm. Default: the view's middle.
    #[serde(default)]
    pub through_mm: Option<[f64; 2]>,
    /// The line's angle from the sheet's x axis, degrees. Default: 90, a vertical line; 0 is
    /// horizontal.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Where the section view goes relative to the line. Default: the side with more room.
    #[serde(default)]
    pub side: Option<Side>,
    /// Its center, [x, y] sheet mm, instead of `side`. It stays aligned with the cut.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// Look the other way than the sheet's projection says for where the view is.
    #[serde(default)]
    pub flip: bool,
    /// The letter on the sheet, which also names the view ("Section View A-A"; SolidWorks
    /// won't rename a section view). Default: the next one free.
    #[serde(default)]
    pub label: Option<String>,
    /// Show only the faces the cut makes, nothing behind them.
    #[serde(default)]
    pub surface_only: bool,
    /// Show only this far behind the cut, in model mm.
    #[serde(default)]
    pub depth_mm: Option<f64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DetailViewParams {
    /// The view to take the detail from, e.g. "Drawing View1".
    pub view: String,
    /// A view entity at the circle's center, from inspect_drawing with the view. Not with
    /// center_mm.
    #[serde(default)]
    pub center: Option<String>,
    /// The circle's center, [x, y] sheet mm.
    #[serde(default)]
    pub center_mm: Option<[f64; 2]>,
    /// The circle's radius as it measures on the sheet, mm.
    pub radius_mm: f64,
    /// "4:1", ... Default: twice the parent's.
    #[serde(default)]
    pub scale: Option<String>,
    /// The view's center, [x, y] sheet mm. Default: a free spot.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// How the circle ties to the view. Default: standard.
    #[serde(default)]
    pub style: Option<DetailStyle>,
    /// Default: profile.
    #[serde(default)]
    pub outline: Option<Outline>,
    /// The letter on the sheet, which also names the view ("Detail View A (2 : 1)"). Default:
    /// the next one free.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AuxiliaryViewParams {
    /// The view to make it from, e.g. "Drawing View1".
    pub view: String,
    /// A straight edge of `view` from inspect_drawing, e.g. "view_edge:3".
    pub edge: String,
    /// Look from the edge's other side.
    #[serde(default)]
    pub flip: bool,
    /// The view's center, [x, y] sheet mm. It stays aligned with the edge's normal.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// The letter on the sheet. Default: the next one free.
    #[serde(default)]
    pub label: Option<String>,
    /// The view's name. Default: SolidWorks' next "Drawing ViewN".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CropViewParams {
    /// The view to crop, e.g. "Drawing View1".
    pub view: String,
    /// A crop circle's center, [x, y] sheet mm.
    #[serde(default)]
    pub circle_center_mm: Option<[f64; 2]>,
    /// A crop circle's radius as it measures on the sheet, mm.
    #[serde(default)]
    pub circle_radius_mm: Option<f64>,
    /// A crop rectangle's opposite corners, [[x1, y1], [x2, y2]] sheet mm.
    #[serde(default)]
    pub rectangle_mm: Option<[[f64; 2]; 2]>,
    /// profile (default), jagged or none.
    #[serde(default)]
    pub outline: Option<Outline>,
    /// Take the crop off instead.
    #[serde(default)]
    pub remove: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BreakRange {
    /// The sheet axis the hidden range runs along: x for a part lying across the sheet.
    pub along: BreakAxis,
    /// Where it starts and ends along that axis, sheet mm.
    pub from_mm: f64,
    pub to_mm: f64,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct BreakViewParams {
    /// The view to break, e.g. "Drawing View1".
    pub view: String,
    /// The ranges to hide.
    #[serde(default)]
    pub breaks: Vec<BreakRange>,
    /// Default: zigzag.
    #[serde(default)]
    pub style: Option<BreakStyle>,
    /// Take all the view's breaks off instead.
    #[serde(default)]
    pub remove: bool,
}

/// What create_drawing_view adds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    /// Of a part or assembly: `model` seen from `orientation`.
    #[default]
    Model,
    /// Projected from `view` to a `side` of it, aligned, following its scale and display;
    /// which face it shows follows the sheet's projection (first / third angle).
    Projected,
    /// `view` cut along a straight line (through `through` or through_mm, at angle_deg), seen
    /// from one side, hatched, with the A-A arrows. The `side` it goes to sets which way it
    /// looks by the projection (third angle, to the right: it looks left); `flip` the other
    /// way. Cuts every component of an assembly.
    Section,
    /// A circle (center or center_mm, radius_mm) of `view` shown bigger, the circle and label
    /// on the parent.
    Detail,
    /// `view` seen square onto an inclined straight `edge`, showing a face at its true shape.
    Auxiliary,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DrawingViewParams {
    #[serde(default, rename = "type")]
    pub kind: ViewKind,
    /// model: an absolute .SLDPRT / .SLDASM path or the name of an open one; it must be
    /// saved. Defaults to what the drawing shows, else the one open part or assembly.
    #[serde(default)]
    pub model: Option<String>,
    /// model: defaults to the model's active one.
    #[serde(default)]
    pub configuration: Option<String>,
    /// model: front, back, left, right, top, bottom, isometric, dimetric, trimetric or a view
    /// saved in the model. Defaults to front.
    #[serde(default)]
    pub orientation: Option<String>,
    /// model: defaults to SolidWorks' (normally hidden_lines_removed).
    #[serde(default)]
    pub display: Option<view::Display>,
    /// model: the sheet to put it on. Defaults to the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// projected, section, detail, auxiliary: the parent view, e.g. "Drawing View1".
    #[serde(default)]
    pub view: Option<String>,
    /// projected (default right), section (default: the side with more room).
    #[serde(default)]
    pub side: Option<Side>,
    /// The view's center, [x, y] sheet mm from the lower-left corner; a projected, section or
    /// auxiliary view stays aligned with its parent. Defaults to a free spot.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// model: "1:2", ... or "sheet" (default: the sheet's; an empty sheet may first be
    /// rescaled to fit); detail: defaults to twice the parent's.
    #[serde(default)]
    pub scale: Option<String>,
    /// section: a view entity id the line goes through (a circle's center, a line's middle).
    #[serde(default)]
    pub through: Option<String>,
    /// section: a point the line goes through, [x, y] sheet mm. Defaults to the view's middle.
    #[serde(default)]
    pub through_mm: Option<[f64; 2]>,
    /// section: the line's angle from the sheet's x axis. Defaults to 90 (vertical).
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// section, auxiliary: look the other way.
    #[serde(default)]
    pub flip: bool,
    /// section: show only the cut faces.
    #[serde(default)]
    pub surface_only: bool,
    /// section: show only this far behind the cut, model mm.
    #[serde(default)]
    pub depth_mm: Option<f64>,
    /// detail: a view entity id at the circle's center.
    #[serde(default)]
    pub center: Option<String>,
    /// detail: the circle's center, [x, y] sheet mm.
    #[serde(default)]
    pub center_mm: Option<[f64; 2]>,
    /// detail: the circle's radius on the sheet.
    #[serde(default)]
    pub radius_mm: Option<f64>,
    /// detail: how the circle ties to the view. Defaults to standard.
    #[serde(default)]
    pub style: Option<DetailStyle>,
    /// detail: profile (default: the circle), full, jagged or none.
    #[serde(default)]
    pub outline: Option<Outline>,
    /// auxiliary: a straight edge of `view` from inspect_drawing ("view_edge:3"), not along
    /// the sheet's axes.
    #[serde(default)]
    pub edge: Option<String>,
    /// section, detail, auxiliary: the letter on the sheet, which also names a section or
    /// detail view ("Section View A-A"). Defaults to the next one free.
    #[serde(default)]
    pub label: Option<String>,
    /// model, projected, auxiliary. Defaults to SolidWorks' next "Drawing ViewN".
    #[serde(default)]
    pub name: Option<String>,
}

/// The create_drawing_view parameters for one kind of view.
pub enum ViewRequest {
    Model(view::CreateDrawingViewParams),
    Projected(ProjectedViewParams),
    Section(SectionViewParams),
    Detail(DetailViewParams),
    Auxiliary(AuxiliaryViewParams),
}

impl DrawingViewParams {
    /// Splits the parameters by kind, refusing those the kind doesn't take.
    pub fn split(self) -> Result<ViewRequest, McpError> {
        use ViewKind::*;
        let key = match self.kind {
            Model => "model",
            Projected => "projected",
            Section => "section",
            Detail => "detail",
            Auxiliary => "auxiliary",
        };
        let given: [(&str, bool, &[ViewKind]); 22] = [
            ("model", self.model.is_some(), &[Model]),
            ("configuration", self.configuration.is_some(), &[Model]),
            ("orientation", self.orientation.is_some(), &[Model]),
            ("display", self.display.is_some(), &[Model]),
            ("sheet", self.sheet.is_some(), &[Model]),
            ("view", self.view.is_some(), &[Projected, Section, Detail, Auxiliary]),
            ("side", self.side.is_some(), &[Projected, Section]),
            ("at_mm", self.at_mm.is_some(), &[Model, Projected, Section, Detail, Auxiliary]),
            ("scale", self.scale.is_some(), &[Model, Detail]),
            ("through", self.through.is_some(), &[Section]),
            ("through_mm", self.through_mm.is_some(), &[Section]),
            ("angle_deg", self.angle_deg.is_some(), &[Section]),
            ("flip", self.flip, &[Section, Auxiliary]),
            ("surface_only", self.surface_only, &[Section]),
            ("depth_mm", self.depth_mm.is_some(), &[Section]),
            ("center", self.center.is_some(), &[Detail]),
            ("center_mm", self.center_mm.is_some(), &[Detail]),
            ("radius_mm", self.radius_mm.is_some(), &[Detail]),
            ("style", self.style.is_some(), &[Detail]),
            ("outline", self.outline.is_some(), &[Detail]),
            ("edge", self.edge.is_some(), &[Auxiliary]),
            ("label", self.label.is_some(), &[Section, Detail, Auxiliary]),
        ];
        if let Some((name, _, _)) = given.iter().find(|(_, given, kinds)| *given && !kinds.contains(&self.kind)) {
            return Err(invalid(format!("type \"{key}\" doesn't take {name}")));
        }
        if self.name.is_some() && matches!(self.kind, Section | Detail) {
            return Err(invalid(format!(
                "type \"{key}\" doesn't take name: its label names it (SolidWorks won't rename it)"
            )));
        }
        let needs = |value: Option<String>, name: &str| {
            value.ok_or_else(|| invalid(format!("type \"{key}\" needs {name}")))
        };
        Ok(match self.kind {
            Model => ViewRequest::Model(view::CreateDrawingViewParams {
                model: self.model,
                configuration: self.configuration,
                orientation: self.orientation,
                at_mm: self.at_mm,
                scale: self.scale,
                display: self.display,
                sheet: self.sheet,
                name: self.name,
            }),
            Projected => ViewRequest::Projected(ProjectedViewParams {
                view: needs(self.view, "view")?,
                side: self.side,
                at_mm: self.at_mm,
                name: self.name,
            }),
            Section => ViewRequest::Section(SectionViewParams {
                view: needs(self.view, "view")?,
                through: self.through,
                through_mm: self.through_mm,
                angle_deg: self.angle_deg,
                side: self.side,
                at_mm: self.at_mm,
                flip: self.flip,
                label: self.label,
                surface_only: self.surface_only,
                depth_mm: self.depth_mm,
            }),
            Detail => ViewRequest::Detail(DetailViewParams {
                view: needs(self.view, "view")?,
                center: self.center,
                center_mm: self.center_mm,
                radius_mm: self.radius_mm.ok_or_else(|| invalid("type \"detail\" needs radius_mm"))?,
                scale: self.scale,
                at_mm: self.at_mm,
                style: self.style,
                outline: self.outline,
                label: self.label,
            }),
            Auxiliary => ViewRequest::Auxiliary(AuxiliaryViewParams {
                view: needs(self.view, "view")?,
                edge: needs(self.edge, "edge")?,
                flip: self.flip,
                at_mm: self.at_mm,
                label: self.label,
                name: self.name,
            }),
        })
    }
}

// ---- What the drawing looks like before the call ------------------------------

fn parent_of<'a>(
    drawing: &'a proto::InspectDrawingResponse,
    name: &str,
) -> Result<(&'a proto::SheetInfo, &'a proto::ViewInfo), McpError> {
    if name.is_empty() {
        return Err(invalid("view: name the view to make it from, e.g. \"Drawing View1\""));
    }
    for sheet in &drawing.sheets {
        if let Some(view) = find_view(sheet, name) {
            return Ok((sheet, view));
        }
    }
    let known: Vec<&str> = drawing.sheets.iter().flat_map(|s| s.views.iter().map(|v| v.name.as_str())).collect();
    Err(invalid(format!("the drawing has no view named {name:?} (its views: {})", known.join(", "))))
}

/// The letter after those the drawing's section and detail views use.
fn next_label(drawing: &proto::InspectDrawingResponse) -> String {
    let used: Vec<&str> = drawing.sheets.iter().flat_map(|s| s.views.iter().map(|v| v.label.as_str())).collect();
    let letters: Vec<char> = ('A'..='Z').filter(|c| !SKIPPED_LETTERS.contains(c)).collect();
    let singles = letters.iter().map(char::to_string);
    let doubles = letters.iter().flat_map(|a| letters.iter().map(move |b| format!("{a}{b}")));
    singles.chain(doubles).find(|l| !used.contains(&l.as_str())).unwrap_or_default()
}

fn label_or_next(drawing: &proto::InspectDrawingResponse, label: Option<&str>) -> Result<String, McpError> {
    let label = text(label);
    if label.is_empty() {
        return Ok(next_label(drawing));
    }
    let taken = drawing.sheets.iter().flat_map(|s| &s.views).find(|v| v.label.eq_ignore_ascii_case(&label));
    match taken {
        Some(view) => Err(invalid(format!("label: {} already uses {label:?}", view.name))),
        None => Ok(label),
    }
}

/// A point on the parent's outline, past which a view of about the same
/// size would clear it: only a start, the view is moved clear afterwards.
fn past_outline(parent: &Rect, direction: DVec2) -> DVec2 {
    let size = parent.hi - parent.lo;
    parent.center() + direction * (size.x * direction.x.abs() + size.y * direction.y.abs())
}

/// How much sheet there is beyond the outline along `direction`.
fn room(sheet: &proto::SheetInfo, parent: &Rect, direction: DVec2) -> f64 {
    let along = |lo: f64, hi: f64, limit: f64, d: f64| {
        if d > 1e-9 {
            (limit - hi) / d
        } else if d < -1e-9 {
            lo / -d
        } else {
            f64::INFINITY
        }
    };
    along(parent.lo.x, parent.hi.x, sheet.width, direction.x)
        .min(along(parent.lo.y, parent.hi.y, sheet.height, direction.y))
}

fn entity_point(cache: &Cache, id: &str, view: &str, what: &str) -> Result<DVec2, McpError> {
    let id = id.trim();
    let Some(geom) = cache.entity(id) else {
        return Err(invalid(format!(
            "{what}: there's no current position for {id:?} -- call inspect_drawing with view={view:?} for ids"
        )));
    };
    if geom.view != view {
        return Err(invalid(format!("{what}: {id} is in {}, not {view}", geom.view)));
    }
    cache.entity_anchor(id).ok_or_else(|| invalid(format!("{what}: {id} has no position")))
}

/// A view to make, and how to finish it after the RPC.
pub struct Plan {
    pub request: proto::DeriveDrawingViewRequest,
    /// Asked for at this place, sheet meters: left there (or as near as its
    /// alignment lets it).
    at: Option<DVec2>,
    placement: Placement,
    remarks: Vec<String>,
}

enum Placement {
    /// Slid along this direction from the parent.
    Along(DVec2),
    /// Anywhere free.
    Free,
    /// Along the direction it turned out to look, by the sheet's projection.
    AsSeen,
}

impl Plan {
    /// Where to move the new view so it's clear of the others; None to leave
    /// it. Only when the caller didn't place it.
    pub fn clear_spot(&self, sheet: &proto::SheetInfo, name: &str) -> Option<DVec2> {
        if self.at.is_some() {
            return None;
        }
        let parent = &self.request.view_name;
        match self.placement {
            Placement::Free => view::free_spot(sheet, name),
            Placement::Along(direction) => view::slide_clear(sheet, name, parent, direction),
            Placement::AsSeen => {
                let direction = seen_side(sheet, find_view(sheet, name)?, find_view(sheet, parent)?)?;
                view::slide_clear(sheet, name, parent, direction)
            }
        }
    }

    /// The reply text with what only this call knows added.
    pub fn finish_text(&self, mut text: String, reply: &proto::SetupDrawingViewResponse) -> String {
        let mut notes = self.remarks.clone();
        if let (Some(at), Some(sheet)) = (self.at, reply.sheet.as_ref())
            && let Some(rect) = find_view(sheet, &reply.view_name).and_then(Rect::of)
            && rect.center().distance(at) > AT_SLACK
        {
            notes.push(format!(
                "its middle is at {} mm, not {} mm: it stays aligned with {}",
                sheet_mm(rect.center()),
                sheet_mm(at),
                self.request.view_name
            ));
        }
        if !notes.is_empty() {
            text += &format!("\nNote: {}.", notes.join("; "));
        }
        text
    }
}

/// Which way from the parent the new view sits under the sheet's projection,
/// from what it looks at: third angle puts a view on the side it's seen from.
fn seen_side(sheet: &proto::SheetInfo, new: &proto::ViewInfo, parent: &proto::ViewInfo) -> Option<DVec2> {
    let (new, parent) = (drawing::ViewXform::of(new)?, drawing::ViewXform::of(parent)?);
    let toward_viewer = new.toward_viewer_in_model();
    let seen_from = parent.direction_on_sheet(toward_viewer).try_normalize()?;
    Some(if sheet.first_angle { -seen_from } else { seen_from })
}

fn new_request(view: &str, name: Option<&str>, label: String, position: DVec2) -> proto::DeriveDrawingViewRequest {
    proto::DeriveDrawingViewRequest {
        view_name: view.to_string(),
        new_name: text(name),
        label,
        position: vec3(position),
        ..Default::default()
    }
}

// ---- Projected ----------------------------------------------------------------

pub fn projected_plan(
    drawing: &proto::InspectDrawingResponse,
    params: &ProjectedViewParams,
) -> Result<Plan, McpError> {
    let view = params.view.trim();
    let (_, parent) = parent_of(drawing, view)?;
    let rect = Rect::of(parent).ok_or_else(|| invalid(format!("{view} has no outline")))?;
    if params.at_mm.is_some() && params.side.is_some() {
        return Err(invalid("give side or at_mm, not both"));
    }
    let at = params.at_mm.map(|at| sheet_m(at, "at_mm")).transpose()?;
    let direction = params.side.unwrap_or(Side::Right).direction();
    let position = at.unwrap_or_else(|| past_outline(&rect, direction));
    let mut request = new_request(view, params.name.as_deref(), String::new(), position);
    request.kind = Some(proto::derive_drawing_view_request::Kind::Projected(proto::ProjectedViewSpec {}));
    Ok(Plan { request, at, placement: Placement::Along(direction), remarks: Vec::new() })
}

// ---- Section ------------------------------------------------------------------

/// The line's two ends over `rect` through `through` along `along`, in the
/// order that makes a view look `look`: SolidWorks looks along the line's
/// direction turned 90 degrees clockwise.
fn section_line(rect: &Rect, through: DVec2, along: DVec2, look: DVec2) -> (DVec2, DVec2) {
    // Turned counterclockwise from where it looks.
    let forward = DVec2::new(-look.y, look.x);
    let along = if along.dot(forward) < 0.0 { -along } else { along };
    let corners = [rect.lo, rect.hi, DVec2::new(rect.lo.x, rect.hi.y), DVec2::new(rect.hi.x, rect.lo.y)];
    let reach = |sign: f64| corners.iter().map(|c| (*c - through).dot(along) * sign).fold(0.0, f64::max) + LINE_OVERHANG;
    (through - along * reach(-1.0), through + along * reach(1.0))
}

pub fn section_plan(
    drawing: &proto::InspectDrawingResponse,
    ids: &IdRegistry,
    cache: &Cache,
    params: &SectionViewParams,
) -> Result<Plan, McpError> {
    use proto::derive_drawing_view_request::Kind;
    let view = params.view.trim();
    let (sheet, parent) = parent_of(drawing, view)?;
    let rect = Rect::of(parent).ok_or_else(|| invalid(format!("{view} has no outline")))?;
    if params.through.is_some() && params.through_mm.is_some() {
        return Err(invalid("give through or through_mm, not both"));
    }
    if params.at_mm.is_some() && params.side.is_some() {
        return Err(invalid("give side or at_mm, not both"));
    }
    if let Some(depth) = params.depth_mm
        && !(depth.is_finite() && depth > 0.0)
    {
        return Err(invalid("depth_mm must be above 0"));
    }
    let through = match (&params.through, params.through_mm) {
        (Some(id), _) => {
            drawing::decode_ref(ids, id)?;
            entity_point(cache, id, view, "through")?
        }
        (None, Some(at)) => sheet_m(at, "through_mm")?,
        (None, None) => rect.center(),
    };
    let angle = params.angle_deg.unwrap_or(90.0);
    if !angle.is_finite() {
        return Err(invalid("angle_deg must be a number"));
    }
    let along = DVec2::from_angle(angle.to_radians());
    let normal = DVec2::new(along.y, -along.x);
    let at = params.at_mm.map(|at| sheet_m(at, "at_mm")).transpose()?;
    // Which side of the line the view goes.
    let wanted = match (at, params.side) {
        (Some(at), _) => (at - through).dot(normal),
        (None, Some(side)) => {
            let d = side.direction().dot(normal);
            if d.abs() < 1e-6 {
                return Err(invalid(format!(
                    "side: a line at {angle} degrees has nothing {side:?} of it -- pick a side across the line"
                )));
            }
            d
        }
        // The side with more room; along +normal when level.
        (None, None) => room(sheet, &rect, normal) - room(sheet, &rect, -normal) + 1e-9,
    };
    let goes = if wanted >= 0.0 { normal } else { -normal };
    // Third angle: a view to the right looks left, from its side at the
    // parent. First angle: the other way round.
    let mut look = if sheet.first_angle { goes } else { -goes };
    if params.flip {
        look = -look;
    }
    let (start, end) = section_line(&rect, through, along, look);
    let position = at.unwrap_or_else(|| past_outline(&rect, goes));
    let mut request = new_request(view, None, label_or_next(drawing, params.label.as_deref())?, position);
    request.kind = Some(Kind::Section(proto::SectionViewSpec {
        start: vec3(start),
        end: vec3(end),
        surface_only: params.surface_only,
        depth: params.depth_mm.unwrap_or(0.0) / MM_PER_M,
    }));
    let mut remarks = Vec::new();
    let inside = through.x >= rect.lo.x && through.x <= rect.hi.x && through.y >= rect.lo.y && through.y <= rect.hi.y;
    if !inside {
        remarks.push(format!(
            "the line goes through {} mm, outside {view}'s outline: it may cut nothing",
            sheet_mm(through)
        ));
    }
    Ok(Plan { request, at, placement: Placement::Along(goes), remarks })
}

// ---- Detail -------------------------------------------------------------------

pub fn detail_plan(
    drawing: &proto::InspectDrawingResponse,
    cache: &Cache,
    params: &DetailViewParams,
) -> Result<Plan, McpError> {
    use proto::derive_drawing_view_request::Kind;
    let view = params.view.trim();
    let (_, parent) = parent_of(drawing, view)?;
    let rect = Rect::of(parent).ok_or_else(|| invalid(format!("{view} has no outline")))?;
    if !(params.radius_mm.is_finite() && params.radius_mm > 0.0) {
        return Err(invalid("radius_mm must be above 0"));
    }
    let center = match (&params.center, params.center_mm) {
        (Some(_), Some(_)) => return Err(invalid("give center or center_mm, not both")),
        (Some(id), None) => entity_point(cache, id, view, "center")?,
        (None, Some(at)) => sheet_m(at, "center_mm")?,
        (None, None) => return Err(invalid("say where the detail is: center or center_mm")),
    };
    let scale = match params.scale.as_deref().map(str::trim) {
        Some(s) => {
            let (numerator, denominator) = crate::sheet::parse_scale(s)?;
            proto::ViewScale { numerator, denominator }
        }
        None => proto::ViewScale {
            numerator: parent.scale_numerator * 2.0,
            denominator: parent.scale_denominator,
        },
    };
    let style = match params.style.unwrap_or(DetailStyle::Standard) {
        DetailStyle::Standard => 0,
        DetailStyle::Broken => 1,
        DetailStyle::Leader => 2,
        DetailStyle::NoLeader => 3,
        DetailStyle::Connected => 4,
    };
    let outline = params.outline.unwrap_or(Outline::Profile);
    let at = params.at_mm.map(|at| sheet_m(at, "at_mm")).transpose()?;
    let position = at.unwrap_or_else(|| past_outline(&rect, DVec2::X));
    let mut request = new_request(view, None, label_or_next(drawing, params.label.as_deref())?, position);
    request.kind = Some(Kind::Detail(proto::DetailViewSpec {
        center: vec3(center),
        radius: params.radius_mm / MM_PER_M,
        scale: Some(scale),
        style,
        full_outline: outline == Outline::Full,
        jagged_outline: outline == Outline::Jagged,
        no_outline: outline == Outline::None,
    }));
    Ok(Plan { request, at, placement: Placement::Free, remarks: Vec::new() })
}

// ---- Auxiliary ----------------------------------------------------------------

pub fn auxiliary_plan(
    drawing: &proto::InspectDrawingResponse,
    ids: &IdRegistry,
    cache: &Cache,
    params: &AuxiliaryViewParams,
) -> Result<Plan, McpError> {
    use proto::derive_drawing_view_request::Kind;
    let view = params.view.trim();
    let (_, parent) = parent_of(drawing, view)?;
    let rect = Rect::of(parent).ok_or_else(|| invalid(format!("{view} has no outline")))?;
    let edge = drawing::decode_ref(ids, &params.edge)?;
    if edge.view_name != view {
        return Err(invalid(format!("{} is in {}, not {view}", params.edge.trim(), edge.view_name)));
    }
    let Some(geom) = cache.entity(&params.edge) else {
        return Err(invalid(format!("there's no current position for {} -- inspect the view again", params.edge.trim())));
    };
    let Shape::Line { a, b } = geom.shape else {
        return Err(invalid(format!("{} is a {}: an auxiliary view needs a straight edge", params.edge.trim(), geom.label)));
    };
    let along = (b - a).try_normalize().ok_or_else(|| invalid("that edge is seen end-on"))?;
    if along.x.abs().min(along.y.abs()) < AXIS_TOLERANCE {
        return Err(invalid(format!(
            "{} runs along the sheet's axes: use a projected view (create_drawing_view type \"projected\") for a view square onto it",
            params.edge.trim()
        )));
    }
    let middle = (a + b) / 2.0;
    let normal = DVec2::new(along.y, -along.x);
    let outward = if (middle - rect.center()).dot(normal) >= 0.0 { normal } else { -normal };
    let at = params.at_mm.map(|at| sheet_m(at, "at_mm")).transpose()?;
    let position = at.unwrap_or_else(|| past_outline(&rect, outward));
    let mut request = new_request(view, params.name.as_deref(), label_or_next(drawing, params.label.as_deref())?, position);
    request.kind = Some(Kind::Auxiliary(proto::AuxiliaryViewSpec { edge: Some(edge), flip: params.flip }));
    Ok(Plan { request, at, placement: Placement::AsSeen, remarks: Vec::new() })
}

// ---- Crop and break -----------------------------------------------------------

pub fn crop_request(params: &CropViewParams) -> Result<proto::ShapeDrawingViewRequest, McpError> {
    use proto::shape_drawing_view_request::Change;
    let view_name = params.view.trim().to_string();
    if view_name.is_empty() {
        return Err(invalid("view: name the view to crop"));
    }
    let circle = params.circle_center_mm.is_some() || params.circle_radius_mm.is_some();
    let rectangle = params.rectangle_mm.is_some();
    if params.remove {
        if circle || rectangle {
            return Err(invalid("remove takes the crop off: give no shape with it"));
        }
        return Ok(proto::ShapeDrawingViewRequest { view_name, change: Some(Change::RemoveCrop(true)) });
    }
    if circle && rectangle {
        return Err(invalid("give a circle or a rectangle, not both"));
    }
    let outline = params.outline.unwrap_or(Outline::Profile);
    if outline == Outline::Full {
        return Err(invalid("outline \"full\" is for detail views: profile, jagged or none"));
    }
    let mut spec = proto::CropViewSpec {
        jagged_outline: outline == Outline::Jagged,
        no_outline: outline == Outline::None,
        ..Default::default()
    };
    if let Some([a, b]) = params.rectangle_mm {
        let (a, b) = (sheet_m(a, "rectangle_mm")?, sheet_m(b, "rectangle_mm")?);
        if (a.x - b.x).abs() < 1e-6 || (a.y - b.y).abs() < 1e-6 {
            return Err(invalid("rectangle_mm: give two opposite corners of a rectangle with some size"));
        }
        spec.corner1 = vec3(a);
        spec.corner2 = vec3(b);
    } else {
        let (Some(center), Some(radius)) = (params.circle_center_mm, params.circle_radius_mm) else {
            return Err(invalid(
                "say what to crop to: circle_center_mm and circle_radius_mm, or rectangle_mm (or remove)",
            ));
        };
        if !(radius.is_finite() && radius > 0.0) {
            return Err(invalid("circle_radius_mm must be above 0"));
        }
        spec.center = vec3(sheet_m(center, "circle_center_mm")?);
        spec.radius = radius / MM_PER_M;
    }
    Ok(proto::ShapeDrawingViewRequest { view_name, change: Some(Change::Crop(spec)) })
}

pub fn break_request(params: &BreakViewParams) -> Result<proto::ShapeDrawingViewRequest, McpError> {
    use proto::shape_drawing_view_request::Change;
    let view_name = params.view.trim().to_string();
    if view_name.is_empty() {
        return Err(invalid("view: name the view to break"));
    }
    if params.remove {
        if !params.breaks.is_empty() {
            return Err(invalid("remove takes the breaks off: give no breaks with it"));
        }
        return Ok(proto::ShapeDrawingViewRequest { view_name, change: Some(Change::RemoveBreaks(true)) });
    }
    if params.breaks.is_empty() {
        return Err(invalid("breaks: give the ranges to hide, or remove"));
    }
    let mut breaks = Vec::new();
    for b in &params.breaks {
        if !(b.from_mm.is_finite() && b.to_mm.is_finite()) || (b.to_mm - b.from_mm).abs() < 1e-3 {
            return Err(invalid(format!("a break from {} to {} mm hides nothing", b.from_mm, b.to_mm)));
        }
        breaks.push(proto::ViewBreakSpec {
            along_x: b.along == BreakAxis::X,
            from: b.from_mm.min(b.to_mm) / MM_PER_M,
            to: b.from_mm.max(b.to_mm) / MM_PER_M,
        });
    }
    let style = match params.style.unwrap_or(BreakStyle::Zigzag) {
        BreakStyle::Straight => 1,
        BreakStyle::Zigzag => 2,
        BreakStyle::Curve => 3,
        BreakStyle::SmallZigzag => 4,
        BreakStyle::Jagged => 5,
    };
    Ok(proto::ShapeDrawingViewRequest { view_name, change: Some(Change::Breaks(proto::BreakViewSpec { breaks, style })) })
}

/// A view's breaks for inspect_drawing.
pub fn breaks_json(view: &proto::ViewInfo) -> Value {
    let styles = ["", "straight", "zigzag", "curve", "small_zigzag", "jagged"];
    json!(
        view.breaks
            .iter()
            .map(|b| json!({
                "along": if b.along_x { "x" } else { "y" },
                "from_mm": crate::inspect::round(b.from * MM_PER_M, 2),
                "to_mm": crate::inspect::round(b.to * MM_PER_M, 2),
                "style": styles.get(b.style as usize).copied().unwrap_or(""),
            }))
            .collect::<Vec<_>>()
    )
}

/// The reply to crop_view / break_view.
pub fn shape_reply_text(request: &proto::ShapeDrawingViewRequest, reply: &proto::SetupDrawingViewResponse) -> String {
    use proto::shape_drawing_view_request::Change;
    let done = match request.change {
        Some(Change::Crop(_)) => "cropped",
        Some(Change::Breaks(_)) => "broke",
        Some(Change::RemoveCrop(_)) => "took the crop off",
        _ => "took the breaks off",
    };
    let Some(view) = reply.sheet.as_ref().and_then(|s| find_view(s, &reply.view_name)) else {
        return format!("{done} {}", reply.view_name);
    };
    let mut text = format!("{done} {}", view.name);
    if let Some(rect) = Rect::of(view) {
        text += &format!(
            ": outline now x {:.0}..{:.0}, y {:.0}..{:.0} mm",
            rect.lo.x * MM_PER_M,
            rect.hi.x * MM_PER_M,
            rect.lo.y * MM_PER_M,
            rect.hi.y * MM_PER_M
        );
    }
    text += ".";
    if view.broken {
        text += &format!(" Note: {BROKEN_NOTE}.");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(args: Value) -> Result<ViewRequest, McpError> {
        serde_json::from_value::<DrawingViewParams>(args).unwrap().split()
    }

    #[test]
    fn view_types_take_only_their_parameters() {
        assert!(matches!(split(json!({"orientation": "top"})), Ok(ViewRequest::Model(_))));
        assert!(matches!(split(json!({"type": "section", "view": "A", "flip": true})), Ok(ViewRequest::Section(s)) if s.flip));
        assert!(matches!(split(json!({"type": "auxiliary", "view": "A", "edge": "view_edge:1"})), Ok(ViewRequest::Auxiliary(_))));
        for (args, needle) in [
            (json!({"type": "projected"}), "needs view"),
            (json!({"type": "detail", "view": "A"}), "needs radius_mm"),
            (json!({"type": "auxiliary", "view": "A"}), "needs edge"),
            (json!({"type": "projected", "view": "A", "scale": "1:2"}), "doesn't take scale"),
            (json!({"view": "A"}), "doesn't take view"),
            (json!({"type": "section", "view": "A", "name": "S"}), "label names it"),
        ] {
            let error = split(args.clone()).err().unwrap();
            assert!(error.message.contains(needle), "{args}: {}", error.message);
        }
    }

    fn view(name: &str, lo: [f64; 2], hi: [f64; 2]) -> proto::ViewInfo {
        let v = |p: [f64; 2]| Some(proto::Vec3 { x: p[0], y: p[1], z: 0.0 });
        proto::ViewInfo {
            name: name.into(),
            r#type: 7,
            scale_numerator: 1.0,
            scale_denominator: 2.0,
            outline: Some(proto::Box3 { min: v(lo), max: v(hi) }),
            position: v([(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0]),
            ..Default::default()
        }
    }

    fn drawing(first_angle: bool, views: Vec<proto::ViewInfo>) -> proto::InspectDrawingResponse {
        proto::InspectDrawingResponse {
            sheets: vec![proto::SheetInfo {
                name: "Sheet1".into(),
                width: 0.42,
                height: 0.297,
                first_angle,
                views,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn section(d: &proto::InspectDrawingResponse, v: serde_json::Value) -> Result<proto::SectionViewSpec, McpError> {
        let plan = section_plan(d, &IdRegistry::default(), &Cache::default(), &serde_json::from_value(v).unwrap())?;
        match plan.request.kind {
            Some(proto::derive_drawing_view_request::Kind::Section(s)) => Ok(s),
            _ => panic!("not a section"),
        }
    }

    /// The direction SolidWorks looks for a line: turned 90 degrees clockwise.
    fn looks(s: &proto::SectionViewSpec) -> DVec2 {
        let (a, b) = (s.start.as_ref().unwrap(), s.end.as_ref().unwrap());
        let along = DVec2::new(b.x - a.x, b.y - a.y).normalize();
        DVec2::new(along.y, -along.x)
    }

    #[test]
    fn a_section_looks_back_at_its_parent_in_third_angle() {
        let d = drawing(false, vec![view("F", [0.05, 0.1], [0.15, 0.2])]);
        // Right: looks left. Default: the side with more room is the right.
        for args in [json!({"view": "F", "side": "right"}), json!({"view": "F"}), json!({"view": "F", "at_mm": [300, 150]})] {
            let s = section(&d, args.clone()).unwrap();
            assert!((looks(&s) + DVec2::X).length() < 1e-9, "{args}: {:?}", looks(&s));
        }
        let s = section(&d, json!({"view": "F", "side": "left"})).unwrap();
        assert!((looks(&s) - DVec2::X).length() < 1e-9);
        let s = section(&d, json!({"view": "F", "side": "right", "flip": true})).unwrap();
        assert!((looks(&s) - DVec2::X).length() < 1e-9);
        // A horizontal line, the view below: looks up.
        let s = section(&d, json!({"view": "F", "angle_deg": 0, "side": "below"})).unwrap();
        assert!((looks(&s) - DVec2::Y).length() < 1e-9);
        // First angle turns it round.
        let d = drawing(true, vec![view("F", [0.05, 0.1], [0.15, 0.2])]);
        let s = section(&d, json!({"view": "F", "side": "right"})).unwrap();
        assert!((looks(&s) - DVec2::X).length() < 1e-9);
    }

    #[test]
    fn a_section_line_crosses_the_outline() {
        let d = drawing(false, vec![view("F", [0.05, 0.1], [0.15, 0.2])]);
        let s = section(&d, json!({"view": "F", "through_mm": [70, 120]})).unwrap();
        let (a, b) = (s.start.unwrap(), s.end.unwrap());
        assert!((a.x - 0.07).abs() < 1e-9 && (b.x - 0.07).abs() < 1e-9, "{a:?} {b:?}");
        let (lo, hi) = (a.y.min(b.y), a.y.max(b.y));
        assert!((lo - (0.1 - LINE_OVERHANG)).abs() < 1e-9 && (hi - (0.2 + LINE_OVERHANG)).abs() < 1e-9, "{lo} {hi}");
    }

    #[test]
    fn section_refusals() {
        let d = drawing(false, vec![view("F", [0.05, 0.1], [0.15, 0.2])]);
        for args in [
            json!({"view": "Nope"}),
            json!({"view": "F", "side": "above"}),
            json!({"view": "F", "side": "left", "at_mm": [1, 2]}),
            json!({"view": "F", "through": "view_edge:1", "through_mm": [1, 2]}),
            json!({"view": "F", "through": "view_edge:1"}),
            json!({"view": "F", "depth_mm": 0}),
        ] {
            assert!(section(&d, args.clone()).is_err(), "{args}");
        }
    }

    #[test]
    fn labels_skip_what_is_used_and_lookalikes() {
        let mut a = view("A", [0.0, 0.0], [0.1, 0.1]);
        a.label = "A".into();
        let mut b = view("B", [0.0, 0.0], [0.1, 0.1]);
        b.label = "B".into();
        assert_eq!(next_label(&drawing(false, vec![a.clone(), b])), "C");
        // All used up to H: I is skipped.
        let used: Vec<proto::ViewInfo> = ('A'..='H')
            .map(|c| {
                let mut v = view(&c.to_string(), [0.0, 0.0], [0.1, 0.1]);
                v.label = c.to_string();
                v
            })
            .collect();
        assert_eq!(next_label(&drawing(false, used)), "J");
        assert_eq!(label_or_next(&drawing(false, vec![a.clone()]), Some(" X ")).unwrap(), "X");
        assert!(label_or_next(&drawing(false, vec![a]), Some("a")).is_err());
    }

    #[test]
    fn detail_defaults_to_twice_the_parents_scale() {
        let d = drawing(false, vec![view("F", [0.05, 0.1], [0.15, 0.2])]);
        let params = serde_json::from_value(json!({"view": "F", "center_mm": [100, 150], "radius_mm": 10})).unwrap();
        let plan = detail_plan(&d, &Cache::default(), &params).unwrap();
        let Some(proto::derive_drawing_view_request::Kind::Detail(s)) = &plan.request.kind else { panic!() };
        let scale = s.scale.as_ref().unwrap();
        assert_eq!((scale.numerator, scale.denominator), (2.0, 2.0));
        assert!((s.radius - 0.01).abs() < 1e-12 && s.center.as_ref().unwrap().x == 0.1);
        assert_eq!(plan.request.label, "A");
    }

    #[test]
    fn crop_and_break_requests() {
        use proto::shape_drawing_view_request::Change;
        let crop = |v| crop_request(&serde_json::from_value(v).unwrap());
        let Some(Change::Crop(c)) = crop(json!({"view": "F", "circle_center_mm": [100, 50], "circle_radius_mm": 20, "outline": "jagged"})).unwrap().change else { panic!() };
        assert!(c.jagged_outline && !c.no_outline && (c.radius - 0.02).abs() < 1e-12);
        assert!(matches!(crop(json!({"view": "F", "remove": true})).unwrap().change, Some(Change::RemoveCrop(true))));
        for bad in [
            json!({"view": "F"}),
            json!({"view": "F", "circle_center_mm": [1, 2]}),
            json!({"view": "F", "circle_radius_mm": 0, "circle_center_mm": [1, 2]}),
            json!({"view": "F", "rectangle_mm": [[1, 2], [1, 9]]}),
            json!({"view": "F", "remove": true, "circle_radius_mm": 1}),
            json!({"view": "F", "rectangle_mm": [[1, 2], [5, 9]], "outline": "full"}),
        ] {
            assert!(crop(bad.clone()).is_err(), "{bad}");
        }

        let brk = |v| break_request(&serde_json::from_value(v).unwrap());
        let Some(Change::Breaks(b)) = brk(json!({"view": "F", "breaks": [{"along": "x", "from_mm": 200, "to_mm": 120}]})).unwrap().change else { panic!() };
        assert_eq!(b.style, 2);
        assert!(b.breaks[0].along_x && (b.breaks[0].from - 0.12).abs() < 1e-12 && (b.breaks[0].to - 0.2).abs() < 1e-12);
        assert!(matches!(brk(json!({"view": "F", "remove": true})).unwrap().change, Some(Change::RemoveBreaks(true))));
        for bad in [
            json!({"view": "F"}),
            json!({"view": "F", "breaks": [{"along": "y", "from_mm": 5, "to_mm": 5}]}),
            json!({"view": "F", "remove": true, "breaks": [{"along": "y", "from_mm": 5, "to_mm": 9}]}),
        ] {
            assert!(brk(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn slide_clear_stays_on_its_side_of_the_parent() {
        let mut d = drawing(false, vec![view("F", [0.05, 0.1], [0.15, 0.2]), view("S", [0.13, 0.12], [0.17, 0.18])]);
        let sheet = d.sheets.remove(0);
        let at = view::slide_clear(&sheet, "S", "F", DVec2::X).unwrap();
        // 5 mm clear of F's edge at 0.15 (half width 0.02), to the next 5 mm step.
        assert!((at.y - 0.15).abs() < 1e-9 && at.x > 0.175 - 1e-9 && at.x < 0.185, "{at}");
        // Clear already: stays.
        let mut d = drawing(false, vec![view("F", [0.05, 0.1], [0.15, 0.2]), view("S", [0.2, 0.12], [0.24, 0.18])]);
        let sheet = d.sheets.remove(0);
        assert_eq!(view::slide_clear(&sheet, "S", "F", DVec2::X), None);
    }
}
