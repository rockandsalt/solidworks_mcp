//! Equations and global variables: the equations tool. What it changes is
//! checked here against the document's equations first, since SolidWorks
//! refuses an equation without saying why and reads a deleted global
//! variable as 0 in the equations still using it. See docs/notes/equations.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::delete::problems_text;
use crate::inspect::round;
use crate::sketch::dimension_json;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EquationSet {
    /// A global variable ("Width") or a dimension ("D1@Boss-Extrude1"), without quotes.
    pub name: String,
    /// The right side, as SolidWorks takes it: "50", "\"Width\" / 2 + 5", "3in", "30deg",
    /// "IIF(\"Width\" > 60, 2, 1)". Names in double quotes; plain numbers are in the document's
    /// units (degrees for angles).
    pub expression: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EquationsParams {
    /// Equations to set, in order: each replaces the equation with that name, or is added. A
    /// global variable can only use the ones that exist or come earlier in this list.
    #[serde(default)]
    pub set: Vec<EquationSet>,
    /// Names (left sides) whose equations to delete, before setting.
    #[serde(default)]
    pub delete: Vec<String>,
}

impl EquationsParams {
    pub fn changes_anything(&self) -> bool {
        !self.set.is_empty() || !self.delete.is_empty()
    }
}

/// The names in double quotes in `text`, in order.
fn quoted_names(text: &str) -> Vec<&str> {
    text.split('"').skip(1).step_by(2).collect()
}

/// A global variable's name, as opposed to a dimension ("D1@Sketch1") or a
/// file property ("SW-Mass@Part1.SLDPRT").
fn is_global_name(name: &str) -> bool {
    !name.contains('@')
}

fn find<'a>(equations: &'a [proto::EquationInfo], name: &str) -> Option<&'a proto::EquationInfo> {
    equations.iter().find(|e| e.name.eq_ignore_ascii_case(name))
}

/// The request for `params`, checked against the document's `equations`.
pub fn request(params: &EquationsParams, equations: &[proto::EquationInfo]) -> Result<proto::EquationsRequest, McpError> {
    let existing = equations.iter().map(|e| format!("\"{}\"", e.name)).collect::<Vec<_>>().join(", ");

    let mut delete = Vec::new();
    for name in &params.delete {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("delete has an empty entry"));
        }
        let Some(found) = find(equations, name) else {
            return Err(invalid(if existing.is_empty() {
                format!("the document has no equations; there's no \"{name}\" to delete")
            } else {
                format!("the document has no equation for \"{name}\" -- it has {existing}")
            }));
        };
        if !delete.iter().any(|d: &String| d.eq_ignore_ascii_case(&found.name)) {
            delete.push(found.name.clone());
        }
    }

    let mut set: Vec<proto::EquationChange> = Vec::new();
    for change in &params.set {
        let name = change.name.trim().trim_matches('"').trim();
        let expression = change.expression.trim();
        if name.is_empty() || name.contains('"') {
            return Err(invalid(format!("\"{}\" isn't an equation name: give a global variable or dimension name without quotes", change.name)));
        }
        if expression.is_empty() {
            return Err(invalid(format!("give \"{name}\" an expression")));
        }
        if set.iter().any(|s| s.name.eq_ignore_ascii_case(name)) {
            return Err(invalid(format!("\"{name}\" is set twice")));
        }
        if delete.iter().any(|d| d.eq_ignore_ascii_case(name)) {
            return Err(invalid(format!("\"{name}\" is both deleted and set -- setting it replaces its equation")));
        }
        // The document's spelling: SolidWorks would take ours and rename the variable.
        let name = find(equations, name).map_or(name, |e| e.name.as_str());
        set.push(proto::EquationChange { name: name.to_string(), expression: expression.to_string() });
    }

    let deleted = |name: &str| delete.iter().any(|d| d.eq_ignore_ascii_case(name));
    let is_set = |name: &str| set.iter().any(|s| s.name.eq_ignore_ascii_case(name));
    // Equations that stay as they are mustn't lose a global variable: SolidWorks
    // would read it as 0 there (seen 2026-11-02).
    for e in equations.iter().filter(|e| !deleted(&e.name) && !is_set(&e.name)) {
        if let Some(gone) = quoted_names(&e.equation).into_iter().skip(1).find(|n| is_global_name(n) && deleted(n)) {
            return Err(invalid(format!(
                "\"{gone}\" is used by {} -- delete or change that equation too (SolidWorks would read \"{gone}\" as 0 there)",
                e.equation
            )));
        }
    }
    // New expressions may only use global variables that exist by then.
    for (i, change) in set.iter().enumerate() {
        for used in quoted_names(&change.expression).into_iter().filter(|n| is_global_name(n)) {
            let exists = find(equations, used).is_some_and(|e| e.global) && !deleted(used);
            let earlier = set[..i].iter().any(|s| s.name.eq_ignore_ascii_case(used));
            if exists || earlier {
                continue;
            }
            return Err(invalid(if set[i + 1..].iter().any(|s| s.name.eq_ignore_ascii_case(used)) {
                format!("\"{}\" uses \"{used}\", which is set later in the list -- set \"{used}\" first", change.name)
            } else if used.eq_ignore_ascii_case(&change.name) {
                format!("\"{}\" uses itself", change.name)
            } else {
                format!(
                    "\"{}\" uses \"{used}\", which is no global variable of the document (dimension names look like \
                     D1@Sketch1)",
                    change.name
                )
            }));
        }
    }
    Ok(proto::EquationsRequest { delete, set })
}

