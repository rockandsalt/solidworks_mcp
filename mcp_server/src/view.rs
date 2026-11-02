//! The drawing view tools: create_drawing_view and edit_drawing_view, both
//! over the SetupDrawingView RPC. Params (orientation and display names,
//! the "1:2" / "sheet" / "parent" scale), a spot for a new view clear of
//! the sheet's other views, and the reply text.

use std::path::Path;

use glam::DVec2;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::drawing::{axes_text, display_mode_name, scale_text};
use crate::inspect::{MM_PER_M, round};
use crate::sheet::parse_scale;

/// Space kept between a new view's outline and the others', and from the
/// sheet's edges: sheet meters.
const VIEW_GAP: f64 = 0.005;
const EDGE_MARGIN: f64 = 0.010;
/// Grid the free spot is searched on.
const SPOT_STEP: f64 = 0.005;
/// Positions that differ by less than this are where they were asked to go.
const POSITION_SLACK: f64 = 5e-4;

/// How the view draws the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    /// Visible edges only (SolidWorks' "Hidden Lines Removed"): the usual drawing view.
    HiddenLinesRemoved,
    /// Visible edges plus hidden ones, drawn dashed / grey ("Hidden Lines Visible").
    HiddenLinesVisible,
    /// Every edge drawn solid, hidden or not.
    Wireframe,
    Shaded,
    ShadedWithEdges,
    /// edit_drawing_view, projected and other derived views: the display of the view it was
    /// made from (the default for those).
    Parent,
}

