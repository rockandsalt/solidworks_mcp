//! Borders and title blocks: draw_sheet_border, draw_title_block,
//! inspect_title_block and fill_title_block (custom properties themselves:
//! properties.rs).
//!
//! A border and a title block are lines and notes in a sheet's format (the
//! layer behind the views, shown on every print), each kind on its own
//! drawing layer so it can be drawn again without touching the rest of the
//! format. The geometry -- frame, zones, centering marks, the title block's
//! grid and where each label and value goes -- is worked out here; the
//! add-in only draws it. A title block's values are property-linked notes
//! ($PRP:"DrawnBy" shows the drawing's DrawnBy, $PRPSHEET:"Material" the
//! model's), so filling one in means setting custom properties: on the
//! drawing or on the model, as the notes say.

use std::collections::BTreeMap;

use glam::DVec2;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::drawing::sheet_mm;
use crate::inspect::{MM_PER_M, round};
use crate::table::Obstacles;

pub const BORDER_LAYER: &str = "BORDER";
pub const TITLE_BLOCK_LAYER: &str = "TITLE BLOCK";

/// ISO 5457: zones about 50 mm, an even number of them along each side.
const ZONE_SIZE: f64 = 0.050;
const ZONE_TEXT: f64 = 0.0035;
/// Text boxes are about this much taller than the text height.
const TEXT_BOX: f64 = 1.3;
const LABEL_TEXT: f64 = 0.002;
const VALUE_TEXT: f64 = 0.0035;
const CELL_PAD: f64 = 0.0012;
/// Letters for zone rows (ISO 5457 leaves out I and O).
const ZONE_LETTERS: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ";

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

fn mm(v: f64) -> f64 {
    round(v * MM_PER_M, 1)
}

fn line(a: DVec2, b: DVec2) -> proto::FormatLine {
    proto::FormatLine {
        start: Some(proto::Vec3 { x: a.x, y: a.y, z: 0.0 }),
        end: Some(proto::Vec3 { x: b.x, y: b.y, z: 0.0 }),
        layer: String::new(),
    }
}

/// A note whose text's middle is at `center` (centered text: the position
/// is the top of its box, at its middle).
fn centered(text: &str, center: DVec2, height: f64) -> proto::FormatNote {
    proto::FormatNote {
        text: text.to_string(),
        position: Some(proto::Vec3 { x: center.x, y: center.y + height * TEXT_BOX / 2.0, z: 0.0 }),
        text_height: height,
        justification: 2,
        ..Default::default()
    }
}

/// A left-justified note whose box's upper-left corner is `top_left`.
fn left(text: &str, top_left: DVec2, height: f64) -> proto::FormatNote {
    proto::FormatNote {
        text: text.to_string(),
        position: Some(proto::Vec3 { x: top_left.x, y: top_left.y, z: 0.0 }),
        text_height: height,
        justification: 1,
        ..Default::default()
    }
}

// ---- Params -----------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DrawSheetBorderParams {
    /// The sheet. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Distance from the sheet's edges to the frame, mm: [left, right, top, bottom]. Default
    /// [20, 10, 10, 10] (ISO 5457: a wider left edge for filing).
    #[serde(default)]
    pub margins_mm: Option<[f64; 4]>,
    /// Number of zones [across, up]. Default: about 50 mm each, an even number (ISO 5457);
    /// [0, 0] for none.
    #[serde(default)]
    pub zones: Option<[u32; 2]>,
    /// Zone labels in the margin: numbers along the top and bottom (from the left), letters
    /// down the sides (from the top). Default true.
    #[serde(default)]
    pub zone_labels: Option<bool>,
    /// Marks at the middle of each side for centering the print. Default true.
    #[serde(default)]
    pub centering_marks: Option<bool>,
    /// Line weight of the border layer: thin, normal, thick (default), thicker.
    #[serde(default)]
    pub weight: Option<LineWeight>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineWeight {
    Thin,
    Normal,
    Thick,
    Thicker,
}

impl LineWeight {
    fn sw(self) -> i32 {
        match self {
            LineWeight::Thin => 0,
            LineWeight::Normal => 1,
            LineWeight::Thick => 2,
            LineWeight::Thicker => 3,
        }
    }
}

