//! The set_visibility tool: its parameters, what can be hidden, and the reply
//! text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{IdRegistry, Kind};
use crate::mirror::unique;

/// The add-in's limit.
const MAX_ITEMS: usize = 500;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetVisibilityParams {
    /// What to hide or show, from inspect_model: bodies of a part ("body:1"), components of an
    /// assembly ("component:2"), sketches and reference geometry ("feature:4", or a name such
    /// as "Sketch2", "Plane1" or "Front Plane").
    pub items: Vec<String>,
    /// true: show them; false: hide them.
    pub visible: bool,
}

pub fn request(ids: &IdRegistry, params: &SetVisibilityParams) -> Result<proto::SetVisibilityRequest, McpError> {
    use proto::feature_target::Target;
    let items = unique(&params.items);
    if items.is_empty() {
        return Err(invalid("list what to hide or show (ids from inspect_model, or feature names)"));
    }
    if items.len() > MAX_ITEMS {
        return Err(invalid(format!("at most {MAX_ITEMS} per call, got {}", items.len())));
    }
    let items = items
        .into_iter()
        .map(|item| {
            if item.is_empty() {
                return Err(invalid("items has an empty entry"));
            }
            let target = match Kind::of_id(item) {
                None => Target::Name(item.to_string()),
                Some(Kind::Body | Kind::Component | Kind::Feature) => {
                    Target::PersistId(ids.persist_id(item).ok_or_else(|| {
                        invalid(ids.unknown_id(item, "inspect_model", "call inspect_model for current ones"))
                    })?)
                }
                Some(_) => {
                    return Err(invalid(format!(
                        "{item} can't be hidden on its own -- give a body, component, sketch or reference geometry"
                    )));
                }
            };
            Ok(proto::FeatureTarget { target: Some(target) })
        })
        .collect::<Result<_, McpError>>()?;
    Ok(proto::SetVisibilityRequest { items, visible: params.visible })
}

fn list(items: &[&proto::VisibilityChange]) -> String {
    items.iter().map(|i| format!("{} ({})", i.name, i.kind)).collect::<Vec<_>>().join(", ")
}

/// "hid Boss-Extrude1 (body). Sketch2 (sketch) was already hidden. ..."
pub fn reply_text(params: &SetVisibilityParams, reply: &proto::SetVisibilityResponse) -> String {
    let (verb, state) = if params.visible { ("showed", "shown") } else { ("hid", "hidden") };
    let filter = |f: &dyn Fn(&proto::VisibilityChange) -> bool| reply.items.iter().filter(|i| f(i)).collect::<Vec<_>>();
    let changed = filter(&|i| i.changed);
    let already = filter(&|i| !i.changed && i.visible == params.visible);
    let refused = filter(&|i| !i.changed && i.visible != params.visible);
    let mut parts = Vec::new();
    if !changed.is_empty() {
        parts.push(format!("{verb} {}.", list(&changed)));
    }
    if !already.is_empty() {
        parts.push(format!("{} already {state}.", list(&already)));
    }
    if !refused.is_empty() {
        parts.push(format!("SolidWorks didn't change {}: not {state}.", list(&refused)));
    }
    if !params.visible && !changed.is_empty() {
        parts.push(
            "Hidden things stay in the model and count in its mass; inspect_model lists them as hidden and \
             render_views leaves them out."
                .to_string(),
        );
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(items: &[&str], visible: bool) -> SetVisibilityParams {
        SetVisibilityParams { items: items.iter().map(|i| i.to_string()).collect(), visible }
    }

    fn change(name: &str, kind: &str, changed: bool, visible: bool) -> proto::VisibilityChange {
        proto::VisibilityChange { name: name.into(), kind: kind.into(), changed, visible }
    }

    #[test]
    fn targets() {
        use proto::feature_target::Target;
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        let body = ids.id_for(Kind::Body, b"body").unwrap();
        let component = ids.id_for(Kind::Component, b"component").unwrap();
        let feature = ids.id_for(Kind::Feature, b"feature").unwrap();
        let r = request(&ids, &params(&[&body, &format!(" {body} "), &component, &feature, " Sketch2 "], false)).unwrap();
        let targets: Vec<_> = r.items.into_iter().map(|i| i.target.unwrap()).collect();
        assert_eq!(targets.len(), 4);
        assert_eq!(targets[0], Target::PersistId(b"body".to_vec()));
        assert_eq!(targets[3], Target::Name("Sketch2".into()));
        assert!(!r.visible);
    }

    #[test]
    fn refuses_bad_input() {
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Part1");
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        assert!(request(&ids, &params(&[], false)).is_err());
        assert!(request(&ids, &params(&[" "], false)).is_err());
        assert!(request(&ids, &params(&[&face], false)).unwrap_err().message.contains("can't be hidden"));
        assert!(request(&ids, &params(&["body:99"], false)).is_err());
    }

    #[test]
    fn replies() {
        let reply = proto::SetVisibilityResponse {
            items: vec![
                change("Boss-Extrude1", "body", true, false),
                change("Sketch2", "sketch", false, false),
                change("Plane1", "reference geometry", false, true),
            ],
        };
        let text = reply_text(&params(&["a"], false), &reply);
        assert!(text.starts_with("hid Boss-Extrude1 (body). Sketch2 (sketch) already hidden. SolidWorks didn't change Plane1 (reference geometry): not hidden."), "{text}");
        assert!(text.contains("render_views leaves them out"));

        let reply = proto::SetVisibilityResponse { items: vec![change("block-1", "component", true, true)] };
        assert_eq!(reply_text(&params(&["a"], true), &reply), "showed block-1 (component).");
    }
}
