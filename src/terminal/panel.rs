use std::path::PathBuf;

use portable_pty::CommandBuilder;
use tokio::sync::mpsc;

use crate::event::Event;
use crate::plugin_install::extract_plugin;
use crate::providers::ProviderKind;
use crate::session_socket::SessionSocket;
use crate::terminal::pane_ref::{PlaceholderInfo, TerminalPaneRef};
use crate::terminal::state::TerminalState;

/// Identity and working context for a terminal session.
#[derive(Clone)]
pub struct SessionInfo {
    pub provider: ProviderKind,
    /// Known only after a session has been created or explicitly provided via CLI.
    pub session_id: Option<String>,
    /// Working directory for the CLI process and for display in the placeholder.
    pub directory: PathBuf,
    /// Binary to invoke (overrides the provider default).
    pub binary: String,
    /// Extra arguments appended to the CLI invocation.
    pub extra_args: Vec<String>,
    /// When true (Claude only), skip automatic `--plugin-dir` injection and rely
    /// on the hook being installed in `~/.claude/settings.json` instead.
    pub disable_plugin: bool,
}

/// Inner state machine for the terminal panel.
pub enum PanelState {
    /// No terminal configured for this session.
    Absent,
    /// CLI has not been launched yet; Ctrl-Y will start it.
    Uninitialized(SessionInfo),
    /// A live PTY is running.
    Live {
        info: SessionInfo,
        ts: Box<TerminalState>,
        /// Unix socket that receives session IDs from the hook subcommand.
        /// Dropped (and socket file unlinked) when transitioning to `Exited`.
        socket: SessionSocket,
    },
    /// CLI exited; Ctrl-Y will spawn a new instance.
    Exited {
        code: Option<i32>,
        info: SessionInfo,
    },
}

/// Owns the embedded PTY pane and its display state.
pub struct TerminalPanel {
    pub state: PanelState,
    /// Whether the terminal pane has keyboard/mouse focus.
    pub active: bool,
    /// Whether the scrollback region is expanded above the live view.
    pub expanded: bool,
}

impl TerminalPanel {
    pub fn absent() -> Self {
        Self {
            state: PanelState::Absent,
            active: false,
            expanded: false,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self.state, PanelState::Live { .. })
    }

    pub fn sync_locked(&self) -> bool {
        match &self.state {
            PanelState::Live { ts, .. } => ts.sync_locked,
            _ => false,
        }
    }

    pub fn render_suppressed(&self) -> bool {
        match &self.state {
            PanelState::Live { ts, .. } => ts.render_suppressed(),
            _ => false,
        }
    }

    pub fn notify_rendered(&mut self) {
        if let PanelState::Live { ts, .. } = &mut self.state {
            ts.notify_rendered();
        }
    }

    /// Returns `&mut TerminalState` if state is `Live`.
    pub fn live_ts(&mut self) -> Option<&mut TerminalState> {
        match &mut self.state {
            PanelState::Live { ts, .. } => Some(ts),
            _ => None,
        }
    }

    /// Returns `&mut TerminalState` only when both `Live` and `active`.
    pub fn active_live_ts(&mut self) -> Option<&mut TerminalState> {
        if !self.active {
            return None;
        }
        self.live_ts()
    }

