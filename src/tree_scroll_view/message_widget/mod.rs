mod brief;
mod json;
mod prose;
pub mod table;
mod tool_call;
pub mod tool_result;

use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use super::state::MessageState;
use crate::theme::styles::{MessageStyle, ToolResultStyle};
use crate::theme::{ColorVar, Palette};
use table::{TableUiState, render::render_table};
use tool_result::{
    ToolResultUiState,
    render::{render_compact, render_tool_result},
};

pub struct MessageWidget<'a> {
    pub node: &'a MessageState,
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
        } else {
            None
        };

        // Layout: col 0 = gutter | 2*depth indent | indicator | space | text…
        // The single indicator slot shows ▼/▶ when the node has children,
        // otherwise the configured message-type glyph (or space if show_indicator is false).
        let prefix_len = self.depth * 2 + 2;

        if self.skip_lines == 0 {
            let col = area.x + 1 + (self.depth * 2) as u16;
            if let Some(cell) = buf.cell_mut((col, area.y))
                && self.node.show_indicator
            {
                let xml_tag = self.node.tag.as_deref();
                let (msg_indicator, msg_indicator_style) = self.style.indicator(xml_tag);
                // An empty symbol writes "" to the terminal (no output), which can't
                // overwrite a previously rendered glyph at that position. Use a space
                // to actively clear the cell instead.
                let sym = msg_indicator.map(|s| if s.is_empty() { " " } else { s });
                if let Some(sym) = sym {
                    let render_style = if let Some(s) = msg_indicator_style {
                        s.to_style(self.palette)
                    } else {
                        self.content_style()
                    };
                    cell.set_symbol(sym).set_style(render_style);
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

        match self.style {
            MessageStyle::ToolCall(tc_style) => {
                tool_call::render_tool_call(
                    text_area,
                    buf,
                    self.node,
                    tc_style,
                    self.palette,
                    self.skip_lines,
                );
            }
            MessageStyle::Json(json_style) => {
                json::render_json(
                    text_area,
                    buf,
                    self.node,
                    json_style,
                    self.palette,
                    self.skip_lines,
                );
            }
            MessageStyle::Table(table_style) => {
                if self.node.show_more {
                    if let Some(ts) = self
                        .node
                        .ui_state
                        .as_deref()
                        .and_then(|s| s.as_any().downcast_ref::<TableUiState>())
                    {
                        render_table(
                            text_area,
                            ts,
                            self.interaction,
                            self.palette,
                            table_style,
                            buf,
                            self.skip_lines,
                        );
                    }
                } else {
                    // Compact: render the brief summary as a single line.
                    let source = self.node.brief.as_deref().unwrap_or("Table");
                    let (clipped, truncated) = brief::clip_brief(source, text_area.width as usize);
                    let line = Line::from(Span::styled(
                        clipped,
                        table_style.cell.to_style(self.palette),
                    ));
                    brief::render_brief_line(
                        text_area,
                        buf,
                        line,
                        truncated,
                        false,
                        self.skip_lines,
                    );
                }
            }
            MessageStyle::ToolResult(tr_style) => {
                render_tool_result_variant(
                    text_area,
                    buf,
                    self.node,
                    tr_style,
                    self.palette,
                    self.skip_lines,
                    self.interaction,
                );
            }
            other_style => {
                let xml_tag = self.node.tag.as_deref();
                let content_style = match other_style {
                    MessageStyle::UserMessage(s) => {
                        s.resolve(xml_tag).content.to_style(self.palette)
                    }
                    MessageStyle::AgentMessage(s) => s.content.to_style(self.palette),
                    MessageStyle::Thinking(s) => s.content.to_style(self.palette),
                    MessageStyle::TaskSummary(s) => s.content.to_style(self.palette),
                    MessageStyle::Container(s) => s.resolve(xml_tag).content.to_style(self.palette),
                    MessageStyle::System(s) => s.content.to_style(self.palette),
                    MessageStyle::Other(s) => s.resolve(xml_tag).content.to_style(self.palette),
                    MessageStyle::ToolCall(_)
                    | MessageStyle::Json(_)
                    | MessageStyle::Table(_)
                    | MessageStyle::ToolResult(_) => {
                        unreachable!()
                    }
                };

                let is_markdown = other_style.uses_markdown(xml_tag);

                prose::render_prose(
                    text_area,
                    buf,
                    self.node,
                    content_style,
                    is_markdown,
                    self.skip_lines,
                    self.palette,
                );
            }
        }

        // Selection gutter at col 0.
        if let Some(color) = gutter_color {
            let stop_before_pad = self.selected && self.last_row_is_pad;
            let gutter_rows = if stop_before_pad {
                area.height.saturating_sub(1)
            } else {
                area.height
            };
            for row in 0..gutter_rows {
                if let Some(cell) = buf.cell_mut((area.x, area.y + row)) {
                    cell.set_symbol("▌").set_fg(color);
                }
            }
        }
    }
}

fn render_tool_result_variant(
    text_area: Rect,
    buf: &mut Buffer,
    node: &MessageState,
    tr_style: &ToolResultStyle,
    palette: &Palette,
    skip_lines: u16,
    interaction: bool,
) {
    let display_text = node.text.as_deref().unwrap_or("");
    let collapsed_with_children = !node.expanded && !node.children.is_empty();

    if node.show_more {
        if let Some(ts) = node
            .ui_state
            .as_deref()
            .and_then(|s| s.as_any().downcast_ref::<ToolResultUiState>())
        {
            render_tool_result(
                text_area,
                ts,
                interaction,
                palette,
                tr_style,
                buf,
                skip_lines,
            );
        } else {
            // No rich widget — fall through to plain text.
            let content_style = tr_style.content.to_style(palette);
            let mut text = display_text
                .into_text()
                .unwrap_or_else(|_| Text::raw(display_text));
            text.style = content_style;
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((skip_lines, 0))
                .render(text_area, buf);
        }
    } else if let Some(ts) = node
        .ui_state
        .as_deref()
        .and_then(|s| s.as_any().downcast_ref::<ToolResultUiState>())
    {
        render_compact(
            text_area,
            ts,
            palette,
            tr_style,
            buf,
            collapsed_with_children,
            skip_lines,
        );
    } else {
        // Compact: render brief summary as a single line.
        let source = node
            .brief
            .as_deref()
            .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
        let (clipped, truncated) = brief::clip_brief(source, text_area.width as usize);
        let line = Line::from(Span::styled(clipped, tr_style.content.to_style(palette)));
        brief::render_brief_line(
            text_area,
            buf,
            line,
            truncated,
            collapsed_with_children,
            skip_lines,
        );
    }
}
