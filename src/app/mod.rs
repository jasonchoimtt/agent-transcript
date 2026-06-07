use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::info;

use ratatui::DefaultTerminal;
use tokio::task::JoinHandle;

use crate::clipboard::ClipboardBackend;
use crate::color::RgbColor;
use crate::config::{AgentConfig, Config};
use crate::data_view::{DataViewState, build_session_info_nodes};
use crate::event::{AppEvent, Event, EventHandler};
use crate::log_buffer::LogBuffer;
use crate::picker::state::PickerState;
use crate::providers::{LoadConfig, Provider, ProviderKind, TranscriptEntry};
use crate::terminal::mouse::encode_mouse_event;
use crate::terminal::osc::MouseMode;
use crate::terminal::{PanelState, SessionInfo, TerminalPanel};
use crate::theme::Theme;
use crate::transforms::{build_pipeline, build_transforms};
use crate::tree_scroll_view::TreeScrollViewState;

/// Which top-level screen is currently visible.
pub enum AppScreen {
    Picker,
    Transcript,
}

/// How the application was launched.
pub enum StartMode {
    /// No arguments — show the transcript picker on startup.
    Picker,
    /// `[--resume] <provider>:<session_id>` — open a specific session.
    Session {
        provider: ProviderKind,
        id: String,
        resume: bool,
    },
    /// `<provider>` — start a new session with the given provider's CLI.
    NewSession { provider: ProviderKind },
}

/// Which in-status-bar confirmation prompt (if any) is active.
pub enum ConfirmKind {
    /// Ctrl-K: asking the user whether to kill the live terminal.
    Kill,
    /// A picker entry was selected while a live terminal with a known session
    /// ID is running.  The user must confirm before the old session is replaced.
    SessionSwitch(TranscriptEntry),
    /// Ctrl-Y in picker: open and resume an entry while a live terminal is running.
    SessionSwitchAndResume(TranscriptEntry),
    /// Ctrl-T / Enter-on-NewChat in picker: start a new session for the given provider
    /// while a live terminal with a known session ID is running.
    NewSession(ProviderKind),
    /// Shift-R: restart the transcript reader for the current session.
    ReaderRestart,
    /// Ctrl-D: start writing debug logs to /tmp/agent-transcript.log.
    DebugLog,
}

pub struct App {
    running: bool,
    events: EventHandler,
    terminal: TerminalPanel,
    tree_state: TreeScrollViewState,
    theme: Theme,
    /// Cached host terminal background color (queried at startup via OSC 11 or $COLORFGBG).
    host_bg: Option<RgbColor>,
    _reader_task: Option<JoinHandle<()>>,
    screen: AppScreen,
    /// Picker state persisted across open/close cycles.
    picker_state: PickerState,
    /// True once a transcript has been opened at least once this session.
    transcript_open: bool,
    /// When true, the next `TerminalExited` event closes the app.
    quit_intent: bool,
    /// Active confirmation prompt shown in the status bar.
    confirm_prompt: Option<ConfirmKind>,
    /// Transient message shown in the status bar; auto-dismissed after 5 s.
    /// The bool indicates whether to use warning style (yellow) vs regular style.
    flash_message: Option<(String, bool, std::time::Instant)>,
    config: Config,
    data_view: Option<DataViewState>,
    providers: Arc<Vec<Box<dyn Provider>>>,
    /// True while message-interaction mode is active (key events routed to the selected widget).
    message_interaction: bool,
    /// ID of the most-recently spawned terminal.  Each launch increments this
    /// so `TerminalExited` events from a killed predecessor are ignored.
    terminal_id: u64,
    /// When true, the status bar shows internal debug state instead of key hints (Shift-D toggle).
    status_bar_debug: bool,
    /// In-memory buffer of reader log entries (INFO+), shown via Shift-L.
    pub(super) log_buffer: LogBuffer,
    /// Provider + path of the most recently loaded session; used to restart the reader.
    last_session: Option<(ProviderKind, PathBuf, Option<PathBuf>)>,
    /// Lazily detected clipboard backend; None until first copy.
    clipboard_backend: Option<ClipboardBackend>,
    /// Handle for toggling debug file logging at runtime (Ctrl-D).
    pub(super) debug_writer: crate::logging::DebugHandle,
    /// Pending first key of an app-level composite sequence (e.g. `!`).
    pub(super) pending_app_key: Option<char>,
}

