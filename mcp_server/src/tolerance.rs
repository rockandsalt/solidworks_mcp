//! The set_dimension_tolerance tool: params, the wire request built from
//! them (deviations in meters / radians, swTolType_e) and the reply.
//!
//! Works on any dimension the add-in can find by full name: sketch and
//! feature dimensions of a part, mate dimensions of an assembly, and in a
//! drawing the view dimensions inspect_drawing lists (reference "RD1@Drawing
//! View1" or the model's own "D1@Boss-Extrude1@block.Part").

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::addin::proto;
use crate::pmi;
use crate::sketch;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// The kinds of tolerance, named as inspect_annotations / inspect_drawing report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToleranceKind {
    /// No tolerance (removes one).
    None,
    /// Basic dimension (shown boxed, toleranced by a feature control frame).
    Basic,
    /// Nominal with an upper and lower deviation, e.g. 40 +0.1/-0.05 (upper and lower).
    Bilateral,
    /// The two limits stacked, e.g. 40.1 / 39.95 (given as upper and lower deviations).
    Limit,
    /// Nominal +/- one value (plus_minus).
    Symmetric,
    /// Shown as a minimum ("min").
    Min,
    /// Shown as a maximum ("max").
    Max,
    /// ISO fit class, e.g. H7 (hole_fit and/or shaft_fit).
    Fit,
    /// Fit class with its deviations from the fit table.
    FitWithTolerance,
    /// Only the fit's deviations from the fit table.
    FitToleranceOnly,
}

impl ToleranceKind {
    /// swTolType_e.
    fn code(self) -> i32 {
        match self {
            ToleranceKind::None => 0,
            ToleranceKind::Basic => 1,
            ToleranceKind::Bilateral => 2,
            ToleranceKind::Limit => 3,
            ToleranceKind::Symmetric => 4,
            ToleranceKind::Min => 5,
            ToleranceKind::Max => 6,
            ToleranceKind::Fit => 7,
            ToleranceKind::FitWithTolerance => 8,
            ToleranceKind::FitToleranceOnly => 9,
        }
    }

    fn name(self) -> &'static str {
        match self {
            ToleranceKind::None => "none",
            ToleranceKind::Basic => "basic",
            ToleranceKind::Bilateral => "bilateral",
            ToleranceKind::Limit => "limit",
            ToleranceKind::Symmetric => "symmetric",
            ToleranceKind::Min => "min",
            ToleranceKind::Max => "max",
            ToleranceKind::Fit => "fit",
            ToleranceKind::FitWithTolerance => "fit_with_tolerance",
            ToleranceKind::FitToleranceOnly => "fit_tolerance_only",
        }
    }

    fn uses_fits(self) -> bool {
        matches!(self, ToleranceKind::Fit | ToleranceKind::FitWithTolerance | ToleranceKind::FitToleranceOnly)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetToleranceParams {
    /// The dimension's name. Drawings: the `dimension` field inspect_drawing lists, e.g.
    /// "RD1@Drawing View1@Draw1.Drawing" or "D1@Boss-Extrude1@block.Part"; the last "@..."
    /// part may be left off ("RD1@Drawing View1") when that's unambiguous. Parts: e.g.
    /// "D1@Sketch1", "D1@Boss-Extrude1"; MBD dimensions as inspect_annotations lists them.
    pub name: String,
    /// The kind of tolerance: "none", "basic", "bilateral", "limit", "symmetric", "min",
    /// "max", "fit", "fit_with_tolerance" or "fit_tolerance_only".
    pub tolerance: ToleranceKind,
    /// Bilateral / limit: upper deviation from the nominal value, signed, in mm (lengths,
    /// diameters, radii), e.g. 0.1.
    #[serde(default)]
    pub upper_mm: Option<f64>,
    /// Bilateral / limit: lower deviation, signed, in mm, e.g. -0.05 for 40 +0.1/-0.05.
    #[serde(default)]
    pub lower_mm: Option<f64>,
    /// Symmetric: the +/- value in mm, e.g. 0.05 for 40 +/-0.05.
    #[serde(default)]
    pub plus_minus_mm: Option<f64>,
    /// As upper_mm, in degrees, for angle dimensions.
    #[serde(default)]
    pub upper_deg: Option<f64>,
    /// As lower_mm, in degrees, for angle dimensions.
    #[serde(default)]
    pub lower_deg: Option<f64>,
    /// As plus_minus_mm, in degrees, for angle dimensions.
    #[serde(default)]
    pub plus_minus_deg: Option<f64>,
    /// Fit types: the hole's ISO fit class, e.g. "H7".
    #[serde(default)]
    pub hole_fit: Option<String>,
    /// Fit types: the shaft's ISO fit class, e.g. "g6".
    #[serde(default)]
    pub shaft_fit: Option<String>,
    /// Decimal places shown for the value, 0..8. Unchanged if not given.
    #[serde(default)]
    pub decimals: Option<u8>,
    /// Decimal places shown for the tolerance values, 0..8. Unchanged if not given.
    #[serde(default)]
    pub tolerance_decimals: Option<u8>,
}

/// An ISO fit class: one or two letters and a grade, e.g. "H7", "js6", "ZC11".
fn fit_class(value: &Option<String>, which: &str) -> Result<String, McpError> {
    let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(String::new());
    };
    let letters = v.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let digits = v[letters..].chars().all(|c| c.is_ascii_digit());
    if !(1..=2).contains(&letters) || !(1..=2).contains(&(v.len() - letters)) || !digits {
        return Err(invalid(format!(
            "{which} {v:?} isn't an ISO fit class -- e.g. \"H7\" (hole) or \"g6\" (shaft)"
        )));
    }
    Ok(v.to_string())
}

