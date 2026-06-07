use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

use super::state::TerminalState;

/// Widget that renders the terminal block inside a scroll view.
///
/// The terminal block layout (cols left-to-right, rows top-to-bottom):
///   Col 0:                         selection gutter (▌ when selected and not active)
///   Cols 1..:
///     Rows 0..scrollback_available  (expanded only) scrollback lines, oldest first
///     Remaining rows                live PTY screen rows
pub struct TerminalWidget<'a> {
    pub state: &'a mut TerminalState,
    pub expanded: bool,
    pub selected: bool,
    /// True when the terminal pane has keyboard focus (PTY receives input).
    /// Gutter is hidden when active so the PTY content fills the full width visually.
    pub active: bool,
    /// First row of the terminal block that maps to the top of `area`.
    pub render_offset: u16,
    pub scrollback_available: u16,
}

impl<'a> TerminalWidget<'a> {
    pub fn new_scroll(
        state: &'a mut TerminalState,
        expanded: bool,
        selected: bool,
        active: bool,
        render_offset: u16,
        scrollback_available: u16,
    ) -> Self {
        Self {
            state,
            expanded,
            selected,
            active,
            render_offset,
            scrollback_available,
        }
    }
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Left gutter column: shown when selected but not active (hides when PTY has focus).
        let gutter_color = if self.selected && !self.active {
            Some(Color::Rgb(34, 139, 34))
        } else {
            None
        };
        for row in 0..area.height {
            if let Some(cell) = buf.cell_mut((area.x, area.y + row)) {
                if let Some(color) = gutter_color {
                    cell.set_symbol("▌").set_fg(color);
                } else {
                    cell.set_symbol(" ").set_style(Style::reset());
                }
            }
        }

        let content_x = area.x + 1;
        let content_width = area.width.saturating_sub(1);
        if content_width == 0 {
            return;
        }
        self.state.resize_cols(content_width);

        let sb = self.scrollback_available;

        for i in 0..area.height {
            let block_row = self.render_offset + i;
            let buf_y = area.y + i;

            if self.expanded && block_row < sb {
                // Scrollback row; 0-indexed from oldest.
                // set_scrollback(k) makes the k newest rows visible; cell(0) is the oldest.
                let scroll_amt = (sb - block_row) as usize;
                render_with_scrollback(
                    self.state,
                    scroll_amt,
                    0,
                    content_x,
                    content_width,
                    buf_y,
                    buf,
                );
            } else {
                // Live PTY row.
                let live_row = if self.expanded {
                    block_row.saturating_sub(sb)
                } else {
                    let base = block_row;
                    if let Some(crop) = self.state.collapsed_crop {
                        base + crop.start_row
                    } else {
                        base
                    }
                };
                if live_row < self.state.rows {
                    render_with_scrollback(
                        self.state,
                        0,
                        live_row,
                        content_x,
                        content_width,
                        buf_y,
                        buf,
                    );
                }
            }
        }

        // Always leave the vt100 view at offset 0 (live screen).
        self.state.parser.screen_mut().set_scrollback(0);
    }
}

/// Set the vt100 scrollback offset, then render one row of the visible screen
/// into `buf` at position (x, y).
fn render_with_scrollback(
    state: &mut TerminalState,
    scrollback_offset: usize,
    screen_row: u16,
    x: u16,
    width: u16,
    y: u16,
    buf: &mut Buffer,
) {
    state.parser.screen_mut().set_scrollback(scrollback_offset);
    let screen = state.parser.screen();
    for col in 0..width {
        let Some(cell) = screen.cell(screen_row, col) else {
            continue;
        };
        let contents = cell.contents();
        let symbol: &str = if contents.is_empty() { " " } else { contents };
        let fg = vt_color(cell.fgcolor());
        let bg = vt_color(cell.bgcolor());
        let mut style = Style::default().fg(fg).bg(bg);
        if cell.bold() {
            style = style.add_modifier(Modifier::BOLD);
        }
        if cell.italic() {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if cell.underline() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if cell.inverse() {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if let Some(buf_cell) = buf.cell_mut((x + col, y)) {
            buf_cell.set_symbol(symbol).set_style(style);
        }
    }
}

fn vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
