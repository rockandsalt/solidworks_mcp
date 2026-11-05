//! The run_macro tool: the escape hatch for what no other tool does. The model's
//! VBA goes into a plain-text macro file (.swb) with a main of ours around its
//! own, which catches its errors and collects what it prints; dialogs it opens
//! anyway are closed from here, since they would block SolidWorks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::{AddinClient, proto};
use crate::discovery;

/// More than this of a macro's output is cut off in the reply.
const MAX_OUTPUT_CHARS: usize = 50_000;

const DIALOG_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunMacroParams {
    /// A VBA module with a `Sub main()`, in ASCII: write other characters with ChrW().
    pub code: String,
}

/// The code with comments cut off and what's in strings blanked.
fn code_part(line: &str) -> String {
    let mut in_string = false;
    let mut code = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '"' => {
                in_string = !in_string;
                code.push(c);
            }
            '\'' if !in_string => break,
            _ => code.push(if in_string { ' ' } else { c }),
        }
    }
    code
}

/// Whether the line has `word` as a statement, not as `object.word` or inside a name.
fn has_statement(code: &str, word: &str) -> bool {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let lower = code.to_ascii_lowercase();
    lower.match_indices(word).any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + word.len()..].chars().next();
        !before.is_some_and(|c| is_name(c) || c == '.') && !after.is_some_and(is_name)
    })
}

/// `Sub main()` of a line, as (what's before the name, what's after the parentheses).
fn main_declaration(line: &str) -> Option<(&str, &str)> {
    let code = code_part(line).to_ascii_lowercase();
    let start = code.len() - code.trim_start().len();
    let mut rest = &code[start..];
    for visibility in ["public ", "private "] {
        rest = rest.strip_prefix(visibility).map_or(rest, str::trim_start);
    }
    let name = rest.strip_prefix("sub ")?.trim_start();
    let parameters = name.strip_prefix("main")?.trim_start();
    let parameters = parameters.strip_prefix('(')?.trim_start();
    if !parameters.starts_with(')') {
        return None;
    }
    // Lengths in the lowercased copy are the same in the ASCII original.
    let name_at = code.len() - name.len();
    let close = code.len() - parameters.len() + 1;
    Some((&line[..name_at], &line[close..]))
}

/// A VBA expression for the text; a .swb is read as ANSI, so anything else goes by ChrW.
fn vba_string(text: &str) -> String {
    let mut parts = vec![String::new()];
    for unit in text.encode_utf16() {
        if (32..127).contains(&unit) && unit != u16::from(b'"') {
            parts.last_mut().unwrap().push(char::from(unit as u8));
        } else {
            parts.push(format!("ChrW({unit})"));
            parts.push(String::new());
        }
    }
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .map(|part| if part.starts_with("ChrW(") { part.clone() } else { format!("\"{part}\"") })
        .collect::<Vec<_>>()
        .join(" & ")
}

/// What a run leaves in its folder.
struct MacroFiles {
    source: PathBuf,
    output: PathBuf,
    error: PathBuf,
    /// Made when main starts. RunMacro2 runs a macro stopped by `End` a second
    /// time and returns false (seen 2026-11-01), so this keeps it from doing its
    /// work twice, and tells that false from one for a macro that doesn't compile.
    started: PathBuf,
    completed: PathBuf,
}

impl MacroFiles {
    fn in_folder(folder: &Path) -> Self {
        Self {
            source: folder.join("macro.swb"),
            output: folder.join("output.txt"),
            error: folder.join("error.txt"),
            started: folder.join("started.txt"),
            completed: folder.join("completed.txt"),
        }
    }
}

