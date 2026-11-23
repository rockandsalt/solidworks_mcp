//! inspect_model's assembly tree (`what="tree"`) and its search: matching
//! components and features by name, file, configuration or type, and
//! components by custom property values. All of it over the InspectModel
//! reply; the add-in only lists (InspectModelRequest component_details,
//! component_properties, all_component_features).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::addin::proto;
use crate::assembly;
use crate::inspect::IdRegistry;

/// A search pattern: `*` any text, `?` one character, case-insensitive. One
/// without wildcards matches anywhere in the text ("bolt" finds "M6 bolt").
#[derive(Debug, Clone)]
pub struct Pattern {
    pattern: Vec<char>,
    anchored: bool,
}

impl Pattern {
    pub fn new(text: &str) -> Pattern {
        let text = text.trim().to_lowercase();
        Pattern {
            anchored: text.contains(['*', '?']),
            pattern: text.chars().collect(),
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        let text: Vec<char> = text.to_lowercase().chars().collect();
        if !self.anchored {
            return text
                .windows(self.pattern.len().max(1))
                .any(|w| w == self.pattern.as_slice())
                || self.pattern.is_empty();
        }
        glob(&self.pattern, &text)
    }
}

/// `*` / `?` matching over the whole text.
fn glob(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// "C:\models\bolt.SLDPRT" -> "bolt.SLDPRT".
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// What a component search looks at: its name (with and without the
/// subassembly path), file name and configuration.
pub fn component_matches(c: &proto::ComponentInfo, search: &Pattern) -> bool {
    let short = c.name.rsplit('/').next().unwrap_or(&c.name);
    [c.name.as_str(), short, file_name(&c.file), &c.configuration]
        .iter()
        .any(|text| !text.is_empty() && search.matches(text))
}

pub fn feature_matches(f: &proto::FeatureInfo, search: &Pattern) -> bool {
    search.matches(&f.name) || search.matches(&f.type_name)
}

/// Every `(property, pattern)` matches one of the component's properties
/// (name compared case-insensitively, the evaluated value by pattern).
pub fn properties_match(c: &proto::ComponentInfo, wanted: &[(String, Pattern)]) -> bool {
    wanted.iter().all(|(name, pattern)| {
        c.properties
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name) && pattern.matches(&p.resolved))
    })
}

/// The component filter of one inspect_model call.
#[derive(Debug, Default)]
pub struct ComponentFilter {
    pub search: Option<Pattern>,
    pub properties: Vec<(String, Pattern)>,
}

impl ComponentFilter {
    pub fn is_empty(&self) -> bool {
        self.search.is_none() && self.properties.is_empty()
    }

    pub fn keeps(&self, c: &proto::ComponentInfo) -> bool {
        self.search.as_ref().is_none_or(|s| component_matches(c, s))
            && properties_match(c, &self.properties)
    }
}

/// Keeps the components the filter matches.
pub fn filter_components(reply: &mut proto::InspectModelResponse, filter: &ComponentFilter) {
    reply.components.retain(|c| filter.keeps(c));
}

/// Keeps the features the search matches (`offset` / `limit` of them; the
/// note says when that isn't all), and of the components only those they're
/// in.
pub fn filter_features(
    reply: &mut proto::InspectModelResponse,
    search: &Pattern,
    offset: usize,
    limit: usize,
) -> Option<String> {
    reply.features.retain(|f| feature_matches(f, search));
    let note = page(&mut reply.features, offset, limit, "features");
    let used: HashSet<&[u8]> = reply
        .features
        .iter()
        .map(|f| f.component_persist_id.as_slice())
        .collect();
    reply
        .components
        .retain(|c| used.contains(c.persist_id.as_slice()));
    note
}

/// Keeps `items[offset .. offset + limit]`; a note when that isn't all.
pub fn page<T>(items: &mut Vec<T>, offset: usize, limit: usize, what: &str) -> Option<String> {
    let total = items.len();
    let start = offset.min(total);
    items.drain(..start);
    items.truncate(limit);
    let end = start + items.len();
    (start > 0 || end < total).then(|| {
        if end < total {
            format!(
                "showing {what} {}-{end} of {total} matching; pass offset={end} for more",
                start + 1
            )
        } else {
            format!("showing {what} {}-{end} of {total} matching", start + 1)
        }
    })
}

