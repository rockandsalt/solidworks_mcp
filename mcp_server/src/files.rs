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
use crate::inspect::MM_PER_M;
use proto::DocumentType;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
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
    /// PDF of a drawing only: the sheets to put in it, by name (as inspect_drawing lists
    /// them), one page each. Default: every sheet.
    #[serde(default)]
    pub pdf_sheets: Option<Vec<String>>,
    /// PDF of a drawing only: true = in color, false = black and white. Default: SolidWorks'
    /// own PDF export option.
    #[serde(default)]
    pub pdf_color: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenDocumentParams {
    /// The file to open, as an absolute path to a .SLDPRT, .SLDASM or .SLDDRW file.
    pub path: String,
    /// Open it read-only. Default false. (Ignored when it's already open.)
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CreateDrawingParams {
    /// Drawing template (.drwdot): an absolute path, or the name of a template in SolidWorks'
    /// document template folder (e.g. "Drawing"). Default: the user's default drawing template.
    #[serde(default)]
    pub template: Option<String>,
    /// Sheet format, the border and title block (.slddrt): an absolute path, or the name of one
    /// in SolidWorks' sheet format folder (e.g. "a3 - iso", "a - landscape"). An unknown name
    /// lists the ones there are. Default: the template's (the stock template has none).
    #[serde(default)]
    pub sheet_format: Option<String>,
    /// Standard paper size of the sheet. Default: the sheet format's own size, else the
    /// template's sheet size, else A3.
    #[serde(default)]
    pub paper_size: Option<PaperSize>,
    /// A custom sheet size instead of paper_size: [width, height] in mm.
    #[serde(default)]
    pub paper_size_mm: Option<[f64; 2]>,
    /// The drawing's unit system (Tools > Options > Document Properties > Units). Default: the
    /// template's.
    #[serde(default)]
    pub units: Option<UnitSystem>,
}

/// Standard sheet sizes (swDwgPaperSizes_e).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub enum PaperSize {
    /// 297 x 210 mm, landscape.
    A4,
    /// 210 x 297 mm.
    #[serde(rename = "A4_portrait")]
    A4Portrait,
    /// 420 x 297 mm.
    A3,
    /// 594 x 420 mm.
    A2,
    /// 841 x 594 mm.
    A1,
    /// 1189 x 841 mm.
    A0,
    /// Letter, 11 x 8.5 in, landscape.
    #[serde(rename = "ANSI_A")]
    AnsiA,
    /// Letter, 8.5 x 11 in.
    #[serde(rename = "ANSI_A_portrait")]
    AnsiAPortrait,
    /// 17 x 11 in.
    #[serde(rename = "ANSI_B")]
    AnsiB,
    /// 22 x 17 in.
    #[serde(rename = "ANSI_C")]
    AnsiC,
    /// 34 x 22 in.
    #[serde(rename = "ANSI_D")]
    AnsiD,
    /// 44 x 34 in.
    #[serde(rename = "ANSI_E")]
    AnsiE,
}

impl PaperSize {
    pub fn sw(self) -> i32 {
        match self {
            PaperSize::AnsiA => 0,
            PaperSize::AnsiAPortrait => 1,
            PaperSize::AnsiB => 2,
            PaperSize::AnsiC => 3,
            PaperSize::AnsiD => 4,
            PaperSize::AnsiE => 5,
            PaperSize::A4 => 6,
            PaperSize::A4Portrait => 7,
            PaperSize::A3 => 8,
            PaperSize::A2 => 9,
            PaperSize::A1 => 10,
            PaperSize::A0 => 11,
        }
    }
}

/// swDwgPaperSizes_e's user-defined size.
pub const PAPER_USER_DEFINED: i32 = 12;

/// Document unit systems (swUnitSystem_e).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UnitSystem {
    /// Millimeter, gram, second.
    Mmgs,
    /// Centimeter, gram, second.
    Cgs,
    /// Meter, kilogram, second.
    Mks,
    /// Inch, pound, second.
    Ips,
}

impl UnitSystem {
    fn sw(self) -> i32 {
        match self {
            UnitSystem::Cgs => 1,
            UnitSystem::Mks => 2,
            UnitSystem::Ips => 3,
            UnitSystem::Mmgs => 5,
        }
    }
}

