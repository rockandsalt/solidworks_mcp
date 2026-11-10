//! file_references: the files a part, assembly or drawing uses, every level,
//! where SolidWorks finds each and which are missing or found elsewhere than
//! saved; and where-used, the files of a folder that use it.
//!
//! The add-in only reads the files a document uses directly
//! (FileReferences); the walk down the levels, the existence checks (the
//! server runs on SolidWorks' machine), the folder search and the matching
//! are done here.

use std::collections::HashMap;
use std::path::Path;

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::addin::{AddinClient, proto};
use crate::files;

/// Files read per FileReferences call (about 30 ms each, seen 2026-11-09).
const BATCH: usize = 100;
/// Most files found as used; more stops the walk.
const MAX_FILES: usize = 5000;
/// Most files read in a where-used folder.
const MAX_FOLDER_FILES: usize = 2000;
/// Files listed in the reply unless `limit` says otherwise; the summary counts all.
const DEFAULT_LIMIT: usize = 100;
/// Users listed per file, and file names per kind of problem in the summary.
const NAMES_SHOWN: usize = 5;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct FileReferencesParams {
    /// An open document's title or file name, or the absolute path of a .SLDPRT / .SLDASM /
    /// .SLDDRW file, open or not (it isn't opened). Default: the active document.
    #[serde(default)]
    pub document: Option<String>,
    /// false: only the files it uses directly. Default true: every level, e.g. the parts of
    /// its subassemblies.
    #[serde(default)]
    pub all_levels: Option<bool>,
    /// Where-used: an absolute folder; also lists the parts, assemblies and drawings in it
    /// and its subfolders that use the document directly.
    #[serde(default)]
    pub where_used_in: Option<String>,
    /// How many files to list. Defaults to 100; when there are more, the missing, found
    /// elsewhere, unsaved and read-only ones come first. The summary counts them all.
    #[serde(default)]
    pub limit: Option<usize>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// With backslashes, as SolidWorks writes paths and the add-in compares them.
fn windows_path(path: &str) -> String {
    path.trim().replace('/', "\\")
}

/// The same file whichever way the path is written.
fn key(path: &str) -> String {
    windows_path(path).to_lowercase()
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

fn type_of(path: &str) -> &'static str {
    files::native_type(path).unwrap_or("file")
}

fn is_solidworks_file(path: &str) -> bool {
    files::native_type(path).is_some()
}

/// The first `NAMES_SHOWN` of `names`, then how many more.
fn some_names(names: &[&str]) -> String {
    let mut text = names.iter().take(NAMES_SHOWN).copied().collect::<Vec<_>>().join(", ");
    if names.len() > NAMES_SHOWN {
        text += &format!(" and {} more", names.len() - NAMES_SHOWN);
    }
    text
}

fn by_files(files: Vec<String>) -> proto::FileReferencesRequest {
    proto::FileReferencesRequest { document: String::new(), files }
}

/// The whole tool: the walk down from the document, then where-used if asked.
pub async fn run(
    addin: &AddinClient,
    params: &FileReferencesParams,
    addin_error: impl Fn(tonic::Status) -> McpError,
) -> Result<Value, McpError> {
    let request = first_request(params)?;
    let reply = addin.file_references(request.clone()).await.map_err(&addin_error)?;
    let mut walk = Walk::new(params, &request, &reply);
    let folder = params.where_used_in.as_deref().map(windows_path);
    if folder.is_some() && walk.root_path().is_empty() {
        return Err(invalid("the document was never saved, so no file can use it -- save_document first"));
    }
    loop {
        let batch = walk.next_batch();
        if batch.is_empty() {
            break;
        }
        let reply = addin.file_references(by_files(batch.clone())).await.map_err(&addin_error)?;
        walk.add_batch(&batch, &reply);
    }
    let where_used = match folder {
        None => None,
        Some(folder) => {
            let (candidates, truncated) = folder_files(&folder, walk.root_path());
            let mut uses = Vec::with_capacity(candidates.len());
            for chunk in candidates.chunks(BATCH) {
                uses.extend(addin.file_references(by_files(chunk.to_vec())).await.map_err(&addin_error)?.uses);
            }
            Some(where_used(&folder, walk.root_path(), &candidates, &uses, truncated, walk.limit))
        }
    };
    Ok(walk.render(where_used))
}

/// The first request: by path (`files`) when `document` is one, else by name.
pub fn first_request(params: &FileReferencesParams) -> Result<proto::FileReferencesRequest, McpError> {
    if let Some(folder) = params.where_used_in.as_deref().map(str::trim) {
        if !files::is_absolute(folder) {
            return Err(invalid(format!("where_used_in {folder:?} isn't an absolute folder")));
        }
        if !Path::new(folder).is_dir() {
            return Err(invalid(format!("there's no folder {folder}")));
        }
    }
    let document = params.document.as_deref().map(str::trim).unwrap_or_default();
    if !files::is_absolute(document) {
        return Ok(proto::FileReferencesRequest { document: document.to_string(), files: vec![] });
    }
    let document = windows_path(document);
    if !is_solidworks_file(&document) {
        return Err(invalid(format!("{document:?} isn't a part, assembly or drawing (.SLDPRT / .SLDASM / .SLDDRW)")));
    }
    if !Path::new(&document).is_file() {
        return Err(invalid(format!("there's no file {document}")));
    }
    Ok(by_files(vec![document]))
}

/// Worth listing first when not every file is listed.
fn needs_attention(u: &Used) -> bool {
    u.missing || u.saved_as.is_some() || u.unsaved_changes || u.read_only
}

struct Used {
    path: String,
    saved_as: Option<String>,
    level: usize,
    used_by: Vec<String>,
    missing: bool,
    read_only: bool,
    loaded: bool,
    unsaved_changes: bool,
}

/// The walk down from the document: files found so far, those still to read.
pub struct Walk {
    /// Its file; empty if never saved.
    root_path: String,
    root_name: String,
    root_loaded: bool,
    root_unsaved: bool,
    all_levels: bool,
    /// Files listed in the reply.
    limit: usize,
    used: Vec<Used>,
    index: HashMap<String, usize>,
    pending: Vec<usize>,
    truncated: bool,
}

impl Walk {
    /// From the first reply (`request` as sent).
    pub fn new(
        params: &FileReferencesParams,
        request: &proto::FileReferencesRequest,
        reply: &proto::FileReferencesResponse,
    ) -> Self {
        let (root_path, root_name) = match (&reply.document, request.files.first()) {
            (Some(doc), _) => (doc.path.clone(), doc.title.clone()),
            (None, Some(path)) => (path.clone(), file_name(path).to_string()),
            (None, None) => (String::new(), String::new()),
        };
        let uses = reply.uses.first().cloned().unwrap_or_default();
        let mut walk = Walk {
            root_name: if root_path.is_empty() { root_name } else { file_name(&root_path).to_string() },
            root_path,
            root_loaded: uses.loaded,
            root_unsaved: uses.unsaved_changes,
            all_levels: params.all_levels != Some(false),
            limit: params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_FILES),
            used: Vec::new(),
            index: HashMap::new(),
            pending: Vec::new(),
            truncated: false,
        };
        let by = walk.root_by();
        walk.add(&by, 1, &uses);
        walk
    }

    pub fn root_path(&self) -> &str {
        &self.root_path
    }

    /// How the root shows in `used_by`.
    fn root_by(&self) -> String {
        if self.root_path.is_empty() { self.root_name.clone() } else { self.root_path.clone() }
    }

    fn add(&mut self, by: &str, level: usize, uses: &proto::FileUses) {
        for r in &uses.references {
            let k = key(&r.path);
            if !self.root_path.is_empty() && k == key(&self.root_path) {
                continue; // a part in the context of the assembly it's in
            }
            if let Some(&i) = self.index.get(&k) {
                if !self.used[i].used_by.iter().any(|b| b == by) {
                    self.used[i].used_by.push(by.to_string());
                }
                continue;
            }
            if self.used.len() >= MAX_FILES {
                self.truncated = true;
                return;
            }
            let missing = !Path::new(&r.path).is_file();
            self.index.insert(k.clone(), self.used.len());
            if self.all_levels && !missing {
                self.pending.push(self.used.len());
            }
            self.used.push(Used {
                path: r.path.clone(),
                saved_as: (!r.saved_path.is_empty() && key(&r.saved_path) != k).then(|| r.saved_path.clone()),
                level,
                used_by: vec![by.to_string()],
                missing,
                read_only: !missing && std::fs::metadata(&r.path).is_ok_and(|m| m.permissions().readonly()),
                loaded: false,
                unsaved_changes: false,
            });
        }
    }

    /// The files to read next; empty when the walk is done.
    pub fn next_batch(&mut self) -> Vec<String> {
        let n = self.pending.len().min(BATCH);
        self.pending.drain(..n).map(|i| self.used[i].path.clone()).collect()
    }

    /// The reply for `batch` (from next_batch).
    pub fn add_batch(&mut self, batch: &[String], reply: &proto::FileReferencesResponse) {
        for (path, uses) in batch.iter().zip(&reply.uses) {
            let Some(&i) = self.index.get(&key(path)) else { continue };
            self.used[i].loaded = uses.loaded;
            self.used[i].unsaved_changes = uses.unsaved_changes;
            let level = self.used[i].level + 1;
            self.add(path, level, uses);
        }
    }

    pub fn render(&self, where_used: Option<Value>) -> Value {
        let mut shown: Vec<&Used> = self.used.iter().collect();
        if shown.len() > self.limit {
            shown.sort_by_key(|u| !needs_attention(u)); // stable: walk order otherwise
            shown.truncate(self.limit);
        }
        let files: Vec<Value> = shown
            .iter()
            .map(|u| {
                let used_by: Vec<&String> = u.used_by.iter().take(NAMES_SHOWN).collect();
                let mut f = json!({ "file": u.path, "type": type_of(&u.path), "level": u.level, "used_by": used_by });
                if u.used_by.len() > NAMES_SHOWN {
                    f["used_by_count"] = json!(u.used_by.len());
                }
                if u.missing {
                    f["missing"] = json!(true);
                }
                if let Some(saved) = &u.saved_as {
                    f["saved_as"] = json!(saved);
                }
                if u.read_only {
                    f["read_only"] = json!(true);
                }
                if u.loaded {
                    f["loaded"] = json!(true);
                }
                if u.unsaved_changes {
                    f["unsaved_changes"] = json!(true);
                }
                f
            })
            .collect();
        let mut out = json!({
            "document": self.root_name,
            "file": (!self.root_path.is_empty()).then_some(&self.root_path),
            "summary": self.summary(),
            "uses": files,
        });
        if shown.len() < self.used.len() {
            out["listed"] = json!(format!(
                "{} of {} files, those missing, found elsewhere, unsaved or read-only first; raise limit for more",
                shown.len(),
                self.used.len()
            ));
        }
        if self.root_unsaved {
            out["unsaved_changes"] = json!(true);
        }
        if self.root_loaded {
            out["loaded"] = json!(true);
        }
        if self.truncated {
            out["truncated"] = json!(format!("stopped at {MAX_FILES} files"));
        }
        if let Some(w) = where_used {
            out["where_used"] = w;
        }
        out
    }

    fn summary(&self) -> String {
        let root = if self.root_path.is_empty() {
            format!("{} (never saved)", self.root_name)
        } else {
            self.root_name.clone()
        };
        if self.used.is_empty() {
            return format!("{root} uses no other files");
        }
        let count = |t: &str| self.used.iter().filter(|u| type_of(&u.path) == t).count();
        let kinds: Vec<String> = [("assembly", "assemblies"), ("part", "parts"), ("drawing", "drawings"), ("file", "other files")]
            .iter()
            .filter_map(|(t, plural)| match count(t) {
                0 => None,
                1 => Some(format!("1 {t}")),
                n => Some(format!("{n} {plural}")),
            })
            .collect();
        let levels = if self.all_levels { "" } else { " directly" };
        let mut text = format!("{root} uses {} file(s){levels}: {}", self.used.len(), kinds.join(", "));
        let names = |pick: &dyn Fn(&Used) -> bool| -> Vec<&str> {
            self.used.iter().filter(|u| pick(u)).map(|u| file_name(&u.path)).collect()
        };
        let missing = names(&|u| u.missing);
        if !missing.is_empty() {
            text += &format!(
                "; {} missing (not found at its saved path or by SolidWorks' search): {}",
                missing.len(),
                some_names(&missing)
            );
        }
        let moved = names(&|u| !u.missing && u.saved_as.is_some());
        if !moved.is_empty() {
            text += &format!("; {} found elsewhere than saved (saved_as): {}", moved.len(), some_names(&moved));
        }
        let unsaved = names(&|u| u.unsaved_changes);
        if !unsaved.is_empty() || self.root_unsaved {
            let mut all: Vec<&str> = unsaved;
            if self.root_unsaved {
                all.insert(0, &self.root_name);
            }
            text += &format!("; unsaved changes in {}", some_names(&all));
        }
        text
    }
}

