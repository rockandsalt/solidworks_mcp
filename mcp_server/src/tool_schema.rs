//! Slims the input schemas schemars generates before they go out in
//! tools/list: every client reads them on every session, so what carries no
//! meaning for a model costs tokens for nothing. Only the listed schemas
//! change; arguments are still read by serde.

use serde_json::{Map, Value};

/// The tool list as sent: each input schema slimmed.
pub fn slim_tools(mut tools: Vec<rmcp::model::Tool>) -> Vec<rmcp::model::Tool> {
    for tool in &mut tools {
        let mut schema = Value::Object((*tool.input_schema).clone());
        slim(&mut schema);
        if let Value::Object(schema) = schema {
            tool.input_schema = schema.into();
        }
    }
    tools
}

/// Drops `$schema`, `format` and `"default": null`; turns `[T, "null"]` and
/// `anyOf [X, null]` into T / X (leaving a field out already says it's
/// optional); and a `oneOf` of documented constants into an `enum`, its
/// variants' docs appended to the description.
fn slim(value: &mut Value) {
    match value {
        Value::Object(node) => {
            node.remove("$schema");
            node.remove("format");
            if node.get("default") == Some(&Value::Null) {
                node.remove("default");
            }
            if let Some(Value::Array(types)) = node.get_mut("type") {
                types.retain(|t| t != "null");
                if types.len() == 1 {
                    let only = types.remove(0);
                    node.insert("type".into(), only);
                }
            }
            unwrap_nullable(node);
            constants_to_enum(node);
            node.values_mut().for_each(slim);
        }
        Value::Array(items) => items.iter_mut().for_each(slim),
        _ => {}
    }
}

fn is_null_type(value: &Value) -> bool {
    value.as_object().is_some_and(|o| o.len() == 1 && o.get("type").is_some_and(|t| t == "null"))
}

fn unwrap_nullable(node: &mut Map<String, Value>) {
    let Some(Value::Array(alternatives)) = node.get("anyOf") else { return };
    if alternatives.len() != 2 || !alternatives.iter().any(is_null_type) {
        return;
    }
    let Some(Value::Object(inner)) = alternatives.iter().find(|a| !is_null_type(a)).cloned() else {
        return;
    };
    node.remove("anyOf");
    for (key, value) in inner {
        node.entry(key).or_insert(value);
    }
}

/// A documented constant `{"const": x, "description": ...}`, or an undocumented
/// `{"enum": [...]}` run of them, as schemars writes an enum's variants.
fn is_constant(variant: &Value) -> bool {
    variant.as_object().is_some_and(|v| {
        (v.contains_key("const") || v.get("enum").is_some_and(Value::is_array))
            && v.keys().all(|k| ["const", "enum", "description", "type"].contains(&k.as_str()))
    })
}

fn constants_to_enum(node: &mut Map<String, Value>) {
    let Some(Value::Array(variants)) = node.get("oneOf") else { return };
    if variants.is_empty() || !variants.iter().all(is_constant) {
        return;
    }
    let Some(Value::Array(variants)) = node.remove("oneOf") else { return };
    let mut values = Vec::new();
    let mut docs = Vec::new();
    let mut kind = None;
    for variant in variants {
        let Value::Object(mut variant) = variant else { continue };
        kind = kind.or(variant.remove("type"));
        if let Some(Value::Array(plain)) = variant.remove("enum") {
            values.extend(plain);
            continue;
        }
        let constant = variant.remove("const").unwrap_or(Value::Null);
        if let Some(Value::String(doc)) = variant.remove("description") {
            let name = constant.as_str().map(str::to_string).unwrap_or_else(|| constant.to_string());
            docs.push(format!("{name}: {}", doc.trim_end_matches('.')));
        }
        values.push(constant);
    }
    node.entry("type").or_insert(kind.unwrap_or_else(|| "string".into()));
    node.insert("enum".into(), Value::Array(values));
    if !docs.is_empty() {
        let docs = docs.join("; ") + ".";
        let description = match node.get("description").and_then(Value::as_str) {
            Some(d) => format!("{d} {docs}"),
            None => docs,
        };
        node.insert("description".into(), description.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slims_what_schemars_adds() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "radius_mm": {"type": ["number", "null"], "format": "double", "default": null},
                "construction": {"type": "boolean", "default": false},
                "plane": {"description": "The plane.", "anyOf": [{"$ref": "#/$defs/P"}, {"type": "null"}]},
                "kind": {"description": "What.", "oneOf": [
                    {"description": "One line.", "type": "string", "const": "length"},
                    {"type": "string", "const": "angle"}
                ]},
                "arrow": {"oneOf": [
                    {"type": "string", "enum": ["open", "dot"]},
                    {"description": "Filled.", "type": "string", "const": "closed"}
                ]}
            }
        });
        slim(&mut schema);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "radius_mm": {"type": "number"},
                    "construction": {"type": "boolean", "default": false},
                    "plane": {"description": "The plane.", "$ref": "#/$defs/P"},
                    "kind": {"description": "What. length: One line.", "type": "string", "enum": ["length", "angle"]},
                    "arrow": {"description": "closed: Filled.", "type": "string", "enum": ["open", "dot", "closed"]}
                }
            })
        );
    }

    #[test]
    fn leaves_real_unions_alone() {
        let union = json!({"anyOf": [{"type": "string"}, {"type": "object"}]});
        let mut schema = union.clone();
        slim(&mut schema);
        assert_eq!(schema, union);
    }
}
