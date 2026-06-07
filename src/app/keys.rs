use std::io::Write;

use ratatui::layout::Rect;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::clipboard::markdown_to_plain;
use crate::data_view::{
    DataViewAction, DataViewState, build_key_shortcuts_nodes, build_reader_log_nodes,
    handle_key_event_dv,
};
use crate::picker::handler::PickerAction;
use crate::providers::ProviderKind;
use crate::terminal::PanelState;
use crate::terminal::keys::key_event_to_bytes;
use crate::tree_scroll_view::{ComponentKeyResult, TreeAction};

use crate::app::{App, AppMode, AppScreen, ConfirmKind};

impl App {
    pub(super) async fn handle_key(&mut self, key: KeyEvent, last_area: Rect) {
        match self.screen {
            AppScreen::Picker => {
                self.handle_picker_screen_key(key).await;
            }
            AppScreen::Transcript => {
                self.handle_transcript_screen_key(key, last_area).await;
            }
        }
    }

    async fn handle_picker_screen_key(&mut self, key: KeyEvent) {
        if !matches!(self.screen, AppScreen::Picker) {
            return;
        }
        let action = self.picker_state.handle_key(key);

        match action {
            PickerAction::None => {}
            PickerAction::ToggleShowAll => {
                let needs_reload = self.picker_state.toggle_show_all();
                if needs_reload {
                    self.start_picker_refresh(None);
                }
            }
            PickerAction::Quit => {
                if self.transcript_open {
                    self.close_picker();
                } else {
                    self.running = false;
                }
            }
            PickerAction::Select(entry) => {
                let needs_confirm = matches!(
                    &self.terminal.state,
                    PanelState::Live { info, .. }
                        if info.session_id.is_some()
                );
                self.close_picker();
                if needs_confirm {
                    self.mode = AppMode::Confirm(ConfirmKind::SessionSwitch(entry));
                } else {
                    self.do_session_switch(entry).await;
                }
            }
            PickerAction::OpenAndResume(entry) => {
                let needs_confirm = matches!(
                    &self.terminal.state,
                    PanelState::Live { info, .. }
                        if info.session_id.is_some()
                );
                self.close_picker();
                if needs_confirm {
                    self.mode = AppMode::Confirm(ConfirmKind::SessionSwitchAndResume(entry));
                } else {
                    self.do_session_switch(entry).await;
                    self.try_launch_deferred_terminal();
                    self.activate_terminal();
                }
            }
            PickerAction::NewSession(provider) => {
                let needs_confirm = matches!(
                    &self.terminal.state,
                    PanelState::Live { info, .. }
                        if info.session_id.is_some()
                );
                self.close_picker();
                if needs_confirm {
                    self.mode = AppMode::Confirm(ConfirmKind::NewSession(provider));
                } else {
                    self.do_new_session(provider);
                }
            }
        }
    }

    async fn handle_transcript_screen_key(&mut self, key: KeyEvent, last_area: Rect) {
        if matches!(self.mode, AppMode::Confirm(_)) {
            self.handle_confirm_key(key).await;
        } else if self.data_view.is_some() {
            self.handle_data_view_key(key, last_area).await;
        } else if self.mode == AppMode::MessageInteraction {
            self.handle_message_interaction_key(key, last_area).await;
        } else if self.mode == AppMode::Terminal {
            self.handle_terminal_key(key).await;
        } else {
            self.handle_normal_key(key, last_area).await;
        }
    }

    // ── Transcript screen handlers ──────────

