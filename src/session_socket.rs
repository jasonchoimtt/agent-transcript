use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::event::{AppEvent, Event};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Message written to the Unix socket by the `hook` subcommand.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct HookMessage {
    pub kind: String,
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub workspace_path: Option<PathBuf>,
}

/// A Unix domain socket scoped to the lifetime of one live CLI process.
///
/// Created in `launch_inner` before the PTY is spawned.  The socket path is
/// set as `AGT_SOCKET` in the child's environment so the hook subcommand can
/// connect to it.  Dropped (and socket file unlinked) when `PanelState::Live`
/// transitions to `Exited`.
pub struct SessionSocket {
    path: PathBuf,
    /// Held until `spawn_accept_task` is called, then moved into the task.
    listener: Option<std::os::unix::net::UnixListener>,
    /// Abort handle for the background accept task.
    abort: Option<tokio::task::AbortHandle>,
}

impl SessionSocket {
    /// Create a new socket at a unique path.
    ///
    /// Uses a process-global counter combined with the PID to avoid collisions
    /// on rapid relaunch.  Does not require a Tokio runtime.
    pub fn new() -> color_eyre::Result<Self> {
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/agt-{pid}-{n}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        Ok(Self {
            path,
            listener: Some(listener),
            abort: None,
        })
    }

    /// Returns the socket path as a `&str` for setting in child environment.
    pub fn path_str(&self) -> &str {
        self.path.to_str().unwrap_or("")
    }

    /// Move the listener into an async accept task.
    ///
    /// Each accepted connection is expected to send one JSON line matching
    /// [`HookMessage`].  On success, `AppEvent::SessionDetected` is emitted.
    /// The task is stopped by `Drop` calling `abort()` on the handle.
    ///
    /// No-op when called outside a Tokio runtime context (e.g. in unit tests).
    pub fn spawn_accept_task(&mut self, sender: mpsc::UnboundedSender<Event>) {
        let Some(std_listener) = self.listener.take() else {
            return;
        };
        if std_listener.set_nonblocking(true).is_err() {
            return;
        }
        // Require a running Tokio runtime; gracefully skip if none is present.
        let Ok(rt_handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(listener) = tokio::net::UnixListener::from_std(std_listener) else {
            return;
        };
        let handle = rt_handle.spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    continue;
                }
                let trimmed = line.trim();
                let Ok(msg) = serde_json::from_str::<HookMessage>(trimmed) else {
                    continue;
                };
                let _ = sender.send(Event::App(AppEvent::SessionDetected {
                    session_id: msg.session_id,
                    transcript_path: msg.transcript_path,
                    workspace_path: msg.workspace_path,
                }));
            }
        });
        self.abort = Some(handle.abort_handle());
    }
}

impl Drop for SessionSocket {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_socket_file() {
        let socket = SessionSocket::new().expect("socket creation must succeed");
        assert!(socket.path.exists(), "socket file should exist after new()");
    }

    #[test]
    fn drop_removes_socket_file() {
        let socket = SessionSocket::new().expect("socket creation must succeed");
        let path = socket.path.clone();
        drop(socket);
        assert!(!path.exists(), "socket file should be removed after drop()");
    }

    #[test]
    fn two_sockets_get_different_paths() {
        let a = SessionSocket::new().expect("first socket");
        let b = SessionSocket::new().expect("second socket");
        assert_ne!(a.path, b.path);
    }
}
