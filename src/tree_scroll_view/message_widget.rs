use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use super::ansi::{clip_to_visual_width, visual_width};
use super::markdown::{first_line_clipped, render_markdown};
use super::state::{MessageType, UiState};
use super::table::{TableUiState, render::render_table};
use super::tool_result::{
    ToolResultUiState,
    render::{render_compact, render_tool_result},
};
use crate::theme::styles::{JsonStyle, MessageStyle, ToolCallStyle};
use crate::theme::{ColorVar, Palette};

const PREFIX_STYLE: Style = Style::new().fg(Color::DarkGray);

pub struct MessageWidget<'a> {
    pub text: Option<&'a str>,
    pub depth: usize,
    pub selected: bool,
    pub expanded: bool,
    pub has_children: bool,
    /// Lines of this node's rendered content to skip at the top (partial first-node render).
    pub skip_lines: u16,
    /// When false, render only one truncated line using `brief` or the first line of text.
    pub show_more: bool,
    /// Short summary shown when `show_more` is false; falls back to first line of text.
    pub brief: Option<&'a str>,
    /// True when this node is a descendant of the selected group node.
    pub group_descent: bool,
    /// True when the last rendered row is the bottom padding line; the gutter
    /// is not drawn on that row so it doesn't bleed into the inter-node margin.
    pub last_row_is_pad: bool,
    /// When false, suppress the message-type indicator glyph (renders a space for alignment).
    pub show_indicator: bool,
    /// XML tag stripped from the message text by the parser; selects per-tag style overrides
    /// in `UserMessageStyle`. `None` for all non-user-message nodes.
    pub xml_tag: Option<&'a str>,
    pub style: MessageStyle<'a>,
    pub palette: &'a Palette,
    pub message_type: &'a MessageType,
    /// JSON node kind (`"string"`, `"number"`, `"bool"`, `"null"`, `"object"`, `"array"`, `"raw"`).
    /// Only meaningful when `style` is `MessageStyle::Json`; empty string otherwise.
    pub display_kind: &'a str,
    /// Rich widget state (e.g. `TableUiState`). Set for nodes with `ui_state`.
    pub ui_state: Option<&'a dyn UiState>,
    /// True when this node is the active target of message-interaction mode.
    pub interaction: bool,
}

