//! Saving and opening files: the save_document and open_document tools'
//! params, which file extension is which format (and which document types
//! it holds), and the text they reply with, naming SolidWorks' save / load
//! error and warning bits (swFileSaveError_e, swFileSaveWarning_e,
//! swFileLoadError_e, swFileLoadWarning_e) that the add-in passes through.
//!
//! The MCP server runs on the machine SolidWorks runs on (the add-in only
//! listens on loopback), so paths are checked here against the file system
//! before anything is sent.

use std::path::Path;

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use proto::DocumentType;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SaveDocumentParams {
    /// Where to save, as an absolute path (e.g. "C:\\Users\\me\\Documents\\bracket.SLDPRT").
    /// Leave out to save the document to its own file. The extension picks the format:
    /// .SLDPRT / .SLDASM / .SLDDRW save the document as that file (Save As; it becomes the
    /// active document's file unless copy=true), anything else exports a copy: .STEP/.STP,
    /// .IGS/.IGES, .X_T/.X_B (Parasolid), .SAT (ACIS), .STL, .3MF for parts and assemblies;
    /// .PDF, .DXF, .DWG for drawings.
    #[serde(default)]
    pub path: Option<String>,
    /// Save As copy: write the file but keep working on the document as it was (exports are
    /// always copies). Default false.
    #[serde(default)]
    pub copy: bool,
    /// Replace a file that's already there. Default false: an existing file is an error.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenDocumentParams {
    /// The file to open, as an absolute path to a .SLDPRT, .SLDASM or .SLDDRW file.
    pub path: String,
    /// Open it read-only. Default false. (Ignored when it's already open.)
    #[serde(default)]
    pub read_only: bool,
}

struct Format {
    extensions: &'static [&'static str],
    name: &'static str,
    /// Native formats are a Save As; the others an export.
    native: bool,
    types: &'static [DocumentType],
}

const MODELS: &[DocumentType] = &[DocumentType::Part, DocumentType::Assembly];
const DRAWINGS: &[DocumentType] = &[DocumentType::Drawing];

const FORMATS: &[Format] = &[
    Format { extensions: &["sldprt"], name: "part", native: true, types: &[DocumentType::Part] },
    Format { extensions: &["sldasm"], name: "assembly", native: true, types: &[DocumentType::Assembly] },
    Format { extensions: &["slddrw"], name: "drawing", native: true, types: DRAWINGS },
    Format { extensions: &["step", "stp"], name: "STEP", native: false, types: MODELS },
    Format { extensions: &["igs", "iges"], name: "IGES", native: false, types: MODELS },
    Format { extensions: &["x_t", "x_b"], name: "Parasolid", native: false, types: MODELS },
    Format { extensions: &["sat"], name: "ACIS", native: false, types: MODELS },
    Format { extensions: &["stl"], name: "STL", native: false, types: MODELS },
    Format { extensions: &["3mf"], name: "3MF", native: false, types: MODELS },
    Format { extensions: &["pdf"], name: "PDF", native: false, types: DRAWINGS },
    Format { extensions: &["dxf"], name: "DXF", native: false, types: DRAWINGS },
    Format { extensions: &["dwg"], name: "DWG", native: false, types: DRAWINGS },
];

fn invalid(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

fn format_of(path: &str) -> Option<&'static Format> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    FORMATS.iter().find(|f| f.extensions.contains(&extension.as_str()))
}

fn extension_list(native: bool) -> String {
    FORMATS
        .iter()
        .filter(|f| native || !f.native)
        .flat_map(|f| f.extensions.iter().map(|e| format!(".{}", e.to_ascii_uppercase())))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A Windows absolute path ("C:\dir\file", "C:/dir/file" or "\\server\share\file"),
/// checked by hand so the tests behave the same off Windows.
fn is_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || path.starts_with("\\\\")
}

fn checked_path(path: &str) -> Result<&str, McpError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(invalid("the path is empty".into()));
    }
    if !is_absolute(path) {
        return Err(invalid(format!(
            "{path:?} isn't an absolute path -- give the whole path, e.g. \
             \"C:\\Users\\me\\Documents\\{}\"",
            Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("part.SLDPRT")
        )));
    }
    Ok(path)
}

