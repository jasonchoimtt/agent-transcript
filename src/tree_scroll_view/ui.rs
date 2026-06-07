use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{StatefulWidget, Widget};

use super::cursor::TreeCursor;
use super::message_widget::MessageWidget;
use super::predicates::nonzero_height;
use super::state::{Precedence, TreeScrollViewState, get_node};
use crate::terminal::pane_ref::{PlaceholderInfo, TerminalPaneRef};
use crate::terminal::placeholder::PlaceholderWidget;
use crate::terminal::ui::TerminalWidget;
use crate::theme::Theme;

pub fn hidden_indicator_char(count: usize) -> &'static str {
    match count {
        0 => "",
        1 => "⠁",
        2 => "⠃",
        3 => "⠇",
        4 => "⡇",
        _ => "⣿",
    }
}

pub struct TreeScrollView<'a> {
    pub terminal: TerminalPaneRef<'a>,
    pub scrollback_available: u16,
    pub terminal_expanded: bool,
    pub terminal_active: bool,
    pub theme: &'a Theme,
    /// True when message-interaction mode is active (keys routed to selected widget).
    pub message_interaction: bool,
}

impl StatefulWidget for TreeScrollView<'_> {
    type State = TreeScrollViewState;

    fn render(mut self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if area.height == 0 {
            return;
        }

        // 1. Update viewport size; clears all cached heights if width changed.
        state.set_viewport_size(area.width, area.height);

        // 2. If at_bottom, snap to bottom before rendering.
        // 3. Otherwise bring the focused item into view according to precedence.
        if state.at_bottom {
            state.snap_to_bottom(self.terminal_active);
        } else {
            match state.precedence.clone() {
                Precedence::Selection => state.ensure_selection_visible(),
                Precedence::InnerFocus { path, line_range } => {
                    state.ensure_inner_focus_visible(path, line_range);
                }
                Precedence::Top => {}
            }
        }

        // 4. Walk visible nodes from top_index and render.
        let mut cur = match TreeCursor::at(&state.items, state.top_index.clone()) {
            Some(c) => c,
            None => match TreeCursor::closest(&state.items, &state.top_index.clone()) {
                Some(c) => c,
                None => return,
            },
        };

        let top_offset = state.top_offset;
        let selection_index = state.selection_index.clone();
        let terminal_expanded = self.terminal_expanded;
        let terminal_active = self.terminal_active;
        let scrollback_available = self.scrollback_available;
        let theme = self.theme;
        let message_interaction = self.message_interaction;

        // Determine if the selected node is a group so we can draw descendant gutters.
        let selected_is_group = get_node(&state.items, &selection_index)
            .map(|n| n.group)
            .unwrap_or(false);

        state.terminal_render_info = None;
        let mut y = area.top();
        let mut first = true;

        loop {
            if y >= area.bottom() {
                break;
            }

            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = state.size_node(&path, depth);

            let skip = if first {
                first = false;
                top_offset.min(h.saturating_sub(1))
            } else {
                0
            };

            let visible_rows = h.saturating_sub(skip).min(area.bottom() - y);
            if visible_rows > 0 {
                let widget_area = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: visible_rows,
                };
                let selected = path == selection_index;
                let node = get_node(&state.items, &path).unwrap();

                if node.is_terminal {
                    state.terminal_render_info =
                        Some((widget_area.x, widget_area.y, widget_area.height, skip));
                    // Move the pane ref out of self; the sentinel is never observed because
                    // the terminal node appears at most once per tree.
                    match std::mem::replace(
                        &mut self.terminal,
                        TerminalPaneRef::Placeholder(PlaceholderInfo {
                            provider_name: "",
                            session_id: None,
                            directory: None,
                            exit_code: None,
                        }),
                    ) {
                        TerminalPaneRef::Live(term) => {
                            TerminalWidget::new_scroll(
                                term,
                                terminal_expanded,
                                selected,
                                terminal_active,
                                skip,
                                scrollback_available,
                            )
                            .render(widget_area, buf);
                        }
                        TerminalPaneRef::Placeholder(info) => {
                            PlaceholderWidget {
                                info: &info,
                                selected,
                                primary: self.theme.palette.primary,
                                muted: self.theme.palette.muted,
                            }
                            .render(widget_area, buf);
                        }
                    }
                } else {
                    let group_descent = selected_is_group
                        && path.len() > selection_index.len()
                        && path.starts_with(&selection_index);

                    // The last rendered row is the padding line only when we are showing
                    // all the way to the end of the node (not clipped at the bottom).
                    let last_row_is_pad = skip + visible_rows == h;

                    let msg_style = theme.style_for(&node.message_type);

                    let highlight = state.search_highlight_for(&path);

                    MessageWidget {
                        node,
                        depth,
                        selected,
                        skip_lines: skip,
                        group_descent,
                        last_row_is_pad,
                        style: msg_style,
                        palette: &theme.palette,
                        interaction: selected && message_interaction,
                        highlight,
                    }
                    .render(widget_area, buf);

                    // Braille indicator in the padding row when hidden nodes follow.
                    if last_row_is_pad {
                        let hidden_count = cur.count_hidden_to_next(&state.items);
                        let indicator = hidden_indicator_char(hidden_count);
                        if !indicator.is_empty() {
                            let pad_y = widget_area.y + visible_rows - 1;
                            if let Some(cell) = buf.cell_mut((area.x + 1, pad_y)) {
                                let muted = theme.palette.muted;
                                cell.set_symbol(indicator).set_fg(muted);
                            }
                        }
                    }
                }

                y += visible_rows;
            }

            if !cur.advance(&state.items, nonzero_height) {
                break;
            }
        }

        // 5. Update at_bottom flag after rendering.
        state.update_at_bottom();
    }
}
