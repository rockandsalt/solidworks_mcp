//! The design_table tool: SolidWorks' Excel table whose rows are
//! configurations and whose columns are dimensions, suppression states,
//! global variables and properties. See docs/notes/equations.md.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::delete::problems_text;
use crate::macro_run::Closed;

/// The add-in's limits.
const MAX_COLUMNS: usize = 200;
const MAX_ROWS: usize = 1000;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesignTableAction {
    #[default]
    Read,
    Write,
    Delete,
}

/// A cell: text or a number.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Cell {
    Number(f64),
    Text(String),
}

impl Cell {
    fn text(&self) -> String {
        match self {
            Cell::Number(n) => n.to_string(),
            Cell::Text(t) => t.trim().to_string(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DesignTableRowParams {
    /// The configuration this row drives; SolidWorks creates it if the document doesn't have it.
    pub configuration: String,
    /// One per column, in the columns' order; "" (or fewer values than columns) leaves a cell
    /// empty: no value for that configuration.
    #[serde(default)]
    pub values: Vec<Cell>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DesignTableParams {
    /// "read" (default), "write" (replace the table's content, making the table if there is
    /// none) or "delete" (remove the table; configurations keep their values).
    #[serde(default)]
    pub action: DesignTableAction,
    /// write: the header row. A dimension ("D1@Boss-Extrude1"; values in the document's units,
    /// degrees for angles), "$STATE@<feature or component>" (values "S" suppressed / "U"
    /// unsuppressed), "$VALUE@<global variable>@equations" (the variable must exist: equations
    /// tool), "$DESCRIPTION", "$PARENT" (derived configurations), "$PRP@<custom property>", ...
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// write: one row per configuration.
    #[serde(default)]
    pub rows: Option<Vec<DesignTableRowParams>>,
}

pub fn request(params: &DesignTableParams) -> Result<proto::DesignTableRequest, McpError> {
    use proto::design_table_request::Action;
    if params.action != DesignTableAction::Write {
        if params.columns.is_some() || params.rows.is_some() {
            return Err(invalid("columns and rows are for action=\"write\""));
        }
        let action = (params.action == DesignTableAction::Delete).then_some(Action::Delete(true));
        return Ok(proto::DesignTableRequest { action });
    }
    let columns: Vec<String> = params.columns.iter().flatten().map(|c| c.trim().to_string()).collect();
    if columns.is_empty() {
        return Err(invalid("give the table's columns (e.g. [\"D1@Boss-Extrude1\", \"$STATE@Fillet1\"])"));
    }
    if columns.len() > MAX_COLUMNS {
        return Err(invalid(format!("at most {MAX_COLUMNS} columns, got {}", columns.len())));
    }
    for (i, column) in columns.iter().enumerate() {
        if column.is_empty() {
            return Err(invalid(format!("column {} has no name", i + 1)));
        }
        if columns[..i].iter().any(|c| c.eq_ignore_ascii_case(column)) {
            return Err(invalid(format!("column \"{column}\" is there twice")));
        }
    }
    let given = params.rows.as_deref().unwrap_or_default();
    if given.is_empty() {
        return Err(invalid("give at least one row (a configuration and its values)"));
    }
    if given.len() > MAX_ROWS {
        return Err(invalid(format!("at most {MAX_ROWS} rows, got {}", given.len())));
    }
    let mut rows: Vec<proto::DesignTableRow> = Vec::new();
    for row in given {
        let configuration = row.configuration.trim();
        if configuration.is_empty() {
            return Err(invalid("a row has no configuration name"));
        }
        if rows.iter().any(|r| r.configuration.eq_ignore_ascii_case(configuration)) {
            return Err(invalid(format!("configuration \"{configuration}\" has two rows")));
        }
        if row.values.len() > columns.len() {
            return Err(invalid(format!(
                "\"{configuration}\" has {} values for {} columns",
                row.values.len(),
                columns.len()
            )));
        }
        let mut values: Vec<String> = row.values.iter().map(Cell::text).collect();
        values.resize(columns.len(), String::new());
        rows.push(proto::DesignTableRow { configuration: configuration.to_string(), values });
    }
    Ok(proto::DesignTableRequest {
        action: Some(Action::Write(proto::DesignTableContent { columns, rows })),
    })
}

/// swDesignTableErrors_e.
fn table_error_name(code: i32) -> String {
    match code {
        1 => "a configuration is invalid".into(),
        2 => "the table is corrupt".into(),
        9 => "a value is invalid".into(),
        10 => "a custom property column is invalid".into(),
        11 => "a dimension column is invalid".into(),
        12 => "a feature column is invalid".into(),
        13 => "a keyword column is invalid".into(),
        14 => "a configuration name is invalid".into(),
        15 => "a component state is invalid".into(),
        16 => "a feature state is invalid (use S or U)".into(),
        17 => "a yes/no value is invalid".into(),
        19 => "a column names nothing in the model".into(),
        20 => "a parent configuration is invalid".into(),
        23 => "a feature can't be suppressed".into(),
        24 => "the table is empty".into(),
        25 => "a dimension is negative".into(),
        30 | 31 => "a dimension value is out of range".into(),
        32 => "an equation is invalid".into(),
        33 => "configurations are their own parents".into(),
        other => format!("design table error {other}"),
    }
}

/// A dialog's text without SolidWorks' "<a href=...>Copy all to Clipboard</a>" links.
fn without_links(text: &str) -> String {
    let mut out = text.to_string();
    while let (Some(start), Some(end)) = (out.find("<a "), out.find("</a>")) {
        if end < start {
            break;
        }
        out.replace_range(start..end + 4, "");
    }
    out.trim().to_string()
}

pub fn table_json(reply: &proto::DesignTableResponse) -> Value {
    let mut out = Map::new();
    out.insert("exists".into(), json!(reply.exists));
    out.insert("active".into(), json!(reply.active));
    if let Some(table) = reply.table.as_ref().filter(|_| reply.exists) {
        out.insert("columns".into(), json!(table.columns));
        let rows: Vec<Value> = table
            .rows
            .iter()
            .map(|r| {
                let values: Map<String, Value> = table
                    .columns
                    .iter()
                    .zip(&r.values)
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(c, v)| (c.clone(), json!(v)))
                    .collect();
                json!({ "configuration": r.configuration, "values": values })
            })
            .collect();
        out.insert("rows".into(), Value::Array(rows));
    }
    Value::Object(out)
}

pub fn reply_text(params: &DesignTableParams, reply: &proto::DesignTableResponse, dialogs: &[Closed]) -> String {
    let mut text = match params.action {
        DesignTableAction::Read if !reply.exists => "the document has no design table.".to_string(),
        DesignTableAction::Read => String::new(),
        DesignTableAction::Delete => {
            "deleted the design table; the configurations stay, with the values they have.".to_string()
        }
        DesignTableAction::Write => {
            let rows = reply.table.as_ref().map_or(0, |t| t.rows.len());
            let mut t = format!("wrote the design table ({rows} configurations); the model was rebuilt.");
            if !reply.created_configurations.is_empty() {
                let list: Vec<String> = reply.created_configurations.iter().map(|c| format!("\"{c}\"")).collect();
                t += &format!(" SolidWorks created the configurations {}.", list.join(", "));
            }
            t
        }
    };
    if reply.table_error != 0 {
        text += &format!(" SolidWorks reports a problem with the table: {}.", table_error_name(reply.table_error));
    }
    if !dialogs.is_empty() {
        // Errors name the column or value; a question (delete the configurations no row
        // names?) got the answer that changes least.
        text += " SolidWorks showed dialogs, closed with the button that changes least:";
        for dialog in dialogs {
            text += &format!(" \"{}\" ({});", without_links(&dialog.text), dialog.pressed);
        }
        if reply.table_error != 0 {
            text += " fix those columns or values and write the table again.";
        }
    }
    text += &problems_text(&reply.new_errors);
    if params.action == DesignTableAction::Write {
        text += " Values are in the document's units; the configurations tool lists what each one now has.";
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text += &table_json(reply).to_string();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(configuration: &str, values: Vec<Cell>) -> DesignTableRowParams {
        DesignTableRowParams { configuration: configuration.into(), values }
    }

    fn write(columns: &[&str], rows: Vec<DesignTableRowParams>) -> DesignTableParams {
        DesignTableParams {
            action: DesignTableAction::Write,
            columns: Some(columns.iter().map(|c| c.to_string()).collect()),
            rows: Some(rows),
        }
    }

    #[test]
    fn requests() {
        use proto::design_table_request::Action;
        let p = write(
            &["D1@Boss-Extrude1", " $STATE@Cut-Extrude1 "],
            vec![
                row("Default", vec![Cell::Number(10.0), Cell::Text("U".into())]),
                row(" Long ", vec![Cell::Number(30.5)]),
            ],
        );
        let Some(Action::Write(table)) = request(&p).unwrap().action else { panic!() };
        assert_eq!(table.columns, vec!["D1@Boss-Extrude1", "$STATE@Cut-Extrude1"]);
        assert_eq!(table.rows[0].values, vec!["10", "U"]);
        assert_eq!((table.rows[1].configuration.as_str(), table.rows[1].values.clone()), ("Long", vec!["30.5".to_string(), String::new()]));

        assert!(request(&write(&[], vec![row("A", vec![])])).is_err());
        assert!(request(&write(&["D1@Sketch1"], vec![])).is_err());
        assert!(request(&write(&["D1@Sketch1", "d1@sketch1"], vec![row("A", vec![])])).is_err());
        assert!(request(&write(&["D1@Sketch1"], vec![row("A", vec![]), row("a", vec![])])).is_err());
        assert!(request(&write(&["D1@Sketch1"], vec![row("A", vec![Cell::Number(1.0), Cell::Number(2.0)])])).is_err());
        let read = DesignTableParams { action: DesignTableAction::Read, columns: None, rows: None };
        assert_eq!(request(&read).unwrap().action, None);
        let delete = DesignTableParams { action: DesignTableAction::Delete, columns: Some(vec![]), rows: None };
        assert!(request(&delete).is_err());
    }

    #[test]
    fn cells_from_json() {
        let r: DesignTableRowParams = serde_json::from_value(json!({ "configuration": "A", "values": [30, "S", 2.5] })).unwrap();
        let texts: Vec<String> = r.values.iter().map(Cell::text).collect();
        assert_eq!(texts, vec!["30", "S", "2.5"]);
    }

    #[test]
    fn replies() {
        let reply = proto::DesignTableResponse {
            exists: true,
            table: Some(proto::DesignTableContent {
                columns: vec!["D1@Boss-Extrude1".into(), "$DESCRIPTION".into()],
                rows: vec![proto::DesignTableRow { configuration: "Long".into(), values: vec!["30".into(), String::new()] }],
            }),
            created_configurations: vec!["Long".into()],
            active: "Default".into(),
            table_error: 0,
            new_errors: vec![],
        };
        let p = write(&["D1@Boss-Extrude1"], vec![row("Long", vec![])]);
        let dialogs = vec![Closed {
            title: "SOLIDWORKS".into(),
            text: "G1@equations <a href=\"Copy all to Clipboard\">Copy all to Clipboard</a>".into(),
            pressed: "OK".into(),
        }];
        let text = reply_text(&p, &reply, &dialogs);
        assert!(
            text.starts_with(
                "wrote the design table (1 configurations); the model was rebuilt. SolidWorks created the \
                 configurations \"Long\". SolidWorks showed dialogs, closed with the button that changes least: \
                 \"G1@equations\" (OK);"
            ),
            "{text}"
        );
        let json: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(json["rows"][0], json!({ "configuration": "Long", "values": { "D1@Boss-Extrude1": "30" } }));

        let none = proto::DesignTableResponse { active: "Default".into(), ..Default::default() };
        let read = DesignTableParams { action: DesignTableAction::Read, columns: None, rows: None };
        assert!(reply_text(&read, &none, &[]).starts_with("the document has no design table.\n"));
    }
}
