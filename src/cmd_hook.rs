use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::session_socket::HookMessage;

/// Payload received on stdin from the agent CLI `SessionStart` hook.
#[derive(serde::Deserialize)]
struct HookPayload {
    session_id: String,
    transcript_path: Option<PathBuf>,
    workspace_roots: Option<Vec<PathBuf>>,
}

/// Run the `hook` subcommand synchronously (no Tokio runtime required).
///
/// Reads the hook payload from stdin, connects to `AGT_SOCKET`, and writes
/// the session info as a [`HookMessage`] JSON line.  Returns immediately if
/// `AGT_SOCKET` is unset so the hook is safe to install globally.
pub fn run() -> color_eyre::Result<()> {
    let socket_path = match std::env::var("AGT_SOCKET") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };

    let mut stdin_data = String::new();
    std::io::stdin().read_to_string(&mut stdin_data)?;

    let payload: HookPayload = serde_json::from_str(&stdin_data)
        .map_err(|e| color_eyre::eyre::eyre!("failed to parse hook payload: {e}"))?;

    let msg = HookMessage {
        kind: "session-start".to_string(),
        session_id: payload.session_id,
        transcript_path: payload.transcript_path,
        workspace_path: payload.workspace_roots.and_then(|r| r.into_iter().next()),
    };

    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|e| color_eyre::eyre::eyre!("connect to {socket_path}: {e}"))?;

    let mut json = serde_json::to_string(&msg)?;
    json.push('\n');
    stream.write_all(json.as_bytes())?;

    Ok(())
}
