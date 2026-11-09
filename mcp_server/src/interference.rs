//! check_interference: where the components of an assembly overlap, as
//! SolidWorks' Interference Detection finds them, as JSON for the model.
//!
//! The add-in checks the assembly as it stands (the active one, or the one a
//! drawing view shows) and lists every interference with its components,
//! volume and box. This module narrows that to the components asked about,
//! sorts the biggest overlaps first and names components by id where ids
//! work: in the active assembly. A drawing's components go by name, since
//! ids belong to the document they came from.

use std::collections::HashMap;

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::assembly::component_json;
use crate::inspect::{IdRegistry, Kind, mm, mm3, point_mm};

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CheckInterferenceParams {
    /// Only interferences involving these components: "component:N" ids from inspect_model
    /// (assemblies), or component names as SolidWorks shows them, e.g. "bracket-1" (also in
    /// drawings). A subassembly covers everything in it. Default: the whole assembly.
    #[serde(default)]
    pub components: Option<Vec<String>>,
    /// Drawings: the view whose assembly to check, e.g. "Drawing View1". Default: the one
    /// assembly the drawing's views show.
    #[serde(default)]
    pub view: Option<String>,
    /// Also report faces that only touch (coincident, volume 0).
    #[serde(default)]
    pub include_touching: bool,
    /// Treat each subassembly as one solid: overlaps inside it aren't checked.
    #[serde(default)]
    pub treat_subassemblies_as_components: bool,
    /// Also check the bodies of one multibody part against each other.
    #[serde(default)]
    pub include_multibody_parts: bool,
    /// Also check hidden components and bodies. Default false (they're ignored).
    #[serde(default)]
    pub include_hidden: bool,
}

/// A component the reply is narrowed to.
#[derive(Debug, Clone, PartialEq)]
pub enum Wanted {
    PersistId(Vec<u8>),
    Name(String),
}

impl CheckInterferenceParams {
    pub fn request(&self) -> proto::CheckInterferenceRequest {
        proto::CheckInterferenceRequest {
            view: self.view.as_deref().map(str::trim).unwrap_or_default().to_string(),
            include_coincident: self.include_touching,
            treat_subassemblies_as_components: self.treat_subassemblies_as_components,
            include_multibody_parts: self.include_multibody_parts,
            include_hidden: self.include_hidden,
        }
    }

    /// `components`, ids resolved to persist ids (`resolve`), names kept.
    pub fn wanted(
        &self,
        resolve: impl Fn(&str) -> Result<Vec<u8>, McpError>,
    ) -> Result<Vec<Wanted>, McpError> {
        let mut out = Vec::new();
        for c in self.components.iter().flatten() {
            let c = c.trim();
            if c.is_empty() {
                continue;
            }
            let w = match Kind::of_id(c) {
                Some(Kind::Component) => {
                    if self.view.is_some() {
                        return Err(McpError::invalid_params(
                            format!(
                                "{c}: in a drawing, give components by name (e.g. \"bracket-1\"); ids \
                                 only work in the assembly they came from"
                            ),
                            None,
                        ));
                    }
                    Wanted::PersistId(resolve(c)?)
                }
                Some(_) => {
                    return Err(McpError::invalid_params(
                        format!("{c} isn't a component -- give \"component:N\" ids or component names"),
                        None,
                    ));
                }
                None => Wanted::Name(c.to_string()),
            };
            if !out.contains(&w) {
                out.push(w);
            }
        }
        Ok(out)
    }
}

/// Whether component `pid` is one of `wanted` or inside one of them.
fn is_wanted(pid: &[u8], name: &str, wanted: &[Wanted], parents: &HashMap<&[u8], &proto::ComponentInfo>) -> bool {
    wanted.iter().any(|w| match w {
        // SolidWorks names nested components "sub-1/bracket-1".
        Wanted::Name(n) => name.eq_ignore_ascii_case(n) || name.to_lowercase().starts_with(&format!("{}/", n.to_lowercase())),
        Wanted::PersistId(want) => {
            let mut at = Some(pid);
            // Bounded: a cycle can't happen, but a bad reply shouldn't hang.
            for _ in 0..64 {
                let Some(p) = at else { break };
                if p == want.as_slice() {
                    return true;
                }
                at = parents.get(p).map(|c| c.parent_persist_id.as_slice()).filter(|p| !p.is_empty());
            }
            false
        }
    })
}

