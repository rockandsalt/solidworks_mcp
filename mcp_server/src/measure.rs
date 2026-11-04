//! measure and mass_properties: SolidWorks' Measure tool and Mass Properties
//! on the active part or assembly, as JSON for the model.
//!
//! The add-in hands back what SolidWorks computes, in SI units; this module
//! resolves the ids, converts to mm / degrees / kg·mm² and leaves out what
//! doesn't apply. Details in docs/notes/measure.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, direction, mm, mm3, point_mm, round};
use crate::refgeom::geometry_ref;

/// Most entities one measure call takes (SolidWorks' Measure has no limit,
/// but more than this is a selection, not a measurement).
const MAX_ENTITIES: usize = 50;

/// SolidWorks' default density for a part without a material, kg/m^3.
const DEFAULT_DENSITY: f64 = 1000.0;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

// ---- measure ---------------------------------------------------------------

/// Where distances to an arc or circle are taken from.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArcPoint {
    /// Its center.
    #[default]
    Center,
    /// Its nearest point.
    Min,
    /// Its farthest point.
    Max,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MeasureParams {
    /// What to measure, one or more: inspect_model ids of faces, edges, vertices, components
    /// ("component:N", assemblies) or reference geometry ("feature:N"); sketch entity ids
    /// ("line:3", "point:7"); "origin"; "front", "top" or "right" for the default planes; or a
    /// reference feature's name such as "Plane1". One gives its size (length, area, perimeter,
    /// radius / diameter); two or more the distances and angle between them.
    pub entities: Vec<String>,
    /// Arcs and circles: take distances from their "center" (default), their nearest point
    /// ("min") or farthest point ("max").
    #[serde(default)]
    pub arc: ArcPoint,
}

pub fn measure_request(ids: &IdRegistry, params: &MeasureParams) -> Result<proto::MeasureRequest, McpError> {
    if params.entities.is_empty() {
        return Err(invalid("give at least one entity to measure"));
    }
    if params.entities.len() > MAX_ENTITIES {
        return Err(invalid(format!("at most {MAX_ENTITIES} entities, got {}", params.entities.len())));
    }
    let mut entities = Vec::new();
    for (i, e) in params.entities.iter().enumerate() {
        let what = format!("entities[{i}]");
        let r = if Kind::of_id(e) == Some(Kind::Component) {
            let persist_id = ids.persist_id(e).ok_or_else(|| {
                invalid(format!("{what}: {}", ids.unknown_id(e, "inspect_model", "call inspect_model for current ones")))
            })?;
            proto::GeometryRef { target: Some(proto::geometry_ref::Target::ModelPersistId(persist_id)) }
        } else {
            geometry_ref(ids, e, &what)?.0
        };
        entities.push(r);
    }
    let arc = match params.arc {
        ArcPoint::Center => proto::ArcMeasure::Center,
        ArcPoint::Min => proto::ArcMeasure::Minimum,
        ArcPoint::Max => proto::ArcMeasure::Maximum,
    };
    Ok(proto::MeasureRequest { entities, arc: arc.into() })
}

fn mm2(m2: f64) -> f64 {
    round(m2 * MM_PER_M * MM_PER_M, 4)
}

pub fn measure_json(params: &MeasureParams, reply: &proto::MeasureResponse) -> Value {
    let mut out = Map::new();
    let measured: Vec<String> = params
        .entities
        .iter()
        .zip(&reply.measured)
        .map(|(given, what)| if given.trim() == what { what.clone() } else { format!("{} ({what})", given.trim()) })
        .collect();
    out.insert("measured".into(), json!(measured));
    let several = params.entities.len() > 1;
    let mut put = |key: &str, value: Option<f64>, convert: fn(f64) -> f64| {
        if let Some(v) = value {
            out.insert(key.into(), json!(convert(v)));
        }
    };
    put("distance_mm", reply.distance, mm);
    put("center_distance_mm", reply.center_distance, mm);
    put("normal_distance_mm", reply.normal_distance, mm);
    put("angle_deg", reply.angle, |a| round(a.to_degrees(), 4));
    put("length_mm", reply.length, mm);
    put("area_mm2", reply.area, mm2);
    put("perimeter_mm", reply.perimeter, mm);
    put("radius_mm", reply.radius, mm);
    put("diameter_mm", reply.diameter, mm);
    put("arc_length_mm", reply.arc_length, mm);
    put("chord_length_mm", reply.chord_length, mm);
    // With one entity the totals are its own length / area again.
    if several {
        put("total_length_mm", reply.total_length, mm);
        put("total_area_mm2", reply.total_area, mm2);
    }
    for (key, flag) in [("parallel", reply.parallel), ("perpendicular", reply.perpendicular), ("intersect", reply.intersect)] {
        if flag {
            out.insert(key.into(), json!(true));
        }
    }
    if let (Some(a), Some(b)) = (&reply.closest_point1, &reply.closest_point2) {
        out.insert("closest_points_mm".into(), json!([point_mm(a), point_mm(b)]));
        out.insert("delta_mm".into(), json!([mm(b.x - a.x), mm(b.y - a.y), mm(b.z - a.z)]));
        if let Some(d) = reply.closest_distance {
            out.insert("closest_distance_mm".into(), json!(mm(d)));
        }
    }
    if out.len() == 1 {
        out.insert(
            "note".into(),
            json!("SolidWorks gave no values that apply to these entities"),
        );
    }
    Value::Object(out)
}