/// The parts, assemblies and drawings under `folder` other than `target`, and
/// whether there were more than are read.
pub fn folder_files(folder: &str, target: &str) -> (Vec<String>, bool) {
    let mut found = Vec::new();
    let mut dirs = vec![Path::new(folder).to_path_buf()];
    let target = key(target);
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let Some(text) = path.to_str() else { continue };
            // ~$ files are SolidWorks' lock files for open documents.
            if name.starts_with("~$") || !is_solidworks_file(text) || key(text) == target {
                continue;
            }
            if found.len() >= MAX_FOLDER_FILES {
                return (found, true);
            }
            found.push(text.to_string());
        }
    }
    (found, false)
}

/// Which of `files` (with `uses`, as the add-in read them) use `target`.
/// One that uses a file of the same name somewhere else is listed apart: it
/// would open that one.
pub fn where_used(
    folder: &str,
    target: &str,
    files: &[String],
    uses: &[proto::FileUses],
    truncated: bool,
    limit: usize,
) -> Value {
    let target_key = key(target);
    let stem = Path::new(target).file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_lowercase();
    let mut users = Vec::new();
    let mut others = Vec::new();
    for (file, u) in files.iter().zip(uses) {
        if let Some(r) = u.references.iter().find(|r| key(&r.path) == target_key || key(&r.saved_path) == target_key) {
            let mut entry = json!({ "file": file, "type": type_of(file) });
            if key(&r.path) != target_key {
                entry["finds_instead"] = json!(r.path);
            } else if key(&r.saved_path) != target_key && !r.saved_path.is_empty() {
                entry["saved_as"] = json!(r.saved_path);
            }
            users.push(entry);
        } else if let Some(r) = u.references.iter().find(|r| {
            r.name.to_lowercase() == stem && type_of(&r.path) == type_of(target)
        }) {
            others.push(json!({ "file": file, "type": type_of(file), "uses": r.path }));
        }
    }
    let found = (users.len(), others.len());
    users.truncate(limit);
    others.truncate(limit);
    let mut out = json!({ "folder": folder, "files_searched": files.len(), "used_by": users });
    if !others.is_empty() {
        out["same_name_elsewhere"] = json!(others);
    }
    if users.len() < found.0 || others.len() < found.1 {
        out["listed"] = json!(format!(
            "{} of {} users and {} of {} same-name; raise limit for more",
            users.len(),
            found.0,
            others.len(),
            found.1
        ));
    }
    if truncated {
        out["truncated"] = json!(format!("only the first {MAX_FOLDER_FILES} files were searched"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(name: &str, path: &str, saved: &str) -> proto::FileReference {
        proto::FileReference { name: name.into(), path: path.into(), saved_path: saved.into() }
    }

    fn uses(refs: Vec<proto::FileReference>) -> proto::FileUses {
        proto::FileUses { loaded: false, unsaved_changes: false, references: refs }
    }

    #[test]
    fn walks_every_level_once() {
        let dir = std::env::temp_dir().join(format!("sld_mcp_refs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |n: &str| dir.join(n).to_str().unwrap().to_string();
        for n in ["sub.SLDASM", "pin.SLDPRT"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let (top, sub, pin, block) = (path("top.SLDASM"), path("sub.SLDASM"), path("pin.SLDPRT"), path("block.SLDPRT"));
        let params = FileReferencesParams::default();
        let request = proto::FileReferencesRequest { document: String::new(), files: vec![top.clone()] };
        let reply = proto::FileReferencesResponse {
            document: None,
            uses: vec![uses(vec![reference("sub", &sub, &sub), reference("pin", &pin, &path("old\\pin.SLDPRT"))])],
        };
        let mut walk = Walk::new(&params, &request, &reply);
        let batch = walk.next_batch();
        assert_eq!(batch, vec![sub.clone(), pin.clone()]);
        let reply = proto::FileReferencesResponse {
            document: None,
            uses: vec![
                proto::FileUses { loaded: true, ..uses(vec![reference("block", &block, &block), reference("pin", &pin, &pin)]) },
                uses(vec![reference("top", &top, &top)]),
            ],
        };
        walk.add_batch(&batch, &reply);
        assert!(walk.next_batch().is_empty(), "block is missing, so not read");
        let json = walk.render(None);
        let used = json["uses"].as_array().unwrap();
        assert_eq!(used.len(), 3, "{json}");
        assert_eq!(used[0]["loaded"], true);
        assert_eq!(used[1]["saved_as"], path("old\\pin.SLDPRT"));
        assert_eq!(used[1]["used_by"].as_array().unwrap().len(), 2);
        assert_eq!(used[2]["missing"], true);
        assert_eq!(used[2]["level"], 2);
        let summary = json["summary"].as_str().unwrap();
        assert!(summary.contains("3 file(s)") && summary.contains("1 missing") && summary.contains("block.SLDPRT"), "{summary}");
        assert!(summary.contains("found elsewhere than saved (saved_as): pin.SLDPRT"), "{summary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_only_reads_nothing_more() {
        let params = FileReferencesParams { all_levels: Some(false), ..Default::default() };
        let request = proto::FileReferencesRequest { document: "Assem1".into(), files: vec![] };
        let reply = proto::FileReferencesResponse {
            document: Some(proto::Document { title: "Assem1".into(), ..Default::default() }),
            uses: vec![proto::FileUses {
                loaded: true,
                unsaved_changes: true,
                references: vec![reference("a", "C:\\x\\a.SLDPRT", "C:\\x\\a.SLDPRT")],
            }],
        };
        let mut walk = Walk::new(&params, &request, &reply);
        assert!(walk.next_batch().is_empty());
        let json = walk.render(None);
        assert!(json["file"].is_null(), "{json}");
        assert_eq!(json["loaded"], true);
        assert_eq!(json["uses"][0]["used_by"][0], "Assem1");
        let summary = json["summary"].as_str().unwrap();
        assert!(summary.starts_with("Assem1 (never saved) uses 1 file(s) directly"), "{summary}");
        assert!(summary.contains("unsaved changes in Assem1"), "{summary}");
    }

    #[test]
    fn limit_lists_problems_first_and_caps_users() {
        let dir = std::env::temp_dir().join(format!("sld_mcp_refs_limit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |n: &str| dir.join(n).to_str().unwrap().to_string();
        std::fs::write(dir.join("a.SLDPRT"), b"x").unwrap();
        let (a, gone) = (path("a.SLDPRT"), path("gone.SLDPRT"));
        let params = FileReferencesParams { all_levels: Some(false), limit: Some(1), ..Default::default() };
        let request = by_files(vec![path("top.SLDASM")]);
        let reply = proto::FileReferencesResponse {
            document: None,
            uses: vec![uses(vec![reference("a", &a, &a), reference("gone", &gone, &gone)])],
        };
        let mut walk = Walk::new(&params, &request, &reply);
        for n in 1..=6 {
            walk.add(&format!("C:\\users\\u{n}.SLDASM"), 2, &uses(vec![reference("a", &a, &a)]));
        }
        let json = walk.render(None);
        let used = json["uses"].as_array().unwrap();
        assert_eq!(used.len(), 1, "{json}");
        assert_eq!(used[0]["file"], gone.as_str(), "the missing one comes first: {json}");
        assert!(json["listed"].as_str().unwrap().starts_with("1 of 2 files"), "{json}");
        assert!(json.get("loaded").is_none(), "read from disk: {json}");
        let all = Walk::new(&FileReferencesParams { limit: None, ..params }, &request, &reply);
        assert!(all.render(None).get("listed").is_none());
        walk.limit = 5;
        let a_entry = &walk.render(None)["uses"][0];
        assert_eq!(a_entry["used_by"].as_array().unwrap().len(), NAMES_SHOWN, "{a_entry}");
        assert_eq!(a_entry["used_by_count"], 7, "{a_entry}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn some_names_says_how_many_more() {
        assert_eq!(some_names(&["a", "b"]), "a, b");
        assert_eq!(some_names(&["a", "b", "c", "d", "e", "f", "g"]), "a, b, c, d, e and 2 more");
    }

    #[test]
    fn a_path_with_forward_slashes_goes_with_backslashes() {
        let file = std::env::temp_dir().join(format!("sld_mcp_refs_slash_{}.SLDASM", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let windows = file.to_str().unwrap().to_string();
        let params = FileReferencesParams { document: Some(windows.replace('\\', "/")), ..Default::default() };
        let request = first_request(&params).unwrap();
        let _ = std::fs::remove_file(&file);
        assert_eq!(request.files, vec![windows]);
    }

    #[test]
    fn where_used_matches_by_path_then_name() {
        let target = "C:\\p\\pin.SLDPRT";
        let files = vec!["C:\\p\\a.SLDASM".to_string(), "C:\\p\\b.SLDDRW".to_string(), "C:\\p\\c.SLDASM".to_string()];
        let uses = vec![
            uses(vec![reference("pin", "c:/p/PIN.sldprt", "C:\\old\\pin.SLDPRT")]),
            uses(vec![reference("pin", "C:\\other\\pin.SLDPRT", "C:\\other\\pin.SLDPRT")]),
            uses(vec![reference("block", "C:\\p\\block.SLDPRT", "C:\\p\\block.SLDPRT")]),
        ];
        let json = where_used("C:\\p", target, &files, &uses, false, 100);
        assert_eq!(json["used_by"].as_array().unwrap().len(), 1, "{json}");
        assert_eq!(json["used_by"][0]["saved_as"], "C:\\old\\pin.SLDPRT");
        assert_eq!(json["same_name_elsewhere"][0]["file"], "C:\\p\\b.SLDDRW");
        assert!(json.get("listed").is_none(), "{json}");
        let json = where_used("C:\\p", target, &files, &uses, false, 0);
        assert_eq!(json["listed"], "0 of 1 users and 0 of 1 same-name; raise limit for more", "{json}");
    }

    #[test]
    fn checks_the_document_and_folder() {
        let p = |d: &str, w: Option<&str>| FileReferencesParams {
            document: Some(d.into()),
            where_used_in: w.map(Into::into),
            ..Default::default()
        };
        assert_eq!(first_request(&p("ref-top", None)).unwrap().document, "ref-top");
        assert!(first_request(&p("C:\\no\\such.SLDASM", None)).is_err());
        assert!(first_request(&p("C:\\x\\a.step", None)).is_err());
        assert!(first_request(&p("", Some("relative\\dir"))).is_err());
    }
}
