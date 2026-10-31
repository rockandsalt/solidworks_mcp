//! Sketch entities, dimensions and relations: short IDs for sketch entities
//! ("line:3", "point:7") and the JSON/text the sketch tools reply with.
//!
//! A sketch entity is identified on the wire by a proto::SketchItemRef (its
//! sketch plus SolidWorks' two-number sketch entity ID). The encoded message
//! goes into the same IdRegistry as model persist IDs, so the same entity
//! always gets the same short ID, and the ID decodes back to the ref.

use prost::Message as _;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind, MM_PER_M, SKETCH_KINDS, mm, round};

/// Most not-yet-defined entities named in a one-line summary.
const SUMMARY_ITEMS: usize = 12;

fn item_kind(t: proto::SketchItemType) -> Kind {
    use proto::SketchItemType as T;
    match t {
        T::Point => Kind::SketchPoint,
        T::Line => Kind::Line,
        T::Arc => Kind::Arc,
        T::Circle => Kind::Circle,
        T::Spline => Kind::Spline,
        T::Ellipse => Kind::Ellipse,
        T::Other | T::Unspecified => Kind::Segment,
    }
}

/// The short ID for a sketch entity.
pub fn ref_id(ids: &mut IdRegistry, kind: Kind, r: &proto::SketchItemRef) -> Option<String> {
    ids.id_for(kind, &r.encode_to_vec())
}

fn point_id(ids: &mut IdRegistry, r: Option<&proto::SketchItemRef>) -> Option<String> {
    r.and_then(|r| ref_id(ids, Kind::SketchPoint, r))
}

pub fn item_id(ids: &mut IdRegistry, item: &proto::SketchItem) -> Option<String> {
    ref_id(ids, item_kind(item.r#type()), item.r#ref.as_ref()?)
}

/// The SketchItemRef behind a sketch entity ID, if `id` is one we handed out.
pub fn decode_ref(ids: &IdRegistry, id: &str) -> Option<proto::SketchItemRef> {
    let kind = Kind::of_id(id)?;
    if !SKETCH_KINDS.contains(&kind) {
        return None;
    }
    proto::SketchItemRef::decode(ids.persist_id(id)?.as_slice()).ok()
}

pub fn status_name(status: proto::DefinitionStatus) -> &'static str {
    use proto::DefinitionStatus as S;
    match status {
        S::Under => "under defined",
        S::Fully => "fully defined",
        S::Over => "over defined",
        S::NoSolution => "unsolvable",
        S::Invalid => "invalid",
        S::NotSolved => "not solved",
        S::Unspecified => "unknown",
    }
}

fn type_name(t: proto::SketchItemType) -> &'static str {
    use proto::SketchItemType as T;
    match t {
        T::Point => "point",
        T::Line => "line",
        T::Arc => "arc",
        T::Circle => "circle",
        T::Spline => "spline",
        T::Ellipse => "ellipse",
        T::Other | T::Unspecified => "other",
    }
}

/// A sketch point (meters) in millimeters: [x, y], or [x, y, z] in a 3D sketch.
fn position_mm(v: &proto::Vec3, is_3d: bool) -> Value {
    if is_3d {
        json!([mm(v.x), mm(v.y), mm(v.z)])
    } else {
        json!([mm(v.x), mm(v.y)])
    }
}

