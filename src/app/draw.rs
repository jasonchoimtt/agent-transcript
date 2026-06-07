use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{StatefulWidget, Widget};

use crate::data_view::DataViewUi;
use crate::picker::ui::PickerUi;
use crate::status_bar::StatusBar;
use crate::terminal::PanelState;
use crate::tree_scroll_view::TreeScrollView;

use crate::app::{App, AppMode, AppScreen};

impl App {
    pub(super) fn draw(&mut self, tui: &mut DefaultTerminal) -> color_eyre::Result<Option<Rect>> {
        let mut last_area: Option<Rect> = None;

        tui.draw(|frame| {
            let area = frame.area();
            last_area = Some(area);

            // Picker screen: render the picker full-screen.
            if matches!(self.screen, AppScreen::Picker) {
                PickerUi {
                    palette: &self.theme.palette,
                }
                .render(area, frame.buffer_mut(), &mut self.picker_state);
                return;
            }

            // Transcript screen: existing layout.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(area);

            let pty_rows = if self.terminal.is_live() {
                area.height.saturating_sub(10).max(20)
            } else {
                20
            };
            if let Some(term) = self.terminal.live_ts() {
                term.resize_rows(pty_rows);
            }
            let scrollback = self
                .terminal
                .live_ts()
                .map(|ts| ts.scrollback_available())
                .unwrap_or(0);
            let collapsed_crop_h = self
                .terminal
                .live_ts()
                .and_then(|ts| ts.collapsed_crop.map(|c| c.height));
            self.tree_state.sync_terminal_layout(
                self.terminal.expanded,
                scrollback,
                collapsed_crop_h,
                pty_rows,
            );

            let terminal_active = self.mode == AppMode::Terminal;
            let terminal_expanded = self.terminal.expanded;
            let pane_ref = self.terminal.pane_ref();

            TreeScrollView {
                terminal: pane_ref,
                scrollback_available: scrollback,
                terminal_expanded,
                terminal_active,
                theme: &self.theme,
                message_interaction: self.mode == AppMode::MessageInteraction,
            }
            .render(chunks[0], frame.buffer_mut(), &mut self.tree_state);

            // Show PTY cursor when terminal is rendered and active.
            if let Some((tx, ty, th, tskip)) = self.tree_state.terminal_render_info
                && let PanelState::Live { ts: ref term, .. } = self.terminal.state
            {
                let sb = self.tree_state.terminal_scrollback_available;
                let live_start = if self.tree_state.terminal_expanded {
                    sb
                } else {
                    0
                };
                let screen = term.parser.screen();
                let (crow, ccol) = screen.cursor_position();
                let cursor_row = live_start as i32 + crow as i32 - tskip as i32;
                if cursor_row >= 0 && cursor_row < th as i32 && !screen.hide_cursor() {
                    // PTY content starts at tx+1 (col 0 is the selection gutter).
                    frame.set_cursor_position((tx + 1 + ccol, ty + cursor_row as u16));
                }
            }

            StatusBar {
                flash: self
                    .flash_message
                    .as_ref()
                    .map(|(msg, warn, _)| (msg.as_str(), *warn)),
                mode: &self.mode,
                terminal_live: self.terminal.is_live(),
                terminal_expanded: self.terminal.expanded,
                data_view_open: self.data_view.is_some(),
                debug: self.status_bar_debug,
                tree_state: &self.tree_state,
                session_label: self.terminal.session_label(),
                collapsed_crop: self.terminal.live_ts().and_then(|ts| ts.collapsed_crop),
                pending_app_key: self.pending_app_key,
                primary: self.theme.palette.primary,
                muted: self.theme.palette.muted,
            }
            .render(chunks[1], frame.buffer_mut());

            if let Some(ref mut dv) = self.data_view {
                DataViewUi { theme: &self.theme }.render(area, frame.buffer_mut(), dv);
            }
        })?;

        Ok(last_area)
    }
}
