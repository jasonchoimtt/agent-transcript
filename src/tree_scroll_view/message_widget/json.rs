use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::styles::MessageStyle;

use super::super::state::MessageState;
use super::brief::{clip_brief, render_brief_line};
use super::component::{ContentRenderContext, MessageComponent};

const PREFIX_STYLE: Style = Style::new().fg(Color::DarkGray);

fn split_at_colon(s: &str) -> Option<(&str, &str)> {
    s.find(':').map(|i| (&s[..i], &s[i + 1..]))
}

// ── JsonComponent ─────────────────────────────────────────────────────────────

pub(super) struct JsonComponent<'a> {
    pub message: &'a mut MessageState,
}

impl<'a> MessageComponent for JsonComponent<'a> {
    fn message_mut(&mut self) -> &mut MessageState {
        self.message
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer, ctx: &ContentRenderContext<'_>) {
        let MessageStyle::Json(json) = ctx.style else {
            return;
        };

        let display_text = self.message.text.as_deref().unwrap_or("");
        let kind = self.message.data.as_str();
        let collapsed_with_children = !self.message.expanded && !self.message.children.is_empty();

        let value_style = match kind {
            "string" => json.string.to_style(ctx.palette),
            "number" => json.number.to_style(ctx.palette),
            "bool" | "null" => json.bool_null.to_style(ctx.palette),
            _ => json.container.to_style(ctx.palette),
        };
        let key_style = json.key.to_style(ctx.palette);
        let available = area.width as usize;

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

        if !self.message.show_more {
            let source = self
                .message
                .brief
                .as_deref()
                .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
            let more_lines = self.message.brief.is_none() && display_text.lines().nth(1).is_some();
            let (clipped, truncated) = clip_brief(source, available);
            let line = make_line(clipped);
            render_brief_line(
                area,
                buf,
                line,
                truncated || more_lines,
                collapsed_with_children,
                ctx.skip_lines,
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
                .scroll((ctx.skip_lines, 0))
                .render(area, buf);
        } else {
            let lines: Vec<Line> = display_text.lines().map(make_line).collect();
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((ctx.skip_lines, 0))
                .render(area, buf);
        }
    }
}
