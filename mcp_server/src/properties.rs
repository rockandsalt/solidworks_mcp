//! Document properties: custom_properties, drawing_properties and
//! part_material.
//!
//! SolidWorks has two things called "material". A part's physical material
//! (IPartDoc::SetMaterialPropertyName2) is an entry of a material database:
//! it sets the density, so the mass, and the appearance and section hatch.
//! A "Material" custom property is text that title blocks and BOMs show.
//! The two are only connected when the property's value links the physical
//! one ("SW-Material@part.SLDPRT"). part_material changes the first,
//! drawing_properties' `material` the second, and the add-in won't overwrite
//! such a link with text unless asked (replace_links).
//!
//! drawing_properties sends part number, revision, description and material
//! where the sheet's title block reads them from: the property a format note
//! links whose name is one of the usual spellings (PartNo, Rev, ...), on the
//! drawing ($PRP) or the sheet's model ($PRPSHEET); otherwise a default home.
//! A part number also becomes the model configuration's BOM part number,
//! which is what bills of materials list.

use std::collections::BTreeMap;
use std::path::Path;

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::inspect::round;
use crate::sheet_format::{Field, PropertyOf, PropertyValue};

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn text(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

fn check_name(name: &str) -> Result<(), McpError> {
    if name.is_empty() {
        return Err(invalid("an empty property name"));
    }
    if name.starts_with("SW-") {
        return Err(invalid(format!("{name} is SolidWorks' own and can't be set")));
    }
    Ok(())
}

// ---- custom_properties ----------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CustomPropertiesParams {
    /// An open document by title or full path (e.g. a part a BOM lists, for its Description).
    /// Default: the active document.
    #[serde(default)]
    pub document: Option<String>,
    /// With a drawing active: the model this sheet takes its properties from (what $PRPSHEET
    /// in its title block shows); "*" = the active sheet. Not with document.
    #[serde(default)]
    pub sheet: Option<String>,
    /// A configuration's own properties. Default: the file's.
    #[serde(default)]
    pub configuration: Option<String>,
    /// Properties to set (added if missing, as text).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Properties to delete.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Parts and assemblies: the part number bills of materials list for the configuration
    /// (`configuration`, else the active one / the sheet view's), as user-specified text.
    #[serde(default)]
    pub bom_part_number: Option<String>,
    /// Overwrite or remove a property linked to a value SolidWorks works out, e.g. Material =
    /// "SW-Material@part.SLDPRT" (shows the physical material), Weight = "SW-Mass@...".
    /// Default false: refused.
    #[serde(default)]
    pub replace_links: bool,
}

pub fn properties_request(params: &CustomPropertiesParams) -> Result<proto::CustomPropertiesRequest, McpError> {
    let document = text(params.document.as_deref());
    let sheet = text(params.sheet.as_deref());
    if !document.is_empty() && !sheet.is_empty() {
        return Err(invalid("give document or sheet, not both"));
    }
    for name in params.set.keys() {
        check_name(name.trim())?;
    }
    Ok(proto::CustomPropertiesRequest {
        sheet_name: sheet,
        configuration: text(params.configuration.as_deref()),
        set: params
            .set
            .iter()
            .map(|(k, v)| proto::CustomProperty { name: k.trim().to_string(), value: v.clone(), ..Default::default() })
            .collect(),
        remove: params.remove.iter().map(|r| r.trim().to_string()).collect(),
        document,
        replace_links: params.replace_links,
        bom_part_number: text(params.bom_part_number.as_deref()),
    })
}

fn stem(title: &str) -> &str {
    Path::new(title).file_stem().and_then(|s| s.to_str()).unwrap_or(title)
}

/// What a BOM lists as the configuration's part number, and why.
fn bom_json(reply: &proto::CustomPropertiesResponse) -> Value {
    let title = reply.document.as_ref().map(|d| d.title.as_str()).unwrap_or_default();
    let (shows, source) = match reply.bom_part_number_source {
        1 => (json!(stem(title)), "document name"),
        2 => (json!(reply.bom_configuration), "configuration name"),
        4 => (Value::Null, "the parent assembly's"),
        8 => (json!(reply.bom_part_number), "user specified"),
        _ => (Value::Null, "unknown"),
    };
    json!({ "configuration": reply.bom_configuration, "part_number": shows, "source": source })
}