pub fn equations_json(reply: &proto::EquationsResponse, length_unit: &str) -> Value {
    let equations: Vec<Value> = reply
        .equations
        .iter()
        .map(|e| {
            let mut obj = Map::new();
            obj.insert("name".into(), json!(e.name));
            obj.insert("equation".into(), json!(e.equation));
            match &e.dimension {
                Some(d) if !e.global => {
                    if let Value::Object(dim) = dimension_json(d) {
                        for key in ["value_mm", "value_deg"] {
                            if let Some(v) = dim.get(key) {
                                obj.insert(key.into(), v.clone());
                            }
                        }
                    }
                }
                _ => {
                    obj.insert("value".into(), json!(round(e.value, 6)));
                }
            }
            if e.global {
                obj.insert("global_variable".into(), json!(true));
            }
            Value::Object(obj)
        })
        .collect();
    json!({
        "equations": equations,
        "plain_numbers_are_in": format!("{length_unit} (the document's length unit); angles in degrees"),
    })
}

pub fn equations_text(params: &EquationsParams, request: &proto::EquationsRequest, reply: &proto::EquationsResponse, length_unit: &str) -> String {
    let mut said = Vec::new();
    if !request.delete.is_empty() {
        let list: Vec<String> = request.delete.iter().map(|n| format!("\"{n}\"")).collect();
        said.push(format!("deleted the equations of {}", list.join(", ")));
    }
    if !request.set.is_empty() {
        let list: Vec<String> = request
            .set
            .iter()
            .map(|s| find(&reply.equations, &s.name).map_or_else(|| format!("\"{}\"", s.name), |e| e.equation.clone()))
            .collect();
        said.push(format!("set {}", list.join("; ")));
    }
    let mut text = if params.changes_anything() {
        format!("{}; the model was rebuilt.", said.join(", and "))
    } else if reply.equations.is_empty() {
        "the document has no equations or global variables.".to_string()
    } else {
        String::new()
    };
    text += &problems_text(&reply.new_errors);
    if !text.is_empty() {
        text.push('\n');
    }
    text += &equations_json(reply, length_unit).to_string();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn equation(text: &str, global: bool) -> proto::EquationInfo {
        proto::EquationInfo {
            name: quoted_names(text)[0].to_string(),
            equation: text.into(),
            global,
            value: 0.0,
            dimension: None,
        }
    }

    fn doc() -> Vec<proto::EquationInfo> {
        vec![
            equation("\"Width\" = 50", true),
            equation("\"Half\" = \"Width\" / 2", true),
            equation("\"D1@Boss-Extrude1\" = \"Half\" + 5", false),
        ]
    }

    fn set(name: &str, expression: &str) -> EquationSet {
        EquationSet { name: name.into(), expression: expression.into() }
    }

    fn params(set: Vec<EquationSet>, delete: &[&str]) -> EquationsParams {
        EquationsParams { set, delete: delete.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn names_in_quotes() {
        assert_eq!(quoted_names("\"D1@Sketch1\" = \"Width\" * 2 + \"Half\""), vec!["D1@Sketch1", "Width", "Half"]);
        assert!(quoted_names("50").is_empty());
    }

    #[test]
    fn requests() {
        let r = request(&params(vec![set(" \"Depth\" ", " \"width\" + 1 "), set("D1@Sketch1", "\"Depth\"")], &[]), &doc())
            .unwrap();
        assert_eq!(r.set[0].name, "Depth");
        assert_eq!(r.set[0].expression, "\"width\" + 1");
        assert_eq!(r.set[1].name, "D1@Sketch1");
        let r = request(&params(vec![set("width", "60")], &[]), &doc()).unwrap();
        assert_eq!(r.set[0].name, "Width");

        // Deleting what's still used, unless that goes or changes too.
        let e = request(&params(vec![], &["half"]), &doc()).unwrap_err();
        assert!(e.message.contains("\"Half\" is used by \"D1@Boss-Extrude1\""), "{}", e.message);
        let r = request(&params(vec![], &["half", "D1@Boss-Extrude1"]), &doc()).unwrap();
        assert_eq!(r.delete, vec!["Half", "D1@Boss-Extrude1"]);
        assert!(request(&params(vec![set("D1@Boss-Extrude1", "\"Width\"")], &["Half"]), &doc()).is_ok());

        assert!(request(&params(vec![], &["Nope"]), &doc()).unwrap_err().message.contains("it has \"Width\""));
        assert!(request(&params(vec![set("Width", "1"), set("width", "2")], &[]), &doc()).is_err());
        assert!(request(&params(vec![set("Width", "1")], &["Width"]), &doc()).is_err());
        assert!(request(&params(vec![set("Width", " ")], &[]), &doc()).is_err());
        let e = request(&params(vec![set("A", "\"B\""), set("B", "1")], &[]), &doc()).unwrap_err();
        assert!(e.message.contains("set later"), "{}", e.message);
        let e = request(&params(vec![set("A", "\"Nope\" * 2")], &[]), &doc()).unwrap_err();
        assert!(e.message.contains("no global variable"), "{}", e.message);
        // A deleted global can't be used by a new equation either.
        assert!(request(&params(vec![set("D1@Boss-Extrude1", "\"Half\"")], &["Half"]), &doc()).is_err());
        // Dimensions and functions are SolidWorks' to check.
        assert!(request(&params(vec![set("A", "\"D1@Sketch1\" * sin(30)")], &[]), &doc()).is_ok());
    }

    #[test]
    fn replies() {
        let mut equations = doc();
        equations[0].value = 50.0;
        equations[2].dimension = Some(proto::DimensionInfo {
            name: "D1@Boss-Extrude1".into(),
            r#type: proto::DimensionType::Linear as i32,
            value: 0.03,
            driven: false,
        });
        let reply = proto::EquationsResponse { equations, new_errors: vec![] };
        let p = params(vec![set("Width", "50")], &[]);
        let r = request(&p, &doc()).unwrap();
        let text = equations_text(&p, &r, &reply, "millimeters");
        assert!(text.starts_with("set \"Width\" = 50; the model was rebuilt.\n"), "{text}");
        let json: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(json["equations"][0], json!({ "name": "Width", "equation": "\"Width\" = 50", "value": 50.0, "global_variable": true }));
        assert_eq!(json["equations"][2]["value_mm"], json!(30.0));

        let empty = proto::EquationsResponse::default();
        let text = equations_text(&EquationsParams::default(), &proto::EquationsRequest::default(), &empty, "inches");
        assert!(text.starts_with("the document has no equations"), "{text}");
        assert!(text.contains("inches (the document's length unit)"), "{text}");
    }
}
