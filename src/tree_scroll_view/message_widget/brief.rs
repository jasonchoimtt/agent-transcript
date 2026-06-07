use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::super::ansi::{clip_to_visual_width, visual_width};

pub(super) fn clip_brief(source: &str, available: usize) -> (&str, bool) {
    if visual_width(source) > available {
        (
            clip_to_visual_width(source, available.saturating_sub(1)).0,
            true,
        )
    } else {
        (source, false)
    }
}

pub(super) fn render_brief_line<'a>(
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