impl Display {
    /// swDisplayMode_e; None for Parent.
    fn sw(self) -> Option<i32> {
        Some(match self {
            Display::Wireframe => 0,
            Display::HiddenLinesVisible => 1,
            Display::HiddenLinesRemoved => 2,
            Display::Shaded => 3,
            Display::ShadedWithEdges => 7,
            Display::Parent => return None,
        })
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CreateDrawingViewParams {
    /// The part or assembly to show: an absolute .SLDPRT / .SLDASM path, or the name of one
    /// open in SolidWorks (e.g. "bracket" or "bracket.SLDPRT"). Default: the model the
    /// drawing's views already show, else the one part or assembly open. It must have been
    /// saved.
    #[serde(default)]
    pub model: Option<String>,
    /// The model's configuration to show. Default: its active one.
    #[serde(default)]
    pub configuration: Option<String>,
    /// Which way the model is seen: front, back, left, right, top, bottom, isometric,
    /// dimetric, trimetric (the model's own standard views), or the name of a view saved in
    /// the model. Default: front.
    #[serde(default)]
    pub orientation: Option<String>,
    /// Where the view's center goes: [x, y] in sheet mm from the lower-left corner. Default: a
    /// free spot as near the sheet's middle as there is.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// "1:2" (half size), "2:1", ... or "sheet" to follow the sheet's scale. Default: the
    /// sheet's scale -- and on a sheet without views SolidWorks may first rescale the sheet to
    /// fit the model.
    #[serde(default)]
    pub scale: Option<String>,
    /// Default: SolidWorks' (normally hidden_lines_removed).
    #[serde(default)]
    pub display: Option<Display>,
    /// The sheet to put the view on. Default: the active sheet. Another sheet stays in the
    /// background.
    #[serde(default)]
    pub sheet: Option<String>,
    /// The view's name. Default: SolidWorks' next "Drawing ViewN".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditDrawingViewParams {
    /// The view to change, e.g. "Drawing View1" (inspect_drawing lists them).
    pub view: String,
    /// Show another part or assembly: an absolute .SLDPRT / .SLDASM path, or the name of one
    /// open in SolidWorks. Dimensions and annotations attached to the old model's edges may be
    /// left dangling.
    #[serde(default)]
    pub model: Option<String>,
    /// Show another configuration of the model.
    #[serde(default)]
    pub configuration: Option<String>,
    /// Turn the view: front, back, left, right, top, bottom, isometric, dimetric, trimetric,
    /// or a view saved in the model. Views projected from it turn with it; a projected view's
    /// own direction can't be set.
    #[serde(default)]
    pub orientation: Option<String>,
    /// Move the view's center to [x, y] sheet mm. A view aligned with another one (projected
    /// views are) only moves along that alignment.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// "1:2", "2:1", ..., "sheet" (follow the sheet's scale) or, for projected and other
    /// derived views, "parent" (follow the view it was made from).
    #[serde(default)]
    pub scale: Option<String>,
    #[serde(default)]
    pub display: Option<Display>,
    /// A new name for the view.
    #[serde(default)]
    pub rename: Option<String>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

/// A model argument: an absolute path is checked here, a name is looked up
/// among the open documents by the add-in.
fn model_arg(value: Option<&str>) -> Result<String, McpError> {
    let value = text(value);
    if value.is_empty() || !(value.contains(['\\', '/']) || value.contains(':')) {
        return Ok(value);
    }
    let path = Path::new(&value);
    if !path.is_absolute() {
        return Err(invalid(format!("model {value:?}: give an absolute path, or the name of an open part or assembly")));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext != "sldprt" && ext != "sldasm" {
        return Err(invalid(format!(
            "model {value:?}: a drawing view shows a part (.SLDPRT) or an assembly (.SLDASM)"
        )));
    }
    if !path.is_file() {
        return Err(invalid(format!("there's no file {value}")));
    }
    Ok(value)
}

/// "front" -> "*Front", ...; anything else is a view saved in the model.
fn orientation_arg(value: Option<&str>) -> String {
    let value = text(value);
    let standard = [
        "front", "back", "left", "right", "top", "bottom", "isometric", "dimetric", "trimetric",
    ];
    let bare = value.strip_prefix('*').unwrap_or(&value).to_ascii_lowercase();
    match standard.iter().find(|s| **s == bare) {
        Some(s) => format!("*{}{}", s[..1].to_ascii_uppercase(), &s[1..]),
        None => value,
    }
}

fn set_scale(request: &mut proto::SetupDrawingViewRequest, scale: Option<&str>) -> Result<(), McpError> {
    use proto::setup_drawing_view_request::Scale;
    let Some(scale) = scale.map(str::trim) else { return Ok(()) };
    request.scale = Some(match scale.to_ascii_lowercase().as_str() {
        "sheet" => Scale::FollowSheet(true),
        "parent" => Scale::FollowParent(true),
        _ => {
            let (numerator, denominator) = parse_scale(scale)?;
            Scale::Ratio(proto::ViewScale { numerator, denominator })
        }
    });
    Ok(())
}

fn set_display(request: &mut proto::SetupDrawingViewRequest, display: Option<Display>) {
    match display {
        Some(Display::Parent) => request.parent_display = true,
        Some(d) => request.display_mode = d.sw(),
        None => {}
    }
}

fn position(at_mm: Option<[f64; 2]>) -> Result<Option<proto::Vec3>, McpError> {
    match at_mm {
        Some([x, y]) if x.is_finite() && y.is_finite() => {
            Ok(Some(proto::Vec3 { x: x / MM_PER_M, y: y / MM_PER_M, z: 0.0 }))
        }
        Some(_) => Err(invalid("at_mm: give [x, y] in sheet mm")),
        None => Ok(None),
    }
}

pub fn create_request(params: &CreateDrawingViewParams) -> Result<proto::SetupDrawingViewRequest, McpError> {
    if params.display == Some(Display::Parent) {
        return Err(invalid("display \"parent\" is for projected views; a new view is made from the model"));
    }
    if params.scale.as_deref().is_some_and(|s| s.trim().eq_ignore_ascii_case("parent")) {
        return Err(invalid("scale \"parent\" is for projected views; a new view is made from the model"));
    }
    let mut request = proto::SetupDrawingViewRequest {
        create: true,
        view_name: text(params.name.as_deref()),
        sheet_name: text(params.sheet.as_deref()),
        model: model_arg(params.model.as_deref())?,
        configuration: text(params.configuration.as_deref()),
        orientation: orientation_arg(params.orientation.as_deref()),
        position: position(params.at_mm)?,
        ..Default::default()
    };
    set_scale(&mut request, params.scale.as_deref())?;
    set_display(&mut request, params.display);
    Ok(request)
}

pub fn edit_request(params: &EditDrawingViewParams) -> Result<proto::SetupDrawingViewRequest, McpError> {
    let view = text(Some(&params.view));
    if view.is_empty() {
        return Err(invalid("view: name the view to change, e.g. \"Drawing View1\""));
    }
    if params.rename.as_ref().is_some_and(|r| r.trim().is_empty()) {
        return Err(invalid("rename: give the view's new name"));
    }
    let mut request = proto::SetupDrawingViewRequest {
        view_name: view,
        model: model_arg(params.model.as_deref())?,
        configuration: text(params.configuration.as_deref()),
        orientation: orientation_arg(params.orientation.as_deref()),
        position: position(params.at_mm)?,
        new_name: text(params.rename.as_deref()),
        ..Default::default()
    };
    set_scale(&mut request, params.scale.as_deref())?;
    set_display(&mut request, params.display);
    if request.model.is_empty()
        && request.configuration.is_empty()
        && request.orientation.is_empty()
        && request.position.is_none()
        && request.scale.is_none()
        && request.display_mode.is_none()
        && !request.parent_display
        && request.new_name.is_empty()
    {
        return Err(invalid(
            "nothing to change: give model, configuration, orientation, at_mm, scale, display or rename",
        ));
    }
    Ok(request)
}

/// Asking for these redraws the view (and the views projected from it):
/// the sheet geometry inspect_drawing gave for their edges is stale.
pub fn redraws(request: &proto::SetupDrawingViewRequest) -> bool {
    !request.model.is_empty()
        || !request.configuration.is_empty()
        || !request.orientation.is_empty()
        || request.position.is_some()
        || request.scale.is_some()
        || !request.new_name.is_empty()
}

// ---- Placement ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub lo: DVec2,
    pub hi: DVec2,
}

impl Rect {
    pub fn of(view: &proto::ViewInfo) -> Option<Rect> {
        let b = view.outline.as_ref()?;
        let (lo, hi) = (b.min.as_ref()?, b.max.as_ref()?);
        Some(Rect { lo: DVec2::new(lo.x, lo.y), hi: DVec2::new(hi.x, hi.y) })
    }

    pub fn overlaps(&self, other: &Rect, gap: f64) -> bool {
        self.lo.x < other.hi.x + gap
            && other.lo.x < self.hi.x + gap
            && self.lo.y < other.hi.y + gap
            && other.lo.y < self.hi.y + gap
    }

    fn size(&self) -> DVec2 {
        self.hi - self.lo
    }

    pub fn center(&self) -> DVec2 {
        (self.lo + self.hi) / 2.0
    }

    /// Moved so that its center is at `center`.
    pub fn centered_at(&self, center: DVec2) -> Rect {
        let half = self.size() / 2.0;
        Rect { lo: center - half, hi: center + half }
    }
}

pub(crate) fn find_view<'a>(sheet: &'a proto::SheetInfo, name: &str) -> Option<&'a proto::ViewInfo> {
    sheet.views.iter().find(|v| v.name == name)
}

/// The views on `sheet` whose outlines `name`'s overlaps.
fn overlapping(sheet: &proto::SheetInfo, name: &str) -> Vec<String> {
    let Some(rect) = find_view(sheet, name).and_then(Rect::of) else { return Vec::new() };
    sheet
        .views
        .iter()
        .filter(|v| v.name != name)
        .filter(|v| Rect::of(v).is_some_and(|r| r.overlaps(&rect, 0.0)))
        .map(|v| v.name.clone())
        .collect()
}

/// Where to put the center of `name`'s view (sheet meters) so that its
/// outline clears the sheet's other views and edges, as near the sheet's
/// middle as possible. None if it already does, or nothing fits.
pub fn free_spot(sheet: &proto::SheetInfo, name: &str) -> Option<DVec2> {
    let rect = find_view(sheet, name).and_then(Rect::of)?;
    let others: Vec<Rect> = sheet.views.iter().filter(|v| v.name != name).filter_map(Rect::of).collect();
    let fits = |r: &Rect| {
        r.lo.x >= EDGE_MARGIN
            && r.lo.y >= EDGE_MARGIN
            && r.hi.x <= sheet.width - EDGE_MARGIN
            && r.hi.y <= sheet.height - EDGE_MARGIN
            && !others.iter().any(|o| o.overlaps(r, VIEW_GAP))
    };
    if others.iter().all(|o| !o.overlaps(&rect, VIEW_GAP)) {
        return None;
    }
    // The view's position is its center as the add-in reports it; the
    // outline may sit off it (annotations, a cropped view).
    let offset = rect.center() - view_position(find_view(sheet, name)?).unwrap_or(rect.center());
    let half = rect.size() / 2.0;
    let middle = DVec2::new(sheet.width, sheet.height) / 2.0;
    let mut best: Option<(f64, DVec2)> = None;
    let (nx, ny) = ((sheet.width / SPOT_STEP) as i64, (sheet.height / SPOT_STEP) as i64);
    for i in 0..=nx {
        for j in 0..=ny {
            let c = DVec2::new(i as f64 * SPOT_STEP, j as f64 * SPOT_STEP);
            let r = Rect { lo: c - half, hi: c + half };
            let d = c.distance_squared(middle);
            if fits(&r) && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, c));
            }
        }
    }
    best.map(|(_, c)| c - offset)
}

