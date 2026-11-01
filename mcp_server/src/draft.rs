//! The draft tool (draft_faces): its parameters, the face and neutral plane
//! references, and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M};
use crate::mirror::unique;
use crate::refgeom::{self, Shape};

/// The add-in's limit.
const MAX_FACES: usize = 500;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DraftPropagation {
    /// Only the faces given.
    #[default]
    None,
    /// Also the faces tangent to them, e.g. round a filleted corner.
    Tangent,
    /// All the walls standing on the neutral plane; faces still names at least one of them.
    All,
    /// The walls of the inner loops on the neutral plane (pockets and holes), and the faces given.
    Inner,
    /// The walls of the outer loop on the neutral plane (the outside), and the faces given.
    Outer,
}

impl DraftPropagation {
    /// swDraftFacePropagationType_e.
    fn sw(self) -> i32 {
        match self {
            DraftPropagation::None => 0,
            DraftPropagation::Tangent => 1,
            DraftPropagation::All => 2,
            DraftPropagation::Inner => 3,
            DraftPropagation::Outer => 4,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftParams {
    /// The taper in degrees, more than 0 and under 90 (1 to 5 is usual for molded parts).
    pub angle_deg: f64,
    /// The faces to taper, by inspect_model id ("face:3"), usually walls standing on the neutral
    /// plane.
    pub faces: Vec<String>,
    /// Where the faces keep their size, the taper pivoting about it: "front", "top" or "right"
    /// (the default planes), a planar face id ("face:3") from inspect_model, or a reference
    /// plane ("feature:4", or its name such as "Plane1"). Usually the parting face.
    pub neutral_plane: String,
    /// Reverse the pull direction, so the faces lean the other way. Defaults to false; the
    /// reply says whether the body grew or shrank.
    #[serde(default)]
    pub flip: bool,
    /// Which more faces to draft with those given. Defaults to none.
    #[serde(default)]
    pub propagation: DraftPropagation,
}

pub fn request(ids: &IdRegistry, params: &DraftParams) -> Result<proto::DraftFacesRequest, McpError> {
    if !(params.angle_deg.is_finite() && params.angle_deg > 0.0 && params.angle_deg < 90.0) {
        return Err(invalid(format!("angle_deg must be more than 0 and under 90, got {}", params.angle_deg)));
    }
    let faces = unique(&params.faces);
    if faces.is_empty() {
        return Err(invalid("give at least one face to draft"));
    }
    if faces.len() > MAX_FACES {
        return Err(invalid(format!("at most {MAX_FACES} faces per call, got {}", faces.len())));
    }
    let neutral = params.neutral_plane.trim();
    if faces.contains(&neutral) {
        return Err(invalid(format!("{neutral} is both a face to draft and the neutral plane")));
    }
    let (neutral_plane, shape) = refgeom::geometry_ref(ids, neutral, "neutral_plane")?;
    if !matches!(shape, Shape::Surface | Shape::Feature) {
        return Err(invalid(format!(
            "neutral_plane: {neutral} isn't a plane -- give \"front\", \"top\" or \"right\", a planar face id \
             (\"face:3\") or a reference plane (\"Plane1\")"
        )));
    }
    let faces = faces
        .into_iter()
        .map(|face| {
            if Kind::of_id(face) != Some(Kind::Face) {
                return Err(invalid(format!(
                    "faces: {face:?} isn't a face id -- give ids like \"face:3\" from inspect_model (what=\"faces\")"
                )));
            }
            let persist_id = ids.persist_id(face).ok_or_else(|| {
                invalid(format!("faces: {}", ids.unknown_id(face, "inspect_model", "call inspect_model for current ones")))
            })?;
            Ok(proto::GeometryRef { target: Some(proto::geometry_ref::Target::ModelPersistId(persist_id)) })
        })
        .collect::<Result<_, McpError>>()?;
    Ok(proto::DraftFacesRequest {
        angle: params.angle_deg.to_radians(),
        faces,
        neutral_plane: Some(neutral_plane),
        flip: params.flip,
        propagation: params.propagation.sw(),
    })
}

/// "created Draft1: 5 degrees on face:3 about top; the body got 87.489 mm^3 smaller. ..."
pub fn reply_text(params: &DraftParams, reply: &proto::DraftFacesResponse) -> String {
    let change = reply.volume_change * MM_PER_M.powi(3);
    let effect = if change.abs() < 0.001 {
        "the body's volume stayed the same".to_string()
    } else {
        format!("the body got {:.3} mm^3 {}", change.abs(), if change < 0.0 { "smaller" } else { "bigger" })
    };
    format!(
        "created {}: {} degrees on {} about {}; {effect} (flip leans the faces the other way). \
         The body's faces and edges have changed: inspect_model again before using their ids.",
        reply.feature_name,
        params.angle_deg,
        unique(&params.faces).join(", "),
        params.neutral_plane.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (IdRegistry, String, String) {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let other = ids.id_for(Kind::Face, b"other").unwrap();
        (ids, face, other)
    }

    fn params(faces: &[&str], neutral: &str, angle_deg: f64) -> DraftParams {
        DraftParams {
            angle_deg,
            faces: faces.iter().map(|f| f.to_string()).collect(),
            neutral_plane: neutral.to_string(),
            flip: false,
            propagation: DraftPropagation::None,
        }
    }

    #[test]
    fn faces_once_each_in_radians() {
        let (ids, face, other) = registry();
        let r = request(&ids, &params(&[&face, &format!(" {face} ")], &other, 45.0)).unwrap();
        assert_eq!(r.faces.len(), 1);
        assert!((r.angle - std::f64::consts::FRAC_PI_4).abs() < 1e-12 && !r.flip && r.propagation == 0);
        assert!(request(&ids, &params(&[&face], "top", 3.0)).is_ok());
    }

    #[test]
    fn propagation_values() {
        let (ids, face, _) = registry();
        let mut p = params(&[&face], "top", 3.0);
        p.propagation = DraftPropagation::Outer;
        assert_eq!(request(&ids, &p).unwrap().propagation, 4);
    }

    #[test]
    fn refuses_bad_input() {
        let (mut ids, face, other) = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        for bad in [0.0, -1.0, 90.0, f64::NAN] {
            assert!(request(&ids, &params(&[&face], "top", bad)).is_err(), "{bad}");
        }
        assert!(request(&ids, &params(&[], "top", 3.0)).is_err());
        assert!(request(&ids, &params(&[edge.as_str()], "top", 3.0)).unwrap_err().message.contains("a face id"));
        assert!(request(&ids, &params(&["top"], "front", 3.0)).unwrap_err().message.contains("a face id"));
        assert!(request(&ids, &params(&["face:999"], "top", 3.0)).is_err());
        assert!(request(&ids, &params(&[&face, &other], &face, 3.0)).unwrap_err().message.contains("both"));
        assert!(request(&ids, &params(&[&face], &edge, 3.0)).unwrap_err().message.contains("isn't a plane"));
    }

    #[test]
    fn replies() {
        let reply = proto::DraftFacesResponse { feature_name: "Draft1".into(), volume_change: -8.75e-8 };
        let text = reply_text(&params(&["face:3", "face:3", "face:4"], " top ", 5.0), &reply);
        assert!(text.starts_with("created Draft1: 5 degrees on face:3, face:4 about top; the body got 87.500 mm^3 smaller"), "{text}");
        let reply = proto::DraftFacesResponse { volume_change: 1e-9, ..reply };
        assert!(reply_text(&params(&["face:3"], "top", 5.0), &reply).contains("1.000 mm^3 bigger"));
        let reply = proto::DraftFacesResponse { volume_change: 0.0, ..reply };
        assert!(reply_text(&params(&["face:3"], "top", 5.0), &reply).contains("volume stayed the same"));
    }
}