pub fn properties_json(reply: &proto::CustomPropertiesResponse) -> Value {
    let props: Vec<Value> = reply
        .properties
        .iter()
        .map(|p| {
            let mut e = Map::new();
            e.insert("name".into(), json!(p.name));
            e.insert("value".into(), json!(p.value));
            if p.resolved != p.value {
                e.insert("resolved".into(), json!(p.resolved));
            }
            Value::Object(e)
        })
        .collect();
    let doc = reply.document.as_ref();
    let mut out = json!({
        "document": doc.map(|d| d.title.clone()).unwrap_or_default(),
        "file": doc.map(|d| d.path.clone()).unwrap_or_default(),
        "configuration": if reply.configuration.is_empty() { Value::Null } else { json!(reply.configuration) },
        "properties": props,
    });
    if !reply.bom_configuration.is_empty() {
        out["bom"] = bom_json(reply);
    }
    out
}

// ---- drawing_properties ------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DrawingPropertiesParams {
    /// The sheet whose model ($PRPSHEET: the part or assembly its views show) gets the model's
    /// properties. Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// The part number. Goes to the property the title block shows for it (PartNo,
    /// PartNumber, Number, ...), else the model's "PartNo"; with `bom` also the model's BOM
    /// part number.
    #[serde(default)]
    pub part_number: Option<String>,
    /// The revision, e.g. "B": the title block's (Revision, Rev, ...), else the drawing's
    /// "Revision".
    #[serde(default)]
    pub revision: Option<String>,
    /// The description: the title block's, else the model's "Description" (a BOM's
    /// DESCRIPTION column shows it).
    #[serde(default)]
    pub description: Option<String>,
    /// The material designation as text, e.g. "EN AW-6061-T6", "S235JR": the title block's
    /// Material, else the model's "Material" property. Text only -- the part's physical
    /// material (density, mass) is part_material's. Refused over a Material property that
    /// shows the physical material (a link) unless replace_links.
    #[serde(default)]
    pub material: Option<String>,
    /// More custom properties of the drawing ($PRP), e.g. {"DrawnBy": "MW", "DrawingNo":
    /// "D-1001"}.
    #[serde(default)]
    pub drawing: BTreeMap<String, String>,
    /// More custom properties of the sheet's model ($PRPSHEET), e.g. {"Finish": "Anodized"}.
    #[serde(default)]
    pub model: BTreeMap<String, String>,
    /// part_number also becomes the BOM part number of the model's configuration (the one the
    /// sheet's view shows), which bills of materials list. Default true.
    #[serde(default)]
    pub bom: Option<bool>,
    /// Overwrite properties linked to a value SolidWorks works out (e.g. Material =
    /// "SW-Material@part.SLDPRT") with the text given. Default false: refused.
    #[serde(default)]
    pub replace_links: bool,
}

/// The fields drawing_properties knows by meaning rather than by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    PartNumber,
    Revision,
    Description,
    Material,
}

impl Known {
    const ALL: [Known; 4] = [Known::PartNumber, Known::Revision, Known::Description, Known::Material];

    fn param(self) -> &'static str {
        match self {
            Known::PartNumber => "part_number",
            Known::Revision => "revision",
            Known::Description => "description",
            Known::Material => "material",
        }
    }

    /// Property names a title block may use for it, lowercased, letters and digits only.
    fn spellings(self) -> &'static [&'static str] {
        match self {
            Known::PartNumber => &["partno", "partnumber", "partnum", "number", "pn"],
            Known::Revision => &["revision", "rev", "revno", "revisionlevel"],
            Known::Description => &["description", "desc"],
            Known::Material => &["material", "materialdesignation", "materialspec"],
        }
    }

    /// Where it goes when the title block doesn't show it.
    fn home(self) -> (&'static str, PropertyOf) {
        match self {
            Known::PartNumber => ("PartNo", PropertyOf::Model),
            Known::Revision => ("Revision", PropertyOf::Drawing),
            Known::Description => ("Description", PropertyOf::Model),
            Known::Material => ("Material", PropertyOf::Model),
        }
    }

    fn value(self, params: &DrawingPropertiesParams) -> Option<&str> {
        match self {
            Known::PartNumber => params.part_number.as_deref(),
            Known::Revision => params.revision.as_deref(),
            Known::Description => params.description.as_deref(),
            Known::Material => params.material.as_deref(),
        }
    }
}

