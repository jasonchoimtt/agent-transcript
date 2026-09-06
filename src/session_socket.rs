use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::event::{AppEvent, Event};

/// PID of the process that connected to `stream`, verified by the kernel via
/// `SO_PEERCRED` (Linux only). Returns `None` on other platforms or if the
/// lookup fails, so callers must treat that as "unknown", not "mismatch".
#[cfg(target_os = "linux")]
fn peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (ret == 0).then_some(cred.pid as u32)
}

#[cfg(not(target_os = "linux"))]
fn peer_pid(_stream: &tokio::net::UnixStream) -> Option<u32> {
    None
}

/// Parent PID of `pid`, read from procfs (Linux only).
#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:")?.trim().parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn parent_pid(_pid: u32) -> Option<u32> {
    None
}

/// Basename of argv[0] for `pid`, read from procfs (Linux only).
#[cfg(target_os = "linux")]
fn cmdline_basename(pid: u32) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv0 = cmdline.split(|&b| b == 0).find(|s| !s.is_empty())?;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(argv0));
    Some(path.file_name()?.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn cmdline_basename(_pid: u32) -> Option<String> {
    None
}

/// Safety bound on how far up the process tree to climb looking for a
/// same-named ancestor -- not a tuned assumption about wrapper depth (see
/// `find_cli_ancestor`), just a guard against unbounded procfs walking.
const MAX_ANCESTOR_HOPS: usize = 8;

/// Walk up from `pid`'s parent looking for the nearest ancestor whose command
/// basename is `binary_name`, skipping over any wrapper processes in between
/// (e.g. the shell Claude Code runs hook commands through). Depth-agnostic by
/// design: it stops at the first process that *is* the CLI, whatever variety
/// or number of wrappers sit between it and `pid`, rather than assuming a
/// fixed number of hops.
fn find_cli_ancestor(pid: u32, binary_name: &str) -> Option<u32> {
    let mut current = pid;
    for _ in 0..MAX_ANCESTOR_HOPS {
        let parent = parent_pid(current)?;
        if cmdline_basename(parent).as_deref() == Some(binary_name) {
            return Some(parent);
        }
        current = parent;
    }
    None
}

/// Identity of the CLI process a `SessionSocket` was handed to at launch.
pub struct ExpectedCli {
    pub pid: u32,
    pub binary: String,
}

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
    /// `expected` identifies the CLI process this socket was handed to (via
    /// `AGT_SOCKET`) at launch. `AGT_SOCKET` is inherited by every descendant
    /// of that process, so a nested sub-agent invocation (e.g. `claude -p`
    /// spawned as a tool call) shares the same socket and fires its own
    /// `SessionStart` hook. Connections are verified via `SO_PEERCRED`: the
    /// connecting hook process's ancestry is walked (skipping wrapper
    /// processes such as the shell Claude Code runs hook commands through)
    /// until the nearest ancestor matching `expected.binary`'s name is found.
    /// If that ancestor's PID doesn't match `expected.pid`, the message is
    /// dropped instead of hijacking the followed transcript -- this is what
    /// happens for a sub-agent, since it runs the same binary as a different
    /// process. `/clear`/`/resume`/compaction inside the tracked CLI (same
    /// PID throughout its life) still switches as expected. When the check is
    /// inconclusive (non-Linux, or no matching ancestor found), the message
    /// is accepted rather than dropped.
    ///
    /// No-op when called outside a Tokio runtime context (e.g. in unit tests).
    pub fn spawn_accept_task(
        &mut self,
        sender: mpsc::UnboundedSender<Event>,
        expected: Option<ExpectedCli>,
    ) {
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
                if let Some(expected) = &expected
                    && let Some(hook_pid) = peer_pid(&stream)
                    && let Some(ancestor_pid) = find_cli_ancestor(hook_pid, &expected.binary)
                    && ancestor_pid != expected.pid
                {
                    tracing::debug!(
                        hook_pid,
                        ancestor_pid,
                        expected_pid = expected.pid,
                        "ignoring session-start from a non-tracked process"
                    );
                    continue;
                }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn parent_pid_resolves_real_relationship() {
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("failed to spawn sleep");
        assert_eq!(parent_pid(child.id()), Some(std::process::id()));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cmdline_basename_resolves_known_binary() {
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("failed to spawn sleep");
        // A freshly-forked process's /proc/{pid}/cmdline can briefly read as
        // empty on some sandboxed/virtualized /proc implementations before
        // execve() lands; retry rather than assume it's immediately visible.
        // This is a test-harness-only concern -- production code only ever
        // inspects a long-running CLI process's cmdline, well past this window.
        let mut basename = None;
        for _ in 0..50 {
            basename = cmdline_basename(child.id());
            if basename.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(basename.as_deref(), Some("sleep"));
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Spawns `sh -c "sleep 2 & echo $! >pidfile; wait"` and returns the
    /// backgrounded `sleep`'s PID once it's been written to `pidfile` -- a
    /// grandchild of this test process via an intermediate shell, mirroring
    /// the real `claude -> sh -c "agt hook" -> agt` chain.
    fn spawn_via_shell_wrapper() -> (std::process::Child, u32, std::path::PathBuf) {
        let pidfile = std::env::temp_dir().join(format!(
            "agt-test-pidfile-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let sh = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 2 & echo $! > {}; wait", pidfile.display()))
            .spawn()
            .expect("failed to spawn sh");

        let mut grandchild_pid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = s.trim().parse()
            {
                grandchild_pid = Some(pid);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let grandchild_pid = grandchild_pid.expect("background job never wrote its pid");
        (sh, grandchild_pid, pidfile)
    }

    /// Mirrors the real `claude -> sh -c "agt hook" -> agt` chain: walking up
    /// from the grandchild must skip past the shell wrapper (whose basename
    /// doesn't match) and land on this test process, however many hops away
    /// -- not a hardcoded depth.
    #[cfg(target_os = "linux")]
    #[test]
    fn find_cli_ancestor_walks_up_through_a_shell_hop() {
        let self_pid = std::process::id();
        let self_basename = cmdline_basename(self_pid).expect("self basename must resolve");
        let (mut sh, grandchild_pid, pidfile) = spawn_via_shell_wrapper();

        assert_eq!(
            find_cli_ancestor(grandchild_pid, &self_basename),
            Some(self_pid)
        );

        let _ = sh.kill();
        let _ = sh.wait();
        let _ = std::fs::remove_file(&pidfile);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_cli_ancestor_returns_none_when_binary_never_appears() {
        assert_eq!(
            find_cli_ancestor(std::process::id(), "no-such-binary-agt-test"),
            None
        );
    }

    async fn send_hook_line(path: &std::path::Path, session_id: &str) {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::UnixStream::connect(path)
            .await
            .expect("connect must succeed");
        let msg = HookMessage {
            kind: "session-start".to_string(),
            session_id: session_id.to_string(),
            transcript_path: None,
            workspace_path: None,
        };
        let mut json = serde_json::to_string(&msg).unwrap();
        json.push('\n');
        stream.write_all(json.as_bytes()).await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn accept_task_drops_session_start_when_ancestor_pid_differs() {
        let mut socket = SessionSocket::new().expect("socket creation must succeed");
        let path = socket.path.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // A direct connection from this test process finds its real parent
        // (whatever binary that is) as the nearest matching-name ancestor,
        // but we claim a *different* PID for that same binary name -- the
        // nested-sub-agent shape: same CLI binary, different process.
        let real_parent = parent_pid(std::process::id()).expect("test process must have a parent");
        let binary = cmdline_basename(real_parent).expect("parent basename must resolve");
        socket.spawn_accept_task(
            tx,
            Some(ExpectedCli {
                pid: std::process::id(),
                binary,
            }),
        );

        send_hook_line(&path, "sub-agent-session").await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        assert!(
            result.is_err(),
            "message from a mismatched process must be dropped"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn accept_task_accepts_session_start_when_ancestor_matches() {
        let mut socket = SessionSocket::new().expect("socket creation must succeed");
        let path = socket.path.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Pretend the tracked CLI is our own real parent process, by both PID
        // and binary name -- simulating a real /clear or /resume inside the
        // tracked CLI, which keeps the same PID and binary throughout its life.
        let real_parent = parent_pid(std::process::id()).expect("test process must have a parent");
        let binary = cmdline_basename(real_parent).expect("parent basename must resolve");
        socket.spawn_accept_task(
            tx,
            Some(ExpectedCli {
                pid: real_parent,
                binary,
            }),
        );

        send_hook_line(&path, "real-session").await;

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("message from tracked process must be accepted")
            .expect("channel should not be closed");
        match event {
            Event::App(AppEvent::SessionDetected { session_id, .. }) => {
                assert_eq!(session_id, "real-session");
            }
            _ => panic!("expected SessionDetected"),
        }
    }
}
