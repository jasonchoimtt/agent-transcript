use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::ColorVar;
use crate::theme::styles::MessageStyle;

use super::super::markdown::{first_line_clipped, render_markdown};
use super::super::state::{MessageState, highlight_text_spans};
use super::brief::{clip_brief, render_brief_line};
use super::component::{ContentRenderContext, MessageComponent};

// ── ProseComponent ────────────────────────────────────────────────────────────

pub(super) struct ProseComponent<'a> {
    pub message: &'a mut MessageState,
}

impl<'a> MessageComponent for ProseComponent<'a> {
    fn message_mut(&mut self) -> &mut MessageState {
        self.message
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer, ctx: &ContentRenderContext<'_>) {
        let xml_tag = self.message.tag.as_deref();
        let (content_style, is_markdown) = match ctx.style {
            MessageStyle::UserMessage(s) => {
                let ts = s.resolve(xml_tag);
                (ts.content.to_style(ctx.palette), ts.uses_markdown)
            }
            MessageStyle::AgentMessage(s) => (s.content.to_style(ctx.palette), s.uses_markdown),
            MessageStyle::Thinking(s) => (s.content.to_style(ctx.palette), false),
            MessageStyle::TaskSummary(s) => (s.content.to_style(ctx.palette), false),
            MessageStyle::Container(s) => (s.resolve(xml_tag).content.to_style(ctx.palette), false),
            MessageStyle::System(s) => (s.content.to_style(ctx.palette), false),
            MessageStyle::Other(s) => (s.resolve(xml_tag).content.to_style(ctx.palette), false),
            _ => (Style::default(), false),
        };

        let display_text = self.message.text.as_deref().unwrap_or("");
        let collapsed_with_children = !self.message.expanded && !self.message.children.is_empty();

        if !self.message.show_more {
            if is_markdown && self.message.brief.is_none() {
                // Render markdown and clip the first line for compact display.
                let rendered = render_markdown(display_text, ctx.palette);
                let has_more_lines = rendered.lines.len() > 1;
                let muted = ctx.palette.resolve(&ColorVar::Muted);
                let (mut line, inline_clipped) = first_line_clipped(&rendered, area.width, muted);
                if has_more_lines && !inline_clipped {
                    line.spans.push(Span::styled("…", Style::new().fg(muted)));
                } else if collapsed_with_children {
                    line.spans.push(Span::styled("▾", Style::new().fg(muted)));
                }
                Paragraph::new(vec![line])
                    .scroll((ctx.skip_lines, 0))
                    .render(area, buf);
            } else {
                let source = self
                    .message
                    .brief
                    .as_deref()
                    .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
                let more_lines =
                    self.message.brief.is_none() && display_text.lines().nth(1).is_some();
                let (clipped, truncated) = clip_brief(source, area.width as usize);
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
                    area,
                    buf,
                    line,
                    truncated || more_lines,
                    collapsed_with_children,
                    ctx.skip_lines,
                );
            }
        } else if is_markdown {
            let rendered = render_markdown(display_text, ctx.palette);
            let text: Text<'_> = if let Some(hl) = ctx.highlight {
                highlight_text_spans(rendered, hl.char_index, hl.query_len)
            } else {
                rendered
            };
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((ctx.skip_lines, 0))
                .render(area, buf);
        } else {
            let mut text = display_text
                .into_text()
                .unwrap_or_else(|_| Text::raw(display_text));
            text.style = content_style;
            let text: Text<'_> = if let Some(hl) = ctx.highlight {
                let mut highlighted = highlight_text_spans(text, hl.char_index, hl.query_len);
                highlighted.style = content_style;
                highlighted
            } else {
                text
            };
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((ctx.skip_lines, 0))
                .render(area, buf);
        }
    }
}
