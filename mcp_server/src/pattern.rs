//! The pattern tools (linear_pattern_features, circular_pattern_features,
//! linear_pattern_sketch_entities, circular_pattern_sketch_entities): their parameters, the
//! direction / axis references, and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::delete::problems_text;
use crate::inspect::{IdRegistry, Kind, SKETCH_KINDS};
use crate::mirror::{body_id, feature_target, sketch_ref, unique};
use crate::refgeom::{self, Shape};
use crate::sketch;

/// The add-in's limits.
const MAX_FEATURES: usize = 500;
const MAX_SKETCH_ENTITIES: usize = 1000;
const MAX_INSTANCES: u64 = 10000;

const SKETCH_TOOLS: &str = "linear_pattern_sketch_entities / circular_pattern_sketch_entities";

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinearPatternParams {
    /// Features to pattern, by feature-tree name ("Cut-Extrude1") or inspect_model id
    /// ("feature:4"). Not with bodies.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whole solid bodies to pattern, by inspect_model id ("body:1"). Not with features.
    #[serde(default)]
    pub bodies: Vec<String>,
    /// The direction to pattern along: a straight edge ("edge:7"), a sketch line ("line:3"),
    /// a reference axis ("Axis1" or "feature:4"), or a planar face ("face:3") or plane
    /// ("front", "Plane1") to pattern along its normal.
    pub direction: String,
    /// Instances along direction, the original included (at least 2).
    pub count: u32,
    /// From one instance to the next along direction, in mm.
    pub spacing_mm: f64,
    /// Pattern the other way along direction. Defaults to false.
    #[serde(default)]
    pub reverse_direction: bool,
    /// A second direction for a grid of instances, same kinds of reference as direction.
    #[serde(default)]
    pub direction2: Option<String>,
    /// Instances along direction2, the original included (at least 2). Required with
    /// direction2.
    #[serde(default)]
    pub count2: Option<u32>,
    /// From one instance to the next along direction2, in mm. Required with direction2.
    #[serde(default)]
    pub spacing2_mm: Option<f64>,
    /// Pattern the other way along direction2. Defaults to false.
    #[serde(default)]
    pub reverse_direction2: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CircularPatternParams {
    /// Features to pattern, by feature-tree name ("Cut-Extrude1") or inspect_model id
    /// ("feature:4"). Not with bodies.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whole solid bodies to pattern, by inspect_model id ("body:1"). Not with features.
    #[serde(default)]
    pub bodies: Vec<String>,
    /// What to pattern round: a reference axis ("Axis1" or "feature:4"), a straight edge or
    /// a circular edge (round its center line) ("edge:7"), a sketch line ("line:3"), or a
    /// cylindrical or conical face ("face:3", round its axis, e.g. a flange's bore).
    pub axis: String,
    /// Instances, the original included (at least 2).
    pub count: u32,
    /// The angle in degrees the instances are spread evenly over, more than 0 and at most
    /// 360. Defaults to 360: all the way round with equal gaps. Not with spacing_deg.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// The angle in degrees from one instance to the next, instead of angle_deg; the
    /// instances must fit in one turn.
    #[serde(default)]
    pub spacing_deg: Option<f64>,
    /// Pattern the other way round the axis. Defaults to false.
    #[serde(default)]
    pub reverse_direction: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinearSketchPatternParams {
    /// Sketch entity ids to copy ("line:3", "circle:1", "point:7") from inspect_sketch or the
    /// sketch_* tools, all in one sketch.
    pub entities: Vec<String>,
    /// Instances along the first direction, the originals included (at least 1; the
    /// pattern needs 2 or more instances in all).
    pub count: u32,
    /// From one instance to the next along the first direction, in mm. Required when count
    /// is more than 1.
    #[serde(default)]
    pub spacing_mm: Option<f64>,
    /// The first direction, in degrees counterclockwise from the sketch's x axis. Defaults
    /// to 0 (+x); 180 is -x.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// Rows along a second direction, for a grid, the originals' row included. Defaults to 1
    /// (no second direction).
    #[serde(default)]
    pub count2: Option<u32>,
    /// From one row to the next, in mm. Required when count2 is more than 1.
    #[serde(default)]
    pub spacing2_mm: Option<f64>,
    /// The second direction, in degrees counterclockwise from the sketch's x axis. Defaults
    /// to 90 (+y); must not be parallel to the first.
    #[serde(default)]
    pub angle2_deg: Option<f64>,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CircularSketchPatternParams {
    /// Sketch entity ids to copy ("line:3", "circle:1", "point:7") from inspect_sketch or the
    /// sketch_* tools, all in one sketch.
    pub entities: Vec<String>,
    /// The point to pattern round, [x, y] in sketch coordinates (mm). Defaults to the sketch
    /// origin [0, 0].
    #[serde(default)]
    pub center: Option<[f64; 2]>,
    /// Instances, the originals included (at least 2).
    pub count: u32,
    /// The angle in degrees the instances are spread evenly over, more than 0 and at most
    /// 360. Defaults to 360: all the way round with equal gaps. Not with spacing_deg.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// The angle in degrees from one instance to the next, instead of angle_deg; the
    /// instances must fit in one turn.
    #[serde(default)]
    pub spacing_deg: Option<f64>,
    /// Go clockwise instead of counterclockwise (seen with the sketch's x axis to the right
    /// and y up). Defaults to false.
    #[serde(default)]
    pub reverse_direction: bool,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

/// The features or bodies of a pattern request, as wire targets / persist ids.
fn seeds(
    ids: &IdRegistry,
    features: &[String],
    bodies: &[String],
) -> Result<(Vec<proto::FeatureTarget>, Vec<Vec<u8>>), McpError> {
    let features = unique(features);
    let bodies = unique(bodies);
    match (features.is_empty(), bodies.is_empty()) {
        (true, true) => return Err(invalid("give the features or the bodies to pattern")),
        (false, false) => {
            return Err(invalid(
                "give features or bodies, not both -- pattern the features in one call and the bodies \
                 in another",
            ));
        }
        _ => {}
    }
    if features.len() + bodies.len() > MAX_FEATURES {
        return Err(invalid(format!("at most {MAX_FEATURES} features or bodies per call")));
    }
    let features = features
        .iter()
        .enumerate()
        .map(|(i, f)| feature_target(ids, f, &format!("feature {}", i + 1), SKETCH_TOOLS))
        .collect::<Result<_, _>>()?;
    let bodies = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| body_id(ids, b, &format!("body {}", i + 1)))
        .collect::<Result<_, _>>()?;
    Ok((features, bodies))
}

fn count(count: u32, what: &str) -> Result<i32, McpError> {
    if !(2..=MAX_INSTANCES).contains(&u64::from(count)) {
        return Err(invalid(format!(
            "{what} must be 2 to {MAX_INSTANCES} (the original included), got {count}"
        )));
    }
    Ok(count as i32)
}

fn spacing(spacing_mm: Option<f64>, what: &str) -> Result<f64, McpError> {
    match spacing_mm {
        Some(s) if s.is_finite() && s > 0.0 => Ok(sketch::m(s)),
        Some(s) => Err(invalid(format!("{what} must be more than 0, got {s}"))),
        None => Err(invalid(format!("give {what}"))),
    }
}

/// A linear pattern's direction string -> its wire ref. Points and arcs can't be directions.
fn direction_ref(ids: &IdRegistry, direction: &str, what: &str) -> Result<proto::GeometryRef, McpError> {
    let (r, shape) = refgeom::geometry_ref(ids, direction, what)?;
    match shape {
        Shape::Edge | Shape::Segment | Shape::Feature | Shape::Surface => Ok(r),
        Shape::Round | Shape::Point => Err(invalid(format!(
            "{what}: {} isn't a direction -- give a straight edge, a sketch line, a reference axis, or a \
             planar face or plane to pattern along its normal",
            direction.trim()
        ))),
    }
}

/// A circular pattern's axis string -> its wire ref. Points, planes and sketch arcs can't be
/// axes.
fn axis_ref(ids: &IdRegistry, axis: &str) -> Result<proto::GeometryRef, McpError> {
    let (r, shape) = refgeom::geometry_ref(ids, axis, "axis")?;
    match (&r.target, shape) {
        (Some(proto::geometry_ref::Target::StandardPlane(_)), _) => Err(invalid(format!(
            "axis: {} is a plane -- give a reference axis, a straight or circular edge, a sketch line \
             or a cylindrical face",
            axis.trim()
        ))),
        (_, Shape::Edge | Shape::Segment | Shape::Feature | Shape::Surface) => Ok(r),
        (_, Shape::Round) => Err(invalid(format!(
            "axis: {} is a sketch arc or circle -- give a reference axis, a straight or circular edge, \
             a sketch line or a cylindrical face (for the center of a sketch circle, \
             create_reference_axis through it first)",
            axis.trim()
        ))),
        (_, Shape::Point) => Err(invalid(format!(
            "axis: {} is a point -- give a reference axis, a straight or circular edge, a sketch line \
             or a cylindrical face",
            axis.trim()
        ))),
    }
}

/// The angle between neighbouring instances (degrees) and whether it came from a total
/// spread evenly; `angle_deg` as given. Checked to fit in one turn.
fn round_spacing(count: u32, angle_deg: Option<f64>, spacing_deg: Option<f64>) -> Result<(f64, bool), McpError> {
    match (angle_deg, spacing_deg) {
        (Some(_), Some(_)) => Err(invalid("give angle_deg or spacing_deg, not both")),
        (None, Some(s)) => {
            if !(s.is_finite() && s > 0.0) {
                return Err(invalid(format!("spacing_deg must be more than 0, got {s}")));
            }
            if s * f64::from(count.saturating_sub(1)) >= 360.0 - 1e-9 {
                return Err(invalid(format!(
                    "{count} instances {s} degrees apart don't fit in one turn -- lower count or \
                     spacing_deg, or give angle_deg to spread them evenly"
                )));
            }
            Ok((s, false))
        }
        (angle, None) => {
            let a = angle.unwrap_or(360.0);
            if !(a.is_finite() && a > 0.0 && a <= 360.0) {
                return Err(invalid(format!("angle_deg must be more than 0 and at most 360, got {a}")));
            }
            // All the way round, the last instance stops a gap short of the first.
            let gaps = if a >= 360.0 { count } else { count.saturating_sub(1) };
            Ok((a / f64::from(gaps.max(1)), true))
        }
    }
}

pub fn linear_request(ids: &IdRegistry, params: &LinearPatternParams) -> Result<proto::PatternFeaturesRequest, McpError> {
    let (features, bodies) = seeds(ids, &params.features, &params.bodies)?;
    let direction1 = proto::PatternDirection {
        direction: Some(direction_ref(ids, &params.direction, "direction")?),
        count: count(params.count, "count")?,
        spacing: spacing(Some(params.spacing_mm), "spacing_mm")?,
        reverse: params.reverse_direction,
    };
    let direction2 = match params.direction2.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => Some(proto::PatternDirection {
            direction: Some(direction_ref(ids, d, "direction2")?),
            count: count(params.count2.ok_or_else(|| invalid("give count2 with direction2"))?, "count2")?,
            spacing: spacing(params.spacing2_mm, "spacing2_mm")?,
            reverse: params.reverse_direction2,
        }),
        None if params.count2.is_some() || params.spacing2_mm.is_some() || params.reverse_direction2 => {
            return Err(invalid("count2, spacing2_mm and reverse_direction2 need direction2"));
        }
        None => None,
    };
    let instances = u64::from(params.count) * u64::from(params.count2.filter(|_| direction2.is_some()).unwrap_or(1));
    if instances > MAX_INSTANCES {
        return Err(invalid(format!("at most {MAX_INSTANCES} instances in all, got {instances}")));
    }
    Ok(proto::PatternFeaturesRequest {
        features,
        bodies,
        pattern: Some(proto::pattern_features_request::Pattern::Linear(proto::LinearFeaturePattern {
            direction1: Some(direction1),
            direction2,
        })),
    })
}

pub fn circular_request(
    ids: &IdRegistry,
    params: &CircularPatternParams,
) -> Result<proto::PatternFeaturesRequest, McpError> {
    let (features, bodies) = seeds(ids, &params.features, &params.bodies)?;
    let count = count(params.count, "count")?;
    let (spacing_deg, even) = round_spacing(params.count, params.angle_deg, params.spacing_deg)?;
    // SolidWorks takes the total for evenly spread instances, else the step.
    let angle_deg = if even { params.angle_deg.unwrap_or(360.0) } else { spacing_deg };
    Ok(proto::PatternFeaturesRequest {
        features,
        bodies,
        pattern: Some(proto::pattern_features_request::Pattern::Circular(proto::CircularFeaturePattern {
            axis: Some(axis_ref(ids, &params.axis)?),
            count,
            angle: angle_deg.to_radians(),
            equal_spacing: even,
            reverse: params.reverse_direction,
        })),
    })
}

/// The given features / bodies, trimmed and once each, for error legends.
pub fn given(features: &[String], bodies: &[String]) -> Vec<String> {
    let list = if features.is_empty() { bodies } else { features };
    unique(list).into_iter().map(str::to_string).collect()
}

/// A reference as the reply names it: faces, edges and sketch entities by the id they were
/// given, the rest by what the add-in calls them ("Right Plane", "Axis1").
fn named(given: &str, described: &str) -> String {
    let given = given.trim();
    match Kind::of_id(given) {
        Some(Kind::Face | Kind::Edge) => given.to_string(),
        Some(kind) if SKETCH_KINDS.contains(&kind) => given.to_string(),
        _ => described.to_string(),
    }
}

/// A length in mm or an angle in degrees for the reply, without float noise.
fn number(v: f64) -> String {
    format!("{}", (v * 1e6).round() / 1e6)
}

fn copies_note(body_count: i32) -> String {
    let bodies = match body_count {
        1 => "1 solid body".to_string(),
        n => format!("{n} solid bodies"),
    };
    format!(
        " The part has {bodies}. The copies' faces and edges have no ids yet, and faces the pattern \
         joined may have new ones: inspect_model again before using face or edge ids."
    )
}

/// "created LPattern1: 4 instances of Cut-Extrude1 along edge:7, 10 mm apart. ..."
pub fn linear_text(params: &LinearPatternParams, reply: &proto::PatternFeaturesResponse) -> String {
    let reversed = |r: bool| if r { " (reversed)" } else { "" };
    let mut text = format!(
        "created {}: {} instances of {} along {}{}, {} mm apart",
        reply.feature_name,
        params.count,
        reply.patterned,
        named(&params.direction, &reply.direction1),
        reversed(params.reverse_direction),
        number(params.spacing_mm)
    );
    if let (Some(d2), Some(count2), Some(spacing2)) = (&params.direction2, params.count2, params.spacing2_mm) {
        text += &format!(
            ", times {count2} along {}{}, {} mm apart ({} in all)",
            named(d2, &reply.direction2),
            reversed(params.reverse_direction2),
            number(spacing2),
            params.count * count2
        );
    }
    text + "." + &copies_note(reply.body_count)
}

/// "created CirPattern1: 6 instances of Cut-Extrude1 round Axis1, 60 degrees apart. ..."
pub fn circular_text(params: &CircularPatternParams, reply: &proto::PatternFeaturesResponse) -> String {
    let (step, even) = round_spacing(params.count, params.angle_deg, params.spacing_deg).unwrap_or((0.0, true));
    let spread = match params.angle_deg {
        Some(a) if even && a < 360.0 => format!(", spread over {} degrees", number(a)),
        _ => String::new(),
    };
    format!(
        "created {}: {} instances of {} round {}{}, {} degrees apart{spread}.{}",
        reply.feature_name,
        params.count,
        reply.patterned,
        named(&params.axis, &reply.direction1),
        if params.reverse_direction { " (reversed)" } else { "" },
        number(step),
        copies_note(reply.body_count)
    )
}

fn sketch_items(ids: &IdRegistry, entities: &[String]) -> Result<Vec<proto::SketchItemRef>, McpError> {
    let entities = unique(entities);
    if entities.is_empty() || entities.len() > MAX_SKETCH_ENTITIES {
        return Err(invalid(format!("give between 1 and {MAX_SKETCH_ENTITIES} sketch entities")));
    }
    entities
        .iter()
        .enumerate()
        .map(|(i, e)| {
            sketch_ref(
                ids,
                e,
                &format!("entity {}", i + 1),
                "linear_pattern_features / circular_pattern_features",
            )
        })
        .collect()
}

fn sketch_name(name: &Option<String>) -> String {
    name.as_deref().map(str::trim).unwrap_or_default().to_string()
}

pub fn linear_sketch_request(
    ids: &IdRegistry,
    params: &LinearSketchPatternParams,
) -> Result<proto::PatternSketchEntitiesRequest, McpError> {
    let items = sketch_items(ids, &params.entities)?;
    let count2 = params.count2.unwrap_or(1);
    let instances = u64::from(params.count) * u64::from(count2);
    if params.count == 0 || count2 == 0 || !(2..=MAX_INSTANCES).contains(&instances) {
        return Err(invalid(format!(
            "count x count2 must be 2 to {MAX_INSTANCES} instances (the originals included), got {} x {count2}",
            params.count
        )));
    }
    let angle = params.angle_deg.unwrap_or(0.0);
    let angle2 = params.angle2_deg.unwrap_or(90.0);
    if !angle.is_finite() || !angle2.is_finite() {
        return Err(invalid("angle_deg and angle2_deg must be numbers"));
    }
    let spacing1 = if params.count > 1 { spacing(params.spacing_mm, "spacing_mm")? } else { 0.0 };
    let spacing2 = if count2 > 1 {
        let between = (angle2 - angle).rem_euclid(180.0);
        if between < 1e-6 || 180.0 - between < 1e-6 {
            return Err(invalid(format!(
                "angle2_deg ({angle2}) is parallel to angle_deg ({angle}) -- the second direction must \
                 cross the first"
            )));
        }
        spacing(params.spacing2_mm, "spacing2_mm")?
    } else if params.spacing2_mm.is_some() || params.angle2_deg.is_some() {
        return Err(invalid("spacing2_mm and angle2_deg need count2 of 2 or more"));
    } else {
        0.0
    };
    Ok(proto::PatternSketchEntitiesRequest {
        sketch_name: sketch_name(&params.sketch_name),
        items,
        pattern: Some(proto::pattern_sketch_entities_request::Pattern::Linear(proto::LinearSketchPattern {
            count: params.count as i32,
            spacing: spacing1,
            angle: angle.to_radians(),
            count2: count2 as i32,
            spacing2,
            angle2: angle2.to_radians(),
        })),
    })
}

pub fn circular_sketch_request(
    ids: &IdRegistry,
    params: &CircularSketchPatternParams,
) -> Result<proto::PatternSketchEntitiesRequest, McpError> {
    let items = sketch_items(ids, &params.entities)?;
    let count = count(params.count, "count")?;
    let (step, _) = round_spacing(params.count, params.angle_deg, params.spacing_deg)?;
    let [x, y] = params.center.unwrap_or([0.0, 0.0]);
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("center must be two numbers, [x, y] in mm"));
    }
    let step = if params.reverse_direction { -step } else { step };
    Ok(proto::PatternSketchEntitiesRequest {
        sketch_name: sketch_name(&params.sketch_name),
        items,
        pattern: Some(proto::pattern_sketch_entities_request::Pattern::Circular(proto::CircularSketchPattern {
            center: Some(proto::Vec3 { x: sketch::m(x), y: sketch::m(y), z: 0.0 }),
            count,
            spacing: step.to_radians(),
        })),
    })
}