impl MessageWidget<'_> {
    /// Returns the primary content style for this message type, used as indicator style fallback.
    fn content_style(&self) -> ratatui::style::Style {
        match &self.style {
            MessageStyle::UserMessage(s) => s.resolve(self.xml_tag).content.to_style(self.palette),
            MessageStyle::AgentMessage(s) => s.content.to_style(self.palette),
            MessageStyle::Thinking(s) => s.content.to_style(self.palette),
            MessageStyle::TaskSummary(s) => s.content.to_style(self.palette),
            MessageStyle::Container(s) => s.resolve(self.xml_tag).content.to_style(self.palette),
            MessageStyle::System(s) => s.content.to_style(self.palette),
            MessageStyle::ToolCall(s) => s.name.to_style(self.palette),
            MessageStyle::ToolResult(s) => s.content.to_style(self.palette),
            MessageStyle::Json(s) => s.container.to_style(self.palette),
            MessageStyle::Table(s) => s.cell.to_style(self.palette),
            MessageStyle::Other(s) => s.resolve(self.xml_tag).content.to_style(self.palette),
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
                && self.show_indicator
            {
                let (msg_indicator, msg_indicator_style) = self.style.indicator(self.xml_tag);
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

        let display_text = self.text.unwrap_or("");

        let collapsed_with_children = !self.expanded && self.has_children;

        match self.style {
            MessageStyle::ToolCall(tc_style) => {
                render_tool_call(
                    text_area,
                    buf,
                    display_text,
                    tc_style,
                    self.palette,
                    self.brief,
                    self.show_more,
                    self.skip_lines,
                    collapsed_with_children,
                );
            }
            MessageStyle::Json(json_style) => {
                render_json(
                    text_area,
                    buf,
                    display_text,
                    json_style,
                    self.palette,
                    self.display_kind,
                    self.brief,
                    self.show_more,
                    self.skip_lines,
                    collapsed_with_children,
                );
            }
            MessageStyle::Table(table_style) => {
                if self.show_more {
                    if let Some(ts) = self
                        .ui_state
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
                    let source = self.brief.unwrap_or("Table");
                    let (clipped, truncated) = clip_brief(source, text_area.width as usize);
                    let line = Line::from(Span::styled(
                        clipped,
                        table_style.cell.to_style(self.palette),
                    ));
                    render_brief_line(text_area, buf, line, truncated, false, self.skip_lines);
                }
            }
            MessageStyle::ToolResult(tr_style) => {
                if self.show_more {
                    if let Some(ts) = self
                        .ui_state
                        .and_then(|s| s.as_any().downcast_ref::<ToolResultUiState>())
                    {
                        render_tool_result(
                            text_area,
                            ts,
                            self.interaction,
                            self.palette,
                            tr_style,
                            buf,
                            self.skip_lines,
                        );
                    } else {
                        // No rich widget — fall through to plain text.
                        let content_style = tr_style.content.to_style(self.palette);
                        let mut text = display_text
                            .into_text()
                            .unwrap_or_else(|_| Text::raw(display_text));
                        text.style = content_style;
                        Paragraph::new(text)
                            .wrap(Wrap { trim: false })
                            .scroll((self.skip_lines, 0))
                            .render(text_area, buf);
                    }
                } else if let Some(ts) = self
                    .ui_state
                    .and_then(|s| s.as_any().downcast_ref::<ToolResultUiState>())
                {
                    render_compact(
                        text_area,
                        ts,
                        self.palette,
                        tr_style,
                        buf,
                        collapsed_with_children,
                        self.skip_lines,
                    );
                } else {
                    // Compact: render brief summary as a single line.
                    let source = self
                        .brief
                        .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
                    let (clipped, truncated) = clip_brief(source, text_area.width as usize);
                    let line = Line::from(Span::styled(
                        clipped,
                        tr_style.content.to_style(self.palette),
                    ));
                    render_brief_line(
                        text_area,
                        buf,
                        line,
                        truncated,
                        collapsed_with_children,
                        self.skip_lines,
                    );
                }
            }
            other_style => {
                let content_style = match other_style {
                    MessageStyle::UserMessage(s) => {
                        s.resolve(self.xml_tag).content.to_style(self.palette)
                    }
                    MessageStyle::AgentMessage(s) => s.content.to_style(self.palette),
                    MessageStyle::Thinking(s) => s.content.to_style(self.palette),
                    MessageStyle::TaskSummary(s) => s.content.to_style(self.palette),
                    MessageStyle::Container(s) => {
                        s.resolve(self.xml_tag).content.to_style(self.palette)
                    }
                    MessageStyle::System(s) => s.content.to_style(self.palette),
                    MessageStyle::Other(s) => {
                        s.resolve(self.xml_tag).content.to_style(self.palette)
                    }
                    MessageStyle::ToolCall(_)
                    | MessageStyle::Json(_)
                    | MessageStyle::Table(_)
                    | MessageStyle::ToolResult(_) => {
                        unreachable!()
                    }
                };

                let is_markdown = other_style.uses_markdown(self.xml_tag);

                if !self.show_more {
                    if is_markdown && self.brief.is_none() {
                        // Render markdown and clip the first line for compact display.
                        let rendered = render_markdown(display_text, self.palette);
                        let has_more_lines = rendered.lines.len() > 1;
                        let muted = self.palette.resolve(&ColorVar::Muted);
                        let (mut line, inline_clipped) =
                            first_line_clipped(&rendered, text_area.width, muted);
                        if has_more_lines && !inline_clipped {
                            line.spans.push(Span::styled("…", Style::new().fg(muted)));
                        } else if collapsed_with_children {
                            line.spans.push(Span::styled("▾", Style::new().fg(muted)));
                        }
                        Paragraph::new(vec![line])
                            .scroll((self.skip_lines, 0))
                            .render(text_area, buf);
                    } else {
                        let source = self
                            .brief
                            .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
                        let more_lines =
                            self.brief.is_none() && display_text.lines().nth(1).is_some();
                        let (clipped, truncated) = clip_brief(source, text_area.width as usize);
                        // Parse ANSI codes so coloured content (e.g. bash-stdout) renders
                        // correctly; for plain text this just produces a single styled span.
                        let line = clipped
                            .into_text()
                            .map(|t| {
                                let mut l = t.lines.into_iter().next().unwrap_or_default();
                                l.style = content_style;
                                l
                            })
                            .unwrap_or_else(|_| {
                                Line::from(Span::styled(clipped.to_string(), content_style))
                            });
                        render_brief_line(
                            text_area,
                            buf,
                            line,
                            truncated || more_lines,
                            collapsed_with_children,
                            self.skip_lines,
                        );
                    }
                } else if is_markdown {
                    let rendered = render_markdown(display_text, self.palette);
                    Paragraph::new(rendered)
                        .wrap(Wrap { trim: false })
                        .scroll((self.skip_lines, 0))
                        .render(text_area, buf);
                } else {
                    let mut text = display_text
                        .into_text()
                        .unwrap_or_else(|_| Text::raw(display_text));
                    text.style = content_style;
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.skip_lines, 0))
                        .render(text_area, buf);
                }
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

fn clip_brief(source: &str, available: usize) -> (&str, bool) {
    if visual_width(source) > available {
        (
            clip_to_visual_width(source, available.saturating_sub(1)).0,
            true,
        )
    } else {
        (source, false)
    }
}

fn render_brief_line<'a>(
    text_area: Rect,
    buf: &mut Buffer,
    mut line: Line<'a>,
    needs_ellipsis: bool,
    collapsed: bool,
    skip_lines: u16,
) {
    if needs_ellipsis {
        line.spans.push(Span::styled("…", Style::new().dim()));
    } else if collapsed {
        line.spans.push(Span::styled("▾", Style::new().dim()));
    }
    Paragraph::new(vec![line])
        .scroll((skip_lines, 0))
        .render(text_area, buf);
}

/// Split a line on the first `:`, returning `(before, after_colon)`.
/// `after_colon` includes any space that follows the colon.
fn split_at_colon(s: &str) -> Option<(&str, &str)> {
    s.find(':').map(|i| (&s[..i], &s[i + 1..]))
}

/// Split at the first `(` or `:`, whichever comes first.
/// Returns `(name, sep_char, after)` where for `(` the trailing `)` in `after` is stripped.
fn split_at_first_sep(s: &str) -> Option<(&str, char, &str)> {
    let paren_pos = s.find('(');
    let colon_pos = s.find(':');
    match (paren_pos, colon_pos) {
        (Some(p), Some(c)) if p < c => Some((
            &s[..p],
            '(',
            s[p + 1..].strip_suffix(')').unwrap_or(&s[p + 1..]),
        )),
        (Some(p), None) => Some((
            &s[..p],
            '(',
            s[p + 1..].strip_suffix(')').unwrap_or(&s[p + 1..]),
        )),
        (_, Some(c)) => Some((&s[..c], ':', &s[c + 1..])),
        (None, None) => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_json(
    text_area: Rect,
    buf: &mut Buffer,
    display_text: &str,
    json: &JsonStyle,
    palette: &Palette,
    kind: &str,
    brief: Option<&str>,
    show_more: bool,
    skip_lines: u16,
    collapsed_with_children: bool,
) {
    let value_style = match kind {
        "string" => json.string.to_style(palette),
        "number" => json.number.to_style(palette),
        "bool" | "null" => json.bool_null.to_style(palette),
        _ => json.container.to_style(palette),
    };
    let key_style = json.key.to_style(palette);
    let available = text_area.width as usize;

    let make_line = |source: &str| -> Line<'static> {
        if let Some((key_part, rest)) = split_at_colon(source) {
            Line::from(vec![
                Span::styled(key_part.to_owned(), key_style),
                Span::styled(":".to_owned(), PREFIX_STYLE),
                Span::styled(rest.to_owned(), value_style),
            ])
        } else {
            Line::from(Span::styled(source.to_owned(), value_style))
        }
    };

    if !show_more {
        let source = brief.unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
        let more_lines = brief.is_none() && display_text.lines().nth(1).is_some();
        let (clipped, truncated) = clip_brief(source, available);
        let line = make_line(clipped);
        render_brief_line(
            text_area,
            buf,
            line,
            truncated || more_lines,
            collapsed_with_children,
            skip_lines,
        );
    } else if kind == "string" {
        // First line is "key:" (split on ':' for coloring); remaining lines are
        // raw value content and must NOT be split again — they may contain ':'.
        let mut text_lines = display_text.lines();
        let mut lines: Vec<Line> = Vec::new();
        if let Some(first) = text_lines.next() {
            lines.push(make_line(first));
        }
        for rest in text_lines {
            lines.push(Line::from(Span::styled(rest.to_owned(), value_style)));
        }
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((skip_lines, 0))
            .render(text_area, buf);
    } else {
        let lines: Vec<Line> = display_text.lines().map(make_line).collect();
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((skip_lines, 0))
            .render(text_area, buf);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_tool_call(
    text_area: Rect,
    buf: &mut Buffer,
    display_text: &str,
    tc: &ToolCallStyle,
    palette: &Palette,
    brief: Option<&str>,
    show_more: bool,
    skip_lines: u16,
    collapsed_with_children: bool,
) {
    let name_style = tc.name.to_style(palette);
    let params_style = tc.params.to_style(palette);
    let available = text_area.width as usize;

    if !show_more {
        let source = brief.unwrap_or_else(|| display_text.lines().next().unwrap_or(""));

        if tc.show_params_in_brief {
            if let Some((name_str, sep, params_str)) = split_at_first_sep(source) {
                let (open, close): (&'static str, Option<&'static str>) = if sep == '(' {
                    ("(", Some(")"))
                } else {
                    (": ", None)
                };
                let params_display = if sep == ':' {
                    params_str.trim_start()
                } else {
                    params_str
                };
                let name_chars = visual_width(name_str);
                let overhead = name_chars + open.len() + close.map_or(0, |s| s.len());
                let params_chars = visual_width(params_display);

                if overhead + params_chars <= available {
                    let mut spans = vec![
                        Span::styled(name_str.to_owned(), name_style),
                        Span::styled(open, params_style),
                        Span::styled(params_display.to_owned(), params_style),
                    ];
                    if let Some(c) = close {
                        spans.push(Span::styled(c, params_style));
                    }
                    render_brief_line(
                        text_area,
                        buf,
                        Line::from(spans),
                        false,
                        collapsed_with_children,
                        skip_lines,
                    );
                } else {
                    let params_available = available.saturating_sub(overhead + 1);
                    let clipped = clip_to_visual_width(params_display, params_available).0;
                    let mut spans = vec![
                        Span::styled(name_str.to_owned(), name_style),
                        Span::styled(open, params_style),
                        Span::styled(clipped.to_owned(), params_style),
                        Span::styled("…", Style::new().dim()),
                    ];
                    if let Some(c) = close {
                        spans.push(Span::styled(c, params_style));
                    }
                    render_brief_line(text_area, buf, Line::from(spans), false, false, skip_lines);
                }
            } else {
                // Plain text fallback (no separator found).
                let (clipped, truncated) = clip_brief(source, available);
                let line = Line::from(Span::styled(clipped.to_owned(), name_style));
                render_brief_line(
                    text_area,
                    buf,
                    line,
                    truncated,
                    collapsed_with_children,
                    skip_lines,
                );
            }
        } else {
            // Only show the tool name.
            let truncated = visual_width(source) > available;
            let take = if truncated {
                available.saturating_sub(1)
            } else {
                available
            };
            let name_src = split_at_first_sep(source).map_or(source, |(n, _, _)| n);
            let max_chars = match tc.name_max_width {
                Some(max) => (max as usize).min(take),
                None => take,
            };
            let clipped_name = clip_to_visual_width(name_src, max_chars).0;
            let line = Line::from(Span::styled(clipped_name.to_owned(), name_style));
            render_brief_line(
                text_area,
                buf,
                line,
                truncated,
                collapsed_with_children,
                skip_lines,
            );
        }
    } else {
        let mut lines: Vec<Line> = Vec::new();
        let mut text_lines = display_text.lines();

        // First line: name and params with distinct styles.
        let first_line = text_lines.next().unwrap_or("");
        if let Some((name_part, sep, params_str)) = split_at_first_sep(first_line) {
            let name_display = match tc.name_max_width {
                Some(max) => name_part.chars().take(max as usize).collect::<String>(),
                None => name_part.to_owned(),
            };
            let (open, close): (&'static str, Option<&'static str>) = if sep == '(' {
                ("(", Some(")"))
            } else {
                (": ", None)
            };
            let params_display = if sep == ':' {
                params_str.trim_start()
            } else {
                params_str
            };
            let mut spans = vec![
                Span::styled(name_display, name_style),
                Span::styled(open, params_style),
                Span::styled(params_display.to_owned(), params_style),
            ];
            if let Some(c) = close {
                spans.push(Span::styled(c, params_style));
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(Span::styled(first_line.to_owned(), name_style)));
        }

        // Remaining lines (individual key=value props) use params style.
        for rest in text_lines {
            lines.push(Line::from(Span::styled(rest.to_owned(), params_style)));
        }

        if collapsed_with_children && let Some(line) = lines.last_mut() {
            line.spans.push(Span::styled("▾", Style::new().dim()));
        }

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((skip_lines, 0))
            .render(text_area, buf);
    }
}