fn spelling(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// A property drawing_properties sets, and why there.
#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    pub property: PropertyValue,
    /// The parameter it came from (part_number, ..., or drawing / model).
    pub from: &'static str,
    /// The title block notes showing it.
    pub notes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct DrawingPlan {
    pub routed: Vec<Routed>,
    /// For the model's configuration.
    pub bom_part_number: String,
}

impl DrawingPlan {
    pub fn of(&self, of: PropertyOf) -> Vec<proto::CustomProperty> {
        self.routed
            .iter()
            .filter(|r| r.property.of == of)
            .map(|r| proto::CustomProperty {
                name: r.property.name.clone(),
                value: r.property.value.clone(),
                ..Default::default()
            })
            .collect()
    }

    pub fn needs_model(&self) -> bool {
        !self.bom_part_number.is_empty() || self.routed.iter().any(|r| r.property.of == PropertyOf::Model)
    }

    pub fn is_empty(&self) -> bool {
        self.routed.is_empty() && self.bom_part_number.is_empty()
    }
}

pub fn has_known(params: &DrawingPropertiesParams) -> bool {
    Known::ALL.iter().any(|k| k.value(params).is_some())
}

/// Where each value goes, given the sheet's title block fields.
pub fn plan(params: &DrawingPropertiesParams, fields: &[Field]) -> Result<DrawingPlan, McpError> {
    let mut out = DrawingPlan::default();
    for known in Known::ALL {
        let Some(value) = known.value(params) else { continue };
        let value = value.trim();
        let mut targets: Vec<(String, PropertyOf, Vec<String>)> = Vec::new();
        for f in fields.iter().filter(|f| !f.property.starts_with("SW-")) {
            if !known.spellings().contains(&spelling(&f.property).as_str()) {
                continue;
            }
            match targets.iter_mut().find(|t| t.0.eq_ignore_ascii_case(&f.property) && t.1 == f.of) {
                Some(t) if !t.2.contains(&f.note) => t.2.push(f.note.clone()),
                Some(_) => {}
                None => targets.push((f.property.clone(), f.of, vec![f.note.clone()])),
            }
        }
        if targets.is_empty() {
            let (name, of) = known.home();
            targets.push((name.to_string(), of, Vec::new()));
        }
        for (name, of, notes) in targets {
            out.routed.push(Routed {
                property: PropertyValue { name, of, value: value.to_string() },
                from: known.param(),
                notes,
            });
        }
        if known == Known::PartNumber && params.bom.unwrap_or(true) {
            if value.is_empty() {
                return Err(invalid("an empty part_number can't be a BOM part number (bom: false sets only the property)"));
            }
            out.bom_part_number = value.to_string();
        }
    }
    for (map, of, from) in [(&params.drawing, PropertyOf::Drawing, "drawing"), (&params.model, PropertyOf::Model, "model")] {
        for (name, value) in map {
            let name = name.trim();
            check_name(name)?;
            let notes = fields
                .iter()
                .filter(|f| f.of == of && f.property.eq_ignore_ascii_case(name))
                .map(|f| f.note.clone())
                .collect();
            out.routed.push(Routed {
                property: PropertyValue { name: name.to_string(), of, value: value.clone() },
                from,
                notes,
            });
        }
    }
    // The same property from two parameters (e.g. revision and drawing.Revision).
    for (i, a) in out.routed.iter().enumerate() {
        if let Some(b) = out.routed[..i]
            .iter()
            .find(|b| b.property.of == a.property.of && b.property.name.eq_ignore_ascii_case(&a.property.name))
        {
            return Err(invalid(format!(
                "{} and {} both set the {}'s {:?}; give it once",
                b.from,
                a.from,
                if a.property.of == PropertyOf::Model { "model" } else { "drawing" },
                a.property.name
            )));
        }
    }
    Ok(out)
}

pub fn plan_request(plan: &DrawingPlan, of: PropertyOf, sheet: &str, replace_links: bool) -> proto::CustomPropertiesRequest {
    let model = of == PropertyOf::Model;
    proto::CustomPropertiesRequest {
        sheet_name: if model { sheet.to_string() } else { String::new() },
        set: plan.of(of),
        replace_links,
        bom_part_number: if model { plan.bom_part_number.clone() } else { String::new() },
        ..Default::default()
    }
}

fn title(reply: &proto::CustomPropertiesResponse) -> String {
    reply.document.as_ref().map(|d| d.title.clone()).unwrap_or_default()
}

pub fn drawing_properties_reply(
    sheet: &str,
    plan: &DrawingPlan,
    drawing: &proto::CustomPropertiesResponse,
    model: Option<&proto::CustomPropertiesResponse>,
    material: Option<&proto::PartMaterialResponse>,
) -> String {
    let mut lines = Vec::new();
    let set_line = |of: PropertyOf| -> Vec<String> {
        plan.routed
            .iter()
            .filter(|r| r.property.of == of)
            .map(|r| {
                let mut s = format!("{}={:?}", r.property.name, r.property.value);
                if !r.notes.is_empty() {
                    s += &format!(" (title block: {})", r.notes.join(", "));
                }
                s
            })
            .collect()
    };
    let on_drawing = set_line(PropertyOf::Drawing);
    if !on_drawing.is_empty() {
        lines.push(format!("Set on the drawing {}: {}.", title(drawing), on_drawing.join(", ")));
    }
    if let Some(model) = model {
        let on_model = set_line(PropertyOf::Model);
        let name = title(model);
        if !on_model.is_empty() {
            lines.push(format!("Set on {sheet}'s model {name}: {}.", on_model.join(", ")));
        }
        if !plan.bom_part_number.is_empty() {
            lines.push(format!(
                "{}'s BOM part number (configuration {}) is now {:?}: bills of materials list it.",
                name, model.bom_configuration, plan.bom_part_number
            ));
        }
        if !on_model.is_empty() || !plan.bom_part_number.is_empty() {
            lines.push(format!("Save {name} too: its properties are stored in it."));
        }
    }
    if plan.routed.iter().any(|r| r.from == "material") {
        match material {
            Some(m) if m.material.is_empty() => lines.push(format!(
                "The material is a designation (text). {} has no physical material (density 1000 kg/m^3); \
                 part_material assigns one.",
                m.document.as_ref().map(|d| d.title.as_str()).unwrap_or_default()
            )),
            Some(m) => lines.push(format!(
                "The material is a designation (text); the physical material (density, mass) is still {} \
                 -- part_material changes that.",
                m.material
            )),
            None => {}
        }
    }
    if plan.is_empty() {
        lines.push(format!("{sheet}'s properties (nothing given to set):"));
    }
    let mut out = json!({ "sheet": sheet, "drawing": properties_json(drawing) });
    if let Some(model) = model {
        out["model"] = properties_json(model);
    }
    if let Some(m) = material {
        out["physical_material"] = material_json(m, None);
    }
    lines.push(out.to_string());
    lines.join("\n")
}

// ---- part_material -------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PartMaterialParams {
    /// An open part by title or full path. Default: the active document.
    #[serde(default)]
    pub document: Option<String>,
    /// With a drawing active: the part this sheet shows (its model); "*" = the active sheet.
    /// Not with document.
    #[serde(default)]
    pub sheet: Option<String>,
    /// A configuration. Default: the sheet view's, else the active one.
    #[serde(default)]
    pub configuration: Option<String>,
    /// Assign this material from SolidWorks' material databases, e.g. "AISI 1020", "6061-T6
    /// (SS)", "ABS PC" (any case; `search` finds names). Omit to only read.
    #[serde(default)]
    pub material: Option<String>,
    /// The database, when the name is in several: "solidworks materials", "SolidWorks DIN
    /// Materials", ...
    #[serde(default)]
    pub database: Option<String>,
    /// List the materials whose name or class has all these words, e.g. "6061",
    /// "stainless", "plastics"; "*" = the classes.
    #[serde(default)]
    pub search: Option<String>,
    /// Remove the part's material.
    #[serde(default)]
    pub remove: bool,
    /// Also make the part's "Material" custom property show this material (a link,
    /// "SW-Material@<part>"), so title blocks and BOMs reading Material follow it. Replaces a
    /// designation text there.
    #[serde(default)]
    pub link_property: bool,
}