/// The given entities, trimmed and once each, for error legends.
pub fn given_entities(entities: &[String]) -> Vec<String> {
    unique(entities).into_iter().map(str::to_string).collect()
}

/// "patterned circle:1 in Sketch1 (...). New: circle:2 (center point:5), ... Sketch1 now has
/// 3 closed and 0 open contours. ..."
pub fn sketch_text(
    ids: &mut IdRegistry,
    entities: &[String],
    how: &str,
    reply: &proto::PatternSketchEntitiesResponse,
) -> String {
    let new: Vec<String> = reply.items.iter().filter_map(|item| sketch::item_summary(ids, item)).collect();
    let mut text = format!("patterned {} in {} ({how}).", unique(entities).join(", "), reply.sketch_name);
    if !new.is_empty() {
        text += &format!(
            " New: {}. Each copy has a patterned relation to its original; the pattern's \
             spacing isn't dimensioned.",
            new.join(", ")
        );
    }
    if let Some(c) = &reply.contours {
        text += &format!(
            " {} now has {} closed and {} open contours.",
            reply.sketch_name,
            c.closed.max(0),
            c.open.max(0)
        );
    }
    text += &sketch::state_text(ids, &reply.sketch_name, reply.state.as_ref());
    text += &problems_text(&reply.new_errors);
    text
}

