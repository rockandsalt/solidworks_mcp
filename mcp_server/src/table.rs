//! The drawing table and balloon tools: inspect_drawing_tables,
//! add_drawing_table, edit_drawing_table and add_balloons.
//!
//! SolidWorks makes the tables (a bill of materials from an assembly view,
//! a hole table from holes picked in a view, a general table of text) and
//! keeps a BOM's rows and a hole table's locations up to date with the
//! model; balloons point at components and show their BOM item numbers.
//! What happens here: turning ids and names into requests, a table's box on
//! the sheet from its anchor corner and row / column sizes, a free spot for
//! a new table clear of the views, the other tables and the sheet format's
//! lines, and the replies.

use glam::DVec2;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::drawing::{self, sheet_mm};
use crate::inspect::{IdRegistry, MM_PER_M, round};
use crate::note;

/// The add-in's limits.
pub const MAX_BALLOONS: usize = 200;
pub const MAX_ROWS: usize = 500;
pub const MAX_COLUMNS: usize = 50;

/// A new table is placed at least this far inside the sheet's edges, and
/// this far from views, other tables and the sheet format's lines.
const MARGIN: f64 = 0.010;
const GAP: f64 = 0.004;
/// Views get more room: their dimensions, balloons and a hole table's origin
/// marker sit just outside their outlines.
const VIEW_GAP: f64 = 0.010;
/// Grid a free spot is searched on.
const STEP: f64 = 0.0025;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

fn point(at_mm: [f64; 2], what: &str) -> Result<DVec2, McpError> {
    if !at_mm.iter().all(|v| v.is_finite()) {
        return Err(invalid(format!("{what} must be finite")));
    }
    Ok(DVec2::new(at_mm[0], at_mm[1]) / MM_PER_M)
}

fn vec3(p: DVec2) -> proto::Vec3 {
    proto::Vec3 { x: p.x, y: p.y, z: 0.0 }
}

fn mm2(p: DVec2) -> String {
    format!("[{}, {}]", round(p.x * MM_PER_M, 1), round(p.y * MM_PER_M, 1))
}

// ---- Words ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TableKindParam {
    /// Bill of materials of an assembly view.
    Bom,
    /// Hole table: the holes picked in a view with their X / Y from an origin vertex.
    Hole,
    /// Rows and columns of text (a coordinate list, a revision list, ...).
    General,
}

impl TableKindParam {
    fn sw(self) -> proto::TableKind {
        match self {
            TableKindParam::Bom => proto::TableKind::Bom,
            TableKindParam::Hole => proto::TableKind::Hole,
            TableKindParam::General => proto::TableKind::General,
        }
    }
}

pub fn table_type_name(t: i32) -> &'static str {
    match t {
        0 => "general",
        1 => "hole",
        2 => "bom",
        3 => "revision",
        4 => "weldment_cut_list",
        5 => "title_block",
        6 => "weld",
        7 => "bend",
        8 => "punch",
        9 => "general_tolerance",
        10 => "family",
        _ => "other",
    }
}

