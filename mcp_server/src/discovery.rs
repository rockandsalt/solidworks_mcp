//! Finding the add-in. Its gRPC server listens on whatever free port the OS
//! gave it and publishes it in `%LOCALAPPDATA%\sld_mcp\instances\<pid>.json`
//! (sld_mcp/InstanceFile.cpp), with the token every call must carry:
//!
//! ```json
//! {"pid": 12345, "port": 61873, "token": "<32 hex digits>", "started": "2026-10-25T10:12:00Z"}
//! ```
//!
//! Files whose process is no longer a running SolidWorks were left by a
//! crash and are deleted. `SLD_MCP_PORT` pins the port instead (both sides
//! read it); the token then comes from the file with that port, if any.

use std::path::PathBuf;

use serde::Deserialize;

const PORT_ENV: &str = "SLD_MCP_PORT";

/// Where to call and with which token.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub port: u16,
    /// None: no instance file for the port (SLD_MCP_PORT pointing at a test
    /// server, say); calls go out without one.
    pub token: Option<String>,
    /// The SolidWorks process, when it came from an instance file.
    pub pid: Option<u32>,
}

#[derive(Deserialize)]
struct InstanceFile {
    pid: u32,
    port: u16,
    token: String,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`, so it sorts as text.
    started: String,
}

/// The add-in to call: the SLD_MCP_PORT one, else the most recently started
/// running SolidWorks. Err says why there's none, for the user.
pub fn find() -> Result<Target, String> {
    let pinned = match std::env::var(PORT_ENV) {
        Ok(value) => Some(
            value
                .parse::<u16>()
                .ok()
                .filter(|&port| port != 0)
                .ok_or_else(|| format!("{PORT_ENV}={value:?} is not a valid port"))?,
        ),
        Err(_) => None,
    };
    choose(pinned, live_instances()).ok_or_else(|| match instances_dir() {
        Some(dir) => format!("no running SolidWorks has the add-in loaded (no instance file in {})", dir.display()),
        None => "LOCALAPPDATA isn't set, so the add-in's instance file can't be found".to_string(),
    })
}

fn choose(pinned: Option<u16>, instances: Vec<InstanceFile>) -> Option<Target> {
    if let Some(port) = pinned {
        let file = instances.into_iter().find(|file| file.port == port);
        return Some(Target {
            port,
            pid: file.as_ref().map(|file| file.pid),
            token: file.map(|file| file.token),
        });
    }
    instances
        .into_iter()
        .max_by(|a, b| a.started.cmp(&b.started))
        .map(|file| Target { port: file.port, token: Some(file.token), pid: Some(file.pid) })
}

fn instances_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("sld_mcp").join("instances"))
}

/// The instance files of running SolidWorks processes; deletes those of
/// processes that are gone. Unreadable files are skipped, not deleted.
fn live_instances() -> Vec<InstanceFile> {
    let Some(entries) = instances_dir().and_then(|dir| std::fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(file) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<InstanceFile>(&bytes).ok())
        else {
            continue;
        };
        if is_solidworks(file.pid) {
            live.push(file);
        } else {
            tracing::info!("deleting {} (SolidWorks pid {} is gone)", path.display(), file.pid);
            let _ = std::fs::remove_file(&path);
        }
    }
    live
}

/// Whether `pid` is a running SLDWORKS.exe (not a process that got the pid
/// of a crashed one).
#[cfg(windows)]
fn is_solidworks(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    // SAFETY: plain Win32 calls on a handle this function opens and closes;
    // the buffers outlive the calls that fill them.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut exit_code = 0u32;
        let running = GetExitCodeProcess(process, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32;
        let mut name = [0u16; 1024];
        let mut len = name.len() as u32;
        let named = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, name.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        running
            && named
            && String::from_utf16_lossy(&name[..len as usize])
                .to_ascii_lowercase()
                .ends_with("\\sldworks.exe")
    }
}

#[cfg(not(windows))]
fn is_solidworks(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(pid: u32, port: u16, started: &str) -> InstanceFile {
        InstanceFile { pid, port, token: format!("token{pid}"), started: started.into() }
    }

    #[test]
    fn newest_solidworks_wins() {
        let files = vec![file(1, 6001, "2026-10-25T09:00:00Z"), file(2, 6002, "2026-10-25T10:00:00Z")];
        assert_eq!(
            choose(None, files),
            Some(Target { port: 6002, token: Some("token2".into()), pid: Some(2) })
        );
        assert_eq!(choose(None, Vec::new()), None);
    }

    #[test]
    fn pinned_port_takes_its_files_token_if_any() {
        let files = || vec![file(1, 6001, "2026-10-25T09:00:00Z"), file(2, 6002, "2026-10-25T10:00:00Z")];
        assert_eq!(
            choose(Some(6001), files()),
            Some(Target { port: 6001, token: Some("token1".into()), pid: Some(1) })
        );
        assert_eq!(choose(Some(7000), files()), Some(Target { port: 7000, token: None, pid: None }));
    }

    #[test]
    fn reads_the_addins_format() {
        let json = br#"{"pid": 12345, "port": 61873, "token": "0123456789abcdef0123456789abcdef", "started": "2026-10-25T10:12:00Z"}
"#;
        let file: InstanceFile = serde_json::from_slice(json).unwrap();
        assert_eq!((file.pid, file.port, file.token.len()), (12345, 61873, 32));
    }
}