/// One material of a database file.
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub name: String,
    pub class: String,
    /// The file name without .sldmat.
    pub database: String,
    /// kg/m^3.
    pub density: Option<f64>,
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let len = tag[start..].find('"')?;
    Some(unescape(&tag[start..start + len]))
}

/// The materials of a .sldmat file's XML: `<classification name=..>`
/// holding `<material name=..>`s, each with a `<DENS value=..>` (kg/m^3).
pub fn parse_database(database: &str, xml: &str) -> Vec<Material> {
    let mut out: Vec<Material> = Vec::new();
    let mut class = String::new();
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if tag.starts_with("classification ") {
            class = attribute(tag, "name").unwrap_or_default();
        } else if tag.starts_with("material ") {
            if let Some(name) = attribute(tag, "name") {
                out.push(Material { name, class: class.clone(), database: database.to_string(), density: None });
            }
        } else if tag.starts_with("DENS ") {
            if let Some(m) = out.last_mut().filter(|m| m.density.is_none()) {
                m.density = attribute(tag, "value").and_then(|v| v.parse().ok());
            }
        }
        rest = &rest[end..];
    }
    out
}

/// .sldmat files are UTF-16 with a byte order mark; UTF-8 is taken too.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |be: bool| {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, ..] => utf16(false),
        [0xFE, 0xFF, ..] => utf16(true),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Every material of the *.sldmat files in `folders` (SolidWorks and the
/// server run on the same machine). Unreadable folders are skipped.
pub fn load_databases(folders: &[String]) -> Vec<Material> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        let mut files: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sldmat")))
            .collect();
        files.sort();
        for path in files {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            if seen.iter().any(|s: &String| s.eq_ignore_ascii_case(&name)) {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                out.extend(parse_database(&name, &decode(&bytes)));
                seen.push(name);
            }
        }
    }
    out
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Materials whose name or class contains every word of `query`.
pub fn search<'a>(materials: &'a [Material], query: &str) -> Vec<&'a Material> {
    let wanted = words(query);
    materials
        .iter()
        .filter(|m| {
            let have = format!("{} {}", m.name, m.class).to_lowercase();
            wanted.iter().all(|w| have.contains(w.as_str()))
        })
        .collect()
}

