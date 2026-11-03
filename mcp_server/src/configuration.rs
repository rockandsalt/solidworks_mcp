//! Configurations: the configurations tool (list, create, activate, delete),
//! set_suppression, and the configuration choice set_dimension shares with
//! set_suppression. See docs/notes/configurations.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::delete::problems_text;
use crate::inspect::{IdRegistry, Kind, mm, round};
use crate::mirror::unique;

/// The add-in's limit.
const MAX_ITEMS: usize = 500;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn quoted(names: &[impl AsRef<str>]) -> String {
    names.iter().map(|n| format!("\"{}\"", n.as_ref())).collect::<Vec<_>>().join(", ")
}

/// "D1@Boss-Extrude1@block.Part" -> "D1@Boss-Extrude1", the name set_dimension takes.
pub fn dimension_name(full: &str) -> &str {
    match full.match_indices('@').nth(1) {
        Some((at, _)) => &full[..at],
        None => full,
    }
}

/// The configurations a change goes to: (ConfigurationScope, names).
/// Neither given: `unspecified`.
pub fn scope(
    configurations: Option<&[String]>,
    all_configurations: Option<bool>,
    unspecified: proto::ConfigurationScope,
) -> Result<(i32, Vec<String>), McpError> {
    let named = configurations.map(unique).unwrap_or_default();
    if named.iter().any(|n| n.is_empty()) {
        return Err(invalid("configurations has an empty entry"));
    }
    match (named.is_empty(), all_configurations.unwrap_or(false)) {
        (false, true) => Err(invalid("give configurations or all_configurations=true, not both")),
        (false, false) => Ok((
            proto::ConfigurationScope::Named as i32,
            named.into_iter().map(str::to_string).collect(),
        )),
        (true, true) => Ok((proto::ConfigurationScope::All as i32, Vec::new())),
        (true, false) if configurations.is_some() => Err(invalid("configurations is empty")),
        (true, false) => Ok((unspecified as i32, Vec::new())),
    }
}