/// The SaveDocument request for `params`, after checking the path, its
/// format and its folder, and that it won't overwrite a file unasked.
pub fn save_request(params: &SaveDocumentParams) -> Result<proto::SaveDocumentRequest, McpError> {
    let Some(path) = params.path.as_deref() else {
        return Ok(proto::SaveDocumentRequest { path: String::new(), copy: false, allowed_types: vec![] });
    };
    let path = checked_path(path)?;
    let format = format_of(path).ok_or_else(|| {
        invalid(format!(
            "can't tell the format from {path:?} -- end it with one of {}",
            extension_list(true)
        ))
    })?;
    let file = Path::new(path);
    if let Some(dir) = file.parent()
        && !dir.as_os_str().is_empty()
        && !dir.is_dir()
    {
        return Err(invalid(format!("there's no folder {}", dir.display())));
    }
    if file.is_dir() {
        return Err(invalid(format!("{path} is a folder -- give a file name")));
    }
    if file.exists() && !params.overwrite {
        return Err(invalid(format!(
            "{path} already exists -- pass overwrite=true to replace it (to save the document to \
             its own file, leave path out)"
        )));
    }
    Ok(proto::SaveDocumentRequest {
        path: path.to_string(),
        copy: params.copy || !format.native,
        allowed_types: format.types.iter().map(|&t| t as i32).collect(),
    })
}

/// The OpenDocument request for `params`: the type comes from the extension.
pub fn open_request(params: &OpenDocumentParams) -> Result<proto::OpenDocumentRequest, McpError> {
    let path = checked_path(&params.path)?;
    let format = format_of(path).filter(|f| f.native).ok_or_else(|| {
        invalid(format!(
            "open_document opens SolidWorks files ({}); {path:?} isn't one",
            extension_list(true).split(", ").take(3).collect::<Vec<_>>().join(", ")
        ))
    })?;
    if !Path::new(path).is_file() {
        return Err(invalid(format!("there's no file {path}")));
    }
    Ok(proto::OpenDocumentRequest {
        path: path.to_string(),
        r#type: format.types[0] as i32,
        read_only: params.read_only,
    })
}

pub fn type_name(t: DocumentType) -> &'static str {
    match t {
        DocumentType::Part => "part",
        DocumentType::Assembly => "assembly",
        DocumentType::Drawing => "drawing",
        DocumentType::Unspecified => "document",
    }
}

