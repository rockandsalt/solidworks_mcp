//! The rib tool (rib_sketch): its parameters and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{Kind, MM_PER_M};

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RibSide {
    /// Centered on the sketch line.
    #[default]
    Both,
    /// All on one side of the line.
    One,
    /// All on the other side of the line.
    Other,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RibParams {
    /// The rib's thickness in mm, more than 0.
    pub thickness_mm: f64,
    /// Feature-tree name of the sketch, e.g. "Sketch2": usually one open line or chain, drawn
    /// where the rib's free edge should be. Omit to use the most recently created sketch that
    /// no feature uses yet.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// Which side of the sketch line the thickness goes. Defaults to both (centered).
    #[serde(default)]
    pub side: RibSide,
    /// Grow the rib along the sketch's normal, out of its plane, instead of within the sketch
    /// plane: for a sketch on a plane above a floor, the rib's outline seen from above. Defaults
    /// to false: a sketch on a plane across the part, its line being the rib's edge seen from
    /// the side.
    #[serde(default)]
    pub normal_to_sketch: bool,
    /// Grow the rib the other way. Defaults to false: the default way, and if that makes
    /// nothing, the other way, which the reply says. Use when both ways reach material and the
    /// reply's added volume shows the rib went to the wrong one.
    #[serde(default)]
    pub reverse_direction: bool,
    /// Taper the rib's sides by this many degrees, 0 to under 90, as for a molded part: it
    /// gets thinner away from the sketch. Defaults to no taper.
    #[serde(default)]
    pub draft_deg: Option<f64>,
    /// With draft_deg: get thicker away from the sketch instead.
    #[serde(default)]
    pub draft_outward: bool,
}

pub fn request(params: &RibParams) -> Result<proto::RibSketchRequest, McpError> {
    if !(params.thickness_mm.is_finite() && params.thickness_mm > 0.0) {
        return Err(invalid(format!("thickness_mm must be more than 0, got {}", params.thickness_mm)));
    }
    let draft_deg = params.draft_deg.unwrap_or(0.0);
    if !(draft_deg.is_finite() && (0.0..90.0).contains(&draft_deg)) {
        return Err(invalid(format!("draft_deg must be from 0 to under 90, got {draft_deg}")));
    }
    if params.draft_outward && draft_deg == 0.0 {
        return Err(invalid("draft_outward needs draft_deg"));
    }
    let sketch_name = params.sketch_name.as_deref().map(str::trim).unwrap_or_default();
    if Kind::of_id(sketch_name).is_some() {
        return Err(invalid(format!(
            "sketch_name: give the feature-tree name of the sketch (e.g. \"Sketch1\"), not an id like {sketch_name}"
        )));
    }
    Ok(proto::RibSketchRequest {
        sketch_name: sketch_name.to_string(),
        thickness: params.thickness_mm / MM_PER_M,
        one_sided: params.side != RibSide::Both,
        flip_side: params.side == RibSide::Other,
        normal_to_sketch: params.normal_to_sketch,
        reverse_direction: params.reverse_direction,
        draft_angle: draft_deg.to_radians(),
        draft_outward: params.draft_outward,
    })
}

/// "created Rib1 from Sketch2: 2 mm thick, adding 400 mm^3. ..."
pub fn reply_text(params: &RibParams, reply: &proto::RibSketchResponse) -> String {
    let side = match params.side {
        RibSide::Both => "",
        RibSide::One => " on one side of the sketch",
        RibSide::Other => " on the other side of the sketch",
    };
    let flipped = if reply.direction_flipped {
        " It grew the reversed way: the default way made nothing."
    } else {
        ""
    };
    format!(
        "created {} from {}: {} mm thick{side}, adding {:.3} mm^3.{flipped} The body's faces and edges have \
         changed: inspect_model again before using their ids.",
        reply.feature_name,
        reply.sketch_name,
        params.thickness_mm,
        reply.volume_change * MM_PER_M.powi(3),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(thickness_mm: f64) -> RibParams {
        RibParams {
            thickness_mm,
            sketch_name: None,
            side: RibSide::Both,
            normal_to_sketch: false,
            reverse_direction: false,
            draft_deg: None,
            draft_outward: false,
        }
    }

    #[test]
    fn defaults_in_meters() {
        let r = request(&params(2.5)).unwrap();
        assert!((r.thickness - 0.0025).abs() < 1e-15);
        assert!(r.sketch_name.is_empty() && !r.one_sided && !r.flip_side && !r.normal_to_sketch);
        assert!(!r.reverse_direction && r.draft_angle == 0.0 && !r.draft_outward);
    }

    #[test]
    fn sides() {
        let mut p = params(1.0);
        p.side = RibSide::One;
        let r = request(&p).unwrap();
        assert!(r.one_sided && !r.flip_side);
        p.side = RibSide::Other;
        let r = request(&p).unwrap();
        assert!(r.one_sided && r.flip_side);
    }

    #[test]
    fn refuses_bad_sizes() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(request(&params(bad)).is_err(), "{bad}");
        }
        for bad in [-1.0, 90.0, f64::NAN] {
            let mut p = params(1.0);
            p.draft_deg = Some(bad);
            assert!(request(&p).is_err(), "{bad}");
        }
    }

    #[test]
    fn draft_in_radians_and_outward_needs_it() {
        let mut p = params(1.0);
        p.draft_outward = true;
        assert!(request(&p).unwrap_err().message.contains("needs draft_deg"));
        p.draft_deg = Some(90.0 / 2.0);
        let r = request(&p).unwrap();
        assert!((r.draft_angle - std::f64::consts::FRAC_PI_4).abs() < 1e-12 && r.draft_outward);
    }

    #[test]
    fn sketch_is_a_name() {
        let mut p = params(1.0);
        p.sketch_name = Some(" Sketch2 ".into());
        assert_eq!(request(&p).unwrap().sketch_name, "Sketch2");
        p.sketch_name = Some("line:3".into());
        assert!(request(&p).unwrap_err().message.contains("feature-tree name"));
    }

    #[test]
    fn replies() {
        let reply = proto::RibSketchResponse {
            feature_name: "Rib1".into(),
            sketch_name: "Sketch2".into(),
            volume_change: 4e-7,
            ..Default::default()
        };
        let text = reply_text(&params(2.0), &reply);
        assert!(text.starts_with("created Rib1 from Sketch2: 2 mm thick, adding 400.000 mm^3."), "{text}");
        assert!(!text.contains("reversed"), "{text}");
        let reply = proto::RibSketchResponse { direction_flipped: true, ..reply };
        let mut p = params(2.0);
        p.side = RibSide::Other;
        let text = reply_text(&p, &reply);
        assert!(text.contains("on the other side of the sketch") && text.contains("reversed"), "{text}");
    }
}