fn unit_system_name(sw: i32) -> String {
    match sw {
        1 => "CGS (centimeter, gram, second)".into(),
        2 => "MKS (meter, kilogram, second)".into(),
        3 => "IPS (inch, pound, second)".into(),
        4 => "custom".into(),
        5 => "MMGS (millimeter, gram, second)".into(),
        other => format!("unit system {other}"),
    }
}

/// Largest sheet side paper_size_mm takes.
const MAX_SHEET_MM: f64 = 10_000.0;

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
        if params.pdf_sheets.is_some() || params.pdf_color.is_some() {
            return Err(invalid("pdf_sheets / pdf_color are for exporting a PDF: give a .pdf path".into()));
        }
        return Ok(proto::SaveDocumentRequest::default());
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
        pdf: pdf_export(params, format)?,
    })
}

/// The PDF options, for a .pdf path; pdf_* for any other format is an error.
fn pdf_export(params: &SaveDocumentParams, format: &Format) -> Result<Option<proto::PdfExport>, McpError> {
    if format.name != "PDF" {
        if params.pdf_sheets.is_some() || params.pdf_color.is_some() {
            return Err(invalid(format!(
                "pdf_sheets / pdf_color are for a PDF export, not {}",
                format.name
            )));
        }
        return Ok(None);
    }
    let mut sheets: Vec<String> = Vec::new();
    for name in params.pdf_sheets.iter().flatten().map(|n| n.trim()) {
        if name.is_empty() {
            return Err(invalid("pdf_sheets has an empty sheet name".into()));
        }
        if !sheets.iter().any(|s| s == name) {
            sheets.push(name.to_string());
        }
    }
    if params.pdf_sheets.is_some() && sheets.is_empty() {
        return Err(invalid("pdf_sheets is empty -- name at least one sheet, or leave it out for all".into()));
    }
    Ok(Some(proto::PdfExport { sheets, color: params.pdf_color }))
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

/// A part or assembly file to insert into an assembly: an absolute path to
/// an existing .SLDPRT / .SLDASM.
pub fn component_file(path: &str) -> Result<String, McpError> {
    let path = checked_path(path)?;
    format_of(path)
        .filter(|f| f.native && f.types[0] != DocumentType::Drawing)
        .ok_or_else(|| invalid(format!("only parts (.SLDPRT) and assemblies (.SLDASM) go into an assembly; {path:?} isn't one")))?;
    if !Path::new(path).is_file() {
        return Err(invalid(format!("there's no file {path}")));
    }
    Ok(path.to_string())
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
        let mut text = format!("exported {} as {} to {}", doc.title, format.map_or("", |f| f.name), reply.saved_path);
        if let Some(pdf) = &request.pdf {
            let pages = reply.pdf_sheets.len();
            text += &format!(
                "
{pages} page{}, one per sheet: {}",
                if pages == 1 { "" } else { "s" },
                reply.pdf_sheets.join(", ")
            );
            if let Some(color) = pdf.color {
                text += if color { "; in color" } else { "; black and white" };
            }
        }
        text
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

/// A template or sheet format: an absolute path to an existing file with
/// `extension` ("drwdot"), or a bare name for the add-in to look up in
/// SolidWorks' folder for them. None = not given.
pub fn template_file(value: Option<&str>, extension: &str, what: &str) -> Result<String, McpError> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(String::new());
    }
    let given = Path::new(value)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| ["drwdot", "slddrt", "prtdot", "asmdot"].contains(&e.as_str()));
    if given.as_deref().is_some_and(|e| e != extension) {
        return Err(invalid(format!("{value:?} isn't a {what} -- those are .{extension} files")));
    }
    if is_absolute(value) {
        if !Path::new(value).is_file() {
            return Err(invalid(format!("there's no file {value}")));
        }
        if given.is_none() {
            return Err(invalid(format!("{value:?} isn't a {what} -- those are .{extension} files")));
        }
    } else if value.contains(['\\', '/']) {
        return Err(invalid(format!(
            "{value:?}: give a {what} as an absolute path, or by name from SolidWorks' folder for them"
        )));
    }
    Ok(value.to_string())
}