/// "block.SLDPRT (part, C:\dir\block.SLDPRT)" or "Part1 (part, never saved)".
pub fn describe(doc: &proto::Document) -> String {
    let file = if doc.path.is_empty() { "never saved" } else { doc.path.as_str() };
    format!("{} ({}, {file})", doc.title, type_name(doc.r#type()))
}

/// Which document ids belong to (`IdRegistry::set_document`), and its name for
/// messages; an empty key when nothing is open. A saved document is its file
/// (persist ids are stable in a file, so its ids even survive SolidWorks
/// restarting); one never saved is its title ("Part1") in this SolidWorks
/// session, since titles start over at Part1 after a restart.
pub fn document_key(reply: &proto::GetActiveDocumentResponse) -> (String, String) {
    match &reply.document {
        None => (String::new(), String::new()),
        Some(doc) if !doc.path.is_empty() => (format!("file:{}", doc.path.to_lowercase()), doc.title.clone()),
        Some(doc) => (format!("new:{}:{}", reply.session, doc.title), doc.title.clone()),
    }
}

/// Names of the bits set in `bits`, from a (bit, name) table; unknown bits
/// by number.
fn bit_names(bits: i32, table: &[(i32, &str)]) -> Vec<String> {
    let mut names: Vec<String> = table
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| name.to_string())
        .collect();
    let known = table.iter().fold(0, |all, (bit, _)| all | bit);
    if bits & !known != 0 {
        names.push(format!("code {}", bits & !known));
    }
    names
}

const SAVE_ERRORS: &[(i32, &str)] = &[
    (1, "SolidWorks couldn't save it"),
    (2, "the document or file is read-only"),
    (4, "the file name is empty"),
    (8, "the file name contains '@'"),
    (16, "the file is locked (open in another program or by another user?)"),
    (32, "that format isn't available for this document"),
    (64, "the model has rebuild errors (diagnose_model shows them)"),
    (128, "SolidWorks wouldn't overwrite the file"),
    (256, "SolidWorks doesn't accept that file extension"),
    (512, "the export needs a selection"),
    (1024, "bad eDrawings version"),
    (2048, "the path is too long"),
    (4096, "SolidWorks can't save this document in that format"),
    (8192, "documents it references need saving first"),
    (16384, "detached drawings aren't supported"),
    (32768, "the file would need upgrading to this SolidWorks version"),
    (65536, "the document has items that format can't hold"),
];

const SAVE_WARNINGS: &[(i32, &str)] = &[
    (1, "the model has rebuild errors (diagnose_model shows them)"),
    (2, "the model needs a rebuild"),
    (4, "drawing views need updating"),
    (8, "animator needs to solve"),
    (16, "animator feature edits"),
    (32, "eDrawings: bad selection"),
    (64, "animator light edits"),
    (128, "animator camera views"),
    (256, "animator section views"),
    (512, "OLE objects are missing"),
    (1024, "the document is open view-only"),
    (2048, "invalid XML"),
];

const LOAD_ERRORS: &[(i32, &str)] = &[
    (1, "SolidWorks couldn't open it"),
    (2, "the file wasn't found"),
    (4, "a referenced file doesn't match (id mismatch)"),
    (8, "the file is read-only"),
    (16, "the file is in use by another program or user"),
    (32, "the drawing's standard needs updating"),
    (64, "sheet scale needs updating"),
    (128, "it needs a rebuild"),
    (256, "its base part wasn't loaded"),
    (512, "it's already open"),
    (1024, "it isn't a file of that type"),
    (2048, "the drawing is detached (RapidDraft)"),
    (4096, "view-only restrictions"),
    (8192, "it was saved by a newer SolidWorks version"),
    (16384, "a view's referenced configuration is missing"),
    (32768, "surface finish symbols need converting"),
    (65536, "another document with the same name is already open -- close it first"),
    (131072, "it's protected (Liquid Machines)"),
    (262144, "not enough memory or resources"),
    (524288, "it has no display data"),
    (1048576, "an add-in interrupted the open"),
    (2097152, "the file needs repairing"),
    (4194304, "the file's critical data needs repairing"),
    (8388608, "SolidWorks is busy"),
    (16777216, "SolidWorks is offline (3DEXPERIENCE)"),
];

const LOAD_WARNINGS: &[(i32, &str)] = &[
    (1, "a referenced file doesn't match (id mismatch)"),
    (2, "it's opened read-only (the file is read-only or in use elsewhere)"),
    (4, "the file is in use by another program or user"),
    (8, "the drawing's standard was updated"),
    (16, "sheet scale was updated"),
    (32, "it needs a rebuild"),
    (64, "its base part wasn't loaded"),
    (128, "it was already open"),
    (256, "the drawing is detached (RapidDraft)"),
    (512, "view-only restrictions"),
    (1024, "a view's referenced configuration is missing"),
    (2048, "surface finish symbols were converted"),
    (4096, "revolve dimension tolerances"),
    (8192, "the model is out of date"),
    (16384, "dimensions reference models incorrectly"),
    (32768, "a component's referenced configuration is missing"),
    (65536, "a linked design table failed to update"),
    (131072, "the design table is missing"),
    (262144, "SolidWorks repaired the file"),
    (524288, "SolidWorks repaired the file's critical data"),
    (1048576, "external references are missing (components or models it uses weren't found)"),
];

/// The reply to a save, or the error when nothing was saved.
pub fn save_reply_text(
    request: &proto::SaveDocumentRequest,
    reply: &proto::SaveDocumentResponse,
) -> Result<String, McpError> {
    let errors = bit_names(reply.errors, SAVE_ERRORS);
    if !reply.saved {
        let reason = if errors.is_empty() { "SolidWorks gave no reason".to_string() } else { errors.join("; ") };
        return Err(McpError::internal_error(
            format!("SolidWorks didn't save {}: {reason}", reply.saved_path),
            None,
        ));
    }
    let doc = reply.document.clone().unwrap_or_default();
    let format = format_of(&reply.saved_path);
    let mut text = if request.path.is_empty() {
        format!("saved {}", describe(&doc))
    } else if format.is_some_and(|f| !f.native) {
        format!("exported {} as {} to {}", doc.title, format.map_or("", |f| f.name), reply.saved_path)
    } else if request.copy {
        format!("saved a copy of {} to {}; still working on {}", doc.title, reply.saved_path, describe(&doc))
    } else {
        format!("saved as {}; that's now the active document", describe(&doc))
    };
    // The generic error says nothing once the file is there: SolidWorks
    // reports it for a drawing's DXF export that worked.
    let mut notes = bit_names(reply.errors & !1, SAVE_ERRORS);
    notes.extend(bit_names(reply.warnings, SAVE_WARNINGS));
    notes.dedup();
    if !notes.is_empty() {
        text += &format!("\nSolidWorks reported: {}", notes.join("; "));
    }
    Ok(text)
}

/// The reply to an open, or the error when SolidWorks couldn't open it.
pub fn open_reply_text(
    request: &proto::OpenDocumentRequest,
    reply: &proto::OpenDocumentResponse,
) -> Result<String, McpError> {
    let Some(doc) = reply.document.as_ref().filter(|_| reply.opened) else {
        let errors = bit_names(reply.errors, LOAD_ERRORS);
        let reason = if errors.is_empty() { "SolidWorks gave no reason".to_string() } else { errors.join("; ") };
        return Err(McpError::internal_error(
            format!("SolidWorks couldn't open {}: {reason}", request.path),
            None,
        ));
    };
    let mut text = if reply.already_open {
        format!("{} was already open; it's now the active document", describe(doc))
    } else {
        format!("opened {}; it's now the active document", describe(doc))
    };
    // Errors on a document that did open are about the files it uses: an
    // assembly whose part is gone opens with "file not found" and that
    // component suppressed. Already-open is said above.
    let mut notes = Vec::new();
    if reply.errors & 2 != 0 {
        notes.push(
            "files it uses weren't found -- an assembly's missing components are suppressed \
             (inspect_model lists them with state \"suppressed\" and the file they need)"
                .to_string(),
        );
    }
    notes.extend(bit_names(reply.errors & !(2 | 512), LOAD_ERRORS));
    notes.extend(bit_names(reply.warnings & !128, LOAD_WARNINGS));
    if !notes.is_empty() {
        text += &format!("\nSolidWorks reported: {}", notes.join("; "));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(path: Option<&str>) -> SaveDocumentParams {
        SaveDocumentParams { path: path.map(Into::into), copy: false, overwrite: false }
    }

    #[test]
    fn documents_are_keyed_by_file_else_session_and_title() {
        let reply = |title: &str, path: &str, session: &str| proto::GetActiveDocumentResponse {
            document: Some(proto::Document { title: title.into(), r#type: 1, path: path.into() }),
            session: session.into(),
        };
        let saved = document_key(&reply("block.SLDPRT", r"C:\Dir\Block.SLDPRT", "s1"));
        assert_eq!(saved, (r"file:c:\dir\block.sldprt".to_string(), "block.SLDPRT".to_string()));
        // Same file after SolidWorks restarted: same document.
        assert_eq!(document_key(&reply("block.SLDPRT", r"c:\dir\block.sldprt", "s2")).0, saved.0);
        // Part1 of another session is another document.
        let new1 = document_key(&reply("Part1", "", "s1")).0;
        assert_ne!(new1, document_key(&reply("Part1", "", "s2")).0);
        assert_ne!(new1, document_key(&reply("Part2", "", "s1")).0);
        let none = proto::GetActiveDocumentResponse { document: None, session: "s1".into() };
        assert_eq!(document_key(&none).0, "");
    }

    #[test]
    fn formats_by_extension() {
        assert_eq!(format_of("C:\\a\\b.SLDPRT").unwrap().name, "part");
        assert_eq!(format_of("C:\\a\\b.stp").unwrap().name, "STEP");
        assert_eq!(format_of("C:\\a\\b.x_t").unwrap().name, "Parasolid");
        assert!(format_of("C:\\a\\b.docx").is_none());
        assert!(format_of("C:\\a\\b").is_none());
    }

    #[test]
    fn save_in_place_sends_no_path() {
        let request = save_request(&save(None)).unwrap();
        assert!(request.path.is_empty() && request.allowed_types.is_empty());
    }

    #[test]
    fn save_checks_the_path() {
        assert!(save_request(&save(Some("block.SLDPRT"))).is_err(), "relative");
        assert!(save_request(&save(Some("C:\\x\\block.docx"))).is_err(), "unknown format");
        let dir = std::env::temp_dir();
        let missing = dir.join("sld_mcp_no_such_dir").join("a.SLDPRT");
        if is_absolute(missing.to_str().unwrap()) {
            assert!(save_request(&save(missing.to_str())).is_err(), "no folder");
            let existing = dir.join("sld_mcp_files_test.step");
            std::fs::write(&existing, b"x").unwrap();
            let p = existing.to_str().unwrap();
            assert!(save_request(&save(Some(p))).is_err(), "exists");
            let request = save_request(&SaveDocumentParams { path: Some(p.into()), copy: false, overwrite: true })
                .unwrap();
            assert!(request.copy, "exports are copies");
            assert_eq!(request.allowed_types, vec![DocumentType::Part as i32, DocumentType::Assembly as i32]);
            std::fs::remove_file(existing).unwrap();
        }
    }

    #[test]
    fn open_takes_native_files_only() {
        let open = |p: &str| open_request(&OpenDocumentParams { path: p.into(), read_only: false });
        assert!(open("C:\\x\\block.step").is_err());
        assert!(open("block.SLDPRT").is_err());
        assert!(open("C:\\sld_mcp_no_such_dir\\block.SLDPRT").is_err());
    }

    #[test]
    fn bits_are_named() {
        assert_eq!(bit_names(64 | 2, SAVE_ERRORS).len(), 2);
        assert_eq!(bit_names(1 << 30, SAVE_ERRORS), vec![format!("code {}", 1 << 30)]);
        assert!(bit_names(0, LOAD_WARNINGS).is_empty());
    }

    #[test]
    fn failed_save_is_an_error() {
        let request = save_request(&save(None)).unwrap();
        let reply = proto::SaveDocumentResponse { saved: false, errors: 2, saved_path: "C:\\a.SLDPRT".into(), ..Default::default() };
        let err = save_reply_text(&request, &reply).unwrap_err();
        assert!(err.message.contains("read-only"), "{}", err.message);
        // Saved after all: the generic error isn't repeated.
        let reply = proto::SaveDocumentResponse { saved: true, errors: 1, ..reply };
        assert!(!save_reply_text(&request, &reply).unwrap().contains("reported"));
    }

    #[test]
    fn missing_references_are_explained() {
        let request = proto::OpenDocumentRequest { path: "C:\\a.SLDASM".into(), r#type: 2, read_only: false };
        let doc = proto::Document { title: "a.SLDASM".into(), r#type: 2, path: request.path.clone() };
        let reply = proto::OpenDocumentResponse { document: Some(doc), opened: true, errors: 2, ..Default::default() };
        let text = open_reply_text(&request, &reply).unwrap();
        assert!(text.contains("suppressed") && !text.contains("file wasn't found"), "{text}");
        let reply = proto::OpenDocumentResponse { document: None, opened: false, errors: 2, ..reply };
        assert!(open_reply_text(&request, &reply).unwrap_err().message.contains("wasn't found"));
    }
}
