//! The hole_wizard tool: friendly names for SolidWorks' hole standards and
//! fastener types, request building and the reply text.
//!
//! SolidWorks identifies a fastener type by one big enum
//! (swWzdHoleStandardFastenerTypes_e) whose values differ per standard; the
//! table here maps (standard, fastener) to it so the add-in can pass it
//! straight through.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{IdRegistry, MM_PER_M, mm, round};
use crate::sketch;

/// Most locations per call (the add-in's limit).
pub const MAX_HOLE_LOCATIONS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HoleKind {
    /// Plain drilled hole: screw clearance, drill size, tap drill or dowel.
    Hole,
    /// Counterbored hole for a screw or bolt head.
    Counterbore,
    /// Countersunk hole for a flat (countersunk) head screw.
    Countersink,
    /// Straight tapped (threaded) hole.
    Tapped,
}

impl HoleKind {
    fn name(self) -> &'static str {
        match self {
            HoleKind::Hole => "hole",
            HoleKind::Counterbore => "counterbore",
            HoleKind::Countersink => "countersink",
            HoleKind::Tapped => "tapped hole",
        }
    }

    fn default_fastener(self) -> Fastener {
        match self {
            HoleKind::Hole => Fastener::ScrewClearance,
            HoleKind::Counterbore => Fastener::SocketHeadCapScrew,
            HoleKind::Countersink => Fastener::FlatHead,
            HoleKind::Tapped => Fastener::Tapped,
        }
    }
}

impl From<HoleKind> for proto::HoleKind {
    fn from(kind: HoleKind) -> Self {
        match kind {
            HoleKind::Hole => proto::HoleKind::Hole,
            HoleKind::Counterbore => proto::HoleKind::Counterbore,
            HoleKind::Countersink => proto::HoleKind::Countersink,
            HoleKind::Tapped => proto::HoleKind::Tap,
        }
    }
}

/// Hole standard (swWzdHoleStandards_e).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HoleStandard {
    #[default]
    Iso,
    AnsiMetric,
    AnsiInch,
    Din,
    Jis,
}

impl HoleStandard {
    fn sw(self) -> i32 {
        match self {
            HoleStandard::AnsiInch => 0,
            HoleStandard::AnsiMetric => 1,
            HoleStandard::Din => 4,
            HoleStandard::Iso => 8,
            HoleStandard::Jis => 9,
        }
    }

    fn from_sw(value: i32) -> Option<Self> {
        [Self::Iso, Self::AnsiMetric, Self::AnsiInch, Self::Din, Self::Jis]
            .into_iter()
            .find(|s| s.sw() == value)
    }

    fn name(self) -> &'static str {
        match self {
            HoleStandard::Iso => "ISO",
            HoleStandard::AnsiMetric => "ANSI Metric",
            HoleStandard::AnsiInch => "ANSI Inch",
            HoleStandard::Din => "DIN",
            HoleStandard::Jis => "JIS",
        }
    }
}

/// What the hole is for: the Hole Wizard's fastener type. Which ones apply depends on the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fastener {
    // kind = hole
    ScrewClearance,
    Drill,
    TapDrill,
    Dowel,
    // kind = counterbore
    SocketHeadCapScrew,
    HexBolt,
    HexCapScrew,
    PanHead,
    CheeseHead,
    ButtonHead,
    ShoulderScrew,
    // kind = countersink
    FlatHead,
    FlatHead100,
    SocketFlatHead,
    RaisedHead,
    // kind = tapped
    Tapped,
    BottomingTapped,
}

const ALL_FASTENERS: [Fastener; 17] = {
    use Fastener::*;
    [
        ScrewClearance, Drill, TapDrill, Dowel, SocketHeadCapScrew, HexBolt, HexCapScrew, PanHead,
        CheeseHead, ButtonHead, ShoulderScrew, FlatHead, FlatHead100, SocketFlatHead, RaisedHead,
        Tapped, BottomingTapped,
    ]
};

impl Fastener {
    fn kind(self) -> HoleKind {
        use Fastener::*;
        match self {
            ScrewClearance | Drill | TapDrill | Dowel => HoleKind::Hole,
            SocketHeadCapScrew | HexBolt | HexCapScrew | PanHead | CheeseHead | ButtonHead
            | ShoulderScrew => HoleKind::Counterbore,
            FlatHead | FlatHead100 | SocketFlatHead | RaisedHead => HoleKind::Countersink,
            Tapped | BottomingTapped => HoleKind::Tapped,
        }
    }

