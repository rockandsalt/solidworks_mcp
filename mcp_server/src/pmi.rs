//! The inspect_annotations tool: the model's 3D annotations (PMI / MBD) --
//! DimXpert ("MBD Dimension") dimensions and tolerances, datums, feature
//! control frames, notes, ... -- as compact JSON.
//!
//! The add-in passes SolidWorks' enum values through untouched
//! (swAnnotationType_e, swTolType_e, swGtolGeomCharSymbol_e,
//! swGtolMatCondition_e); they're named here.

use rmcp::schemars;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, mm, point_mm, round};

pub const DEFAULT_LIMIT: usize = 100;

/// Annotation kinds the tool can filter by (swAnnotationType_e, grouped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    /// Dimensions, with their tolerances.
    Dimension,
    /// Datum feature symbols ("A", "B", ...).
    Datum,
    /// Geometric tolerances (feature control frames).
    Gtol,
    /// Datum targets (points, lines or areas that establish a datum).
    DatumTarget,
    /// Notes (free text).
    Note,
    /// Surface finish (roughness) symbols.
    SurfaceFinish,
    /// Anything else: weld symbols, cosmetic threads, center marks, ...
    Other,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectAnnotationsParams {
    /// Only annotations of this kind: "dimension", "datum", "gtol", "datum_target", "note",
    /// "surface_finish" or "other". Defaults to all.
    #[serde(default)]
    pub kind: Option<AnnotationKind>,
    /// Only annotations attached to this entity, an id from inspect_model: a face or edge
    /// ("face:7"), a reference plane/axis feature, or in an assembly a component
    /// ("component:2", for everything attached to its geometry).
    #[serde(default)]
    pub on: Option<String>,
    /// Also list the dimensions that drive features and sketches (e.g. "D1@Sketch1"). By
    /// default only annotation dimensions are listed: DimXpert / MBD dimensions and
    /// reference dimensions.
    #[serde(default)]
    pub include_feature_dimensions: bool,
    /// How many annotations to return. Defaults to 100.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Skip this many matching annotations first, for paging. Defaults to 0.
    #[serde(default)]
    pub offset: Option<usize>,
}

/// swAnnotationType_e values the add-in reports.
mod sw {
    pub const CTHREAD: i32 = 1;
    pub const DATUM_TAG: i32 = 2;
    pub const DATUM_TARGET: i32 = 3;
    pub const DIMENSION: i32 = 4;
    pub const GTOL: i32 = 5;
    pub const NOTE: i32 = 6;
    pub const SF_SYMBOL: i32 = 7;
}

pub fn kind_of(a: &proto::AnnotationInfo) -> AnnotationKind {
    match a.r#type {
        sw::DIMENSION => AnnotationKind::Dimension,
        sw::DATUM_TAG => AnnotationKind::Datum,
        sw::GTOL => AnnotationKind::Gtol,
        sw::DATUM_TARGET => AnnotationKind::DatumTarget,
        sw::NOTE => AnnotationKind::Note,
        sw::SF_SYMBOL => AnnotationKind::SurfaceFinish,
        _ => AnnotationKind::Other,
    }
}

/// swAnnotationType_e as a word.
pub fn type_name(t: i32) -> &'static str {
    match t {
        sw::CTHREAD => "cosmetic_thread",
        sw::DATUM_TAG => "datum",
        sw::DATUM_TARGET => "datum_target",
        sw::DIMENSION => "dimension",
        sw::GTOL => "gtol",
        sw::NOTE => "note",
        sw::SF_SYMBOL => "surface_finish",
        8 => "weld_symbol",
        9 => "custom_symbol",
        10 => "dowel_pin_symbol",
        11 => "leader",
        12 => "block",
        13 => "center_mark",
        14 => "table",
        15 => "centerline",
        16 => "datum_origin",
        17 => "weld_bead",
        18 => "revision_cloud",
        19 => "imported_pmi",
        _ => "other",
    }
}