/// A sheet size from `paper_size` / `paper_size_mm` (at most one given):
/// swDwgPaperSizes_e (None = neither) and, for a custom size, width and
/// height in meters.
pub fn paper(
    paper_size: Option<PaperSize>,
    paper_size_mm: Option<[f64; 2]>,
) -> Result<(Option<i32>, f64, f64), McpError> {
    Ok(match (paper_size, paper_size_mm) {
        (Some(_), Some(_)) => {
            return Err(invalid("give paper_size or paper_size_mm, not both".into()));
        }
        (Some(size), None) => (Some(size.sw()), 0.0, 0.0),
        (None, Some([w, h])) => {
            if ![w, h].iter().all(|s| s.is_finite() && *s > 0.0 && *s <= MAX_SHEET_MM) {
                return Err(invalid(format!(
                    "paper_size_mm = [width, height] in mm, each above 0 and at most {MAX_SHEET_MM}"
                )));
            }
            (Some(PAPER_USER_DEFINED), w / MM_PER_M, h / MM_PER_M)
        }
        (None, None) => (None, 0.0, 0.0),
    })
}

/// The NewDrawing request for `params`: sizes in meters, enums as SolidWorks' values.
pub fn new_drawing_request(params: &CreateDrawingParams) -> Result<proto::NewDrawingRequest, McpError> {
    let (paper_size, width, height) = paper(params.paper_size, params.paper_size_mm)?;
    Ok(proto::NewDrawingRequest {
        template: template_file(params.template.as_deref(), "drwdot", "drawing template")?,
        sheet_format: template_file(params.sheet_format.as_deref(), "slddrt", "sheet format")?,
        paper_size,
        width,
        height,
        unit_system: params.units.map_or(0, UnitSystem::sw),
    })
}

fn file_name(path: &str) -> &str {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path)
}

/// "A3 (420 x 297 mm)", or "500 x 300 mm" for a custom size.
pub fn sheet_size_text(sheet: &proto::SheetInfo) -> String {
    let mm = |m: f64| (m * MM_PER_M * 10.0).round() / 10.0;
    let size = format!("{} x {} mm", mm(sheet.width), mm(sheet.height));
    match crate::drawing::paper_name(sheet.paper_size) {
        Some(paper) if sheet.paper_size != PAPER_USER_DEFINED => format!("{paper} ({size})"),
        _ => size,
    }
}

