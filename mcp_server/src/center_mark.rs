//! The center mark and centerline tools: add_center_marks, add_centerlines.
//!
//! SolidWorks makes the marks and lines and keeps them on their geometry.
//! What happens here: ids into requests, what can be told from the sheet
//! geometry inspect_drawing cached (a center mark needs a circle or arc seen
//! face-on, a centerline two parallel lines, a circular group circles that
//! aren't in a row), and the replies.

use glam::DVec2;
use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::Value;

use crate::addin::proto;
use crate::drawing::{self, Cache, Shape};
use crate::inspect::IdRegistry;
use crate::note;

/// The add-in's limit, per call.
const MAX_ENTITIES: usize = 200;
/// Sines of angles below this are parallel, or in a row.
const ALIGNED: f64 = 1e-3;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CenterMarkGroup {
    /// One mark over holes in a row, joined by a line through their centers.
    Linear,
    /// One mark over holes on a common circle.
    Circular,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CenterMarkAuto {
    Holes,
    Fillets,
    Slots,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AddCenterMarksParams {
    /// The drawing view. Not needed with `on`, whose ids say it.
    #[serde(default)]
    pub view: Option<String>,
    /// Circles and arcs seen face-on, edge ids from inspect_drawing with the view
    /// ("view_edge:3"): one mark each, unless `group`. Empty: every one of `throughout` in the
    /// view.
    #[serde(default)]
    pub on: Vec<String>,
    /// One mark over all of `on`, replacing their own marks: linear (at least 2 holes) or
    /// circular (at least 3).
    #[serde(default)]
    pub group: Option<CenterMarkGroup>,
    /// Without `on`: what to mark throughout the view: holes (default), fillets, slots.
    #[serde(default)]
    pub throughout: Vec<CenterMarkAuto>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AddCenterlinesParams {
    /// The drawing view. Not needed with `between`, whose ids say it.
    #[serde(default)]
    pub view: Option<String>,
    /// Pairs of parallel straight edges or silhouettes from inspect_drawing with the view, one
    /// centerline midway between each: [["silhouette:1", "silhouette:2"]] for a cylinder seen
    /// side-on. Empty: SolidWorks' own, on the view's cylinders.
    #[serde(default)]
    pub between: Vec<[String; 2]>,
}

/// The view all of `refs` are in, which `view` must agree with if given.
fn common_view(view: &str, refs: &[(&str, &proto::DrawingEntityRef)]) -> Result<String, McpError> {
    let mut out = view.to_string();
    for (id, r) in refs {
        if out.is_empty() {
            out = r.view_name.clone();
        } else if out != r.view_name {
            return Err(invalid(format!("{id} is in {}, not {out}: one view per call", r.view_name)));
        }
    }
    if out.is_empty() {
        return Err(invalid("say which view (`view`, or the ids of its entities)"));
    }
    Ok(out)
}

fn circle_center(shape: &Shape) -> Option<DVec2> {
    match shape {
        Shape::Circle { center, .. } | Shape::Arc { center, .. } => Some(*center),
        _ => None,
    }
}

/// Whether all `points` lie on one line.
fn in_a_row(points: &[DVec2]) -> bool {
    let Some(far) = points.iter().copied().max_by(|a, b| {
        points[0].distance_squared(*a).total_cmp(&points[0].distance_squared(*b))
    }) else {
        return true;
    };
    let along = far - points[0];
    let length = along.length();
    length < 1e-9 || points.iter().all(|p| along.perp_dot(*p - points[0]).abs() <= ALIGNED * length * length)
}

pub fn marks_request(ids: &IdRegistry, cache: &Cache, params: &AddCenterMarksParams) -> Result<proto::AddCenterMarksRequest, McpError> {
    if params.on.len() > MAX_ENTITIES {
        return Err(invalid(format!("at most {MAX_ENTITIES} circles per call")));
    }
    if !params.on.is_empty() && !params.throughout.is_empty() {
        return Err(invalid("give on (these circles) or throughout (all of a kind in the view), not both"));
    }
    if params.group.is_some() && params.on.is_empty() {
        return Err(invalid("group goes with on: the circles it's over"));
    }
    let refs = params.on.iter().map(|id| drawing::decode_ref(ids, id)).collect::<Result<Vec<_>, _>>()?;
    let named: Vec<(&str, &proto::DrawingEntityRef)> = params.on.iter().map(String::as_str).zip(&refs).collect();
    let view_name = common_view(&text(params.view.as_deref()), &named)?;
    let mut centers = Vec::new();
    for id in &params.on {
        if let Some(geometry) = cache.entity(id) {
            match circle_center(&geometry.shape) {
                Some(c) => centers.push(c),
                None => return Err(invalid(format!("{id} isn't a circle or arc seen face-on: a center mark goes on one"))),
            }
        }
    }
    let least = match params.group {
        Some(CenterMarkGroup::Linear) => 2,
        Some(CenterMarkGroup::Circular) => 3,
        None => 1,
    };
    if !params.on.is_empty() && params.on.len() < least {
        return Err(invalid(format!("that group needs at least {least} circles")));
    }
    // SolidWorks gives single marks instead, with a mark at no center among
    // them (seen 2026-11-01).
    if params.group == Some(CenterMarkGroup::Circular) && centers.len() == params.on.len() && in_a_row(&centers) {
        return Err(invalid("those circles are in a row: a circular group goes over circles on a common circle; use group=linear"));
    }
    let style = match params.group {
        Some(CenterMarkGroup::Linear) => 3,
        Some(CenterMarkGroup::Circular) => 4,
        None if params.on.is_empty() => 0,
        None => 2,
    };
    let auto_types = params.throughout.iter().fold(0, |flags, kind| {
        flags
            | match kind {
                CenterMarkAuto::Holes => 1,
                CenterMarkAuto::Fillets => 2,
                CenterMarkAuto::Slots => 4,
            }
    });
    Ok(proto::AddCenterMarksRequest { view_name, circles: refs, style, auto_types })
}

pub fn lines_request(ids: &IdRegistry, cache: &Cache, params: &AddCenterlinesParams) -> Result<proto::AddCenterlinesRequest, McpError> {
    if params.between.len() > MAX_ENTITIES {
        return Err(invalid(format!("at most {MAX_ENTITIES} centerlines per call")));
    }
    let mut view_name = text(params.view.as_deref());
    let mut pairs = Vec::new();
    for [a, b] in &params.between {
        let (ra, rb) = (drawing::decode_ref(ids, a)?, drawing::decode_ref(ids, b)?);
        view_name = common_view(&view_name, &[(a, &ra), (b, &rb)])?;
        if a == b {
            return Err(invalid(format!("a centerline goes between 2 entities, not {a} twice")));
        }
        if let (Some(ga), Some(gb)) = (cache.entity(a), cache.entity(b)) {
            let (Shape::Line { a: a0, b: a1 }, Shape::Line { a: b0, b: b1 }) = (&ga.shape, &gb.shape) else {
                return Err(invalid(format!("{a} and {b}: a centerline goes between straight edges or silhouettes")));
            };
            let (da, db) = (*a1 - *a0, *b1 - *b0);
            if da.perp_dot(db).abs() > ALIGNED * da.length() * db.length() {
                return Err(invalid(format!("{a} and {b} aren't parallel on the sheet")));
            }
        }
        pairs.push(proto::CenterlinePair { entities: vec![ra, rb] });
    }
    if view_name.is_empty() {
        return Err(invalid("say which view (`view`, or the ids of its entities)"));
    }
    Ok(proto::AddCenterlinesRequest { view_name, pairs })
}

/// The reply to both tools. `asked`: what each already_marked index stands for.
pub fn reply(
    ids: &mut IdRegistry,
    what: &str,
    view: &str,
    asked: &[String],
    reply: &proto::AddCenterMarksResponse,
) -> String {
    let sheet = reply.sheet.clone().unwrap_or_default();
    let frames = note::frames(&sheet);
    let names: Vec<&str> = reply.added.iter().map(|a| a.name.as_str()).collect();
    let mut lines = vec![if names.is_empty() {
        format!("no new {what} in {view}.")
    } else {
        format!("added {} {what} in {view} on {}: {}.", names.len(), sheet.name, names.join(", "))
    }];
    if !reply.already_marked.is_empty() {
        let already: Vec<&str> =
            reply.already_marked.iter().filter_map(|i| asked.get(*i as usize)).map(String::as_str).collect();
        lines.push(format!("Already had one, left as it is: {}.", already.join(", ")));
    }
    if reply.duplicates_removed > 0 {
        lines.push(format!("{} circle(s) had a mark already and kept it.", reply.duplicates_removed));
    }
    if !reply.replaced.is_empty() {
        lines.push(format!("The group replaced the circles' own marks: {}.", reply.replaced.join(", ")));
    }
    let entries: Vec<Value> = reply.added.iter().map(|a| note::annotation_entry(ids, &frames, a)).collect();
    lines.push(Value::Array(entries).to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circles_in_a_row() {
        let p = |x, y| DVec2::new(x, y);
        assert!(in_a_row(&[p(0.0, 0.0), p(0.02, 0.0), p(0.04, 0.0)]));
        assert!(in_a_row(&[p(0.02, 0.01), p(0.0, 0.0), p(0.04, 0.02)]));
        assert!(!in_a_row(&[p(0.02, 0.0), p(0.0, 0.02), p(-0.02, 0.0)]));
    }

    #[test]
    fn needs_a_view() {
        let ids = IdRegistry::default();
        let cache = Cache::default();
        let err = marks_request(&ids, &cache, &AddCenterMarksParams::default()).unwrap_err();
        assert!(err.message.contains("which view"));
        let err = lines_request(&ids, &cache, &AddCenterlinesParams::default()).unwrap_err();
        assert!(err.message.contains("which view"));
    }

    #[test]
    fn auto_flags() {
        let ids = IdRegistry::default();
        let cache = Cache::default();
        let r = marks_request(
            &ids,
            &cache,
            &AddCenterMarksParams {
                view: Some("Drawing View1".into()),
                throughout: vec![CenterMarkAuto::Holes, CenterMarkAuto::Slots],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((r.auto_types, r.style), (5, 0));
    }

    #[test]
    fn a_group_needs_its_circles() {
        let ids = IdRegistry::default();
        let cache = Cache::default();
        let err = marks_request(
            &ids,
            &cache,
            &AddCenterMarksParams {
                view: Some("Drawing View1".into()),
                group: Some(CenterMarkGroup::Linear),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.message.contains("group goes with on"));
    }
}
