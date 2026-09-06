use std::path::PathBuf;

use super::state::TerminalState;

/// Reference to the terminal pane passed into the tree scroll view for rendering.
pub enum TerminalPaneRef<'a> {
    /// A live PTY is running.
    Live(&'a mut TerminalState),
    /// No live PTY; show a placeholder with session info.
    Placeholder(PlaceholderInfo),
}

/// Display information for the placeholder shown when no live PTY is running.
pub struct PlaceholderInfo {
    pub provider_name: &'static str,
    pub session_id: Option<String>,
    pub directory: Option<PathBuf>,
    pub status: PlaceholderStatus,
}

/// Why the placeholder is showing instead of a live PTY.
pub enum PlaceholderStatus {
    /// Not yet launched.
    NotStarted,
    /// CLI process exited with the given code.
    Exited(i32),
    /// Child process is alive but SIGSTOP'd.
    Suspended,
}
