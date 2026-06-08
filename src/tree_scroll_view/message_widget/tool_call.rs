use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::styles::MessageStyle;

use super::super::ansi::{clip_to_visual_width, visual_width};
use super::super::state::MessageState;
use super::brief::{clip_brief, render_brief_line};
use super::component::{ContentRenderContext, MessageComponent};

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

// ── ToolCallComponent ─────────────────────────────────────────────────────────

pub(super) struct ToolCallComponent<'a> {
    pub message: &'a mut MessageState,
}

impl<'a> MessageComponent for ToolCallComponent<'a> {
    fn message_mut(&mut self) -> &mut MessageState {
        self.message
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer, ctx: &ContentRenderContext<'_>) {
        let MessageStyle::ToolCall(tc) = ctx.style else {
            return;
        };

        let display_text = self.message.text.as_deref().unwrap_or("");
        let collapsed_with_children = !self.message.expanded && !self.message.children.is_empty();

        let name_style = tc.name.to_style(ctx.palette);
        let params_style = tc.params.to_style(ctx.palette);
        let available = area.width as usize;

        if !self.message.show_more {
            let source = self
                .message
                .brief
                .as_deref()
                .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));

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
                            area,
                            buf,
                            Line::from(spans),
                            false,
                            collapsed_with_children,
                            ctx.skip_lines,
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
                        render_brief_line(
                            area,
                            buf,
                            Line::from(spans),
                            false,
                            false,
                            ctx.skip_lines,
                        );
                    }
                } else {
                    // Plain text fallback (no separator found).
                    let (clipped, truncated) = clip_brief(source, available);
                    let line = Line::from(Span::styled(clipped.to_owned(), name_style));
                    render_brief_line(
                        area,
                        buf,
                        line,
                        truncated,
                        collapsed_with_children,
                        ctx.skip_lines,
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
                    area,
                    buf,
                    line,
                    truncated,
                    collapsed_with_children,
                    ctx.skip_lines,
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
                .scroll((ctx.skip_lines, 0))
                .render(area, buf);
        }
    }
}
