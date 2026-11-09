//! revolve_sketch (also the cut): its parameters,
//! the axis reference, and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind};
use crate::refgeom::{self, Shape};
use crate::sketch;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RevolveParams {
    /// A sketch line id ("line:3"), a straight edge ("edge:7") or a reference axis
    /// ("feature:4", "Axis1"). Defaults to the sketch's one centerline (a construction line).
    #[serde(default)]
    pub axis: Option<String>,
    /// More than 0, at most 360; with mid_plane the total. Defaults to 360.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Remove material instead (cut-revolve).
    #[serde(default)]
    pub cut: bool,
    /// The sketch, e.g. "Sketch1". Defaults to the newest sketch no feature uses yet.
    #[serde(default)]
    pub sketch_name: Option<String>,
    /// The other way round the axis. Ignored with mid_plane or a full turn.
    #[serde(default)]
    pub reverse_direction: bool,
    /// Symmetric to both sides of the sketch plane.
    #[serde(default)]
    pub mid_plane: bool,
}

impl RevolveParams {
    fn angle_deg(&self) -> f64 {
        self.angle_deg.unwrap_or(360.0)
    }
}

/// The axis string -> its wire ref. Points, faces and planes can't be axes.
fn axis_ref(ids: &IdRegistry, axis: &str) -> Result<proto::GeometryRef, McpError> {
    let (r, shape) = refgeom::geometry_ref(ids, axis, "axis")?;
    match shape {
        Shape::Edge | Shape::Segment | Shape::Feature => Ok(r),
        Shape::Round => Err(invalid(format!(
            "axis: {axis} is an arc or circle -- give a sketch line, a straight edge or a reference \
             axis (for the axis of a hole or a round face, make a reference axis on the face first)"
        ))),
        Shape::Surface => Err(invalid(format!(
            "axis: {axis} is a face or plane -- give a sketch line, a straight edge or a reference \
             axis (for the axis of a cylindrical face, make a reference axis on it first)"
        ))),
        Shape::Point => Err(invalid(format!(
            "axis: {axis} is a point -- give a sketch line, a straight edge or a reference axis"
        ))),
    }
}

pub fn request(ids: &IdRegistry, params: &RevolveParams, cut: bool) -> Result<proto::RevolveSketchRequest, McpError> {
    let angle_deg = params.angle_deg();
    if !(angle_deg.is_finite() && angle_deg > 0.0 && angle_deg <= 360.0) {
        return Err(invalid(format!(
            "angle_deg must be more than 0 and at most 360, got {angle_deg}"
        )));
    }
    let axis = params
        .axis
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| axis_ref(ids, a))
        .transpose()?;
    Ok(proto::RevolveSketchRequest {
        sketch_name: params.sketch_name.as_deref().map(str::trim).unwrap_or_default().to_string(),
        axis,
        angle: angle_deg.to_radians(),
        reverse_direction: params.reverse_direction,
        mid_plane: params.mid_plane,
        cut,
    })
}

/// "created Revolve1 by revolving Sketch1 360 degrees about the centerline of Sketch1
/// (line:3)."
pub fn reply_text(ids: &mut IdRegistry, params: &RevolveParams, reply: &proto::RevolveSketchResponse, cut: bool) -> String {
    let angle_deg = params.angle_deg();
    let how = if params.mid_plane {
        " (mid-plane)"
    } else if params.reverse_direction && angle_deg < 360.0 {
        " (reversed)"
    } else {
        ""
    };
    let axis = match reply.axis_item.as_ref().and_then(|r| sketch::ref_id(ids, Kind::Line, r)) {
        Some(id) => format!("{} ({id})", reply.axis),
        None => reply.axis.clone(),
    };
    let flipped = if reply.direction_flipped {
        " It cut the other way round the axis (reversed): the default way there was nothing to remove."
    } else {
        ""
    };
    format!(
        "created {} by {} {} {angle_deg} degrees{how} about {axis}.{flipped}",
        reply.feature_name,
        if cut { "cutting with" } else { "revolving" },
        reply.sketch_name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> IdRegistry {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        ids
    }

    fn params(axis: Option<&str>, angle_deg: Option<f64>) -> RevolveParams {
        RevolveParams {
            axis: axis.map(Into::into),
            angle_deg,
            cut: false,
            sketch_name: None,
            reverse_direction: false,
            mid_plane: false,
        }
    }

    fn line_ref() -> proto::SketchItemRef {
        proto::SketchItemRef { sketch_persist_id: vec![1, 2, 3], point: false, id0: 0, id1: 4, ..Default::default() }
    }

    #[test]
    fn defaults_to_a_full_turn_about_the_sketch_centerline() {
        let r = request(&registry(), &params(None, None), false).unwrap();
        assert!((r.angle - std::f64::consts::TAU).abs() < 1e-12);
        assert!(r.axis.is_none() && !r.cut && r.sketch_name.is_empty());
    }

    #[test]
    fn angle_must_be_within_one_turn() {
        for bad in [0.0, -10.0, 360.5, f64::NAN] {
            assert!(request(&registry(), &params(None, Some(bad)), true).is_err(), "{bad}");
        }
        let r = request(&registry(), &params(None, Some(90.0)), true).unwrap();
        assert!((r.angle - std::f64::consts::FRAC_PI_2).abs() < 1e-12 && r.cut);
    }

    #[test]
    fn axis_takes_lines_edges_and_axes_only() {
        let mut ids = registry();
        let line = sketch::ref_id(&mut ids, Kind::Line, &line_ref()).unwrap();
        let circle = sketch::ref_id(
            &mut ids,
            Kind::Circle,
            &proto::SketchItemRef { sketch_persist_id: vec![1, 2, 3], point: false, id0: 0, id1: 5, ..Default::default() },
        )
        .unwrap();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();

        use proto::geometry_ref::Target;
        let target = |axis: &str| request(&ids, &params(Some(axis), None), false).map(|r| r.axis.unwrap().target.unwrap());
        assert_eq!(target(&line).unwrap(), Target::SketchItem(line_ref()));
        assert_eq!(target(&edge).unwrap(), Target::ModelPersistId(b"edge".to_vec()));
        assert_eq!(target("Axis1").unwrap(), Target::FeatureName("Axis1".into()));
        for bad in [circle.as_str(), face.as_str(), "front", "origin"] {
            assert!(target(bad).is_err(), "{bad}");
        }
        assert!(request(&ids, &params(Some("  "), None), false).unwrap().axis.is_none());
    }

    #[test]
    fn reply_names_the_axis_line() {
        let mut ids = registry();
        let reply = proto::RevolveSketchResponse {
            feature_name: "Cut-Revolve1".into(),
            sketch_name: "Sketch2".into(),
            axis: "the centerline of Sketch2".into(),
            axis_item: Some(line_ref()),
            direction_flipped: true,
            ..Default::default()
        };
        let text = reply_text(&mut ids, &params(None, Some(90.0)), &reply, true);
        let line = sketch::ref_id(&mut ids, Kind::Line, &line_ref()).unwrap();
        assert!(text.starts_with(&format!(
            "created Cut-Revolve1 by cutting with Sketch2 90 degrees about the centerline of Sketch2 ({line})."
        )), "{text}");
        assert!(text.contains("other way"), "{text}");
    }
}