    /// The tool-facing name (its serde name).
    fn name(self) -> &'static str {
        use Fastener::*;
        match self {
            ScrewClearance => "screw_clearance",
            Drill => "drill",
            TapDrill => "tap_drill",
            Dowel => "dowel",
            SocketHeadCapScrew => "socket_head_cap_screw",
            HexBolt => "hex_bolt",
            HexCapScrew => "hex_cap_screw",
            PanHead => "pan_head",
            CheeseHead => "cheese_head",
            ButtonHead => "button_head",
            ShoulderScrew => "shoulder_screw",
            FlatHead => "flat_head",
            FlatHead100 => "flat_head_100",
            SocketFlatHead => "socket_flat_head",
            RaisedHead => "raised_head",
            Tapped => "tapped",
            BottomingTapped => "bottoming_tapped",
        }
    }

    /// swWzdHoleStandardFastenerTypes_e for this fastener in `standard`, if the standard has it.
    fn sw(self, standard: HoleStandard) -> Option<i32> {
        use Fastener::*;
        use HoleStandard::*;
        Some(match (standard, self) {
            (AnsiInch, ScrewClearance) => 22,
            (AnsiInch, Drill) => 18, // all drill sizes: fractional, letter and number
            (AnsiInch, TapDrill) => 23,
            (AnsiInch, Dowel) => 703,
            (AnsiInch, SocketHeadCapScrew) => 9,
            (AnsiInch, HexBolt) => 3,
            (AnsiInch, HexCapScrew) => 6,
            (AnsiInch, PanHead) => 8,
            (AnsiInch, ButtonHead) => 1,
            (AnsiInch, ShoulderScrew) => 10,
            (AnsiInch, FlatHead) => 15, // 82 degrees
            (AnsiInch, FlatHead100) => 14,
            (AnsiInch, SocketFlatHead) => 13,
            (AnsiInch, RaisedHead) => 16, // oval head
            (AnsiInch, Tapped) => 27,
            (AnsiInch, BottomingTapped) => 26,

            (AnsiMetric, ScrewClearance) => 40,
            (AnsiMetric, Drill) => 39,
            (AnsiMetric, TapDrill) => 41,
            (AnsiMetric, Dowel) => 704,
            (AnsiMetric, SocketHeadCapScrew) => 33,
            (AnsiMetric, HexBolt) => 29,
            (AnsiMetric, HexCapScrew) => 30,
            (AnsiMetric, PanHead) => 32,
            (AnsiMetric, ButtonHead) => 28,
            (AnsiMetric, ShoulderScrew) => 34,
            (AnsiMetric, FlatHead) => 36,
            (AnsiMetric, SocketFlatHead) => 35,
            (AnsiMetric, RaisedHead) => 37, // oval head
            (AnsiMetric, Tapped) => 43,
            (AnsiMetric, BottomingTapped) => 42,

            (Iso, ScrewClearance) => 144,
            (Iso, Drill) => 143,
            (Iso, TapDrill) => 145,
            (Iso, Dowel) => 710,
            (Iso, SocketHeadCapScrew) => 139,
            (Iso, HexBolt) => 135,
            (Iso, HexCapScrew) => 136,
            (Iso, PanHead) => 138,
            (Iso, CheeseHead) => 134,
            (Iso, FlatHead) => 141,
            (Iso, SocketFlatHead) => 140,
            (Iso, RaisedHead) => 142,
            (Iso, Tapped) => 147,
            (Iso, BottomingTapped) => 146,

            (Din, ScrewClearance) => 73,
            (Din, Drill) => 72,
            (Din, TapDrill) => 74,
            (Din, Dowel) => 707,
            (Din, SocketHeadCapScrew) => 67,
            (Din, HexBolt) => 63,
            (Din, HexCapScrew) => 64,
            (Din, PanHead) => 66,
            (Din, CheeseHead) => 62,
            (Din, FlatHead) => 69,
            (Din, SocketFlatHead) => 68,
            (Din, RaisedHead) => 70,
            (Din, Tapped) => 76,
            (Din, BottomingTapped) => 75,

            (Jis, ScrewClearance) => 161,
            (Jis, Drill) => 160,
            (Jis, TapDrill) => 162,
            (Jis, Dowel) => 711,
            (Jis, SocketHeadCapScrew) => 156,
            (Jis, HexBolt) => 152,
            (Jis, HexCapScrew) => 153,
            (Jis, PanHead) => 155,
            (Jis, CheeseHead) => 149,
            (Jis, ButtonHead) => 151,
            (Jis, ShoulderScrew) => 157,
            (Jis, FlatHead) => 158,
            (Jis, RaisedHead) => 159,
            (Jis, Tapped) => 164,
            (Jis, BottomingTapped) => 163,

            _ => return None,
        })
    }

    fn from_sw(value: i32) -> Option<(HoleStandard, Fastener)> {
        use HoleStandard::*;
        [Iso, AnsiMetric, AnsiInch, Din, Jis].into_iter().find_map(|standard| {
            ALL_FASTENERS
                .into_iter()
                .find(|f| f.sw(standard) == Some(value))
                .map(|f| (standard, f))
        })
    }
}

