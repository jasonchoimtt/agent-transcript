mod brief;
pub mod component;
mod json;
mod prose;
pub mod table;
mod tool_call;
pub mod tool_result;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use super::state::{MessageState, MessageType, SearchHighlight};
use crate::theme::styles::MessageStyle;
use crate::theme::{ColorVar, Palette};
use crate::tree_scroll_view::message_widget::component::{
    ContentRenderContext, HoverTarget, MessageComponent,
};
use table::{TableComponent, TableState};
use tool_result::{ToolResultComponent, ToolResultState};

/// Returns a boxed [`MessageComponent`] for the given node, or `None` for
/// nodes that should not receive component-level rendering (hidden, group, terminal).
pub fn get_message_component<'a>(
    node: &'a mut MessageState,
) -> Option<Box<dyn MessageComponent + 'a>> {
    // Priority 1: stored state identifies the component type.
    if node
        .ui_state
        .as_ref()
        .is_some_and(|s| s.as_any().is::<TableState>())
    {
        return Some(Box::new(TableComponent { message: node }));
    }
    if node
        .ui_state
        .as_ref()
        .is_some_and(|s| s.as_any().is::<ToolResultState>())
    {
        return Some(Box::new(ToolResultComponent { message: node }));
    }

    // Priority 2: message type.
    match node.message_type {
        MessageType::ToolCall => Some(Box::new(tool_call::ToolCallComponent { message: node })),
        MessageType::Json => Some(Box::new(json::JsonComponent { message: node })),
        MessageType::Table => None, // Table without TableState ui_state — degenerate
        MessageType::ToolResult => {
            // No rich state — render as prose.
            Some(Box::new(prose::ProseComponent { message: node }))
        }
        _ => Some(Box::new(prose::ProseComponent { message: node })),
    }
}

/// Returns the inner-component mouse hit for a node at `(rel_x, rel_y)` in table-space
/// (rel_x from content_x, rel_y from table top including skip_lines offset).
pub fn match_mouse_node(node: &mut MessageState, rel_x: u16, rel_y: u16) -> Option<Vec<usize>> {
    get_message_component(node)?.match_mouse(rel_x, rel_y)
}

pub struct MessageWidget<'a> {
    pub node: &'a mut MessageState,
    pub depth: usize,
    pub selected: bool,
    /// Lines of this node's rendered content to skip at the top (partial first-node render).
    pub skip_lines: u16,
    /// True when this node is a descendant of the selected group node.
    pub group_descent: bool,
    /// True when the last rendered row is the bottom padding line; the gutter
    /// is not drawn on that row so it doesn't bleed into the inter-node margin.
    pub last_row_is_pad: bool,
    pub style: MessageStyle<'a>,
    pub palette: &'a Palette,
    /// True when this node is the active target of message-interaction mode.
    pub interaction: bool,
    /// Non-None when this node contains the current search match.
    pub highlight: Option<SearchHighlight>,
    /// Mark char assigned to this message, shown in the gutter on the first row.
    pub mark: Option<char>,
    /// True when the mouse cursor is over this node.
    pub hovered: bool,
    /// Which sub-region of this node the cursor is over (only meaningful when hovered).
    pub hover_target: Option<&'a HoverTarget>,
}

impl MessageWidget<'_> {
    /// Returns the primary content style for this message type, used as indicator style fallback.
    fn content_style(&self) -> ratatui::style::Style {
        let xml_tag = self.node.tag.as_deref();
        match &self.style {
            MessageStyle::UserMessage(s) => s.resolve(xml_tag).content.to_style(self.palette),
            MessageStyle::AgentMessage(s) => s.content.to_style(self.palette),
            MessageStyle::Thinking(s) => s.content.to_style(self.palette),
            MessageStyle::TaskSummary(s) => s.content.to_style(self.palette),
            MessageStyle::Container(s) => s.resolve(xml_tag).content.to_style(self.palette),
            MessageStyle::System(s) => s.content.to_style(self.palette),
            MessageStyle::ToolCall(s) => s.name.to_style(self.palette),
            MessageStyle::ToolResult(s) => s.content.to_style(self.palette),
            MessageStyle::Json(s) => s.container.to_style(self.palette),
            MessageStyle::Table(s) => s.cell.to_style(self.palette),
            MessageStyle::Other(s) => s.resolve(xml_tag).content.to_style(self.palette),
        }
    }
}