/// Which corner of a table its position is.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Anchor {
    fn sw(self) -> i32 {
        match self {
            Anchor::TopLeft => 1,
            Anchor::TopRight => 2,
            Anchor::BottomLeft => 3,
            Anchor::BottomRight => 4,
        }
    }

    fn of(sw: i32) -> Anchor {
        match sw {
            2 => Anchor::TopRight,
            3 => Anchor::BottomLeft,
            4 => Anchor::BottomRight,
            _ => Anchor::TopLeft,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Anchor::TopLeft => "top_left",
            Anchor::TopRight => "top_right",
            Anchor::BottomLeft => "bottom_left",
            Anchor::BottomRight => "bottom_right",
        }
    }

    /// The corner of the box (lo, hi) this anchor names.
    fn corner(self, lo: DVec2, hi: DVec2) -> DVec2 {
        match self {
            Anchor::TopLeft => DVec2::new(lo.x, hi.y),
            Anchor::TopRight => hi,
            Anchor::BottomLeft => lo,
            Anchor::BottomRight => DVec2::new(hi.x, lo.y),
        }
    }

    /// The box of a `size` table whose anchor corner is at `at`.
    fn box_at(self, at: DVec2, size: DVec2) -> (DVec2, DVec2) {
        let lo = match self {
            Anchor::TopLeft => DVec2::new(at.x, at.y - size.y),
            Anchor::TopRight => at - size,
            Anchor::BottomLeft => at,
            Anchor::BottomRight => DVec2::new(at.x - size.x, at.y),
        };
        (lo, lo + size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BomType {
    /// The assembly's own components; a subassembly is one row.
    TopLevel,
    /// Every part, subassemblies dissolved.
    PartsOnly,
    /// Subassemblies with their parts indented under them (1, 1.1, 1.2, ...).
    Indented,
}

impl BomType {
    fn sw(self) -> i32 {
        match self {
            BomType::PartsOnly => 1,
            BomType::TopLevel => 2,
            BomType::Indented => 3,
        }
    }
}

fn bom_type_name(t: i32) -> &'static str {
    match t {
        1 => "parts_only",
        2 => "top_level",
        3 => "indented",
        4 => "flattened",
        _ => "other",
    }
}

fn column_type_name(t: i32) -> Option<&'static str> {
    Some(match t {
        101 => "x_location",
        102 => "y_location",
        103 => "tag",
        104 | 203 => "quantity",
        105 => "size",
        201 => "part_number",
        202 => "item_number",
        204 => "custom_property",
        205 => "unit_of_measure",
        206 => "equation",
        207 => "component_reference",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BalloonLayout {
    Square,
    Circle,
    Top,
    Bottom,
    Right,
    Left,
}

impl BalloonLayout {
    fn sw(self) -> i32 {
        match self {
            BalloonLayout::Square => 1,
            BalloonLayout::Circle => 2,
            BalloonLayout::Top => 3,
            BalloonLayout::Bottom => 4,
            BalloonLayout::Right => 5,
            BalloonLayout::Left => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BalloonStyle {
    Circle,
    SplitCircle,
    Box,
    Triangle,
    Hexagon,
    Diamond,
    Pentagon,
    Underline,
    None,
}

impl BalloonStyle {
    fn sw(self) -> i32 {
        match self {
            BalloonStyle::None => 0,
            BalloonStyle::Circle => 1,
            BalloonStyle::Triangle => 2,
            BalloonStyle::Hexagon => 3,
            BalloonStyle::Box => 4,
            BalloonStyle::Diamond => 5,
            BalloonStyle::Pentagon => 6,
            BalloonStyle::SplitCircle => 7,
            BalloonStyle::Underline => 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BalloonText {
    /// The component's item number in the BOM.
    ItemNumber,
    Quantity,
    PartNumber,
    FileName,
    ComponentReference,
    /// The text given with it.
    Custom,
}

impl BalloonText {
    fn sw(self) -> i32 {
        match self {
            BalloonText::Custom => 0,
            BalloonText::ItemNumber => 1,
            BalloonText::Quantity => 2,
            BalloonText::ComponentReference => 4,
            BalloonText::PartNumber => 6,
            BalloonText::FileName => 7,
        }
    }
}

// ---- Params -----------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectDrawingTablesParams {
    /// Only this sheet. Default: every sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Only this table, by name (e.g. "DetailItem7").
    #[serde(default)]
    pub name: Option<String>,
    /// Only this kind: bom, hole, general, revision, ...
    #[serde(default)]
    pub kind: Option<String>,
    /// Also list the sheet format's lines (border, title block grid) per sheet.
    #[serde(default)]
    pub include_format_lines: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddDrawingTableParams {
    /// bom (bill of materials of an assembly view), hole (hole table: holes of a view with
    /// their X / Y from an origin vertex and their size) or general (rows of text you give).
    pub kind: TableKindParam,
    /// bom / hole: the view it's of, e.g. "Drawing View1". The table goes on its sheet.
    #[serde(default)]
    pub view: Option<String>,
    /// general: the sheet it goes on. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Where the table's `anchor` corner goes, [x, y] sheet mm from the sheet's lower-left
    /// corner. Default: a free spot clear of the views, other tables and the title block (a BOM
    /// toward the top right, a hole table near its view, a general table toward the top left).
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// Which corner at_mm is: top_left (default), top_right, bottom_left, bottom_right. The
    /// table grows away from it as rows are added.
    #[serde(default)]
    pub anchor: Option<Anchor>,
    /// A table template file: an absolute path or a name in SolidWorks' table template folder
    /// (e.g. "bom-material", "standard hole table--numbers"). Default: bom-standard, standard
    /// hole table--letters, none (general).
    #[serde(default)]
    pub template: Option<String>,
    /// bom: top_level (default; a subassembly is one row), parts_only, indented.
    #[serde(default)]
    pub bom_type: Option<BomType>,
    /// bom: the assembly configuration to list. Default: the view's.
    #[serde(default)]
    pub configuration: Option<String>,
    /// hole: the origin X / Y are measured from: a vertex id of the view from inspect_drawing
    /// ("view_vertex:4"), e.g. the part's lower-left corner.
    #[serde(default)]
    pub origin: Option<String>,
    /// hole: the holes, one edge id each (their circles seen face-on, "view_edge:7").
    #[serde(default)]
    pub holes: Vec<String>,
    /// hole: the first tag, "A" (default; A1, A2, ...) or "1".
    #[serde(default)]
    pub start_tag: Option<String>,
    /// general: the table's text, row by row from the top (the first row can be the column
    /// titles), e.g. [["TAG", "X", "Y"], ["P1", "10.0", "25.0"]]. Columns = the longest row.
    #[serde(default)]
    pub rows: Vec<Vec<String>>,
    /// general: more empty columns than `rows` has.
    #[serde(default)]
    pub columns: Option<usize>,
    /// Column widths in mm from the first column; 0 keeps one.
    #[serde(default)]
    pub column_widths_mm: Vec<f64>,
    /// A title shown above the table.
    #[serde(default)]
    pub title: Option<String>,
    /// The drawing layer to put it on.
    #[serde(default)]
    pub layer: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CellParam {
    /// 0 = the top row (column titles count as a row).
    pub row: i32,
    pub column: i32,
    pub text: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditDrawingTableParams {
    /// The table's name as inspect_drawing_tables lists it, e.g. "DetailItem7".
    pub name: String,
    /// Only look for it on this sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Cell texts to set, after add_rows / delete_rows. In a BOM, typing over an item number
    /// renumbers that item (its balloons follow); cells SolidWorks computes (quantities, hole
    /// locations) may be refused.
    #[serde(default)]
    pub cells: Vec<CellParam>,
    /// General tables: rows to add at the bottom, with their text.
    #[serde(default)]
    pub add_rows: Vec<Vec<String>>,
    /// General tables: rows to delete, by their index before this change.
    #[serde(default)]
    pub delete_rows: Vec<i32>,
    /// Column widths in mm from the first column; 0 keeps one.
    #[serde(default)]
    pub column_widths_mm: Vec<f64>,
    /// A title shown above the table; "" removes it.
    #[serde(default)]
    pub title: Option<String>,
    /// The drawing layer; "" for none.
    #[serde(default)]
    pub layer: Option<String>,
    /// Move the table's anchor corner (see anchor) to [x, y] sheet mm.
    #[serde(default)]
    pub at_mm: Option<[f64; 2]>,
    /// Instead, shift it by [dx, dy] mm.
    #[serde(default)]
    pub move_by_mm: Option<[f64; 2]>,
    /// Which corner the table is anchored by (it grows away from it); with at_mm, the corner
    /// at_mm places.
    #[serde(default)]
    pub anchor: Option<Anchor>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AddBalloonsParams {
    /// The assembly view, e.g. "Drawing View1".
    pub view: String,
    /// Balloon these components (names as inspect_drawing lists them, e.g. "bracket-1"), one
    /// balloon each, its leader to one of the component's visible edges. Default (with no
    /// attach_to either): every component of the view (SolidWorks' Auto Balloon), one per BOM
    /// item unless every_instance.
    #[serde(default)]
    pub components: Vec<String>,
    /// Instead, balloon the components of these view entities (ids from inspect_drawing:
    /// "view_edge:3"), the leader to that entity.
    #[serde(default)]
    pub attach_to: Vec<String>,
    /// With attach_to: where each leader's arrow touches its entity, [x, y] sheet mm.
    #[serde(default)]
    pub arrow_at_mm: Vec<[f64; 2]>,
    /// Where each balloon goes: a list with one [x, y] sheet mm per balloon (components then
    /// attach_to, in order), e.g. [[150, 250]] for one. Default: where SolidWorks puts it, next
    /// to its arrow.
    #[serde(default)]
    pub at_mm: Vec<[f64; 2]>,
    /// Auto Balloon's arrangement around the view: square (default), circle, top, bottom,
    /// left, right.
    #[serde(default)]
    pub layout: Option<BalloonLayout>,
    /// Auto Balloon: a balloon on every instance, not one per BOM item.
    #[serde(default)]
    pub every_instance: bool,
    /// circle (default), split_circle (two texts), box, triangle, hexagon, diamond, pentagon,
    /// underline, none.
    #[serde(default)]
    pub style: Option<BalloonStyle>,
    /// How many characters the balloon is sized for, 1..5; 0 = tight around the text.
    #[serde(default)]
    pub size_chars: Option<u8>,
    /// What it shows: item_number (default), quantity, part_number, file_name,
    /// component_reference, custom (with `custom_text`).
    #[serde(default)]
    pub text: Option<BalloonText>,
    #[serde(default)]
    pub custom_text: Option<String>,
    /// split_circle: what the lower half shows (default quantity).
    #[serde(default)]
    pub lower_text: Option<BalloonText>,
    #[serde(default)]
    pub lower_custom_text: Option<String>,
    /// The drawing layer to put them on.
    #[serde(default)]
    pub layer: Option<String>,
}

// ---- Geometry ---------------------------------------------------------------------

/// A table's size on the sheet: its shown columns' widths and rows' heights.
fn table_size(t: &proto::TableInfo) -> DVec2 {
    let w: f64 = t
        .column_widths
        .iter()
        .enumerate()
        .filter(|(i, _)| !t.column_hidden.get(*i).copied().unwrap_or(false))
        .map(|(_, w)| *w)
        .sum();
    let h: f64 = t
        .row_heights
        .iter()
        .enumerate()
        .filter(|(i, _)| !t.rows.get(*i).is_some_and(|r| r.hidden))
        .map(|(_, h)| *h)
        .sum();
    DVec2::new(w, h + t.title_height)
}

/// A table's box on the sheet, from its anchor corner and size.
pub fn table_box(t: &proto::TableInfo) -> Option<(DVec2, DVec2)> {
    let p = t.position.as_ref()?;
    Some(Anchor::of(t.anchor).box_at(DVec2::new(p.x, p.y), table_size(t)))
}

fn boxes_overlap(a: (DVec2, DVec2), b: (DVec2, DVec2), gap: f64) -> bool {
    a.0.x < b.1.x + gap && b.0.x < a.1.x + gap && a.0.y < b.1.y + gap && b.0.y < a.1.y + gap
}

/// Whether segment a-b passes through the box grown by `gap`.
fn segment_hits_box(a: DVec2, b: DVec2, lo: DVec2, hi: DVec2, gap: f64) -> bool {
    let (lo, hi) = (lo - DVec2::splat(gap), hi + DVec2::splat(gap));
    // Liang-Barsky clipping.
    let d = b - a;
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (p, q) in [(-d.x, a.x - lo.x), (d.x, hi.x - a.x), (-d.y, a.y - lo.y), (d.y, hi.y - a.y)] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

/// What a new table must stay clear of on a sheet.
pub struct Obstacles {
    pub views: Vec<(String, (DVec2, DVec2))>,
    pub boxes: Vec<(String, (DVec2, DVec2))>,
    pub lines: Vec<(DVec2, DVec2)>,
    pub size: DVec2,
}

impl Obstacles {
    /// The sheet's views, tables (but `skip`) and sheet format lines.
    pub fn of(sheet: &proto::SheetInfo, skip: &str) -> Obstacles {
        let views = sheet.views.iter().filter_map(|v| Some((v.name.clone(), note::outline(v)?))).collect();
        let boxes = sheet.tables.iter().filter(|t| t.name != skip).filter_map(|t| Some((t.name.clone(), table_box(t)?))).collect();
        let lines = sheet
            .format_lines
            .iter()
            .filter_map(|l| {
                let (a, b) = (l.start.as_ref()?, l.end.as_ref()?);
                Some((DVec2::new(a.x, a.y), DVec2::new(b.x, b.y)))
            })
            .collect();
        Obstacles { views, boxes, lines, size: DVec2::new(sheet.width, sheet.height) }
    }

    /// The things (lo, hi) runs into: names of boxes, "the sheet format" for lines, "the
    /// sheet's edge".
    pub fn hits(&self, lo: DVec2, hi: DVec2, gap: f64) -> Vec<String> {
        self.hits_with(lo, hi, gap, gap)
    }

    fn hits_with(&self, lo: DVec2, hi: DVec2, gap: f64, view_gap: f64) -> Vec<String> {
        let mut out: Vec<String> = self
            .views
            .iter()
            .filter(|(_, b)| boxes_overlap((lo, hi), *b, view_gap))
            .chain(self.boxes.iter().filter(|(_, b)| boxes_overlap((lo, hi), *b, gap)))
            .map(|(name, _)| name.clone())
            .collect();
        if self.lines.iter().any(|(a, b)| segment_hits_box(*a, *b, lo, hi, gap)) {
            out.push("the sheet format's lines (border / title block)".into());
        }
        if self.size.x > 0.0 && (lo.x < -1e-6 || lo.y < -1e-6 || hi.x > self.size.x + 1e-6 || hi.y > self.size.y + 1e-6) {
            out.push("the sheet's edge".into());
        }
        out
    }

    fn free(&self, lo: DVec2, hi: DVec2) -> bool {
        lo.x >= MARGIN
            && lo.y >= MARGIN
            && hi.x <= self.size.x - MARGIN
            && hi.y <= self.size.y - MARGIN
            && self.hits_with(lo, hi, GAP, VIEW_GAP).is_empty()
    }

    /// The free box of `size` whose `corner` is nearest `target`, searched on a grid.
    pub fn free_spot(&self, size: DVec2, corner: Anchor, target: DVec2) -> Option<(DVec2, DVec2)> {
        if self.size.x <= 0.0 || self.size.y <= 0.0 {
            return None;
        }
        let mut best: Option<(f64, (DVec2, DVec2))> = None;
        let nx = ((self.size.x - size.x) / STEP).floor() as i64;
        let ny = ((self.size.y - size.y) / STEP).floor() as i64;
        for i in 0..=nx.max(-1) {
            for j in 0..=ny.max(-1) {
                let lo = DVec2::new(i as f64 * STEP, j as f64 * STEP);
                let hi = lo + size;
                let d = corner.corner(lo, hi).distance(target);
                if best.is_some_and(|(bd, _)| bd <= d) || !self.free(lo, hi) {
                    continue;
                }
                best = Some((d, (lo, hi)));
            }
        }
        best.map(|(_, b)| b)
    }
}

/// Where a new table should go when no position was given: (corner,
/// target) for free_spot. A BOM toward the sheet's top right, a hole table
/// under its view, a general table toward the top left.
fn placement_goal(sheet: &proto::SheetInfo, kind: TableKindParam, view: Option<&str>) -> (Anchor, DVec2) {
    let size = DVec2::new(sheet.width, sheet.height);
    match kind {
        TableKindParam::Bom => (Anchor::TopRight, size),
        TableKindParam::Hole => {
            let outline = view.and_then(|v| sheet.views.iter().find(|x| x.name == v)).and_then(note::outline);
            match outline {
                Some((lo, _)) => (Anchor::TopLeft, DVec2::new(lo.x, lo.y)),
                None => (Anchor::TopLeft, DVec2::new(0.0, size.y)),
            }
        }
        TableKindParam::General => (Anchor::TopLeft, DVec2::new(0.0, size.y)),
    }
}

/// A free spot for the new `table` on `sheet` if it isn't in one already
/// (it was placed at the add-in's default corner): the position to move
/// its anchor corner to.
pub fn relocation(sheet: &proto::SheetInfo, table: &proto::TableInfo, kind: TableKindParam, view: Option<&str>) -> Option<DVec2> {
    let (lo, hi) = table_box(table)?;
    let obstacles = Obstacles::of(sheet, &table.name);
    let (corner, target) = placement_goal(sheet, kind, view);
    let (nlo, nhi) = obstacles.free_spot(hi - lo, corner, target)?;
    if (nlo - lo).length() < 1e-6 {
        return None;
    }
    Some(Anchor::of(table.anchor).corner(nlo, nhi))
}

// ---- Requests ---------------------------------------------------------------------

fn widths(mm: &[f64]) -> Result<Vec<f64>, McpError> {
    if mm.len() > MAX_COLUMNS {
        return Err(invalid(format!("at most {MAX_COLUMNS} column widths")));
    }
    mm.iter()
        .map(|w| {
            if w.is_finite() && *w >= 0.0 && *w < 1000.0 {
                Ok(w / MM_PER_M)
            } else {
                Err(invalid("column widths are mm, 0 or more"))
            }
        })
        .collect()
}

fn entity(ids: &IdRegistry, id: &str, view: &str, what: &str) -> Result<proto::DrawingEntityRef, McpError> {
    let r = drawing::decode_ref(ids, id)?;
    if r.view_name != view {
        return Err(invalid(format!("{what} {id} is in {}, not {view}", r.view_name)));
    }
    Ok(r)
}

pub fn add_request(ids: &IdRegistry, params: &AddDrawingTableParams) -> Result<proto::SetupDrawingTableRequest, McpError> {
    let view = text(params.view.as_deref());
    let mut request = proto::SetupDrawingTableRequest {
        kind: params.kind.sw() as i32,
        view_name: view.clone(),
        sheet_name: text(params.sheet.as_deref()),
        template_file: text(params.template.as_deref()),
        column_widths: widths(&params.column_widths_mm)?,
        title: params.title.clone(),
        layer: params.layer.clone(),
        anchor: params.anchor.map(Anchor::sw).unwrap_or(0),
        ..Default::default()
    };
    if let Some(at) = params.at_mm {
        request.position = Some(vec3(point(at, "at_mm")?));
    }
    let needs_view = params.kind != TableKindParam::General;
    if needs_view && view.is_empty() {
        return Err(invalid("say which view the table is of (`view`)"));
    }
    if !needs_view && !view.is_empty() {
        return Err(invalid("a general table isn't of a view: give `sheet` (or nothing for the active sheet)"));
    }
    if needs_view && !request.sheet_name.is_empty() {
        return Err(invalid("a BOM / hole table goes on its view's sheet; leave `sheet` out"));
    }
    let only = |ok: bool, what: &str, kind: &str| -> Result<(), McpError> {
        if ok { Ok(()) } else { Err(invalid(format!("{what} is for {kind} tables"))) }
    };
    only(params.bom_type.is_none() && params.configuration.is_none() || params.kind == TableKindParam::Bom, "bom_type / configuration", "bom")?;
    only(
        params.origin.is_none() && params.holes.is_empty() && params.start_tag.is_none() || params.kind == TableKindParam::Hole,
        "origin / holes / start_tag",
        "hole",
    )?;
    only(params.rows.is_empty() && params.columns.is_none() || params.kind == TableKindParam::General, "rows / columns", "general")?;
    match params.kind {
        TableKindParam::Bom => {
            request.bom_type = params.bom_type.map(BomType::sw).unwrap_or(0);
            request.configuration = text(params.configuration.as_deref());
        }
        TableKindParam::Hole => {
            let origin = text(params.origin.as_deref());
            if origin.is_empty() || params.holes.is_empty() {
                return Err(invalid(
                    "a hole table needs `origin` (a vertex id of the view) and `holes` (an edge id per hole) from \
                     inspect_drawing with the view",
                ));
            }
            request.hole_origin = Some(entity(ids, &origin, &view, "origin")?);
            request.hole_edges =
                params.holes.iter().map(|h| entity(ids, h, &view, "hole")).collect::<Result<Vec<_>, _>>()?;
            request.start_tag = text(params.start_tag.as_deref());
        }
        TableKindParam::General => {
            let width = params.rows.iter().map(Vec::len).max().unwrap_or(0).max(params.columns.unwrap_or(0));
            if params.rows.is_empty() || width == 0 {
                return Err(invalid("give the table's `rows` (text, row by row; [[\"\"]] for one empty cell)"));
            }
            if params.rows.len() > MAX_ROWS || width > MAX_COLUMNS {
                return Err(invalid(format!("at most {MAX_ROWS} rows and {MAX_COLUMNS} columns")));
            }
            request.rows = params.rows.len() as i32;
            request.columns = width as i32;
            for (r, row) in params.rows.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    if !cell.is_empty() {
                        request.cells.push(proto::TableCell { row: r as i32, column: c as i32, text: cell.clone() });
                    }
                }
            }
        }
    }
    Ok(request)
}

pub fn edit_request(params: &EditDrawingTableParams) -> Result<proto::SetupDrawingTableRequest, McpError> {
    let name = params.name.trim();
    if name.is_empty() {
        return Err(invalid("say which table (`name`, from inspect_drawing_tables)"));
    }
    if params.at_mm.is_some() && params.move_by_mm.is_some() {
        return Err(invalid("give at_mm or move_by_mm, not both"));
    }
    let request = proto::SetupDrawingTableRequest {
        name: name.to_string(),
        sheet_name: text(params.sheet.as_deref()),
        cells: params
            .cells
            .iter()
            .map(|c| proto::TableCell { row: c.row, column: c.column, text: c.text.clone() })
            .collect(),
        add_rows: params.add_rows.iter().map(|r| proto::TableRowText { cells: r.clone() }).collect(),
        delete_rows: params.delete_rows.clone(),
        column_widths: widths(&params.column_widths_mm)?,
        title: params.title.clone(),
        layer: params.layer.clone(),
        anchor: params.anchor.map(Anchor::sw).unwrap_or(0),
        position: params.at_mm.map(|at| point(at, "at_mm")).transpose()?.map(vec3),
        ..Default::default()
    };
    let changes = !request.cells.is_empty()
        || !request.add_rows.is_empty()
        || !request.delete_rows.is_empty()
        || !request.column_widths.is_empty()
        || request.title.is_some()
        || request.layer.is_some()
        || request.anchor != 0
        || request.position.is_some()
        || params.move_by_mm.is_some();
    if !changes {
        return Err(invalid("nothing to change: give cells, add_rows, delete_rows, column_widths_mm, title, layer, \
                            at_mm / move_by_mm or anchor"));
    }
    if params.add_rows.len() + params.delete_rows.len() > MAX_ROWS {
        return Err(invalid(format!("at most {MAX_ROWS} rows added or deleted per call")));
    }
    Ok(request)
}

/// move_by_mm needs the table's position: the request moving its anchor
/// corner from `table`'s.
pub fn apply_move_by(request: &mut proto::SetupDrawingTableRequest, table: &proto::TableInfo, by_mm: [f64; 2]) -> Result<(), McpError> {
    let by = point(by_mm, "move_by_mm")?;
    let p = table.position.as_ref().ok_or_else(|| invalid("SolidWorks didn't say where the table is; give at_mm"))?;
    request.position = Some(vec3(DVec2::new(p.x, p.y) + by));
    Ok(())
}

pub fn inspect_request() -> proto::InspectDrawingRequest {
    proto::InspectDrawingRequest { include_tables: true, ..Default::default() }
}

pub fn balloons_request(ids: &IdRegistry, params: &AddBalloonsParams) -> Result<proto::AddBalloonsRequest, McpError> {
    let view = params.view.trim().to_string();
    if view.is_empty() {
        return Err(invalid("say which view (`view`)"));
    }
    let count = params.components.len() + params.attach_to.len();
    if count > MAX_BALLOONS {
        return Err(invalid(format!("at most {MAX_BALLOONS} balloons per call")));
    }
    if !params.arrow_at_mm.is_empty() && params.arrow_at_mm.len() != params.attach_to.len() {
        return Err(invalid("give one arrow_at_mm per attach_to entity"));
    }
    if !params.at_mm.is_empty() && params.at_mm.len() != count {
        return Err(invalid("give one at_mm per balloon (components, then attach_to), or none"));
    }
    if count > 0 && (params.layout.is_some() || params.every_instance) {
        return Err(invalid("layout / every_instance are for ballooning the whole view (no components / attach_to)"));
    }
    if count == 0 && !params.at_mm.is_empty() {
        return Err(invalid("at_mm places balloons given by components / attach_to; Auto Balloon places its own \
                            (move them with edit_drawing_annotation)"));
    }
    if params.size_chars.is_some_and(|s| s > 5) {
        return Err(invalid("size_chars is 0 (tight) to 5"));
    }
    let custom = |kind: Option<BalloonText>, given: &Option<String>, what: &str| -> Result<String, McpError> {
        let t = text(given.as_deref());
        match (kind, t.is_empty()) {
            (Some(BalloonText::Custom), true) => Err(invalid(format!("{what} custom needs its text"))),
            (k, false) if k != Some(BalloonText::Custom) => {
                Err(invalid(format!("{what}: a custom text goes with custom")))
            }
            _ => Ok(t),
        }
    };
    let mut targets = Vec::new();
    let mut at = params.at_mm.iter();
    let position = |p: Option<&[f64; 2]>, i: usize| -> Result<Option<proto::DrawingPoint>, McpError> {
        p.map(|p| {
            let q = point(*p, &format!("at_mm {}", i + 1))?;
            Ok(proto::DrawingPoint { x: q.x, y: q.y, view_name: String::new() })
        })
        .transpose()
    };
    for (i, c) in params.components.iter().enumerate() {
        let c = c.trim();
        if c.is_empty() {
            return Err(invalid("an empty component name"));
        }
        targets.push(proto::BalloonTarget { component: c.to_string(), position: position(at.next(), i)?, ..Default::default() });
    }
    for (i, id) in params.attach_to.iter().enumerate() {
        let r = entity(ids, id, &view, "attach_to")?;
        let arrow = params
            .arrow_at_mm
            .get(i)
            .map(|p| point(*p, "arrow_at_mm").map(|q| proto::DrawingPoint { x: q.x, y: q.y, view_name: String::new() }))
            .transpose()?;
        let n = params.components.len() + i;
        targets.push(proto::BalloonTarget {
            entity: Some(r),
            arrow_point: arrow,
            position: position(at.next(), n)?,
            ..Default::default()
        });
    }
    Ok(proto::AddBalloonsRequest {
        view_name: view,
        targets,
        layout: params.layout.map(BalloonLayout::sw).unwrap_or(0),
        every_instance: params.every_instance,
        style: params.style.map(BalloonStyle::sw),
        size: params.size_chars.map(i32::from),
        upper_content: params.text.map(BalloonText::sw),
        upper_text: custom(params.text, &params.custom_text, "text")?,
        lower_content: params.lower_text.map(BalloonText::sw),
        lower_text: custom(params.lower_text, &params.lower_custom_text, "lower_text")?,
        layer: params.layer.clone(),
    })
}

// ---- Replies ----------------------------------------------------------------------

/// One table as the table tools show it.
pub fn table_json(t: &proto::TableInfo) -> Value {
    let mut obj = Map::new();
    obj.insert("name".into(), json!(t.name));
    obj.insert("kind".into(), json!(table_type_name(t.r#type)));
    if let Some(owner) = match t.owner() {
        proto::AnnotationOwner::View => Some(format!("view {}", t.owner_name)),
        proto::AnnotationOwner::Sheet => Some("sheet".to_string()),
        proto::AnnotationOwner::SheetFormat => Some("sheet_format".to_string()),
        proto::AnnotationOwner::Unspecified => None,
    } {
        obj.insert("in".into(), json!(owner));
    }
    let anchor = Anchor::of(t.anchor);
    if let Some(p) = &t.position {
        obj.insert("anchor".into(), json!(anchor.name()));
        obj.insert("at_mm".into(), sheet_mm(DVec2::new(p.x, p.y)));
    }
    if let Some((lo, hi)) = table_box(t) {
        obj.insert("box_mm".into(), json!([sheet_mm(lo), sheet_mm(hi)]));
    }
    if !t.title.is_empty() {
        obj.insert("title".into(), json!(t.title));
    }
    if !t.layer.is_empty() {
        obj.insert("layer".into(), json!(t.layer));
    }
    if t.r#type == 2 {
        let mut bom = Map::new();
        bom.insert("type".into(), json!(bom_type_name(t.bom_type)));
        bom.insert("assembly".into(), json!(t.bom_model_file));
        bom.insert("configuration".into(), json!(t.bom_configuration));
        if !t.bom_feature.is_empty() {
            bom.insert("feature".into(), json!(t.bom_feature));
        }
        obj.insert("bom".into(), Value::Object(bom));
    }
    let columns: Vec<Value> = (0..t.column_widths.len())
        .map(|c| {
            let mut col = Map::new();
            col.insert("width_mm".into(), json!(round(t.column_widths[c] * MM_PER_M, 2)));
            if let Some(kind) = t.column_types.get(c).copied().and_then(column_type_name) {
                col.insert("type".into(), json!(kind));
            }
            if let Some(p) = t.column_properties.get(c).filter(|p| !p.is_empty()) {
                col.insert("property".into(), json!(p));
            }
            if t.column_hidden.get(c).copied().unwrap_or(false) {
                col.insert("hidden".into(), json!(true));
            }
            Value::Object(col)
        })
        .collect();
    obj.insert("columns".into(), Value::Array(columns));
    if t.header_rows > 0 {
        obj.insert("header_rows".into(), json!(t.header_rows));
    }
    let rows: Vec<Value> = t
        .rows
        .iter()
        .map(|r| {
            let cells: Vec<Value> = r.cells.iter().map(|c| json!(c.replace("\r\n", "\n"))).collect();
            if r.components.is_empty() && !r.hidden {
                return Value::Array(cells);
            }
            let mut row = Map::new();
            row.insert("cells".into(), Value::Array(cells));
            if !r.components.is_empty() {
                row.insert("components".into(), json!(r.components));
            }
            if r.hidden {
                row.insert("hidden".into(), json!(true));
            }
            Value::Object(row)
        })
        .collect();
    obj.insert("rows".into(), Value::Array(rows));
    if t.truncated {
        obj.insert("truncated".into(), json!(true));
    }
    Value::Object(obj)
}

pub fn format_lines_json(sheet: &proto::SheetInfo) -> Value {
    let lines: Vec<Value> = sheet
        .format_lines
        .iter()
        .filter_map(|l| {
            let (a, b) = (l.start.as_ref()?, l.end.as_ref()?);
            let mut e = Map::new();
            e.insert("from_mm".into(), sheet_mm(DVec2::new(a.x, a.y)));
            e.insert("to_mm".into(), sheet_mm(DVec2::new(b.x, b.y)));
            if !l.layer.is_empty() {
                e.insert("layer".into(), json!(l.layer));
            }
            Some(Value::Object(e))
        })
        .collect();
    Value::Array(lines)
}

pub fn render_inspect(reply: &proto::InspectDrawingResponse, params: &InspectDrawingTablesParams) -> Result<Value, McpError> {
    let sheet_filter = text(params.sheet.as_deref());
    let name = text(params.name.as_deref());
    let kind = text(params.kind.as_deref()).to_lowercase();
    if !sheet_filter.is_empty() && !reply.sheets.iter().any(|s| s.name.eq_ignore_ascii_case(&sheet_filter)) {
        let names: Vec<&str> = reply.sheets.iter().map(|s| s.name.as_str()).collect();
        return Err(invalid(format!("the drawing has no sheet {sheet_filter:?} (its sheets: {})", names.join(", "))));
    }
    let mut sheets = Vec::new();
    let mut found = false;
    for s in &reply.sheets {
        if !sheet_filter.is_empty() && !s.name.eq_ignore_ascii_case(&sheet_filter) {
            continue;
        }
        let tables: Vec<Value> = s
            .tables
            .iter()
            .filter(|t| name.is_empty() || t.name == name)
            .filter(|t| kind.is_empty() || table_type_name(t.r#type) == kind)
            .map(table_json)
            .collect();
        found |= !tables.is_empty();
        let mut obj = Map::new();
        obj.insert("name".into(), json!(s.name));
        if s.active {
            obj.insert("active".into(), json!(true));
        }
        obj.insert("size_mm".into(), json!([round(s.width * MM_PER_M, 2), round(s.height * MM_PER_M, 2)]));
        obj.insert(
            "views".into(),
            Value::Array(
                s.views
                    .iter()
                    .map(|v| {
                        let mut e = Map::new();
                        e.insert("name".into(), json!(v.name));
                        if let Some((lo, hi)) = note::outline(v) {
                            e.insert("outline".into(), json!([sheet_mm(lo), sheet_mm(hi)]));
                        }
                        e.insert("model".into(), json!(file_name(&v.model_file)));
                        Value::Object(e)
                    })
                    .collect(),
            ),
        );
        obj.insert("tables".into(), Value::Array(tables));
        if params.include_format_lines {
            obj.insert("format_lines".into(), format_lines_json(s));
        }
        sheets.push(Value::Object(obj));
    }
    if !name.is_empty() && !found {
        return Err(invalid(format!("the drawing has no table named {name:?}")));
    }
    Ok(json!({
        "document": reply.document.as_ref().map(|d| d.title.clone()).unwrap_or_default(),
        "coordinates": "sheet mm from the sheet's lower-left corner (render_drawing's rulers). at_mm is the table's \
                        anchor corner (it grows away from it); rows[0] is the top row. Delete a table with \
                        delete_drawing_annotations.",
        "sheets": sheets,
    }))
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// What looks wrong about where a table ended up.
pub fn table_warnings(sheet: &proto::SheetInfo, t: &proto::TableInfo) -> Vec<String> {
    let Some((lo, hi)) = table_box(t) else { return Vec::new() };
    let hits = Obstacles::of(sheet, &t.name).hits(lo, hi, 0.0);
    if hits.is_empty() {
        return Vec::new();
    }
    vec![format!("it overlaps {} -- move it with edit_drawing_table at_mm", hits.join(", "))]
}

/// The reply to add_drawing_table / edit_drawing_table.
pub fn setup_reply(
    ids: &mut IdRegistry,
    request: &proto::SetupDrawingTableRequest,
    reply: &proto::SetupDrawingTableResponse,
    moved_to: Option<DVec2>,
) -> String {
    let Some(t) = &reply.table else { return "SolidWorks returned no table".into() };
    let kind = table_type_name(t.r#type);
    let mut lines = Vec::new();
    let what = match kind {
        "bom" => "bill of materials",
        "hole" => "hole table",
        _ => "table",
    };
    let rows = t.rows.len().saturating_sub(t.header_rows.max(0) as usize);
    let size = table_size(t);
    let place = table_box(t).map(|(lo, hi)| format!(" at {}..{} mm", mm2(lo), mm2(hi))).unwrap_or_default();
    lines.push(if reply.created {
        format!(
            "added {what} {} on {}: {} x {} mm{place}, {} column(s), {rows} row(s) under its header.",
            t.name,
            reply.sheet.as_ref().map(|s| s.name.as_str()).unwrap_or("?"),
            round(size.x * MM_PER_M, 1),
            round(size.y * MM_PER_M, 1),
            t.column_widths.len(),
        )
    } else {
        format!("changed {what} {}{place}.", t.name)
    });
    if let Some(p) = moved_to {
        lines.push(format!("Placed it with its {} corner at {} mm, clear of the views and other tables.", Anchor::of(t.anchor).name(), mm2(p)));
    } else if reply.created && request.position.is_none() {
        lines.push("No free spot was found for it on the sheet: it's at the sheet's upper left; move it with \
                    edit_drawing_table at_mm."
            .into());
    }
    if kind == "bom" && reply.created {
        lines.push(format!(
            "Each row is a component of {} (configuration {}); its item numbers are what add_balloons shows. Item \
             numbers, quantities and part numbers follow the assembly; the DESCRIPTION column shows each part's \
             \"Description\" property (custom_properties sets it).",
            file_name(&t.bom_model_file),
            t.bom_configuration
        ));
    }
    if !reply.hole_tags.is_empty() {
        let tags: Vec<String> = reply.hole_tags.iter().map(|a| a.text.first().cloned().unwrap_or_default()).collect();
        lines.push(format!("SolidWorks tagged the holes in {}: {}.", request.view_name, tags.join(", ")));
    }
    if let Some(sheet) = &reply.sheet {
        for w in table_warnings(sheet, t) {
            lines.push(format!("Warning: {w}"));
        }
    }
    let _ = ids;
    lines.push(table_json(t).to_string());
    lines.join("\n")
}

/// The component a balloon points at, as the assembly names it: the reply
/// names the drawing-side stand-in ("pair-1/bracket-2"); the components
/// listed give the assembly's own names.
fn balloon_component(name: &str, components: &[proto::ComponentInfo]) -> String {
    if components.iter().any(|c| c.name == name) {
        return name.to_string();
    }
    let mut rest = name;
    while let Some((_, tail)) = rest.split_once('/') {
        if components.iter().any(|c| c.name == tail) {
            return tail.to_string();
        }
        rest = tail;
    }
    name.to_string()
}

pub fn balloons_reply(ids: &mut IdRegistry, request: &proto::AddBalloonsRequest, reply: &proto::AddBalloonsResponse) -> String {
    let frames = reply.sheet.as_ref().map(note::frames).unwrap_or_default();
    let mut lines = vec![format!("added {} balloon(s) in {}.", reply.balloons.len(), request.view_name)];
    if reply.boms.is_empty() {
        lines.push(format!(
            "Warning: no bill of materials on the drawing lists {}'s assembly, so the item numbers aren't backed by a \
             BOM table -- add one with add_drawing_table kind=bom view=\"{}\" (the balloons then show its numbers).",
            request.view_name, request.view_name
        ));
    } else {
        lines.push(format!("Item numbers come from {}.", reply.boms.join(", ")));
    }
    let mut list = Vec::new();
    for b in &reply.balloons {
        let mut entry = match note::annotation_entry(ids, &frames, b) {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        if let Some(info) = &b.balloon {
            entry.insert("component".into(), json!(balloon_component(&info.component, &reply.components)));
            if info.upper_text.contains('*') {
                lines.push(format!(
                    "Warning: {} shows \"{}\": SolidWorks couldn't match it to one BOM item (several BOMs of the \
                     assembly, or none?).",
                    b.name, info.upper_text
                ));
            }
        }
        if let Some(sheet) = &reply.sheet
            && let Some(box_) = b.note.as_ref().and_then(|n| n.extent.as_ref()).and_then(|e| {
                let (lo, hi) = (e.min.as_ref()?, e.max.as_ref()?);
                Some((DVec2::new(lo.x, lo.y), DVec2::new(hi.x, hi.y)))
            })
        {
            let mut obstacles = Obstacles::of(sheet, "");
            obstacles.views.retain(|(name, _)| *name != request.view_name);
            obstacles.lines.clear();
            let hits = obstacles.hits(box_.0, box_.1, 0.0);
            if !hits.is_empty() {
                lines.push(format!("Warning: {} overlaps {} -- move it with edit_drawing_annotation.", b.name, hits.join(", ")));
            }
        }
        list.push(Value::Object(entry));
    }
    lines.push(Value::Array(list).to_string());
    lines.join("\n")
}

/// The "balloon" fields annotation_entry adds for a BOM balloon.
/// (INote::GetBomBalloonTextStyle reported 2, quantity, for balloons showing
/// item numbers, and the lower text for unsplit ones: neither is shown
/// unless the balloon is split.)
pub fn balloon_json(info: &proto::BalloonInfo, split: bool) -> Value {
    let mut obj = Map::new();
    obj.insert("shows".into(), json!(info.upper_text));
    if split && !info.lower_text.is_empty() {
        obj.insert("lower".into(), json!(info.lower_text));
    }
    if !info.component.is_empty() {
        obj.insert("component".into(), json!(info.component));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, at: [f64; 2], anchor: i32, widths: &[f64], heights: &[f64]) -> proto::TableInfo {
        proto::TableInfo {
            name: name.into(),
            position: Some(proto::Vec3 { x: at[0], y: at[1], z: 0.0 }),
            anchor,
            column_widths: widths.to_vec(),
            column_hidden: vec![false; widths.len()],
            row_heights: heights.to_vec(),
            rows: heights.iter().map(|_| proto::TableRow::default()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn boxes_follow_the_anchor() {
        let t = table("T", [0.3, 0.1], 1, &[0.02, 0.03], &[0.005, 0.005]);
        let (lo, hi) = table_box(&t).unwrap();
        assert!((lo - DVec2::new(0.3, 0.09)).length() < 1e-12 && (hi - DVec2::new(0.35, 0.1)).length() < 1e-12);
        let t = table("T", [0.3, 0.1], 4, &[0.02, 0.03], &[0.005, 0.005]);
        let (lo, hi) = table_box(&t).unwrap();
        assert!((lo - DVec2::new(0.25, 0.1)).length() < 1e-12 && (hi - DVec2::new(0.3, 0.11)).length() < 1e-12);
        // Hidden columns take no room.
        let mut t = table("T", [0.0, 0.0], 3, &[0.02, 0.03], &[0.01]);
        t.column_hidden[1] = true;
        assert!((table_size(&t) - DVec2::new(0.02, 0.01)).length() < 1e-12);
    }

    #[test]
    fn segments_hit_boxes() {
        let (lo, hi) = (DVec2::new(0.0, 0.0), DVec2::new(1.0, 1.0));
        assert!(segment_hits_box(DVec2::new(-1.0, 0.5), DVec2::new(2.0, 0.5), lo, hi, 0.0));
        assert!(!segment_hits_box(DVec2::new(-1.0, 1.5), DVec2::new(2.0, 1.5), lo, hi, 0.0));
        assert!(segment_hits_box(DVec2::new(-1.0, 1.05), DVec2::new(2.0, 1.05), lo, hi, 0.1));
        assert!(segment_hits_box(DVec2::new(0.2, 0.2), DVec2::new(0.3, 0.3), lo, hi, 0.0));
    }

    #[test]
    fn a_bom_goes_top_right_clear_of_views_and_format_lines() {
        let mut sheet = proto::SheetInfo { name: "Sheet1".into(), width: 0.42, height: 0.297, ..Default::default() };
        sheet.views.push(proto::ViewInfo {
            name: "Drawing View1".into(),
            outline: Some(proto::Box3 {
                min: Some(proto::Vec3 { x: 0.05, y: 0.13, z: 0.0 }),
                max: Some(proto::Vec3 { x: 0.39, y: 0.23, z: 0.0 }),
            }),
            ..Default::default()
        });
        // A border frame 10 mm in.
        for (a, b) in [((0.01, 0.01), (0.41, 0.01)), ((0.41, 0.01), (0.41, 0.287)), ((0.41, 0.287), (0.01, 0.287)), ((0.01, 0.287), (0.01, 0.01))] {
            sheet.format_lines.push(proto::FormatLine {
                start: Some(proto::Vec3 { x: a.0, y: a.1, z: 0.0 }),
                end: Some(proto::Vec3 { x: b.0, y: b.1, z: 0.0 }),
                layer: String::new(),
            });
        }
        let bom = table("DetailItem7", [0.02, 0.277], 1, &[0.02, 0.05, 0.05, 0.02], &[0.006, 0.006, 0.006]);
        sheet.tables.push(bom.clone());
        let to = relocation(&sheet, &bom, TableKindParam::Bom, None).unwrap();
        let mut moved = bom.clone();
        moved.position = Some(proto::Vec3 { x: to.x, y: to.y, z: 0.0 });
        let (lo, hi) = table_box(&moved).unwrap();
        // Inside the frame, right of nothing but above the view.
        assert!(hi.x <= 0.41 - GAP + 1e-9 && hi.y <= 0.287 - GAP + 1e-9, "{lo} {hi}");
        assert!(lo.y >= 0.23 + GAP - 1e-9, "{lo} {hi}");
        assert!(hi.x > 0.39, "toward the right: {lo} {hi}");
    }

    #[test]
    fn requests_check_kinds() {
        let ids = IdRegistry::default();
        let general = AddDrawingTableParams {
            kind: TableKindParam::General,
            view: None,
            sheet: None,
            at_mm: Some([20.0, 100.0]),
            anchor: None,
            template: None,
            bom_type: None,
            configuration: None,
            origin: None,
            holes: vec![],
            start_tag: None,
            rows: vec![vec!["TAG".into(), "X".into(), "Y".into()], vec!["P1".into(), "10".into()]],
            columns: None,
            column_widths_mm: vec![],
            title: None,
            layer: None,
        };
        let r = add_request(&ids, &general).unwrap();
        assert_eq!((r.rows, r.columns, r.cells.len()), (2, 3, 5));
        assert!((r.position.unwrap().x - 0.02).abs() < 1e-12);
        let bad = AddDrawingTableParams { bom_type: Some(BomType::Indented), ..general };
        assert!(add_request(&ids, &bad).is_err());
    }

    #[test]
    fn balloon_components_lose_the_view_root() {
        let comps = vec![proto::ComponentInfo { name: "block-2".into(), ..Default::default() }];
        assert_eq!(balloon_component("pair-1/block-2", &comps), "block-2");
        assert_eq!(balloon_component("block-2", &comps), "block-2");
        assert_eq!(balloon_component("other-1", &comps), "other-1");
    }
}