/// "Default = 10 mm, Long = 30 mm" for set_dimension's reply; empty with one configuration.
pub fn values_text(reply: &proto::SetDimensionValueResponse) -> String {
    if reply.values.is_empty() {
        return String::new();
    }
    let angle = reply.dimension.as_ref().is_some_and(|d| d.r#type() == proto::DimensionType::Angular);
    let list: Vec<String> = reply
        .values
        .iter()
        .map(|v| {
            if angle {
                format!("{} = {}°", v.configuration, round(v.value.to_degrees(), 4))
            } else {
                format!("{} = {} mm", v.configuration, mm(v.value))
            }
        })
        .collect();
    format!(" By configuration: {}.", list.join(", "))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationAction {
    #[default]
    List,
    Create,
    Activate,
    Delete,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigurationsParams {
    /// "list" (default), "create", "activate" or "delete".
    #[serde(default)]
    pub action: ConfigurationAction,
    /// create: the new configuration's name; activate / delete: which configuration.
    #[serde(default)]
    pub name: Option<String>,
    /// create: derive the new configuration from this one: it follows its parent's values and
    /// suppression until given its own. Default: a top-level copy of the active configuration.
    #[serde(default)]
    pub parent: Option<String>,
    /// create: its description. Default: SolidWorks' (the name).
    #[serde(default)]
    pub description: Option<String>,
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

pub fn request(params: &ConfigurationsParams) -> Result<proto::ConfigurationsRequest, McpError> {
    use proto::configurations_request::Action;
    let name = text(params.name.as_deref());
    let needs_name = |what: &str| -> Result<String, McpError> {
        if name.is_empty() {
            Err(invalid(format!("give the name of the configuration to {what}")))
        } else {
            Ok(name.clone())
        }
    };
    if params.action != ConfigurationAction::Create && (params.parent.is_some() || params.description.is_some()) {
        return Err(invalid("parent and description are for action=\"create\""));
    }
    let action = match params.action {
        ConfigurationAction::List if !name.is_empty() => {
            return Err(invalid("name is for create, activate or delete -- give an action"));
        }
        ConfigurationAction::List => None,
        ConfigurationAction::Create => Some(Action::Create(proto::CreateConfiguration {
            name: needs_name("create")?,
            description: text(params.description.as_deref()),
            parent: text(params.parent.as_deref()),
        })),
        ConfigurationAction::Activate => Some(Action::Activate(needs_name("activate")?)),
        ConfigurationAction::Delete => Some(Action::Delete(needs_name("delete")?)),
    };
    Ok(proto::ConfigurationsRequest { action })
}

/// What a ConfigurationsResponse lists, as JSON.
pub fn configurations_json(reply: &proto::ConfigurationsResponse) -> Value {
    let configurations: Vec<Value> = reply
        .configurations
        .iter()
        .map(|c| {
            let mut obj = Map::new();
            obj.insert("name".into(), json!(c.name));
            if c.name == reply.active {
                obj.insert("active".into(), json!(true));
            }
            if !c.parent.is_empty() {
                obj.insert("derived_from".into(), json!(c.parent));
            }
            if !c.description.is_empty() && c.description != c.name {
                obj.insert("description".into(), json!(c.description));
            }
            if !c.suppressed_features.is_empty() {
                obj.insert("suppressed_features".into(), json!(c.suppressed_features));
            }
            if !c.suppressed_components.is_empty() {
                obj.insert("suppressed_components".into(), json!(c.suppressed_components));
            }
            Value::Object(obj)
        })
        .collect();
    let mut out = Map::new();
    out.insert("active".into(), json!(reply.active));
    out.insert("configurations".into(), Value::Array(configurations));
    if !reply.dimensions.is_empty() {
        let dimensions: Vec<Value> = reply
            .dimensions
            .iter()
            .map(|d| {
                let values: Map<String, Value> = reply
                    .configurations
                    .iter()
                    .zip(&d.values)
                    .map(|(c, v)| {
                        let v = if d.angle { round(v.to_degrees(), 4) } else { mm(*v) };
                        (c.name.clone(), json!(v))
                    })
                    .collect();
                let key = if d.angle { "values_deg" } else { "values_mm" };
                json!({ "name": dimension_name(&d.name), key: values })
            })
            .collect();
        out.insert("dimensions_that_differ".into(), Value::Array(dimensions));
    }
    Value::Object(out)
}

pub fn configurations_text(params: &ConfigurationsParams, reply: &proto::ConfigurationsResponse) -> String {
    let name = text(params.name.as_deref());
    let mut text = match params.action {
        ConfigurationAction::List => String::new(),
        ConfigurationAction::Create => {
            let from = match text(params.parent.as_deref()) {
                p if p.is_empty() => "a copy of the configuration that was active".to_string(),
                p => format!("derived from \"{p}\": it follows its values and suppression until given its own"),
            };
            format!("created configuration \"{name}\" ({from}); \"{}\" is now the active one.\n", reply.active)
        }
        ConfigurationAction::Activate => format!("\"{}\" is now the active configuration.\n", reply.active),
        ConfigurationAction::Delete => {
            let others: Vec<&String> = reply.deleted.iter().filter(|d| !d.eq_ignore_ascii_case(&name)).collect();
            if others.is_empty() {
                format!("deleted configuration \"{name}\".\n")
            } else {
                format!("deleted configuration \"{name}\" and the ones derived from it: {}.\n", quoted(&others))
            }
        }
    };
    text += &configurations_json(reply).to_string();
    text
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetSuppressionParams {
    /// What to suppress or unsuppress, from inspect_model: features and mates ("feature:4", or
    /// names such as "Cut-Extrude1", "Fillet2", "Coincident3") and top-level components of an
    /// assembly ("component:2", or a name such as "bracket-1").
    pub items: Vec<String>,
    /// true: suppress them; false: unsuppress them.
    pub suppressed: bool,
    /// Only in these configurations (names from the configurations tool). Default: the active
    /// configuration.
    #[serde(default)]
    pub configurations: Option<Vec<String>>,
    /// true: in every configuration.
    #[serde(default)]
    pub all_configurations: Option<bool>,
}

pub fn suppression_request(
    ids: &IdRegistry,
    params: &SetSuppressionParams,
) -> Result<proto::SetSuppressionRequest, McpError> {
    use proto::feature_target::Target;
    let items = unique(&params.items);
    if items.is_empty() {
        return Err(invalid("list what to suppress or unsuppress (ids from inspect_model, or names)"));
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
                Some(Kind::Component | Kind::Feature) => Target::PersistId(ids.persist_id(item).ok_or_else(|| {
                    invalid(ids.unknown_id(item, "inspect_model", "call inspect_model for current ones"))
                })?),
                Some(_) => {
                    return Err(invalid(format!(
                        "{item} can't be suppressed -- give a feature (the one that made it), mate or component"
                    )));
                }
            };
            Ok(proto::FeatureTarget { target: Some(target) })
        })
        .collect::<Result<_, McpError>>()?;
    let (scope, configurations) = scope(
        params.configurations.as_deref(),
        params.all_configurations,
        proto::ConfigurationScope::Active,
    )?;
    Ok(proto::SetSuppressionRequest { items, suppressed: params.suppressed, scope, configurations })
}

/// "suppressed Cut-Extrude1 (feature) in "Long". ..."
pub fn suppression_text(params: &SetSuppressionParams, reply: &proto::SetSuppressionResponse) -> String {
    let (verb, state) = if params.suppressed { ("suppressed", "suppressed") } else { ("unsuppressed", "unsuppressed") };
    let mut parts = Vec::new();
    for item in &reply.items {
        let pick = |f: &dyn Fn(&proto::SuppressionState) -> bool| {
            item.states.iter().filter(|s| f(s)).map(|s| s.configuration.as_str()).collect::<Vec<_>>()
        };
        let changed = pick(&|s| s.changed);
        let already = pick(&|s| !s.changed && s.suppressed == params.suppressed);
        let refused = pick(&|s| !s.changed && s.suppressed != params.suppressed);
        let what = format!("{} ({})", item.name, item.kind);
        let mut said = Vec::new();
        if !changed.is_empty() {
            said.push(format!("{verb} {what} in {}", quoted(&changed)));
        }
        if !already.is_empty() {
            let lead = if said.is_empty() { what.clone() } else { "it".to_string() };
            said.push(format!("{lead} was already {state} in {}", quoted(&already)));
        }
        if !refused.is_empty() {
            said.push(format!("SolidWorks didn't change {what} in {}: not {state}", quoted(&refused)));
        }
        parts.push(said.join("; ") + ".");
    }
    if !reply.also_changed.is_empty() {
        parts.push(format!(
            "With them, in the active configuration \"{}\": {} changed too (features that depend on them).",
            reply.active,
            reply.also_changed.join(", ")
        ));
    }
    let mut text = parts.join(" ");
    text += &problems_text(&reply.new_errors);
    if params.suppressed {
        text += " Suppressed things keep their ids but are left out of the model: no geometry, mass or \
                 render; the configurations tool lists what each configuration suppresses.";
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scopes() {
        let active = proto::ConfigurationScope::Active;
        let unspecified = proto::ConfigurationScope::Unspecified;
        assert_eq!(scope(None, None, active).unwrap(), (active as i32, vec![]));
        assert_eq!(scope(None, Some(false), unspecified).unwrap(), (unspecified as i32, vec![]));
        assert_eq!(scope(None, Some(true), active).unwrap(), (proto::ConfigurationScope::All as i32, vec![]));
        let list = names(&[" Long ", "Short", "Long"]);
        assert_eq!(
            scope(Some(&list), None, active).unwrap(),
            (proto::ConfigurationScope::Named as i32, names(&["Long", "Short"]))
        );
        assert!(scope(Some(&list), Some(true), active).is_err());
        assert!(scope(Some(&[]), None, active).is_err());
        assert!(scope(Some(&names(&[" "])), None, active).is_err());
    }

    #[test]
    fn dimension_names() {
        assert_eq!(dimension_name("D1@Boss-Extrude1@block.Part"), "D1@Boss-Extrude1");
        assert_eq!(dimension_name("D1@Sketch1"), "D1@Sketch1");
        assert_eq!(dimension_name("D1"), "D1");
    }

    fn params(action: ConfigurationAction, name: Option<&str>) -> ConfigurationsParams {
        ConfigurationsParams { action, name: name.map(str::to_string), parent: None, description: None }
    }

    #[test]
    fn configuration_requests() {
        use proto::configurations_request::Action;
        assert_eq!(request(&params(ConfigurationAction::List, None)).unwrap().action, None);
        assert!(request(&params(ConfigurationAction::List, Some("Long"))).is_err());
        assert!(request(&params(ConfigurationAction::Create, Some(" "))).is_err());
        assert!(request(&params(ConfigurationAction::Delete, None)).is_err());
        assert_eq!(
            request(&params(ConfigurationAction::Activate, Some(" Long "))).unwrap().action,
            Some(Action::Activate("Long".into()))
        );
        let mut p = params(ConfigurationAction::Create, Some("Long"));
        p.parent = Some("Default".into());
        let Some(Action::Create(create)) = request(&p).unwrap().action else { panic!() };
        assert_eq!((create.name.as_str(), create.parent.as_str()), ("Long", "Default"));
        let mut p = params(ConfigurationAction::Delete, Some("Long"));
        p.parent = Some("Default".into());
        assert!(request(&p).is_err());
    }

    #[test]
    fn configuration_replies() {
        let reply = proto::ConfigurationsResponse {
            configurations: vec![
                proto::ConfigurationInfo { name: "Default".into(), description: "Default".into(), ..Default::default() },
                proto::ConfigurationInfo {
                    name: "Long".into(),
                    parent: "Default".into(),
                    suppressed_features: names(&["Cut-Extrude1", "Sketch2"]),
                    ..Default::default()
                },
            ],
            active: "Long".into(),
            deleted: vec![],
            dimensions: vec![proto::ConfiguredDimension {
                name: "D1@Boss-Extrude1@block.Part".into(),
                angle: false,
                values: vec![0.01, 0.03],
            }],
        };
        let mut p = params(ConfigurationAction::Create, Some("Long"));
        p.parent = Some("Default".into());
        let text = configurations_text(&p, &reply);
        assert!(text.starts_with("created configuration \"Long\" (derived from \"Default\""), "{text}");
        let json: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(json["configurations"][0], json!({ "name": "Default" }));
        assert_eq!(json["configurations"][1]["active"], json!(true));
        assert_eq!(json["configurations"][1]["suppressed_features"][0], json!("Cut-Extrude1"));
        assert_eq!(
            json["dimensions_that_differ"][0],
            json!({ "name": "D1@Boss-Extrude1", "values_mm": { "Default": 10.0, "Long": 30.0 } })
        );

        let reply = proto::ConfigurationsResponse {
            active: "Default".into(),
            deleted: names(&["Long", "Long-short"]),
            ..Default::default()
        };
        let text = configurations_text(&params(ConfigurationAction::Delete, Some("long")), &reply);
        assert!(text.starts_with("deleted configuration \"long\" and the ones derived from it: \"Long-short\"."), "{text}");
    }

    fn state(configuration: &str, suppressed: bool, changed: bool) -> proto::SuppressionState {
        proto::SuppressionState { configuration: configuration.into(), suppressed, changed }
    }

    #[test]
    fn suppression_requests() {
        use proto::feature_target::Target;
        let mut ids = IdRegistry::default();
        ids.set_document("doc", "Assem1");
        let component = ids.id_for(Kind::Component, b"component").unwrap();
        let face = ids.id_for(Kind::Face, b"face").unwrap();
        let p = |items: &[&str]| SetSuppressionParams {
            items: names(items),
            suppressed: true,
            configurations: None,
            all_configurations: None,
        };
        let r = suppression_request(&ids, &p(&[&component, " Cut-Extrude1 "])).unwrap();
        assert_eq!(r.items[0].target, Some(Target::PersistId(b"component".to_vec())));
        assert_eq!(r.items[1].target, Some(Target::Name("Cut-Extrude1".into())));
        assert_eq!(r.scope, proto::ConfigurationScope::Active as i32);
        assert!(suppression_request(&ids, &p(&[])).is_err());
        assert!(suppression_request(&ids, &p(&[&face])).unwrap_err().message.contains("can't be suppressed"));
        assert!(suppression_request(&ids, &p(&["feature:99"])).is_err());
    }

    #[test]
    fn suppression_replies() {
        let params = SetSuppressionParams { items: vec![], suppressed: true, configurations: None, all_configurations: None };
        let reply = proto::SetSuppressionResponse {
            items: vec![
                proto::SuppressionChange {
                    name: "Cut-Extrude1".into(),
                    kind: "feature".into(),
                    states: vec![state("Default", true, false), state("Long", true, true)],
                },
                proto::SuppressionChange {
                    name: "bolt-1".into(),
                    kind: "component".into(),
                    states: vec![state("Default", false, false)],
                },
            ],
            also_changed: names(&["Sketch2"]),
            active: "Default".into(),
            new_errors: vec![],
        };
        let text = suppression_text(&params, &reply);
        assert!(
            text.starts_with(
                "suppressed Cut-Extrude1 (feature) in \"Long\"; it was already suppressed in \"Default\". \
                 SolidWorks didn't change bolt-1 (component) in \"Default\": not suppressed. With them"
            ),
            "{text}"
        );
    }
}