impl Widget for MessageWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let gutter_color = if self.selected {
            Some(self.palette.resolve(&ColorVar::Primary))
        } else if self.group_descent {
            Some(self.palette.resolve(&ColorVar::PrimaryLight))
        } else if self.hovered {
            Some(self.palette.muted)
        } else {
            None
        };

        // Layout: col 0 = gutter | 2*depth indent | indicator | space | text…
        // The single indicator slot shows ▼/▶ when the node has children,
        // otherwise the configured message-type glyph (or space if show_indicator is false).
        let prefix_len = self.depth * 2 + 2;

        // Read node fields needed for indicator before taking mutable borrow.
        let show_indicator = self.node.show_indicator;
        let xml_tag = self.node.tag.clone();
        let has_children = !self.node.children.is_empty();
        let expanded = self.node.expanded;

        if self.skip_lines == 0 {
            let col = area.x + 1 + (self.depth * 2) as u16;
            if let Some(cell) = buf.cell_mut((col, area.y)) {
                let hovering_indicator =
                    matches!(self.hover_target, Some(HoverTarget::IndicatorArea));
                if show_indicator {
                    let (msg_indicator, msg_indicator_style) =
                        self.style.indicator(xml_tag.as_deref());
                    let sym = msg_indicator.map(|s| if s.is_empty() { " " } else { s });
                    // Slot is blank when indicator is None or an explicit space.
                    let slot_blank = sym.is_none_or(|s| s == " ");
                    if hovering_indicator && slot_blank && has_children {
                        let hint = if expanded { "-" } else { "+" };
                        cell.set_symbol(hint).set_fg(self.palette.muted);
                    } else if let Some(sym) = sym {
                        let render_style = if let Some(s) = msg_indicator_style {
                            s.to_style(self.palette)
                        } else {
                            self.content_style()
                        };
                        cell.set_symbol(sym).set_style(render_style);
                    }
                } else if hovering_indicator && has_children {
                    // show_indicator is false — slot is blank; show dimmed hint.
                    let hint = if expanded { "-" } else { "+" };
                    cell.set_symbol(hint).set_fg(self.palette.muted);
                }
            }
        }

        // Text area is inset so the paragraph never touches the indent region.
        let text_area = Rect {
            x: area.x + 1 + prefix_len as u16,
            y: area.y,
            width: area.width.saturating_sub(1 + prefix_len as u16),
            height: area.height,
        };

        // Inner hover slice: passed to render_content for table cell highlighting.
        let inner_hover: Option<&[usize]> = if self.hovered && !self.interaction {
            match self.hover_target {
                Some(HoverTarget::Inner(v)) => Some(v.as_slice()),
                _ => None,
            }
        } else {
            None
        };

        let ctx = ContentRenderContext {
            palette: self.palette,
            style: &self.style,
            skip_lines: self.skip_lines,
            interaction: self.interaction,
            highlight: self.highlight.as_ref(),
            hover: inner_hover,
        };

        if let Some(component) = get_message_component(self.node) {
            component.render_content(text_area, buf, &ctx);
        }

        // Selection gutter at col 0.
        // First row: show mark char when present (muted if unselected, white-on-primary if selected).
        // Remaining rows: show ▌ in the selection color as before.
        let show_mark_on_first = self.mark.is_some() && self.skip_lines == 0;
        if show_mark_on_first
            && let (Some(ch), Some(cell)) = (self.mark, buf.cell_mut((area.x, area.y)))
        {
            if self.selected {
                let bg = self.palette.resolve(&ColorVar::Primary);
                cell.set_symbol(&ch.to_string())
                    .set_fg(ratatui::style::Color::White)
                    .set_bg(bg);
            } else {
                cell.set_symbol(&ch.to_string()).set_fg(self.palette.muted);
            }
        }
        if let Some(color) = gutter_color {
            let stop_before_pad = self.last_row_is_pad && (self.selected || self.hovered);
            let gutter_rows = if stop_before_pad {
                area.height.saturating_sub(1)
            } else {
                area.height
            };
            for row in 0..gutter_rows {
                if row == 0 && show_mark_on_first {
                    continue;
                }
                if let Some(cell) = buf.cell_mut((area.x, area.y + row)) {
                    cell.set_symbol("▌").set_fg(color);
                }
            }
        }
    }
}
