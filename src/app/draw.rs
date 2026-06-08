use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Clear, StatefulWidget, Widget};

use crate::data_view::DataViewUi;
use crate::picker::ui::PickerUi;
use crate::status_bar::StatusBar;
use crate::terminal::PanelState;
use crate::terminal::ui::TerminalWidget;
use crate::theme::Theme;
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

            // Collect prompt overlay parameters before borrowing terminal as pane_ref.
            let prompt_box_start_row = self
                .terminal
                .live_ts()
                .and_then(|ts| ts.prompt_box_start_row);
            let overlay_collapsed_crop = self.terminal.live_ts().and_then(|ts| ts.collapsed_crop);

            let show_overlay = prompt_box_start_row.is_some()
                && overlay_collapsed_crop.is_some()
                && (self.prompt_pinned || (!self.tree_state.at_bottom && terminal_active));

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

            // Prompt box overlay — drawn on top of the tree when scrolled above bottom.
            // pane_ref is dropped after TreeScrollView::render, so live_ts() is available again.
            if show_overlay {
                let pbsr = prompt_box_start_row.unwrap();
                let crop = overlay_collapsed_crop.unwrap();
                let prompt_h = (crop.start_row + crop.height).saturating_sub(pbsr);
                if prompt_h > 0 {
                    let overlay_height = prompt_h + 2; // +1 for divider row, +1 to match terminal node's bottom padding row
                    let oa_y = chunks[0]
                        .y
                        .saturating_add(chunks[0].height)
                        .saturating_sub(overlay_height);
                    // Store geometry for mouse translation (area_x, prompt_rows_y, prompt_h, pty_start).
                    self.prompt_overlay_render_info = Some((chunks[0].x, oa_y + 1, prompt_h, pbsr));
                    let oa = Rect {
                        y: oa_y,
                        height: overlay_height,
                        ..chunks[0]
                    };
                    Clear.render(oa, frame.buffer_mut());
                    render_prompt_divider(
                        Rect { height: 1, ..oa },
                        frame.buffer_mut(),
                        self.prompt_pinned,
                        &self.theme,
                    );
                    if let Some(term) = self.terminal.live_ts() {
                        let prompt_block_offset = pbsr.saturating_sub(crop.start_row);
                        TerminalWidget::new_scroll(
                            term,
                            false,
                            false,
                            terminal_active,
                            prompt_block_offset,
                            0,
                        )
                        .render(
                            Rect {
                                y: oa.y + 1,
                                height: prompt_h + 1, // +1 to include the terminal node's bottom padding row
                                ..oa
                            },
                            frame.buffer_mut(),
                        );
                    }

                    // Cursor: if cursor is inside prompt overlay rows, place it there.
                    if terminal_active
                        && let PanelState::Live { ts: ref term, .. } = self.terminal.state
                    {
                        let screen = term.parser.screen();
                        let (crow, ccol) = screen.cursor_position();
                        if crow >= pbsr && !screen.hide_cursor() {
                            let row_in_overlay = crow - pbsr;
                            if row_in_overlay < prompt_h {
                                frame.set_cursor_position((
                                    oa.x + 1 + ccol,
                                    oa.y + 1 + row_in_overlay,
                                ));
                            }
                        }
                    }
                }
            }

            if !show_overlay {
                self.prompt_overlay_render_info = None;
            }

            // Show PTY cursor for inline terminal rows not covered by the overlay.
            // Skip when pinned but not active — the user is reading, not typing.
            // When the overlay is active, only suppress the cursor for rows >= prompt_box_start_row
            // (those are drawn in the overlay instead); rows above (scrollback) still show here.
            if (terminal_active || !self.prompt_pinned)
                && let Some((tx, ty, th, tskip)) = self.tree_state.terminal_render_info
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
                let in_overlay =
                    show_overlay && prompt_box_start_row.is_some_and(|pbsr| crow >= pbsr);
                let cursor_row = live_start as i32 + crow as i32 - tskip as i32;
                if !in_overlay && cursor_row >= 0 && cursor_row < th as i32 && !screen.hide_cursor()
                {
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
                prompt_pinned: self.prompt_pinned,
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

fn render_prompt_divider(area: Rect, buf: &mut Buffer, pinned: bool, theme: &Theme) {
    let (ch, style) = if pinned {
        ("─", Style::default().fg(theme.palette.fg))
    } else {
        (
            "─",
            Style::default()
                .fg(theme.palette.fg)
                .add_modifier(Modifier::DIM),
        )
    };
    for x in area.left()..area.right() {
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_symbol(ch).set_style(style);
        }
    }
}