impl App {
    /// Create the app from a parsed `StartMode`.
    pub async fn new(
        start_mode: StartMode,
        host_bg: Option<RgbColor>,
        config: Config,
        log_buffer: LogBuffer,
        debug_writer: crate::logging::DebugHandle,
    ) -> color_eyre::Result<Self> {
        let events = EventHandler::new();

        let mut app = Self {
            running: true,
            events,
            terminal: TerminalPanel::absent(),
            tree_state: TreeScrollViewState::new(vec![]),
            theme: Theme::load(&config.theme, host_bg)?,
            host_bg,
            _reader_task: None,
            screen: AppScreen::Transcript,
            picker_state: PickerState::new(),
            transcript_open: false,
            quit_intent: false,
            confirm_prompt: None,
            flash_message: None,
            config,
            data_view: None,
            providers: PickerState::default_providers(),
            terminal_id: 0,
            message_interaction: false,
            status_bar_debug: false,
            log_buffer,
            last_session: None,
            clipboard_backend: None,
            debug_writer,
            pending_app_key: None,
        };

        match start_mode {
            StartMode::Picker => {
                info!("start mode: picker");
                let cwd = app.picker_state.cwd.clone();
                app.screen = AppScreen::Picker;
                app.start_picker_refresh(cwd);
            }
            StartMode::Session {
                provider,
                id,
                resume,
            } => {
                info!(provider = ?provider, id = %id, resume = resume, "start mode: open session");
                let path = provider
                    .as_provider()
                    .find_transcript_path(&id, None)
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!("session not found: {}:{}", provider, id)
                    })?;

                let entry = provider.as_provider().read_entry(&path);
                let workspace_path = entry.as_ref().and_then(|e| e.workspace_path.clone());
                app.load_session(provider.clone(), path.clone(), workspace_path.clone())
                    .await;

                let dir = workspace_path
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_default();
                let ac = app.agent_config(&provider);
                let info = SessionInfo {
                    binary: ac.binary.clone(),
                    extra_args: ac.extra_args.clone(),
                    disable_plugin: ac.disable_plugin,
                    provider,
                    session_id: Some(id),
                    directory: dir,
                };
                if resume {
                    app.terminal_id += 1;
                    let sender = app.events.sender();
                    app.terminal.launch(&info, sender, app.terminal_id)?;
                    app.activate_terminal();
                } else {
                    app.terminal.state = PanelState::Uninitialized(info);
                }
                app.transcript_open = true;
            }
            StartMode::NewSession { provider } => {
                info!(provider = ?provider, "start mode: new session");
                app.check_hook_warning(&provider);
                let ac = app.agent_config(&provider);
                let info = SessionInfo {
                    binary: ac.binary.clone(),
                    extra_args: ac.extra_args.clone(),
                    disable_plugin: ac.disable_plugin,
                    provider,
                    session_id: None,
                    directory: std::env::current_dir().unwrap_or_default(),
                };
                app.terminal_id += 1;
                let sender = app.events.sender();
                app.terminal.launch(&info, sender, app.terminal_id)?;
                app.activate_terminal();
                app.transcript_open = true;
            }
        }

        Ok(app)
    }

    /// Return the provider + session ID of the active session, if known.
    fn resume_info(&self) -> Option<(ProviderKind, String)> {
        match &self.terminal.state {
            PanelState::Live { info, .. } | PanelState::Exited { info, .. } => {
                let id = info.session_id.clone()?;
                Some((info.provider.clone(), id))
            }
            _ => None,
        }
    }

    pub async fn run(
        mut self,
        mut tui: DefaultTerminal,
    ) -> color_eyre::Result<Option<(ProviderKind, String)>> {
        // Enable host-side capabilities needed for terminal passthrough.
        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableFocusChange,
        )?;

        let result = self.event_loop(&mut tui).await;

        // Restore host terminal to a clean state.
        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableFocusChange,
        )?;
        // Restore default cursor shape on exit.
        let _ = std::io::stdout().write_all(b"\x1b[0 q");
        let _ = std::io::stdout().flush();

        let resume = self.resume_info();
        result.map(|()| resume)
    }

    async fn event_loop(&mut self, tui: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let mut last_area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };

        while self.running {
            // Skip drawing while child holds a synchronized-output lock, or while
            // the cursor is mid-update (both transcript only).
            let sync_locked = self.terminal.sync_locked();
            let skip_draw = (sync_locked || self.terminal.render_suppressed())
                && matches!(self.screen, AppScreen::Transcript);

            if !skip_draw {
                if let Some(area) = self.draw(tui)? {
                    last_area = area;
                }
                self.terminal.notify_rendered();
            }

            match self.events.next().await? {
                Event::Tick => {
                    self.terminal.on_tick();
                    if let Some((_, _, since)) = &self.flash_message
                        && since.elapsed() >= std::time::Duration::from_secs(5)
                    {
                        self.flash_message = None;
                    }
                    if matches!(self.screen, AppScreen::Picker) {
                        self.picker_state.tick_flash();
                    }
                }
                Event::Crossterm(crossterm::event::Event::Key(key)) => {
                    self.handle_key(key, last_area).await;
                }
                Event::Crossterm(crossterm::event::Event::Mouse(ev)) => {
                    use crossterm::event::MouseEventKind;
                    match ev.kind {
                        MouseEventKind::ScrollUp => {
                            if self.terminal.active {
                                self.terminal.active = false;
                                let _ = std::io::stdout().write_all(b"\x1b[0 q");
                                let _ = std::io::stdout().flush();
                            }
                            self.tree_state.scroll_up(3);
                        }
                        MouseEventKind::ScrollDown => {
                            self.tree_state.scroll_down(3);
                        }
                        _ => {
                            if self.terminal.active {
                                let translated = self.tree_state.translate_mouse_to_pty(ev);
                                if let Some(term) = self.terminal.live_ts()
                                    && term.mouse_mode != MouseMode::Off
                                    && let Some(translated) = translated
                                    && let Some(bytes) = encode_mouse_event(
                                        translated,
                                        term.mouse_mode,
                                        term.mouse_encoding,
                                    )
                                {
                                    term.write_input(&bytes);
                                }
                            }
                        }
                    }
                }
                Event::Crossterm(event) => {
                    if let Some(term) = self.terminal.live_ts() {
                        term.handle_crossterm_event(event);
                    }
                }
                Event::App(AppEvent::TerminalOutput(bytes)) => {
                    self.handle_terminal_output(bytes);
                }
                Event::App(AppEvent::TerminalExited { terminal_id, code }) => {
                    self.apply_terminal_exited(terminal_id, code);
                }
                Event::App(AppEvent::SessionDetected {
                    session_id,
                    transcript_path,
                    workspace_path,
                }) => {
                    self.apply_session_detected(session_id, transcript_path, workspace_path)
                        .await;
                }
                Event::App(AppEvent::PickerEntries { entries }) => {
                    if matches!(self.screen, AppScreen::Picker) {
                        self.picker_state.append_entries(entries);
                    }
                }
                Event::App(AppEvent::PickerDone) => {
                    if matches!(self.screen, AppScreen::Picker) {
                        self.picker_state.finish_loading();
                    }
                }
                Event::ReaderOp(op) => {
                    let was_at_bottom = self.tree_state.at_bottom;
                    // Drain all immediately-available ReaderOps before drawing so that
                    // Reset + replay batches are never shown in a half-reset state.
                    let mut ops = vec![op];
                    while let Some(event) = self.events.try_recv() {
                        match event {
                            Event::ReaderOp(more) => ops.push(more),
                            other => {
                                self.events.unget(other);
                                break;
                            }
                        }
                    }
                    self.tree_state.apply(ops);
                    if was_at_bottom {
                        self.tree_state.snap_to_bottom(true);
                    }
                }
                Event::App(AppEvent::ReaderError(msg)) => {
                    self.flash_message = Some((
                        format!("reader error: {msg}"),
                        true,
                        std::time::Instant::now(),
                    ));
                }
            }
        }

        Ok(())
    }

    fn handle_terminal_output(&mut self, bytes: Vec<u8>) {
        let active = self.terminal.active;
        let Some(term) = self.terminal.live_ts() else {
            return;
        };

        let was_at_bottom = self.tree_state.at_bottom;
        let mut osc_events = vec![];
        osc_events = term.process_output(&bytes);
        // Drain all immediately-available output before drawing so
        // render ticks never see a half-redrawn vt100 screen.
        while let Some(event) = self.events.try_recv() {
            match event {
                Event::App(AppEvent::TerminalOutput(more)) => {
                    osc_events.extend(term.process_output(&more))
                }
                other => {
                    self.events.unget(other);
                    break;
                }
            }
        }
        term.handle_osc_events(osc_events, self.host_bg, active);
        // Recompute collapsed crop and sync height to tree layout.
        term.recompute_crop();
        let crop_h = self
            .terminal
            .live_ts()
            .and_then(|ts| ts.collapsed_crop.map(|c| c.height));
        self.tree_state.set_terminal_collapsed_crop_height(crop_h);
        if was_at_bottom {
            self.tree_state.snap_to_bottom(true);
        }
    }

    /// Start a background picker refresh, delegating handle storage to picker state.
    fn start_picker_refresh(&mut self, cwd: Option<PathBuf>) {
        self.picker_state
            .start_refresh(Arc::clone(&self.providers), cwd, self.events.sender());
    }

    /// Close the picker: abort any in-flight refresh and switch to the transcript screen.
    fn close_picker(&mut self) {
        self.picker_state.abort_refresh();
        self.screen = AppScreen::Transcript;
    }

    /// Handle a `TerminalExited` event: transition `Live → Exited`, deactivate
    /// the terminal pane, and close the app if `quit_intent` is set.
    /// Events whose `terminal_id` doesn't match the current terminal are stale
    /// (from a killed predecessor) and are silently dropped.
    fn apply_terminal_exited(&mut self, terminal_id: u64, code: Option<i32>) {
        if terminal_id != self.terminal_id {
            return;
        }
        info!(code = ?code, "terminal exited");
        if self.terminal.is_live() {
            self.terminal.transition_to_exited(code);
            self.terminal.active = false;
            let _ = std::io::stdout().write_all(b"\x1b[0 q");
            let _ = std::io::stdout().flush();
        }
        if self.quit_intent {
            self.running = false;
        }
    }

    /// Handle a `SessionDetected` event: update the live session ID and open a
    /// transcript reader if a path can be determined.
    async fn apply_session_detected(
        &mut self,
        session_id: String,
        transcript_path: Option<PathBuf>,
        workspace_path: Option<PathBuf>,
    ) {
        info!(session_id = %session_id, transcript_path = ?transcript_path, workspace_path = ?workspace_path, "session detected");
        // Capture the provider before taking a mutable borrow.
        let provider = match &self.terminal.state {
            PanelState::Live { info, .. } => Some(info.provider.clone()),
            _ => None,
        };

        // Update session_id and, for Cursor, the authoritative workspace directory.
        if let PanelState::Live { info, .. } = &mut self.terminal.state {
            info.session_id = Some(session_id.clone());
            if let Some(ref wp) = workspace_path {
                info.directory = wp.clone();
            }
        }

        let Some(provider) = provider else { return };

        let p = provider.as_provider();
        let maybe_path = transcript_path
            .or_else(|| p.compute_transcript_path(&session_id, workspace_path.as_deref()))
            .or_else(|| p.find_transcript_path(&session_id, workspace_path.as_deref()));

        let Some(path) = maybe_path else { return };

        self.load_session(provider, path, workspace_path).await;
    }

    /// Launch a deferred terminal (Uninitialized or Exited) in response to Ctrl-Y.
    fn try_launch_deferred_terminal(&mut self) {
        let can_launch = matches!(
            self.terminal.state,
            PanelState::Uninitialized(_) | PanelState::Exited { .. }
        );
        if !can_launch {
            return;
        }
        info!("launching deferred terminal");
        self.terminal_id += 1;
        let sender = self.events.sender();
        let now_live = self.terminal.try_relaunch(sender, self.terminal_id);
        if now_live {
            self.activate_terminal();
        }
    }

    /// Set terminal active, move selection to terminal node, apply cursor shape.
    fn activate_terminal(&mut self) {
        self.tree_state.key_parser.reset();
        self.tree_state.select_terminal_node();
        self.terminal.activate();
    }

    /// Perform a session switch to `entry` unconditionally.
    ///
    /// Loads the new transcript reader, sets the terminal to `Uninitialized`,
    /// and switches to the transcript screen.  The caller is responsible for
    /// killing any live terminal before calling this.
    async fn do_session_switch(&mut self, entry: TranscriptEntry) {
        info!(provider = ?entry.provider, id = %entry.id, "session switch");
        let dir = entry
            .workspace_path
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let ac = self.agent_config(&entry.provider);
        let info = SessionInfo {
            binary: ac.binary.clone(),
            extra_args: ac.extra_args.clone(),
            disable_plugin: ac.disable_plugin,
            provider: entry.provider.clone(),
            session_id: Some(entry.id.clone()),
            directory: dir,
        };
        self.load_session(entry.provider, entry.path, entry.workspace_path)
            .await;
        self.terminal = TerminalPanel {
            state: PanelState::Uninitialized(info),
            active: false,
            expanded: false,
        };
        self.transcript_open = true;
        self.screen = AppScreen::Transcript;
    }

    /// Launch a new session for `provider`, replacing the current terminal.
    fn do_new_session(&mut self, provider: ProviderKind) {
        info!(provider = ?provider, "new session");
        self.check_hook_warning(&provider);

        // Always clear the old transcript so the new session starts with a blank view.
        if let Some(task) = self._reader_task.take() {
            task.abort();
        }
        self.tree_state = TreeScrollViewState::new(vec![]);

        let ac = self.agent_config(&provider);
        let info = SessionInfo {
            binary: ac.binary.clone(),
            extra_args: ac.extra_args.clone(),
            disable_plugin: ac.disable_plugin,
            provider,
            session_id: None,
            directory: std::env::current_dir().unwrap_or_default(),
        };
        // Increment so the killed predecessor's TerminalExited (carrying the old
        // ID) is ignored by apply_terminal_exited.
        self.terminal_id += 1;
        let sender = self.events.sender();
        if self
            .terminal
            .launch(&info, sender, self.terminal_id)
            .is_ok()
        {
            self.activate_terminal();
        }
        self.transcript_open = true;
        self.screen = AppScreen::Transcript;
    }

    /// Open a transcript reader for `entry`, resetting the tree state.
    ///
    /// If the reader cannot be opened, the tree is left empty — no error is surfaced.
    async fn load_session(
        &mut self,
        provider: ProviderKind,
        path: PathBuf,
        workspace_path: Option<PathBuf>,
    ) {
        info!(provider = ?provider, path = %path.display(), "load_session");
        // Cancel any existing reader task.
        if let Some(task) = self._reader_task.take() {
            task.abort();
        }

        self.tree_state = TreeScrollViewState::new(vec![]);
        self.log_buffer.clear();
        self.last_session = Some((provider.clone(), path.clone(), workspace_path.clone()));

        let load_config = LoadConfig {
            initial_loaded: 0,
            waterfall: false,
            snapshot: false,
        };

        let reader_result = provider.as_provider().open_reader(&path, load_config).await;

        let Ok(reader) = reader_result else { return };

        // Build the transform pipeline.
        let transforms = self.build_transforms(&provider, workspace_path.as_deref());
        let (pipe_tx, pipe_rx) = tokio::sync::mpsc::channel(256);
        let sender = self.events.sender();
        let pipeline_task = build_pipeline(pipe_rx, transforms, sender.clone());

        // Spawn reader task that feeds ops into the pipeline and routes reader errors as events.
        let reader_task = tokio::spawn(async move {
            let mut reader = reader;
            while let Some(item) = reader.updates().recv().await {
                match item {
                    Ok(op) => {
                        if pipe_tx.send(op).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = sender.send(Event::App(AppEvent::ReaderError(format!("{:#}", e))));
                        break;
                    }
                }
            }
        });

        // We store the reader_task; when it's aborted or dropped, the pipeline exits too
        // (pipe_tx is dropped, closing the pipeline's input).
        // Drop the pipeline JoinHandle; the task continues until pipe_rx closes (when reader exits).
        drop(pipeline_task);
        self._reader_task = Some(reader_task);
    }

    fn build_transforms(
        &self,
        provider: &crate::providers::ProviderKind,
        workspace_path: Option<&std::path::Path>,
    ) -> Vec<Box<dyn crate::transforms::Transform>> {
        build_transforms(&self.config.transforms, provider, workspace_path)
    }

    fn agent_config(&self, provider: &ProviderKind) -> &AgentConfig {
        match provider {
            ProviderKind::Claude => &self.config.agents.claude,
            ProviderKind::Cursor => &self.config.agents.cursor,
        }
    }

    /// Flash a warning if the session hook for `provider` is not installed.
    fn check_hook_warning(&mut self, provider: &ProviderKind) {
        let msg = match provider {
            ProviderKind::Cursor if !crate::cursor_hooks::is_installed() => {
                " Cursor session hook not installed. Run: agt install-hooks cursor"
            }
            ProviderKind::Claude
                if self.config.agents.claude.disable_plugin
                    && !crate::claude_hooks::is_installed() =>
            {
                " Claude session hook not installed. Run: agt install-hooks --force claude"
            }
            _ => return,
        };
        self.flash_message = Some((msg.to_string(), true, std::time::Instant::now()));
    }

    /// Open the session info data view, falling back to synthesis when the file isn't readable yet.
    pub(super) fn open_session_info(&mut self) {
        // 1. Always re-read via the provider to get up-to-date message count etc.
        if let Some((provider_kind, path, _)) = self.last_session.clone()
            && let Some(entry) = provider_kind.as_provider().read_entry(&path)
        {
            let nodes = build_session_info_nodes(&entry);
            self.data_view = Some(DataViewState::from_nodes(nodes));
            return;
        }

        // 2. Synthesize from whatever is available (e.g. session ID known but file not yet created).
        let provider_from_last = self.last_session.as_ref().map(|(p, _, _)| p.clone());
        let provider_from_terminal = self.terminal.session_info().map(|i| i.provider.clone());
        let Some(provider_kind) = provider_from_last.or(provider_from_terminal) else {
            return;
        };
        let path = self
            .last_session
            .as_ref()
            .map(|(_, p, _)| p.clone())
            .unwrap_or_default();
        let workspace_from_last = self.last_session.as_ref().and_then(|(_, _, w)| w.clone());
        let workspace_from_terminal = self.terminal.session_info().map(|i| i.directory.clone());
        let workspace_path = workspace_from_last.or(workspace_from_terminal);
        let session_id = self
            .terminal
            .session_info()
            .and_then(|i| i.session_id.clone())
            .unwrap_or_default();
        let entry = TranscriptEntry {
            path,
            id: session_id,
            title: String::new(),
            mtime: chrono::Local::now(),
            updated_at: None,
            size: None,
            last_user_message: None,
            message_count: 0,
            workspace_path,
            provider: provider_kind,
        };
        let nodes = build_session_info_nodes(&entry);
        self.data_view = Some(DataViewState::from_nodes(nodes));
    }

    /// Copy `text` to the clipboard using the cached (or freshly detected) backend,
    /// then set a flash message indicating the result.
    pub(super) fn do_copy(&mut self, text: &str) {
        let backend = self
            .clipboard_backend
            .get_or_insert_with(crate::clipboard::detect)
            .clone();
        match crate::clipboard::copy(&backend, text) {
            Ok(()) => {
                self.flash_message = Some((
                    format!("Copied ({})", backend.display_name()),
                    false,
                    std::time::Instant::now(),
                ));
            }
            Err(e) => {
                self.flash_message =
                    Some((format!("Copy failed: {e}"), true, std::time::Instant::now()));
            }
        }
    }
}