/// The interferences, biggest first, then the checked components.
pub fn render(ids: &mut IdRegistry, params: &CheckInterferenceParams, wanted: &[Wanted], reply: &proto::CheckInterferenceResponse) -> Value {
    // Ids only mean something in the active document: the assembly itself,
    // not a drawing of it.
    let use_ids = reply.view.is_empty();
    let by_pid: HashMap<&[u8], &proto::ComponentInfo> =
        reply.components.iter().map(|c| (c.persist_id.as_slice(), c)).collect();

    let mut found: Vec<&proto::Interference> = reply
        .interferences
        .iter()
        .filter(|i| {
            wanted.is_empty()
                || i.component_persist_ids
                    .iter()
                    .zip(&i.component_names)
                    .any(|(pid, name)| is_wanted(pid, name, wanted, &by_pid))
        })
        .collect();
    found.sort_by(|a, b| b.volume.total_cmp(&a.volume));

    let mut listed: Vec<&[u8]> = Vec::new();
    let mut items = Vec::new();
    for i in &found {
        let mut obj = Map::new();
        let mut components = Vec::new();
        for (k, name) in i.component_names.iter().enumerate() {
            let pid = i.component_persist_ids.get(k).map(Vec::as_slice).unwrap_or_default();
            if use_ids && let Some(id) = ids.id_for(Kind::Component, pid) {
                components.push(json!(format!("{id} ({name})")));
            } else {
                components.push(json!(name));
            }
            if !pid.is_empty() && !listed.contains(&pid) {
                listed.push(pid);
            }
        }
        obj.insert("components".into(), Value::Array(components));
        obj.insert("volume_mm3".into(), json!(mm3(i.volume)));
        if i.volume == 0.0 {
            obj.insert("touching".into(), json!(true));
        }
        if let Some(b) = &i.r#box
            && let (Some(lo), Some(hi)) = (&b.min, &b.max)
        {
            obj.insert("bbox_mm".into(), json!({ "min": point_mm(lo), "max": point_mm(hi) }));
            obj.insert(
                "size_mm".into(),
                json!([mm(hi.x - lo.x), mm(hi.y - lo.y), mm(hi.z - lo.z)]),
            );
        }
        if i.fastener {
            obj.insert("fastener".into(), json!("a fastener in its hole (usually intended)"));
        }
        if i.possible {
            obj.insert("possible".into(), json!("SolidWorks isn't sure it's real"));
        }
        if i.ignored {
            obj.insert("ignored".into(), json!("marked ignored in the assembly"));
        }
        items.push(Value::Object(obj));
    }

    let mut out = Map::new();
    out.insert("assembly".into(), json!(reply.document.as_ref().map(|d| d.title.as_str()).unwrap_or_default()));
    if !reply.configuration.is_empty() {
        out.insert("configuration".into(), json!(reply.configuration));
    }
    if !reply.view.is_empty() {
        out.insert("view".into(), json!(reply.view));
        if !reply.view_configuration.is_empty() && !reply.view_configuration.eq_ignore_ascii_case(&reply.configuration) {
            out.insert(
                "warning".into(),
                json!(format!(
                    "{} shows configuration \"{}\", but SolidWorks checks the assembly's active one (\"{}\"); \
                     open the assembly and activate \"{}\" to check what the view shows",
                    reply.view, reply.view_configuration, reply.configuration, reply.view_configuration
                )),
            );
        }
    }
    out.insert("checked_components".into(), json!(reply.checked_components));
    let real = found.iter().filter(|i| !i.ignored && i.volume > 0.0).count();
    let total: f64 = found.iter().filter(|i| !i.ignored).map(|i| i.volume).sum();
    out.insert("summary".into(), json!(summary(real, found.len(), total, !wanted.is_empty(), params)));
    out.insert("interferences".into(), Value::Array(items));
    if use_ids {
        let components: Vec<Value> = listed
            .iter()
            .filter_map(|pid| by_pid.get(pid))
            .map(|c| component_json(ids, c))
            .collect();
        if !components.is_empty() {
            out.insert("components".into(), Value::Array(components));
        }
    }
    if reply.marked_modified {
        out.insert(
            "modified".into(),
            json!(format!(
                "SolidWorks now shows {} as modified: ending its interference check does that, \
                 though nothing changed (saving is optional)",
                reply.document.as_ref().map(|d| d.title.as_str()).unwrap_or("the assembly")
            )),
        );
    }
    if reply.truncated {
        out.insert("truncated".into(), json!("only the first interferences SolidWorks found are listed"));
    }
    out.insert(
        "note".into(),
        json!(
            "Checked with every component where it is now (the saved / solved pose); parts that \
             move aren't swept through their motion. Boxes are in assembly coordinates."
        ),
    );
    Value::Object(out)
}

