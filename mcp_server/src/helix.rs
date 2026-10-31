//! The create_helix tool: its parameters, which two sizes define the curve, and
//! the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{MM_PER_M, mm, point_mm, round};

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct HelixParams {
    /// Feature-tree name of a sketch holding one circle, e.g. "Sketch1" (sketch_circle): the
    /// circle sets the helix's diameter and where it starts, its plane the axis. Omit to use
    /// the most recently created sketch that no feature uses yet.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// Distance between turns, in mm. A helix takes exactly two of pitch_mm, height_mm and
    /// revolutions; with spiral=true, this is how much the radius grows per turn.
    #[serde(default)]
    pub pitch_mm: Option<f64>,
    /// Total height along the axis, in mm.
    #[serde(default)]
    pub height_mm: Option<f64>,
    /// Number of turns; need not be whole.
    #[serde(default)]
    pub revolutions: Option<f64>,
    /// Make a flat spiral in the sketch's plane instead, from pitch_mm and revolutions. Defaults
    /// to false.
    #[serde(default)]
    pub spiral: bool,
    /// Run to the other side of the sketch plane (a helix goes along the plane's normal by
    /// default). Defaults to false.
    #[serde(default)]
    pub reverse_direction: bool,
    /// Wind clockwise instead of counterclockwise. Defaults to false.
    #[serde(default)]
    pub clockwise: bool,
    /// Where on the circle the curve starts, in degrees. Defaults to 0. The reply says where
    /// that puts the start.
    #[serde(default)]
    pub start_angle_deg: Option<f64>,
    /// Taper the helix into a cone by this many degrees (more than 0, less than 90); it narrows
    /// along its length unless taper_outward is set. Not with a spiral.
    #[serde(default)]
    pub taper_deg: Option<f64>,
    /// With taper_deg: widen along the length instead. Defaults to false.
    #[serde(default)]
    pub taper_outward: bool,
}

/// More turns than anyone means.
const MAX_REVOLUTIONS: f64 = 10_000.0;

fn positive(value: Option<f64>, what: &str) -> Result<Option<f64>, McpError> {
    match value {
        Some(v) if !(v.is_finite() && v > 0.0) => Err(invalid(format!("{what} must be more than 0, got {v}"))),
        other => Ok(other),
    }
}

pub fn request(params: &HelixParams) -> Result<proto::CreateHelixRequest, McpError> {
    use proto::HelixDefinition as D;
    let pitch = positive(params.pitch_mm, "pitch_mm")?;
    let height = positive(params.height_mm, "height_mm")?;
    let revolutions = positive(params.revolutions, "revolutions")?;
    if revolutions.is_some_and(|r| r > MAX_REVOLUTIONS) {
        return Err(invalid(format!("revolutions must be at most {MAX_REVOLUTIONS}")));
    }
    let definition = match (params.spiral, pitch, height, revolutions) {
        (true, Some(_), None, Some(_)) => D::Spiral,
        (true, ..) => return Err(invalid("a spiral takes pitch_mm and revolutions, and no height_mm")),
        (false, Some(_), None, Some(_)) => D::PitchAndRevolutions,
        (false, None, Some(_), Some(_)) => D::HeightAndRevolutions,
        (false, Some(_), Some(_), None) => D::HeightAndPitch,
        _ => return Err(invalid("give exactly two of pitch_mm, height_mm and revolutions")),
    };
    let start_angle = params.start_angle_deg.unwrap_or(0.0);
    if !start_angle.is_finite() {
        return Err(invalid("start_angle_deg must be a number"));
    }
    let taper = match params.taper_deg {
        None => 0.0,
        Some(_) if params.spiral => return Err(invalid("a spiral can't be tapered -- drop taper_deg")),
        Some(t) if !(t.is_finite() && t > 0.0 && t < 90.0) => {
            return Err(invalid(format!("taper_deg must be between 0 and 90, got {t}")));
        }
        Some(t) => t.to_radians(),
    };
    if params.taper_outward && taper == 0.0 {
        return Err(invalid("taper_outward needs taper_deg"));
    }
    Ok(proto::CreateHelixRequest {
        sketch_name: params.sketch_name.as_deref().map(str::trim).unwrap_or_default().to_string(),
        definition: definition.into(),
        height: height.unwrap_or(0.0) / MM_PER_M,
        pitch: pitch.unwrap_or(0.0) / MM_PER_M,
        revolutions: revolutions.unwrap_or(0.0),
        reverse_direction: params.reverse_direction,
        clockwise: params.clockwise,
        start_angle: start_angle.to_radians(),
        taper_angle: taper,
        taper_outward: params.taper_outward,
    })
}