/// swGtolGeomCharSymbol_e as a word; None for "no symbol".
fn characteristic_name(symbol: i32) -> Option<&'static str> {
    Some(match symbol {
        13 => "symmetry",
        14 => "straightness",
        15 => "flatness",
        16 => "circularity",
        17 => "cylindricity",
        18 => "profile_of_a_line",
        19 => "profile_of_a_surface",
        20 => "angularity",
        21 => "perpendicularity",
        22 => "parallelism",
        23 => "position",
        24 => "concentricity",
        25 => "circular_runout",
        26 => "total_runout",
        27 => "circular_runout_open",
        28 => "total_runout_open",
        30 => "square",
        12 | 0 | -1 => return None,
        _ => "unknown",
    })
}

/// swGtolMatCondition_e as the modifier a frame shows; None for "none".
fn condition_name(condition: i32) -> Option<&'static str> {
    Some(match condition {
        1 => "MMC",
        2 => "RFS",
        3 => "LMC",
        5 => "projected",
        6 => "diameter",
        7 => "spherical_diameter",
        8 => "radius",
        9 => "spherical_radius",
        10 => "reference",
        11 => "arc_length",
        _ => return None,
    })
}

/// Short modifier notation for callouts: "(M)", "(L)", ...
fn condition_mark(condition: i32) -> Option<&'static str> {
    Some(match condition {
        1 => "(M)",
        2 => "(S)",
        3 => "(L)",
        5 => "(P)",
        _ => return None,
    })
}

/// swTolType_e as a word; None for no tolerance.
fn tolerance_name(t: i32) -> Option<&'static str> {
    Some(match t {
        1 => "basic",
        2 => "bilateral",
        3 => "limit",
        4 => "symmetric",
        5 => "min",
        6 => "max",
        7 => "fit",
        8 => "fit_with_tolerance",
        9 => "fit_tolerance_only",
        10 => "block",
        11 => "general",
        _ => return None,
    })
}

/// A dimension that drives a feature or sketch (as opposed to an
/// annotation dimension: DimXpert, reference, ...).
pub fn is_feature_dimension(a: &proto::AnnotationInfo) -> bool {
    if a.r#type != sw::DIMENSION || a.dimxpert {
        return false;
    }
    let Some(d) = &a.dimension else {
        return false;
    };
    let driven = d.dimension.as_ref().is_some_and(|i| i.driven);
    !d.owner_feature.is_empty() && !d.reference && !driven
}

/// Whether any of the annotation's attachments is `persist_id`, or (in an
/// assembly) is in that component.
fn attached_to(a: &proto::AnnotationInfo, persist_id: &[u8]) -> bool {
    a.attached
        .iter()
        .any(|e| e.persist_id == persist_id || e.component_persist_id == persist_id)
}

fn attached_json(ids: &mut IdRegistry, a: &proto::AnnotationInfo, obj: &mut Map<String, Value>) {
    use proto::AttachedKind as K;
    let mut attached = Vec::new();
    let mut components = Vec::new();
    let mut others = 0;
    for e in &a.attached {
        let kind = match e.kind() {
            K::Face => Some(Kind::Face),
            K::Edge => Some(Kind::Edge),
            K::Feature => Some(Kind::Feature),
            _ => None,
        };
        match kind.and_then(|k| ids.id_for(k, &e.persist_id)) {
            Some(id) => attached.push(Value::String(id)),
            None if e.kind() == K::Vertex => attached.push(json!("vertex")),
            None => others += 1,
        }
        if let Some(c) = ids.id_for(Kind::Component, &e.component_persist_id) {
            let c = Value::String(c);
            if !components.contains(&c) {
                components.push(c);
            }
        }
    }
    if !attached.is_empty() {
        obj.insert("attached".into(), Value::Array(attached));
    }
    if others > 0 {
        obj.insert("attached_other".into(), json!(others));
    }
    if !components.is_empty() {
        obj.insert("components".into(), Value::Array(components));
    }
}

/// A value in mm (or degrees for angles), rounded.
pub fn value_of(value: f64, angular: bool) -> f64 {
    if angular { round(value.to_degrees(), 4) } else { mm(value) }
}