/// Names close to an unknown one: those with the most of its words at the
/// start of one of theirs ("aisi 102" → "AISI 1020", ...). One-letter words
/// match too much ("Unobtainium 9" → every name with a 9...) to count.
pub fn suggestions<'a>(materials: &'a [Material], name: &str) -> Vec<&'a Material> {
    let wanted: Vec<String> = words(name).into_iter().filter(|w| w.chars().count() > 1).collect();
    let mut scored: Vec<(usize, &Material)> = materials
        .iter()
        .map(|m| {
            let have = words(&m.name);
            (wanted.iter().filter(|w| have.iter().any(|h| h.starts_with(w.as_str()))).count(), m)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    let best = scored.iter().map(|(score, _)| *score).max().unwrap_or(0);
    scored.retain(|(score, _)| *score == best);
    scored.sort_by(|a, b| a.1.name.len().cmp(&b.1.name.len()));
    scored.into_iter().take(8).map(|(_, m)| m).collect()
}

fn names(list: &[&Material]) -> String {
    list.iter().map(|m| format!("{:?} ({})", m.name, m.database)).collect::<Vec<_>>().join(", ")
}

/// The database entry `name` (any case) means; `database` picks among
/// several. With no databases read, the name goes to SolidWorks as given.
pub fn resolve<'a>(materials: &'a [Material], name: &str, database: &str) -> Result<Option<&'a Material>, McpError> {
    if materials.is_empty() {
        return Ok(None);
    }
    let found: Vec<&Material> = materials
        .iter()
        .filter(|m| m.name.eq_ignore_ascii_case(name))
        .filter(|m| database.is_empty() || m.database.eq_ignore_ascii_case(database))
        .collect();
    if found.is_empty() {
        let near = suggestions(materials, name);
        let mut message = format!("no material {name:?}");
        if !database.is_empty() {
            message += &format!(" in database {database:?}");
        }
        if near.is_empty() {
            message += " in SolidWorks' material databases (search lists names, e.g. search: \"steel\")";
        } else {
            message += &format!("; close: {} (search lists more)", names(&near));
        }
        return Err(invalid(message));
    }
    let exact: Vec<&Material> = found.iter().copied().filter(|m| m.name == name).collect();
    let pick = if exact.len() == 1 { exact } else { found };
    if pick.len() > 1 {
        if let Some(m) = pick.iter().find(|m| m.database.eq_ignore_ascii_case("solidworks materials")) {
            return Ok(Some(m));
        }
        return Err(invalid(format!("{name:?} is in several databases: {}; give database", names(&pick))));
    }
    Ok(Some(pick[0]))
}

