use anyhow::Result;
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::unix::{io::{AsFd, BorrowedFd}, net::UnixStream},
    path::PathBuf,
};

#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: u64,
    pub idx: u8,
    pub is_focused: bool,
}

#[derive(Debug, Default)]
pub struct NiriState {
    pub workspaces: Vec<Workspace>,
    pub focused_window_title: Option<String>,
    windows: HashMap<u64, String>,
    focused_window_id: Option<u64>,
    socket_path: Option<PathBuf>,
    event_stream: Option<BufReader<UnixStream>>,
    action_stream: Option<UnixStream>,
}

fn find_socket() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NIRI_SOCKET") {
        let path = PathBuf::from(p);
        if path.exists() { return Some(path); }
    }
    // Search all uid dirs
    let uid_dirs = std::fs::read_dir("/run/user").ok()?;
    for uid_dir in uid_dirs.flatten() {
        if let Ok(entries) = std::fs::read_dir(uid_dir.path()) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("niri.wayland-") && name.ends_with(".sock") {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

/// Drain all available lines from a nonblocking BufReader.
fn drain_lines(reader: &mut BufReader<UnixStream>) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => lines.push(line),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    lines
}

impl NiriState {
    /// Connect to niri. Must be called before privilege drop.
    pub fn connect() -> Option<NiriState> {
        let socket_path = find_socket()?;
        eprintln!("[niri] socket: {}", socket_path.display());

        // --- Event stream socket ---
        let mut stream = UnixStream::connect(&socket_path).ok()?;
        stream.write_all(b"\"EventStream\"\n").ok()?;
        let mut reader = BufReader::new(stream);
        // Consume {"Ok":"Handled"}
        let mut ack = String::new();
        reader.read_line(&mut ack).ok()?;

        // --- Action socket (persistent, stays open) ---
        let action_stream = UnixStream::connect(&socket_path).ok();
        if action_stream.is_none() {
            eprintln!("[niri] warning: could not open action socket");
        }

        let mut state = NiriState {
            socket_path: Some(socket_path),
            event_stream: Some(reader),
            action_stream,
            ..Default::default()
        };

        // Read initial burst with 3s timeout
        state.read_initial_state();

        eprintln!("[niri] ready: {} workspaces, window: {:?}",
            state.workspaces.len(), state.focused_window_title);

        if let Some(ref r) = state.event_stream {
            let _ = r.get_ref().set_nonblocking(true);
        }

        Some(state)
    }

    fn read_initial_state(&mut self) {
        let reader = match self.event_stream.as_mut() {
            Some(r) => r,
            None => return,
        };
        let _ = reader.get_ref().set_read_timeout(Some(std::time::Duration::from_secs(3)));
        let mut got_workspaces = false;
        let mut got_windows = false;
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line.contains("WorkspacesChanged") { got_workspaces = true; }
                    if line.contains("WindowsChanged") { got_windows = true; }
                    lines.push(line);
                    if got_workspaces && got_windows { break; }
                }
            }
        }
        let _ = reader.get_ref().set_read_timeout(None);
        for line in &lines {
            self.apply_event_line(line.trim());
        }
    }

    /// Drain pending events. Returns true if state changed.
    pub fn process_events(&mut self) -> bool {
        let lines = match self.event_stream.as_mut() {
            Some(r) => drain_lines(r),
            None => return false,
        };
        let mut changed = false;
        for line in &lines {
            if self.apply_event_line(line.trim()) { changed = true; }
        }
        changed
    }

    fn apply_event_line(&mut self, line: &str) -> bool {
        if line.is_empty() { return false; }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            eprintln!("[niri] parse error: {}", line);
            return false;
        };

        // WorkspaceActivated when switching workspaces
        if let Some(inner) = event.get("WorkspaceActivated") {
            if let (Some(id), Some(focused)) = (inner["id"].as_u64(), inner["focused"].as_bool()) {
                let mut changed = false;
                for ws in &mut self.workspaces {
                    let was_focused = ws.is_focused;
                    ws.is_focused = focused && ws.id == id;
                    if ws.is_focused != was_focused { changed = true; }
                }
                return changed;
            }
            return false;
        }

        // Full workspace list on add/remove
        if let Some(inner) = event.get("WorkspacesChanged") {
            if let Some(arr) = inner["workspaces"].as_array() {
                let mut new_ws: Vec<Workspace> = arr.iter().filter_map(parse_workspace).collect();
                new_ws.sort_by_key(|w| w.idx);
                if !workspaces_eq(&self.workspaces, &new_ws) {
                    self.workspaces = new_ws;
                    return true;
                }
            }
            return false;
        }

        if let Some(inner) = event.get("WindowsChanged") {
            if let Some(arr) = inner["windows"].as_array() {
                self.windows.clear();
                self.focused_window_id = None;
                let mut new_title = None;
                for w in arr {
                    if let (Some(id), Some(title)) = (w["id"].as_u64(), w["title"].as_str()) {
                        self.windows.insert(id, title.to_string());
                        if w["is_focused"].as_bool().unwrap_or(false) {
                            self.focused_window_id = Some(id);
                            new_title = Some(title.to_string());
                        }
                    }
                }
                if new_title != self.focused_window_title {
                    self.focused_window_title = new_title;
                    return true;
                }
            }
            return false;
        }

        // Focus changed
        if let Some(inner) = event.get("WindowFocusChanged") {
            let new_id = inner["id"].as_u64();
            if new_id == self.focused_window_id { return false; }
            self.focused_window_id = new_id;
            let new_title = new_id.and_then(|id| self.windows.get(&id)).cloned();
            if new_title != self.focused_window_title {
                self.focused_window_title = new_title;
                return true;
            }
            return false;
        }

        // window opened updated
        if let Some(inner) = event.get("WindowOpenedOrChanged") {
            if let Some(w) = inner.get("window") {
                if let (Some(id), Some(title)) = (w["id"].as_u64(), w["title"].as_str()) {
                    self.windows.insert(id, title.to_string());
                    if self.focused_window_id == Some(id) {
                        let new_title = Some(title.to_string());
                        if new_title != self.focused_window_title {
                            self.focused_window_title = new_title;
                            return true;
                        }
                    }
                }
            }
            return false;
        }

        // Window closed
        if let Some(inner) = event.get("WindowClosed") {
            if let Some(id) = inner["id"].as_u64() {
                self.windows.remove(&id);
                if self.focused_window_id == Some(id) {
                    self.focused_window_id = None;
                    if self.focused_window_title.is_some() {
                        self.focused_window_title = None;
                        return true;
                    }
                }
            }
            return false;
        }

        false
    }

    /// Focus a workspace by idx.
    pub fn focus_workspace(&mut self, idx: u8) {
        let req = format!(
            "{{\"Action\":{{\"FocusWorkspace\":{{\"reference\":{{\"Index\":{}}}}}}}}}\n",
            idx
        );
        if let Some(ref mut sock) = self.action_stream {
            if sock.write_all(req.as_bytes()).is_err() {
                eprintln!("[niri] action socket write failed");
                self.action_stream = None;
            }
        } else {
            eprintln!("[niri] no action socket available");
        }
    }
}

impl AsFd for NiriState {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.event_stream
            .as_ref()
            .expect("NiriState has no event stream")
            .get_ref()
            .as_fd()
    }
}

fn parse_workspace(w: &Value) -> Option<Workspace> {
    Some(Workspace {
        id: w["id"].as_u64()?,
        idx: w["idx"].as_u64()? as u8,
        is_focused: w["is_focused"].as_bool().unwrap_or(false),
    })
}

fn workspaces_eq(a: &[Workspace], b: &[Workspace]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| {
        x.id == y.id && x.idx == y.idx && x.is_focused == y.is_focused
    })
}