/// The macro file's text: the model's module, its main renamed, and ours.
fn macro_source(code: &str, files: &MacroFiles) -> Result<String, String> {
    if let Some(c) = code.chars().find(|c| !c.is_ascii()) {
        return Err(format!(
            "SolidWorks would garble {c:?}: macro files are read as ANSI, so keep the code in ASCII and write \
             other characters with ChrW({}) in strings",
            u32::from(c)
        ));
    }
    let mut lines = Vec::new();
    let mut mains = 0;
    for line in code.lines() {
        let part = code_part(line);
        // These stop in the VBA debugger, which blocks SolidWorks with no dialog to close (seen 2026-11-01).
        if has_statement(&part, "stop") || has_statement(&part, "debug.assert") {
            return Err(format!(
                "Stop and Debug.Assert would hang SolidWorks in the VBA debugger; take this line out: {}",
                line.trim()
            ));
        }
        match main_declaration(line) {
            Some((before, after)) => {
                mains += 1;
                lines.push(format!("{before}McpUserMain(){after}"));
            }
            None => lines.push(line.to_string()),
        }
    }
    if mains != 1 {
        return Err(format!("the code must define one `Sub main()` with no parameters, it has {mains}"));
    }
    let output_file = vba_string(&files.output.to_string_lossy());
    let error_file = vba_string(&files.error.to_string_lossy());
    let started_file = vba_string(&files.started.to_string_lossy());
    let completed_file = vba_string(&files.completed.to_string_lossy());
    let ours = format!(
        r#"
Private Sub McpAppend(file As String, text As String)
    Dim stream As Object
    Set stream = CreateObject("Scripting.FileSystemObject").OpenTextFile(file, 8, True, -1)
    stream.WriteLine text
    stream.Close
End Sub

Public Sub McpPrint(text As Variant)
    McpAppend {output_file}, CStr(text)
End Sub

Private Sub McpReportError(text As String)
    On Error Resume Next
    McpAppend {error_file}, text
End Sub

Sub main()
    On Error GoTo McpFailed
    Dim files As Object
    Set files = CreateObject("Scripting.FileSystemObject")
    If files.FileExists({started_file}) Then Exit Sub
    files.CreateTextFile({started_file}).Close
    McpUserMain
    files.CreateTextFile({completed_file}).Close
    Exit Sub
McpFailed:
    McpReportError "error " & Err.Number & ": " & Err.Description
End Sub"#
    );
    lines.extend(ours.lines().map(str::to_string));
    lines.push(String::new());
    Ok(lines.join("\r\n"))
}

/// What the macro left in a file of its: UTF-16 with a BOM.
fn read_text(file: &Path) -> String {
    let Ok(bytes) = std::fs::read(file) else {
        return String::new();
    };
    let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    let text = String::from_utf16_lossy(units.strip_prefix(&[0xFEFF]).unwrap_or(&units));
    text.replace("\r\n", "\n").trim_end().to_string()
}

fn capped(output: String) -> String {
    match output.char_indices().nth(MAX_OUTPUT_CHARS) {
        Some((at, _)) => format!("{}\n... (output cut off at {MAX_OUTPUT_CHARS} characters)", &output[..at]),
        None => output,
    }
}

/// The reply: what it printed and which dialogs had to be closed, after a first line.
fn reply_text(headline: &str, output: String, dialogs: &[dialogs::Closed]) -> String {
    let mut text = headline.to_string();
    if !output.is_empty() {
        text += &format!("\n\nOutput:\n{}", capped(output));
    }
    if !dialogs.is_empty() {
        text += "\n\nIt opened dialogs nobody can answer, so they were closed:";
        for dialog in dialogs {
            text += &format!("\n- \"{}\": {} (pressed {})", dialog.title, dialog.text, dialog.pressed);
        }
    }
    text
}