pub fn material_request(params: &PartMaterialParams) -> Result<proto::PartMaterialRequest, McpError> {
    let document = text(params.document.as_deref());
    let sheet = text(params.sheet.as_deref());
    if !document.is_empty() && !sheet.is_empty() {
        return Err(invalid("give document or sheet, not both"));
    }
    let material = text(params.material.as_deref());
    if params.remove && !material.is_empty() {
        return Err(invalid("give material or remove, not both"));
    }
    if params.link_property && params.remove {
        return Err(invalid("link_property needs a material, not remove"));
    }
    if params.search.is_some() && (!material.is_empty() || params.remove) {
        return Err(invalid("search only lists materials; assign one in another call"));
    }
    Ok(proto::PartMaterialRequest {
        document,
        sheet_name: sheet,
        configuration: text(params.configuration.as_deref()),
        ..Default::default()
    })
}

/// The custom property value that makes "Material" show the physical
/// material of `part` (its window title, e.g. "plate.SLDPRT").
pub fn material_link(part: &str) -> String {
    format!("\"SW-Material@{part}\"")
}

fn mass_text(kg: f64) -> String {
    if kg < 1.0 { format!("{} g", round(kg * 1000.0, 3)) } else { format!("{} kg", round(kg, 4)) }
}

pub fn material_json(reply: &proto::PartMaterialResponse, entry: Option<&Material>) -> Value {
    let mut out = Map::new();
    out.insert("part".into(), json!(reply.document.as_ref().map(|d| d.title.clone()).unwrap_or_default()));
    out.insert("configuration".into(), json!(reply.configuration));
    out.insert("material".into(), if reply.material.is_empty() { Value::Null } else { json!(reply.material) });
    if !reply.database.is_empty() {
        out.insert("database".into(), json!(reply.database));
    }
    if let Some(m) = entry {
        out.insert("class".into(), json!(m.class));
    }
    if reply.has_mass {
        out.insert("density_kg_m3".into(), json!(round(reply.density, 3)));
        out.insert("mass_kg".into(), json!(round(reply.mass, 6)));
    }
    for (key, p) in [("material_property", &reply.material_property), (
        "configuration_material_property",
        &reply.configuration_material_property,
    )] {
        if let Some(p) = p {
            let mut e = Map::new();
            e.insert("value".into(), json!(p.value));
            e.insert("shows".into(), json!(p.resolved));
            e.insert("linked".into(), json!(p.value.contains("SW-Material")));
            out.insert(key.into(), Value::Object(e));
        }
    }
    Value::Object(out)
}

/// What the part's "Material" custom property says about the physical one.
fn property_line(reply: &proto::PartMaterialResponse) -> String {
    let part = reply.document.as_ref().map(|d| d.title.as_str()).unwrap_or_default();
    let p = reply.configuration_material_property.as_ref().or(reply.material_property.as_ref());
    match p {
        Some(p) if p.value.contains("SW-Material") => format!(
            "Its \"Material\" property links the physical material, so title blocks and BOMs reading \
             Material show {:?}.",
            p.resolved
        ),
        Some(p) => format!(
            "Its \"Material\" property is the text {:?}, a designation of its own: title blocks and \
             BOMs reading Material still show that (drawing_properties material changes it; \
             link_property makes it follow the physical material).",
            p.resolved
        ),
        None => format!(
            "{part} has no \"Material\" property, so title blocks reading $PRPSHEET:\"Material\" show \
             nothing (link_property makes it show the physical material; drawing_properties material sets \
             a designation text)."
        ),
    }
}

pub fn material_reply(
    before: &proto::PartMaterialResponse,
    after: &proto::PartMaterialResponse,
    entry: Option<&Material>,
    changed: bool,
    linked: bool,
) -> String {
    let part = after.document.as_ref().map(|d| d.title.clone()).unwrap_or_default();
    let describe = |r: &proto::PartMaterialResponse| {
        let mut s = if r.material.is_empty() {
            "no material".to_string()
        } else {
            format!("{} ({})", r.material, r.database)
        };
        if r.has_mass {
            s += &format!(", density {} kg/m^3, mass {}", round(r.density, 1), mass_text(r.mass));
        }
        s
    };
    let mut lines = vec![format!(
        "{part} (configuration {}): physical material {}.",
        after.configuration,
        describe(after)
    )];
    if changed {
        lines.push(format!("Was: {}.", describe(before)));
    }
    if !after.has_mass {
        lines.push(format!(
            "No mass shown: configuration {} isn't the active one, or the part has no body.",
            after.configuration
        ));
    }
    if linked {
        lines.push(format!("Its \"Material\" property now links it ({}).", material_link(&part)));
    } else {
        lines.push(property_line(after));
    }
    if changed || linked {
        lines.push(format!("Save {part}: the material is stored in the part."));
    }
    lines.push(material_json(after, entry).to_string());
    lines.join("\n")
}

