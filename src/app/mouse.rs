use std::io::Write;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::app::{App, AppMode};
use crate::event::Event;
use crate::terminal::mouse::encode_mouse_event;
use crate::terminal::osc::MouseMode;
use crate::tree_scroll_view::TreeAction;
use crate::tree_scroll_view::message_widget::component::{HoverState, HoverTarget, MouseHitResult};
use crate::tree_scroll_view::message_widget::table::TableState;
use crate::tree_scroll_view::state::get_node_mut;

impl App {
    pub(super) fn handle_mouse(&mut self, ev: MouseEvent) {
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if ev.kind == MouseEventKind::ScrollUp && self.mode == AppMode::Terminal {
                    self.mode = AppMode::Normal;
                    let _ = std::io::stdout().write_all(b"\x1b[0 q");
                    let _ = std::io::stdout().flush();
                }
                let mut count = 1;

                // Batch consecutive scroll events of the same direction.
                while let Some(event2) = self.events.try_recv() {
                    match event2 {
                        Event::Crossterm(crossterm::event::Event::Mouse(ev2))
                            if ev2.kind == ev.kind =>
                        {
                            count += 1
                        }
                        other => {
                            self.events.unget(other);
                            break;
                        }
                    }
                }

                let in_dv = self
                    .data_view
                    .as_ref()
                    .is_some_and(|dv| rect_contains(dv.popup_area, ev.column, ev.row));
                if in_dv {
                    let dv = self.data_view.as_mut().unwrap();
                    match ev.kind {
                        MouseEventKind::ScrollUp => dv.tree.scroll_up(count * 3),
                        MouseEventKind::ScrollDown => dv.tree.scroll_down(count * 3),
                        _ => unreachable!(),
                    }
                } else {
                    match ev.kind {
                        MouseEventKind::ScrollUp => self.tree_state.scroll_up(count * 3),
                        MouseEventKind::ScrollDown => self.tree_state.scroll_down(count * 3),
                        _ => unreachable!(),
                    }
                }
            }

            MouseEventKind::Moved => {
                if self.mode == AppMode::Terminal && self.is_over_terminal(ev.column, ev.row) {
                    self.forward_mouse_to_pty(ev);
                } else if self.is_over_prompt_overlay(ev.column, ev.row) {
                    // Overlay covers tree content — don't trigger hover on hidden messages.
                    self.tree_state.hover = None;
                } else {
                    let in_dv = self
                        .data_view
                        .as_ref()
                        .is_some_and(|dv| rect_contains(dv.popup_area, ev.column, ev.row));
                    if in_dv {
                        let hit = self
                            .data_view
                            .as_mut()
                            .unwrap()
                            .tree
                            .hit_test(ev.column, ev.row);
                        self.data_view.as_mut().unwrap().tree.hover = hit_to_hover(hit);
                        self.tree_state.hover = None;
                    } else {
                        if let Some(dv) = &mut self.data_view {
                            dv.tree.hover = None;
                        }
                        let hit = self.tree_state.hit_test(ev.column, ev.row);
                        self.tree_state.hover = hit_to_hover(hit);
                    }
                }
            }

            MouseEventKind::Down(MouseButton::Left) => {
                if self.mode == AppMode::Terminal && self.is_over_terminal(ev.column, ev.row) {
                    self.forward_mouse_to_pty(ev);
                } else if self.is_over_prompt_overlay(ev.column, ev.row) {
                    // Clicking the overlay activates the terminal in floating mode.
                    if self.terminal.is_live() {
                        self.activate_terminal_floating();
                    }
                } else {
                    // Clicking outside the terminal exits terminal mode so that a
                    // subsequent click on the terminal pane activates rather than forwards.
                    if self.mode == AppMode::Terminal {
                        self.mode = AppMode::Normal;
                    }
                    let in_dv = self
                        .data_view
                        .as_ref()
                        .is_some_and(|dv| rect_contains(dv.popup_area, ev.column, ev.row));
                    if in_dv {
                        self.handle_click_in_data_view(ev.column, ev.row);
                    } else {
                        self.handle_click_in_tree(ev.column, ev.row);
                    }
                }
            }

