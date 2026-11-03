//! The rollback and reorder_feature tools: moving the rollback bar and moving a feature in the
//! tree. See docs/notes/editing.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::delete::{problems_text, type_label};
use crate::edit::feature_target;
use crate::inspect::IdRegistry;

/// Rolled-back features named in a reply; the rest are counted.
const LISTED: usize = 20;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

// ---- rollback ----------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RollbackParams {
    /// Move the bar to just before this feature ("feature:N" from inspect_model, or a name such
    /// as "Fillet1"): it and everything below it are rolled back. Leave out (with `after`
    /// unset) to roll everything forward again.
    #[serde(default)]
    pub before: Option<String>,
    /// Move the bar to just after this feature instead.
    #[serde(default)]
    pub after: Option<String>,
}

pub fn rollback_request(ids: &IdRegistry, params: &RollbackParams) -> Result<proto::RollbackRequest, McpError> {
    match (&params.before, &params.after) {
        (Some(_), Some(_)) => Err(invalid("give before or after, not both")),
        (Some(f), None) => Ok(proto::RollbackRequest { feature: Some(feature_target(ids, f, "feature")?), after: false }),
        (None, Some(f)) => Ok(proto::RollbackRequest { feature: Some(feature_target(ids, f, "feature")?), after: true }),
        (None, None) => Ok(proto::RollbackRequest { feature: None, after: false }),
    }
}

fn names(features: &[proto::FeatureRef]) -> String {
    let mut list: Vec<String> = features
        .iter()
        .take(LISTED)
        .map(|f| match type_label(&f.type_name) {
            "" => f.name.clone(),
            label => format!("{} ({label})", f.name),
        })
        .collect();
    if features.len() > LISTED {
        list.push(format!("and {} more", features.len() - LISTED));
    }
    list.join(", ")
}

/// "rolled back to before Fillet1: Fillet1 (fillet), Cut-Extrude1 (cut-extrude) are left out ..."
pub fn rollback_text(params: &RollbackParams, reply: &proto::RollbackResponse) -> String {
    if reply.rolled_back.is_empty() {
        return if reply.rolled_back_before == 0 {
            "nothing was rolled back, and nothing is: the rollback bar is at the end of the tree.".to_string()
        } else {
            format!(
                "rolled forward to the end of the tree: the {} rolled-back features are back in the model.",
                reply.rolled_back_before
            )
        };
    }
    let place = match (&params.before, &params.after) {
        (Some(f), _) => format!("before {}", f.trim()),
        (_, Some(f)) => format!("after {}", f.trim()),
        _ => "the end".to_string(),
    };
    format!(
        "moved the rollback bar to {place}. Rolled back now, left out of the model until the bar moves \
         down again: {}. New features go in at the bar, above those; rollback with nothing given rolls \
         everything forward. Saving keeps the bar where it is.",
        names(&reply.rolled_back)
    )
}

// ---- reorder_feature ---------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReorderParams {
    /// The feature to move: "feature:N" from inspect_model, or a name such as "Fillet1". What it
    /// absorbed (its sketch) moves with it.
    pub feature: String,
    /// Put it just before this feature.
    #[serde(default)]
    pub before: Option<String>,
    /// Put it just after this feature.
    #[serde(default)]
    pub after: Option<String>,
}

pub fn reorder_request(ids: &IdRegistry, params: &ReorderParams) -> Result<proto::ReorderFeatureRequest, McpError> {
    let (target, after) = match (&params.before, &params.after) {
        (Some(t), None) => (t, false),
        (None, Some(t)) => (t, true),
        _ => return Err(invalid("give exactly one of before or after: the feature to put it next to")),
    };
    let feature = feature_target(ids, &params.feature, "feature to move")?;
    let target = feature_target(ids, target, "feature to put it next to")?;
    if feature == target {
        return Err(invalid("the feature and the one to put it next to are the same"));
    }
    Ok(proto::ReorderFeatureRequest { feature: Some(feature), target: Some(target), after })
}