/// "sheet format a3 - iso" (the file's name) or "no sheet format".
pub fn sheet_format_text(format_path: &str) -> String {
    if format_path.is_empty() {
        return "no sheet format".to_string();
    }
    let name = file_name(format_path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    format!("sheet format {stem}")
}

/// "Sheet1, A3 (420 x 297 mm), sheet format a3 - iso, scale 1:2, first-angle projection"
pub fn sheet_text(sheet: &proto::SheetInfo, format_path: &str) -> String {
    let projection = if sheet.first_angle { "first-angle" } else { "third-angle" };
    format!(
        "{}, {}, {}, scale {}:{}, {projection} projection",
        sheet.name,
        sheet_size_text(sheet),
        sheet_format_text(format_path),
        sheet.scale_numerator,
        sheet.scale_denominator
    )
}

/// "created drawing Draw1 from Drawing.drwdot: Sheet1, A3 (420 x 297 mm), ..."
pub fn new_drawing_reply_text(reply: &proto::NewDrawingResponse) -> String {
    let title = reply.document.as_ref().map(|doc| doc.title.as_str()).unwrap_or_default();
    let mut text = format!("created drawing {title} from {}", file_name(&reply.template));
    if let Some(sheet) = &reply.sheet {
        text += &format!(": {}", sheet_text(sheet, &reply.sheet_format_path));
    }
    text += &format!("; units {}. It has no views yet.", unit_system_name(reply.unit_system));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(path: Option<&str>) -> SaveDocumentParams {
        SaveDocumentParams { path: path.map(Into::into), ..Default::default() }
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
            let request =
                save_request(&SaveDocumentParams { path: Some(p.into()), overwrite: true, ..Default::default() })
                    .unwrap();
            assert!(request.copy, "exports are copies");
            assert_eq!(request.allowed_types, vec![DocumentType::Part as i32, DocumentType::Assembly as i32]);
            std::fs::remove_file(existing).unwrap();
        }
    }

    #[test]
    fn pdf_options_go_with_pdf_paths_only() {
        let pdf = |sheets: Option<Vec<&str>>, color: Option<bool>, path: Option<&str>| {
            save_request(&SaveDocumentParams {
                path: path.map(Into::into),
                pdf_sheets: sheets.map(|s| s.into_iter().map(Into::into).collect()),
                pdf_color: color,
                ..Default::default()
            })
        };
        let file = std::env::temp_dir().join("sld_mcp_files_test.pdf");
        let path = file.to_str().unwrap();
        if !is_absolute(path) || file.exists() {
            return;
        }
        let request = pdf(None, None, Some(path)).unwrap();
        assert_eq!(request.pdf, Some(proto::PdfExport { sheets: vec![], color: None }), "all sheets");
        let request = pdf(Some(vec![" Sheet2 ", "Sheet1", "Sheet2"]), Some(false), Some(path)).unwrap();
        assert_eq!(request.pdf, Some(proto::PdfExport { sheets: vec!["Sheet2".into(), "Sheet1".into()], color: Some(false) }));
        assert!(pdf(Some(vec![]), None, Some(path)).is_err(), "no sheets");
        assert!(pdf(Some(vec![" "]), None, Some(path)).is_err(), "blank name");
        let dxf = file.with_extension("dxf");
        assert!(pdf(Some(vec!["Sheet1"]), None, dxf.to_str()).is_err(), "not a PDF");
        assert!(pdf(None, Some(true), None).is_err(), "in place");
        assert_eq!(save_request(&save(dxf.to_str())).unwrap().pdf, None);
    }

    #[test]
    fn pdf_reply_lists_the_pages() {
        let request = proto::SaveDocumentRequest {
            path: r"C:\d.pdf".into(),
            copy: true,
            pdf: Some(proto::PdfExport { sheets: vec![], color: Some(false) }),
            ..Default::default()
        };
        let reply = proto::SaveDocumentResponse {
            document: Some(proto::Document { title: "d - Sheet1".into(), r#type: 3, path: String::new() }),
            saved_path: r"C:\d.pdf".into(),
            saved: true,
            pdf_sheets: vec!["Sheet1".into(), "Sheet2".into()],
            ..Default::default()
        };
        let text = save_reply_text(&request, &reply).unwrap();
        assert!(text.contains("as PDF") && text.contains("2 pages, one per sheet: Sheet1, Sheet2; black and white"), "{text}");
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

    #[test]
    fn drawing_requests_map_sizes_and_units() {
        let request = new_drawing_request(&CreateDrawingParams::default()).unwrap();
        assert_eq!(request, proto::NewDrawingRequest::default());

        let params: CreateDrawingParams = serde_json::from_value(serde_json::json!({
            "sheet_format": " a3 - iso ", "paper_size": "A4_portrait", "units": "ips"
        }))
        .unwrap();
        let request = new_drawing_request(&params).unwrap();
        assert_eq!((request.sheet_format.as_str(), request.paper_size, request.unit_system), ("a3 - iso", Some(7), 3));

        let params = CreateDrawingParams { paper_size_mm: Some([500.0, 300.0]), ..Default::default() };
        let request = new_drawing_request(&params).unwrap();
        assert_eq!((request.paper_size, request.width, request.height), (Some(12), 0.5, 0.3));

        for bad in [
            CreateDrawingParams { paper_size: Some(PaperSize::A3), paper_size_mm: Some([1.0, 1.0]), ..Default::default() },
            CreateDrawingParams { paper_size_mm: Some([0.0, 300.0]), ..Default::default() },
            CreateDrawingParams { template: Some("Part.prtdot".into()), ..Default::default() },
            CreateDrawingParams { sheet_format: Some("formats\\a3 - iso".into()), ..Default::default() },
            CreateDrawingParams { template: Some("C:\\no\\such\\Drawing.drwdot".into()), ..Default::default() },
        ] {
            assert!(new_drawing_request(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn drawing_reply_names_sheet_and_units() {
        let reply = proto::NewDrawingResponse {
            document: Some(proto::Document { title: "Draw1".into(), r#type: 3, path: String::new() }),
            template: r"C:\templates\Drawing.drwdot".into(),
            sheet: Some(proto::SheetInfo {
                name: "Sheet1".into(),
                paper_size: 8,
                width: 0.42,
                height: 0.297,
                scale_numerator: 1.0,
                scale_denominator: 2.0,
                first_angle: true,
                ..Default::default()
            }),
            sheet_format_path: r"C:\formats\a3 - iso.slddrt".into(),
            unit_system: 5,
        };
        assert_eq!(
            new_drawing_reply_text(&reply),
            "created drawing Draw1 from Drawing.drwdot: Sheet1, A3 (420 x 297 mm), sheet format a3 - iso, \
             scale 1:2, first-angle projection; units MMGS (millimeter, gram, second). It has no views yet."
        );
    }
}
