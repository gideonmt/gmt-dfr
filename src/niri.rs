use anyhow::Result;
use libc;
use serde::Deserialize;
use std::{
    env,
    io::{BufRead, BufReader, Write},
    os::unix::{
        io::{AsFd, BorrowedFd},
        net::UnixStream,
    },
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
}

#[derive(Deserialize, Debug)]
struct NiriWorkspaceRaw {
    id: u64,
    idx: u8,
    is_focused: bool,
}

#[derive(Deserialize, Debug)]
struct NiriWindowRaw {
    title: Option<String>,
}

#[derive(Deserialize, Debug)]
struct WorkspacesChangedInner {
    workspaces: Vec<NiriWorkspaceRaw>,
}

#[derive(Deserialize, Debug)]
struct WindowFocusChangedInner {
    window: Option<NiriWindowRaw>,
}

pub struct NiriConnection {
    reader: BufReader<UnixStream>,
    stream_for_fd: UnixStream,
    pub state: NiriState,
}

fn socket_path() -> Result<String> {
    if let Ok(p) = env::var("NIRI_SOCKET") {
        return Ok(p);
    }
    let uid = unsafe { libc::getuid() };
    Ok(format!("/run/user/{}/niri.sock", uid))
}

impl NiriConnection {
    pub fn connect() -> Result<NiriConnection> {
        let path = socket_path()?;
        let mut stream = UnixStream::connect(&path)?;

        // Blocking write for the initial request
        stream.write_all(br#"{"action":"EventStream"}"#)?;
        stream.write_all(b"\n")?;
        // Stay blocking so we can read the initial state burst synchronously

        let stream_for_fd = stream.try_clone()?;
        let reader = BufReader::new(stream);

        let mut conn = NiriConnection {
            reader,
            stream_for_fd,
            state: NiriState::default(),
        };

        // Block-read until we have workspaces (niri sends WorkspacesChanged immediately)
        conn.read_until_initial_state();

        // Now switch to nonblocking for the epoll loop
        conn.stream_for_fd.set_nonblocking(true)?;
        conn.reader.get_ref().set_nonblocking(true)?;

        Ok(conn)
    }

    /// Blocking read loop until we've received at least one WorkspacesChanged.
    /// Times out after 2 seconds to avoid hanging if niri misbehaves.
    fn read_until_initial_state(&mut self) {
        self.reader
            .get_ref()
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        self.handle_line(&line);
                    }
                    // Stop once we have workspaces populated
                    if !self.state.workspaces.is_empty() {
                        break;
                    }
                }
            }
        }
        self.reader.get_ref().set_read_timeout(None).ok();
    }

    /// Read and process all pending lines. Returns true if state changed.
    pub fn process_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        changed |= self.handle_line(&line);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        changed
    }

    fn handle_line(&mut self, line: &str) -> bool {
        let Ok(outer) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        let Some(ok) = outer.get("Ok") else {
            return false;
        };

        if let Some(ws) = ok.get("WorkspacesChanged") {
            if let Ok(inner) = serde_json::from_value::<WorkspacesChangedInner>(ws.clone()) {
                self.state.workspaces = inner
                    .workspaces
                    .into_iter()
                    .map(|w| Workspace {
                        id: w.id,
                        idx: w.idx,
                        is_focused: w.is_focused,
                    })
                    .collect();
                self.state.workspaces.sort_by_key(|w| w.idx);
                return true;
            }
        }

        if let Some(wf) = ok.get("WindowFocusChanged") {
            if let Ok(inner) = serde_json::from_value::<WindowFocusChangedInner>(wf.clone()) {
                self.state.focused_window_title = inner.window.and_then(|w| w.title);
                return true;
            }
        }

        false
    }

    /// Send a focus-workspace request (fire and forget on a fresh socket).
    pub fn focus_workspace(&self, id: u64) {
        if let Ok(mut s) = UnixStream::connect(socket_path().unwrap_or_default()) {
            let msg = format!(
                "{{\"action\":{{\"FocusWorkspace\":{{\"reference\":{{\"Id\":{}}}}}}}}}\n",
                id
            );
            let _ = s.write_all(msg.as_bytes());
        }
    }
}

impl AsFd for NiriConnection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream_for_fd.as_fd()
    }
}
