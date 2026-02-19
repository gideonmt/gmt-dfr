use anyhow::Result;
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: u64,
    pub idx: u8,
    pub is_focused: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NiriState {
    pub workspaces: Vec<Workspace>,
    pub focused_window_title: Option<String>,
    socket_path: Option<PathBuf>,
}

fn find_socket() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NIRI_SOCKET") {
        return Some(PathBuf::from(p));
    }
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/run/user/{}", uid));
    std::fs::read_dir(&dir).ok()?.find_map(|e| {
        let e = e.ok()?;
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("niri.wayland-") && name.ends_with(".sock") {
            Some(e.path())
        } else {
            None
        }
    })
}

fn niri_request(socket: &PathBuf, request_json: &str) -> Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(request_json.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(serde_json::from_str(response.trim())?)
}

impl NiriState {
    pub fn connect() -> Option<NiriState> {
        let socket_path = find_socket()?;
        eprintln!("[niri] found socket: {}", socket_path.display());
        let mut state = NiriState {
            socket_path: Some(socket_path),
            ..Default::default()
        };
        state.poll_workspaces();
        state.poll_window();
        eprintln!(
            "[niri] initial state: {} workspaces, window: {:?}",
            state.workspaces.len(),
            state.focused_window_title
        );
        Some(state)
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        changed |= self.poll_workspaces();
        changed |= self.poll_window();
        changed
    }

    fn poll_workspaces(&mut self) -> bool {
        let Some(socket) = self.socket_path.clone() else {
            return false;
        };
        let Ok(resp) = niri_request(&socket, "\"Workspaces\"") else {
            return false;
        };
        let Some(arr) = resp
            .get("Ok")
            .and_then(|ok| ok.get("Workspaces"))
            .and_then(|w| w.as_array())
        else {
            eprintln!("[niri] unexpected workspaces response: {}", resp);
            return false;
        };

        let mut new_workspaces: Vec<Workspace> = arr
            .iter()
            .filter_map(|w| {
                Some(Workspace {
                    id: w["id"].as_u64()?,
                    idx: w["idx"].as_u64()? as u8,
                    is_focused: w["is_focused"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        new_workspaces.sort_by_key(|w| w.idx);

        if !workspaces_eq(&self.workspaces, &new_workspaces) {
            self.workspaces = new_workspaces;
            return true;
        }
        false
    }

    fn poll_window(&mut self) -> bool {
        let Some(socket) = self.socket_path.clone() else {
            return false;
        };
        let Ok(resp) = niri_request(&socket, "\"FocusedWindow\"") else {
            return false;
        };
        let new_title = resp
            .get("Ok")
            .and_then(|ok| ok.get("FocusedWindow"))
            .and_then(|fw| fw.get("window"))
            .and_then(|w| w.get("title"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());

        if new_title != self.focused_window_title {
            self.focused_window_title = new_title;
            return true;
        }
        false
    }

    pub fn focus_workspace(&self, idx: u8) {
        let Some(socket) = &self.socket_path else { return };
        let req = format!(
            "{{\"Action\":{{\"FocusWorkspace\":{{\"reference\":{{\"Index\":{}}}}}}}}}",
            idx
        );
        let _ = niri_request(socket, &req);
    }
}

fn workspaces_eq(a: &[Workspace], b: &[Workspace]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.id == y.id && x.idx == y.idx && x.is_focused == y.is_focused)
}