/// One sketch entity. Positions are in the sketch's own 2D coordinates, or model coordinates
/// in a 3D sketch.
pub fn item_json(ids: &mut IdRegistry, item: &proto::SketchItem, is_3d: bool) -> Value {
    use proto::SketchItemType as T;
    let mut obj = Map::new();
    obj.insert("id".into(), item_id(ids, item).map_or(Value::Null, Value::String));
    let t = item.r#type();
    obj.insert("type".into(), json!(type_name(t)));
    if item.construction {
        obj.insert("construction".into(), json!(true));
    }
    let status = item.status();
    if !matches!(status, proto::DefinitionStatus::Fully | proto::DefinitionStatus::Unspecified) {
        obj.insert("status".into(), json!(status_name(status)));
    }
    // A point field: its ID under `key`, its position under `{key}_mm`.
    let mut point = |obj: &mut Map<String, Value>,
                     key: &str,
                     r: Option<&proto::SketchItemRef>,
                     at: Option<&proto::Vec3>| {
        if let Some(id) = point_id(ids, r) {
            obj.insert(key.to_string(), json!(id));
        }
        if let Some(at) = at {
            obj.insert(format!("{key}_mm"), position_mm(at, is_3d));
        }
    };
    match t {
        T::Point => {
            if let Some(p) = &item.position {
                obj.insert("at_mm".into(), position_mm(p, is_3d));
            }
        }
        T::Circle => {
            point(&mut obj, "center", item.center_point.as_ref(), item.center.as_ref());
            obj.insert("radius_mm".into(), json!(mm(item.radius)));
        }
        T::Arc => {
            point(&mut obj, "center", item.center_point.as_ref(), item.center.as_ref());
            point(&mut obj, "start", item.start_point.as_ref(), item.start.as_ref());
            point(&mut obj, "end", item.end_point.as_ref(), item.end.as_ref());
            obj.insert("radius_mm".into(), json!(mm(item.radius)));
        }
        T::Ellipse => {
            point(&mut obj, "center", item.center_point.as_ref(), item.center.as_ref());
            point(&mut obj, "major", item.major_point.as_ref(), item.major.as_ref());
            point(&mut obj, "minor", item.minor_point.as_ref(), item.minor.as_ref());
            point(&mut obj, "start", item.start_point.as_ref(), item.start.as_ref());
            point(&mut obj, "end", item.end_point.as_ref(), item.end.as_ref());
            if let (Some(c), Some(major), Some(minor)) = (&item.center, &item.major, &item.minor) {
                let from_center = |p: &proto::Vec3| (p.x - c.x).hypot(p.y - c.y);
                obj.insert("major_radius_mm".into(), json!(mm(from_center(major))));
                obj.insert("minor_radius_mm".into(), json!(mm(from_center(minor))));
            }
        }
        _ => {
            point(&mut obj, "start", item.start_point.as_ref(), item.start.as_ref());
            point(&mut obj, "end", item.end_point.as_ref(), item.end.as_ref());
        }
    }
    if t != T::Point && item.length > 0.0 {
        obj.insert("length_mm".into(), json!(mm(item.length)));
    }
    if !item.name.is_empty() {
        obj.insert("name".into(), json!(item.name));
    }
    Value::Object(obj)
}

/// "line:1 (point:1 -> point:2)", "circle:1 (center point:5)" -- for the
/// replies of the drawing tools.
pub fn item_summary(ids: &mut IdRegistry, item: &proto::SketchItem) -> Option<String> {
    use proto::SketchItemType as T;
    let id = item_id(ids, item)?;
    let start = point_id(ids, item.start_point.as_ref());
    let end = point_id(ids, item.end_point.as_ref());
    let center = point_id(ids, item.center_point.as_ref());
    Some(match (item.r#type(), start, end, center) {
        (T::Arc | T::Ellipse, Some(s), Some(e), Some(c)) => format!("{id} ({s} -> {e}, center {c})"),
        (T::Circle | T::Ellipse, _, _, Some(c)) => format!("{id} (center {c})"),
        (_, Some(s), Some(e), _) => format!("{id} ({s} -> {e})"),
        _ => id,
    })
}

/// Lists what a drawing tool created, e.g. " Ids: line:1 (point:1 -> point:2), ...".
pub fn items_text(ids: &mut IdRegistry, items: &[proto::SketchItem]) -> String {
    let parts: Vec<String> = items.iter().filter_map(|item| item_summary(ids, item)).collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" Ids: {}.", parts.join(", "))
    }
}

fn dimension_type_name(t: proto::DimensionType) -> &'static str {
    use proto::DimensionType as D;
    match t {
        D::Linear => "linear",
        D::Angular => "angle",
        D::Radius => "radius",
        D::Diameter => "diameter",
        D::ArcLength => "arc length",
        D::Ordinate => "ordinate",
        D::Other | D::Unspecified => "other",
    }
}