    /// Builds a `TerminalPaneRef` for the tree scroll view renderer.
    /// Returns `Placeholder` with empty fields when state is `Absent`.
    pub fn pane_ref(&mut self) -> TerminalPaneRef<'_> {
        match &mut self.state {
            PanelState::Live { ts, .. } => TerminalPaneRef::Live(ts),
            PanelState::Uninitialized(info) => TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: info.provider.display_name(),
                session_id: info.session_id.clone(),
                directory: Some(info.directory.clone()),
                exit_code: None,
            }),
            PanelState::Exited { code, info } => TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: info.provider.display_name(),
                session_id: info.session_id.clone(),
                directory: Some(info.directory.clone()),
                exit_code: Some(code.unwrap_or(-1)),
            }),
            PanelState::Absent => TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: "",
                session_id: None,
                directory: None,
                exit_code: None,
            }),
        }
    }

    /// Spawn the PTY process and return a `Live` `PanelState`.
    fn launch_inner(
        info: &SessionInfo,
        sender: mpsc::UnboundedSender<Event>,
        terminal_id: u64,
    ) -> color_eyre::Result<PanelState> {
        let mut cmd = CommandBuilder::new(&info.binary);
        if let Some(ref session_id) = info.session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
        for arg in &info.extra_args {
            cmd.arg(arg);
        }
        let cwd = Some(info.directory.clone());
        let crop_detector = info.provider.crop_detector();

        // For Claude, either inject the plugin via --plugin-dir (default) or rely
        // on the hook installed in ~/.claude/settings.json (when disable_plugin).
        if info.provider == ProviderKind::Claude
            && !info.disable_plugin
            && let Ok(plugin_dir) = extract_plugin(&info.provider)
        {
            cmd.arg("--plugin-dir");
            cmd.arg(plugin_dir);
        }

        // Create the session socket before spawning the child so AGT_SOCKET is
        // set in the child's environment from the start.
        let mut socket = SessionSocket::new()?;
        cmd.env("AGT_SOCKET", socket.path_str());

        let mut ts =
            TerminalState::new_with_cmd(cmd, cwd, crop_detector, sender.clone(), terminal_id)?;
        ts.crop_min_height = 7;
        socket.spawn_accept_task(sender);

        Ok(PanelState::Live {
            info: info.clone(),
            ts: Box::new(ts),
            socket,
        })
    }

    /// Set state to `Live` by spawning the CLI described by `info`.
    pub fn launch(
        &mut self,
        info: &SessionInfo,
        sender: mpsc::UnboundedSender<Event>,
        terminal_id: u64,
    ) -> color_eyre::Result<()> {
        self.state = Self::launch_inner(info, sender, terminal_id)?;
        Ok(())
    }

    /// `Live → Exited`; no-op for other states. Display fields are preserved.
    pub fn transition_to_exited(&mut self, code: Option<i32>) {
        let prev = std::mem::replace(&mut self.state, PanelState::Absent);
        self.state = match prev {
            PanelState::Live { info, .. } => PanelState::Exited { code, info },
            other => other,
        };
    }

    /// `Uninitialized/Exited → Live`; returns `true` if launch succeeded.
    /// Display fields are preserved.
    pub fn try_relaunch(&mut self, sender: mpsc::UnboundedSender<Event>, terminal_id: u64) -> bool {
        let prev = std::mem::replace(&mut self.state, PanelState::Absent);
        let info = match prev {
            PanelState::Uninitialized(info) => info,
            PanelState::Exited { info, .. } => info,
            other => {
                self.state = other;
                return false;
            }
        };
        match Self::launch_inner(&info, sender, terminal_id) {
            Ok(live) => {
                self.state = live;
                true
            }
            Err(_) => {
                self.state = PanelState::Uninitialized(info);
                false
            }
        }
    }

    /// Returns `&SessionInfo` for whichever non-`Absent` state is active.
    pub fn session_info(&self) -> Option<&SessionInfo> {
        match &self.state {
            PanelState::Live { info, .. } => Some(info),
            PanelState::Exited { info, .. } => Some(info),
            PanelState::Uninitialized(info) => Some(info),
            PanelState::Absent => None,
        }
    }

    /// Formats the session label for the status bar (`<provider>:<short-id>`),
    /// or `None` when no session ID is known.
    pub fn session_label(&self) -> Option<String> {
        let info = self.session_info()?;
        let id = info.session_id.as_ref()?;
        let short = &id[..id.len().min(8)];
        Some(format!(" {}:{} ", info.provider.cli_command(), short))
    }

    pub fn activate(&mut self) {
        if let PanelState::Live { ts: term, .. } = &mut self.state {
            self.active = true;
            term.apply_cursor_shape();
        }
    }

    /// Called on each tick: flushes pending PTY resizes and expires a stale sync lock.
    pub fn on_tick(&mut self) {
        if let PanelState::Live { ts: term, .. } = &mut self.state {
            term.on_tick();
        }
    }
}

#[cfg(test)]
mod tests {
    use portable_pty::CommandBuilder;
    use tokio::sync::mpsc;

    use super::*;
    use crate::providers::ProviderKind;
    use crate::terminal::crop::NullCropDetector;
    use crate::terminal::state::TerminalState;

    fn sh_sender() -> mpsc::UnboundedSender<Event> {
        mpsc::unbounded_channel().0
    }

    fn sh_live_panel() -> TerminalPanel {
        let info = SessionInfo {
            provider: ProviderKind::Claude,
            session_id: Some("test-sess".to_string()),
            directory: std::path::PathBuf::from("/"),
            binary: "sh".to_string(),
            extra_args: vec![],
            disable_plugin: false,
        };
        let ts = TerminalState::new_with_cmd(
            CommandBuilder::new("sh"),
            None,
            Box::new(NullCropDetector),
            sh_sender(),
            0,
        )
        .expect("sh must be available");
        let socket = SessionSocket::new().expect("socket creation must succeed");
        TerminalPanel {
            state: PanelState::Live {
                info,
                ts: Box::new(ts),
                socket,
            },
            active: false,
            expanded: false,
        }
    }

    #[test]
    fn absent_is_not_live() {
        let mut panel = TerminalPanel::absent();
        assert!(!panel.is_live());
        assert!(panel.live_ts().is_none());
    }

    #[test]
    fn transition_to_exited_preserves_display_state() {
        let mut panel = sh_live_panel();
        panel.active = true;
        panel.expanded = true;
        panel.transition_to_exited(Some(0));
        assert!(matches!(
            panel.state,
            PanelState::Exited { code: Some(0), .. }
        ));
        assert!(panel.active);
        assert!(panel.expanded);
    }

    #[test]
    fn transition_to_exited_noop_on_absent() {
        let mut panel = TerminalPanel::absent();
        panel.transition_to_exited(Some(0));
        assert!(matches!(panel.state, PanelState::Absent));
    }

    #[test]
    fn try_relaunch_preserves_display_state() {
        let info = SessionInfo {
            provider: ProviderKind::Claude,
            session_id: None,
            directory: std::path::PathBuf::from("/"),
            binary: "claude".to_string(),
            extra_args: vec![],
            disable_plugin: false,
        };
        let mut panel = TerminalPanel {
            state: PanelState::Exited {
                code: Some(0),
                info,
            },
            active: true,
            expanded: false,
        };
        // try_relaunch will fail (claude CLI not installed), but active must remain true.
        panel.try_relaunch(sh_sender(), 1);
        assert!(panel.active);
    }

    #[test]
    fn active_live_ts_gated_by_active_flag() {
        let mut panel = sh_live_panel();
        panel.active = false;
        assert!(panel.active_live_ts().is_none());
        panel.active = true;
        assert!(panel.active_live_ts().is_some());
    }
}