/// A dimension's fields as inspect_annotations / inspect_drawing show them
/// (name, value, tolerance, ...).
pub fn dimension_fields(d: &proto::AnnotationDimension) -> Map<String, Value> {
    let mut obj = Map::new();
    dimension_json(d, &mut obj);
    obj
}

fn dimension_json(d: &proto::AnnotationDimension, obj: &mut Map<String, Value>) {
    use proto::DimensionType as T;
    let unit = |angular: bool| if angular { "deg" } else { "mm" };
    let mut angular = false;
    let mut nominal = None;
    if let Some(info) = &d.dimension {
        if !info.name.is_empty() {
            obj.insert("dimension".into(), json!(info.name));
        }
        let measures = match info.r#type() {
            T::Linear => "length",
            T::Angular => "angle",
            T::Radius => "radius",
            T::Diameter => "diameter",
            T::ArcLength => "arc_length",
            T::Ordinate => "ordinate",
            T::Other | T::Unspecified => "other",
        };
        obj.insert("measures".into(), json!(measures));
        angular = info.r#type() == T::Angular;
        nominal = Some(info.value);
        obj.insert(format!("value_{}", unit(angular)), json!(value_of(info.value, angular)));
        if info.driven {
            obj.insert("driven".into(), json!(true));
        }
    }
    if !d.owner_feature.is_empty() {
        obj.insert("feature".into(), json!(d.owner_feature));
    }
    if d.reference {
        obj.insert("reference".into(), json!(true));
    }
    if d.hole_callout {
        obj.insert("hole_callout".into(), json!(true));
    }
    if d.inspection {
        obj.insert("inspection".into(), json!(true));
    }
    for (key, text) in [
        ("prefix", &d.prefix),
        ("suffix", &d.suffix),
        ("callout_above", &d.callout_above),
        ("callout_below", &d.callout_below),
    ] {
        if !text.trim().is_empty() {
            obj.insert(key.into(), json!(text));
        }
    }

    let Some(tol_type) = tolerance_name(d.tolerance_type) else {
        return;
    };
    let u = unit(angular);
    let mut tol = Map::new();
    tol.insert("type".into(), json!(tol_type));
    // SolidWorks reports the tolerance as the upper (max) and lower (min)
    // deviation from the nominal value.
    let (upper, lower) = (d.tolerance_max, d.tolerance_min);
    match d.tolerance_type {
        // Symmetric: one value, shown as +/-; take whichever side was set.
        4 => {
            let half = upper.abs().max(lower.abs());
            tol.insert(format!("plus_minus_{u}"), json!(value_of(half, angular)));
            if let Some(n) = nominal {
                tol.insert(
                    format!("limits_{u}"),
                    json!([value_of(n - half, angular), value_of(n + half, angular)]),
                );
            }
        }
        2 | 3 | 5 | 6 | 8 => {
            tol.insert(format!("upper_{u}"), json!(value_of(upper, angular)));
            tol.insert(format!("lower_{u}"), json!(value_of(lower, angular)));
            if let Some(n) = nominal {
                tol.insert(
                    format!("limits_{u}"),
                    json!([value_of(n + lower, angular), value_of(n + upper, angular)]),
                );
            }
        }
        // Block / general (ISO 2768): no values of their own; DimXpert's
        // limits for them when it has them.
        10 | 11 => {
            if let (Some(hi), Some(lo)) = (d.general_upper_limit, d.general_lower_limit) {
                if let Some(n) = nominal {
                    if ((hi - n) - (n - lo)).abs() < 1e-9 {
                        tol.insert(format!("plus_minus_{u}"), json!(value_of(hi - n, angular)));
                    }
                }
                tol.insert(format!("limits_{u}"), json!([value_of(lo, angular), value_of(hi, angular)]));
            }
        }
        _ => {}
    }
    if matches!(d.tolerance_type, 7..=9) {
        if !d.hole_fit.is_empty() {
            tol.insert("hole_fit".into(), json!(d.hole_fit));
        }
        if !d.shaft_fit.is_empty() {
            tol.insert("shaft_fit".into(), json!(d.shaft_fit));
        }
    }
    obj.insert("tolerance".into(), Value::Object(tol));
}

