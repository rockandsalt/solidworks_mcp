//! Document units. Every tool takes and reports mm and degrees whatever
//! the document's units: the wire is meters, and SolidWorks shows the
//! document's dimensions, tolerances and sheet sizes in its own units. This
//! module is where the difference shows: unit system and drafting standard
//! names, the text written into a document as is (a feature control frame's
//! values, which SolidWorks shows as typed), and whether a document gets
//! inch (ANSI / ASME) or metric (ISO) defaults. See docs/notes/units.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::addin::proto;
use crate::inspect::round;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// Document unit systems (swUnitSystem_e).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UnitSystem {
    /// Millimeter, gram, second.
    Mmgs,
    /// Centimeter, gram, second.
    Cgs,
    /// Meter, kilogram, second.
    Mks,
    /// Inch, pound, second.
    Ips,
}

impl UnitSystem {
    pub fn sw(self) -> i32 {
        match self {
            UnitSystem::Cgs => 1,
            UnitSystem::Mks => 2,
            UnitSystem::Ips => 3,
            UnitSystem::Mmgs => 5,
        }
    }
}

pub fn unit_system_name(sw: i32) -> String {
    match sw {
        1 => "CGS (centimeter, gram, second)".into(),
        2 => "MKS (meter, kilogram, second)".into(),
        3 => "IPS (inch, pound, second)".into(),
        4 => "custom".into(),
        5 => "MMGS (millimeter, gram, second)".into(),
        other => format!("unit system {other}"),
    }
}

/// Drafting standards (swDetailingStandard_e): how dimensions, datums and
/// frames are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DraftingStandard {
    Ansi,
    Iso,
    Din,
    Jis,
    Bsi,
    Gost,
    Gb,
}

impl DraftingStandard {
    fn sw(self) -> i32 {
        match self {
            DraftingStandard::Ansi => 1,
            DraftingStandard::Iso => 2,
            DraftingStandard::Din => 3,
            DraftingStandard::Jis => 4,
            DraftingStandard::Bsi => 5,
            DraftingStandard::Gost => 6,
            DraftingStandard::Gb => 7,
        }
    }
}

fn drafting_standard_name(sw: i32) -> String {
    match sw {
        1 => "ANSI".into(),
        2 => "ISO".into(),
        3 => "DIN".into(),
        4 => "JIS".into(),
        5 => "BSI".into(),
        6 => "GOST".into(),
        7 => "GB".into(),
        8 => "user defined".into(),
        other => format!("standard {other}"),
    }
}

/// A length unit (swLengthUnit_e) as text written into a document uses it.
struct LengthUnit {
    plural: &'static str,
    mm: f64,
    /// Decimals a value written as text keeps.
    decimals: i32,
}

fn length_unit(sw: i32) -> LengthUnit {
    let (plural, mm, decimals) = match sw {
        1 => ("centimeters", 10.0, 5),
        2 => ("meters", 1000.0, 7),
        3 => ("inches", 25.4, 4),
        4 => ("feet", 304.8, 5),
        // Text in a feet-and-inches document is read as inches.
        5 => ("feet and inches", 25.4, 4),
        6 => ("angstroms", 1e-7, 0),
        7 => ("nanometers", 1e-6, 0),
        8 => ("microns", 1e-3, 2),
        9 => ("mils", 0.0254, 2),
        10 => ("microinches", 0.0000254, 0),
        _ => ("millimeters", 1.0, 6),
    };
    LengthUnit { plural, mm, decimals }
}

/// Whether a document shows millimeters (or nothing is known about it).
pub fn is_mm(units: Option<&proto::DocumentUnits>) -> bool {
    units.is_none_or(|u| u.length_unit == 0)
}

/// Inch documents get ANSI / ASME defaults: letter-size sheets, ANSI Inch
/// holes, ASME borders.
pub fn is_inch(units: Option<&proto::DocumentUnits>) -> bool {
    units.is_some_and(|u| u.unit_system == 3 || matches!(u.length_unit, 3 | 4 | 5 | 9 | 10))
}

/// "inches": what lengths in the document are shown in.
pub fn length_name(units: &proto::DocumentUnits) -> &'static str {
    length_unit(units.length_unit).plural
}

/// `mm` as the text a document in `units` reads as that length: 0.05 mm is
/// "0.05" in a mm document, "0.002" in an inch one. None = mm.
pub fn length_text(mm: f64, units: Option<&proto::DocumentUnits>) -> String {
    let unit = length_unit(units.map_or(0, |u| u.length_unit));
    // Shortest form that reads back the same: 0.05, not 0.05000000000000000277.
    format!("{}", round(mm / unit.mm, unit.decimals))
}

/// "IPS (inch, pound, second): lengths in inches, 3 decimals; drafting standard ANSI"
pub fn describe(units: &proto::DocumentUnits) -> String {
    format!(
        "{}: lengths in {}, {} decimals; drafting standard {}",
        unit_system_name(units.unit_system),
        length_name(units),
        units.length_decimals,
        drafting_standard_name(units.drafting_standard)
    )
}