fn summary(real: usize, listed: usize, total: f64, narrowed: bool, params: &CheckInterferenceParams) -> String {
    let scope = if narrowed { " involving those components" } else { "" };
    if listed == 0 {
        return format!(
            "no interference{scope}{}",
            if params.include_touching { " (nothing touches either)" } else { "" }
        );
    }
    let mut text = if real == 0 {
        format!("no overlapping volume{scope}")
    } else {
        format!(
            "{real} interference{}{scope}, {} mm³ overlapping in all",
            if real == 1 { "" } else { "s" },
            mm3(total)
        )
    };
    if listed > real {
        text += &format!(" ({} more listed: touching or ignored)", listed - real);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f64, y: f64, z: f64) -> proto::Vec3 {
        proto::Vec3 { x, y, z }
    }

    fn component(pid: &[u8], name: &str, parent: &[u8]) -> proto::ComponentInfo {
        proto::ComponentInfo {
            persist_id: pid.to_vec(),
            name: name.into(),
            parent_persist_id: parent.to_vec(),
            ..Default::default()
        }
    }

    fn interference(between: &[(&[u8], &str)], volume: f64) -> proto::Interference {
        proto::Interference {
            component_persist_ids: between.iter().map(|(p, _)| p.to_vec()).collect(),
            component_names: between.iter().map(|(_, n)| n.to_string()).collect(),
            volume,
            r#box: Some(proto::Box3 { min: Some(v(0.0, 0.0, 0.0)), max: Some(v(0.01, 0.002, 0.005)) }),
            ..Default::default()
        }
    }

    fn reply(view: &str) -> proto::CheckInterferenceResponse {
        proto::CheckInterferenceResponse {
            document: Some(proto::Document { title: "top.SLDASM".into(), ..Default::default() }),
            view: view.into(),
            configuration: "Default".into(),
            interferences: vec![
                interference(&[(b"a", "block-1"), (b"b", "block-2")], 1e-7),
                interference(&[(b"b", "block-2"), (b"p", "sub-1/pin-1")], 1e-6),
                interference(&[(b"a", "block-1"), (b"c", "block-3")], 0.0),
            ],
            components: vec![
                component(b"a", "block-1", b""),
                component(b"b", "block-2", b""),
                component(b"c", "block-3", b""),
                component(b"p", "sub-1/pin-1", b"s"),
                component(b"s", "sub-1", b""),
            ],
            checked_components: 4,
            ..Default::default()
        }
    }

    #[test]
    fn biggest_first_in_mm() {
        let mut ids = IdRegistry::default();
        let out = render(&mut ids, &CheckInterferenceParams::default(), &[], &reply(""));
        let list = out["interferences"].as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["volume_mm3"], json!(1000.0));
        assert_eq!(list[1]["volume_mm3"], json!(100.0));
        assert_eq!(list[2]["touching"], json!(true));
        assert_eq!(list[0]["size_mm"], json!([10.0, 2.0, 5.0]));
        assert!(list[0]["components"][0].as_str().unwrap().starts_with("component:"), "{out}");
        assert_eq!(out["summary"], json!("2 interferences, 1100 mm³ overlapping in all (1 more listed: touching or ignored)"));
        assert_eq!(out["components"].as_array().unwrap().len(), 4);
        assert!(out.get("modified").is_none());
        let mut r = reply("");
        r.marked_modified = true;
        let out = render(&mut ids, &CheckInterferenceParams::default(), &[], &r);
        assert!(out["modified"].as_str().unwrap().starts_with("SolidWorks now shows top.SLDASM"), "{out}");
    }

    #[test]
    fn narrowed_by_subassembly_and_by_name() {
        let mut ids = IdRegistry::default();
        let p = CheckInterferenceParams::default();
        let out = render(&mut ids, &p, &[Wanted::PersistId(b"s".to_vec())], &reply(""));
        let list = out["interferences"].as_array().unwrap();
        assert_eq!(list.len(), 1, "{out}");
        assert_eq!(list[0]["volume_mm3"], json!(1000.0));
        let out = render(&mut ids, &p, &[Wanted::Name("SUB-1".into())], &reply(""));
        assert_eq!(out["interferences"].as_array().unwrap().len(), 1, "{out}");
        let out = render(&mut ids, &p, &[Wanted::Name("block-3".into())], &reply(""));
        assert_eq!(out["summary"], json!("no overlapping volume involving those components (1 more listed: touching or ignored)"));
    }

    #[test]
    fn drawings_go_by_name() {
        let mut ids = IdRegistry::default();
        let mut r = reply("Drawing View1");
        r.view_configuration = "Open".into();
        let out = render(&mut ids, &CheckInterferenceParams::default(), &[], &r);
        assert_eq!(out["interferences"][0]["components"], json!(["block-2", "sub-1/pin-1"]));
        assert!(out.get("components").is_none());
        assert!(out["warning"].as_str().unwrap().contains("\"Open\""), "{out}");
    }

    #[test]
    fn nothing_found() {
        let mut ids = IdRegistry::default();
        let mut r = reply("");
        r.interferences.clear();
        let out = render(&mut ids, &CheckInterferenceParams::default(), &[], &r);
        assert_eq!(out["summary"], json!("no interference"));
    }

    #[test]
    fn wanted_ids_and_names() {
        let resolve = |id: &str| Ok(id.as_bytes().to_vec());
        let mut p = CheckInterferenceParams {
            components: Some(vec!["component:2".into(), "bracket-1".into(), "component:2".into()]),
            ..Default::default()
        };
        assert_eq!(
            p.wanted(resolve).unwrap(),
            vec![Wanted::PersistId(b"component:2".to_vec()), Wanted::Name("bracket-1".into())]
        );
        p.components = Some(vec!["face:3".into()]);
        assert!(p.wanted(resolve).is_err());
        p.components = Some(vec!["component:2".into()]);
        p.view = Some("Drawing View1".into());
        assert!(p.wanted(resolve).is_err());
    }
}
