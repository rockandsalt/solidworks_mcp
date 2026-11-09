//! The edge-finishing tools (fillet_edges, chamfer_edges): their parameters,
//! the edge / face references, and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M};

/// The add-in's limit.
const MAX_EDGES: usize = 500;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FilletParams {
    /// Edges to round, by inspect_model id ("edge:7"), or faces ("face:3") to round all of
    /// their edges, e.g. the top face of a box for a rounded top.
    pub edges: Vec<String>,
    /// Fillet radius in mm, more than 0.
    pub radius_mm: f64,
    /// Also round the edges that run on tangentially from the given ones (SolidWorks'
    /// default), so a chain of edges is rounded as one. Defaults to true.
    #[serde(default = "default_true")]
    pub tangent_propagation: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChamferParams {
    /// Edges to bevel, by inspect_model id ("edge:7"), or faces ("face:3") to bevel all of
    /// their edges.
    pub edges: Vec<String>,
    /// How far the chamfer cuts back into the faces on both sides of the edge, in mm (a
    /// 45 degree, equal-distance chamfer). With angle_deg or distance2_mm: how far it cuts
    /// into the first face.
    pub distance_mm: f64,
    /// Angle-distance chamfer: the bevel's angle to the first face, in degrees, more than 0
    /// and less than 90. Not with distance2_mm.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Distance-distance chamfer: how far it cuts into the second face, in mm. Not with
    /// angle_deg.
    #[serde(default)]
    pub distance2_mm: Option<f64>,
    /// Swap the faces distance_mm and the angle / distance2_mm are measured on. Only with
    /// angle_deg or distance2_mm.
    #[serde(default)]
    pub flip: bool,
    /// Also bevel the edges that run on tangentially from the given ones (SolidWorks'
    /// default). Defaults to true.
    #[serde(default = "default_true")]
    pub tangent_propagation: bool,
}

/// The edge / face strings, trimmed, without repeats.
fn unique(edges: &[String]) -> Vec<&str> {
    let mut list: Vec<&str> = Vec::new();
    for edge in edges.iter().map(|e| e.trim()) {
        if !list.contains(&edge) {
            list.push(edge);
        }
    }
    list
}

/// The edge / face strings -> wire refs.
fn edge_refs(ids: &IdRegistry, edges: &[String]) -> Result<Vec<proto::GeometryRef>, McpError> {
    let seen = unique(edges);
    if seen.is_empty() {
        return Err(invalid("edges is empty -- give edge ids (\"edge:7\") or face ids from inspect_model"));
    }
    if seen.len() > MAX_EDGES {
        return Err(invalid(format!("at most {MAX_EDGES} edges and faces per call, got {}", seen.len())));
    }
    seen.into_iter()
        .map(|edge| {
            if !matches!(Kind::of_id(edge), Some(Kind::Edge | Kind::Face)) {
                return Err(invalid(format!(
                    "edges: {edge:?} isn't an edge or face id -- give ids like \"edge:7\" or \"face:3\" \
                     from inspect_model (what=\"edges\" or \"faces\")"
                )));
            }
            let persist_id = ids.persist_id(edge).ok_or_else(|| {
                invalid(format!(
                    "edges: {}",
                    ids.unknown_id(edge, "inspect_model", "call inspect_model for current ones")
                ))
            })?;
            Ok(proto::GeometryRef { target: Some(proto::geometry_ref::Target::ModelPersistId(persist_id)) })
        })
        .collect()
}

fn positive_mm(value: f64, name: &str) -> Result<f64, McpError> {
    if !(value.is_finite() && value > 0.0) {
        return Err(invalid(format!("{name} must be more than 0, got {value}")));
    }
    Ok(value / MM_PER_M)
}

pub fn fillet_request(ids: &IdRegistry, params: &FilletParams) -> Result<proto::FilletEdgesRequest, McpError> {
    Ok(proto::FilletEdgesRequest {
        edges: edge_refs(ids, &params.edges)?,
        radius: positive_mm(params.radius_mm, "radius_mm")?,
        tangent_propagation: params.tangent_propagation,
    })
}

pub fn chamfer_request(ids: &IdRegistry, params: &ChamferParams) -> Result<proto::ChamferEdgesRequest, McpError> {
    let angle = match params.angle_deg {
        None => 0.0,
        Some(a) if !(a.is_finite() && a > 0.0 && a < 90.0) => {
            return Err(invalid(format!("angle_deg must be more than 0 and less than 90, got {a}")));
        }
        Some(_) if params.distance2_mm.is_some() => {
            return Err(invalid("give angle_deg or distance2_mm, not both"));
        }
        Some(a) => a.to_radians(),
    };
    let other_distance = params.distance2_mm.map(|d| positive_mm(d, "distance2_mm")).transpose()?.unwrap_or(0.0);
    if params.flip && angle == 0.0 && other_distance == 0.0 {
        return Err(invalid("flip only applies with angle_deg or distance2_mm -- an equal-distance chamfer is symmetric"));
    }
    Ok(proto::ChamferEdgesRequest {
        edges: edge_refs(ids, &params.edges)?,
        distance: positive_mm(params.distance_mm, "distance_mm")?,
        angle,
        other_distance,
        flip: params.flip,
        tangent_propagation: params.tangent_propagation,
    })
}

/// "created Fillet1: a 2 mm radius on edge:3, face:1. The faces next to them ..."
pub fn fillet_text(params: &FilletParams, reply: &proto::EdgeFeatureResponse) -> String {
    format!(
        "created {}: a {} mm radius on {}.{STALE_IDS}",
        reply.feature_name,
        params.radius_mm,
        unique(&params.edges).join(", ")
    )
}

/// "created Chamfer1: 1 mm x 45 degrees on edge:3."
pub fn chamfer_text(params: &ChamferParams, reply: &proto::EdgeFeatureResponse) -> String {
    let size = match (params.angle_deg, params.distance2_mm) {
        (Some(a), _) => format!("{} mm x {a} degrees", params.distance_mm),
        (_, Some(d)) => format!("{} mm x {d} mm", params.distance_mm),
        _ => format!("{} mm x 45 degrees (equal distance)", params.distance_mm),
    };
    let flipped = if params.flip { ", flipped" } else { "" };
    format!(
        "created {}: {size}{flipped} on {}.{STALE_IDS}",
        reply.feature_name,
        unique(&params.edges).join(", ")
    )
}

/// SolidWorks gives every face a feature changes a new persist id.
const STALE_IDS: &str = " The faces next to those edges have new ids now: inspect_model again before using face \
                         or edge ids there.";

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (IdRegistry, String, String) {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        (ids, edge, face)
    }

    fn fillet(edges: &[&str], radius_mm: f64) -> FilletParams {
        FilletParams { edges: edges.iter().map(|e| e.to_string()).collect(), radius_mm, tangent_propagation: true }
    }

    fn chamfer(edges: &[&str], distance_mm: f64) -> ChamferParams {
        ChamferParams {
            edges: edges.iter().map(|e| e.to_string()).collect(),
            distance_mm,
            angle_deg: None,
            distance2_mm: None,
            flip: false,
            tangent_propagation: true,
        }
    }

    fn persist_ids(refs: &[proto::GeometryRef]) -> Vec<Vec<u8>> {
        refs.iter()
            .map(|r| match &r.target {
                Some(proto::geometry_ref::Target::ModelPersistId(p)) => p.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn fillet_takes_edges_and_faces_once_each_in_meters() {
        let (ids, edge, face) = registry();
        let r = fillet_request(&ids, &fillet(&[&edge, &face, &format!(" {edge} ")], 2.5)).unwrap();
        assert_eq!(persist_ids(&r.edges), vec![b"edge".to_vec(), b"face".to_vec()]);
        assert!((r.radius - 0.0025).abs() < 1e-15 && r.tangent_propagation);
    }

    #[test]
    fn only_known_edge_and_face_ids() {
        let (mut ids, edge, _) = registry();
        let body = ids.id_for(Kind::Body, b"body").unwrap();
        for bad in ["", "Boss-Extrude1", "front", body.as_str(), "edge:999"] {
            assert!(fillet_request(&ids, &fillet(&[bad], 1.0)).is_err(), "{bad}");
        }
        assert!(fillet_request(&ids, &fillet(&[], 1.0)).is_err());
        for bad in [0.0, -1.0, f64::NAN] {
            assert!(fillet_request(&ids, &fillet(&[&edge], bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn chamfer_kinds() {
        let (ids, edge, _) = registry();
        let r = chamfer_request(&ids, &chamfer(&[&edge], 1.0)).unwrap();
        assert!((r.distance - 0.001).abs() < 1e-15 && r.angle == 0.0 && r.other_distance == 0.0);

        let mut p = chamfer(&[&edge], 1.0);
        p.angle_deg = Some(30.0);
        p.flip = true;
        let r = chamfer_request(&ids, &p).unwrap();
        assert!((r.angle - std::f64::consts::FRAC_PI_6).abs() < 1e-12 && r.flip);

        let mut p = chamfer(&[&edge], 1.0);
        p.distance2_mm = Some(2.0);
        let r = chamfer_request(&ids, &p).unwrap();
        assert!((r.other_distance - 0.002).abs() < 1e-15);
    }

    #[test]
    fn chamfer_refusals() {
        let (ids, edge, _) = registry();
        for bad in [0.0, 90.0, -5.0, f64::NAN] {
            let mut p = chamfer(&[&edge], 1.0);
            p.angle_deg = Some(bad);
            assert!(chamfer_request(&ids, &p).is_err(), "{bad}");
        }
        let mut p = chamfer(&[&edge], 1.0);
        p.angle_deg = Some(30.0);
        p.distance2_mm = Some(2.0);
        assert!(chamfer_request(&ids, &p).is_err());
        let mut p = chamfer(&[&edge], 1.0);
        p.distance2_mm = Some(0.0);
        assert!(chamfer_request(&ids, &p).is_err());
        let mut p = chamfer(&[&edge], 1.0);
        p.flip = true;
        assert!(chamfer_request(&ids, &p).is_err());
    }

    #[test]
    fn replies() {
        let reply = proto::EdgeFeatureResponse { feature_name: "Fillet1".into() };
        let text = fillet_text(&fillet(&["edge:3", "face:1", "edge:3"], 2.0), &reply);
        assert!(text.starts_with("created Fillet1: a 2 mm radius on edge:3, face:1."), "{text}");
        let reply = proto::EdgeFeatureResponse { feature_name: "Chamfer1".into() };
        let mut p = chamfer(&["edge:3"], 1.0);
        assert!(chamfer_text(&p, &reply).starts_with("created Chamfer1: 1 mm x 45 degrees (equal distance) on edge:3."));
        p.distance2_mm = Some(2.0);
        p.flip = true;
        assert!(chamfer_text(&p, &reply).starts_with("created Chamfer1: 1 mm x 2 mm, flipped on edge:3."));
    }
}