fn decimals(value: Option<u8>, which: &str) -> Result<Option<i32>, McpError> {
    match value {
        Some(d) if d > 8 => Err(invalid(format!("{which} must be 0..8, got {d}"))),
        d => Ok(d.map(i32::from)),
    }
}

pub fn request(params: &SetToleranceParams) -> Result<proto::SetDimensionToleranceRequest, McpError> {
    let name = params.name.trim();
    if name.is_empty() {
        return Err(invalid(
            "give the dimension's full name, e.g. \"RD1@Drawing View1\" (inspect_drawing lists them) or \
             \"D1@Sketch1\"",
        ));
    }
    let kind = params.tolerance;
    let mm_given = [params.upper_mm, params.lower_mm, params.plus_minus_mm].iter().any(Option::is_some);
    let deg_given = [params.upper_deg, params.lower_deg, params.plus_minus_deg].iter().any(Option::is_some);
    if mm_given && deg_given {
        return Err(invalid("give the deviations in mm (lengths) or in degrees (angles), not both"));
    }
    let angle = deg_given;
    let (upper, lower, plus_minus) = if angle {
        (params.upper_deg, params.lower_deg, params.plus_minus_deg)
    } else {
        (params.upper_mm, params.lower_mm, params.plus_minus_mm)
    };
    let unit = if angle { "deg" } else { "mm" };
    let to_wire = |v: f64| if angle { v.to_radians() } else { sketch::m(v) };
    for v in [upper, lower, plus_minus].into_iter().flatten() {
        if !v.is_finite() {
            return Err(invalid(format!("deviations must be numbers, got {v}")));
        }
    }

    let fits_given = params.hole_fit.is_some() || params.shaft_fit.is_some();
    if fits_given && !kind.uses_fits() {
        return Err(invalid(
            "hole_fit / shaft_fit only go with tolerance \"fit\", \"fit_with_tolerance\" or \"fit_tolerance_only\"",
        ));
    }

    let (wire_upper, wire_lower) = match kind {
        ToleranceKind::Bilateral | ToleranceKind::Limit => {
            if plus_minus.is_some() {
                return Err(invalid(format!(
                    "plus_minus_{unit} is for tolerance \"symmetric\"; give upper_{unit} and lower_{unit} \
                     (signed deviations) for {}",
                    kind.name()
                )));
            }
            let (Some(u), Some(l)) = (upper, lower) else {
                return Err(invalid(format!(
                    "a {} tolerance needs upper_{unit} and lower_{unit}: signed deviations from the nominal \
                     value, e.g. upper 0.1 and lower -0.05 for 40 +0.1/-0.05",
                    kind.name()
                )));
            };
            if u < l {
                return Err(invalid(format!(
                    "upper_{unit} ({u}) is below lower_{unit} ({l}) -- both are signed: lower is usually \
                     negative, e.g. -0.05"
                )));
            }
            (to_wire(u), to_wire(l))
        }
        ToleranceKind::Symmetric => {
            if upper.is_some() || lower.is_some() {
                return Err(invalid(format!(
                    "a symmetric tolerance takes plus_minus_{unit} only (use \"bilateral\" for different \
                     upper and lower deviations)"
                )));
            }
            match plus_minus {
                Some(v) if v > 0.0 => (to_wire(v), -to_wire(v)),
                Some(v) => return Err(invalid(format!("plus_minus_{unit} must be positive, got {v}"))),
                None => {
                    return Err(invalid(format!(
                        "a symmetric tolerance needs plus_minus_{unit}, e.g. 0.05 for +/-0.05"
                    )));
                }
            }
        }
        _ => {
            if mm_given || deg_given {
                return Err(invalid(format!(
                    "tolerance \"{}\" takes no deviations -- they go with \"bilateral\", \"limit\" (upper/lower) \
                     or \"symmetric\" (plus_minus)",
                    kind.name()
                )));
            }
            (0.0, 0.0)
        }
    };

    let hole_fit = fit_class(&params.hole_fit, "hole_fit")?;
    let shaft_fit = fit_class(&params.shaft_fit, "shaft_fit")?;
    if kind.uses_fits() && hole_fit.is_empty() && shaft_fit.is_empty() {
        return Err(invalid("a fit tolerance needs hole_fit (e.g. \"H7\") and/or shaft_fit (e.g. \"g6\")"));
    }

    Ok(proto::SetDimensionToleranceRequest {
        name: name.to_string(),
        tolerance_type: kind.code(),
        upper: wire_upper,
        lower: wire_lower,
        angle,
        hole_fit,
        shaft_fit,
        value_precision: decimals(params.decimals, "decimals")?,
        tolerance_precision: decimals(params.tolerance_decimals, "tolerance_decimals")?,
    })
}

