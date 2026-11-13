//! The DimXpert ("MBD Dimension") tools on parts: add_pmi_dimension (a size,
//! location, angle or basic dimension on faces, with a tolerance if asked),
//! auto_dimension_scheme (DimXpert's Auto Dimension Scheme) and
//! pmi_general_tolerance (the general tolerance dimensions take when they
//! have no tolerance of their own: ISO 2768-1, or ASME's block tolerance
//! by decimal places). See docs/notes/dimxpert.md.
//!
//! DimXpert's document options are plain document properties
//! (swUserPreference*_e ids); the table of which id is what lives here, the
//! add-in only reads and writes ids (DocumentPreferences). ISO or ASME
//! (`standard`, by default the document's, as for frames: gdt.rs) decides
//! which general tolerance applies: DimXpert's own (ISO 2768) or the part's
//! block table, which DimXpert can't be switched to through the API, given
//! to each dimension as its own +/-.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::gdt::GdtStandard;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, mm, round};
use crate::macro_run::Closed;
use crate::pmi;
use crate::tolerance::{self, ToleranceKind, ToleranceValues};
use crate::units;

/// The standard the tools follow: the one asked, else the document's.
pub fn standard(asked: Option<GdtStandard>, units: Option<&proto::DocumentUnits>) -> GdtStandard {
    asked.unwrap_or_else(|| GdtStandard::of_document(units))
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// DimXpert's warning that a new dimension over-constrains the part ("Added
/// tolerance has overconstrained the part.", a modal dialog; seen
/// 2026-11-12): the refusal's text, since double dimensioning is what
/// ISO 129 / ASME Y14.5 rule out.
pub fn overconstrained(dialogs: &[Closed]) -> Option<String> {
    let warning = dialogs.iter().find(|d| d.text.to_lowercase().contains("overconstrain"))?;
    Some(format!(
        "DimXpert: \"{}\" -- the part already has a dimension that controls this (e.g. the hole located from \
         another face in the same direction), and a feature is dimensioned once; delete the other one first \
         (delete_drawing_annotations) if this is the one you want",
        warning.text.trim()
    ))
}

/// Other dialogs SolidWorks showed (and that were closed), for a note.
fn dialog_note(dialogs: &[Closed]) -> Option<String> {
    if dialogs.is_empty() {
        return None;
    }
    let shown: Vec<String> = dialogs.iter().map(|d| format!("\"{}\" ({})", d.text.trim(), d.pressed)).collect();
    Some(format!("SolidWorks showed dialogs, closed: {}", shown.join(", ")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PmiDimensionKind {
    /// The size of the feature DimXpert recognizes from one face: a hole's or shaft's
    /// diameter, a fillet's radius, a chamfer, a cone's angle, a counterbore, ...
    Size,
    /// The distance between two features, from the first: planes, axes of holes and
    /// shafts, ... (DimXpert makes it an angle when they aren't parallel).
    Location,
    /// The angle between two features.
    Angle,
    /// A basic (theoretically exact, boxed) location between two features, for feature
    /// control frames. (A basic size: a size, then set_dimension_tolerance "basic".)
    Basic,
}

impl PmiDimensionKind {
    fn code(self) -> i32 {
        use proto::PmiDimensionKind as K;
        match self {
            PmiDimensionKind::Size => K::Size as i32,
            PmiDimensionKind::Location => K::Location as i32,
            PmiDimensionKind::Angle => K::Angle as i32,
            PmiDimensionKind::Basic => K::Basic as i32,
        }
    }

    fn name(self) -> &'static str {
        match self {
            PmiDimensionKind::Size => "size",
            PmiDimensionKind::Location => "location",
            PmiDimensionKind::Angle => "angle",
            PmiDimensionKind::Basic => "basic",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddPmiDimensionParams {
    /// "size", "location", "angle" or "basic".
    pub kind: PmiDimensionKind,
    /// Face ids from inspect_model. size: one face of the feature (a hole's cylinder, a
    /// fillet, ...). location / angle / basic: two, from (the origin) then to. Two
    /// parallel planes (a slot, a block's width): location.
    pub on: Vec<String>,
    /// Where the text goes, [x, y, z] model mm; DimXpert keeps it in its annotation
    /// view's plane. Default: outside the part, past the faces.
    #[serde(default)]
    pub at_mm: Option<Vec<f64>>,
    /// The tolerance to give it (as set_dimension_tolerance: "bilateral", "limit",
    /// "symmetric", "fit", "fit_with_tolerance", "fit_tolerance_only", "min", "max",
    /// "none"), with its values below. Unset: the general tolerance (pmi_general_tolerance)
    /// -- ISO: DimXpert's default for its kind (ISO 2768 or +/-); ASME: the part's block
    /// tolerance for the decimals it shows (`decimals` picks them).
    #[serde(default)]
    pub tolerance: Option<ToleranceKind>,
    /// "iso" or "asme": which general tolerance an untoleranced dimension gets. Default:
    /// the document's drafting standard (ANSI: asme).
    #[serde(default)]
    pub standard: Option<GdtStandard>,
    #[serde(flatten)]
    pub values: ToleranceValues,
}

fn values_given(v: &ToleranceValues) -> bool {
    [v.upper_mm, v.lower_mm, v.plus_minus_mm, v.upper_deg, v.lower_deg, v.plus_minus_deg].iter().any(Option::is_some)
        || v.hole_fit.is_some()
        || v.shaft_fit.is_some()
        || v.tolerance_decimals.is_some()
}

fn face_ids(ids: &IdRegistry, on: &[String]) -> Result<Vec<Vec<u8>>, McpError> {
    on.iter()
        .map(|id| {
            let id = id.trim();
            match Kind::of_id(id) {
                Some(Kind::Face) => ids
                    .persist_id(id)
                    .ok_or_else(|| invalid(ids.unknown_id(id, "inspect_model", "call inspect_model for current ones"))),
                _ => Err(invalid(format!(
                    "{id:?}: DimXpert dimensions go on faces -- give face ids from inspect_model (\"face:7\")"
                ))),
            }
        })
        .collect()
}

/// The AddPmiDimension request, and the tolerance to set on what it adds
/// (its name filled in then).
pub fn request(
    ids: &IdRegistry,
    p: &AddPmiDimensionParams,
    standard: GdtStandard,
) -> Result<(proto::AddPmiDimensionRequest, Option<proto::SetDimensionToleranceRequest>), McpError> {
    let count = p.on.len();
    // DimXpert makes no basic dimension of one face (seen 2026-11-12).
    let ok = match p.kind {
        PmiDimensionKind::Size => count == 1,
        _ => count == 2,
    };
    if !ok {
        return Err(invalid(match p.kind {
            PmiDimensionKind::Size => "a size dimension takes one face of the feature in `on`".to_string(),
            PmiDimensionKind::Basic => "a basic dimension takes two faces in `on`: from, to (for a basic size, add a \
                                        size and set_dimension_tolerance it \"basic\")"
                .to_string(),
            kind => format!("a {} dimension takes two faces in `on`: from, to", kind.name()),
        }));
    }
    let faces = face_ids(ids, &p.on)?;
    if count == 2 && faces[0] == faces[1] {
        return Err(invalid("give two different faces"));
    }
    let position = match p.at_mm.as_deref() {
        None => None,
        Some([x, y, z]) if x.is_finite() && y.is_finite() && z.is_finite() => {
            Some(proto::Vec3 { x: x / MM_PER_M, y: y / MM_PER_M, z: z / MM_PER_M })
        }
        Some(_) => return Err(invalid("at_mm: give [x, y, z] in model mm")),
    };
    if let Some(d) = p.values.decimals.filter(|d| *d > 8) {
        return Err(invalid(format!("decimals must be 0..8, got {d}")));
    }
    let tolerance = match p.tolerance {
        None if values_given(&p.values) => {
            return Err(invalid("tolerance values go with `tolerance` (e.g. \"bilateral\", \"fit\")"));
        }
        // ASME: the decimals pick the block tolerance (block_tolerance).
        None if p.values.decimals.is_some() && (standard != GdtStandard::Asme || p.kind == PmiDimensionKind::Basic) => {
            return Err(invalid(
                "decimals without a tolerance only pick an ASME block tolerance row; give `tolerance` too",
            ));
        }
        None => None,
        Some(ToleranceKind::Basic) if p.kind != PmiDimensionKind::Basic => {
            return Err(invalid("for a basic dimension use kind \"basic\""));
        }
        Some(kind) if p.kind == PmiDimensionKind::Basic && kind != ToleranceKind::Basic => {
            return Err(invalid(
                "a basic dimension takes no tolerance of its own: a feature control frame tolerances the feature",
            ));
        }
        // The name is the new dimension's, known once it's there.
        Some(kind) => Some(tolerance::request_for("(new)", kind, &p.values)?),
    };
    let request = proto::AddPmiDimensionRequest { kind: p.kind.code(), faces, position };
    Ok((request, tolerance))
}

fn dimension_name(a: &proto::AnnotationInfo) -> Option<&str> {
    a.dimension.as_ref()?.dimension.as_ref().map(|i| i.name.as_str()).filter(|n| !n.is_empty())
}

/// Plan tolerances for every dimension, retaining annotation indices so the
/// read-back replaces the right entry even when other annotations precede it.
pub fn dimension_tolerances(
    added: &[proto::AnnotationInfo],
    explicit: Option<&proto::SetDimensionToleranceRequest>,
    block: Option<&BlockTable>,
    use_block: bool,
    decimals: Option<u8>,
    units: Option<&proto::DocumentUnits>,
) -> Result<(Vec<(usize, proto::SetDimensionToleranceRequest)>, Vec<String>), McpError> {
    let mut requests = Vec::new();
    let mut notes = Vec::new();
    for (index, annotation) in added.iter().enumerate() {
        let Some(name) = dimension_name(annotation) else { continue };
        if let Some(explicit) = explicit {
            let mut request = explicit.clone();
            request.name = name.to_string();
            requests.push((index, request));
        } else if use_block {
            match block_tolerance(block, annotation, decimals, units) {
                Ok((request, what)) => {
                    requests.push((index, request));
                    notes.push(format!("ASME: {name}: {what}"));
                }
                Err(why) => notes.push(format!("{name}: {why}")),
            }
        }
    }
    if explicit.is_some() && requests.is_empty() {
        return Err(McpError::internal_error("DimXpert's new annotation is no dimension to tolerance", None));
    }
    Ok((requests, notes))
}

/// A failed edit must distinguish confirmed deletion from failed or partial
/// cleanup. A transport error cannot establish which annotations remain.
pub fn cleanup_error(
    mut error: McpError,
    names: &[String],
    cleanup: Result<proto::DeleteDrawingAnnotationsResponse, tonic::Status>,
) -> McpError {
    let note = match cleanup {
        Ok(reply) => {
            let remaining: Vec<&str> = names.iter()
                .filter(|name| !reply.deleted.iter().any(|a| a.name == **name))
                .map(String::as_str).collect();
            if remaining.is_empty() {
                "the new dimension(s) were deleted again".to_string()
            } else {
                format!("cleanup did not confirm deletion of {}; inspect_annotations before retrying", remaining.join(", "))
            }
        }
        Err(status) => format!(
            "cleanup failed: {}; new annotations may remain: {}; inspect_annotations before retrying",
            status.message(), names.join(", ")
        ),
    };
    error.message = format!("{} -- {note}", error.message).into();
    error
}

// ---- Block tolerance (ASME) --------------------------------------------------------

/// DimXpert's block tolerance table, a document option: three rows of
/// decimal places (swPartDimXpertLengthUnitTol{1,2,3}Decimals) and +/-
/// (…Tol{1,2,3}Value, meters), and one for angles
/// (swPartDimXpertAngularUnitTolValue, radians).
const BLOCK_DECIMALS: [i32; 3] = [405, 406, 407];
const BLOCK_VALUES: [i32; 3] = [123, 124, 125];
const BLOCK_ANGLE: i32 = 126;
/// swPartDimXpertBlockTolerance: which half of DimXpert's tolerance options
/// SolidWorks lets the API change -- on, the block rows' decimals; off, the
/// ISO 2768 class (each refused otherwise). It changes nothing else: DimXpert
/// keeps its ISO 2768 method either way, and it reads off again after the
/// part is reopened (seen 2026-11-12).
const BLOCK_TOLERANCE_TOGGLE: i32 = 424;
/// What a part made from this install's template has there (2 / 3 / 4
/// decimals at 0.01 / 0.014 / 0.0025 mm, seen 2026-11-12): not a table
/// anyone set, so it isn't applied.
const TEMPLATE_BLOCK_ROWS: [(i32, f64); 3] = [(2, 1e-5), (3, 1.4e-5), (4, 2.5e-6)];
// The template's angular placeholder, radians (approximately 0.01 degrees).
const TEMPLATE_BLOCK_ANGLE: f64 = 1.745e-4;

#[derive(Debug, Clone, PartialEq)]
pub struct BlockTable {
    /// (decimal places, +/- in meters).
    pub rows: Vec<(i32, f64)>,
    /// +/- in radians.
    pub angle: Option<f64>,
}

/// The ids block_table reads.
pub fn block_read_request() -> proto::DocumentPreferencesRequest {
    proto::DocumentPreferencesRequest {
        integers: BLOCK_DECIMALS.to_vec(),
        doubles: BLOCK_VALUES.iter().copied().chain([BLOCK_ANGLE]).collect(),
        ..Default::default()
    }
}

/// The part's block tolerance table; None while it's the template's.
pub fn block_table(reply: &proto::DocumentPreferencesResponse) -> Option<BlockTable> {
    let mut rows: Vec<(i32, f64)> = BLOCK_DECIMALS
        .iter()
        .zip(BLOCK_VALUES)
        .filter_map(|(d, v)| Some((*reply.integers.get(d)?, *reply.doubles.get(&v)?)))
        .collect();
    let placeholder = rows.len() == 3
        && rows.iter().zip(TEMPLATE_BLOCK_ROWS).all(|((d, v), (td, tv))| *d == td && (v - tv).abs() < 1e-12);
    let mut angle = reply.doubles.get(&BLOCK_ANGLE).copied().filter(|a| a.is_finite() && *a > 0.0);
    if placeholder {
        rows.clear();
        // A changed angular row is useful even with untouched length rows.
        // Account for the template's rounded radian value as well as 0.01 deg.
        angle = angle.filter(|a| (a - TEMPLATE_BLOCK_ANGLE).abs() > 1e-7);
    }
    (!rows.is_empty() || angle.is_some()).then_some(BlockTable { rows, angle })
}

/// A length as the document shows it: "0.01 in" in an inch part, "0.254 mm" otherwise.
fn shown_length(m: f64, units: Option<&proto::DocumentUnits>) -> String {
    let in_document = units::length_text(mm(m), units);
    match units.filter(|_| !units::is_mm(units)) {
        Some(u) => format!("{in_document} {} ({} mm)", units::length_name(u), mm(m)),
        None => format!("{} mm", mm(m)),
    }
}

/// ASME: the block tolerance of the new dimension -- the row for the
/// decimal places it shows (or `decimals`, which it's then given), angles
/// the angular one -- as its own symmetric tolerance, with what it is for
/// the reply. Err: why none applies (a note).
pub fn block_tolerance(
    table: Option<&BlockTable>,
    added: &proto::AnnotationInfo,
    decimals: Option<u8>,
    units: Option<&proto::DocumentUnits>,
) -> Result<(proto::SetDimensionToleranceRequest, String), String> {
    let Some(table) = table else {
        return Err("ASME: the part has no block tolerance table yet (pmi_general_tolerance block=[...]), so it \
                    keeps DimXpert's default; give `tolerance`, or set the table"
            .to_string());
    };
    let Some(d) = added.dimension.as_ref() else { return Err("DimXpert's new annotation is no dimension".to_string()) };
    let name = d.dimension.as_ref().map(|i| i.name.clone()).unwrap_or_default();
    let angular = d.dimension.as_ref().is_some_and(|i| i.r#type() == proto::DimensionType::Angular);
    let shown = decimals.map(i32::from).or(d.decimals);
    let (value, what) = if angular {
        let Some(a) = table.angle else {
            return Err("ASME: the part's block tolerance has no angle (block_angle_deg)".to_string());
        };
        (a, format!("+/-{} deg, the part's block tolerance for angles", round(a.to_degrees(), 4)))
    } else {
        let Some(places) = shown else {
            return Err("ASME: SolidWorks didn't say how many decimals the dimension shows; give `decimals`".to_string());
        };
        let Some((_, v)) = table.rows.iter().find(|(rd, _)| *rd == places) else {
            let have: Vec<String> = table.rows.iter().map(|(rd, _)| rd.to_string()).collect();
            return Err(format!(
                "ASME: the part's block tolerance has no row for {places} decimals (it has {}): give `decimals` or \
                 `tolerance`",
                have.join(", ")
            ));
        };
        (*v, format!("+/-{}, the part's block tolerance for {places} decimals", shown_length(*v, units)))
    };
    let request = proto::SetDimensionToleranceRequest {
        name,
        tolerance_type: 4,
        upper: value,
        lower: -value,
        angle: angular,
        value_precision: decimals.map(i32::from),
        // ASME: an inch tolerance has as many decimals as its dimension.
        tolerance_precision: shown.filter(|_| !angular),
        ..Default::default()
    };
    Ok((request, what))
}

fn mm3(p: &proto::Vec3) -> String {
    format!("[{}, {}, {}]", mm(p.x), mm(p.y), mm(p.z))
}

/// The reply: what DimXpert made of the faces, then its JSON. With a
/// The annotations already contain tolerance read-back. `general` says what
/// ASME's block tolerance did for each dimension (applied, or why not).
pub fn reply(
    ids: &mut IdRegistry,
    p: &AddPmiDimensionParams,
    added: &[proto::AnnotationInfo],
    general: &[String],
    dialogs: &[Closed],
) -> String {
    let json: Vec<Value> = added.iter().map(|a| pmi::annotation_json(ids, a)).collect();
    let Some(first) = json.first() else { return "done".to_string() };
    let field = |key: &str| first.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let measures = field("measures");
    let mut text = format!("added {} dimension {}", p.kind.name(), field("name"));
    let value = first.get("value_mm").map(|v| format!("{v} mm")).or_else(|| first.get("value_deg").map(|v| format!("{v} deg")));
    if let Some(v) = value {
        text += &format!(" ({measures} {v})");
    }
    if let Some(a) = added.first() {
        if !a.dimxpert_name.is_empty() {
            text += &format!(", DimXpert's {}", a.dimxpert_name);
        }
    }
    if let Some(Value::Array(on)) = first.get("attached") {
        let on: Vec<&str> = on.iter().filter_map(Value::as_str).collect();
        if !on.is_empty() {
            text += &format!(", on {}", on.join(", "));
        }
    }
    if let Some(pos) = added.first().and_then(|a| a.position.as_ref()) {
        text += &format!(", text at {} mm", mm3(pos));
    }
    text += ".";

    let mut notes = Vec::new();
    if p.kind == PmiDimensionKind::Location && measures == "angle" {
        notes.push(
            "DimXpert made an angle: the two features aren't parallel (a plane across a hole's axis has no distance \
             to it -- locate the hole from planes parallel to its axis)"
                .to_string(),
        );
    }
    let tol_type = first.get("tolerance").and_then(|t| t.get("type")).and_then(Value::as_str).unwrap_or_default();
    if !general.is_empty() {
        notes.extend_from_slice(general);
    } else if p.tolerance.is_none() {
        match tol_type {
            "general" => notes.push(
                "toleranced by the part's general tolerance (pmi_general_tolerance shows its ISO 2768 class)".to_string(),
            ),
            "" | "basic" => {}
            _ => notes.push(
                "this is DimXpert's default tolerance for its kind (pmi_general_tolerance; Document Properties > \
                 DimXpert); give `tolerance` for another"
                    .to_string(),
            ),
        }
    }
    if added.len() > 1 {
        notes.push(format!("DimXpert added {} annotations for it (all in the JSON)", added.len()));
    }
    notes.extend(dialog_note(dialogs));
    if !notes.is_empty() {
        text += &format!("\nNote: {}.", notes.join("; "));
    }
    let json = if json.len() == 1 { json.into_iter().next().unwrap_or_default() } else { json!({ "annotations": json }) };
    text += &format!("\n{json}");
    text
}

// ---- Auto dimension scheme --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchemePartType {
    /// Milled, block-like parts.
    Prismatic,
    /// Lathe parts: the primary datum is the axis's face.
    Turned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchemeTolerancing {
    /// Size and location dimensions with +/- (or general) tolerances.
    PlusMinus,
    /// Datum symbols, form / orientation frames on the datums, position and profile
    /// frames on the features, size dimensions.
    Geometric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchemePattern {
    /// Hole patterns located by x / y distances.
    Linear,
    /// Hole patterns located on a circle (diameter and angle).
    Polar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeatureFilter {
    Plane,
    Surface,
    Cone,
    Cylinder,
    Boss,
    Fillet,
    Chamfer,
    SimpleHole,
    Counterbore,
    Countersink,
    Slot,
    Notch,
    Pocket,
    SurfaceOfRevolution,
    Torus,
    CompoundHole,
}

impl FeatureFilter {
    /// swDimXpertFeatureFilters_e.
    fn bit(self) -> i32 {
        1 << (self as i32)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AutoDimensionSchemeParams {
    /// Datum faces A, B, C (face ids from inspect_model), primary first: the face the
    /// part sits on, then the ones it's located against. One to three.
    pub datums: Vec<String>,
    /// "prismatic" (default) or "turned".
    #[serde(default)]
    pub part_type: Option<SchemePartType>,
    /// "plus_minus" (default): size and location dimensions; "geometric": datums with
    /// position / profile frames.
    #[serde(default)]
    pub tolerancing: Option<SchemeTolerancing>,
    /// How hole patterns are located: "linear" (default) or "polar".
    #[serde(default)]
    pub pattern: Option<SchemePattern>,
    /// Only these kinds of feature (default all): plane, surface, cone, cylinder, boss,
    /// fillet, chamfer, simple_hole, counterbore, countersink, slot, notch, pocket,
    /// surface_of_revolution, torus, compound_hole.
    #[serde(default)]
    pub features: Option<Vec<FeatureFilter>>,
    /// "iso" or "asme": composite frames are ASME's. Default: the document's drafting
    /// standard (ANSI: asme).
    #[serde(default)]
    pub standard: Option<GdtStandard>,
    /// Geometric: the position tolerance of holes and patterns, mm (diameter zone, at
    /// MMC). Default: the part's DimXpert option.
    #[serde(default)]
    pub position_mm: Option<f64>,
    /// Geometric: the surface profile tolerance of the other faces, mm.
    #[serde(default)]
    pub profile_mm: Option<f64>,
    /// Geometric, ASME: patterns get a composite position frame, this the lower
    /// (pattern-locating) segment's tolerance in mm, under position_mm.
    #[serde(default)]
    pub composite_position_mm: Option<f64>,
    /// Geometric, ASME: a composite profile frame, its lower segment's tolerance in mm.
    #[serde(default)]
    pub composite_profile_mm: Option<f64>,
}

/// swPartDimXpertGeometricPositionComposite / ...SurfaceProfileComposite
/// (toggles) and swPartDimXpertGeometricPositionTolValue / ...Composite... /
/// SurfaceProfile... / ...Composite... (doubles, meters). The scheme takes
/// them when it runs (seen 2026-11-12).
const COMPOSITE_POSITION: i32 = 432;
const COMPOSITE_PROFILE: i32 = 433;
const POSITION_VALUE: i32 = 140;
const COMPOSITE_POSITION_VALUE: i32 = 141;
const PROFILE_VALUE: i32 = 142;
const COMPOSITE_PROFILE_VALUE: i32 = 143;

/// The DimXpert options a geometric scheme runs with, set first (they stay
/// in the part, as when set in the UI): composite frames on exactly when a
/// composite value is given (never in ISO, which has none), the values
/// given. None for a plus / minus scheme.
pub fn scheme_preferences(
    p: &AutoDimensionSchemeParams,
    standard: GdtStandard,
) -> Result<Option<proto::DocumentPreferencesRequest>, McpError> {
    let values = [
        ("position_mm", p.position_mm, POSITION_VALUE),
        ("profile_mm", p.profile_mm, PROFILE_VALUE),
        ("composite_position_mm", p.composite_position_mm, COMPOSITE_POSITION_VALUE),
        ("composite_profile_mm", p.composite_profile_mm, COMPOSITE_PROFILE_VALUE),
    ];
    if !matches!(p.tolerancing, Some(SchemeTolerancing::Geometric)) {
        if let Some((name, ..)) = values.iter().find(|v| v.1.is_some()) {
            return Err(invalid(format!("{name} is for tolerancing \"geometric\"")));
        }
        return Ok(None);
    }
    let composite = p.composite_position_mm.is_some() || p.composite_profile_mm.is_some();
    if composite && standard == GdtStandard::Iso {
        return Err(invalid(
            "composite frames are ASME Y14.5's; ISO has none (its schemes locate patterns with separate frames) -- \
             standard=\"asme\" if the part follows ASME",
        ));
    }
    let mut request = proto::DocumentPreferencesRequest::default();
    request.set_toggles.insert(COMPOSITE_POSITION, p.composite_position_mm.is_some());
    request.set_toggles.insert(COMPOSITE_PROFILE, p.composite_profile_mm.is_some());
    for (name, value, id) in values {
        if let Some(v) = value {
            if !(v.is_finite() && v > 0.0) {
                return Err(invalid(format!("{name} must be a positive number of mm, got {v}")));
            }
            request.set_doubles.insert(id, v / MM_PER_M);
        }
    }
    for (upper, lower, what) in [
        (p.position_mm, p.composite_position_mm, "composite_position_mm"),
        (p.profile_mm, p.composite_profile_mm, "composite_profile_mm"),
    ] {
        if let (Some(u), Some(l)) = (upper, lower) {
            if l >= u {
                return Err(invalid(format!("{what} ({l}) refines the upper segment: it must be smaller ({u})")));
            }
        }
    }
    Ok(Some(request))
}

/// DimXpert prints a frame's tolerance with 2 decimals in the document's
/// unit (".005 in" as ".01", seen 2026-11-12, whatever the dimension
/// decimals): a note for the values that don't survive that.
fn frame_rounding_note(p: &AutoDimensionSchemeParams, units: Option<&proto::DocumentUnits>) -> Option<String> {
    let lost: Vec<String> = [p.position_mm, p.profile_mm, p.composite_position_mm, p.composite_profile_mm]
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let shown: f64 = units::length_text(v, units).parse().ok()?;
            let rounded = (shown * 100.0).round() / 100.0;
            ((shown - rounded).abs() > 1e-9).then(|| format!("{shown} as {rounded}"))
        })
        .collect();
    (!lost.is_empty()).then(|| {
        format!(
            "DimXpert prints frame tolerances with 2 decimals in the document's unit, so its frames show {} \
             (inspect_annotations reads what they show); for such values use add_geometric_tolerance",
            lost.join(", ")
        )
    })
}

pub fn scheme_request(ids: &IdRegistry, p: &AutoDimensionSchemeParams) -> Result<proto::AutoDimensionSchemeRequest, McpError> {
    if p.datums.is_empty() || p.datums.len() > 3 {
        return Err(invalid("datums: one to three faces, primary first"));
    }
    let faces = face_ids(ids, &p.datums)?;
    for (i, f) in faces.iter().enumerate() {
        if faces[..i].contains(f) {
            return Err(invalid("each datum needs a face of its own"));
        }
    }
    let filters = match &p.features {
        Some(list) if list.is_empty() => return Err(invalid("features: name at least one kind, or leave it out for all")),
        Some(list) => list.iter().fold(0, |bits, f| bits | f.bit()),
        None => 0,
    };
    let mut faces = faces.into_iter();
    Ok(proto::AutoDimensionSchemeRequest {
        primary_datum: faces.next().unwrap_or_default(),
        secondary_datum: faces.next().unwrap_or_default(),
        tertiary_datum: faces.next().unwrap_or_default(),
        part_type: matches!(p.part_type, Some(SchemePartType::Turned)) as i32,
        tolerance_type: matches!(p.tolerancing, Some(SchemeTolerancing::Geometric)) as i32,
        pattern_type: matches!(p.pattern, Some(SchemePattern::Polar)) as i32,
        feature_filters: filters,
    })
}

pub fn scheme_reply(
    ids: &mut IdRegistry,
    p: &AutoDimensionSchemeParams,
    reply: &proto::AutoDimensionSchemeResponse,
    units: Option<&proto::DocumentUnits>,
    dialogs: &[Closed],
) -> String {
    let json: Vec<Value> = reply.annotations.iter().map(|a| pmi::annotation_json(ids, a)).collect();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for a in &reply.annotations {
        let t = pmi::type_name(a.r#type).to_string();
        match counts.iter_mut().find(|(name, _)| *name == t) {
            Some((_, n)) => *n += 1,
            None => counts.push((t, 1)),
        }
    }
    let summary: Vec<String> = counts.iter().map(|(t, n)| format!("{n} {t}")).collect();
    let tolerancing = match p.tolerancing {
        Some(SchemeTolerancing::Geometric) => "geometric",
        _ => "plus/minus",
    };
    let datums: Vec<String> = reply
        .annotations
        .iter()
        .zip(&json)
        .filter(|(a, _)| !a.datum_label.trim().is_empty())
        .map(|(a, j)| {
            let on = j.get("attached").and_then(Value::as_array).and_then(|v| v.first()).and_then(Value::as_str);
            format!("{} on {}", a.datum_label.trim(), on.unwrap_or("?"))
        })
        .collect();
    let mut text = format!(
        "DimXpert's {tolerancing} scheme added {} annotation(s): {}",
        reply.annotations.len(),
        summary.join(", ")
    );
    if !datums.is_empty() {
        text += &format!("; datums {}", datums.join(", "));
    }
    text += ". Delete any with delete_drawing_annotations (names below); set_dimension_tolerance changes a dimension's \
             tolerance.";
    let notes: Vec<String> = frame_rounding_note(p, units).into_iter().chain(dialog_note(dialogs)).collect();
    if !notes.is_empty() {
        text += &format!("\nNote: {}.", notes.join("; "));
    }
    text += &format!("\n{}", json!({ "annotations": json }));
    text
}

// ---- General tolerance ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub enum Iso2768Class {
    /// Fine.
    #[serde(rename = "f", alias = "fine")]
    Fine,
    /// Medium.
    #[serde(rename = "m", alias = "medium")]
    Medium,
    /// Coarse.
    #[serde(rename = "c", alias = "coarse")]
    Coarse,
    /// Very coarse.
    #[serde(rename = "v", alias = "very_coarse")]
    VeryCoarse,
}

/// One row of an ASME block tolerance ("UNLESS OTHERWISE SPECIFIED .XX ±.01").
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct BlockRow {
    /// The decimal places a dimension shows: 1 for .X, 2 for .XX, 3 for .XXX (0..4).
    pub decimals: u8,
    /// Its +/- in mm (0.254 for .01 in).
    pub plus_minus_mm: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GeneralToleranceParams {
    /// ISO 2768-1 class: "f" (fine), "m" (medium), "c" (coarse) or "v" (very coarse). Puts
    /// every kind of DimXpert dimension under it (also those already there with a general
    /// tolerance).
    #[serde(default)]
    pub iso2768: Option<Iso2768Class>,
    /// ASME block tolerance by decimal places, one to three rows, e.g. [{"decimals": 2,
    /// "plus_minus_mm": 0.254}, {"decimals": 3, "plus_minus_mm": 0.127}]: add_pmi_dimension
    /// gives an untoleranced ASME dimension the row for the decimals it shows.
    #[serde(default)]
    pub block: Option<Vec<BlockRow>>,
    /// ASME block tolerance for angles, +/- degrees, e.g. 0.5.
    #[serde(default)]
    pub block_angle_deg: Option<f64>,
    /// "iso" or "asme": which of the two applies to the part's dimensions. Default: the
    /// document's drafting standard (ANSI: asme).
    #[serde(default)]
    pub standard: Option<GdtStandard>,
}

/// swPartDimXpertGeneralToleranceClass: swDimXpertGeneralTolClass_e.
const GENERAL_CLASS: i32 = 408;
/// swDimXpertTolType_e.
const TOL_BILATERAL: i32 = 0;
const TOL_SYMMETRIC: i32 = 1;
const TOL_GENERAL: i32 = 2;

/// Each kind of DimXpert dimension: its name, the integer id of its default
/// tolerance type and the double ids of its upper / lower deviation
/// (swUserPreferenceIntegerValue_e / swUserPreferenceDoubleValue_e
/// swPartDimXpert*), and whether those are angles.
const DIMENSION_KINDS: &[(&str, i32, i32, i32, bool)] = &[
    ("diameter", 421, 151, 152, false),
    ("counterbore_diameter", 422, 153, 154, false),
    ("countersink_diameter", 423, 155, 156, false),
    ("countersink_angle", 424, 157, 158, true),
    ("slot_length", 425, 159, 160, false),
    ("slot_width", 426, 161, 162, false),
    ("depth", 427, 163, 164, false),
    ("fillet_radius", 428, 165, 166, false),
    ("chamfer_distance", 417, 147, 148, false),
    ("chamfer_angle", 418, 149, 150, true),
    ("location_distance", 409, 127, 128, false),
    ("location_angle", 410, 129, 130, true),
    ("pattern_location", 413, 131, 132, false),
    ("chain_distance", 414, 133, 134, false),
];

fn class_code(c: Iso2768Class) -> i32 {
    match c {
        Iso2768Class::Fine => 0,
        Iso2768Class::Medium => 1,
        Iso2768Class::Coarse => 2,
        Iso2768Class::VeryCoarse => 3,
    }
}

fn class_name(code: i32) -> String {
    match code {
        0 => "f (fine)".to_string(),
        1 => "m (medium)".to_string(),
        2 => "c (coarse)".to_string(),
        3 => "v (very coarse)".to_string(),
        other => format!("custom ({other})"),
    }
}

/// The DocumentPreferences request: everything read, what's asked set. With
/// the remarks for a general tolerance of the other standard.
pub fn general_tolerance_request(
    p: &GeneralToleranceParams,
    standard: GdtStandard,
) -> Result<(proto::DocumentPreferencesRequest, Vec<String>), McpError> {
    let block = block_read_request();
    let mut request = proto::DocumentPreferencesRequest {
        integers: std::iter::once(GENERAL_CLASS)
            .chain(DIMENSION_KINDS.iter().map(|k| k.1))
            .chain(block.integers)
            .collect(),
        doubles: DIMENSION_KINDS.iter().flat_map(|k| [k.2, k.3]).chain(block.doubles).collect(),
        ..Default::default()
    };
    let mut remarks = Vec::new();
    if p.iso2768.is_some() && (p.block.is_some() || p.block_angle_deg.is_some()) {
        return Err(invalid("give iso2768 (ISO) or block (ASME), not both"));
    }
    if let Some(class) = p.iso2768 {
        request.set_toggles.insert(BLOCK_TOLERANCE_TOGGLE, false);
        request.set_integers.insert(GENERAL_CLASS, class_code(class));
        for k in DIMENSION_KINDS {
            request.set_integers.insert(k.1, TOL_GENERAL);
        }
        // Dimensions already under a general tolerance take a new class on
        // a rebuild (seen 2026-11-12).
        request.rebuild = true;
        if standard == GdtStandard::Asme {
            remarks.push(
                "ISO 2768 is ISO's general tolerance; this part follows ASME (its drafting standard, or standard=asme), \
                 where add_pmi_dimension gives untoleranced dimensions the block tolerance instead (block=[...])"
                    .to_string(),
            );
        }
    }
    if let Some(rows) = &p.block {
        if rows.is_empty() || rows.len() > 3 {
            return Err(invalid("block: one to three rows, e.g. [{\"decimals\": 2, \"plus_minus_mm\": 0.254}]"));
        }
        let mut rows = rows.clone();
        rows.sort_by_key(|r| r.decimals);
        for (i, r) in rows.iter().enumerate() {
            if r.decimals > 4 {
                return Err(invalid(format!("block: decimals 0..4, got {}", r.decimals)));
            }
            if !(r.plus_minus_mm.is_finite() && r.plus_minus_mm > 0.0) {
                return Err(invalid(format!("block: plus_minus_mm must be positive, got {}", r.plus_minus_mm)));
            }
            if i > 0 && rows[i - 1].decimals == r.decimals {
                return Err(invalid(format!("block: two rows for {} decimals", r.decimals)));
            }
        }
        // DimXpert has three rows: fewer repeat the last one (it takes
        // repeated decimals).
        let last = rows[rows.len() - 1].clone();
        rows.resize(3, last);
        request.set_toggles.insert(BLOCK_TOLERANCE_TOGGLE, true);
        for (i, r) in rows.iter().enumerate() {
            request.set_integers.insert(BLOCK_DECIMALS[i], i32::from(r.decimals));
            request.set_doubles.insert(BLOCK_VALUES[i], r.plus_minus_mm / MM_PER_M);
        }
    }
    if let Some(deg) = p.block_angle_deg {
        if !(deg.is_finite() && deg > 0.0) {
            return Err(invalid(format!("block_angle_deg must be positive, got {deg}")));
        }
        request.set_doubles.insert(BLOCK_ANGLE, deg.to_radians());
    }
    if (p.block.is_some() || p.block_angle_deg.is_some()) && standard == GdtStandard::Iso {
        remarks.push(
            "block tolerances by decimal places are ASME's; this part follows ISO (its drafting standard, or \
             standard=iso), where untoleranced dimensions take ISO 2768 (iso2768=...) -- add_pmi_dimension applies the \
             block table only with standard=asme"
                .to_string(),
        );
    }
    Ok((request, remarks))
}

pub fn general_tolerance_json(
    p: &GeneralToleranceParams,
    reply: &proto::DocumentPreferencesResponse,
    standard: GdtStandard,
    units: Option<&proto::DocumentUnits>,
    remarks: &[String],
) -> Value {
    let mut obj = Map::new();
    obj.insert("document".into(), json!(reply.document));
    obj.insert("standard".into(), json!(standard.name()));
    obj.insert(
        "applies".into(),
        json!(match standard {
            GdtStandard::Iso => "DimXpert's general tolerance (ISO 2768-1) to untoleranced dimensions",
            GdtStandard::Asme => "the block tolerance below, given to each untoleranced dimension as its own +/- by \
                                  add_pmi_dimension (DimXpert's own block method can't be switched on through the API)",
        }),
    );
    if let Some(c) = reply.integers.get(&GENERAL_CLASS) {
        obj.insert("iso2768_class".into(), json!(class_name(*c)));
    }
    match block_table(reply) {
        Some(table) => {
            let mut rows: Vec<Value> = Vec::new();
            for (d, v) in &table.rows {
                let row = json!({ "decimals": d, "plus_minus_mm": mm(*v), "plus_minus": shown_length(*v, units) });
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
            obj.insert("block_tolerance".into(), Value::Array(rows));
            if let Some(a) = table.angle {
                obj.insert("block_angle_deg".into(), json!(round(a.to_degrees(), 4)));
            }
        }
        None => {
            obj.insert("block_tolerance".into(), json!("not set (the template's placeholder)"));
        }
    }
    let mut general = Vec::new();
    let mut own = Map::new();
    for (name, type_id, upper_id, lower_id, angular) in DIMENSION_KINDS {
        let unit = if *angular { "deg" } else { "mm" };
        let value = |id: &i32| {
            reply.doubles.get(id).map(|v| if *angular { round(v.to_degrees(), 4) } else { mm(*v) })
        };
        match reply.integers.get(type_id).copied() {
            Some(TOL_GENERAL) => general.push(json!(name)),
            Some(TOL_SYMMETRIC) => {
                own.insert(name.to_string(), json!({ "type": "symmetric", format!("plus_minus_{unit}"): value(upper_id) }));
            }
            Some(TOL_BILATERAL) => {
                own.insert(
                    name.to_string(),
                    json!({ "type": "bilateral", format!("upper_{unit}"): value(upper_id), format!("lower_{unit}"): value(lower_id) }),
                );
            }
            Some(other) => {
                own.insert(name.to_string(), json!({ "type": other }));
            }
            None => {}
        }
    }
    if !general.is_empty() {
        obj.insert("general_tolerance".into(), Value::Array(general));
    }
    if !own.is_empty() {
        obj.insert("own_default".into(), Value::Object(own));
    }
    let note = if p.iso2768.is_some() {
        "new DimXpert dimensions take this; existing ones with a general tolerance were updated, others keep theirs \
         (set_dimension_tolerance). ISO 2768-2 (geometrical) isn't part of it: say it in a note"
    } else if p.block.is_some() || p.block_angle_deg.is_some() {
        "new ASME dimensions without a tolerance take the row for the decimals they show; existing ones keep theirs"
    } else {
        "general_tolerance / own_default: what DimXpert gives each kind of new dimension (in ASME, the block table \
         replaces it when set)"
    };
    obj.insert("note".into(), json!(note));
    if !remarks.is_empty() {
        obj.insert("remarks".into(), json!(remarks));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISO: GdtStandard = GdtStandard::Iso;
    const ASME: GdtStandard = GdtStandard::Asme;

    fn registry() -> IdRegistry {
        let mut ids = IdRegistry::default();
        ids.id_for(Kind::Face, b"hole");
        ids.id_for(Kind::Face, b"plane");
        ids
    }

    fn params(kind: PmiDimensionKind, on: &[&str]) -> AddPmiDimensionParams {
        AddPmiDimensionParams {
            kind,
            on: on.iter().map(|s| s.to_string()).collect(),
            at_mm: None,
            tolerance: None,
            standard: None,
            values: ToleranceValues::default(),
        }
    }

    fn scheme(tolerancing: Option<SchemeTolerancing>) -> AutoDimensionSchemeParams {
        AutoDimensionSchemeParams {
            datums: vec!["face:2".into(), "face:1".into()],
            part_type: None,
            tolerancing,
            pattern: None,
            features: None,
            standard: None,
            position_mm: None,
            profile_mm: None,
            composite_position_mm: None,
            composite_profile_mm: None,
        }
    }

    fn general(iso2768: Option<Iso2768Class>, block: Option<Vec<BlockRow>>) -> GeneralToleranceParams {
        GeneralToleranceParams { iso2768, block, block_angle_deg: None, standard: None }
    }

    fn inch() -> proto::DocumentUnits {
        proto::DocumentUnits { unit_system: 3, length_unit: 3, length_decimals: 3, drafting_standard: 1 }
    }

    #[test]
    fn face_counts_and_ids() {
        let ids = registry();
        let (r, tol) = request(&ids, &params(PmiDimensionKind::Size, &["face:1"]), ISO).unwrap();
        assert_eq!((r.kind, r.faces.len(), tol.is_none()), (proto::PmiDimensionKind::Size as i32, 1, true));
        assert!(request(&ids, &params(PmiDimensionKind::Size, &["face:1", "face:2"]), ISO).is_err());
        assert!(request(&ids, &params(PmiDimensionKind::Location, &["face:1"]), ISO).is_err());
        assert!(request(&ids, &params(PmiDimensionKind::Location, &["face:1", "face:1"]), ISO).is_err());
        let (r, _) = request(&ids, &params(PmiDimensionKind::Location, &["face:2", "face:1"]), ISO).unwrap();
        assert_eq!(r.faces, vec![b"plane".to_vec(), b"hole".to_vec()]);
        assert!(request(&ids, &params(PmiDimensionKind::Basic, &["face:1"]), ISO).is_err());
        assert!(request(&ids, &params(PmiDimensionKind::Basic, &["face:2", "face:1"]), ISO).is_ok());
        assert!(request(&ids, &params(PmiDimensionKind::Size, &["edge:1"]), ISO).is_err());
        assert!(request(&ids, &params(PmiDimensionKind::Size, &["face:9"]), ISO).is_err());
        let at = AddPmiDimensionParams { at_mm: Some(vec![10.0, 20.0, 30.0]), ..params(PmiDimensionKind::Size, &["face:1"]) };
        let (r, _) = request(&ids, &at, ISO).unwrap();
        assert_eq!(r.position.map(|p| (p.x, p.y, p.z)), Some((0.01, 0.02, 0.03)));
    }

    #[test]
    fn tolerances() {
        let ids = registry();
        let fit = AddPmiDimensionParams {
            tolerance: Some(ToleranceKind::Fit),
            values: ToleranceValues { hole_fit: Some("H7".into()), ..ToleranceValues::default() },
            ..params(PmiDimensionKind::Size, &["face:1"])
        };
        let (_, tol) = request(&ids, &fit, ISO).unwrap();
        assert_eq!(tol.map(|t| (t.tolerance_type, t.hole_fit)), Some((7, "H7".to_string())));
        // Values without a kind, a tolerance on a basic dimension.
        let loose = AddPmiDimensionParams {
            values: ToleranceValues { plus_minus_mm: Some(0.1), ..ToleranceValues::default() },
            ..params(PmiDimensionKind::Size, &["face:1"])
        };
        assert!(request(&ids, &loose, ASME).is_err());
        let basic = AddPmiDimensionParams {
            tolerance: Some(ToleranceKind::Symmetric),
            values: ToleranceValues { plus_minus_mm: Some(0.1), ..ToleranceValues::default() },
            ..params(PmiDimensionKind::Basic, &["face:1", "face:2"])
        };
        assert!(request(&ids, &basic, ISO).is_err());
        let wrong = AddPmiDimensionParams { tolerance: Some(ToleranceKind::Basic), ..params(PmiDimensionKind::Size, &["face:1"]) };
        assert!(request(&ids, &wrong, ISO).is_err());
        // Decimals alone pick an ASME block row; in ISO they need a tolerance.
        let decimals = AddPmiDimensionParams {
            values: ToleranceValues { decimals: Some(3), ..ToleranceValues::default() },
            ..params(PmiDimensionKind::Size, &["face:1"])
        };
        assert!(request(&ids, &decimals, ASME).is_ok());
        assert!(request(&ids, &decimals, ISO).is_err());
    }

    #[test]
    fn overconstrained_is_refused_other_dialogs_noted() {
        let warning = Closed {
            title: "SOLIDWORKS".into(),
            text: "Added tolerance has overconstrained the part.".into(),
            pressed: "its close box".into(),
        };
        let why = overconstrained(std::slice::from_ref(&warning)).unwrap();
        assert!(why.contains("overconstrained the part") && !why.contains("deleted"), "{why}");
        let other = Closed { text: "Something else".into(), ..warning };
        assert!(overconstrained(std::slice::from_ref(&other)).is_none());
        assert!(dialog_note(&[other]).unwrap().contains("\"Something else\""));
        assert!(dialog_note(&[]).is_none());
    }

    fn table_reply(rows: [(i32, f64); 3], angle: f64) -> proto::DocumentPreferencesResponse {
        let mut reply = proto::DocumentPreferencesResponse { document: "Part1".into(), ..Default::default() };
        for (i, (d, v)) in rows.into_iter().enumerate() {
            reply.integers.insert(BLOCK_DECIMALS[i], d);
            reply.doubles.insert(BLOCK_VALUES[i], v);
        }
        reply.doubles.insert(BLOCK_ANGLE, angle);
        reply
    }

    fn dimension(kind: proto::DimensionType, decimals: Option<i32>) -> proto::AnnotationInfo {
        proto::AnnotationInfo {
            r#type: 4, // swDisplayDimension
            dimension: Some(proto::AnnotationDimension {
                dimension: Some(proto::DimensionInfo { name: "TXD1@Scheme2@Part1.Part".into(), r#type: kind as i32, ..Default::default() }),
                decimals,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn block_tolerance_by_decimals() {
        // The template's placeholder isn't a table.
        assert!(block_table(&table_reply(TEMPLATE_BLOCK_ROWS, 1.745e-4)).is_none());
        let table = block_table(&table_reply([(1, 0.00254), (2, 0.000254), (3, 0.000127)], 0.5f64.to_radians())).unwrap();
        let units = inch();
        // .XX: +/-.01 in, the tolerance with the dimension's decimals.
        let (r, what) = block_tolerance(Some(&table), &dimension(proto::DimensionType::Diameter, Some(2)), None, Some(&units)).unwrap();
        assert_eq!((r.name.as_str(), r.tolerance_type, r.angle), ("TXD1@Scheme2@Part1.Part", 4, false));
        assert!((r.upper - 0.000254).abs() < 1e-12 && (r.lower + 0.000254).abs() < 1e-12);
        assert_eq!((r.value_precision, r.tolerance_precision), (None, Some(2)));
        assert!(what.contains("0.01 inches") && what.contains("2 decimals"), "{what}");
        // `decimals` picks .XXX and is set on the dimension.
        let (r, _) = block_tolerance(Some(&table), &dimension(proto::DimensionType::Linear, Some(2)), Some(3), Some(&units)).unwrap();
        assert!((r.upper - 0.000127).abs() < 1e-12 && r.value_precision == Some(3));
        // Angles take the angular row.
        let (r, _) = block_tolerance(Some(&table), &dimension(proto::DimensionType::Angular, Some(1)), None, Some(&units)).unwrap();
        assert!(r.angle && (r.upper - 0.5f64.to_radians()).abs() < 1e-12 && r.tolerance_precision.is_none());
        // No row for 4 decimals, no table.
        assert!(block_tolerance(Some(&table), &dimension(proto::DimensionType::Linear, Some(4)), None, None).unwrap_err().contains("no row for 4"));
        assert!(block_tolerance(None, &dimension(proto::DimensionType::Linear, Some(2)), None, None).unwrap_err().contains("block=["));
    }

    #[test]
    fn angular_block_tolerance_does_not_enable_placeholder_length_rows() {
        let angle = 0.5f64.to_radians();
        let table = block_table(&table_reply(TEMPLATE_BLOCK_ROWS, angle)).unwrap();
        assert!(table.rows.is_empty(), "placeholder lengths must stay unset: {table:?}");
        let (request, _) = block_tolerance(Some(&table), &dimension(proto::DimensionType::Angular, Some(2)), None, None).unwrap();
        assert!((request.upper - angle).abs() < 1e-12 && request.angle, "{request:?}");
        assert!(block_tolerance(Some(&table), &dimension(proto::DimensionType::Linear, Some(2)), None, None).is_err());
        let p = GeneralToleranceParams { block_angle_deg: Some(0.5), ..general(None, None) };
        let json = general_tolerance_json(&p, &table_reply(TEMPLATE_BLOCK_ROWS, angle), ASME, None, &[]);
        assert_eq!(json["block_angle_deg"], json!(0.5), "{json}");
        assert_eq!(json["block_tolerance"], json!([]), "{json}");
        assert!(block_table(&table_reply(TEMPLATE_BLOCK_ROWS, 0.01f64.to_radians())).is_none());
    }

    #[test]
    fn block_tolerances_cover_every_dimension_and_preserve_annotation_indices() {
        let table = BlockTable { rows: vec![(2, 0.000254), (3, 0.000127)], angle: Some(0.5f64.to_radians()) };
        let mut diameter = dimension(proto::DimensionType::Diameter, Some(2));
        diameter.name = "TXD1".into();
        diameter.dimension.as_mut().unwrap().dimension.as_mut().unwrap().name = "diameter".into();
        let mut depth = dimension(proto::DimensionType::Linear, Some(3));
        depth.name = "TXD2".into();
        depth.dimension.as_mut().unwrap().dimension.as_mut().unwrap().name = "depth".into();
        let mut angle = dimension(proto::DimensionType::Angular, Some(2));
        angle.name = "TXD3".into();
        let mut added = vec![proto::AnnotationInfo::default(), diameter, depth, angle];
        let (requests, notes) = dimension_tolerances(&added, None, Some(&table), true, None, Some(&inch())).unwrap();
        assert_eq!(requests.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(requests.iter().map(|(_, r)| r.upper).collect::<Vec<_>>(), [0.000254, 0.000127, 0.5f64.to_radians()]);
        assert_eq!(notes.len(), 3, "{notes:?}");
        for (index, request) in requests {
            let d = added[index].dimension.as_mut().unwrap();
            d.tolerance_type = request.tolerance_type;
            d.tolerance_max = request.upper;
            d.tolerance_min = request.lower;
        }
        let text = reply(&mut registry(), &params(PmiDimensionKind::Size, &["face:1"]), &added[1..], &notes, &[]);
        let json: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        let annotations = json["annotations"].as_array().unwrap();
        assert_eq!(annotations[0]["tolerance"]["plus_minus_mm"], json!(0.254), "{text}");
        assert_eq!(annotations[1]["tolerance"]["plus_minus_mm"], json!(0.127), "{text}");
        assert_eq!(annotations[2]["tolerance"]["plus_minus_deg"], json!(0.5), "{text}");
    }

    #[test]
    fn cleanup_reports_confirmed_partial_and_failed_deletion() {
        let names = vec!["TXD1".to_string(), "TXD2".to_string()];
        let error = || invalid("tolerance refused");
        let deleted = |name: &str| proto::AnnotationInfo { name: name.into(), ..Default::default() };
        let complete = cleanup_error(error(), &names, Ok(proto::DeleteDrawingAnnotationsResponse {
            deleted: names.iter().map(|n| deleted(n)).collect(),
        }));
        assert!(complete.message.contains("were deleted again"), "{complete:?}");
        let partial = cleanup_error(error(), &names, Ok(proto::DeleteDrawingAnnotationsResponse {
            deleted: vec![deleted("TXD1")],
        }));
        assert!(partial.message.contains("did not confirm deletion of TXD2"), "{partial:?}");
        assert!(!partial.message.contains("were deleted again"), "{partial:?}");
        let failed = cleanup_error(error(), &names, Err(tonic::Status::unavailable("lost connection")));
        assert!(failed.message.contains("lost connection") && failed.message.contains("TXD1, TXD2"), "{failed:?}");
        assert!(!failed.message.contains("were deleted again"), "{failed:?}");
    }

    #[test]
    fn scheme_options() {
        let ids = registry();
        let p = AutoDimensionSchemeParams {
            features: Some(vec![FeatureFilter::Plane, FeatureFilter::SimpleHole]),
            ..scheme(Some(SchemeTolerancing::Geometric))
        };
        let r = scheme_request(&ids, &p).unwrap();
        assert_eq!((r.primary_datum.as_slice(), r.secondary_datum.as_slice(), r.tertiary_datum.is_empty()), (&b"plane"[..], &b"hole"[..], true));
        assert_eq!((r.tolerance_type, r.part_type, r.feature_filters), (1, 0, 1 | 128));
        assert!(scheme_request(&ids, &AutoDimensionSchemeParams { datums: vec![], ..p }).is_err());

        // Plus / minus: no options, and values refused.
        assert!(scheme_preferences(&scheme(None), ISO).unwrap().is_none());
        assert!(scheme_preferences(&AutoDimensionSchemeParams { position_mm: Some(0.5), ..scheme(None) }, ISO).is_err());
        // Geometric: composite off unless given; ASME only; under the upper segment.
        let plain = scheme_preferences(&scheme(Some(SchemeTolerancing::Geometric)), ISO).unwrap().unwrap();
        assert_eq!((plain.set_toggles.get(&COMPOSITE_POSITION), plain.set_toggles.get(&COMPOSITE_PROFILE)), (Some(&false), Some(&false)));
        let composite = AutoDimensionSchemeParams {
            position_mm: Some(0.508),
            composite_position_mm: Some(0.127),
            ..scheme(Some(SchemeTolerancing::Geometric))
        };
        let r = scheme_preferences(&composite, ASME).unwrap().unwrap();
        assert_eq!(r.set_toggles.get(&COMPOSITE_POSITION), Some(&true));
        assert!((r.set_doubles[&POSITION_VALUE] - 0.000508).abs() < 1e-12 && (r.set_doubles[&COMPOSITE_POSITION_VALUE] - 0.000127).abs() < 1e-12);
        assert!(scheme_preferences(&composite, ISO).unwrap_err().message.contains("ASME"));
        let upside_down = AutoDimensionSchemeParams { composite_position_mm: Some(0.6), ..composite };
        assert!(scheme_preferences(&upside_down, ASME).is_err());
        // .005 in prints as .01 with DimXpert's 2 decimals.
        let fine = AutoDimensionSchemeParams { composite_position_mm: Some(0.127), position_mm: Some(0.508), ..scheme(Some(SchemeTolerancing::Geometric)) };
        let note = frame_rounding_note(&fine, Some(&inch())).unwrap();
        assert!(note.contains("0.005 as 0.01") && !note.contains("0.02 as"), "{note}");
        assert!(frame_rounding_note(&AutoDimensionSchemeParams { position_mm: Some(0.5), ..scheme(None) }, None).is_none());
    }

    #[test]
    fn general_tolerance_iso_and_asme() {
        let (read, remarks) = general_tolerance_request(&general(None, None), ISO).unwrap();
        assert!(read.set_integers.is_empty() && !read.rebuild && read.integers.contains(&GENERAL_CLASS) && remarks.is_empty());
        assert!(read.integers.contains(&BLOCK_DECIMALS[0]) && read.doubles.contains(&BLOCK_ANGLE));
        let (set, remarks) = general_tolerance_request(&general(Some(Iso2768Class::Coarse), None), ISO).unwrap();
        assert_eq!(set.set_integers.get(&GENERAL_CLASS), Some(&2));
        assert_eq!(set.set_integers.get(&421), Some(&TOL_GENERAL));
        assert_eq!(set.set_toggles.get(&BLOCK_TOLERANCE_TOGGLE), Some(&false));
        assert!(set.rebuild && remarks.is_empty());
        // ISO 2768 in an ASME part: a remark.
        assert_eq!(general_tolerance_request(&general(Some(Iso2768Class::Medium), None), ASME).unwrap().1.len(), 1);
        // Block rows: sorted, padded to three, in meters; both kinds refused.
        let rows = vec![BlockRow { decimals: 3, plus_minus_mm: 0.127 }, BlockRow { decimals: 2, plus_minus_mm: 0.254 }];
        let p = GeneralToleranceParams { block_angle_deg: Some(0.5), ..general(None, Some(rows.clone())) };
        let (set, remarks) = general_tolerance_request(&p, ASME).unwrap();
        assert!(remarks.is_empty());
        let decimals: Vec<i32> = BLOCK_DECIMALS.iter().map(|id| set.set_integers[id]).collect();
        assert_eq!(decimals, vec![2, 3, 3]);
        assert_eq!(set.set_toggles.get(&BLOCK_TOLERANCE_TOGGLE), Some(&true));
        assert!((set.set_doubles[&BLOCK_VALUES[0]] - 0.000254).abs() < 1e-12);
        assert!((set.set_doubles[&BLOCK_ANGLE] - 0.5f64.to_radians()).abs() < 1e-12);
        assert_eq!(general_tolerance_request(&p, ISO).unwrap().1.len(), 1);
        assert!(general_tolerance_request(&general(Some(Iso2768Class::Fine), Some(rows)), ASME).is_err());
        let twice = vec![BlockRow { decimals: 2, plus_minus_mm: 0.1 }, BlockRow { decimals: 2, plus_minus_mm: 0.2 }];
        assert!(general_tolerance_request(&general(None, Some(twice)), ASME).is_err());

        let mut reply = table_reply([(2, 0.000254), (3, 0.000127), (3, 0.000127)], 0.5f64.to_radians());
        reply.integers.insert(GENERAL_CLASS, 1);
        reply.integers.insert(421, TOL_GENERAL);
        reply.integers.insert(409, TOL_SYMMETRIC);
        reply.doubles.insert(127, 0.0005);
        let units = inch();
        let json = general_tolerance_json(&general(None, None), &reply, ASME, Some(&units), &[]);
        assert_eq!(json["iso2768_class"], "m (medium)");
        assert_eq!(json["general_tolerance"], json!(["diameter"]));
        assert_eq!(json["own_default"]["location_distance"]["plus_minus_mm"], json!(0.5));
        assert_eq!(json["block_tolerance"].as_array().map(Vec::len), Some(2), "{json}");
        assert_eq!(json["block_tolerance"][0]["plus_minus"], "0.01 inches (0.254 mm)");
        assert_eq!(json["block_angle_deg"], json!(0.5));
        assert!(json["applies"].as_str().unwrap().contains("block tolerance"));
        let placeholder = general_tolerance_json(&general(None, None), &table_reply(TEMPLATE_BLOCK_ROWS, 1.745e-4), ISO, None, &[]);
        assert!(placeholder["block_tolerance"].as_str().unwrap().contains("not set"));
        let class: GeneralToleranceParams = serde_json::from_value(json!({ "iso2768": "fine" })).unwrap();
        assert_eq!(class.iso2768, Some(Iso2768Class::Fine));
    }
}