/// `spot` slid along `direction` (a unit vector, the only way an aligned view
/// moves) until the outline clears the sheet's other views and edges: the
/// nearest such place on the far side of `parent` from where it starts, or
/// failing that the nearest that clears `parent` alone. The view's position
/// for that, sheet meters; None if it's already fine or nothing works.
pub fn slide_clear(sheet: &proto::SheetInfo, name: &str, parent: &str, direction: DVec2) -> Option<DVec2> {
    let view = find_view(sheet, name)?;
    let rect = Rect::of(view)?;
    let parent_rect = find_view(sheet, parent).and_then(Rect::of)?;
    let others: Vec<Rect> = sheet.views.iter().filter(|v| v.name != name).filter_map(Rect::of).collect();
    let fits = |r: &Rect| {
        r.lo.x >= EDGE_MARGIN
            && r.lo.y >= EDGE_MARGIN
            && r.hi.x <= sheet.width - EDGE_MARGIN
            && r.hi.y <= sheet.height - EDGE_MARGIN
            && !others.iter().any(|o| o.overlaps(r, VIEW_GAP))
    };
    let side = |c: DVec2| (c - parent_rect.center()).dot(direction) >= 0.0;
    if side(rect.center()) && fits(&rect) {
        return None;
    }
    let reach = sheet.width.hypot(sheet.height);
    let steps = (reach / SPOT_STEP) as i64;
    let places = (-steps..=steps).map(|i| rect.center() + direction * (i as f64 * SPOT_STEP));
    let nearest = |test: &dyn Fn(&Rect) -> bool| {
        places
            .clone()
            .filter(|c| side(*c) && test(&rect.centered_at(*c)))
            .min_by(|a, b| a.distance_squared(parent_rect.center()).total_cmp(&b.distance_squared(parent_rect.center())))
    };
    let center = nearest(&fits).or_else(|| nearest(&|r| !r.overlaps(&parent_rect, VIEW_GAP)))?;
    // The position may sit off the outline's middle (a detail view, a crop).
    Some(center - rect.center() + view_position(view).unwrap_or(rect.center()))
}