/// An inspect reply's JSON with a note, when the document isn't in mm,
/// that the text read from it (frame tolerances, dimension and note text)
/// is in the document's unit while the reply's numbers are mm.
pub fn note_text_units(mut json: Value, units: Option<&proto::DocumentUnits>) -> Value {
    if let (Some(u), Value::Object(obj)) = (units.filter(|_| !is_mm(units)), &mut json) {
        obj.insert(
            "document_units".into(),
            json!(format!(
                "{}: text read from the document (frame tolerances, dimension and note text) is in {}; the \
                 numbers here (*_mm, positions) are mm",
                describe(u),
                length_name(u)
            )),
        );
    }
    json
}

/// The units of a new document, or a change to the active one's.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct UnitsParams {
    /// Unit system (Tools > Options > Document Properties > Units): "mmgs" (mm), "ips"
    /// (inch), "cgs" (cm) or "mks" (m). Only what SolidWorks shows: tools still take and report
    /// mm. Default: as it is (a new document: its template's).
    #[serde(default)]
    pub units: Option<UnitSystem>,
    /// Decimal places lengths show, 0..8 (e.g. 3 for inch drawings: 1.250). Default: as it is.
    #[serde(default)]
    pub decimals: Option<u32>,
    /// Drafting standard -- how dimensions, datums and frames are drawn: "ansi" (usual for inch
    /// drawings), "iso", "din", "jis", "bsi", "gost", "gb". Default: as it is.
    #[serde(default)]
    pub drafting_standard: Option<DraftingStandard>,
}

impl UnitsParams {
    pub fn is_empty(&self) -> bool {
        self.units.is_none() && self.decimals.is_none() && self.drafting_standard.is_none()
    }
}

/// The UnitsChange for `params` (all unset = no change).
pub fn change(params: &UnitsParams) -> Result<proto::UnitsChange, McpError> {
    if params.decimals.is_some_and(|d| d > 8) {
        return Err(invalid("decimals: 0..8"));
    }
    Ok(proto::UnitsChange {
        unit_system: params.units.map_or(0, UnitSystem::sw),
        length_decimals: params.decimals.map(|d| d as i32),
        drafting_standard: params.drafting_standard.map_or(0, DraftingStandard::sw),
    })
}

/// The document_units reply.
pub fn reply_text(document: &proto::Document, before: Option<&proto::DocumentUnits>) -> String {
    let units = document.units.clone().unwrap_or_default();
    let mut text = match before {
        Some(before) if *before != units => {
            format!("{} units: {} (were {})", document.title, describe(&units), describe(before))
        }
        Some(_) => format!("{} units: {} (unchanged)", document.title, describe(&units)),
        None => format!("{} units: {}", document.title, describe(&units)),
    };
    text += ". Tool parameters and replies stay in mm and degrees; SolidWorks shows the document's \
             dimensions, tolerances and sheet sizes in its units.";
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(system: i32, length: i32) -> proto::DocumentUnits {
        proto::DocumentUnits { unit_system: system, length_unit: length, length_decimals: 2, drafting_standard: 2 }
    }

    #[test]
    fn lengths_are_written_in_the_documents_unit() {
        assert_eq!(length_text(0.05, None), "0.05");
        assert_eq!(length_text(0.05, Some(&units(5, 0))), "0.05");
        assert_eq!(length_text(0.127, Some(&units(3, 3))), "0.005");
        assert_eq!(length_text(0.05, Some(&units(3, 3))), "0.002");
        assert_eq!(length_text(25.4, Some(&units(3, 5))), "1");
        assert_eq!(length_text(0.05, Some(&units(1, 1))), "0.005");
    }

    #[test]
    fn inch_documents_are_told_apart() {
        assert!(is_inch(Some(&units(3, 3))));
        // A custom system showing mils.
        assert!(is_inch(Some(&units(4, 9))));
        assert!(!is_inch(Some(&units(5, 0))) && !is_inch(None));
        assert!(is_mm(None) && is_mm(Some(&units(5, 0))) && !is_mm(Some(&units(3, 3))));
    }

    #[test]
    fn changes_and_replies() {
        let params = UnitsParams { units: Some(UnitSystem::Ips), decimals: Some(3), drafting_standard: Some(DraftingStandard::Ansi) };
        assert_eq!(change(&params).unwrap(), proto::UnitsChange { unit_system: 3, length_decimals: Some(3), drafting_standard: 1 });
        assert_eq!(change(&UnitsParams::default()).unwrap(), proto::UnitsChange::default());
        assert!(change(&UnitsParams { decimals: Some(9), ..Default::default() }).is_err());

        let doc = proto::Document {
            title: "Part1".into(),
            units: Some(proto::DocumentUnits { unit_system: 3, length_unit: 3, length_decimals: 3, drafting_standard: 1 }),
            ..Default::default()
        };
        let text = reply_text(&doc, Some(&units(5, 0)));
        assert!(text.starts_with(
            "Part1 units: IPS (inch, pound, second): lengths in inches, 3 decimals; drafting standard ANSI (were MMGS"
        ), "{text}");
        assert!(reply_text(&doc, doc.units.as_ref()).contains("(unchanged)"));
    }

    #[test]
    fn inspect_replies_say_when_text_isnt_mm() {
        let noted = note_text_units(json!({"document": "Part1"}), Some(&units(3, 3)));
        assert!(noted["document_units"].as_str().unwrap().contains("is in inches; the numbers here"), "{noted}");
        assert_eq!(note_text_units(json!({}), Some(&units(5, 0))), json!({}));
        assert_eq!(note_text_units(json!({}), None), json!({}));
    }
}
