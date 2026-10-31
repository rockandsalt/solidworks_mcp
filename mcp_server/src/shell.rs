//! The shell tool (shell_body): its parameters, the face and body references,
//! and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M};

/// The add-in's limit.
const MAX_FACES: usize = 500;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShellParams {
    /// Wall thickness in mm, more than 0.
    pub thickness_mm: f64,
    /// Faces to remove, leaving openings, by inspect_model id ("face:3"), e.g. the top face of
    /// a box for an open tray. Defaults to none: the body is closed all round and hollow inside
    /// (an empty cavity you can't see from outside).
    #[serde(default)]
    pub open_faces: Vec<String>,
    /// Walls with a thickness of their own, e.g. a thicker base. Each face stays a wall, so not
    /// one of open_faces. Defaults to none: thickness_mm everywhere.
    #[serde(default)]
    pub face_thicknesses: Vec<FaceThickness>,
    /// Add the walls outside the body's faces, keeping its faces as the inside of the shell and
    /// making the body bigger, instead of inside them. Defaults to false.
    #[serde(default)]
    pub outward: bool,
    /// The body to shell, by inspect_model id ("body:1"), for a closed shell of a part with
    /// several solid bodies. Not with open_faces (they already say which) or face_thicknesses
    /// (a closed shell with its own wall thicknesses only works in a part with one body).
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FaceThickness {
    /// The wall, by inspect_model id ("face:3").
    pub face: String,
    /// Its thickness in mm, more than 0.
    pub thickness_mm: f64,
}

/// The strings, trimmed, without repeats.
fn unique(faces: &[String]) -> Vec<&str> {
    let mut list: Vec<&str> = Vec::new();
    for face in faces.iter().map(|f| f.trim()) {
        if !list.contains(&face) {
            list.push(face);
        }
    }
    list
}

fn face_ref(ids: &IdRegistry, what: &str, face: &str) -> Result<proto::GeometryRef, McpError> {
    if Kind::of_id(face) != Some(Kind::Face) {
        return Err(invalid(format!(
            "{what}: {face:?} isn't a face id -- give ids like \"face:3\" from inspect_model (what=\"faces\")"
        )));
    }
    let persist_id = ids
        .persist_id(face)
        .ok_or_else(|| invalid(format!("{what}: {}", ids.unknown_id(face, "inspect_model", "call inspect_model for current ones"))))?;
    Ok(proto::GeometryRef { target: Some(proto::geometry_ref::Target::ModelPersistId(persist_id)) })
}

fn positive_mm(value: f64, name: &str) -> Result<f64, McpError> {
    if !(value.is_finite() && value > 0.0) {
        return Err(invalid(format!("{name} must be more than 0, got {value}")));
    }
    Ok(value / MM_PER_M)
}

pub fn request(ids: &IdRegistry, params: &ShellParams) -> Result<proto::ShellBodyRequest, McpError> {
    let open = unique(&params.open_faces);
    if open.len() + params.face_thicknesses.len() > MAX_FACES {
        return Err(invalid(format!(
            "at most {MAX_FACES} faces per call, got {}",
            open.len() + params.face_thicknesses.len()
        )));
    }
    let mut walls: Vec<&str> = Vec::new();
    for entry in &params.face_thicknesses {
        let face = entry.face.trim();
        if open.contains(&face) {
            return Err(invalid(format!(
                "{face} is in both open_faces and face_thicknesses -- a face is either removed or a wall"
            )));
        }
        if walls.contains(&face) {
            return Err(invalid(format!("{face} is in face_thicknesses more than once")));
        }
        walls.push(face);
    }
    let body = match params.body.as_deref().map(str::trim) {
        None => Vec::new(),
        Some(_) if !open.is_empty() || !walls.is_empty() => {
            return Err(invalid(
                "body is only for a closed shell of one even thickness -- open_faces already say which body, and \
                 SolidWorks can't combine a closed shell with face_thicknesses in a part with several bodies",
            ));
        }
        Some(body) => {
            if Kind::of_id(body) != Some(Kind::Body) {
                return Err(invalid(format!(
                    "body: {body:?} isn't a body id -- give an id like \"body:1\" from inspect_model (what=\"bodies\")"
                )));
            }
            ids.persist_id(body).ok_or_else(|| {
                invalid(format!("body: {}", ids.unknown_id(body, "inspect_model", "call inspect_model for current ones")))
            })?
        }
    };
    Ok(proto::ShellBodyRequest {
        thickness: positive_mm(params.thickness_mm, "thickness_mm")?,
        open_faces: open.into_iter().map(|f| face_ref(ids, "open_faces", f)).collect::<Result<_, _>>()?,
        face_thicknesses: params
            .face_thicknesses
            .iter()
            .map(|entry| {
                Ok(proto::ShellFaceThickness {
                    face: Some(face_ref(ids, "face_thicknesses", entry.face.trim())?),
                    thickness: positive_mm(entry.thickness_mm, "face_thicknesses thickness_mm")?,
                })
            })
            .collect::<Result<_, McpError>>()?,
        outward: params.outward,
        body,
    })
}

/// "created Shell1: 2 mm walls inside the body, open at face:3 (5 mm at face:1). ..."
pub fn reply_text(params: &ShellParams, reply: &proto::EdgeFeatureResponse) -> String {
    let open = unique(&params.open_faces);
    let opening = if open.is_empty() {
        "closed all round, hollow inside".to_string()
    } else {
        format!("open at {}", open.join(", "))
    };
    let walls = if params.face_thicknesses.is_empty() {
        String::new()
    } else {
        let list: Vec<String> =
            params.face_thicknesses.iter().map(|e| format!("{} mm at {}", e.thickness_mm, e.face.trim())).collect();
        format!(" ({})", list.join(", "))
    };
    format!(
        "created {}: {} mm walls {} the body{walls}, {opening}. The body's faces and edges have changed: \
         inspect_model again before using their ids.",
        reply.feature_name,
        params.thickness_mm,
        if params.outward { "outside" } else { "inside" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (IdRegistry, String, String, String) {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let other = ids.id_for(Kind::Face, b"other").unwrap();
        let body = ids.id_for(Kind::Body, b"body").unwrap();
        (ids, face, other, body)
    }

    fn params(open: &[&str], thickness_mm: f64) -> ShellParams {
        ShellParams {
            thickness_mm,
            open_faces: open.iter().map(|f| f.to_string()).collect(),
            face_thicknesses: Vec::new(),
            outward: false,
            body: None,
        }
    }

    fn wall(face: &str, thickness_mm: f64) -> FaceThickness {
        FaceThickness { face: face.to_string(), thickness_mm }
    }

    fn persist_id(r: &proto::GeometryRef) -> Vec<u8> {
        match &r.target {
            Some(proto::geometry_ref::Target::ModelPersistId(p)) => p.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn open_faces_once_each_in_meters() {
        let (ids, face, other, _) = registry();
        let r = request(&ids, &params(&[&face, &other, &format!(" {face} ")], 2.5)).unwrap();
        assert_eq!(r.open_faces.iter().map(persist_id).collect::<Vec<_>>(), vec![b"face".to_vec(), b"other".to_vec()]);
        assert!((r.thickness - 0.0025).abs() < 1e-15 && !r.outward && r.body.is_empty());
    }

    #[test]
    fn closed_needs_no_faces() {
        let (ids, ..) = registry();
        let r = request(&ids, &params(&[], 1.0)).unwrap();
        assert!(r.open_faces.is_empty() && r.face_thicknesses.is_empty());
    }

    #[test]
    fn walls_with_their_own_thickness() {
        let (ids, face, other, _) = registry();
        let mut p = params(&[&face], 2.0);
        p.face_thicknesses = vec![wall(&format!(" {other} "), 5.0)];
        let r = request(&ids, &p).unwrap();
        assert_eq!(persist_id(r.face_thicknesses[0].face.as_ref().unwrap()), b"other".to_vec());
        assert!((r.face_thicknesses[0].thickness - 0.005).abs() < 1e-15);
    }

    #[test]
    fn refuses_a_face_in_two_roles() {
        let (ids, face, other, _) = registry();
        let mut p = params(&[&face], 2.0);
        p.face_thicknesses = vec![wall(&face, 5.0)];
        assert!(request(&ids, &p).unwrap_err().message.contains("both"));
        let mut p = params(&[], 2.0);
        p.face_thicknesses = vec![wall(&other, 5.0), wall(&other, 3.0)];
        assert!(request(&ids, &p).unwrap_err().message.contains("more than once"));
    }

    #[test]
    fn only_known_face_ids_and_positive_sizes() {
        let (mut ids, face, other, body) = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        for bad in ["", "Boss-Extrude1", "top", edge.as_str(), body.as_str(), "face:999"] {
            assert!(request(&ids, &params(&[bad], 1.0)).is_err(), "{bad}");
            let mut p = params(&[], 1.0);
            p.face_thicknesses = vec![wall(bad, 1.0)];
            assert!(request(&ids, &p).is_err(), "{bad}");
        }
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(request(&ids, &params(&[&face], bad)).is_err(), "{bad}");
            let mut p = params(&[&face], 1.0);
            p.face_thicknesses = vec![wall(&other, bad)];
            assert!(request(&ids, &p).is_err(), "{bad}");
        }
    }

    #[test]
    fn body_only_for_a_closed_shell() {
        let (ids, face, other, body) = registry();
        let mut p = params(&[], 1.0);
        p.body = Some(format!(" {body} "));
        assert_eq!(request(&ids, &p).unwrap().body, b"body".to_vec());
        p.body = Some(face.clone());
        assert!(request(&ids, &p).unwrap_err().message.contains("isn't a body id"));
        p.body = Some("body:999".into());
        assert!(request(&ids, &p).is_err());
        let mut p = params(&[&face], 1.0);
        p.body = Some(body.clone());
        assert!(request(&ids, &p).unwrap_err().message.contains("closed shell"));
        let mut p = params(&[], 1.0);
        p.face_thicknesses = vec![wall(&other, 2.0)];
        p.body = Some(body);
        assert!(request(&ids, &p).unwrap_err().message.contains("closed shell"));
    }

    #[test]
    fn replies() {
        let reply = proto::EdgeFeatureResponse { feature_name: "Shell1".into() };
        let text = reply_text(&params(&["face:3", "face:1", "face:3"], 2.0), &reply);
        assert!(text.starts_with("created Shell1: 2 mm walls inside the body, open at face:3, face:1."), "{text}");
        let mut p = params(&[], 1.5);
        p.outward = true;
        p.face_thicknesses = vec![wall("face:2", 5.0), wall("face:4", 3.0)];
        let text = reply_text(&p, &reply);
        assert!(
            text.starts_with(
                "created Shell1: 1.5 mm walls outside the body (5 mm at face:2, 3 mm at face:4), closed all round, hollow inside."
            ),
            "{text}"
        );
    }
}