fn view_position(view: &proto::ViewInfo) -> Option<DVec2> {
    view.position.as_ref().map(|p| DVec2::new(p.x, p.y))
}

// ---- Reply --------------------------------------------------------------------

fn mm2(p: DVec2) -> String {
    format!("[{}, {}]", round(p.x * MM_PER_M, 1), round(p.y * MM_PER_M, 1))
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// "Drawing View1 on Sheet1: *Front of block.SLDPRT (configuration Default), ..."
fn view_text(sheet: &proto::SheetInfo, view: &proto::ViewInfo) -> String {
    let mut parts = Vec::new();
    let model = file_name(&view.model_file);
    // A section, detail or auxiliary view is what it is whichever orientation
    // SolidWorks reports for it.
    let what = if let (2 | 3 | 5, false) = (view.r#type, view.base_view.is_empty()) {
        let kind = match view.r#type {
            2 => "section",
            3 => "detail",
            _ => "auxiliary view",
        };
        format!("{kind} of {} ({model})", view.base_view)
    } else if view.orientation.is_empty() {
        match view.base_view.as_str() {
            "" => model.to_string(),
            base => format!("{model} projected from {base}"),
        }
    } else {
        format!("{} of {}", view.orientation, file_name(&view.model_file))
    };
    parts.push(if view.configuration.is_empty() {
        what
    } else {
        format!("{what} (configuration {})", view.configuration)
    });
    if let Some(scale) = scale_text(view.scale_numerator, view.scale_denominator) {
        let from = if view.uses_parent_scale {
            format!(" (from {})", view.base_view)
        } else if view.uses_sheet_scale {
            " (sheet scale)".to_string()
        } else {
            String::new()
        };
        parts.push(format!("scale {scale}{from}"));
    }
    if let Some(mode) = display_mode_name(view.display_mode) {
        let from = if view.uses_parent_display { format!(" (from {})", view.base_view) } else { String::new() };
        parts.push(format!("{}{from}", mode.replace('_', " ")));
    }
    // A detail view's position isn't its middle, and a crop or break moves
    // the middle off it: the outline says where those are.
    if let (Some(p), false) = (view_position(view), view.r#type == 3 || view.cropped || view.broken) {
        parts.push(format!("centered at {} mm", mm2(p)));
    }
    if view.cropped {
        parts.push("cropped".to_string());
    }
    if view.broken {
        parts.push("broken".to_string());
    }
    if let Some(r) = Rect::of(view) {
        parts.push(format!(
            "outline x {:.0}..{:.0}, y {:.0}..{:.0} mm",
            r.lo.x * MM_PER_M,
            r.hi.x * MM_PER_M,
            r.lo.y * MM_PER_M,
            r.hi.y * MM_PER_M
        ));
    }
    let mut text = format!("{} on {}: {}", view.name, sheet.name, parts.join(", "));
    if let Some(axes) = axes_text(view) {
        text += &format!(". On the sheet {axes}");
    }
    text
}