// ---- mass_properties ---------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct MassPropertiesParams {
    /// Only these: components of the active assembly ("component:N" from inspect_model; a
    /// subassembly counts with everything in it) or bodies of the active part ("body:N").
    /// Default: the whole part or assembly.
    #[serde(default)]
    pub only: Option<Vec<String>>,
    /// A coordinate system feature of the active document by name, e.g. "Coordinate System1":
    /// the center of mass and the moments of inertia are then given in it. Default: the part's /
    /// assembly's own origin and axes.
    #[serde(default)]
    pub coordinate_system: Option<String>,
    /// Leave hidden bodies / components out. Default false: hidden things are still part of the
    /// model and count.
    #[serde(default)]
    pub exclude_hidden: bool,
}

pub fn mass_request(ids: &IdRegistry, params: &MassPropertiesParams) -> Result<proto::GetMassPropertiesRequest, McpError> {
    let mut persist_ids: Vec<Vec<u8>> = Vec::new();
    for item in params.only.iter().flatten() {
        let item = item.trim();
        if !matches!(Kind::of_id(item), Some(Kind::Component | Kind::Body)) {
            return Err(invalid(format!(
                "{item:?} isn't a component or body -- give \"component:N\" or \"body:N\" ids from inspect_model"
            )));
        }
        let pid = ids.persist_id(item).ok_or_else(|| {
            invalid(ids.unknown_id(item, "inspect_model", "call inspect_model for current ones"))
        })?;
        if !persist_ids.contains(&pid) {
            persist_ids.push(pid);
        }
    }
    Ok(proto::GetMassPropertiesRequest {
        persist_ids,
        coordinate_system: params.coordinate_system.as_deref().map(str::trim).unwrap_or_default().to_string(),
        include_hidden: !params.exclude_hidden,
    })
}

/// To `digits` significant digits: moments of inertia span many orders of
/// magnitude.
fn significant(value: f64, digits: i32) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return 0.0;
    }
    let places = digits - 1 - value.abs().log10().floor() as i32;
    round(value, places.max(0))
}

/// kg·m² -> kg·mm².
fn kg_mm2(i: f64) -> f64 {
    significant(i * MM_PER_M * MM_PER_M, 7)
}

/// 9 values row by row -> a 3x3 matrix in kg·mm². Terms below a billionth
/// of the largest are round-off (SolidWorks gives 1e-34 for a zero product
/// of inertia) and shown as 0.
fn tensor(values: &[f64]) -> Option<Value> {
    let largest = values.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let clean = |v: f64| if v.abs() < largest * 1e-9 { 0.0 } else { kg_mm2(v) };
    (values.len() == 9).then(|| {
        json!(values.chunks(3).map(|row| row.iter().map(|&v| clean(v)).collect::<Vec<_>>()).collect::<Vec<_>>())
    })
}