/// The standard for a hole of `size` when none is given: metric sizes ("M5")
/// are ISO, numbered and fractional ones ("#10", "1/4") ANSI Inch; anything
/// else (drill sizes) follows the part's units.
pub fn default_standard(size: &str, inch_document: bool) -> HoleStandard {
    let size = size.trim();
    let metric = size.strip_prefix(['M', 'm']).is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    if metric {
        HoleStandard::Iso
    } else if size.starts_with('#') || size.contains('/') || inch_document {
        HoleStandard::AnsiInch
    } else {
        HoleStandard::Iso
    }
}

/// Clearance between screw and hole.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScrewFit {
    Close,
    Normal,
    Loose,
}

impl From<ScrewFit> for proto::ScrewFit {
    fn from(fit: ScrewFit) -> Self {
        match fit {
            ScrewFit::Close => proto::ScrewFit::Close,
            ScrewFit::Normal => proto::ScrewFit::Normal,
            ScrewFit::Loose => proto::ScrewFit::Loose,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HoleWizardParams {
    /// "hole" (plain drilled/clearance hole), "counterbore", "countersink" or "tapped".
    pub kind: HoleKind,
    /// The size exactly as the Hole Wizard lists it for the standard and fastener, e.g. "M5"
    /// (metric screws and taps), "#10" or "1/4" (ANSI inch).
    pub size: String,
    /// Put a hole at every sketch point of this sketch, e.g. "Sketch3" -- typically points
    /// placed with sketch_point on a face and located with dimension_sketch. The sketch may
    /// also hold construction geometry (ignored) but nothing else, and mustn't be used by
    /// another feature. It becomes the hole feature's placement sketch, so its dimensions keep
    /// driving the holes. Give this or `on`.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// Instead of sketch_name: the planar face the holes start on, an id from inspect_model,
    /// e.g. "face:4", with `locations`. Holes go into the material, against the face's outward
    /// normal.
    #[serde(default)]
    pub on: Option<String>,
    /// With `on`: hole centers as model coordinates [x, y, z] in millimeters, each projected
    /// onto the face; every one must land on the face. Omit for a single hole at the middle of
    /// the face's bounding box.
    #[serde(default)]
    pub locations: Option<Vec<[f64; 3]>>,
    /// Hole standard: "iso", "ansi_metric", "ansi_inch", "din" or "jis". Default: by the size
    /// ("M5" iso, "#10" or "1/4" ansi_inch), else ansi_inch in an inch part and iso otherwise.
    #[serde(default)]
    pub standard: Option<HoleStandard>,
    /// The fastener the hole is for. kind=hole: "screw_clearance" (default), "drill",
    /// "tap_drill", "dowel". counterbore: "socket_head_cap_screw" (default), "hex_bolt",
    /// "hex_cap_screw", "pan_head", "cheese_head" (ISO/DIN/JIS), "button_head" and
    /// "shoulder_screw" (ANSI/JIS). countersink: "flat_head" (default), "flat_head_100" (ANSI
    /// inch), "socket_flat_head", "raised_head". tapped: "tapped" (default),
    /// "bottoming_tapped".
    #[serde(default)]
    pub fastener: Option<Fastener>,
    /// Hole depth in millimeters (for tapped holes, the drilled depth). Required unless
    /// through_all is set.
    #[serde(default)]
    pub depth_mm: Option<f64>,
    /// Go through the whole part instead of to a depth. Defaults to false.
    #[serde(default)]
    pub through_all: bool,
    /// Drill the other way (away from the material side), e.g. when the sketch is on a
    /// reference plane or SolidWorks says the holes miss the material. Defaults to false.
    #[serde(default)]
    pub reverse_direction: bool,
    /// Screw clearance ("close", "normal", "loose") for clearance holes, counterbores and
    /// countersinks; defaults to SolidWorks' normal fit.
    #[serde(default)]
    pub fit: Option<ScrewFit>,
    /// Override the standard's hole diameter, in millimeters.
    #[serde(default)]
    pub diameter_mm: Option<f64>,
    /// Counterbores only: override the counterbore diameter, in millimeters.
    #[serde(default)]
    pub counterbore_diameter_mm: Option<f64>,
    /// Counterbores only: override the counterbore depth, in millimeters.
    #[serde(default)]
    pub counterbore_depth_mm: Option<f64>,
    /// Countersinks only: override the countersink diameter, in millimeters.
    #[serde(default)]
    pub countersink_diameter_mm: Option<f64>,
    /// Countersinks only: override the countersink's included angle, in degrees.
    #[serde(default)]
    pub countersink_angle_deg: Option<f64>,
    /// Tapped holes only: override the thread depth, in millimeters.
    #[serde(default)]
    pub thread_depth_mm: Option<f64>,
    /// Tapped holes only: show a cosmetic thread (with its callout). Defaults to false.
    #[serde(default)]
    pub cosmetic_thread: bool,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// An optional positive millimeter value -> meters.
fn opt_mm(value: Option<f64>, what: &str) -> Result<Option<f64>, McpError> {
    match value {
        None => Ok(None),
        Some(v) if v.is_finite() && v > 0.0 => Ok(Some(v / MM_PER_M)),
        Some(v) => Err(invalid(format!("{what} must be a positive number of millimeters, got {v}"))),
    }
}

/// The trimmed sketch name, if one was given.
pub fn sketch_name(params: &HoleWizardParams) -> Option<&str> {
    params.sketch_name.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The trimmed face id, if one was given.
pub fn face_id(params: &HoleWizardParams) -> Option<&str> {
    params.on.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The wire request. `face_persist_id` is what `params.on` resolved to (empty with a sketch).
pub fn request(params: &HoleWizardParams, face_persist_id: Vec<u8>) -> Result<proto::HoleWizardRequest, McpError> {
    let kind = params.kind;
    let sketch_name = sketch_name(params);
    match (sketch_name, face_id(params)) {
        (Some(_), Some(_)) => {
            return Err(invalid("give either sketch_name (a sketch of hole points) or `on` (a face), not both"));
        }
        (None, None) => {
            return Err(invalid(
                "give sketch_name (a sketch whose points are the hole centers) or `on` (a face id) \
                 with locations",
            ));
        }
        (Some(_), None) if params.locations.is_some() => {
            return Err(invalid("locations only go with `on`; with a sketch, its points are the hole centers"));
        }
        _ => {}
    }
    let size = params.size.trim();
    if size.is_empty() {
        return Err(invalid("give the hole size, e.g. \"M5\""));
    }

    let fastener = params.fastener.unwrap_or(kind.default_fastener());
    if fastener.kind() != kind {
        return Err(invalid(format!(
            "fastener \"{}\" goes with kind \"{}\", not \"{}\"",
            fastener.name(),
            fastener.kind().name(),
            kind.name()
        )));
    }
    let standard = params.standard.unwrap_or_default();
    let fastener_type = fastener.sw(standard).ok_or_else(|| {
        let available: Vec<&str> = ALL_FASTENERS
            .into_iter()
            .filter(|f| f.kind() == kind && f.sw(standard).is_some())
            .map(Fastener::name)
            .collect();
        invalid(format!(
            "the {} standard has no \"{}\" {}; it has: {}",
            standard.name(),
            fastener.name(),
            kind.name(),
            available.join(", ")
        ))
    })?;

    let depth = match (params.through_all, params.depth_mm) {
        (true, _) => 0.0,
        (false, Some(d)) if d.is_finite() && d > 0.0 => d / MM_PER_M,
        (false, Some(d)) => {
            return Err(invalid(format!("depth must be a positive number of millimeters, got {d}")));
        }
        (false, None) => return Err(invalid("give depth_mm, or set through_all")),
    };

    // Options that only apply to some kinds.
    let only = |set: bool, name: &str, for_kind: HoleKind| -> Result<(), McpError> {
        if set && kind != for_kind {
            return Err(invalid(format!("{name} only applies to kind \"{}\"", for_kind.name())));
        }
        Ok(())
    };
    only(params.counterbore_diameter_mm.is_some(), "counterbore_diameter_mm", HoleKind::Counterbore)?;
    only(params.counterbore_depth_mm.is_some(), "counterbore_depth_mm", HoleKind::Counterbore)?;
    only(params.countersink_diameter_mm.is_some(), "countersink_diameter_mm", HoleKind::Countersink)?;
    only(params.countersink_angle_deg.is_some(), "countersink_angle_deg", HoleKind::Countersink)?;
    only(params.thread_depth_mm.is_some(), "thread_depth_mm", HoleKind::Tapped)?;
    only(params.cosmetic_thread, "cosmetic_thread", HoleKind::Tapped)?;
    if params.fit.is_some() && kind == HoleKind::Tapped {
        return Err(invalid("fit doesn't apply to tapped holes"));
    }

    let countersink_angle = match params.countersink_angle_deg {
        None => None,
        Some(a) if a.is_finite() && a > 0.0 && a < 180.0 => Some(a.to_radians()),
        Some(a) => return Err(invalid(format!("countersink_angle_deg must be between 0 and 180, got {a}"))),
    };

    let locations = params.locations.as_deref().unwrap_or_default();
    if locations.len() > MAX_HOLE_LOCATIONS {
        return Err(invalid(format!("at most {MAX_HOLE_LOCATIONS} locations per call")));
    }
    let locations = locations
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if p.iter().all(|v| v.is_finite()) {
                Ok(proto::Vec3 { x: p[0] / MM_PER_M, y: p[1] / MM_PER_M, z: p[2] / MM_PER_M })
            } else {
                Err(invalid(format!("location {} must be finite", i + 1)))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(proto::HoleWizardRequest {
        sketch_name: sketch_name.unwrap_or_default().to_string(),
        face_persist_id,
        locations,
        kind: proto::HoleKind::from(kind).into(),
        standard: standard.sw(),
        fastener_type,
        size: size.to_string(),
        through_all: params.through_all,
        depth,
        reverse_direction: params.reverse_direction,
        fit: params.fit.map(proto::ScrewFit::from).unwrap_or(proto::ScrewFit::Unspecified).into(),
        diameter: opt_mm(params.diameter_mm, "diameter_mm")?,
        counterbore_diameter: opt_mm(params.counterbore_diameter_mm, "counterbore_diameter_mm")?,
        counterbore_depth: opt_mm(params.counterbore_depth_mm, "counterbore_depth_mm")?,
        countersink_diameter: opt_mm(params.countersink_diameter_mm, "countersink_diameter_mm")?,
        countersink_angle,
        thread_depth: opt_mm(params.thread_depth_mm, "thread_depth_mm")?,
        cosmetic_thread: params.cosmetic_thread,
    })
}

/// "Ø5.5 mm" etc., or None for a value SolidWorks didn't report.
fn len(label: &str, m: f64) -> Option<String> {
    (m > 0.0).then(|| format!("{label}{} mm", mm(m)))
}

/// The sizes SolidWorks reports, for the reply, e.g. "Ø5.5 mm through all, counterbore Ø10 x 5.5 mm".
fn spec_text(kind: HoleKind, spec: &proto::HoleSpec) -> String {
    let depth = if spec.through_all {
        Some("through all".to_string())
    } else {
        len("", spec.depth).map(|d| format!("{d} deep"))
    };
    let mut parts: Vec<String> = Vec::new();
    match kind {
        HoleKind::Tapped => {
            parts.extend(len("tap drill Ø", spec.tap_drill_diameter));
            parts.extend(depth);
            parts.extend(len("thread Ø", spec.thread_diameter));
            parts.extend(len("thread depth ", spec.thread_depth));
        }
        _ => {
            parts.extend(len("Ø", spec.diameter));
            parts.extend(depth);
        }
    }
    match kind {
        HoleKind::Counterbore => {
            if let (Some(d), Some(h)) = (len("Ø", spec.counterbore_diameter), len("", spec.counterbore_depth)) {
                parts.push(format!("counterbore {d} x {h} deep"));
            }
        }
        HoleKind::Countersink => {
            if let Some(d) = len("Ø", spec.countersink_diameter) {
                let angle = if spec.countersink_angle > 0.0 {
                    format!(" x {}°", round(spec.countersink_angle.to_degrees(), 2))
                } else {
                    String::new()
                };
                parts.push(format!("countersink {d}{angle}"));
            }
        }
        _ => {}
    }
    parts.join(", ")
}

/// The tool's reply.
pub fn reply_text(ids: &mut IdRegistry, params: &HoleWizardParams, reply: &proto::HoleWizardResponse) -> String {
    let spec = reply.spec.clone().unwrap_or_default();
    let size = if spec.size.is_empty() { params.size.trim() } else { spec.size.as_str() };
    let fastener = match Fastener::from_sw(spec.fastener_type) {
        Some((standard, f)) => format!("{} {}", standard.name(), f.name().replace('_', " ")),
        None => HoleStandard::from_sw(spec.standard)
            .unwrap_or(params.standard.unwrap_or_default())
            .name()
            .to_string(),
    };
    let n = reply.centers.len();
    let plural = if n == 1 { "" } else { "s" };
    let mut text = format!(
        "created {}: {n} {size} {}{plural} ({fastener})",
        reply.feature_name,
        params.kind.name(),
    );
    let sizes = spec_text(params.kind, &spec);
    if !sizes.is_empty() {
        text += &format!(", {sizes}");
    }
    text.push('.');

    // Hole centers, with ids to dimension them by.
    let centers: Vec<String> = reply
        .centers
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let id = sketch::item_id(ids, item)?;
            Some(match reply.centers_model.get(i) {
                Some(p) => format!("{id} at {}", crate::inspect::point_mm(p)),
                None => id,
            })
        })
        .collect();
    if !centers.is_empty() {
        text += &format!(
            " Hole centers (model mm) are the points of placement sketch {}: {}.",
            reply.sketch_name,
            centers.join(", ")
        );
        text += &if sketch_name(params).is_some() {
            format!(
                " {} now belongs to the hole feature; its dimensions locate the holes \
                 (set_dimension on them moves the holes).",
                reply.sketch_name
            )
        } else {
            format!(
                " They aren't dimensioned yet: dimension_sketch / add_sketch_relations with \
                 sketch_name \"{}\" locate them (e.g. distances to model edges).",
                reply.sketch_name
            )
        };
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(kind: HoleKind) -> HoleWizardParams {
        HoleWizardParams {
            kind,
            size: "M5".into(),
            sketch_name: None,
            on: Some("face:1".into()),
            locations: None,
            standard: Some(HoleStandard::Iso),
            fastener: None,
            depth_mm: None,
            through_all: true,
            reverse_direction: false,
            fit: None,
            diameter_mm: None,
            counterbore_diameter_mm: None,
            counterbore_depth_mm: None,
            countersink_diameter_mm: None,
            countersink_angle_deg: None,
            thread_depth_mm: None,
            cosmetic_thread: false,
        }
    }

    #[test]
    fn standards_follow_the_size_then_the_part() {
        assert_eq!(default_standard("M5", true), HoleStandard::Iso);
        assert_eq!(default_standard(" #10", false), HoleStandard::AnsiInch);
        assert_eq!(default_standard("1/4", false), HoleStandard::AnsiInch);
        assert_eq!(default_standard("Ø5.0", false), HoleStandard::Iso);
        assert_eq!(default_standard("Letter F", true), HoleStandard::AnsiInch);
        // Unset in the request: ISO.
        let p = HoleWizardParams { standard: None, ..params(HoleKind::Counterbore) };
        assert_eq!(request(&p, vec![1]).unwrap().standard, 8);
    }

    #[test]
    fn defaults_pick_a_fastener_per_kind() {
        let r = request(&params(HoleKind::Counterbore), vec![1]).unwrap();
        assert_eq!((r.standard, r.fastener_type), (8, 139)); // ISO socket head cap screw
        assert_eq!(r.kind(), proto::HoleKind::Counterbore);
        assert!(r.through_all);
        assert_eq!(request(&params(HoleKind::Hole), vec![1]).unwrap().fastener_type, 144);
        assert_eq!(request(&params(HoleKind::Countersink), vec![1]).unwrap().fastener_type, 141);
        assert_eq!(request(&params(HoleKind::Tapped), vec![1]).unwrap().fastener_type, 147);
    }

    #[test]
    fn every_kind_has_its_default_fastener_in_every_standard() {
        use HoleStandard::*;
        for standard in [Iso, AnsiMetric, AnsiInch, Din, Jis] {
            for kind in [HoleKind::Hole, HoleKind::Counterbore, HoleKind::Countersink, HoleKind::Tapped] {
                assert!(kind.default_fastener().sw(standard).is_some(), "{standard:?} {kind:?}");
            }
        }
    }

    #[test]
    fn fastener_values_are_unique_and_round_trip() {
        use HoleStandard::*;
        let mut seen = std::collections::HashSet::new();
        for standard in [Iso, AnsiMetric, AnsiInch, Din, Jis] {
            for f in ALL_FASTENERS {
                if let Some(v) = f.sw(standard) {
                    assert!(seen.insert(v), "{v} used twice");
                    assert_eq!(Fastener::from_sw(v), Some((standard, f)));
                }
            }
        }
    }

    #[test]
    fn rejects_mismatches() {
        let mut p = params(HoleKind::Counterbore);
        p.fastener = Some(Fastener::FlatHead);
        assert!(request(&p, vec![1]).is_err());

        let mut p = params(HoleKind::Counterbore);
        p.fastener = Some(Fastener::ButtonHead); // ISO has none
        let err = request(&p, vec![1]).unwrap_err();
        assert!(err.message.contains("socket_head_cap_screw"), "{}", err.message);

        let mut p = params(HoleKind::Hole);
        p.countersink_angle_deg = Some(90.0);
        assert!(request(&p, vec![1]).is_err());

        let mut p = params(HoleKind::Tapped);
        p.fit = Some(ScrewFit::Close);
        assert!(request(&p, vec![1]).is_err());

        let mut p = params(HoleKind::Hole);
        p.through_all = false;
        assert!(request(&p, vec![1]).is_err());
        p.depth_mm = Some(-2.0);
        assert!(request(&p, vec![1]).is_err());
    }

    #[test]
    fn takes_a_sketch_or_a_face() {
        let mut p = params(HoleKind::Tapped);
        p.on = None;
        p.sketch_name = Some(" Sketch3 ".into());
        let r = request(&p, Vec::new()).unwrap();
        assert_eq!(r.sketch_name, "Sketch3");
        assert!(r.face_persist_id.is_empty());

        p.locations = Some(vec![[0.0, 0.0, 0.0]]);
        assert!(request(&p, Vec::new()).is_err()); // locations need a face
        p.locations = None;
        p.on = Some("face:1".into());
        assert!(request(&p, vec![1]).is_err()); // both
        p.on = None;
        p.sketch_name = None;
        assert!(request(&p, Vec::new()).is_err()); // neither
    }

    #[test]
    fn converts_units() {
        let mut p = params(HoleKind::Countersink);
        p.through_all = false;
        p.depth_mm = Some(12.0);
        p.countersink_angle_deg = Some(90.0);
        p.countersink_diameter_mm = Some(10.0);
        p.locations = Some(vec![[10.0, 20.0, 5.0]]);
        let r = request(&p, vec![1]).unwrap();
        assert_eq!(r.depth, 0.012);
        assert_eq!(r.countersink_diameter, Some(0.01));
        assert!((r.countersink_angle.unwrap() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        let l = &r.locations[0];
        assert_eq!((l.x, l.y, l.z), (0.01, 0.02, 0.005));
        assert_eq!(r.counterbore_diameter, None);
    }
}