/// Whose property a title block field shows.
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PropertyOf {
    /// The drawing's own ($PRP).
    #[default]
    Drawing,
    /// The model the sheet shows ($PRPSHEET), e.g. Material, Description.
    Model,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct TitleBlockCell {
    /// The small caption at the cell's top left, e.g. "DRAWN BY".
    #[serde(default)]
    pub label: Option<String>,
    /// The cell shows this custom property (it updates when the property changes), e.g.
    /// "DrawnBy".
    #[serde(default)]
    pub property: Option<String>,
    /// Whose property: drawing (default) or model.
    #[serde(default)]
    pub of: Option<PropertyOf>,
    /// With `property`: set the property to this. Without: fixed text for the cell (may hold
    /// links like $PRP:"SW-Sheet Scale").
    #[serde(default)]
    pub value: Option<String>,
    /// Width in mm. Default: the row's width left over, shared by the cells without one.
    #[serde(default)]
    pub width_mm: Option<f64>,
    /// The value's text height in mm. Default 3.5 (less in low rows).
    #[serde(default)]
    pub height_mm: Option<f64>,
    #[serde(default)]
    pub bold: bool,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct TitleBlockRow {
    pub cells: Vec<TitleBlockCell>,
    /// Row height in mm. Default 9.
    #[serde(default)]
    pub height_mm: Option<f64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DrawTitleBlockParams {
    /// The sheet. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// The rows from the top. Default: TITLE / DWG NO.; MATERIAL / FINISH / SCALE / SHEET;
    /// DRAWN / DATE / CHECKED / DATE / REV; COMPANY / FILE, linked to the usual properties.
    #[serde(default)]
    pub rows: Vec<TitleBlockRow>,
    /// Width in mm. Default 180 (ISO 7200).
    #[serde(default)]
    pub width_mm: Option<f64>,
    /// The title block's lower-right corner, [x, y] sheet mm. Default: the border frame's
    /// lower-right corner (draw_sheet_border), else 10 mm in from the sheet's.
    #[serde(default)]
    pub corner_mm: Option<[f64; 2]>,
    /// Make the value notes title block fields (users fill them in on the sheet). Default true.
    #[serde(default)]
    pub fields: Option<bool>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectTitleBlockParams {
    /// The sheet. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct FillTitleBlockParams {
    /// The sheet whose title block to fill. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Property name -> value, e.g. {"DrawnBy": "MW", "Description": "Bracket"}: names as
    /// inspect_title_block lists the fields. Each goes where the title block takes it from:
    /// the drawing ($PRP) or the sheet's model ($PRPSHEET).
    pub values: BTreeMap<String, String>,
    /// Also set names no title block note shows (on the drawing). Default false: refused.
    #[serde(default)]
    pub allow_unlisted: bool,
    /// Overwrite properties linked to a value SolidWorks works out, e.g. Material =
    /// "SW-Material@part.SLDPRT" (the part's physical material) with the text given. Default
    /// false: refused.
    #[serde(default)]
    pub replace_links: bool,
}

// ---- Border -------------------------------------------------------------------------

pub struct Border {
    pub lines: Vec<proto::FormatLine>,
    pub notes: Vec<proto::FormatNote>,
    pub frame: (DVec2, DVec2),
    pub zones: [u32; 2],
}

fn zone_count(length: f64) -> u32 {
    let n = (length / ZONE_SIZE).round() as u32;
    (n + n % 2).max(2)
}

fn zone_letter(i: u32) -> String {
    let letters: Vec<char> = ZONE_LETTERS.chars().collect();
    let n = letters.len() as u32;
    if i < n { letters[i as usize].to_string() } else { format!("{}{}", letters[(i / n - 1) as usize], letters[(i % n) as usize]) }
}

pub fn border(size: DVec2, params: &DrawSheetBorderParams) -> Result<Border, McpError> {
    let [l, r, t, b] = params.margins_mm.unwrap_or([20.0, 10.0, 10.0, 10.0]);
    if ![l, r, t, b].iter().all(|m| m.is_finite() && *m >= 0.0) {
        return Err(invalid("margins are mm, 0 or more"));
    }
    let (l, r, t, b) = (l / MM_PER_M, r / MM_PER_M, t / MM_PER_M, b / MM_PER_M);
    let lo = DVec2::new(l, b);
    let hi = DVec2::new(size.x - r, size.y - t);
    if hi.x - lo.x < 0.02 || hi.y - lo.y < 0.02 {
        return Err(invalid(format!(
            "the margins leave no room on a {:.0} x {:.0} mm sheet",
            size.x * MM_PER_M,
            size.y * MM_PER_M
        )));
    }
    let frame = hi - lo;
    let zones = params.zones.unwrap_or([zone_count(frame.x), zone_count(frame.y)]);
    if zones[0] > 50 || zones[1] > 50 {
        return Err(invalid("at most 50 zones a side"));
    }
    let mut lines = vec![
        line(lo, DVec2::new(hi.x, lo.y)),
        line(DVec2::new(hi.x, lo.y), hi),
        line(hi, DVec2::new(lo.x, hi.y)),
        line(DVec2::new(lo.x, hi.y), lo),
    ];
    let mut notes = Vec::new();
    // Zone divisions: ticks from the frame halfway out to the sheet's edge,
    // labels in the middle of each zone in the margin.
    let labels = params.zone_labels.unwrap_or(true);
    if zones[0] > 0 {
        let w = frame.x / zones[0] as f64;
        for i in 0..zones[0] {
            let x = lo.x + w * i as f64;
            if i > 0 {
                if t > 0.0 {
                    lines.push(line(DVec2::new(x, hi.y), DVec2::new(x, hi.y + t / 2.0)));
                }
                if b > 0.0 {
                    lines.push(line(DVec2::new(x, lo.y), DVec2::new(x, lo.y - b / 2.0)));
                }
            }
            if labels {
                let label = (i + 1).to_string();
                let cx = x + w / 2.0;
                if t >= ZONE_TEXT * 1.5 {
                    notes.push(centered(&label, DVec2::new(cx, size.y - t / 2.0), ZONE_TEXT));
                }
                if b >= ZONE_TEXT * 1.5 {
                    notes.push(centered(&label, DVec2::new(cx, b / 2.0), ZONE_TEXT));
                }
            }
        }
    }
    if zones[1] > 0 {
        let h = frame.y / zones[1] as f64;
        for i in 0..zones[1] {
            // Lettered from the top.
            let y = hi.y - h * i as f64;
            if i > 0 {
                if l > 0.0 {
                    lines.push(line(DVec2::new(lo.x, y), DVec2::new(lo.x - l / 2.0, y)));
                }
                if r > 0.0 {
                    lines.push(line(DVec2::new(hi.x, y), DVec2::new(hi.x + r / 2.0, y)));
                }
            }
            if labels {
                let label = zone_letter(i);
                let cy = y - h / 2.0;
                if l >= ZONE_TEXT * 1.5 {
                    notes.push(centered(&label, DVec2::new(l / 2.0, cy), ZONE_TEXT));
                }
                if r >= ZONE_TEXT * 1.5 {
                    notes.push(centered(&label, DVec2::new(size.x - r / 2.0, cy), ZONE_TEXT));
                }
            }
        }
    }
    if params.centering_marks.unwrap_or(true) {
        // From the sheet's edge to 5 mm inside the frame, at each side's
        // middle (a zone division when the zone count is even).
        let inside = 0.005;
        let mid = (lo + hi) / 2.0;
        lines.push(line(DVec2::new(mid.x, size.y), DVec2::new(mid.x, hi.y - inside)));
        lines.push(line(DVec2::new(mid.x, 0.0), DVec2::new(mid.x, lo.y + inside)));
        lines.push(line(DVec2::new(0.0, mid.y), DVec2::new(lo.x + inside, mid.y)));
        lines.push(line(DVec2::new(size.x, mid.y), DVec2::new(hi.x - inside, mid.y)));
    }
    Ok(Border { lines, notes, frame: (lo, hi), zones })
}

pub fn border_request(sheet: &str, border: &Border, weight: Option<LineWeight>) -> proto::DrawSheetFormatRequest {
    proto::DrawSheetFormatRequest {
        sheet_name: sheet.to_string(),
        layer: BORDER_LAYER.into(),
        layer_color: 0,
        layer_weight: weight.unwrap_or(LineWeight::Thick).sw(),
        clear: true,
        lines: border.lines.clone(),
        notes: border.notes.clone(),
    }
}

/// The border frame drawn on `sheet` (the BORDER layer's long lines): its
/// lower-left and upper-right corners.
pub fn frame_of(sheet: &proto::SheetInfo) -> Option<(DVec2, DVec2)> {
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for l in sheet.format_lines.iter().filter(|l| l.layer == BORDER_LAYER) {
        let (Some(a), Some(b)) = (&l.start, &l.end) else { continue };
        if (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() > sheet.height / 2.0 {
            xs.push(a.x);
        }
        if (a.y - b.y).abs() < 1e-9 && (a.x - b.x).abs() > sheet.width / 2.0 {
            ys.push(a.y);
        }
    }
    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (xs.len() >= 2 && ys.len() >= 2).then(|| (DVec2::new(min(&xs), min(&ys)), DVec2::new(max(&xs), max(&ys))))
}

pub fn border_reply(sheet_name: &str, border: &Border, reply: &proto::DrawSheetFormatResponse) -> String {
    let (lo, hi) = border.frame;
    let mut lines = vec![format!(
        "drew the border on {sheet_name}'s sheet format (layer {BORDER_LAYER}): frame [{}, {}]..[{}, {}] mm, {} x {} zones, {} lines and {} labels.",
        mm(lo.x),
        mm(lo.y),
        mm(hi.x),
        mm(hi.y),
        border.zones[0],
        border.zones[1],
        reply.lines_added,
        reply.notes.len()
    )];
    if reply.lines_deleted > 0 || reply.notes_deleted > 0 {
        lines.push(format!(
            "It replaced the border drawn before ({} lines, {} notes on layer {BORDER_LAYER}).",
            reply.lines_deleted, reply.notes_deleted
        ));
    }
    if let Some(sheet) = &reply.sheet {
        let outside: Vec<&str> = sheet
            .views
            .iter()
            .filter(|v| {
                crate::note::outline(v).is_some_and(|(vlo, vhi)| vlo.x < lo.x || vlo.y < lo.y || vhi.x > hi.x || vhi.y > hi.y)
            })
            .map(|v| v.name.as_str())
            .collect();
        if !outside.is_empty() {
            lines.push(format!("Warning: {} reach(es) past the frame -- move or rescale them.", outside.join(", ")));
        }
        if !sheet.sheet_format.is_empty() && sheet.format_lines.iter().any(|l| l.layer != BORDER_LAYER && l.layer != TITLE_BLOCK_LAYER) {
            lines.push(format!(
                "Note: the sheet format ({}) has lines of its own (likely its own border / title block); they're still \
                 there.",
                sheet.sheet_format
            ));
        }
    }
    lines.push("draw_title_block puts a title block in the frame's lower-right corner.".into());
    lines.join("\n")
}

// ---- Title block --------------------------------------------------------------------

/// A field of a title block: the custom property a note shows.
fn link_names(linked: &str) -> Vec<(String, PropertyOf)> {
    let mut out = Vec::new();
    let mut rest = linked;
    while let Some(i) = rest.find("$PRP") {
        rest = &rest[i + 4..];
        let (of, after) = if let Some(r) = rest.strip_prefix("SHEET:\"") {
            (PropertyOf::Model, r)
        } else if let Some(r) = rest.strip_prefix("MODEL:\"") {
            (PropertyOf::Model, r)
        } else if let Some(r) = rest.strip_prefix(":\"") {
            (PropertyOf::Drawing, r)
        } else {
            continue;
        };
        if let Some(end) = after.find('"') {
            out.push((after[..end].to_string(), of));
            rest = &after[end + 1..];
        }
    }
    out
}

fn link(property: &str, of: PropertyOf) -> String {
    match of {
        PropertyOf::Drawing => format!("$PRP:\"{property}\""),
        PropertyOf::Model => format!("$PRPSHEET:\"{property}\""),
    }
}

fn cell(label: &str, property: &str, of: PropertyOf, width: f64) -> TitleBlockCell {
    TitleBlockCell {
        label: Some(label.into()),
        property: Some(property.into()),
        of: Some(of),
        width_mm: Some(width),
        ..Default::default()
    }
}

fn fixed(label: &str, value: &str, width: f64) -> TitleBlockCell {
    TitleBlockCell { label: Some(label.into()), value: Some(value.into()), width_mm: Some(width), ..Default::default() }
}

pub fn default_rows() -> Vec<TitleBlockRow> {
    use PropertyOf::{Drawing, Model};
    vec![
        TitleBlockRow {
            cells: vec![
                TitleBlockCell { height_mm: Some(5.0), bold: true, ..cell("TITLE", "Title", Drawing, 110.0) },
                TitleBlockCell { height_mm: Some(5.0), ..cell("DWG NO.", "DrawingNo", Drawing, 70.0) },
            ],
            height_mm: Some(14.0),
        },
        TitleBlockRow {
            cells: vec![
                cell("MATERIAL", "Material", Model, 60.0),
                cell("FINISH", "Finish", Model, 50.0),
                fixed("SCALE", "$PRP:\"SW-Sheet Scale\"", 30.0),
                fixed("SHEET", "$PRP:\"SW-Current Sheet\" OF $PRP:\"SW-Total Sheets\"", 40.0),
            ],
            height_mm: None,
        },
        TitleBlockRow {
            cells: vec![
                cell("DRAWN", "DrawnBy", Drawing, 45.0),
                cell("DATE", "DrawnDate", Drawing, 30.0),
                cell("CHECKED", "CheckedBy", Drawing, 45.0),
                cell("DATE", "CheckedDate", Drawing, 30.0),
                cell("REV", "Revision", Drawing, 30.0),
            ],
            height_mm: None,
        },
        TitleBlockRow {
            cells: vec![cell("COMPANY", "CompanyName", Drawing, 110.0), fixed("FILE", "$PRP:\"SW-File Name\"", 70.0)],
            height_mm: None,
        },
    ]
}

/// A property a new title block sets.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyValue {
    pub name: String,
    pub of: PropertyOf,
    pub value: String,
}

pub struct TitleBlock {
    pub lines: Vec<proto::FormatLine>,
    pub notes: Vec<proto::FormatNote>,
    pub bounds: (DVec2, DVec2),
    pub properties: Vec<PropertyValue>,
    /// (label, what the cell shows) per cell, for the reply.
    pub cells: Vec<(String, String)>,
}

/// The title block's lines and notes with its lower-right corner at
/// `corner`; `skip_edges` leaves out its right and bottom edges (they're
/// the border frame's).
pub fn title_block(rows: &[TitleBlockRow], width: f64, corner: DVec2, skip_edges: bool, fields: bool) -> Result<TitleBlock, McpError> {
    if rows.is_empty() || rows.len() > 30 {
        return Err(invalid("a title block has 1..30 rows"));
    }
    if !(width.is_finite() && width >= 0.02 && width <= 1.0) {
        return Err(invalid("width_mm must be 20..1000"));
    }
    let heights: Vec<f64> = rows
        .iter()
        .map(|r| r.height_mm.unwrap_or(9.0) / MM_PER_M)
        .collect();
    if heights.iter().any(|h| !(h.is_finite() && *h >= 0.004 && *h <= 0.1)) {
        return Err(invalid("row heights are 4..100 mm"));
    }
    let total: f64 = heights.iter().sum();
    let lo = DVec2::new(corner.x - width, corner.y);
    let hi = DVec2::new(corner.x, corner.y + total);
    let mut lines = vec![line(DVec2::new(lo.x, lo.y), DVec2::new(lo.x, hi.y)), line(DVec2::new(lo.x, hi.y), hi)];
    if !skip_edges {
        lines.push(line(lo, DVec2::new(hi.x, lo.y)));
        lines.push(line(DVec2::new(hi.x, lo.y), hi));
    }
    let mut notes = Vec::new();
    let mut properties = Vec::new();
    let mut cells_out = Vec::new();
    let mut top = hi.y;
    for (r, (row, h)) in rows.iter().zip(&heights).enumerate() {
        if row.cells.is_empty() || row.cells.len() > 20 {
            return Err(invalid(format!("row {} needs 1..20 cells", r + 1)));
        }
        if r > 0 {
            lines.push(line(DVec2::new(lo.x, top), DVec2::new(hi.x, top)));
        }
        // Widths: given ones, the rest share what's left.
        let given: f64 = row.cells.iter().filter_map(|c| c.width_mm).map(|w| w / MM_PER_M).sum();
        let open = row.cells.iter().filter(|c| c.width_mm.is_none()).count();
        if row.cells.iter().filter_map(|c| c.width_mm).any(|w| !(w.is_finite() && w > 0.0)) {
            return Err(invalid(format!("row {}: cell widths are mm above 0", r + 1)));
        }
        let share = if open > 0 { (width - given) / open as f64 } else { 0.0 };
        if given > width + 1e-9 || (open > 0 && share < 0.005) {
            return Err(invalid(format!(
                "row {}'s cells are {:.0} mm wide in a {:.0} mm title block",
                r + 1,
                (given + share.max(0.0) * open as f64) * MM_PER_M,
                width * MM_PER_M
            )));
        }
        let mut x = lo.x;
        for (i, c) in row.cells.iter().enumerate() {
            let w = c.width_mm.map(|w| w / MM_PER_M).unwrap_or(share);
            // The last cell of a row ends at the right edge whatever the rounding.
            let w = if i + 1 == row.cells.len() { hi.x - x } else { w };
            if i > 0 {
                lines.push(line(DVec2::new(x, top), DVec2::new(x, top - h)));
            }
            let label = text(c.label.as_deref());
            let property = text(c.property.as_deref());
            let of = c.of.unwrap_or_default();
            let value = c.value.clone().unwrap_or_default();
            let shown = if !property.is_empty() {
                if property.starts_with("SW-") && c.value.is_some() {
                    return Err(invalid(format!("{property} is SolidWorks' own (read-only); leave its value out")));
                }
                if c.value.is_some() {
                    properties.push(PropertyValue { name: property.clone(), of, value: value.clone() });
                }
                link(&property, of)
            } else {
                value
            };
            let label_band = if label.is_empty() { 0.0 } else { 0.0008 + LABEL_TEXT * TEXT_BOX + 0.0004 };
            if !label.is_empty() {
                notes.push(left(&label, DVec2::new(x + CELL_PAD, top - 0.0008), LABEL_TEXT));
            }
            if !shown.is_empty() {
                let room = h - label_band - 0.001;
                let height = c
                    .height_mm
                    .map(|v| v / MM_PER_M)
                    .unwrap_or(VALUE_TEXT)
                    .min(room / TEXT_BOX)
                    .max(0.0015);
                if c.height_mm.is_some_and(|v| !(v.is_finite() && v > 0.0)) {
                    return Err(invalid(format!("row {}: height_mm must be above 0", r + 1)));
                }
                // Below the label, centered in what's left of the cell.
                let y = top - label_band - ((h - label_band) - height * TEXT_BOX).max(0.0) / 2.0;
                let mut note = left(&shown, DVec2::new(x + CELL_PAD * 1.25, y), height);
                note.bold = c.bold;
                note.field = fields && !property.is_empty();
                notes.push(note);
            }
            cells_out.push((label, shown));
            x += w;
        }
        top -= h;
    }
    Ok(TitleBlock { lines, notes, bounds: (lo, hi), properties, cells: cells_out })
}

pub fn title_block_request(sheet: &str, block: &TitleBlock) -> proto::DrawSheetFormatRequest {
    proto::DrawSheetFormatRequest {
        sheet_name: sheet.to_string(),
        layer: TITLE_BLOCK_LAYER.into(),
        layer_color: 0,
        layer_weight: LineWeight::Normal.sw(),
        clear: true,
        lines: block.lines.clone(),
        notes: block.notes.clone(),
    }
}

/// The custom property requests for the values a title block sets: one for
/// the drawing, one for the sheet's model.
pub fn property_requests(sheet: &str, values: &[PropertyValue], replace_links: bool) -> Vec<proto::CustomPropertiesRequest> {
    let mut out = Vec::new();
    for (of, sheet_name) in [(PropertyOf::Drawing, String::new()), (PropertyOf::Model, sheet.to_string())] {
        let set: Vec<proto::CustomProperty> = values
            .iter()
            .filter(|v| v.of == of)
            .map(|v| proto::CustomProperty { name: v.name.clone(), value: v.value.clone(), ..Default::default() })
            .collect();
        if !set.is_empty() {
            out.push(proto::CustomPropertiesRequest { sheet_name: sheet_name.clone(), set, replace_links, ..Default::default() });
        }
    }
    out
}

/// The sheet (by name, else the active one) of an inspect reply.
pub fn find_sheet<'a>(reply: &'a proto::InspectDrawingResponse, name: &str) -> Result<&'a proto::SheetInfo, McpError> {
    let found = if name.is_empty() {
        reply.sheets.iter().find(|s| s.active).or(reply.sheets.first())
    } else {
        reply.sheets.iter().find(|s| s.name == name).or_else(|| reply.sheets.iter().find(|s| s.name.eq_ignore_ascii_case(name)))
    };
    found.ok_or_else(|| {
        let names: Vec<&str> = reply.sheets.iter().map(|s| s.name.as_str()).collect();
        invalid(format!("the drawing has no sheet {name:?} (its sheets: {})", names.join(", ")))
    })
}

pub fn title_block_reply(
    sheet: &proto::SheetInfo,
    block: &TitleBlock,
    reply: &proto::DrawSheetFormatResponse,
    set: &[PropertyValue],
) -> String {
    let (lo, hi) = block.bounds;
    let mut lines = vec![format!(
        "drew the title block on {}'s sheet format (layer {TITLE_BLOCK_LAYER}): [{}, {}]..[{}, {}] mm, {} lines, {} notes.",
        sheet.name,
        mm(lo.x),
        mm(lo.y),
        mm(hi.x),
        mm(hi.y),
        reply.lines_added,
        reply.notes.len()
    )];
    if reply.lines_deleted > 0 || reply.notes_deleted > 0 {
        lines.push(format!("It replaced the title block drawn before ({} lines, {} notes).", reply.lines_deleted, reply.notes_deleted));
    }
    if reply.fields_defined > 0 {
        lines.push(format!("{} value notes are title block fields (users can fill them in on the sheet).", reply.fields_defined));
    } else if !reply.fields_problem.is_empty() {
        lines.push(format!("Note: title block fields weren't defined: {}", reply.fields_problem));
    }
    if !set.is_empty() {
        let list: Vec<String> = set
            .iter()
            .map(|v| format!("{}={:?} ({})", v.name, v.value, if v.of == PropertyOf::Model { "model" } else { "drawing" }))
            .collect();
        lines.push(format!("Set {}.", list.join(", ")));
    }
    if let Some(s) = &reply.sheet {
        let mut obstacles = Obstacles::of(s, "");
        obstacles.lines.clear();
        let hits = obstacles.hits(lo, hi, 0.0);
        if !hits.is_empty() {
            lines.push(format!("Warning: it overlaps {} -- move them (edit_drawing_view / edit_drawing_table).", hits.join(", ")));
        }
    }
    lines.push("fill_title_block sets the values (inspect_title_block lists the fields).".into());
    let cells: Vec<Value> = block
        .cells
        .iter()
        .map(|(label, shown)| json!({ "label": label, "shows": shown }))
        .collect();
    lines.push(Value::Array(cells).to_string());
    lines.join("\n")
}

// ---- Fields of any title block ---------------------------------------------------

/// A title block field: a property-linked note of the sheet format.
pub struct Field {
    pub property: String,
    pub of: PropertyOf,
    pub note: String,
    pub shows: String,
    pub at: Option<DVec2>,
}

/// The property-linked notes of a sheet's format.
pub fn fields(sheet: &proto::SheetInfo) -> Vec<Field> {
    let mut out = Vec::new();
    for a in sheet.annotations.iter().filter(|a| a.owner() == proto::AnnotationOwner::SheetFormat) {
        let Some(n) = &a.note else { continue };
        for (property, of) in link_names(&n.linked_text) {
            out.push(Field {
                property,
                of,
                note: a.name.clone(),
                shows: a.text.first().cloned().unwrap_or_default().replace("\r\n", "\n"),
                at: a.position.as_ref().map(|p| DVec2::new(p.x, p.y)),
            });
        }
    }
    out
}

pub fn fields_request() -> proto::InspectDrawingRequest {
    proto::InspectDrawingRequest { include_annotations: true, ..Default::default() }
}

pub fn fields_json(fields: &[Field], drawing: &[proto::CustomProperty], model: Option<&proto::CustomPropertiesResponse>) -> Value {
    let list: Vec<Value> = fields
        .iter()
        .map(|f| {
            let mut e = Map::new();
            e.insert("property".into(), json!(f.property));
            e.insert("of".into(), json!(if f.of == PropertyOf::Model { "model" } else { "drawing" }));
            let props = if f.of == PropertyOf::Model { model.map(|m| m.properties.as_slice()).unwrap_or(&[]) } else { drawing };
            if f.property.starts_with("SW-") {
                e.insert("read_only".into(), json!(true));
            } else if let Some(p) = props.iter().find(|p| p.name.eq_ignore_ascii_case(&f.property)) {
                e.insert("value".into(), json!(p.value));
            } else {
                e.insert("value".into(), Value::Null);
            }
            e.insert("note".into(), json!(f.note));
            e.insert("note_shows".into(), json!(f.shows));
            if let Some(at) = f.at {
                e.insert("at_mm".into(), sheet_mm(at));
            }
            Value::Object(e)
        })
        .collect();
    Value::Array(list)
}

/// Splits fill_title_block's values by where the title block takes each
/// from. Unknown names go to the drawing with `allow_unlisted`.
pub fn route_values(
    fields: &[Field],
    values: &BTreeMap<String, String>,
    allow_unlisted: bool,
) -> Result<(Vec<PropertyValue>, Vec<String>), McpError> {
    let mut out = Vec::new();
    let mut unlisted = Vec::new();
    for (name, value) in values {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("an empty property name"));
        }
        if name.starts_with("SW-") {
            return Err(invalid(format!("{name} is SolidWorks' own and can't be set")));
        }
        let mut wheres: Vec<PropertyOf> =
            fields.iter().filter(|f| f.property.eq_ignore_ascii_case(name)).map(|f| f.of).collect();
        wheres.dedup();
        // The name as the title block spells it.
        let spelled = fields.iter().find(|f| f.property.eq_ignore_ascii_case(name)).map(|f| f.property.clone());
        if wheres.is_empty() {
            if !allow_unlisted {
                let mut known: Vec<&str> = fields.iter().filter(|f| !f.property.starts_with("SW-")).map(|f| f.property.as_str()).collect();
                known.sort();
                known.dedup();
                return Err(invalid(format!(
                    "the title block shows no property {name:?} (it shows: {}) -- allow_unlisted sets it on the \
                     drawing anyway",
                    if known.is_empty() { "none".to_string() } else { known.join(", ") }
                )));
            }
            unlisted.push(name.to_string());
            wheres.push(PropertyOf::Drawing);
        }
        for of in [PropertyOf::Drawing, PropertyOf::Model] {
            if wheres.contains(&of) {
                out.push(PropertyValue { name: spelled.clone().unwrap_or_else(|| name.to_string()), of, value: value.clone() });
            }
        }
    }
    Ok((out, unlisted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_parsed() {
        assert_eq!(
            link_names("WEIGHT: $PRP:\"Weight\"$PRPSHEET:\"Weight\""),
            vec![("Weight".into(), PropertyOf::Drawing), ("Weight".into(), PropertyOf::Model)]
        );
        assert_eq!(link_names("SHEET $PRP:\"SW-Current Sheet\" OF $PRP:\"SW-Total Sheets\"").len(), 2);
        assert!(link_names("plain").is_empty());
    }

    #[test]
    fn an_a3_border_has_even_zones_and_ticks() {
        let b = border(DVec2::new(0.42, 0.297), &DrawSheetBorderParams::default()).unwrap();
        assert_eq!(b.zones, [8, 6]);
        assert!((b.frame.0 - DVec2::new(0.02, 0.01)).length() < 1e-12);
        assert!((b.frame.1 - DVec2::new(0.41, 0.287)).length() < 1e-12);
        // 4 frame lines, ticks (7 top + 7 bottom + 5 left + 5 right), 4 centering marks.
        assert_eq!(b.lines.len(), 4 + 14 + 10 + 4);
        // Labels 1..8 twice, A..F twice.
        assert_eq!(b.notes.len(), 16 + 12);
        assert!(b.notes.iter().any(|n| n.text == "F"));
    }

    #[test]
    fn title_block_cells_fill_their_rows() {
        let tb = title_block(&default_rows(), 0.18, DVec2::new(0.41, 0.01), true, true).unwrap();
        assert!((tb.bounds.0 - DVec2::new(0.23, 0.01)).length() < 1e-12);
        assert!((tb.bounds.1.y - (0.01 + 0.041)).abs() < 1e-12);
        // Every note inside the block.
        for n in &tb.notes {
            let p = n.position.as_ref().unwrap();
            assert!(p.x > 0.23 && p.x < 0.41 && p.y > 0.01 && p.y <= 0.051 + 1e-9, "{} at {:?}", n.text, p);
        }
        assert!(tb.notes.iter().any(|n| n.text == "$PRPSHEET:\"Material\"" && n.field));
        let bad = vec![TitleBlockRow { cells: vec![TitleBlockCell { width_mm: Some(200.0), ..Default::default() }], height_mm: None }];
        assert!(title_block(&bad, 0.18, DVec2::new(0.41, 0.01), false, true).is_err());
    }

    #[test]
    fn values_go_where_the_title_block_reads_them() {
        let f = |p: &str, of| Field { property: p.into(), of, note: "N".into(), shows: String::new(), at: None };
        let fields = vec![f("DrawnBy", PropertyOf::Drawing), f("Material", PropertyOf::Model)];
        let mut values = BTreeMap::new();
        values.insert("drawnby".to_string(), "MW".to_string());
        values.insert("Material".to_string(), "Steel".to_string());
        let (routed, _) = route_values(&fields, &values, false).unwrap();
        assert_eq!(routed[0], PropertyValue { name: "Material".into(), of: PropertyOf::Model, value: "Steel".into() });
        assert_eq!(routed[1].name, "DrawnBy");
        values.insert("Nope".into(), "x".into());
        assert!(route_values(&fields, &values, false).is_err());
        assert_eq!(route_values(&fields, &values, true).unwrap().1, vec!["Nope".to_string()]);
    }
}