pub fn search_reply(reply: &proto::PartMaterialResponse, materials: &[Material], query: &str) -> String {
    const LIMIT: usize = 60;
    if materials.is_empty() {
        return format!(
            "no material databases (*.sldmat) found in {}",
            if reply.database_folders.is_empty() { "SolidWorks' settings".to_string() } else { reply.database_folders.join("; ") }
        );
    }
    if query.trim() == "*" {
        let mut classes: Vec<(String, String, usize)> = Vec::new();
        for m in materials {
            match classes.iter_mut().find(|c| c.0 == m.database && c.1 == m.class) {
                Some(c) => c.2 += 1,
                None => classes.push((m.database.clone(), m.class.clone(), 1)),
            }
        }
        let list: Vec<Value> =
            classes.iter().map(|(db, class, n)| json!({ "database": db, "class": class, "materials": n })).collect();
        return format!(
            "{} materials in {} classes; search with words of a class or name to list them.\n{}",
            materials.len(),
            classes.len(),
            Value::Array(list)
        );
    }
    let found = search(materials, query);
    let list: Vec<Value> = found
        .iter()
        .take(LIMIT)
        .map(|m| {
            let mut e = Map::new();
            e.insert("name".into(), json!(m.name));
            e.insert("class".into(), json!(m.class));
            e.insert("database".into(), json!(m.database));
            if let Some(d) = m.density {
                e.insert("density_kg_m3".into(), json!(round(d, 1)));
            }
            Value::Object(e)
        })
        .collect();
    let mut head = format!("{} material(s) match {query:?}", found.len());
    if found.len() > LIMIT {
        head += &format!(" (first {LIMIT}; add words to narrow it)");
    }
    if found.is_empty() {
        head += "; \"*\" lists the classes";
    }
    let current = if reply.material.is_empty() { "none".to_string() } else { reply.material.clone() };
    format!(
        "{head}. {}'s material: {current}.\n{}",
        reply.document.as_ref().map(|d| d.title.as_str()).unwrap_or_default(),
        Value::Array(list)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(property: &str, of: PropertyOf, note: &str) -> Field {
        Field { property: property.into(), of, note: note.into(), shows: String::new(), at: None }
    }

    #[test]
    fn known_fields_follow_the_title_block() {
        let fields = vec![
            field("PartNumber", PropertyOf::Model, "DetailItem1"),
            field("Rev", PropertyOf::Drawing, "DetailItem2"),
            field("SW-Material", PropertyOf::Model, "DetailItem3"),
        ];
        let params = DrawingPropertiesParams {
            part_number: Some("P-100".into()),
            revision: Some("B".into()),
            material: Some("S235JR".into()),
            drawing: [("DrawnBy".to_string(), "MW".to_string())].into(),
            ..Default::default()
        };
        let plan = plan(&params, &fields).unwrap();
        let got: Vec<(&str, PropertyOf, &str)> =
            plan.routed.iter().map(|r| (r.property.name.as_str(), r.property.of, r.from)).collect();
        assert_eq!(
            got,
            vec![
                ("PartNumber", PropertyOf::Model, "part_number"),
                ("Rev", PropertyOf::Drawing, "revision"),
                // SW-Material is SolidWorks' own: the default home instead.
                ("Material", PropertyOf::Model, "material"),
                ("DrawnBy", PropertyOf::Drawing, "drawing"),
            ]
        );
        assert_eq!(plan.routed[0].notes, vec!["DetailItem1".to_string()]);
        assert_eq!(plan.bom_part_number, "P-100");
        assert!(plan.needs_model());
        assert_eq!(plan.of(PropertyOf::Drawing).len(), 2);
    }

    #[test]
    fn defaults_without_a_title_block() {
        let params = DrawingPropertiesParams {
            part_number: Some("P-1".into()),
            revision: Some("A".into()),
            description: Some("Bracket".into()),
            bom: Some(false),
            ..Default::default()
        };
        let plan = plan(&params, &[]).unwrap();
        let got: Vec<(&str, PropertyOf)> = plan.routed.iter().map(|r| (r.property.name.as_str(), r.property.of)).collect();
        assert_eq!(
            got,
            vec![("PartNo", PropertyOf::Model), ("Revision", PropertyOf::Drawing), ("Description", PropertyOf::Model)]
        );
        assert!(plan.bom_part_number.is_empty());
    }

    #[test]
    fn a_property_given_twice_is_refused() {
        let params = DrawingPropertiesParams {
            revision: Some("B".into()),
            drawing: [("revision".to_string(), "C".to_string())].into(),
            ..Default::default()
        };
        assert!(plan(&params, &[]).is_err());
        let params = DrawingPropertiesParams { model: [("SW-Mass".to_string(), "1".to_string())].into(), ..Default::default() };
        assert!(plan(&params, &[]).is_err());
    }

    const XML: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<mstns:materials xmlns:mstns="http://www.solidworks.com/sldmaterials" version="2008.03">
  <curves id="curve0"><point x="1.0" y="1.0" /></curves>
  <classification name="Steel">
    <material name="AISI 1020" matid="2">
      <physicalproperties><EX displayname="Elastic modulus" value="2e11" /><DENS displayname="Mass density" value="7900" /></physicalproperties>
    </material>
    <material name="AISI 1020 Steel, Cold Rolled" matid="3">
      <physicalproperties><DENS displayname="Mass density" value="7870" /></physicalproperties>
    </material>
  </classification>
  <classification name="Aluminium Alloys">
    <material name="6061 Alloy &amp; more"><physicalproperties><DENS value="2700" /></physicalproperties></material>
  </classification>
</mstns:materials>"#;

    #[test]
    fn a_database_is_parsed() {
        let m = parse_database("solidworks materials", XML);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0], Material { name: "AISI 1020".into(), class: "Steel".into(), database: "solidworks materials".into(), density: Some(7900.0) });
        assert_eq!(m[2].name, "6061 Alloy & more");
        assert_eq!(m[2].class, "Aluminium Alloys");
        assert_eq!(m[2].density, Some(2700.0));
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(XML.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(parse_database("x", &decode(&utf16)).len(), 3);
    }

    #[test]
    fn names_resolve_in_any_case_with_suggestions() {
        let mut m = parse_database("solidworks materials", XML);
        assert_eq!(resolve(&m, "aisi 1020", "").unwrap().unwrap().name, "AISI 1020");
        let err = resolve(&m, "AISI 102", "").unwrap_err();
        assert!(err.message.contains("\"AISI 1020\""), "{}", err.message);
        let near: Vec<&str> = suggestions(&m, "aisi 102").iter().map(|m| m.name.as_str()).collect();
        assert_eq!(near, vec!["AISI 1020", "AISI 1020 Steel, Cold Rolled"]);
        assert!(suggestions(&m, "Unobtainium 9").is_empty());
        assert_eq!(search(&m, "steel cold").len(), 1);
        assert_eq!(search(&m, "alumin").len(), 1);
        // In two databases: SolidWorks' main one wins unless database says.
        m.push(Material { name: "AISI 1020".into(), class: "Steel".into(), database: "custom".into(), density: None });
        assert_eq!(resolve(&m, "AISI 1020", "").unwrap().unwrap().database, "solidworks materials");
        assert_eq!(resolve(&m, "AISI 1020", "Custom").unwrap().unwrap().database, "custom");
        assert!(resolve(&[], "anything", "").unwrap().is_none());
    }

    #[test]
    fn the_bom_part_number_says_where_it_comes_from() {
        let reply = proto::CustomPropertiesResponse {
            document: Some(proto::Document { title: "plate.SLDPRT".into(), ..Default::default() }),
            bom_configuration: "Default".into(),
            bom_part_number_source: 1,
            ..Default::default()
        };
        assert_eq!(bom_json(&reply)["part_number"], "plate");
        let reply = proto::CustomPropertiesResponse { bom_part_number_source: 8, bom_part_number: "P-100".into(), ..reply };
        assert_eq!(bom_json(&reply)["part_number"], "P-100");
        assert_eq!(bom_json(&reply)["source"], "user specified");
    }
}
