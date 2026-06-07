use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use tui_markdown::{Options, StyleSheet, from_str_with_options};

use crate::theme::{ColorVar, Palette};

#[derive(Clone)]
struct PaletteStyleSheet {
    primary: Color,
    accent: Color,
    accent_light: Color,
    muted: Color,
}

impl PaletteStyleSheet {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            primary: palette.resolve(&ColorVar::Primary),
            accent: palette.resolve(&ColorVar::Accent),
            accent_light: palette.resolve(&ColorVar::AccentLight),
            muted: palette.resolve(&ColorVar::Muted),
        }
    }
}

impl StyleSheet for PaletteStyleSheet {
    fn heading(&self, level: u8) -> Style {
        let base = Style::new().bold();
        match level {
            1 => base.fg(self.accent).underlined(),
            2 => base.fg(self.accent),
            3 => base.fg(self.accent_light),
            _ => base.fg(self.muted),
        }
    }

    fn code(&self) -> Style {
        Style::new().fg(self.accent_light)
    }

    fn link(&self) -> Style {
        Style::new().fg(self.accent).underlined()
    }

    fn blockquote(&self) -> Style {
        Style::new().fg(self.primary).italic()
    }

    fn heading_meta(&self) -> Style {
        Style::new().fg(self.muted)
    }

    fn metadata_block(&self) -> Style {
        Style::new().fg(self.muted)
    }
}

pub fn render_markdown<'a>(text: &'a str, palette: &Palette) -> Text<'a> {
    let ss = PaletteStyleSheet::from_palette(palette);
    from_str_with_options(text, &Options::new(ss))
}

/// Clips the first line of `text` to `available` columns.
///
/// Returns the clipped `Line` (with owned content so it has `'static` lifetime)
/// and a bool indicating whether truncation occurred.
pub fn first_line_clipped(text: &Text<'_>, available: u16, muted: Color) -> (Line<'static>, bool) {
    let available = available as usize;

    if available == 0 {
        return (Line::default(), false);
    }

    let first_line = match text.lines.first() {
        Some(l) => l,
        None => return (Line::default(), false),
    };

    let mut result_spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;

    for span in &first_line.spans {
        let span_chars = span.content.chars().count();
        if used + span_chars <= available {
            result_spans.push(Span::styled(span.content.to_string(), span.style));
            used += span_chars;
        } else {
            // Span overflows: take as many chars as fit, leaving room for the ellipsis.
            let take = available.saturating_sub(used).saturating_sub(1);
            if take > 0 {
                let partial: String = span.content.chars().take(take).collect();
                result_spans.push(Span::styled(partial, span.style));
            }
            truncated = true;
            break;
        }
    }

    if truncated {
        result_spans.push(Span::styled("…", Style::new().fg(muted)));
    }

    (Line::from(result_spans), truncated)
}