/// The views made from `name` (projected from it, or from one of those).
fn dependents(sheet: &proto::SheetInfo, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut frontier = vec![name.to_string()];
    while let Some(parent) = frontier.pop() {
        for v in &sheet.views {
            if v.base_view == parent && v.name != name && !out.contains(&v.name) {
                out.push(v.name.clone());
                frontier.push(v.name.clone());
            }
        }
    }
    out
}

/// The view and those made from it: what an edit redraws.
pub fn redrawn_views(reply: &proto::SetupDrawingViewResponse) -> Vec<String> {
    let Some(sheet) = &reply.sheet else { return vec![reply.view_name.clone()] };
    let mut names = vec![reply.view_name.clone()];
    names.extend(dependents(sheet, &reply.view_name));
    names
}

/// The reply to create_drawing_view / edit_drawing_view. `asked_at` is
/// where the view was asked to go (sheet meters), `moved_to` where it was
/// moved to clear the other views.
pub fn reply_text(
    request: &proto::SetupDrawingViewRequest,
    reply: &proto::SetupDrawingViewResponse,
    asked_at: Option<DVec2>,
    moved_to: Option<DVec2>,
) -> String {
    let Some(sheet) = &reply.sheet else { return format!("done: {}", reply.view_name) };
    let Some(view) = find_view(sheet, &reply.view_name) else {
        return format!("done; {} has no view {} now", sheet.name, reply.view_name);
    };
    let mut text = format!("{} {}", if reply.created { "created" } else { "changed" }, view_text(sheet, view));
    if sheet.name != reply.active_sheet {
        text += &format!(" ({} stays the active sheet)", reply.active_sheet);
    }
    text += ".";

    let mut notes = Vec::new();
    let differs = |a: f64, b: f64| (a - b).abs() > 1e-9 * a.abs().max(b.abs());
    if let Some(before) = &reply.sheet_scale_before
        && (differs(before.numerator, sheet.scale_numerator) || differs(before.denominator, sheet.scale_denominator))
    {
        notes.push(format!(
            "SolidWorks set the sheet's scale from {} to {} to fit its first view (views using the \
             sheet scale follow it; give scale to choose one, or edit_sheet to change the sheet's)",
            scale_text(before.numerator, before.denominator).unwrap_or_default(),
            scale_text(sheet.scale_numerator, sheet.scale_denominator).unwrap_or_default()
        ));
    }
    if let (Some(asked), Some(at)) = (asked_at, view_position(view))
        && asked.distance(at) > POSITION_SLACK
    {
        let why = if view.base_view.is_empty() {
            String::new()
        } else {
            format!(" (it stays aligned with {})", view.base_view)
        };
        notes.push(format!("it's centered at {} mm, not {} mm{why}", mm2(at), mm2(asked)));
    }
    if moved_to.is_some() {
        notes.push("it was put clear of the sheet's other views; pass at_mm to place it yourself".to_string());
    }
    if let Some(r) = Rect::of(view) {
        let slack = 1e-4;
        if r.lo.x < -slack || r.lo.y < -slack || r.hi.x > sheet.width + slack || r.hi.y > sheet.height + slack {
            notes.push(format!(
                "it reaches past the sheet's edges ({:.0} x {:.0} mm) -- give a smaller scale, move it, \
                 or a bigger paper size (edit_sheet)",
                sheet.width * MM_PER_M,
                sheet.height * MM_PER_M
            ));
        }
    }
    let overlaps = overlapping(sheet, &view.name);
    if !overlaps.is_empty() {
        notes.push(format!("its outline overlaps {} -- move one with at_mm", overlaps.join(", ")));
    }
    let followers = dependents(sheet, &view.name);
    if !reply.created && !followers.is_empty() && (!request.orientation.is_empty() || request.scale.is_some()
        || request.display_mode.is_some() || !request.model.is_empty() || !request.configuration.is_empty())
    {
        notes.push(format!("{} (made from it) follow it", followers.join(", ")));
    }
    if !reply.created && redraws(request) {
        notes.push("edge ids inspect_drawing gave for it are stale: inspect it again".to_string());
    }
    if !notes.is_empty() {
        text += &format!("\nNote: {}.", notes.join("; "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::setup_drawing_view_request::Scale;

    #[test]
    fn create_sends_names_and_scale() {
        let params: CreateDrawingViewParams = serde_json::from_value(serde_json::json!({
            "orientation": "Top", "at_mm": [100, 50], "scale": "1:2", "display": "shaded_with_edges",
            "sheet": " Sheet2 ", "configuration": "Long", "model": "bracket"
        }))
        .unwrap();
        let r = create_request(&params).unwrap();
        assert!(r.create);
        assert_eq!(
            (r.orientation.as_str(), r.sheet_name.as_str(), r.configuration.as_str(), r.model.as_str()),
            ("*Top", "Sheet2", "Long", "bracket")
        );
        assert_eq!(r.position, Some(proto::Vec3 { x: 0.1, y: 0.05, z: 0.0 }));
        assert_eq!(r.scale, Some(Scale::Ratio(proto::ViewScale { numerator: 1.0, denominator: 2.0 })));
        assert_eq!(r.display_mode, Some(7));
        // Defaults: everything left to the add-in.
        let r = create_request(&CreateDrawingViewParams::default()).unwrap();
        assert_eq!(r, proto::SetupDrawingViewRequest { create: true, ..Default::default() });
        let r = create_request(&CreateDrawingViewParams { scale: Some("Sheet".into()), ..Default::default() }).unwrap();
        assert_eq!(r.scale, Some(Scale::FollowSheet(true)));
        for bad in [
            CreateDrawingViewParams { display: Some(Display::Parent), ..Default::default() },
            CreateDrawingViewParams { scale: Some("parent".into()), ..Default::default() },
            CreateDrawingViewParams { scale: Some("1/2".into()), ..Default::default() },
            CreateDrawingViewParams { model: Some("parts\\a.SLDPRT".into()), ..Default::default() },
            CreateDrawingViewParams { model: Some("C:\\x\\a.SLDDRW".into()), ..Default::default() },
            CreateDrawingViewParams { model: Some("C:\\no\\such.SLDPRT".into()), ..Default::default() },
        ] {
            assert!(create_request(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn orientations_map_to_named_views() {
        assert_eq!(orientation_arg(Some("isometric")), "*Isometric");
        assert_eq!(orientation_arg(Some("*front")), "*Front");
        assert_eq!(orientation_arg(Some(" BOTTOM ")), "*Bottom");
        assert_eq!(orientation_arg(Some("Detail A")), "Detail A");
        assert_eq!(orientation_arg(None), "");
    }

    #[test]
    fn edit_needs_a_change() {
        let edit = |v: serde_json::Value| edit_request(&serde_json::from_value(v).unwrap());
        assert!(edit(serde_json::json!({"view": "Drawing View1"})).is_err());
        assert!(edit(serde_json::json!({"view": " ", "scale": "1:1"})).is_err());
        assert!(edit(serde_json::json!({"view": "Drawing View1", "rename": " "})).is_err());
        let r = edit(serde_json::json!({"view": "Drawing View2", "scale": "parent", "display": "parent"})).unwrap();
        assert_eq!((r.view_name.as_str(), r.create), ("Drawing View2", false));
        assert_eq!((r.scale, r.parent_display, r.display_mode), (Some(Scale::FollowParent(true)), true, None));
        assert!(!redraws(&proto::SetupDrawingViewRequest { parent_display: true, ..Default::default() }));
        let r = edit(serde_json::json!({"view": "Drawing View1", "rename": "Main"})).unwrap();
        assert!(r.new_name == "Main" && redraws(&r));
    }

    fn view(name: &str, base: &str, lo: [f64; 2], hi: [f64; 2]) -> proto::ViewInfo {
        let v = |p: [f64; 2]| Some(proto::Vec3 { x: p[0], y: p[1], z: 0.0 });
        proto::ViewInfo {
            name: name.into(),
            r#type: if base.is_empty() { 7 } else { 4 },
            orientation: if base.is_empty() { "*Front".into() } else { String::new() },
            model_file: r"C:\parts\block.SLDPRT".into(),
            configuration: "Default".into(),
            base_view: base.into(),
            scale_numerator: 1.0,
            scale_denominator: 2.0,
            uses_sheet_scale: base.is_empty(),
            uses_parent_scale: !base.is_empty(),
            display_mode: 2,
            outline: Some(proto::Box3 { min: v(lo), max: v(hi) }),
            position: v([(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0]),
            ..Default::default()
        }
    }

    fn a3(views: Vec<proto::ViewInfo>) -> proto::SheetInfo {
        proto::SheetInfo {
            name: "Sheet1".into(),
            width: 0.42,
            height: 0.297,
            scale_numerator: 1.0,
            scale_denominator: 2.0,
            views,
            ..Default::default()
        }
    }

    #[test]
    fn free_spot_clears_the_other_views() {
        // A new 100 x 60 view dropped on the middle, where a 100 x 100 one is.
        let sheet = a3(vec![
            view("Drawing View1", "", [0.16, 0.10], [0.26, 0.20]),
            view("Drawing View2", "", [0.16, 0.12], [0.26, 0.18]),
        ]);
        let c = free_spot(&sheet, "Drawing View2").unwrap();
        let moved = Rect { lo: c - DVec2::new(0.05, 0.03), hi: c + DVec2::new(0.05, 0.03) };
        let first = Rect::of(&sheet.views[0]).unwrap();
        assert!(!moved.overlaps(&first, VIEW_GAP - 1e-9), "{c}");
        assert!(moved.lo.x >= EDGE_MARGIN && moved.hi.x <= 0.42 - EDGE_MARGIN, "{c}");
        // The nearest such spot to the middle: below the first view (83.5 mm
        // from the middle; above would be 86.5, beside 105).
        assert!((c - DVec2::new(0.21, 0.065)).length() < 1e-9, "{c}");
        // Already clear: stays.
        let sheet = a3(vec![
            view("Drawing View1", "", [0.02, 0.02], [0.10, 0.10]),
            view("Drawing View2", "", [0.16, 0.12], [0.26, 0.18]),
        ]);
        assert_eq!(free_spot(&sheet, "Drawing View2"), None);
        // No room anywhere.
        let sheet = a3(vec![
            view("Drawing View1", "", [0.0, 0.0], [0.42, 0.297]),
            view("Drawing View2", "", [0.16, 0.12], [0.26, 0.18]),
        ]);
        assert_eq!(free_spot(&sheet, "Drawing View2"), None);
    }

    #[test]
    fn reply_describes_the_view_and_flags_what_went_differently() {
        let request = proto::SetupDrawingViewRequest { create: true, ..Default::default() };
        let reply = proto::SetupDrawingViewResponse {
            sheet: Some(a3(vec![view("Drawing View1", "", [0.1, 0.1], [0.2, 0.2])])),
            view_name: "Drawing View1".into(),
            created: true,
            sheet_scale_before: Some(proto::ViewScale { numerator: 1.0, denominator: 1.0 }),
            active_sheet: "Sheet1".into(),
        };
        let text = reply_text(&request, &reply, None, None);
        assert!(
            text.starts_with(
                "created Drawing View1 on Sheet1: *Front of block.SLDPRT (configuration Default), \
                 scale 1:2 (sheet scale), hidden lines removed, centered at [150, 150] mm, outline x \
                 100..200, y 100..200 mm."
            ),
            "{text}"
        );
        assert!(text.contains("set the sheet's scale from 1:1 to 1:2"), "{text}");

        // A projected view moved off its alignment, changed in the background.
        let mut reply = reply;
        reply.created = false;
        reply.sheet_scale_before = Some(proto::ViewScale { numerator: 1.0, denominator: 2.0 });
        reply.active_sheet = "Sheet2".into();
        reply.view_name = "Drawing View2".into();
        reply.sheet = Some(a3(vec![
            view("Drawing View1", "", [0.1, 0.1], [0.2, 0.2]),
            view("Drawing View2", "Drawing View1", [0.25, 0.1], [0.3, 0.2]),
            view("Drawing View3", "Drawing View2", [0.25, 0.22], [0.3, 0.26]),
        ]));
        let request = proto::SetupDrawingViewRequest {
            view_name: "Drawing View2".into(),
            position: Some(proto::Vec3 { x: 0.275, y: 0.2, z: 0.0 }),
            ..Default::default()
        };
        let text = reply_text(&request, &reply, Some(DVec2::new(0.275, 0.2)), None);
        assert!(text.starts_with("changed Drawing View2 on Sheet1: block.SLDPRT projected from Drawing View1"), "{text}");
        assert!(text.contains("scale 1:2 (from Drawing View1)") && text.contains("(Sheet2 stays the active sheet)"), "{text}");
        assert!(text.contains("centered at [275, 150] mm, not [275, 200] mm (it stays aligned with Drawing View1)"), "{text}");
        assert!(!text.contains("sheet's scale from") && !text.contains("follow it"), "{text}");
        assert!(text.contains("stale"), "{text}");
        assert_eq!(redrawn_views(&reply), ["Drawing View2", "Drawing View3"]);
    }
}