            _ => {
                if self.mode == AppMode::Terminal && self.is_over_terminal(ev.column, ev.row) {
                    self.forward_mouse_to_pty(ev);
                }
            }
        }
    }

    fn handle_click_in_tree(&mut self, col: u16, row: u16) {
        let hit = self.tree_state.hit_test(col, row);
        match hit {
            MouseHitResult::Terminal => {
                if self.terminal.is_live() {
                    self.activate_terminal();
                }
            }
            MouseHitResult::GapRow { path, .. } => {
                self.tree_state.select_path(path);
                self.apply_tree_action(TreeAction::RevealNextFive);
            }
            MouseHitResult::IndicatorArea { path } => {
                self.tree_state.select_path(path);
                self.apply_tree_action(TreeAction::ToggleExpand);
            }
            MouseHitResult::InnerComponent { path, hit } => {
                self.tree_state.select_path(path.clone());
                if let Some([display_row, col_idx]) = hit.get(..2).and_then(|s| {
                    if s.len() == 2 {
                        Some([s[0], s[1]])
                    } else {
                        None
                    }
                }) {
                    if let Some(node) = get_node_mut(&mut self.tree_state.items, &path) {
                        if let Some(ts) = node
                            .ui_state
                            .as_mut()
                            .and_then(|s| s.as_any_mut().downcast_mut::<TableState>())
                        {
                            ts.selected_row = if display_row == 0 {
                                None
                            } else {
                                Some(display_row - 1)
                            };
                            ts.selected_col = col_idx;
                            ts.clamp_scroll();
                        }
                    }
                }
                self.mode = AppMode::MessageInteraction;
                self.tree_state.enter_component_focus();
            }
            MouseHitResult::Message { path } => {
                if path == self.tree_state.selection_index {
                    self.apply_tree_action(TreeAction::ToggleShowMore);
                } else {
                    self.tree_state.select_path(path);
                    if self.mode == AppMode::MessageInteraction {
                        self.mode = AppMode::Normal;
                    }
                }
            }
            MouseHitResult::Outside => {}
        }
    }

    fn handle_click_in_data_view(&mut self, col: u16, row: u16) {
        let Some(dv) = &mut self.data_view else {
            return;
        };
        let hit = dv.tree.hit_test(col, row);
        match hit {
            MouseHitResult::GapRow { path, .. } => {
                dv.tree.select_path(path);
                dv.tree.apply_action(TreeAction::RevealNextFive);
            }
            MouseHitResult::IndicatorArea { path } => {
                dv.tree.select_path(path);
                dv.tree.toggle_expand();
            }
            MouseHitResult::Message { path } | MouseHitResult::InnerComponent { path, .. } => {
                if path == dv.tree.selection_index {
                    if dv.tree.selection_can_show_more() {
                        dv.tree.toggle_show_more();
                    }
                } else {
                    dv.tree.select_path(path);
                }
            }
            MouseHitResult::Terminal | MouseHitResult::Outside => {}
        }
    }

    fn is_over_terminal(&self, col: u16, row: u16) -> bool {
        if let Some((tx, ty, th, _)) = self.tree_state.terminal_render_info {
            if col >= tx && col < tx + self.tree_state.viewport_width && row >= ty && row < ty + th
            {
                return true;
            }
        }
        self.is_over_prompt_overlay(col, row)
    }

    fn is_over_prompt_overlay(&self, col: u16, row: u16) -> bool {
        let Some((area_x, prompt_y, prompt_h, _)) = self.prompt_overlay_render_info else {
            return false;
        };
        col > area_x && row >= prompt_y && row < prompt_y + prompt_h
    }

    /// Forward a mouse event to the PTY if it is in a mouse-tracking mode.
    fn forward_mouse_to_pty(&mut self, ev: MouseEvent) {
        // Overlay translation takes priority: when the overlay is active it visually covers
        // the bottom rows of the inline terminal, so those coordinates must map to prompt
        // rows rather than through the inline terminal's scrollback offset.
        let translated = self
            .translate_mouse_to_prompt_overlay(ev)
            .or_else(|| self.tree_state.translate_mouse_to_pty(ev));
        if let Some(term) = self.terminal.live_ts()
            && term.mouse_mode != MouseMode::Off
            && let Some(translated) = translated
            && let Some(bytes) =
                encode_mouse_event(translated, term.mouse_mode, term.mouse_encoding)
        {
            term.write_input(&bytes);
        }
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

fn hit_to_hover(hit: MouseHitResult) -> Option<HoverState> {
    match hit {
        MouseHitResult::GapRow { path, hidden_after } => Some(HoverState {
            path,
            target: HoverTarget::GapRow { hidden_after },
        }),
        MouseHitResult::IndicatorArea { path } => Some(HoverState {
            path,
            target: HoverTarget::IndicatorArea,
        }),
        MouseHitResult::InnerComponent { path, hit } => Some(HoverState {
            path,
            target: HoverTarget::Inner(hit),
        }),
        MouseHitResult::Message { path } => Some(HoverState {
            path,
            target: HoverTarget::Message,
        }),
        MouseHitResult::Terminal | MouseHitResult::Outside => None,
    }
}