// ---- The tree ------------------------------------------------------------------

/// The tree of an InspectModel reply with every component (and, for
/// `component_details`, their flags). With `filter`, only the components it
/// matches and the subassemblies they're in. `depth` 1 = top level only.
pub fn render_tree(
    ids: &mut IdRegistry,
    reply: &proto::InspectModelResponse,
    filter: &ComponentFilter,
    depth: Option<usize>,
) -> Value {
    let mut out = crate::inspect::render_document(reply);
    let by_pid: HashMap<&[u8], &proto::ComponentInfo> = reply
        .components
        .iter()
        .map(|c| (c.persist_id.as_slice(), c))
        .collect();
    let mut children: HashMap<&[u8], Vec<&proto::ComponentInfo>> = HashMap::new();
    for c in &reply.components {
        // A parent that isn't listed (shouldn't happen) puts it at the top.
        let parent = if by_pid.contains_key(c.parent_persist_id.as_slice()) {
            c.parent_persist_id.as_slice()
        } else {
            &[]
        };
        children.entry(parent).or_default().push(c);
    }
    // SolidWorks' order isn't stable between runs; names are.
    for list in children.values_mut() {
        list.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    }

    // With a filter: the matches and every subassembly above one.
    let shown: Option<HashSet<&[u8]>> = (!filter.is_empty()).then(|| {
        let mut keep = HashSet::new();
        for c in reply.components.iter().filter(|c| filter.keeps(c)) {
            let mut at = Some(c);
            while let Some(component) = at {
                if !keep.insert(component.persist_id.as_slice()) {
                    break;
                }
                at = by_pid.get(component.parent_persist_id.as_slice()).copied();
            }
        }
        keep
    });
    let matched = reply.components.iter().filter(|c| filter.keeps(c)).count();

    let tree = Tree {
        children: &children,
        shown: shown.as_ref(),
        depth: depth.unwrap_or(usize::MAX).max(1),
    };
    let nodes = tree.nodes(ids, &[], 1);
    out.insert("summary".into(), summary(reply));
    if !filter.is_empty() {
        out.insert("matched".into(), json!(matched));
    }
    out.insert("tree".into(), Value::Array(nodes));
    if reply.components.is_empty() {
        out.insert("note".into(), json!("the assembly has no components"));
    }
    Value::Object(out)
}

struct Tree<'a> {
    children: &'a HashMap<&'a [u8], Vec<&'a proto::ComponentInfo>>,
    shown: Option<&'a HashSet<&'a [u8]>>,
    depth: usize,
}

impl Tree<'_> {
    fn nodes(&self, ids: &mut IdRegistry, parent: &[u8], level: usize) -> Vec<Value> {
        let Some(list) = self.children.get(parent) else {
            return Vec::new();
        };
        let mut nodes: Vec<Value> = Vec::new();
        // Leaves alike in all but id and name share one node, by this key.
        let mut groups: HashMap<String, usize> = HashMap::new();
        for c in list.iter().filter(|c| {
            self.shown
                .is_none_or(|s| s.contains(c.persist_id.as_slice()))
        }) {
            let mut node = node_json(ids, c);
            let below = self
                .children
                .get(c.persist_id.as_slice())
                .map_or(0, Vec::len);
            if below > 0 {
                if level < self.depth {
                    let kids = self.nodes(ids, &c.persist_id, level + 1);
                    node.insert("children".into(), Value::Array(kids));
                } else {
                    node.insert(
                        "children_not_shown".into(),
                        json!(count_below(self, &c.persist_id)),
                    );
                }
                nodes.push(Value::Object(node));
                continue;
            }
            let id = node.remove("id").unwrap_or(Value::Null);
            let name = node.remove("name").unwrap_or(Value::Null);
            let key = Value::Object(node.clone()).to_string();
            if let Some(&at) = groups.get(&key)
                && let Some(group) = nodes[at].as_object_mut()
            {
                into_group(group);
                push(group, "ids", id);
                push(group, "names", name);
                let count = group.get("count").and_then(Value::as_u64).unwrap_or(1);
                group.insert("count".into(), json!(count + 1));
                continue;
            }
            groups.insert(key, nodes.len());
            let mut first = Map::new();
            first.insert("id".into(), id);
            first.insert("name".into(), name);
            first.extend(node);
            nodes.push(Value::Object(first));
        }
        nodes
    }
}