pub fn mass_json(params: &MassPropertiesParams, reply: &proto::GetMassPropertiesResponse) -> Value {
    let mut out = Map::new();
    let document = reply.document.as_ref();
    out.insert("document".into(), json!(document.map(|d| d.title.clone()).unwrap_or_default()));
    out.insert("configuration".into(), json!(reply.configuration));
    if !reply.measured.is_empty() {
        out.insert("measured".into(), json!(reply.measured));
    }
    out.insert("mass_kg".into(), json!(significant(reply.mass, 7)));
    out.insert("volume_mm3".into(), json!(mm3(reply.volume)));
    out.insert("surface_area_mm2".into(), json!(mm2(reply.surface_area)));
    out.insert("density_kg_m3".into(), json!(round(reply.density, 3)));
    let frame = match params.coordinate_system.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "the model's origin and axes".to_string(),
    };
    out.insert("coordinate_system".into(), json!(frame));
    if let Some(c) = &reply.center_of_mass {
        out.insert("center_of_mass_mm".into(), point_mm(c));
    }
    let mut inertia = Map::new();
    if !reply.principal_moments.is_empty() {
        inertia.insert(
            "principal_moments".into(),
            json!(reply.principal_moments.iter().map(|&p| kg_mm2(p)).collect::<Vec<_>>()),
        );
        inertia.insert("principal_axes".into(), json!(reply.principal_axes.iter().map(direction).collect::<Vec<_>>()));
    }
    if let Some(t) = tensor(&reply.inertia_at_center) {
        inertia.insert("at_center_of_mass".into(), t);
    }
    if let Some(t) = tensor(&reply.inertia_at_origin) {
        inertia.insert("at_coordinate_system".into(), t);
    }
    if !inertia.is_empty() {
        out.insert("inertia_kg_mm2".into(), Value::Object(inertia));
    }
    let is_part = document.is_some_and(|d| d.r#type() == proto::DocumentType::Part);
    if is_part && reply.measured.is_empty() && (reply.density - DEFAULT_DENSITY).abs() < 1e-9 {
        out.insert(
            "note".into(),
            json!("density 1000 kg/m^3 is SolidWorks' default for a part without a material: set one with \
                   part_material for a real mass"),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> proto::Vec3 {
        proto::Vec3 { x, y, z }
    }

    #[test]
    fn measure_request_takes_ids_names_and_planes() {
        let mut ids = IdRegistry::default();
        ids.set_document("a", "Part1");
        let face = ids.id_for(Kind::Face, b"f").unwrap();
        let comp = ids.id_for(Kind::Component, b"c").unwrap();
        let params = MeasureParams { entities: vec![face, comp, "front".into(), "Plane1".into()], arc: ArcPoint::Max };
        let r = measure_request(&ids, &params).unwrap();
        use proto::geometry_ref::Target;
        assert_eq!(r.entities[0].target, Some(Target::ModelPersistId(b"f".to_vec())));
        assert_eq!(r.entities[1].target, Some(Target::ModelPersistId(b"c".to_vec())));
        assert_eq!(r.entities[2].target, Some(Target::StandardPlane(proto::Plane::Front.into())));
        assert_eq!(r.entities[3].target, Some(Target::FeatureName("Plane1".into())));
        assert_eq!(r.arc(), proto::ArcMeasure::Maximum);
    }

    #[test]
    fn measure_request_refuses_nothing_and_unknown_ids() {
        let ids = IdRegistry::default();
        assert!(measure_request(&ids, &MeasureParams { entities: vec![], arc: ArcPoint::Center }).is_err());
        let e = measure_request(&ids, &MeasureParams { entities: vec!["face:99".into()], arc: ArcPoint::Center })
            .unwrap_err();
        assert!(e.message.contains("entities[0]"), "{}", e.message);
    }

    #[test]
    fn measure_json_converts_and_leaves_out_what_does_not_apply() {
        let params = MeasureParams { entities: vec!["face:1".into(), "face:2".into()], arc: ArcPoint::Center };
        let reply = proto::MeasureResponse {
            measured: vec!["face".into(), "face".into()],
            distance: Some(0.01),
            normal_distance: Some(0.01),
            angle: Some(0.0),
            total_area: Some(0.0008),
            parallel: true,
            closest_point1: Some(v(0.0, 0.0, 0.0)),
            closest_point2: Some(v(0.0, 0.0, 0.01)),
            closest_distance: Some(0.01),
            ..Default::default()
        };
        let j = measure_json(&params, &reply);
        assert_eq!(j["measured"], json!(["face:1 (face)", "face:2 (face)"]));
        assert_eq!(j["distance_mm"], json!(10.0));
        assert_eq!(j["angle_deg"], json!(0.0));
        assert_eq!(j["total_area_mm2"], json!(800.0));
        assert_eq!(j["parallel"], json!(true));
        assert!(j.get("perpendicular").is_none() && j.get("length_mm").is_none());
        assert_eq!(j["delta_mm"], json!([0.0, 0.0, 10.0]));
        assert_eq!(j["closest_distance_mm"], json!(10.0));
    }

    #[test]
    fn measure_json_one_entity_has_no_totals() {
        let params = MeasureParams { entities: vec!["edge:1".into()], arc: ArcPoint::Center };
        let reply = proto::MeasureResponse {
            measured: vec!["edge".into()],
            length: Some(0.04),
            total_length: Some(0.04),
            ..Default::default()
        };
        let j = measure_json(&params, &reply);
        assert_eq!(j["length_mm"], json!(40.0));
        assert!(j.get("total_length_mm").is_none());
    }

    #[test]
    fn mass_request_takes_components_and_bodies_only() {
        let mut ids = IdRegistry::default();
        ids.set_document("a", "Asm1");
        let comp = ids.id_for(Kind::Component, b"c").unwrap();
        let face = ids.id_for(Kind::Face, b"f").unwrap();
        let params = MassPropertiesParams { only: Some(vec![comp.clone(), comp]), ..Default::default() };
        let r = mass_request(&ids, &params).unwrap();
        assert_eq!(r.persist_ids, vec![b"c".to_vec()]);
        assert!(r.include_hidden);
        let params = MassPropertiesParams { only: Some(vec![face]), ..Default::default() };
        assert!(mass_request(&ids, &params).is_err());
    }

    #[test]
    fn mass_json_converts_units() {
        let reply = proto::GetMassPropertiesResponse {
            document: Some(proto::Document {
                title: "Part1".into(),
                r#type: proto::DocumentType::Part.into(),
                ..Default::default()
            }),
            configuration: "Default".into(),
            mass: 0.0628,
            volume: 8e-6,
            surface_area: 0.0028,
            density: 7850.0,
            center_of_mass: Some(v(0.02, 0.01, 0.005)),
            inertia_at_center: vec![2.6e-6, 0.0, 0.0, 0.0, 8.9e-6, 0.0, 0.0, 0.0, 1.05e-5],
            principal_moments: vec![2.6e-6, 8.9e-6, 1.05e-5],
            principal_axes: vec![v(1.0, 0.0, 0.0), v(0.0, 1.0, 0.0), v(0.0, 0.0, 1.0)],
            ..Default::default()
        };
        let j = mass_json(&MassPropertiesParams::default(), &reply);
        assert_eq!(j["volume_mm3"], json!(8000.0));
        assert_eq!(j["surface_area_mm2"], json!(2800.0));
        assert_eq!(j["center_of_mass_mm"], json!([20.0, 10.0, 5.0]));
        assert_eq!(j["inertia_kg_mm2"]["principal_moments"], json!([2.6, 8.9, 10.5]));
        assert_eq!(j["inertia_kg_mm2"]["at_center_of_mass"][1], json!([0.0, 8.9, 0.0]));
        assert!(j["inertia_kg_mm2"].get("at_coordinate_system").is_none());
        assert!(j.get("note").is_none());
    }

    #[test]
    fn mass_json_flags_the_default_density() {
        let reply = proto::GetMassPropertiesResponse {
            document: Some(proto::Document { r#type: proto::DocumentType::Part.into(), ..Default::default() }),
            density: 1000.0,
            ..Default::default()
        };
        assert!(mass_json(&MassPropertiesParams::default(), &reply)["note"].is_string());
    }

    #[test]
    fn tensor_drops_round_off() {
        let t = tensor(&[1.05e-5, -2.1e-40, 3e-6, -2.1e-40, 2.6e-6, 0.0, 3e-6, 0.0, 8.9e-6]).unwrap();
        assert_eq!(t, json!([[10.5, 0.0, 3.0], [0.0, 2.6, 0.0], [3.0, 0.0, 8.9]]));
        assert!(tensor(&[1.0; 8]).is_none());
    }

    #[test]
    fn significant_digits() {
        assert_eq!(significant(0.000123456789, 3), 0.000123);
        assert_eq!(significant(123456.7, 3), 123457.0);
        assert_eq!(significant(0.0, 3), 0.0);
    }
}