/// "3 x 2 instances, 10 mm apart at 0 degrees and 5 mm apart at 90 degrees"
pub fn linear_sketch_how(params: &LinearSketchPatternParams) -> String {
    let count2 = params.count2.unwrap_or(1);
    let along = |spacing: Option<f64>, angle: f64| {
        format!("{} mm apart at {} degrees", number(spacing.unwrap_or(0.0)), number(angle))
    };
    let first = along(params.spacing_mm, params.angle_deg.unwrap_or(0.0));
    if count2 > 1 {
        let second = along(params.spacing2_mm, params.angle2_deg.unwrap_or(90.0));
        if params.count > 1 {
            format!("{} x {count2} instances, {first} and {second}", params.count)
        } else {
            format!("{count2} instances, {second}")
        }
    } else {
        format!("{} instances, {first}", params.count)
    }
}

/// "6 instances round [0, 0], 60 degrees apart counterclockwise"
pub fn circular_sketch_how(params: &CircularSketchPatternParams) -> String {
    let (step, _) = round_spacing(params.count, params.angle_deg, params.spacing_deg).unwrap_or((0.0, true));
    let [x, y] = params.center.unwrap_or([0.0, 0.0]);
    format!(
        "{} instances round [{}, {}], {} degrees apart {}",
        params.count,
        number(x),
        number(y),
        number(step),
        if params.reverse_direction { "clockwise" } else { "counterclockwise" }
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

    fn sketch_id(ids: &mut IdRegistry, kind: Kind, id1: i64) -> (String, proto::SketchItemRef) {
        let r = proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point: kind == Kind::SketchPoint, id0: 0, id1, ..Default::default() };
        (sketch::ref_id(ids, kind, &r).unwrap(), r)
    }

    fn linear(features: &[&str], direction: &str, count: u32, spacing_mm: f64) -> LinearPatternParams {
        LinearPatternParams {
            features: features.iter().map(|f| f.to_string()).collect(),
            bodies: vec![],
            direction: direction.into(),
            count,
            spacing_mm,
            reverse_direction: false,
            direction2: None,
            count2: None,
            spacing2_mm: None,
            reverse_direction2: false,
        }
    }

    fn circular(features: &[&str], axis: &str, count: u32) -> CircularPatternParams {
        CircularPatternParams {
            features: features.iter().map(|f| f.to_string()).collect(),
            bodies: vec![],
            axis: axis.into(),
            count,
            angle_deg: None,
            spacing_deg: None,
            reverse_direction: false,
        }
    }

    fn linear_of(r: proto::PatternFeaturesRequest) -> proto::LinearFeaturePattern {
        match r.pattern {
            Some(proto::pattern_features_request::Pattern::Linear(l)) => l,
            p => panic!("{p:?}"),
        }
    }

    fn circular_of(r: proto::PatternFeaturesRequest) -> proto::CircularFeaturePattern {
        match r.pattern {
            Some(proto::pattern_features_request::Pattern::Circular(c)) => c,
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn linear_features_one_and_two_directions() {
        let mut ids = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let r = linear_request(&ids, &linear(&["Cut-Extrude1", " Cut-Extrude1"], &edge, 4, 10.0)).unwrap();
        assert_eq!(r.features.len(), 1);
        let l = linear_of(r);
        let d1 = l.direction1.unwrap();
        assert_eq!((d1.count, d1.spacing, d1.reverse), (4, 0.01, false));
        assert!(l.direction2.is_none());

        let mut p = linear(&["Cut-Extrude1"], "right", 3, 5.0);
        p.direction2 = Some(face.clone());
        p.count2 = Some(2);
        p.spacing2_mm = Some(7.5);
        p.reverse_direction2 = true;
        let l = linear_of(linear_request(&ids, &p).unwrap());
        let d2 = l.direction2.unwrap();
        assert_eq!((d2.count, d2.spacing, d2.reverse), (2, 0.0075, true));
        assert!(matches!(d2.direction.unwrap().target, Some(proto::geometry_ref::Target::ModelPersistId(p)) if p == b"face"));
    }

    #[test]
    fn linear_feature_refusals() {
        let mut ids = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let vertex = ids.id_for(Kind::Vertex, b"vertex").unwrap();
        let body = ids.id_for(Kind::Body, b"body").unwrap();
        let (circle, _) = sketch_id(&mut ids, Kind::Circle, 1);
        assert!(linear_request(&ids, &linear(&[], &edge, 3, 5.0)).is_err());
        assert!(linear_request(&ids, &linear(&["Cut-Extrude1"], &edge, 1, 5.0)).is_err());
        assert!(linear_request(&ids, &linear(&["Cut-Extrude1"], &edge, 3, 0.0)).is_err());
        assert!(linear_request(&ids, &linear(&["Cut-Extrude1"], &edge, 3, -1.0)).is_err());
        assert!(linear_request(&ids, &linear(&[&body], &edge, 3, 5.0)).is_err());
        for bad in [&vertex, &circle, "origin", ""] {
            assert!(linear_request(&ids, &linear(&["Cut-Extrude1"], bad, 3, 5.0)).is_err(), "{bad}");
        }
        let mut p = linear(&["Cut-Extrude1"], &edge, 3, 5.0);
        p.count2 = Some(2);
        assert!(linear_request(&ids, &p).is_err(), "count2 without direction2");
        p.direction2 = Some(edge.clone());
        assert!(linear_request(&ids, &p).is_err(), "no spacing2");
        p.spacing2_mm = Some(5.0);
        assert!(linear_request(&ids, &p).is_ok());
        p.count = 200;
        p.count2 = Some(200);
        assert!(linear_request(&ids, &p).is_err(), "too many instances");
        let mut p = linear(&[], &edge, 3, 5.0);
        p.bodies = vec![body.clone()];
        assert_eq!(linear_request(&ids, &p).unwrap().bodies, vec![b"body".to_vec()]);
        p.features = vec!["Cut-Extrude1".into()];
        assert!(linear_request(&ids, &p).is_err(), "features and bodies");
    }

    #[test]
    fn circular_features_spread_or_stepped() {
        let mut ids = registry();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let (line, _) = sketch_id(&mut ids, Kind::Line, 1);
        let c = circular_of(circular_request(&ids, &circular(&["Cut-Extrude1"], &face, 6)).unwrap());
        assert!(c.equal_spacing && (c.angle - std::f64::consts::TAU).abs() < 1e-12 && c.count == 6);
        let mut p = circular(&["Cut-Extrude1"], "Axis1", 4);
        p.angle_deg = Some(90.0);
        let c = circular_of(circular_request(&ids, &p).unwrap());
        assert!(c.equal_spacing && (c.angle - 90f64.to_radians()).abs() < 1e-12);
        let mut p = circular(&["Cut-Extrude1"], &line, 3);
        p.spacing_deg = Some(30.0);
        p.reverse_direction = true;
        let c = circular_of(circular_request(&ids, &p).unwrap());
        assert!(!c.equal_spacing && (c.angle - 30f64.to_radians()).abs() < 1e-12 && c.reverse);
    }

    #[test]
    fn circular_feature_refusals() {
        let mut ids = registry();
        let vertex = ids.id_for(Kind::Vertex, b"vertex").unwrap();
        let (circle, _) = sketch_id(&mut ids, Kind::Circle, 1);
        for bad in ["front", &vertex, &circle, "origin"] {
            assert!(circular_request(&ids, &circular(&["Cut-Extrude1"], bad, 4)).is_err(), "{bad}");
        }
        let mut p = circular(&["Cut-Extrude1"], "Axis1", 4);
        p.angle_deg = Some(90.0);
        p.spacing_deg = Some(30.0);
        assert!(circular_request(&ids, &p).is_err(), "both angles");
        p.angle_deg = None;
        p.spacing_deg = Some(120.0);
        assert!(circular_request(&ids, &p).is_err(), "4 x 120 doesn't fit");
        p.spacing_deg = Some(90.0);
        assert!(circular_request(&ids, &p).is_ok(), "4 x 90 fits");
        p.spacing_deg = None;
        p.angle_deg = Some(400.0);
        assert!(circular_request(&ids, &p).is_err());
    }

    #[test]
    fn sketch_patterns() {
        let mut ids = registry();
        let (circle, circle_ref) = sketch_id(&mut ids, Kind::Circle, 1);
        let (point, point_ref) = sketch_id(&mut ids, Kind::SketchPoint, 2);
        let p = LinearSketchPatternParams {
            entities: vec![circle.clone(), point.clone(), circle.clone()],
            count: 3,
            spacing_mm: Some(10.0),
            angle_deg: None,
            count2: Some(2),
            spacing2_mm: Some(5.0),
            angle2_deg: None,
            sketch_name: None,
        };
        let r = linear_sketch_request(&ids, &p).unwrap();
        assert_eq!(r.items, vec![circle_ref.clone(), point_ref]);
        let Some(proto::pattern_sketch_entities_request::Pattern::Linear(l)) = r.pattern else { panic!() };
        assert_eq!((l.count, l.spacing, l.count2, l.spacing2), (3, 0.01, 2, 0.005));
        assert!(l.angle == 0.0 && (l.angle2 - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert_eq!(linear_sketch_how(&p), "3 x 2 instances, 10 mm apart at 0 degrees and 5 mm apart at 90 degrees");

        let bad = |f: &dyn Fn(&mut LinearSketchPatternParams)| {
            let mut q = LinearSketchPatternParams { entities: vec![circle.clone()], count: 3, spacing_mm: Some(10.0), angle_deg: None, count2: None, spacing2_mm: None, angle2_deg: None, sketch_name: None };
            f(&mut q);
            linear_sketch_request(&ids, &q).is_err()
        };
        assert!(!bad(&|_| {}));
        assert!(bad(&|q| q.count = 1));
        assert!(bad(&|q| q.spacing_mm = None));
        assert!(bad(&|q| q.spacing2_mm = Some(5.0)));
        assert!(bad(&|q| { q.count2 = Some(2); q.spacing2_mm = Some(5.0); q.angle2_deg = Some(180.0) }));
        assert!(bad(&|q| q.entities = vec!["Cut-Extrude1".into()]));
        assert!(!bad(&|q| { q.count = 1; q.spacing_mm = None; q.count2 = Some(3); q.spacing2_mm = Some(5.0) }));

        let c = CircularSketchPatternParams {
            entities: vec![circle.clone()],
            center: Some([10.0, -5.0]),
            count: 6,
            angle_deg: None,
            spacing_deg: None,
            reverse_direction: true,
            sketch_name: Some(" Sketch2 ".into()),
        };
        let r = circular_sketch_request(&ids, &c).unwrap();
        assert_eq!(r.sketch_name, "Sketch2");
        let Some(proto::pattern_sketch_entities_request::Pattern::Circular(p)) = r.pattern else { panic!() };
        assert_eq!(p.center, Some(proto::Vec3 { x: 0.01, y: -0.005, z: 0.0 }));
        assert!((p.spacing + 60f64.to_radians()).abs() < 1e-12 && p.count == 6);
        assert_eq!(circular_sketch_how(&c), "6 instances round [10, -5], 60 degrees apart clockwise");
    }

    #[test]
    fn replies() {
        let mut ids = registry();
        let edge = ids.id_for(Kind::Edge, b"edge").unwrap();
        let reply = proto::PatternFeaturesResponse {
            feature_name: "LPattern1".into(),
            patterned: "Cut-Extrude1".into(),
            direction1: "edge".into(),
            direction2: "Right Plane".into(),
            body_count: 1,
        };
        let mut p = linear(&["Cut-Extrude1"], &edge, 4, 10.0);
        assert!(linear_text(&p, &reply).starts_with(&format!(
            "created LPattern1: 4 instances of Cut-Extrude1 along {edge}, 10 mm apart. The part has 1 solid body."
        )));
        p.direction2 = Some("right".into());
        p.count2 = Some(2);
        p.spacing2_mm = Some(2.5);
        assert!(linear_text(&p, &reply).contains(", times 2 along Right Plane, 2.5 mm apart (8 in all)."));

        let reply = proto::PatternFeaturesResponse { feature_name: "CirPattern1".into(), direction1: "Axis1".into(), body_count: 2, ..reply };
        let mut p = circular(&["Cut-Extrude1"], "Axis1", 6);
        assert!(circular_text(&p, &reply).starts_with("created CirPattern1: 6 instances of Cut-Extrude1 round Axis1, 60 degrees apart. The part has 2 solid bodies."));
        p.angle_deg = Some(90.0);
        p.count = 4;
        assert!(circular_text(&p, &reply).contains("round Axis1, 30 degrees apart, spread over 90 degrees."));

        let (circle, _) = sketch_id(&mut ids, Kind::Circle, 1);
        let (_, new_ref) = sketch_id(&mut ids, Kind::Circle, 2);
        let reply = proto::PatternSketchEntitiesResponse {
            sketch_name: "Sketch1".into(),
            items: vec![proto::SketchItem { r#ref: Some(new_ref.clone()), r#type: proto::SketchItemType::Circle as i32, ..Default::default() }],
            contours: Some(proto::SketchContours { closed: 2, open: 0 }),
            state: None,
            new_errors: vec![],
        };
        let text = sketch_text(&mut ids, &[circle.clone()], "2 instances, 10 mm apart at 0 degrees", &reply);
        let new_id = sketch::ref_id(&mut ids, Kind::Circle, &new_ref).unwrap();
        assert!(text.starts_with(&format!("patterned {circle} in Sketch1 (2 instances, 10 mm apart at 0 degrees). New: {new_id}.")), "{text}");
        assert!(text.contains("Sketch1 now has 2 closed and 0 open contours."), "{text}");
    }
}