/// How many components are under `pid`, at any depth (and shown).
fn count_below(tree: &Tree, pid: &[u8]) -> usize {
    tree.children.get(pid).map_or(0, |list| {
        list.iter()
            .filter(|c| {
                tree.shown
                    .is_none_or(|s| s.contains(c.persist_id.as_slice()))
            })
            .map(|c| 1 + count_below(tree, &c.persist_id))
            .sum()
    })
}

/// A single node {id, name, ...} turned into a group {count, ids, names, ...}.
fn into_group(node: &mut Map<String, Value>) {
    if node.contains_key("count") {
        return;
    }
    let id = node.remove("id").unwrap_or(Value::Null);
    let name = node.remove("name").unwrap_or(Value::Null);
    let rest = std::mem::take(node);
    node.insert("count".into(), json!(1));
    node.insert("ids".into(), json!([id]));
    node.insert("names".into(), json!([name]));
    node.extend(rest);
}

fn push(node: &mut Map<String, Value>, key: &str, value: Value) {
    if let Some(Value::Array(list)) = node.get_mut(key) {
        list.push(value);
    }
}

/// A component as the tree shows it: what it is, not where it sits
/// (inspect_model what="components" has placements and boxes).
fn node_json(ids: &mut IdRegistry, c: &proto::ComponentInfo) -> Map<String, Value> {
    let Value::Object(mut obj) = assembly::component_json(ids, c) else {
        return Map::new();
    };
    for key in ["parent", "origin_mm", "axes", "bbox_mm", "subassembly"] {
        obj.remove(key);
    }
    if obj.get("fixed") == Some(&json!(false)) {
        obj.remove("fixed");
    }
    obj
}

/// Counts per file and configuration (a BOM of every level, suppressed ones
/// left out), and what needs attention.
fn summary(reply: &proto::InspectModelResponse) -> Value {
    let mut counts: BTreeMap<(String, String), (usize, bool)> = BTreeMap::new();
    let mut suppressed = 0;
    let mut deepest = 0;
    for c in &reply.components {
        deepest = deepest.max(c.name.matches('/').count() + 1);
        if c.state() == proto::ComponentState::Suppressed {
            suppressed += 1;
            continue;
        }
        let key = (file_name(&c.file).to_string(), c.configuration.clone());
        let entry = counts.entry(key).or_insert((0, c.subassembly));
        entry.0 += 1;
    }
    let mut files: Vec<_> = counts.into_iter().collect();
    files.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    let files: Vec<Value> = files
        .into_iter()
        .map(|((file, configuration), (count, subassembly))| {
            let mut row = Map::new();
            row.insert("file".into(), json!(file));
            if !configuration.is_empty() {
                row.insert("configuration".into(), json!(configuration));
            }
            row.insert("count".into(), json!(count));
            if subassembly {
                row.insert("subassembly".into(), json!(true));
            }
            Value::Object(row)
        })
        .collect();

    let mut out = Map::new();
    out.insert("components".into(), json!(reply.components.len()));
    out.insert("levels".into(), json!(deepest));
    if suppressed > 0 {
        out.insert("suppressed".into(), json!(suppressed));
    }
    let lightweight = reply
        .components
        .iter()
        .filter(|c| c.state() == proto::ComponentState::Lightweight)
        .count();
    if lightweight > 0 {
        out.insert("lightweight".into(), json!(lightweight));
    }
    let in_context = reply
        .components
        .iter()
        .filter(|c| !c.external_references.is_empty())
        .count();
    if in_context > 0 {
        out.insert("with_in_context_references".into(), json!(in_context));
    }
    let broken = reply
        .components
        .iter()
        .flat_map(|c| &c.external_references)
        .filter(|r| assembly::is_broken_reference(r))
        .count();
    if broken > 0 {
        out.insert("broken_in_context_references".into(), json!(broken));
    }
    out.insert("files".into(), Value::Array(files));
    Value::Object(out)
}