/// "created Helix/Spiral1 from Sketch1: 4 turns, pitch 5 mm, height 20 mm, diameter 20 mm, around
/// [0.0, 0.0, 0.0] mm; it runs from [10.0, 0.0, 0.0] mm to [10.0, 0.0, 20.0] mm, 252 mm long. ..."
pub fn reply_text(params: &HelixParams, reply: &proto::CreateHelixResponse) -> String {
    let what = if params.spiral { "spiral" } else { "helix" };
    let mut text = format!(
        "created {} ({what}) from {}: {} turns, pitch {} mm",
        reply.feature_name,
        reply.sketch_name,
        round(reply.revolutions, 4),
        mm(reply.pitch),
    );
    if !params.spiral {
        text += &format!(", height {} mm", mm(reply.height));
    }
    text += &format!(", starting diameter {} mm", mm(reply.radius * 2.0));
    if let Some(center) = &reply.center {
        text += &format!(", around {} mm", point_mm(center));
    }
    if let (Some(start), Some(end)) = (&reply.start, &reply.end) {
        text += &format!("; it runs from {} mm to {} mm", point_mm(start), point_mm(end));
    }
    if reply.length > 0.0 {
        text += &format!(", {} mm long", mm(reply.length));
    }
    text += &format!(
        ". To make it solid: sweep_sketch path=\"{}\" with circle_diameter_mm for a round wire, or a \
         profile sketch through the start point on a plane containing the axis.",
        reply.feature_name
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::HelixDefinition as D;

    fn params(pitch: Option<f64>, height: Option<f64>, revolutions: Option<f64>) -> HelixParams {
        HelixParams { pitch_mm: pitch, height_mm: height, revolutions, ..Default::default() }
    }

    #[test]
    fn any_two_sizes_define_it() {
        let r = request(&params(Some(5.0), None, Some(4.0))).unwrap();
        assert_eq!(r.definition(), D::PitchAndRevolutions);
        assert!((r.pitch - 0.005).abs() < 1e-15 && r.revolutions == 4.0 && r.height == 0.0);
        assert_eq!(request(&params(None, Some(20.0), Some(4.0))).unwrap().definition(), D::HeightAndRevolutions);
        assert_eq!(request(&params(Some(5.0), Some(20.0), None)).unwrap().definition(), D::HeightAndPitch);
    }

    #[test]
    fn not_one_or_three_sizes() {
        for p in [
            params(None, None, None),
            params(Some(5.0), None, None),
            params(Some(5.0), Some(20.0), Some(4.0)),
        ] {
            let err = request(&p).unwrap_err();
            assert!(err.message.contains("exactly two"), "{}", err.message);
        }
    }

    #[test]
    fn sizes_must_be_positive() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(request(&params(Some(bad), None, Some(4.0))).is_err(), "{bad}");
            assert!(request(&params(Some(5.0), None, Some(bad))).is_err(), "{bad}");
        }
        assert!(request(&params(Some(5.0), None, Some(MAX_REVOLUTIONS + 1.0))).is_err());
    }

    #[test]
    fn spiral_takes_pitch_and_turns_only() {
        let mut p = params(Some(4.0), None, Some(3.0));
        p.spiral = true;
        assert_eq!(request(&p).unwrap().definition(), D::Spiral);
        p.height_mm = Some(10.0);
        assert!(request(&p).is_err());
        p.height_mm = None;
        p.taper_deg = Some(5.0);
        assert!(request(&p).unwrap_err().message.contains("tapered"));
    }

    #[test]
    fn taper_and_start_angle_in_radians() {
        let mut p = params(Some(5.0), None, Some(4.0));
        p.taper_deg = Some(45.0);
        p.start_angle_deg = Some(90.0);
        let r = request(&p).unwrap();
        assert!((r.taper_angle - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
        assert!((r.start_angle - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        for bad in [0.0, 90.0, -5.0, f64::NAN] {
            p.taper_deg = Some(bad);
            assert!(request(&p).is_err(), "{bad}");
        }
        p.taper_deg = None;
        p.taper_outward = true;
        assert!(request(&p).is_err());
    }

    #[test]
    fn reply_says_where_it_runs() {
        let v = |x: f64, y: f64, z: f64| Some(proto::Vec3 { x, y, z });
        let reply = proto::CreateHelixResponse {
            feature_name: "Helix/Spiral1".into(),
            sketch_name: "Sketch1".into(),
            height: 0.02,
            pitch: 0.005,
            revolutions: 4.0,
            center: v(0.0, 0.0, 0.0),
            radius: 0.01,
            start: v(0.01, 0.0, 0.0),
            end: v(0.01, 0.0, 0.02),
            length: 0.25,
        };
        let text = reply_text(&params(Some(5.0), None, Some(4.0)), &reply);
        assert!(text.starts_with("created Helix/Spiral1 (helix) from Sketch1: 4 turns, pitch 5 mm, height 20 mm"), "{text}");
        assert!(text.contains("from [10.0,0.0,0.0] mm to [10.0,0.0,20.0] mm, 250 mm long"), "{text}");
        assert!(text.contains("path=\"Helix/Spiral1\""), "{text}");
    }
}
