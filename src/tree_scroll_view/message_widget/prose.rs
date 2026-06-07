use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::{ColorVar, Palette};

use super::super::markdown::{first_line_clipped, render_markdown};
use super::super::state::MessageState;
use super::brief::{clip_brief, render_brief_line};

pub(super) fn render_prose(
    text_area: Rect,
    buf: &mut Buffer,
    node: &MessageState,
    content_style: Style,
    is_markdown: bool,
    skip_lines: u16,
    palette: &Palette,
) {
    let display_text = node.text.as_deref().unwrap_or("");
    let collapsed_with_children = !node.expanded && !node.children.is_empty();

    if !node.show_more {
        if is_markdown && node.brief.is_none() {
            // Render markdown and clip the first line for compact display.
            let rendered = render_markdown(display_text, palette);
            let has_more_lines = rendered.lines.len() > 1;
            let muted = palette.resolve(&ColorVar::Muted);
            let (mut line, inline_clipped) = first_line_clipped(&rendered, text_area.width, muted);
            if has_more_lines && !inline_clipped {
                line.spans.push(Span::styled("…", Style::new().fg(muted)));
            } else if collapsed_with_children {
                line.spans.push(Span::styled("▾", Style::new().fg(muted)));
            }
            Paragraph::new(vec![line])
                .scroll((skip_lines, 0))
                .render(text_area, buf);
        } else {
            let source = node
                .brief
                .as_deref()
                .unwrap_or_else(|| display_text.lines().next().unwrap_or(""));
            let more_lines = node.brief.is_none() && display_text.lines().nth(1).is_some();
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
                .unwrap_or_else(|_| Line::from(Span::styled(clipped.to_string(), content_style)));
            render_brief_line(
                text_area,
                buf,
                line,
                truncated || more_lines,
                collapsed_with_children,
                skip_lines,
            );
        }
    } else if is_markdown {
        let rendered = render_markdown(display_text, palette);
        Paragraph::new(rendered)
            .wrap(Wrap { trim: false })
            .scroll((skip_lines, 0))
            .render(text_area, buf);
    } else {
        let mut text = display_text
            .into_text()
            .unwrap_or_else(|_| Text::raw(display_text));
        text.style = content_style;
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((skip_lines, 0))
            .render(text_area, buf);
    }
}