/// "bolt-2" before "bolt-10".
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for ch in s.chars() {
            let digit = ch.is_ascii_digit();
            match out.last_mut() {
                Some((d, text)) if *d == digit => text.push(ch),
                _ => out.push((digit, ch.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for ((da, ta), (db, tb)) in ca.iter().zip(&cb) {
        let ord = if *da && *db {
            let (na, nb) = (ta.trim_start_matches('0'), tb.trim_start_matches('0'));
            na.len().cmp(&nb.len()).then_with(|| na.cmp(nb))
        } else {
            ta.to_lowercase().cmp(&tb.to_lowercase())
        };
        if ord.is_ne() {
            return ord;
        }
    }
    ca.len().cmp(&cb.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(pid: u8, name: &str, file: &str, parent: Option<u8>) -> proto::ComponentInfo {
        proto::ComponentInfo {
            persist_id: vec![pid],
            name: name.into(),
            file: format!("C:\\models\\{file}"),
            configuration: "Default".into(),
            parent_persist_id: parent.map(|p| vec![p]).unwrap_or_default(),
            state: proto::ComponentState::Resolved.into(),
            ..Default::default()
        }
    }

    fn assembly(components: Vec<proto::ComponentInfo>) -> proto::InspectModelResponse {
        proto::InspectModelResponse {
            document: Some(proto::Document {
                title: "top.SLDASM".into(),
                r#type: proto::DocumentType::Assembly.into(),
                ..Default::default()
            }),
            components,
            ..Default::default()
        }
    }

    /// top: base-1, sub-1 (bolt-1, bolt-2, bolt-10, washer-1), bolt-3
    fn sample() -> proto::InspectModelResponse {
        let mut sub = component(2, "sub-1", "sub.SLDASM", None);
        sub.subassembly = true;
        let mut base = component(1, "base-1", "base.SLDPRT", None);
        base.fixed = true;
        base.properties = vec![proto::CustomProperty {
            name: "Vendor".into(),
            value: "Acme".into(),
            resolved: "Acme".into(),
            ..Default::default()
        }];
        assembly(vec![
            base,
            sub,
            component(3, "sub-1/bolt-1", "bolt.SLDPRT", Some(2)),
            component(4, "sub-1/bolt-10", "bolt.SLDPRT", Some(2)),
            component(5, "sub-1/bolt-2", "bolt.SLDPRT", Some(2)),
            component(6, "sub-1/washer-1", "washer.SLDPRT", Some(2)),
            component(7, "bolt-3", "bolt.SLDPRT", None),
        ])
    }

    #[test]
    fn patterns() {
        assert!(Pattern::new("bolt").matches("M6 BOLT-1"));
        assert!(!Pattern::new("bolt").matches("washer"));
        assert!(Pattern::new("bolt*").matches("Bolt-1"));
        assert!(!Pattern::new("bolt*").matches("m6 bolt"));
        assert!(Pattern::new("*hole*").matches("CBORE hole1"));
        assert!(Pattern::new("hole?").matches("Hole1"));
        assert!(!Pattern::new("hole?").matches("Hole12"));
        assert!(Pattern::new("*").matches(""));
    }

    #[test]
    fn component_search_looks_at_name_file_and_configuration() {
        let reply = sample();
        let bolt = &reply.components[2];
        assert!(component_matches(bolt, &Pattern::new("bolt-1")));
        assert!(component_matches(bolt, &Pattern::new("bolt.sldprt")));
        assert!(component_matches(bolt, &Pattern::new("default")));
        // The short name too, not only "sub-1/bolt-1".
        assert!(component_matches(bolt, &Pattern::new("bolt-*")));
        assert!(!component_matches(bolt, &Pattern::new("washer")));
    }

    #[test]
    fn property_filter_compares_evaluated_values() {
        let reply = sample();
        let wanted = vec![("vendor".to_string(), Pattern::new("acme"))];
        assert!(properties_match(&reply.components[0], &wanted));
        assert!(!properties_match(&reply.components[2], &wanted));
        let other = vec![("Vendor".to_string(), Pattern::new("globex"))];
        assert!(!properties_match(&reply.components[0], &other));
    }

    #[test]
    fn tree_nests_sorts_and_groups_alike_leaves() {
        let mut ids = IdRegistry::default();
        let tree = render_tree(&mut ids, &sample(), &ComponentFilter::default(), None);
        let top = tree["tree"].as_array().unwrap();
        let names: Vec<&str> = top
            .iter()
            .map(|n| n["name"].as_str().or(n["names"][0].as_str()).unwrap())
            .collect();
        assert_eq!(names, ["base-1", "bolt-3", "sub-1"]);
        assert_eq!(top[0]["fixed"], true);
        let sub = &top[2]["children"];
        // bolt-1, bolt-2, bolt-10 in one node, in natural order; then washer-1.
        assert_eq!(sub[0]["count"], 3);
        assert_eq!(
            sub[0]["names"],
            json!(["sub-1/bolt-1", "sub-1/bolt-2", "sub-1/bolt-10"])
        );
        assert_eq!(sub[0]["file"], "bolt.SLDPRT");
        assert_eq!(sub[1]["name"], "sub-1/washer-1");
        assert!(sub[1].get("count").is_none());

        let summary = &tree["summary"];
        assert_eq!(summary["components"], 7);
        assert_eq!(summary["levels"], 2);
        assert_eq!(
            summary["files"][0],
            json!({"file": "bolt.SLDPRT", "configuration": "Default", "count": 4})
        );
    }

    #[test]
    fn tree_depth_counts_what_it_leaves_out() {
        let mut ids = IdRegistry::default();
        let tree = render_tree(&mut ids, &sample(), &ComponentFilter::default(), Some(1));
        let sub = &tree["tree"][2];
        assert_eq!(sub["name"], "sub-1");
        assert!(sub.get("children").is_none());
        assert_eq!(sub["children_not_shown"], 4);
    }

    #[test]
    fn filtered_tree_keeps_matches_and_their_subassemblies() {
        let mut ids = IdRegistry::default();
        let filter = ComponentFilter {
            search: Some(Pattern::new("washer")),
            ..Default::default()
        };
        let tree = render_tree(&mut ids, &sample(), &filter, None);
        assert_eq!(tree["matched"], 1);
        let top = tree["tree"].as_array().unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0]["name"], "sub-1");
        assert_eq!(top[0]["children"].as_array().unwrap().len(), 1);
        assert_eq!(top[0]["children"][0]["name"], "sub-1/washer-1");
    }

    #[test]
    fn feature_search_keeps_only_their_components() {
        let mut reply = sample();
        reply.features = vec![
            proto::FeatureInfo {
                name: "CBORE for M6".into(),
                type_name: "HoleWzd".into(),
                component_persist_id: vec![1],
                ..Default::default()
            },
            proto::FeatureInfo {
                name: "Boss-Extrude1".into(),
                type_name: "Extrusion".into(),
                component_persist_id: vec![3],
                ..Default::default()
            },
        ];
        assert_eq!(
            filter_features(&mut reply, &Pattern::new("holewzd"), 0, 100),
            None
        );
        assert_eq!(reply.features.len(), 1);
        assert_eq!(reply.components.len(), 1);
        assert_eq!(reply.components[0].name, "base-1");
    }

    #[test]
    fn paging() {
        let mut items: Vec<u32> = (1..=5).collect();
        let note = page(&mut items, 1, 2, "components");
        assert_eq!(items, [2, 3]);
        assert_eq!(
            note.as_deref(),
            Some("showing components 2-3 of 5 matching; pass offset=3 for more")
        );
        let mut all: Vec<u32> = (1..=3).collect();
        assert_eq!(page(&mut all, 0, 10, "components"), None);
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["bolt-10", "bolt-2", "Bolt-1", "axle-1"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["axle-1", "Bolt-1", "bolt-2", "bolt-10"]);
    }
}