pub fn dimension_json(d: &proto::DimensionInfo) -> Value {
    let mut obj = Map::new();
    obj.insert("name".into(), json!(d.name));
    obj.insert("type".into(), json!(dimension_type_name(d.r#type())));
    if d.r#type() == proto::DimensionType::Angular {
        obj.insert("value_deg".into(), json!(round(d.value.to_degrees(), 4)));
    } else {
        obj.insert("value_mm".into(), json!(mm(d.value)));
    }
    if d.driven {
        obj.insert("driven".into(), json!(true));
    }
    Value::Object(obj)
}

/// "D1@Sketch1 = 50 mm", "D3@Sketch1 = 30°".
pub fn dimension_text(d: &proto::DimensionInfo) -> String {
    let value = if d.r#type() == proto::DimensionType::Angular {
        format!("{}°", round(d.value.to_degrees(), 4))
    } else {
        format!("{} mm", mm(d.value))
    };
    let driven = if d.driven { " (driven)" } else { "" };
    format!("{} = {value}{driven}", d.name)
}

/// swConstraintType_e as a readable name.
pub fn relation_name(constraint_type: i32) -> String {
    match constraint_type {
        4 | 25 => "horizontal",
        5 | 26 => "vertical",
        6 => "tangent",
        7 => "parallel",
        8 => "perpendicular",
        9 => "coincident",
        10 => "concentric",
        11 => "symmetric",
        12 => "midpoint",
        13 => "intersection",
        14 => "equal",
        15 => "equal diameter",
        16 => "offset",
        17 => "fixed",
        27 => "collinear",
        28 => "coradial",
        32 => "converted edge",
        42 => "merge",
        47 => "sketch offset",
        48 | 51 | 64 | 66 => "along x",
        49 | 52 | 65 | 67 => "along y",
        50 | 53 => "along z",
        57 => "patterned",
        n => return format!("other (type {n})"),
    }
    .to_string()
}

/// The short ID for a sketch entity a relation names. The ref doesn't say
/// what kind of segment it is, so this reuses the ID the entity already got
/// when it was listed ("line:3"), else mints a generic one.
pub fn relation_entity_id(ids: &mut IdRegistry, item: &proto::SketchItemRef) -> Option<String> {
    let bytes = item.encode_to_vec();
    let kind = if item.point { Kind::SketchPoint } else { Kind::Segment };
    ids.existing_id(&bytes).or_else(|| ids.id_for(kind, &bytes))
}

fn relation_json(ids: &mut IdRegistry, r: &proto::RelationInfo) -> Value {
    let mut entities: Vec<Value> = r
        .items
        .iter()
        .filter_map(|item| relation_entity_id(ids, item).map(Value::String))
        .collect();
    for _ in 0..r.external_count {
        entities.push(json!("external"));
    }
    json!({ "type": relation_name(r.constraint_type), "entities": entities })
}

pub fn render_inspect(ids: &mut IdRegistry, reply: &proto::InspectSketchResponse) -> Value {
    let mut out = Map::new();
    out.insert("sketch".into(), json!(reply.sketch_name));
    out.insert("status".into(), json!(status_name(reply.status())));
    if reply.is_3d {
        out.insert("is_3d".into(), json!(true));
    }
    if let Some(frame) = &reply.frame
        && let (Some(o), Some(x), Some(y), Some(n)) = (&frame.origin, &frame.x_axis, &frame.y_axis, &frame.normal)
    {
        out.insert(
            "sketch_axes".into(),
            json!({
                "origin_mm": crate::inspect::point_mm(o),
                "x": crate::inspect::direction(x),
                "y": crate::inspect::direction(y),
                "normal": crate::inspect::direction(n),
            }),
        );
    }
    if let Some(c) = &reply.contours {
        out.insert("contours".into(), json!({ "closed": c.closed, "open": c.open }));
    }
    // Segments first, so their IDs are minted with the right kind before
    // relations look them up.
    let (points, segments): (Vec<_>, Vec<_>) = reply
        .items
        .iter()
        .partition(|item| item.r#type() == proto::SketchItemType::Point);
    let segments: Vec<Value> = segments.iter().map(|item| item_json(ids, item, reply.is_3d)).collect();
    let points: Vec<Value> = points.iter().map(|item| item_json(ids, item, reply.is_3d)).collect();
    out.insert("segments".into(), Value::Array(segments));
    out.insert("points".into(), Value::Array(points));
    out.insert(
        "dimensions".into(),
        Value::Array(reply.dimensions.iter().map(dimension_json).collect()),
    );
    out.insert(
        "relations".into(),
        Value::Array(reply.relations.iter().map(|r| relation_json(ids, r)).collect()),
    );
    if reply.truncated {
        out.insert(
            "warning".into(),
            json!("the sketch has more entities than the add-in lists at once; some are missing"),
        );
    }
    Value::Object(out)
}

/// One sentence on how defined the sketch is now, naming what can still move.
pub fn state_text(ids: &mut IdRegistry, sketch_name: &str, state: Option<&proto::SketchState>) -> String {
    let Some(state) = state else {
        return String::new();
    };
    let status = state.status();
    let mut text = format!(" {sketch_name} is {}", status_name(status));
    let named: Vec<String> = state
        .not_fully_defined
        .iter()
        .take(SUMMARY_ITEMS)
        .filter_map(|item| item_id(ids, item))
        .collect();
    let total = state.not_fully_defined_count.max(named.len() as i32) as usize;
    if status == proto::DefinitionStatus::Fully || named.is_empty() {
        text.push('.');
        return text;
    }
    let more = if total > named.len() {
        format!(" and {} more", total - named.len())
    } else {
        String::new()
    };
    text += &format!("; not fully defined yet: {}{more}.", named.join(", "));
    text += if state.is_3d {
        " Add dimensions/relations for what matters."
    } else {
        " Add dimensions/relations for what matters, then fully_define_sketch to lock the rest."
    };
    text
}

/// What a dimension or relation attaches to: a sketch entity, the sketch
/// origin, or model geometry (an edge or face).
pub enum RefTarget {
    Item(proto::SketchItemRef),
    Origin,
    Model(Vec<u8>),
}

impl RefTarget {
    pub fn into_proto(self) -> proto::SketchRef {
        use proto::sketch_ref::Target;
        proto::SketchRef {
            target: Some(match self {
                RefTarget::Item(r) => Target::Item(r),
                RefTarget::Origin => Target::Origin(true),
                RefTarget::Model(bytes) => Target::ModelPersistId(bytes),
            }),
        }
    }
}

/// Millimeters -> meters.
pub fn m(mm: f64) -> f64 {
    mm / MM_PER_M
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(point: bool, id1: i64) -> proto::SketchItemRef {
        proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point, id0: 0, id1, ..Default::default() }
    }

    #[test]
    fn sketch_ids_round_trip() {
        let mut ids = IdRegistry::default();
        let line = proto::SketchItem {
            r#ref: Some(r(false, 7)),
            r#type: proto::SketchItemType::Line as i32,
            start_point: Some(r(true, 1)),
            end_point: Some(r(true, 2)),
            start: Some(proto::Vec3 { x: 0.0, y: 0.0, z: 0.0 }),
            end: Some(proto::Vec3 { x: 0.05, y: 0.0, z: 0.0 }),
            length: 0.05,
            status: proto::DefinitionStatus::Under as i32,
            ..Default::default()
        };
        let json = item_json(&mut ids, &line, false);
        assert_eq!(json["id"], "line:1");
        assert_eq!(json["start"], "point:1");
        assert_eq!(json["end_mm"], json!([50.0, 0.0]));
        assert_eq!(item_json(&mut ids, &line, true)["end_mm"], json!([50.0, 0.0, 0.0]));
        assert_eq!(json["status"], "under defined");
        assert_eq!(decode_ref(&ids, "line:1").unwrap(), r(false, 7));
        assert_eq!(decode_ref(&ids, "point:2").unwrap(), r(true, 2));
        // Model ids and unknown ids aren't sketch refs.
        assert!(decode_ref(&ids, "line:9").is_none());
        ids.id_for(Kind::Face, b"face-bytes");
        assert!(decode_ref(&ids, "face:1").is_none());
        assert_eq!(item_summary(&mut ids, &line).unwrap(), "line:1 (point:1 -> point:2)");
    }

    #[test]
    fn describes_dimensions_and_state() {
        let angle = proto::DimensionInfo {
            name: "D2@Sketch1".into(),
            r#type: proto::DimensionType::Angular as i32,
            value: std::f64::consts::FRAC_PI_6,
            driven: false,
        };
        assert_eq!(dimension_text(&angle), "D2@Sketch1 = 30°");
        assert_eq!(dimension_json(&angle)["value_deg"], 30.0);

        let mut ids = IdRegistry::default();
        let state = proto::SketchState {
            status: proto::DefinitionStatus::Under as i32,
            not_fully_defined: vec![proto::SketchItem {
                r#ref: Some(r(true, 3)),
                r#type: proto::SketchItemType::Point as i32,
                ..Default::default()
            }],
            not_fully_defined_count: 4,
            is_3d: false,
        };
        let text = state_text(&mut ids, "Sketch1", Some(&state));
        assert!(text.contains("Sketch1 is under defined"), "{text}");
        assert!(text.contains("point:1 and 3 more"), "{text}");
        assert!(text.contains("fully_define_sketch"), "{text}");
        let state_3d = proto::SketchState { is_3d: true, ..state.clone() };
        let text = state_text(&mut ids, "3DSketch1", Some(&state_3d));
        assert!(!text.contains("fully_define_sketch"), "{text}");
        let fully = proto::SketchState { status: proto::DefinitionStatus::Fully as i32, ..Default::default() };
        assert_eq!(state_text(&mut ids, "Sketch1", Some(&fully)), " Sketch1 is fully defined.");
    }
}