/// The dimension afterwards, as inspect_drawing shows dimensions, plus what
/// SolidWorks didn't take as asked.
pub fn reply_json(request: &proto::SetDimensionToleranceRequest, reply: &proto::SetDimensionToleranceResponse) -> Value {
    let d = reply.dimension.clone().unwrap_or_default();
    let mut obj = pmi::dimension_fields(&d);
    if !reply.view_name.is_empty() {
        obj.insert("view".into(), json!(reply.view_name));
    }
    if !reply.other_views.is_empty() {
        obj.insert("also_in_views".into(), json!(reply.other_views));
    }
    if reply.value_precision >= 0 {
        obj.insert("decimals".into(), json!(reply.value_precision));
    }
    if reply.tolerance_precision >= 0 {
        obj.insert("tolerance_decimals".into(), json!(reply.tolerance_precision));
    }

    let mut notes = Vec::new();
    if d.tolerance_type != request.tolerance_type {
        notes.push(format!(
            "SolidWorks reports tolerance type {} instead of the requested {}",
            d.tolerance_type, request.tolerance_type
        ));
    }
    // Deviations are read back to 1 nm / 1 µrad.
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    if matches!(request.tolerance_type, 2..=4)
        && !(close(d.tolerance_max, request.upper) && close(d.tolerance_min, request.lower))
    {
        notes.push("SolidWorks reports other deviations than the ones given (see tolerance)".to_string());
    }
    // A drawing's own (reference) dimensions belong to their view.
    if !reply.view_name.is_empty() && !d.owner_feature.is_empty() && d.owner_feature != reply.view_name {
        notes.push(format!(
            "this is the model's own dimension (of {}), so the model shows the new tolerance too",
            d.owner_feature
        ));
    }
    if !notes.is_empty() {
        obj.insert("notes".into(), json!(notes));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(tolerance: ToleranceKind) -> SetToleranceParams {
        SetToleranceParams {
            name: "RD1@Drawing View1".into(),
            tolerance,
            upper_mm: None,
            lower_mm: None,
            plus_minus_mm: None,
            upper_deg: None,
            lower_deg: None,
            plus_minus_deg: None,
            hole_fit: None,
            shaft_fit: None,
            decimals: None,
            tolerance_decimals: None,
        }
    }

    #[test]
    fn bilateral_converts_signed_deviations_to_meters() {
        let p = SetToleranceParams { upper_mm: Some(0.1), lower_mm: Some(-0.05), ..params(ToleranceKind::Bilateral) };
        let r = request(&p).unwrap();
        assert_eq!(r.tolerance_type, 2);
        assert!((r.upper - 1e-4).abs() < 1e-15 && (r.lower + 5e-5).abs() < 1e-15, "{r:?}");
        assert!(!r.angle);
        assert_eq!((r.value_precision, r.tolerance_precision), (None, None));
        // Both needed, in order, no plus_minus.
        assert!(request(&SetToleranceParams { upper_mm: Some(0.1), ..params(ToleranceKind::Bilateral) }).is_err());
        let swapped = SetToleranceParams { upper_mm: Some(-0.1), lower_mm: Some(0.1), ..params(ToleranceKind::Limit) };
        assert!(request(&swapped).is_err());
        let pm = SetToleranceParams {
            upper_mm: Some(0.1),
            lower_mm: Some(0.0),
            plus_minus_mm: Some(0.1),
            ..params(ToleranceKind::Bilateral)
        };
        assert!(request(&pm).is_err());
    }

    #[test]
    fn symmetric_and_angles() {
        let r = request(&SetToleranceParams { plus_minus_mm: Some(0.05), ..params(ToleranceKind::Symmetric) }).unwrap();
        assert_eq!(r.tolerance_type, 4);
        assert!((r.upper - 5e-5).abs() < 1e-15 && (r.lower + 5e-5).abs() < 1e-15);
        let r = request(&SetToleranceParams { plus_minus_deg: Some(0.5), ..params(ToleranceKind::Symmetric) }).unwrap();
        assert!(r.angle && (r.upper - 0.5f64.to_radians()).abs() < 1e-15);
        assert!(request(&SetToleranceParams { plus_minus_mm: Some(-1.0), ..params(ToleranceKind::Symmetric) }).is_err());
        assert!(request(&params(ToleranceKind::Symmetric)).is_err());
        let mixed = SetToleranceParams { upper_mm: Some(0.1), lower_deg: Some(-0.1), ..params(ToleranceKind::Bilateral) };
        assert!(request(&mixed).is_err());
    }

    #[test]
    fn fits_and_value_free_kinds() {
        let fit = SetToleranceParams {
            hole_fit: Some(" H7 ".into()),
            shaft_fit: Some("g6".into()),
            ..params(ToleranceKind::FitWithTolerance)
        };
        let r = request(&fit).unwrap();
        assert_eq!((r.tolerance_type, r.hole_fit.as_str(), r.shaft_fit.as_str()), (8, "H7", "g6"));
        assert!(request(&params(ToleranceKind::Fit)).is_err());
        assert!(request(&SetToleranceParams { hole_fit: Some("7H".into()), ..params(ToleranceKind::Fit) }).is_err());
        assert!(request(&SetToleranceParams { hole_fit: Some("H7".into()), ..params(ToleranceKind::Basic) }).is_err());

        assert_eq!(request(&params(ToleranceKind::None)).unwrap().tolerance_type, 0);
        assert!(request(&SetToleranceParams { upper_mm: Some(0.1), ..params(ToleranceKind::Max) }).is_err());
        let dec = SetToleranceParams { decimals: Some(2), tolerance_decimals: Some(3), ..params(ToleranceKind::Basic) };
        let r = request(&dec).unwrap();
        assert_eq!((r.value_precision, r.tolerance_precision), (Some(2), Some(3)));
        assert!(request(&SetToleranceParams { decimals: Some(9), ..params(ToleranceKind::Basic) }).is_err());
        assert!(request(&SetToleranceParams { name: " ".into(), ..params(ToleranceKind::Basic) }).is_err());
    }
}
