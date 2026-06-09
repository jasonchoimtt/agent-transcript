use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Clear, StatefulWidget, Widget};

use super::cursor::TreeCursor;
use super::message_widget::MessageWidget;
use super::predicates::nonzero_height;
use super::state::{MessageRenderInfo, Precedence, TreeScrollViewState, get_node, get_node_mut};
use crate::terminal::crop::CollapsedCrop;
use crate::terminal::pane_ref::TerminalPaneRef;
use crate::terminal::placeholder::PlaceholderWidget;
use crate::terminal::ui::TerminalWidget;
use crate::theme::Theme;
use crate::tree_scroll_view::message_widget::component::HoverTarget;

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
    /// When true, the prompt overlay is pinned even when the terminal is not active.
    pub prompt_pinned: bool,
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

        // Pre-compute the overlay plan before the loop so both the inline-terminal height
        // clip and the overlay rendering share a single evaluation of the show condition.
        // `Some((pbsr, crop, prompt_h))` means the overlay will be drawn.
        let overlay_plan: Option<(u16, CollapsedCrop, u16)> = if let TerminalPaneRef::Live(ts) =
            &self.terminal
        {
            if let (Some(pbsr), Some(crop)) = (ts.prompt_box_start_row, ts.collapsed_crop) {
                let prompt_h = (crop.start_row + crop.height).saturating_sub(pbsr);
                if prompt_h > 0 && (self.prompt_pinned || (!state.at_bottom && terminal_active)) {
                    Some((pbsr, crop, prompt_h))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        state.terminal_render_info = None;
        state.render_rects.clear();
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

                // Read immutable data before taking mutable borrow for MessageWidget.
                let (is_terminal, msg_style, highlight) = {
                    let node = get_node(&state.items, &path).unwrap();
                    let s = theme.style_for(&node.message_type);
                    let h = state.search_highlight_for(&path);
                    (node.is_terminal, s, h)
                };

                if is_terminal {
                    state.terminal_render_info =
                        Some((widget_area.x, widget_area.y, widget_area.height, skip));

                    // Clip the inline terminal height to exclude PTY rows that the prompt
                    // overlay will draw, preventing a double-render of those rows.
                    let draw_height = if let Some((pbsr, crop, _)) = overlay_plan {
                        // First block_row (render_offset-space) that maps to pbsr.
                        let first_excluded: i32 = if terminal_expanded {
                            scrollback_available as i32 + pbsr as i32
                        } else {
                            pbsr as i32 - crop.start_row as i32
                        };
                        (first_excluded - skip as i32).clamp(0, visible_rows as i32) as u16
                    } else {
                        visible_rows
                    };

                    let draw_area = Rect {
                        height: draw_height,
                        ..widget_area
                    };
                    match &mut self.terminal {
                        TerminalPaneRef::Live(ts) => {
                            TerminalWidget::new_scroll(
                                ts,
                                terminal_expanded,
                                selected,
                                terminal_active,
                                skip,
                                scrollback_available,
                            )
                            .render(draw_area, buf);
                        }
                        TerminalPaneRef::Placeholder(info) => {
                            PlaceholderWidget {
                                info,
                                selected,
                                primary: self.theme.palette.primary,
                                muted: self.theme.palette.muted,
                            }
                            .render(draw_area, buf);
                        }
                    }
                } else {
                    let group_descent = selected_is_group
                        && path.len() > selection_index.len()
                        && path.starts_with(&selection_index);

                    // The last rendered row is the padding line only when we are showing
                    // all the way to the end of the node (not clipped at the bottom).
                    let last_row_is_pad = skip + visible_rows == h;

                    let hidden_count = if last_row_is_pad {
                        cur.count_hidden_to_next(&state.items)
                    } else {
                        0
                    };

                    // Record render info for hit-testing.
                    state.render_rects.push(MessageRenderInfo {
                        path: path.clone(),
                        widget_area,
                        has_gap_row: last_row_is_pad,
                        hidden_after: hidden_count,
                        skip_lines: skip,
                        visual_depth: depth,
                    });

                    // Derive hover state for this node.
                    let hovered = state.hover.as_ref().is_some_and(|h| h.path == path);
                    let hover_target: Option<&HoverTarget> = if hovered {
                        state.hover.as_ref().map(|h| &h.target)
                    } else {
                        None
                    };

                    let node = get_node_mut(&mut state.items, &path).unwrap();
                    let mark = state.marks.mark_for_id(&node.id);

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
                        mark,
                        hovered,
                        hover_target,
                        terminal_active,
                    }
                    .render(widget_area, buf);

                    // Braille indicator in the padding row when hidden nodes follow.
                    if last_row_is_pad {
                        let indicator = hidden_indicator_char(hidden_count);
                        if !indicator.is_empty() {
                            let pad_y = widget_area.y + visible_rows - 1;
                            if let Some(cell) = buf.cell_mut((area.x + 1, pad_y)) {
                                let muted = theme.palette.muted;
                                cell.set_symbol(indicator).set_fg(muted);
                            }
                        }

                        // Gap row hover text: show "(N hidden)" when hovering.
                        if hidden_count > 0
                            && hovered
                            && matches!(hover_target, Some(HoverTarget::GapRow { .. }))
                        {
                            let pad_y = widget_area.y + visible_rows - 1;
                            let hint = format!("({} hidden)", hidden_count);
                            let muted = theme.palette.muted;
                            for (hx, ch) in (area.x + 2..).zip(hint.chars()) {
                                if hx >= area.x + area.width {
                                    break;
                                }
                                if let Some(cell) = buf.cell_mut((hx, pad_y)) {
                                    cell.set_symbol(&ch.to_string()).set_fg(muted);
                                }
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

        // 6. Prompt overlay — drawn on top of the tree when scrolled above bottom while
        //    the terminal is active or pinned.
        if let Some((pbsr, crop, prompt_h)) = overlay_plan {
            let overlay_height = (prompt_h + 2).min(area.height); // +1 divider row, +1 bottom padding row
            let oa_y = area
                .y
                .saturating_add(area.height)
                .saturating_sub(overlay_height);
            state.prompt_overlay_render_info = Some((area.x, oa_y + 1, prompt_h, pbsr));
            let oa = Rect {
                y: oa_y,
                height: overlay_height,
                ..area
            };
            Clear.render(oa, buf);
            render_prompt_divider(
                Rect { height: 1, ..oa },
                buf,
                self.prompt_pinned,
                self.theme,
            );
            if let TerminalPaneRef::Live(ts) = &mut self.terminal {
                let prompt_block_offset = pbsr.saturating_sub(crop.start_row);
                let terminal_y = oa.y + 1;
                let terminal_h = (prompt_h + 1).min(area.bottom().saturating_sub(terminal_y));
                TerminalWidget::new_scroll(
                    ts,
                    false,
                    false,
                    terminal_active,
                    prompt_block_offset,
                    0,
                )
                .render(
                    Rect {
                        y: terminal_y,
                        height: terminal_h,
                        ..oa
                    },
                    buf,
                );
            }
        } else {
            state.prompt_overlay_render_info = None;
        }
    }
}

fn render_prompt_divider(area: Rect, buf: &mut Buffer, pinned: bool, theme: &Theme) {
    let style = if pinned {
        Style::default().fg(theme.palette.fg)
    } else {
        Style::default()
            .fg(theme.palette.fg)
            .add_modifier(Modifier::DIM)
    };
    for x in area.left()..area.right() {
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_symbol("─").set_style(style);
        }
    }
}
