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
    /// `Some` = terminal has exited (value is the exit code), `None` = not yet started.
    pub exit_code: Option<i32>,
}