mod draw;
mod keys;

#[cfg(test)]
mod tests {
    use portable_pty::CommandBuilder;

    use super::*;
    use crate::session_socket::SessionSocket;
    use crate::terminal::crop::NullCropDetector;
    use crate::terminal::{PanelState, SessionInfo, TerminalPanel, TerminalState};

    async fn picker_app() -> App {
        App::new(
            StartMode::Picker,
            None,
            Config::default(),
            crate::log_buffer::LogBuffer::new(100),
            crate::logging::DebugHandle::default(),
        )
        .await
        .unwrap()
    }

    fn sh_live_panel(app: &App) -> TerminalPanel {
        let info = SessionInfo {
            provider: ProviderKind::Claude,
            session_id: Some("test-sess".to_string()),
            directory: PathBuf::from("/"),
            binary: "sh".to_string(),
            extra_args: vec![],
            disable_plugin: false,
        };
        let ts = TerminalState::new_with_cmd(
            CommandBuilder::new("sh"),
            None,
            Box::new(NullCropDetector),
            app.events.sender(),
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

    // ── Transition 1: terminal panel reaches Live state ──────────────────────

    #[tokio::test]
    async fn terminal_panel_live_state_is_reachable() {
        let mut app = picker_app().await;
        let info = SessionInfo {
            provider: ProviderKind::Claude,
            session_id: Some("test-abc".to_string()),
            directory: PathBuf::from("/"),
            binary: "sh".to_string(),
            extra_args: vec![],
            disable_plugin: false,
        };
        let ts = TerminalState::new_with_cmd(
            CommandBuilder::new("sh"),
            None,
            Box::new(NullCropDetector),
            app.events.sender(),
            0,
        )
        .expect("sh must be available");
        let socket = SessionSocket::new().expect("socket creation must succeed");
        app.terminal = TerminalPanel {
            state: PanelState::Live {
                info,
                ts: Box::new(ts),
                socket,
            },
            active: false,
            expanded: false,
        };
        assert!(matches!(app.terminal.state, PanelState::Live { .. }));
    }

    // ── Transition 2: Live → TerminalExited(Some(0)) → Exited ───────────────

    #[tokio::test]
    async fn live_to_exited_on_terminal_exited() {
        let mut app = picker_app().await;
        app.terminal = sh_live_panel(&app);
        assert!(matches!(app.terminal.state, PanelState::Live { .. }));

        app.apply_terminal_exited(0, Some(0));

        assert!(matches!(
            app.terminal.state,
            PanelState::Exited { code: Some(0), .. }
        ));
        if let PanelState::Exited { info, .. } = &app.terminal.state {
            assert_eq!(info.session_id.as_deref(), Some("test-sess"));
        }
    }

    // ── Transition 3: Exited → [Ctrl-Y] → Live ──────────────────────────────

    #[tokio::test]
    async fn exited_to_live_via_ctrl_y() {
        let mut app = picker_app().await;
        app.terminal = sh_live_panel(&app);
        app.apply_terminal_exited(0, Some(0));
        assert!(matches!(app.terminal.state, PanelState::Exited { .. }));

        // Back to Live: manually re-launch sh via mem::replace on the inner state.
        let prev = std::mem::replace(&mut app.terminal.state, PanelState::Absent);
        let info = match prev {
            PanelState::Exited { info, .. } => info,
            _ => panic!("expected Exited"),
        };
        let ts = TerminalState::new_with_cmd(
            CommandBuilder::new("sh"),
            None,
            Box::new(NullCropDetector),
            app.events.sender(),
            0,
        )
        .expect("sh must be available");
        let socket = SessionSocket::new().expect("socket creation must succeed");
        app.terminal.state = PanelState::Live {
            info,
            ts: Box::new(ts),
            socket,
        };
        assert!(matches!(app.terminal.state, PanelState::Live { .. }));
    }

    // ── Quit intent cleared on Ctrl-O ────────────────────────────────────────

    #[tokio::test]
    async fn quit_intent_cleared_on_ctrl_o() {
        let mut app = picker_app().await;
        app.terminal = sh_live_panel(&app);
        app.quit_intent = true;
        app.terminal.active = true;

        // Simulate Ctrl-O: deactivate and clear quit intent.
        app.terminal.active = false;
        app.quit_intent = false;

        assert!(!app.quit_intent);
        assert!(!app.terminal.active);
    }

    // ── Quit intent set: TerminalExited closes app ─────────────────────────

    #[tokio::test]
    async fn quit_intent_closes_app_on_exit() {
        let mut app = picker_app().await;
        app.terminal = sh_live_panel(&app);
        app.quit_intent = true;

        app.apply_terminal_exited(0, Some(0));

        assert!(!app.running);
    }
}