    /// Dispatch a key press to the active confirmation prompt.
    async fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let AppMode::Confirm(kind) = std::mem::replace(&mut self.mode, AppMode::Normal)
                else {
                    return;
                };
                match kind {
                    ConfirmKind::Kill => {
                        if let Some(ts) = self.terminal.live_ts() {
                            ts.kill();
                        }
                    }
                    ConfirmKind::SessionSwitch(entry) => {
                        if let Some(ts) = self.terminal.live_ts() {
                            ts.kill();
                        }
                        self.do_session_switch(entry).await;
                    }
                    ConfirmKind::SessionSwitchAndResume(entry) => {
                        if let Some(ts) = self.terminal.live_ts() {
                            ts.kill();
                        }
                        self.do_session_switch(entry).await;
                        self.try_launch_deferred_terminal();
                        self.activate_terminal();
                    }
                    ConfirmKind::NewSession(provider) => {
                        if let Some(ts) = self.terminal.live_ts() {
                            ts.kill();
                        }
                        self.do_new_session(provider);
                    }
                    ConfirmKind::ReaderRestart => {
                        if let Some((provider, path, workspace_path)) = self.last_session.clone() {
                            self.load_session(provider, path, workspace_path).await;
                        }
                    }
                    ConfirmKind::DebugLog => match self.debug_writer.enable() {
                        Ok(()) => {
                            self.flash_message = Some((
                                format!("Writing logs to {}", crate::logging::LOG_PATH),
                                false,
                                std::time::Instant::now(),
                            ));
                        }
                        Err(e) => {
                            self.flash_message = Some((
                                format!("Failed to open log file: {e}"),
                                true,
                                std::time::Instant::now(),
                            ));
                        }
                    },
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    async fn handle_data_view_key(&mut self, key: KeyEvent, last_area: Rect) {
        let Some(ref mut dv) = self.data_view else {
            return;
        };
        // Data view popup: capture all keys.
        match handle_key_event_dv(dv, key, last_area.height.saturating_sub(1)) {
            DataViewAction::Close => self.data_view = None,
            DataViewAction::ToggleDisplay => {
                dv.toggle_display();
            }
            DataViewAction::Tree(action) => match action {
                TreeAction::CopyMarkdown => {
                    let text = dv.tree.selected_text().to_owned();
                    self.do_copy(&text);
                }
                TreeAction::CopyPlainText => {
                    let md = dv.tree.selected_text().to_owned();
                    let plain = markdown_to_plain(&md);
                    self.do_copy(&plain);
                }
                TreeAction::CopyRawData => {
                    let data = dv.tree.selected_data().to_owned();
                    self.do_copy(&data);
                }
                _ => {
                    dv.tree.apply_action(action);
                }
            },
        }
    }

    async fn handle_message_interaction_key(&mut self, key: KeyEvent, last_area: Rect) {
        if self.tree_state.is_interaction_supported() {
            match self.tree_state.apply_component_key(key) {
                ComponentKeyResult::ExitInteraction => {
                    self.mode = AppMode::Normal;
                    self.tree_state.precedence =
                        crate::tree_scroll_view::state::Precedence::Selection;
                }
                ComponentKeyResult::Passthrough => {
                    let action = self
                        .tree_state
                        .handle_key(key, last_area.height.saturating_sub(1));
                    self.tree_state.apply_action(action);
                }
                ComponentKeyResult::Copy { content } => {
                    self.do_copy(&content);
                }
                _ => {}
            }
        }
    }

    async fn handle_terminal_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('o')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Ctrl-O: deactivate terminal, also clear quit intent.
            self.mode = AppMode::Normal;
            self.quit_intent = false;
            let _ = std::io::stdout().write_all(b"\x1b[0 q");
            let _ = std::io::stdout().flush();
        } else if let Some(term) = self.terminal.live_ts() {
            let app_cursor = term.parser.screen().application_cursor();
            let app_keypad = term.parser.screen().application_keypad();
            if let Some(bytes) = key_event_to_bytes(&key, app_cursor, app_keypad) {
                term.write_input(&bytes);
            }
        }
    }

    async fn handle_normal_key(&mut self, key: KeyEvent, last_area: Rect) {
        // If a multi-key prefix is pending (e.g. `y` waiting for `yr`/`yt`/`yy`),
        // forward directly to the tree handler so single-key shortcuts below can't
        // intercept the second key.
        if self.tree_state.key_parser.pending_char().is_some() {
            let action = self
                .tree_state
                .handle_key(key, last_area.height.saturating_sub(1));
            self.apply_tree_action(action);
            return;
        }

        // Resolve app-level composite keys (prefix `!`).
        if self.pending_app_key == Some('!') {
            self.pending_app_key = None;
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('l') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // !l: open reader log view.
                        self.data_view = Some(DataViewState::from_nodes(build_reader_log_nodes(
                            &self.log_buffer,
                        )));
                    }
                    KeyCode::Char('L') => {
                        // !L: enable debug logging to file.
                        if self.debug_writer.is_enabled() {
                            self.flash_message = Some((
                                format!("Already writing logs to {}", crate::logging::LOG_PATH),
                                false,
                                std::time::Instant::now(),
                            ));
                        } else {
                            self.mode = AppMode::Confirm(ConfirmKind::DebugLog);
                        }
                    }
                    KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // !s: toggle status bar debug mode.
                        self.status_bar_debug = !self.status_bar_debug;
                    }
                    // !r: restart reader (with confirm).
                    KeyCode::Char('r')
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && self.last_session.is_some() =>
                    {
                        self.mode = AppMode::Confirm(ConfirmKind::ReaderRestart);
                    }
                    _ => {}
                }
            }
            // Always consume the second key (don't fall through).
            return;
        }

        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('x')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Ctrl-X: open picker (preserving existing state).
            let cwd = self.picker_state.cwd.clone();
            self.picker_state.restart_loading();
            self.screen = AppScreen::Picker;
            self.start_picker_refresh(cwd);
        } else if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('y')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Ctrl-Y: launch terminal.
            self.try_launch_deferred_terminal();
        } else if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('m')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Ctrl-U: send Ctrl-O to the terminal PTY.
            if let Some(term) = self.terminal.live_ts() {
                term.write_input(b"\x0f");
            }
        } else if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
            && self.terminal.is_live()
        {
            // Esc: activate terminal (go back to chat).
            self.activate_terminal();
        } else if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('r') {
            // r: open raw data view for selected message.
            let data = self.tree_state.selected_data().to_owned();
            self.data_view = Some(DataViewState::new(&data));
        } else if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('I') {
            // Shift-I: open session info view.
            self.open_session_info();
        } else if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('?') {
            // ?: open key shortcuts view.
            self.data_view = Some(DataViewState::from_nodes(build_key_shortcuts_nodes()));
        } else if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('!') {
            // !: begin composite key sequence (!l / !L).
            self.pending_app_key = Some('!');
        } else if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('k')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Ctrl-K: kill confirmation (only when live).
            if self.terminal.is_live() {
                self.mode = AppMode::Confirm(ConfirmKind::Kill);
            }
        } else {
            let action = self
                .tree_state
                .handle_key(key, last_area.height.saturating_sub(1));
            self.apply_tree_action(action);
        }
    }

    fn apply_tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Quit => {
                if self.terminal.is_live() {
                    // Set quit intent and let the user
                    // forward the quit signal via the PTY.
                    self.quit_intent = true;
                    self.activate_terminal();
                    let hint = match self.terminal.session_info().map(|i| &i.provider) {
                        Some(ProviderKind::Cursor) => " Ctrl-D to exit",
                        _ => " Ctrl-D twice to exit",
                    };
                    self.flash_message = Some((hint.to_string(), false, std::time::Instant::now()));
                } else {
                    self.running = false;
                }
            }
            // Space expands terminal
            TreeAction::CycleDisplay if self.tree_state.is_terminal_selected() => {
                self.terminal.expanded = !self.terminal.expanded;
                let sb = self.tree_state.terminal_scrollback_available;
                let crop_h = self.tree_state.terminal_collapsed_crop_height;
                let pty_rows = self.tree_state.terminal_pty_rows;
                self.tree_state
                    .sync_terminal_layout(self.terminal.expanded, sb, crop_h, pty_rows);
            }
            // Interactive component (e.g. table): Enter enters message interaction mode.
            TreeAction::ToggleExpand if self.tree_state.is_interaction_supported() => {
                self.mode = AppMode::MessageInteraction;
                self.tree_state.enter_component_focus();
            }
            // Terminal node overrides: Enter toggle the PTY pane.
            TreeAction::ToggleExpand if self.tree_state.is_terminal_selected() => {
                if self.terminal.is_live() {
                    self.activate_terminal();
                }
            }
            // Ctrl-O / Esc only activates when a live PTY is running.
            TreeAction::TerminalActivate => {
                if self.terminal.is_live() {
                    self.activate_terminal();
                }
            }
            TreeAction::CopyMarkdown => {
                let text = self.tree_state.selected_text().to_owned();
                self.do_copy(&text);
            }
            TreeAction::CopyPlainText => {
                let md = self.tree_state.selected_text().to_owned();
                let plain = markdown_to_plain(&md);
                self.do_copy(&plain);
            }
            TreeAction::CopyRawData => {
                let data = self.tree_state.selected_data().to_owned();
                self.do_copy(&data);
            }
            action => self.tree_state.apply_action(action),
        }
    }
}
