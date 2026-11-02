//! The undo tool: its parameters and the reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::delete::type_label;
use crate::inspect::MM_PER_M;

/// The add-in's limit.
const MAX_STEPS: i32 = 100;

fn default_steps() -> i32 {
    1
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UndoParams {
    /// How many of SolidWorks' undo steps to take back, 1 to 100. Defaults to 1. These aren't
    /// quite tool calls: a new feature or a change of dimension is 1, but a failed call may have
    /// left steps too. It never stops inside a sketch: it goes on until the sketch is closed.
    #[serde(default = "default_steps")]
    pub steps: i32,
}

pub fn request(params: &UndoParams) -> Result<proto::UndoRequest, McpError> {
    if !(1..=MAX_STEPS).contains(&params.steps) {
        return Err(McpError::invalid_params(
            format!("steps must be between 1 and {MAX_STEPS}, got {}", params.steps),
            None,
        ));
    }
    Ok(proto::UndoRequest { steps: params.steps })
}

fn feature_list(features: &[proto::FeatureRef]) -> String {
    features
        .iter()
        .map(|f| format!("{} ({})", f.name, type_label(&f.type_name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "undid 1 step in Part1: removed Boss-Extrude1 (boss-extrude); the solid
/// volume went from 4000 to 0 mm^3. ..."
pub fn reply_text(reply: &proto::UndoResponse) -> String {
    let mut changes = Vec::new();
    if !reply.removed.is_empty() {
        changes.push(format!("removed {}", feature_list(&reply.removed)));
    }
    if !reply.restored.is_empty() {
        changes.push(format!("restored {}", feature_list(&reply.restored)));
    }
    let (before, after) = (reply.volume_before * MM_PER_M.powi(3), reply.volume_after * MM_PER_M.powi(3));
    if (before - after).abs() >= 0.001 {
        changes.push(format!("the solid volume went from {before:.3} to {after:.3} mm^3"));
    }
    let steps = if reply.steps_taken == 1 { "1 step".to_string() } else { format!("{} steps", reply.steps_taken) };
    if changes.is_empty() {
        return format!(
            "undid {steps} in {}, but the feature tree and solid volume are as they were: that was a change \
             they don't show (inside a sketch, hiding a body; check with inspect_model or inspect_sketch), or \
             there was nothing left to undo. Undo more steps if what you wanted back is still there.",
            reply.document
        );
    }
    format!(
        "undid {steps} in {}: {}. Ids of what changed may no longer work: inspect_model again before using them.",
        reply.document,
        changes.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(name: &str, type_name: &str) -> proto::FeatureRef {
        proto::FeatureRef { name: name.into(), type_name: type_name.into() }
    }

    #[test]
    fn steps_default_and_range() {
        let params: UndoParams = serde_json::from_str("{}").unwrap();
        assert_eq!(request(&params).unwrap().steps, 1);
        for bad in [0, -1, 101] {
            assert!(request(&UndoParams { steps: bad }).is_err(), "{bad}");
        }
        assert!(request(&UndoParams { steps: 100 }).is_ok());
    }

    #[test]
    fn replies() {
        let reply = proto::UndoResponse {
            document: "Part1".into(),
            removed: vec![feature("Boss-Extrude1", "Extrusion")],
            volume_before: 4e-6,
            steps_taken: 1,
            ..Default::default()
        };
        let text = reply_text(&reply);
        assert!(text.starts_with("undid 1 step in Part1: removed Boss-Extrude1 (boss-extrude); the solid volume went from 4000.000 to 0.000 mm^3"), "{text}");

        let reply = proto::UndoResponse {
            document: "Part1".into(),
            restored: vec![feature("Fillet1", "Fillet")],
            steps_taken: 2,
            ..Default::default()
        };
        assert!(reply_text(&reply).starts_with("undid 2 steps in Part1: restored Fillet1 (fillet)."));

        let reply = proto::UndoResponse { document: "Part1".into(), steps_taken: 1, ..Default::default() };
        assert!(reply_text(&reply).contains("are as they were"));
    }
}
