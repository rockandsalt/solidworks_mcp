//! The sweep tools (sweep_sketch, cut_sweep_sketch): their parameters and the
//! reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{Kind, MM_PER_M};

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SweepParams {
    /// Feature-tree name of the path to sweep along, e.g. "Sketch1": a sketch holding one open
    /// or closed chain of lines, arcs and splines (a 3D sketch or a helix works too). The
    /// profile starts where it crosses the path, usually at the path's start.
    pub path: String,
    /// Feature-tree name of the profile sketch, e.g. "Sketch2": one closed contour, usually on
    /// a plane across the start of the path. Omit to use the most recently created sketch that
    /// no feature uses yet, other than the path. Leave out with circle_diameter_mm.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// Sweep a circle of this diameter (mm), centered on the path, instead of a profile sketch:
    /// a rod, wire, spring or pipe along the path in one step.
    #[serde(default)]
    pub circle_diameter_mm: Option<f64>,
    /// Twist the profile by this many degrees from the start of the path to its end (negative
    /// for the other way). Not with circle_diameter_mm. Defaults to 0: the profile follows the
    /// path without turning.
    #[serde(default)]
    pub twist_deg: Option<f64>,
}

/// More turns than anyone means.
const MAX_TWIST_DEG: f64 = 36_000.0;

pub fn request(params: &SweepParams, cut: bool) -> Result<proto::SweepSketchRequest, McpError> {
    let path = params.path.trim();
    if path.is_empty() {
        return Err(invalid("path is empty -- give the feature-tree name of the path sketch, e.g. \"Sketch1\""));
    }
    if Kind::of_id(path).is_some() {
        return Err(invalid(format!(
            "path: give the feature-tree name of the path (e.g. \"Sketch1\"), not an id like {path}"
        )));
    }
    let sketch_name = params.sketch_name.as_deref().map(str::trim).unwrap_or_default();
    let circle_diameter = match params.circle_diameter_mm {
        None => 0.0,
        Some(d) if !(d.is_finite() && d > 0.0) => {
            return Err(invalid(format!("circle_diameter_mm must be more than 0, got {d}")));
        }
        Some(_) if !sketch_name.is_empty() => {
            return Err(invalid("give sketch_name (a profile sketch) or circle_diameter_mm, not both"));
        }
        Some(d) => d / MM_PER_M,
    };
    let twist_deg = params.twist_deg.unwrap_or(0.0);
    if !(twist_deg.is_finite() && twist_deg.abs() <= MAX_TWIST_DEG) {
        return Err(invalid(format!("twist_deg must be within +-{MAX_TWIST_DEG}, got {twist_deg}")));
    }
    if twist_deg != 0.0 && circle_diameter > 0.0 {
        return Err(invalid("a circular profile can't be twisted -- drop twist_deg or sweep a profile sketch"));
    }
    Ok(proto::SweepSketchRequest {
        sketch_name: sketch_name.to_string(),
        path: path.to_string(),
        circle_diameter,
        twist_angle: twist_deg.to_radians(),
        cut,
    })
}

/// "created Sweep1 by sweeping Sketch2 along Sketch1 with a 90 degree twist."
pub fn reply_text(params: &SweepParams, reply: &proto::SweepSketchResponse, cut: bool) -> String {
    let profile = match params.circle_diameter_mm {
        Some(d) => format!("a {d} mm circle"),
        None => reply.sketch_name.clone(),
    };
    let twist = match params.twist_deg {
        Some(t) if t != 0.0 => format!(" with a {t} degree twist"),
        _ => String::new(),
    };
    format!(
        "created {} by {} {profile} along {}{twist}.",
        reply.feature_name,
        if cut { "cutting with" } else { "sweeping" },
        reply.path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(path: &str) -> SweepParams {
        SweepParams { path: path.into(), sketch_name: None, circle_diameter_mm: None, twist_deg: None }
    }

    #[test]
    fn profile_sketch_by_default() {
        let r = request(&params(" Sketch1 "), false).unwrap();
        assert_eq!(r.path, "Sketch1");
        assert!(r.sketch_name.is_empty() && r.circle_diameter == 0.0 && r.twist_angle == 0.0 && !r.cut);
    }

    #[test]
    fn path_is_a_name() {
        assert!(request(&params(""), false).is_err());
        let err = request(&params("feature:3"), false).unwrap_err();
        assert!(err.message.contains("feature-tree name"), "{}", err.message);
    }

    #[test]
    fn circle_in_meters_and_alone() {
        let mut p = params("Sketch1");
        p.circle_diameter_mm = Some(5.0);
        let r = request(&p, true).unwrap();
        assert!((r.circle_diameter - 0.005).abs() < 1e-15 && r.cut);
        for bad in [0.0, -1.0, f64::NAN] {
            p.circle_diameter_mm = Some(bad);
            assert!(request(&p, false).is_err(), "{bad}");
        }
        p.circle_diameter_mm = Some(5.0);
        p.sketch_name = Some("Sketch2".into());
        assert!(request(&p, false).is_err());
        p.sketch_name = None;
        p.twist_deg = Some(90.0);
        assert!(request(&p, false).is_err());
    }

    #[test]
    fn twist_in_radians() {
        let mut p = params("Sketch1");
        p.twist_deg = Some(-90.0);
        let r = request(&p, false).unwrap();
        assert!((r.twist_angle + std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        p.twist_deg = Some(f64::INFINITY);
        assert!(request(&p, false).is_err());
    }

    #[test]
    fn reply_names_profile_path_and_twist() {
        let reply = proto::SweepSketchResponse {
            feature_name: "Sweep1".into(),
            sketch_name: "Sketch2".into(),
            path: "Sketch1".into(),
            ..Default::default()
        };
        let mut p = params("Sketch1");
        p.twist_deg = Some(90.0);
        assert_eq!(reply_text(&p, &reply, false), "created Sweep1 by sweeping Sketch2 along Sketch1 with a 90 degree twist.");
        let reply = proto::SweepSketchResponse { feature_name: "Cut-Sweep1".into(), sketch_name: String::new(), ..reply };
        let mut p = params("Sketch1");
        p.circle_diameter_mm = Some(4.0);
        assert_eq!(reply_text(&p, &reply, true), "created Cut-Sweep1 by cutting with a 4 mm circle along Sketch1.");
    }
}