fn new_folder() -> std::io::Result<PathBuf> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    loop {
        let folder = std::env::temp_dir().join(format!(
            "sld_mcp-macro-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A reused process id must not reuse an old run's start/completion markers.
        match std::fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Writes the macro, runs it with an eye on the dialogs, and says how it went.
pub async fn run(addin: &AddinClient, params: &RunMacroParams) -> Result<String, McpError> {
    let folder = new_folder()
        .map_err(|error| McpError::internal_error(format!("no temp folder: {error}"), None))?;
    let outcome = run_in(addin, &params.code, &folder).await;
    // On a transport failure it may still be queued or executing. Keep its source,
    // markers and output both for late execution and for diagnosis.
    if !outcome
        .as_ref()
        .is_err_and(|failure| failure.preserve_files)
    {
        let _ = std::fs::remove_dir_all(&folder);
    }
    outcome.map_err(|failure| failure.error)
}

struct Failure {
    error: McpError,
    preserve_files: bool,
}

impl Failure {
    fn new(error: McpError) -> Self {
        Self {
            error,
            preserve_files: false,
        }
    }
}

async fn run_in(addin: &AddinClient, code: &str, folder: &Path) -> Result<String, Failure> {
    let files = MacroFiles::in_folder(folder);
    let source = macro_source(code, &files)
        .map_err(|message| Failure::new(McpError::invalid_params(message, None)))?;
    std::fs::write(&files.source, source).map_err(|error| {
        Failure::new(McpError::internal_error(
            format!("can't write the macro: {error}"),
            None,
        ))
    })?;

    let started_at = Instant::now();
    let (result, closed) = closing_dialogs(addin.run_macro(proto::RunMacroRequest {
        path: files.source.to_string_lossy().into_owned(),
    }))
    .await;

    finish_run(
        &files,
        result,
        &closed,
        started_at.elapsed(),
        addin.endpoint().as_deref(),
    )
}

/// Interpret filesystem evidence independently of the transport result. No marker
/// means startup is unconfirmed; a start marker alone cannot prove it still runs.
fn finish_run(
    files: &MacroFiles,
    result: Result<proto::RunMacroResponse, tonic::Status>,
    closed: &[Closed],
    elapsed: Duration,
    endpoint: Option<&str>,
) -> Result<String, Failure> {
    let output = read_text(&files.output);
    let error = read_text(&files.error);
    let started = files.started.exists();
    let completed = files.completed.exists();
    let failed = |headline: String| Failure {
        error: McpError::internal_error(
            reply_text(
                &format!(
                    "{headline}\nDiagnostics preserved in {}",
                    files.source.parent().unwrap().display()
                ),
                output.clone(),
                closed,
            ),
            None,
        ),
        preserve_files: true,
    };
    let reply = match result {
        Ok(reply) => reply,
        Err(status) => {
            let evidence = if !error.is_empty() {
                format!("The wrapper reported failure: {error}.")
            } else if completed {
                "The wrapper recorded completion.".to_string()
            } else if started {
                "The wrapper started, but no completion was recorded. Whether it is still running is unknown.".to_string()
            } else {
                "Macro startup was not observed (no started.txt). It may be queued, failed before wrapper entry, or have an unobserved execution state.".to_string()
            };
            return Err(failed(format!(
                "Macro RPC ended after {:.1}s with {:?}: {}. {evidence}\nEndpoint: {}. \
                 A transport timeout or cancellation does not stop an in-flight COM call. \
                 Other tools may wait for outstanding work. Do not retry a macro that changes the model until its outcome is known. \
                 Output is captured only by McpPrint; Debug.Print is not captured.",
                elapsed.as_secs_f64(),
                status.code(),
                status.message(),
                endpoint.unwrap_or("unavailable")
            )));
        }
    };
    if !error.is_empty() {
        return Err(failed(format!(
            "the macro stopped with {error}. What it did before that is still in the model (see undo)."
        )));
    }
    if !started && !completed {
        return Err(failed(format!(
            "Macro startup was not observed (no started.txt); RunMacro2 returned ran={}. \
             A compile error, missing file, launch failure or wrapper initialization failure is possible; \
             SolidWorks gives no reason or line. Check macro.swb and use McpPrint for output.",
            reply.ran
        )));
    }
    Ok(reply_text(
        if completed {
            "The macro ran. The wrapper recorded completion. Ids of what it changed may no longer work: inspect_model again before using them."
        } else {
            // VBA End stops the wrapper too; RunMacro2 has now returned, so it
            // is no longer running even though it skipped the completion marker.
            "The macro ran. RunMacro2 returned without wrapper completion (for example, VBA End). Ids of what it changed may no longer work: inspect_model again before using them."
        },
        output,
        closed,
    ))
}

/// Runs an add-in call while closing the dialogs SolidWorks opens meanwhile
/// (they would block it), and says which ones were closed.
pub async fn closing_dialogs<F: std::future::Future>(call: F) -> (F::Output, Vec<Closed>) {
    let mut watcher = dialogs::Watcher::new(discovery::find().ok().and_then(|target| target.pid));
    tokio::pin!(call);
    let mut poll = tokio::time::interval(DIALOG_POLL);
    let result = loop {
        tokio::select! {
            result = &mut call => break result,
            _ = poll.tick() => watcher.close_new(),
        }
    };
    (result, watcher.closed)
}

pub use dialogs::Closed;

/// Closing dialogs of SolidWorks: runtime errors the macro's main can't catch,
/// MsgBox and whatever an API call asks.
mod dialogs {
    #[derive(Debug, Clone, PartialEq)]
    pub struct Closed {
        pub title: String,
        pub text: String,
        pub pressed: String,
    }

    /// The buttons to press, in this order: those that leave things as they are first.
    #[cfg_attr(not(windows), allow(dead_code))]
    const BUTTONS: [&str; 5] = ["End", "Cancel", "No", "OK", "Close"];

    #[cfg_attr(not(windows), allow(dead_code))]
    fn button_rank(label: &str) -> Option<usize> {
        let label = label.replace('&', "");
        BUTTONS.iter().position(|button| button.eq_ignore_ascii_case(&label))
    }

    pub struct Watcher {
        #[cfg_attr(not(windows), allow(dead_code))]
        pid: Option<u32>,
        #[cfg_attr(not(windows), allow(dead_code))]
        known: Vec<isize>,
        pub closed: Vec<Closed>,
    }

    #[cfg(windows)]
    impl Watcher {
        /// Dialogs open now aren't the macro's: they stay.
        pub fn new(pid: Option<u32>) -> Self {
            let known = pid.map(win::dialogs_of).unwrap_or_default();
            Self { pid, known, closed: Vec::new() }
        }

        pub fn close_new(&mut self) {
            let Some(pid) = self.pid else { return };
            for window in win::dialogs_of(pid) {
                if !self.known.contains(&window) {
                    self.known.push(window);
                    self.closed.push(win::close(window));
                }
            }
        }
    }

    #[cfg(not(windows))]
    impl Watcher {
        pub fn new(pid: Option<u32>) -> Self {
            Self { pid, known: Vec::new(), closed: Vec::new() }
        }

        pub fn close_new(&mut self) {}
    }

    #[cfg(windows)]
    mod win {
        use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            BM_CLICK, EnumChildWindows, EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId,
            IsWindowVisible, PostMessageW, WM_CLOSE,
        };

        use super::{Closed, button_rank};

        const DIALOG_CLASS: &str = "#32770";

        fn class_of(window: HWND) -> String {
            let mut name = [0u16; 64];
            // SAFETY: the buffer outlives the call, which writes at most its length.
            let len = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
            String::from_utf16_lossy(&name[..len.max(0) as usize])
        }

        fn text_of(window: HWND) -> String {
            let mut text = [0u16; 2048];
            // SAFETY: the buffer outlives the call, which writes at most its length.
            let len = unsafe { GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32) };
            String::from_utf16_lossy(&text[..len.max(0) as usize])
        }

        unsafe extern "system" fn collect(window: HWND, list: LPARAM) -> i32 {
            // SAFETY: `list` is the Vec the callers below keep alive during the enumeration.
            unsafe { (*(list as *mut Vec<isize>)).push(window as isize) };
            1
        }

        /// The process's visible top-level dialogs.
        pub fn dialogs_of(pid: u32) -> Vec<isize> {
            let mut windows: Vec<isize> = Vec::new();
            // SAFETY: `collect` only pushes onto `windows`, which outlives the call.
            unsafe { EnumWindows(Some(collect), &mut windows as *mut Vec<isize> as LPARAM) };
            windows.retain(|&window| {
                let window = window as HWND;
                let mut owner = 0u32;
                // SAFETY: plain queries; a window that's gone by now just answers 0.
                unsafe { GetWindowThreadProcessId(window, &mut owner) };
                owner == pid && unsafe { IsWindowVisible(window) } != 0 && class_of(window) == DIALOG_CLASS
            });
            windows
        }

        /// Reads the dialog and presses its best button (no such button: closes it).
        pub fn close(window: isize) -> Closed {
            let window = window as HWND;
            let mut children: Vec<isize> = Vec::new();
            // SAFETY: `collect` only pushes onto `children`, which outlives the call.
            unsafe { EnumChildWindows(window, Some(collect), &mut children as *mut Vec<isize> as LPARAM) };
            let mut texts = Vec::new();
            let mut best: Option<(usize, isize, String)> = None;
            for child in children {
                let text = text_of(child as HWND);
                if class_of(child as HWND) != "Button" {
                    if !text.trim().is_empty() {
                        texts.push(text.split_whitespace().collect::<Vec<_>>().join(" "));
                    }
                } else if let Some(rank) = button_rank(&text)
                    && best.as_ref().is_none_or(|(known, ..)| rank < *known)
                {
                    best = Some((rank, child, text.replace('&', "")));
                }
            }
            let pressed = match best {
                Some((_, button, label)) => {
                    // SAFETY: posting to a window that may be gone is harmless.
                    unsafe { PostMessageW(button as HWND, BM_CLICK, 0 as WPARAM, 0 as LPARAM) };
                    label
                }
                None => {
                    // SAFETY: as above.
                    unsafe { PostMessageW(window, WM_CLOSE, 0 as WPARAM, 0 as LPARAM) };
                    "its close box".to_string()
                }
            };
            Closed { title: text_of(window), text: texts.join(" "), pressed }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn buttons_that_change_nothing_come_first() {
            assert!(button_rank("&End") < button_rank("OK"));
            assert!(button_rank("Cancel") < button_rank("OK"));
            assert!(button_rank("&No") < button_rank("OK"));
            assert_eq!(button_rank("&Yes"), None);
            assert_eq!(button_rank("&Debug"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(code: &str) -> Result<String, String> {
        macro_source(code, &MacroFiles::in_folder(Path::new("C:\\t")))
    }

    #[test]
    fn main_runs_the_users_code_only_once() {
        let text = source("Sub main()\nEnd Sub").unwrap();
        let guard = text
            .find("If files.FileExists(\"C:\\t\\started.txt\") Then Exit Sub")
            .unwrap();
        let made = text
            .find("files.CreateTextFile(\"C:\\t\\started.txt\").Close")
            .unwrap();
        let call = text.find("    McpUserMain\r\n").unwrap();
        let done = text
            .find("files.CreateTextFile(\"C:\\t\\completed.txt\").Close")
            .unwrap();
        assert!(guard < made && made < call && call < done, "{text}");
    }

    #[test]
    fn users_main_is_renamed_and_ours_calls_it() {
        for code in ["Sub main()\nEnd Sub", "  private  SUB  Main ( ) ' start\nEnd Sub"] {
            let text = source(code).unwrap();
            assert!(text.contains("McpUserMain()"), "{text}");
            assert_eq!(text.matches("Sub main()").count(), 1, "{text}");
            assert!(text.contains("\r\n"), "{text}");
        }
        let text = source("Option Explicit\r\nPublic Sub main() ' go\r\nEnd Sub").unwrap();
        assert!(text.starts_with("Option Explicit\r\nPublic Sub McpUserMain() ' go\r\n"), "{text}");
    }

    #[test]
    fn needs_exactly_one_main() {
        for code in ["Sub other()\nEnd Sub", "Sub main()\nEnd Sub\nSub main()\nEnd Sub", "Sub main(x)\nEnd Sub", "' Sub main()", "Function main()\nEnd Function"] {
            assert!(source(code).unwrap_err().contains("one `Sub main()`"), "{code}");
        }
        // Not its name.
        assert!(source("Sub mainly()\nEnd Sub").is_err());
    }

    #[test]
    fn refuses_what_would_hang_or_garble() {
        for code in ["Sub main()\nStop\nEnd Sub", "Sub main()\n  x = 1: stop\nEnd Sub", "Sub main()\nDebug.Assert False\nEnd Sub"] {
            assert!(source(code).unwrap_err().contains("debugger"), "{code}");
        }
        assert!(source("Sub main()\nx = \"\u{4e2d}\"\nEnd Sub").unwrap_err().contains("ChrW(20013)"));
        // A method, a name, a string and a comment are fine.
        let code = "Sub main()\nmotion.Stop\nstopper = 1\nx = \"Stop\" ' Stop\nEnd Sub";
        assert!(source(code).is_ok());
    }

    #[test]
    fn strings_for_vba() {
        assert_eq!(vba_string("C:\\a b"), "\"C:\\a b\"");
        assert_eq!(vba_string("a\u{4e2d}b"), "\"a\" & ChrW(20013) & \"b\"");
        assert_eq!(vba_string("\u{4e2d}"), "ChrW(20013)");
        assert_eq!(vba_string("\u{1F600}"), "ChrW(55357) & ChrW(56832)");
    }

    #[test]
    fn replies() {
        assert_eq!(reply_text("Done.", String::new(), &[]), "Done.");
        let dialog = dialogs::Closed { title: "SOLIDWORKS".into(), text: "hello".into(), pressed: "OK".into() };
        let text = reply_text("Done.", "x=1".into(), &[dialog]);
        assert_eq!(
            text,
            "Done.\n\nOutput:\nx=1\n\nIt opened dialogs nobody can answer, so they were closed:\n- \"SOLIDWORKS\": hello (pressed OK)"
        );
        let long = capped("a".repeat(MAX_OUTPUT_CHARS + 1));
        assert!(long.ends_with("(output cut off at 50000 characters)"), "{long}");
    }

    #[test]
    fn reads_what_vba_writes() {
        let folder = new_folder().unwrap();
        let file = folder.join("output.txt");
        let bytes: Vec<u8> = [0xFEFFu16].into_iter().chain("x\u{4e2d}\r\ny\r\n".encode_utf16()).flat_map(u16::to_le_bytes).collect();
        std::fs::write(&file, bytes).unwrap();
        assert_eq!(read_text(&file), "x\u{4e2d}\ny");
        assert_eq!(read_text(&folder.join("none.txt")), "");
        std::fs::remove_dir_all(folder).unwrap();
    }

    fn write_vba_text(file: &Path, text: &str) {
        let bytes: Vec<u8> = [0xFEFFu16]
            .into_iter()
            .chain(text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        std::fs::write(file, bytes).unwrap();
    }

    fn transport_failure(files: &MacroFiles, code: tonic::Code) -> Failure {
        match finish_run(
            files,
            Err(tonic::Status::new(code, "test transport failure")),
            &[],
            Duration::from_millis(1234),
            Some("test endpoint"),
        ) {
            Err(failure) => failure,
            Ok(text) => panic!("unexpected success: {text}"),
        }
    }

    #[test]
    fn transport_failure_without_markers_does_not_claim_execution() {
        let folder = new_folder().unwrap();
        let files = MacroFiles::in_folder(&folder);
        for code in [
            tonic::Code::Cancelled,
            tonic::Code::DeadlineExceeded,
            tonic::Code::Unavailable,
        ] {
            let failure = transport_failure(&files, code);
            let text = failure.error.message.to_string();
            assert!(failure.preserve_files);
            assert!(
                text.contains("startup was not observed") && text.contains("may be queued"),
                "{text}"
            );
            assert!(
                text.contains("1.2s")
                    && text.contains("test endpoint")
                    && text.contains(&folder.display().to_string()),
                "{text}"
            );
            assert!(text.contains("Debug.Print is not captured"), "{text}");
            assert!(
                !text.contains("ending SolidWorks") && !text.contains("It goes on in SolidWorks"),
                "{text}"
            );
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn timeout_preserves_partial_output_and_reports_start_without_claiming_liveness() {
        let folder = new_folder().unwrap();
        let files = MacroFiles::in_folder(&folder);
        std::fs::write(&files.started, "").unwrap();
        write_vba_text(&files.output, "title=assembly\r\n");
        let failure = transport_failure(&files, tonic::Code::Cancelled);
        let text = failure.error.message.to_string();
        assert!(failure.preserve_files);
        assert!(
            text.contains("wrapper started") && text.contains("still running is unknown"),
            "{text}"
        );
        assert!(text.contains("Output:\ntitle=assembly"), "{text}");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn terminal_evidence_survives_transport_failure() {
        let folder = new_folder().unwrap();
        let files = MacroFiles::in_folder(&folder);
        std::fs::write(&files.started, "").unwrap();
        std::fs::write(&files.completed, "").unwrap();
        let text = transport_failure(&files, tonic::Code::DeadlineExceeded)
            .error
            .message
            .to_string();
        assert!(text.contains("wrapper recorded completion"), "{text}");
        write_vba_text(&files.error, "error 11: Division by zero");
        let text = transport_failure(&files, tonic::Code::Unavailable)
            .error
            .message
            .to_string();
        assert!(
            text.contains("wrapper reported failure: error 11"),
            "{text}"
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn launch_failure_is_not_assumed_to_be_a_compile_error() {
        let folder = new_folder().unwrap();
        let files = MacroFiles::in_folder(&folder);
        for ran in [false, true] {
            let failure = finish_run(
                &files,
                Ok(proto::RunMacroResponse { ran }),
                &[],
                Duration::ZERO,
                None,
            )
            .err()
            .unwrap();
            let text = failure.error.message.to_string();
            assert!(failure.preserve_files);
            assert!(
                text.contains("startup was not observed") && text.contains("launch failure"),
                "{text}"
            );
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn returned_macro_distinguishes_completion_end_and_runtime_failure() {
        let folder = new_folder().unwrap();
        let files = MacroFiles::in_folder(&folder);
        std::fs::write(&files.started, "").unwrap();
        let finish = || {
            finish_run(
                &files,
                Ok(proto::RunMacroResponse { ran: false }),
                &[],
                Duration::ZERO,
                None,
            )
        };
        // VBA End returns false and skips our wrapper's completion marker.
        assert!(
            finish()
                .ok()
                .unwrap()
                .contains("without wrapper completion")
        );
        std::fs::write(&files.completed, "").unwrap();
        assert!(
            finish()
                .ok()
                .unwrap()
                .contains("wrapper recorded completion")
        );
        write_vba_text(&files.error, "error 11: Division by zero");
        let failure = finish().err().unwrap();
        assert!(failure.preserve_files);
        assert!(
            failure
                .error
                .message
                .contains("macro stopped with error 11")
        );
        std::fs::remove_dir_all(folder).unwrap();
    }
}