/// A datum reference as shown: SolidWorks folds a datum's material
/// condition into its text as a symbol code ("B<MOD-MMC>"), which the
/// condition already says.
fn datum_text(datum: &str) -> String {
    let mut out = String::new();
    let mut code = false;
    for c in datum.chars() {
        match c {
            '<' => code = true,
            '>' if code => code = false,
            _ if !code => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// One row of a feature control frame as a readable callout, e.g.
/// "position | dia 0.1 (M) | A | B | C".
pub fn frame_callout(f: &proto::GtolFrame) -> String {
    let mut parts = vec![characteristic_name(f.symbol).unwrap_or("?").to_string()];
    for (value, diameter, condition) in [
        (&f.tolerance1, f.tolerance1_diameter, f.tolerance1_condition),
        (&f.tolerance2, f.tolerance2_diameter, f.tolerance2_condition),
    ] {
        if value.trim().is_empty() {
            continue;
        }
        let mut cell = String::new();
        if diameter || condition == 6 {
            cell += "dia ";
        } else if condition == 7 {
            cell += "S dia ";
        }
        cell += value.trim();
        if let Some(mark) = condition_mark(condition) {
            cell += " ";
            cell += mark;
        }
        parts.push(cell);
    }
    for (i, datum) in f.datums.iter().enumerate() {
        let mut cell = datum_text(datum);
        if cell.is_empty() {
            continue;
        }
        if let Some(mark) = f.datum_conditions.get(i).and_then(|c| condition_mark(*c)) {
            cell += " ";
            cell += mark;
        }
        parts.push(cell);
    }
    parts.join(" | ")
}

fn frame_json(f: &proto::GtolFrame) -> Value {
    let mut obj = Map::new();
    obj.insert("callout".into(), json!(frame_callout(f)));
    if let Some(name) = characteristic_name(f.symbol) {
        obj.insert("characteristic".into(), json!(name));
    }
    let mut add_tolerance = |key: &str, value: &str, diameter: bool, condition: i32| {
        if value.trim().is_empty() {
            return;
        }
        let mut t = Map::new();
        t.insert("value".into(), json!(value.trim()));
        if diameter || condition == 6 {
            t.insert("diameter_zone".into(), json!(true));
        }
        if let Some(c) = condition_name(condition).filter(|c| *c != "diameter") {
            t.insert("modifier".into(), json!(c));
        }
        obj.insert(key.into(), Value::Object(t));
    };
    add_tolerance("tolerance", &f.tolerance1, f.tolerance1_diameter, f.tolerance1_condition);
    add_tolerance("tolerance2", &f.tolerance2, f.tolerance2_diameter, f.tolerance2_condition);
    let datums: Vec<Value> = f
        .datums
        .iter()
        .enumerate()
        .map(|(i, d)| (i, datum_text(d)))
        .filter(|(_, d)| !d.is_empty())
        .map(|(i, d)| match f.datum_conditions.get(i).and_then(|c| condition_name(*c)) {
            Some(m) => json!({ "datum": d, "modifier": m }),
            None => json!(d),
        })
        .collect();
    if !datums.is_empty() {
        obj.insert("datums".into(), Value::Array(datums));
    }
    Value::Object(obj)
}

fn gtol_json(g: &proto::AnnotationGtol, obj: &mut Map<String, Value>) {
    if !g.frames.is_empty() {
        obj.insert("frames".into(), Value::Array(g.frames.iter().map(frame_json).collect()));
    }
    if !g.datum_label.trim().is_empty() {
        obj.insert("datum_label".into(), json!(g.datum_label.trim()));
    }
    if g.composite {
        obj.insert("composite".into(), json!(true));
    }
    if g.all_around {
        obj.insert("all_around".into(), json!(true));
    }
    if !g.projected_zone_height.trim().is_empty() {
        obj.insert("projected_zone_height".into(), json!(g.projected_zone_height.trim()));
    }
}

pub fn annotation_json(ids: &mut IdRegistry, a: &proto::AnnotationInfo) -> Value {
    let mut obj = annotation_fields(a);
    attached_json(ids, a, &mut obj);
    if let Some(p) = &a.position {
        obj.insert("position_mm".into(), point_mm(p));
    }
    Value::Object(obj)
}

/// Everything annotation_json shows except what it's attached to and
/// where it is, which depend on whether it's a 3D annotation or one on a
/// drawing sheet.
pub fn annotation_fields(a: &proto::AnnotationInfo) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("type".into(), json!(type_name(a.r#type)));
    if !a.name.is_empty() {
        obj.insert("name".into(), json!(a.name));
    }
    if a.dimxpert {
        obj.insert("dimxpert".into(), json!(true));
        if !a.dimxpert_feature.is_empty() {
            obj.insert("dimxpert_feature".into(), json!(a.dimxpert_feature));
        }
    }
    match a.r#type {
        sw::DIMENSION => {
            if let Some(d) = &a.dimension {
                dimension_json(d, &mut obj);
                // DimXpert dimensions are all driven; saying so is noise.
                if a.dimxpert {
                    obj.remove("driven");
                }
            }
        }
        sw::GTOL => {
            if let Some(g) = &a.gtol {
                gtol_json(g, &mut obj);
            }
        }
        sw::DATUM_TAG if !a.datum_label.trim().is_empty() => {
            obj.insert("label".into(), json!(a.datum_label.trim()));
        }
        sw::SF_SYMBOL => {
            if let Some(s) = &a.surface_finish {
                crate::gdt::surface_finish_json(s, &mut obj);
            }
        }
        8 => {
            if let Some(w) = &a.weld {
                crate::gdt::weld_json(w, &mut obj);
            }
        }
        _ => {}
    }
    // Raw display text: for notes it's the content; for the rest it fills
    // in what the structured fields don't cover. A dimension's is mostly
    // the "<DIM>" placeholder, so it's only shown when it says more.
    let text: Vec<&String> = a
        .text
        .iter()
        .filter(|t| !t.trim().is_empty())
        .filter(|t| a.r#type != sw::DIMENSION || t.trim() != "<DIM>")
        .collect();
    if !text.is_empty() {
        obj.insert("text".into(), json!(text));
    }
    match a.visibility {
        2 => {
            obj.insert("visibility".into(), json!("half_hidden"));
        }
        3 => {
            obj.insert("visibility".into(), json!("hidden"));
        }
        _ => {}
    }
    if a.dangling {
        obj.insert("dangling".into(), json!(true));
    }
    obj
}

/// The inspect_annotations JSON. `on` is a resolved persist id.
pub fn render(
    ids: &mut IdRegistry,
    reply: &proto::InspectAnnotationsResponse,
    params: &InspectAnnotationsParams,
    on: Option<&[u8]>,
) -> Value {
    let mut out = Map::new();
    if let Some(doc) = &reply.document {
        out.insert("document".into(), json!(doc.title));
        if doc.r#type() == proto::DocumentType::Assembly {
            out.insert("document_type".into(), json!("assembly"));
        }
    }

    let (feature_dims, annotations): (Vec<_>, Vec<_>) =
        reply.annotations.iter().partition(|a| is_feature_dimension(a));
    let listed: Vec<&proto::AnnotationInfo> = annotations
        .into_iter()
        .chain(feature_dims.iter().copied().filter(|_| params.include_feature_dimensions))
        .collect();

    // What there is, before the kind/on filters, so the model knows what to
    // ask for.
    let mut summary = Map::new();
    for a in &listed {
        let key = type_name(a.r#type);
        let n = summary.get(key).and_then(Value::as_u64).unwrap_or(0);
        summary.insert(key.into(), json!(n + 1));
    }
    let dimxpert = listed.iter().filter(|a| a.dimxpert).count();
    if dimxpert > 0 {
        summary.insert("from_dimxpert".into(), json!(dimxpert));
    }
    out.insert("summary".into(), Value::Object(summary));

    let matching: Vec<&proto::AnnotationInfo> = listed
        .into_iter()
        .filter(|a| params.kind.is_none_or(|k| kind_of(a) == k))
        .filter(|a| on.is_none_or(|pid| attached_to(a, pid)))
        .collect();
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let shown: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(limit)
        .map(|a| annotation_json(ids, a))
        .collect();
    let end = offset + shown.len();
    if params.kind.is_some() || on.is_some() {
        out.insert("matching".into(), json!(matching.len()));
    }
    if end < matching.len() {
        out.insert(
            "annotations_note".into(),
            json!(format!(
                "showing annotations {}-{} of {}; pass offset={} for more",
                offset + 1,
                end,
                matching.len(),
                end
            )),
        );
    }
    out.insert("annotations".into(), Value::Array(shown));

    if !reply.components.is_empty() {
        let components = reply
            .components
            .iter()
            .map(|c| crate::assembly::component_json(ids, c))
            .collect();
        out.insert("component_list".into(), Value::Array(components));
    }

    let mut notes = Vec::new();
    if reply.annotations.is_empty() {
        notes.push(
            "this document has no 3D annotations (PMI) -- they're added with the MBD Dimension \
             (DimXpert) tools or the annotation tools in SolidWorks"
                .to_string(),
        );
    }
    if !feature_dims.is_empty() && !params.include_feature_dimensions {
        notes.push(format!(
            "{} feature/sketch dimension(s) not listed; pass include_feature_dimensions=true to see them",
            feature_dims.len()
        ));
    }
    if reply.truncated {
        notes.push("the document has more annotations than the add-in reports at once".to_string());
    }
    if !notes.is_empty() {
        out.insert("note".into(), json!(notes.join("; ")));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dim(name: &str, value: f64, tol_type: i32, max: f64, min: f64) -> proto::AnnotationInfo {
        proto::AnnotationInfo {
            name: name.into(),
            r#type: sw::DIMENSION,
            dimxpert: true,
            dimxpert_feature: "Simple Hole1".into(),
            attached: vec![proto::AttachedEntity {
                kind: proto::AttachedKind::Face as i32,
                persist_id: b"hole-face".to_vec(),
                component_persist_id: vec![],
            }],
            dimension: Some(proto::AnnotationDimension {
                dimension: Some(proto::DimensionInfo {
                    name: "DimXpert-D1".into(),
                    r#type: proto::DimensionType::Diameter as i32,
                    value,
                    driven: true,
                }),
                tolerance_type: tol_type,
                tolerance_max: max,
                tolerance_min: min,
                prefix: "4X ".into(),
                ..Default::default()
            }),
            text: vec!["<DIM>".into()],
            ..Default::default()
        }
    }

    #[test]
    fn dimension_with_bilateral_tolerance() {
        let mut ids = IdRegistry::default();
        let a = dim("D1", 0.010, 2, 0.0001, -0.00005);
        let v = annotation_json(&mut ids, &a);
        assert_eq!(v["type"], "dimension");
        assert_eq!(v["measures"], "diameter");
        assert_eq!(v["value_mm"], 10.0);
        assert_eq!(v["tolerance"]["type"], "bilateral");
        assert_eq!(v["tolerance"]["upper_mm"], 0.1);
        assert_eq!(v["tolerance"]["lower_mm"], -0.05);
        assert_eq!(v["tolerance"]["limits_mm"], json!([9.95, 10.1]));
        assert_eq!(v["prefix"], "4X ");
        assert_eq!(v["attached"], json!(["face:1"]));
        assert_eq!(v["dimxpert_feature"], "Simple Hole1");
        // The bare "<DIM>" placeholder isn't worth showing.
        assert!(v.get("text").is_none());
    }

    #[test]
    fn symmetric_tolerance_uses_either_side() {
        let mut ids = IdRegistry::default();
        let v = annotation_json(&mut ids, &dim("D1", 0.020, 4, 0.0002, 0.0));
        assert_eq!(v["tolerance"]["plus_minus_mm"], 0.2);
        assert_eq!(v["tolerance"]["limits_mm"], json!([19.8, 20.2]));
    }

    #[test]
    fn gtol_frame_callout() {
        let frame = proto::GtolFrame {
            symbol: 23,
            tolerance1: "0.1".into(),
            tolerance1_diameter: true,
            tolerance1_condition: 1,
            datums: vec!["A".into(), "B".into(), "".into()],
            datum_conditions: vec![0, 1, 0],
            ..Default::default()
        };
        assert_eq!(frame_callout(&frame), "position | dia 0.1 (M) | A | B (M)");
        // SolidWorks folds a datum's condition into its text.
        let folded = proto::GtolFrame { datums: vec!["A".into(), "B<MOD-MMC>".into(), "".into()], ..frame.clone() };
        assert_eq!(frame_callout(&folded), "position | dia 0.1 (M) | A | B (M)");
        let v = frame_json(&frame);
        assert_eq!(v["characteristic"], "position");
        assert_eq!(v["tolerance"]["diameter_zone"], true);
        assert_eq!(v["tolerance"]["modifier"], "MMC");
        assert_eq!(v["datums"], json!(["A", { "datum": "B", "modifier": "MMC" }]));

        let flatness = proto::GtolFrame {
            symbol: 15,
            tolerance1: "0.02".into(),
            datums: vec!["".into(), "".into(), "".into()],
            ..Default::default()
        };
        assert_eq!(frame_callout(&flatness), "flatness | 0.02");
        assert!(frame_json(&flatness).get("datums").is_none());
    }

    #[test]
    fn hides_feature_dimensions_and_filters() {
        let mut ids = IdRegistry::default();
        let face = ids.id_for(Kind::Face, b"hole-face").unwrap();
        let sketch_dim = proto::AnnotationInfo {
            r#type: sw::DIMENSION,
            dimension: Some(proto::AnnotationDimension {
                dimension: Some(proto::DimensionInfo {
                    name: "D1@Sketch1".into(),
                    value: 0.05,
                    ..Default::default()
                }),
                owner_feature: "Sketch1".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let datum = proto::AnnotationInfo {
            r#type: sw::DATUM_TAG,
            datum_label: "A".into(),
            attached: vec![proto::AttachedEntity {
                kind: proto::AttachedKind::Face as i32,
                persist_id: b"bottom".to_vec(),
                component_persist_id: vec![],
            }],
            visibility: 3,
            ..Default::default()
        };
        let reply = proto::InspectAnnotationsResponse {
            annotations: vec![sketch_dim, dim("D2", 0.01, 0, 0.0, 0.0), datum],
            ..Default::default()
        };
        let params = InspectAnnotationsParams::default();
        let out = render(&mut ids, &reply, &params, None);
        assert_eq!(out["annotations"].as_array().unwrap().len(), 2);
        assert_eq!(out["summary"], json!({ "dimension": 1, "datum": 1, "from_dimxpert": 1 }));
        assert!(out["note"].as_str().unwrap().contains("include_feature_dimensions"));
        assert_eq!(out["annotations"][1]["label"], "A");
        assert_eq!(out["annotations"][1]["visibility"], "hidden");
        // No tolerance type: no tolerance object.
        assert!(out["annotations"][0].get("tolerance").is_none());

        let params = InspectAnnotationsParams { include_feature_dimensions: true, ..Default::default() };
        let out = render(&mut ids, &reply, &params, None);
        assert_eq!(out["annotations"].as_array().unwrap().len(), 3);
        assert!(out.get("note").is_none());

        let params = InspectAnnotationsParams { kind: Some(AnnotationKind::Datum), ..Default::default() };
        let out = render(&mut ids, &reply, &params, None);
        assert_eq!(out["matching"], 1);
        assert_eq!(out["annotations"][0]["type"], "datum");

        let pid = ids.persist_id(&face).unwrap();
        let params = InspectAnnotationsParams::default();
        let out = render(&mut ids, &reply, &params, Some(&pid));
        assert_eq!(out["matching"], 1);
        assert_eq!(out["annotations"][0]["attached"], json!([face]));
    }

    #[test]
    fn empty_document_says_how_to_add_pmi() {
        let mut ids = IdRegistry::default();
        let reply = proto::InspectAnnotationsResponse::default();
        let out = render(&mut ids, &reply, &InspectAnnotationsParams::default(), None);
        assert!(out["note"].as_str().unwrap().contains("MBD Dimension"));
        assert_eq!(out["annotations"], json!([]));
    }
}
