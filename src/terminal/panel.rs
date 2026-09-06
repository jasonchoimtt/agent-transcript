use std::path::PathBuf;

use portable_pty::CommandBuilder;
use tokio::sync::mpsc;

use crate::event::Event;
use crate::plugin_install::extract_plugin;
use crate::providers::ProviderKind;
use crate::session_socket::{ExpectedCli, SessionSocket};
use crate::terminal::pane_ref::{PlaceholderInfo, PlaceholderStatus, TerminalPaneRef};
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
    /// Child process is SIGSTOP'd; `ts`/`socket` are kept alive (not torn
    /// down) so scrollback and the PTY handles survive the round-trip.
    /// Ctrl-Y sends SIGCONT to the same child and returns to `Live`.
    Suspended {
        info: SessionInfo,
        ts: Box<TerminalState>,
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
    /// Whether the scrollback region is expanded above the live view.
    pub expanded: bool,
}

impl TerminalPanel {
    pub fn absent() -> Self {
        Self {
            state: PanelState::Absent,
            expanded: false,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self.state, PanelState::Live { .. })
    }

    pub fn is_suspended(&self) -> bool {
        matches!(self.state, PanelState::Suspended { .. })
    }

    /// True when a child process exists, whether running (`Live`) or
    /// stopped (`Suspended`). Use this instead of `is_live()` for
    /// process-lifecycle decisions (kill-before-switch, quit cleanup) so a
    /// suspended child isn't silently leaked; use `is_live()` for
    /// I/O-forwarding and rendering decisions, where `Suspended` should
    /// behave like a placeholder rather than a live pane.
    pub fn has_child(&self) -> bool {
        matches!(
            self.state,
            PanelState::Live { .. } | PanelState::Suspended { .. }
        )
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

    /// Returns `&mut TerminalState` if a child process exists, whether
    /// `Live` or `Suspended`. See `has_child()` for when to prefer this
    /// over `live_ts()`.
    pub fn running_ts(&mut self) -> Option<&mut TerminalState> {
        match &mut self.state {
            PanelState::Live { ts, .. } | PanelState::Suspended { ts, .. } => Some(ts),
            _ => None,
        }
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
                status: PlaceholderStatus::NotStarted,
            }),
            PanelState::Suspended { info, .. } => TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: info.provider.display_name(),
                session_id: info.session_id.clone(),
                directory: Some(info.directory.clone()),
                status: PlaceholderStatus::Suspended,
            }),
            PanelState::Exited { code, info } => TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: info.provider.display_name(),
                session_id: info.session_id.clone(),
                directory: Some(info.directory.clone()),
                status: PlaceholderStatus::Exited(code.unwrap_or(-1)),
            }),
            PanelState::Absent => TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: "",
                session_id: None,
                directory: None,
                status: PlaceholderStatus::NotStarted,
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
        // Gate the socket to hook messages whose invoking CLI process is this
        // directly-spawned child, so a nested sub-agent (e.g. `claude -p`, which
        // inherits AGT_SOCKET and fires its own SessionStart hook) can't hijack
        // the followed session.
        let binary_name = std::path::Path::new(&info.binary)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| info.binary.clone());
        let expected_cli = ts.child_pid().map(|pid| ExpectedCli {
            pid: pid as u32,
            binary: binary_name,
        });
        socket.spawn_accept_task(sender, expected_cli);

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

    /// `Live/Suspended → Exited`; no-op for other states. Display fields are preserved.
    pub fn transition_to_exited(&mut self, code: Option<i32>) {
        let prev = std::mem::replace(&mut self.state, PanelState::Absent);
        self.state = match prev {
            PanelState::Live { info, .. } | PanelState::Suspended { info, .. } => {
                PanelState::Exited { code, info }
            }
            other => other,
        };
    }

    /// `Live → Suspended`: sends SIGSTOP to the child and keeps its `ts`/`socket` alive.
    /// No-op for other states.
    pub fn suspend(&mut self) {
        let prev = std::mem::replace(&mut self.state, PanelState::Absent);
        self.state = match prev {
            PanelState::Live { info, ts, socket } => {
                ts.suspend();
                PanelState::Suspended { info, ts, socket }
            }
            other => other,
        };
    }

    /// `Suspended → Live`: sends SIGCONT to the same child (no respawn).
    /// No-op for other states.
    pub fn resume_suspended(&mut self) {
        let prev = std::mem::replace(&mut self.state, PanelState::Absent);
        self.state = match prev {
            PanelState::Suspended {
                info,
                mut ts,
                socket,
            } => {
                ts.resume();
                PanelState::Live { info, ts, socket }
            }
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
            PanelState::Suspended { info, .. } => Some(info),
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

    /// Apply the PTY cursor shape to the host terminal (call after entering Terminal mode).
    pub fn apply_cursor_shape(&mut self) {
        if let PanelState::Live { ts: term, .. } = &mut self.state {
            term.apply_cursor_shape();
        }
    }

    /// Notify the PTY that the terminal pane gained or lost keyboard focus.
    pub fn set_active(&mut self, active: bool) {
        if let PanelState::Live { ts: term, .. } = &mut self.state {
            term.set_active(active);
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

    /// Wraps a `TerminalPanel` spawned with a real child process. Captures the
    /// child's PID up front and force-resumes + kills it on drop, so a test
    /// that suspends the child, panics on an assertion, or calls
    /// `transition_to_exited()` (which drops the `TerminalState` without ever
    /// signalling the child) can never leak a running or permanently-stopped
    /// orphan process.
    struct GuardedPanel {
        panel: TerminalPanel,
        child_pid: libc::pid_t,
    }

    impl std::ops::Deref for GuardedPanel {
        type Target = TerminalPanel;
        fn deref(&self) -> &TerminalPanel {
            &self.panel
        }
    }

    impl std::ops::DerefMut for GuardedPanel {
        fn deref_mut(&mut self) -> &mut TerminalPanel {
            &mut self.panel
        }
    }

    impl Drop for GuardedPanel {
        fn drop(&mut self) {
            unsafe {
                // SIGCONT first: a stopped process still dies from SIGKILL, but
                // continuing it too means it isn't left in a confusing stopped
                // state if something inspects it before the kill is processed.
                libc::kill(self.child_pid, libc::SIGCONT);
                libc::kill(self.child_pid, libc::SIGKILL);
            }
        }
    }

    fn guard(panel: TerminalPanel) -> GuardedPanel {
        let child_pid = match &panel.state {
            PanelState::Live { ts, .. } | PanelState::Suspended { ts, .. } => {
                ts.child_pid().expect("spawned child must have a pid")
            }
            _ => panic!("guard() requires a panel with a live child"),
        };
        GuardedPanel { panel, child_pid }
    }

    fn sh_live_panel() -> GuardedPanel {
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
        guard(TerminalPanel {
            state: PanelState::Live {
                info,
                ts: Box::new(ts),
                socket,
            },
            expanded: false,
        })
    }

    /// Like `sh_live_panel`, but spawns a plain `sleep` instead of a shell,
    /// to verify actual OS-level signal delivery without a shell's own job
    /// control getting in the way.
    fn sleep_live_panel() -> GuardedPanel {
        let info = SessionInfo {
            provider: ProviderKind::Claude,
            session_id: Some("test-sess".to_string()),
            directory: std::path::PathBuf::from("/"),
            binary: "sleep".to_string(),
            extra_args: vec![],
            disable_plugin: false,
        };
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("100");
        let ts = TerminalState::new_with_cmd(cmd, None, Box::new(NullCropDetector), sh_sender(), 0)
            .expect("sleep must be available");
        let socket = SessionSocket::new().expect("socket creation must succeed");
        guard(TerminalPanel {
            state: PanelState::Live {
                info,
                ts: Box::new(ts),
                socket,
            },
            expanded: false,
        })
    }

    /// Poll `/proc/<pid>/stat` for the given state char (e.g. `T` = stopped,
    /// `S`/`R` = running/sleeping), up to a short timeout.
    fn wait_for_proc_state(pid: libc::pid_t, want: char) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false; // process gone
            };
            // Format: "pid (comm) state ...". comm may itself contain ')', so
            // split on the last ')' rather than the first.
            if let Some((_, rest)) = stat.rsplit_once(')')
                && let Some(state_char) = rest.trim_start().chars().next()
                && state_char == want
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn absent_is_not_live() {
        let mut panel = TerminalPanel::absent();
        assert!(!panel.is_live());
        assert!(panel.live_ts().is_none());
    }

    #[test]
    fn absent_has_no_child() {
        let panel = TerminalPanel::absent();
        assert!(!panel.has_child());
        assert!(!panel.is_suspended());
    }

    #[test]
    fn suspend_transitions_live_to_suspended_and_preserves_display_state() {
        let mut panel = sh_live_panel();
        panel.expanded = true;
        panel.suspend();
        assert!(matches!(panel.state, PanelState::Suspended { .. }));
        assert!(panel.is_suspended());
        assert!(panel.has_child());
        assert!(!panel.is_live());
        assert!(panel.expanded);
    }

    // These two tests verify actual OS-level stop/continue via /proc, not just
    // our state-machine transition.
    #[test]
    fn suspend_actually_stops_the_child_process() {
        let mut panel = sleep_live_panel();
        let pid = panel.running_ts().unwrap().child_pid().unwrap();
        panel.suspend();
        assert!(
            wait_for_proc_state(pid, 'T'),
            "child did not reach stopped state after suspend()"
        );
        // GuardedPanel's drop forces resume + kill, so no manual cleanup needed here.
    }

    #[test]
    fn resume_suspended_transitions_back_to_live_without_respawning() {
        let mut panel = sh_live_panel();
        let pid_before = panel.running_ts().unwrap().child_pid();
        panel.suspend();
        panel.resume_suspended();
        assert!(matches!(panel.state, PanelState::Live { .. }));
        assert!(panel.is_live());
        assert!(!panel.is_suspended());
        let pid_after = panel.running_ts().unwrap().child_pid();
        assert_eq!(pid_before, pid_after, "resume must not spawn a new process");
    }

    #[test]
    fn resume_actually_continues_the_child_process() {
        let mut panel = sleep_live_panel();
        let pid = panel.running_ts().unwrap().child_pid().unwrap();
        panel.suspend();
        assert!(wait_for_proc_state(pid, 'T'));
        panel.resume_suspended();
        assert!(
            wait_for_proc_state(pid, 'S') || wait_for_proc_state(pid, 'R'),
            "child did not resume running after resume_suspended()"
        );
    }

    #[test]
    fn suspend_and_resume_are_noop_on_other_states() {
        let mut panel = TerminalPanel::absent();
        panel.suspend();
        assert!(matches!(panel.state, PanelState::Absent));
        panel.resume_suspended();
        assert!(matches!(panel.state, PanelState::Absent));
    }

    #[test]
    fn transition_to_exited_from_suspended() {
        let mut panel = sh_live_panel();
        panel.suspend();
        panel.transition_to_exited(Some(137));
        assert!(matches!(
            panel.state,
            PanelState::Exited {
                code: Some(137),
                ..
            }
        ));
    }

    #[test]
    fn transition_to_exited_preserves_display_state() {
        let mut panel = sh_live_panel();
        panel.expanded = true;
        panel.transition_to_exited(Some(0));
        assert!(matches!(
            panel.state,
            PanelState::Exited { code: Some(0), .. }
        ));
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
            expanded: false,
        };
        // try_relaunch will fail (claude CLI not installed), but expanded must remain false.
        panel.try_relaunch(sh_sender(), 1);
        assert!(!panel.expanded);
    }
}