/// "moved Fillet1 (fillet): now between Boss-Extrude1 and Cut-Extrude1."
pub fn reorder_text(reply: &proto::ReorderFeatureResponse) -> String {
    let moved = reply.moved.as_ref().map(|f| names(std::slice::from_ref(f))).unwrap_or_default();
    let place = match (reply.previous.as_str(), reply.next.as_str()) {
        ("", "") => "it's the only feature".to_string(),
        ("", next) => format!("now first, before {next}"),
        (previous, "") => format!("now last, after {previous}"),
        (previous, next) => format!("now between {previous} and {next}"),
    };
    let mut text = format!("moved {moved}: {place}.");
    text += &problems_text(&reply.new_errors);
    if !reply.new_errors.is_empty() {
        text += " Move it back with reorder_feature if that isn't wanted.";
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::Kind;
    use proto::feature_target::Target;

    fn feature(name: &str, type_name: &str) -> proto::FeatureRef {
        proto::FeatureRef { name: name.into(), type_name: type_name.into() }
    }

    #[test]
    fn rollback_requests() {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        let id = ids.id_for(Kind::Feature, b"fillet").unwrap();
        let p = |before: Option<&str>, after: Option<&str>| RollbackParams {
            before: before.map(str::to_string),
            after: after.map(str::to_string),
        };
        let r = rollback_request(&ids, &p(Some(&id), None)).unwrap();
        assert_eq!(r.feature.unwrap().target, Some(Target::PersistId(b"fillet".to_vec())));
        assert!(!r.after);
        let r = rollback_request(&ids, &p(None, Some(" Cut-Extrude1 "))).unwrap();
        assert_eq!(r.feature.unwrap().target, Some(Target::Name("Cut-Extrude1".into())));
        assert!(r.after);
        assert_eq!(rollback_request(&ids, &p(None, None)).unwrap().feature, None);
        assert!(rollback_request(&ids, &p(Some("A"), Some("B"))).is_err());
        assert!(rollback_request(&ids, &p(Some("face:1"), None)).is_err());
    }

    #[test]
    fn rollback_replies() {
        let p = RollbackParams { before: Some("Fillet1".into()), after: None };
        let reply = proto::RollbackResponse {
            rolled_back: vec![feature("Fillet1", "Fillet"), feature("Cut-Extrude1", "Cut")],
            rolled_back_before: 0,
        };
        let text = rollback_text(&p, &reply);
        assert!(text.starts_with("moved the rollback bar to before Fillet1."), "{text}");
        assert!(text.contains("Fillet1 (fillet), Cut-Extrude1"), "{text}");
        let none = RollbackParams { before: None, after: None };
        let reply = proto::RollbackResponse { rolled_back: vec![], rolled_back_before: 3 };
        assert!(rollback_text(&none, &reply).contains("the 3 rolled-back features are back"));
        let reply = proto::RollbackResponse { rolled_back: vec![], rolled_back_before: 0 };
        assert!(rollback_text(&none, &reply).starts_with("nothing was rolled back"));
    }

    #[test]
    fn reorder_requests() {
        let ids = IdRegistry::default();
        let p = |before: Option<&str>, after: Option<&str>| ReorderParams {
            feature: "Fillet1".into(),
            before: before.map(str::to_string),
            after: after.map(str::to_string),
        };
        let r = reorder_request(&ids, &p(None, Some("Boss-Extrude1"))).unwrap();
        assert_eq!(r.target.unwrap().target, Some(Target::Name("Boss-Extrude1".into())));
        assert!(r.after);
        assert!(!reorder_request(&ids, &p(Some("Cut-Extrude1"), None)).unwrap().after);
        assert!(reorder_request(&ids, &p(None, None)).is_err());
        assert!(reorder_request(&ids, &p(Some("A"), Some("B"))).is_err());
        assert!(reorder_request(&ids, &p(Some(" Fillet1"), None)).is_err());
    }

    #[test]
    fn reorder_replies() {
        let reply = proto::ReorderFeatureResponse {
            moved: Some(feature("Fillet1", "Fillet")),
            previous: "Boss-Extrude1".into(),
            next: "Cut-Extrude1".into(),
            new_errors: vec![],
        };
        assert_eq!(reorder_text(&reply), "moved Fillet1 (fillet): now between Boss-Extrude1 and Cut-Extrude1.");
        let reply = proto::ReorderFeatureResponse {
            moved: Some(feature("Fillet1", "Fillet")),
            previous: "Cut-Extrude1".into(),
            new_errors: vec![proto::FeatureProblem {
                name: "Fillet1".into(),
                type_name: "Fillet".into(),
                error_code: 13,
                warning: false,
            }],
            ..Default::default()
        };
        let text = reorder_text(&reply);
        assert!(text.starts_with("moved Fillet1 (fillet): now last, after Cut-Extrude1. Now failing"), "{text}");
        assert!(text.ends_with("Move it back with reorder_feature if that isn't wanted."), "{text}");
    }
}
