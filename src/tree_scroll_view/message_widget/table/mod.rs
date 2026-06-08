mod handler;
pub mod render;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::super::markdown::render_markdown;
use super::super::state::{MessageState, measure_text_height};
use super::component::{
    ComponentKeyResult, ComponentState, ContentRenderContext, MessageComponent,
};
use crate::clipboard::markdown_to_plain;
use crate::theme::Palette;

pub const CELL_PADDING: u16 = 1;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TableData {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Per-node state for a table widget. Holds both the parsed data and the
/// mutable interaction/layout state. `col_widths` is lazily initialized on
/// first render; until then it is empty.
#[derive(Debug, Clone)]
pub struct TableState {
    pub data: TableData,
    /// None = header row selected; Some(r) = data row r (0-indexed).
    pub selected_row: Option<usize>,
    pub selected_col: usize,
    /// Column widths including padding. Empty until first layout pass.
    pub col_widths: Vec<u16>,
    /// Index of the first visible column (column-level horizontal scroll).
    pub scroll_x: u16,
    /// Available viewport width (columns) for the table area; set each layout pass.
    pub viewport_width: u16,
    /// True once the user has manually resized any column; suppresses auto-relayout on resize.
    pub user_resized: bool,
    /// True when `y` has been pressed and we are waiting for the second key (yy/yt).
    pending_y: bool,
}

impl TableState {
    pub fn new(data: TableData) -> Self {
        Self {
            data,
            selected_row: None,
            selected_col: 0,
            col_widths: vec![],
            scroll_x: 0,
            viewport_width: 0,
            user_resized: false,
            pending_y: false,
        }
    }

    fn selected_cell_text(&self) -> String {
        match self.selected_row {
            None => self
                .data
                .headers
                .get(self.selected_col)
                .cloned()
                .unwrap_or_default(),
            Some(row) => self
                .data
                .rows
                .get(row)
                .and_then(|r| r.get(self.selected_col))
                .cloned()
                .unwrap_or_default(),
        }
    }

    pub fn apply_move(&mut self, row_delta: i32, col_delta: i32) {
        let num_cols = self.data.headers.len();
        let num_rows = self.data.rows.len();
        if num_cols == 0 {
            return;
        }

        let new_col = (self.selected_col as i32 + col_delta).clamp(0, num_cols as i32 - 1) as usize;
        self.selected_col = new_col;

        // selected_row: None=0 header, Some(r)=r+1
        let display_row = self.selected_row.map(|r| r as i32 + 1).unwrap_or(0);
        let new_dr = (display_row + row_delta).clamp(0, num_rows as i32);
        self.selected_row = if new_dr == 0 {
            None
        } else {
            Some(new_dr as usize - 1)
        };

        // Scroll to keep selected column visible.
        self.clamp_scroll();
    }

    pub fn apply_resize(&mut self, col: usize, delta: i16) {
        if col >= self.col_widths.len() {
            return;
        }
        let min_width = 5 + 2 * CELL_PADDING;
        let new_w = (self.col_widths[col] as i16 + delta).max(min_width as i16) as u16;
        self.col_widths[col] = new_w;
        self.user_resized = true;
        self.clamp_scroll();
    }

    /// Returns `(top_line, bottom_line)` — the half-open line range of the selected row
    /// within the table's rendered area (line 0 = table top border). Returns `None` if
    /// col_widths are not yet initialized.
    pub fn selected_row_line_range(&self, palette: &Palette) -> Option<(u16, u16)> {
        if self.col_widths.is_empty() {
            return None;
        }
        let display_row = self.selected_row.map(|r| r + 1).unwrap_or(0);
        let num_display_rows = self.data.rows.len() + 1;
        // Compute per-row heights up through display_row.
        let mut row_top: u16 = 0;
        for r in 0..display_row {
            let cells: &[String] = if r == 0 {
                &self.data.headers
            } else {
                &self.data.rows[r - 1]
            };
            row_top += row_render_height(cells, &self.col_widths, palette) + 1;
        }
        let sel_cells: &[String] = if display_row == 0 {
            &self.data.headers
        } else if display_row < num_display_rows {
            &self.data.rows[display_row - 1]
        } else {
            return None;
        };
        let row_h = row_render_height(sel_cells, &self.col_widths, palette) + 2;
        Some((row_top, row_top + row_h))
    }

    /// Adjust scroll_x so selected_col is visible.
    fn clamp_scroll(&mut self) {
        if self.col_widths.is_empty() {
            return;
        }
        let sel = self.selected_col;

        // Left bound: scroll_x can't be past selected_col.
        self.scroll_x = self.scroll_x.min(sel as u16);

        // Right bound: advance scroll_x until selected_col fits within viewport_width.
        // Width of columns [scroll_x..=sel] mirrors render.rs table_w formula:
        //   first col: width + 2; each additional col: width + 1.
        if self.viewport_width > 0 {
            // Walk left from sel to find the leftmost column that can be scroll_x
            // while keeping sel visible.
            let mut accum = self.col_widths[sel] + 2;
            let mut first_visible = sel;
            while first_visible > 0 {
                let prev = first_visible - 1;
                let extra = self.col_widths[prev] + 1;
                if accum + extra > self.viewport_width {
                    break;
                }
                accum += extra;
                first_visible = prev;
            }
            self.scroll_x = self.scroll_x.max(first_visible as u16);
        }
    }
}

impl ComponentState for TableState {
    fn clone_box(&self) -> Box<dyn ComponentState> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn on_update(
        &self,
        new_message: &super::super::state::MessageState,
    ) -> Option<Box<dyn ComponentState>> {
        let new_ts = new_message
            .ui_state
            .as_ref()?
            .as_any()
            .downcast_ref::<TableState>()?;
        if new_ts.data.headers.len() != self.data.headers.len() {
            return Some(Box::new(TableState::new(new_ts.data.clone())));
        }
        Some(Box::new(TableState {
            data: new_ts.data.clone(),
            col_widths: self.col_widths.clone(),
            selected_col: self
                .selected_col
                .min(new_ts.data.headers.len().saturating_sub(1)),
            selected_row: self.selected_row.filter(|&r| r < new_ts.data.rows.len()),
            scroll_x: self.scroll_x,
            viewport_width: self.viewport_width,
            user_resized: self.user_resized,
            pending_y: false,
        }))
    }

    fn supports_interaction(&self) -> bool {
        true
    }
}

// ── TableComponent ────────────────────────────────────────────────────────────

pub struct TableComponent<'a> {
    pub message: &'a mut MessageState,
}

impl<'a> TableComponent<'a> {
    fn state(&self) -> &TableState {
        self.message
            .ui_state
            .as_ref()
            .unwrap()
            .as_any()
            .downcast_ref::<TableState>()
            .unwrap()
    }

    fn state_mut(&mut self) -> &mut TableState {
        self.message
            .ui_state
            .as_mut()
            .unwrap()
            .as_any_mut()
            .downcast_mut::<TableState>()
            .unwrap()
    }
}

impl<'a> MessageComponent for TableComponent<'a> {
    fn message_mut(&mut self) -> &mut MessageState {
        self.message
    }

    fn handle_key(&mut self, key: KeyEvent) -> ComponentKeyResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ts = self.state_mut();

        if ts.pending_y {
            ts.pending_y = false;
            return match key.code {
                KeyCode::Char('y') => ComponentKeyResult::Copy {
                    content: ts.selected_cell_text(),
                },
                KeyCode::Char('t') => ComponentKeyResult::Copy {
                    content: markdown_to_plain(&ts.selected_cell_text()),
                },
                _ => ComponentKeyResult::Unhandled,
            };
        }

        match key.code {
            KeyCode::Esc => ComponentKeyResult::ExitInteraction,
            KeyCode::Char('c') if ctrl => ComponentKeyResult::ExitInteraction,
            KeyCode::Char('n') | KeyCode::Char('p') if ctrl => ComponentKeyResult::Passthrough,
            KeyCode::Char('d') | KeyCode::Char('u') if ctrl => ComponentKeyResult::Passthrough,
            KeyCode::PageDown | KeyCode::PageUp => ComponentKeyResult::Passthrough,
            KeyCode::Char('Y') => ComponentKeyResult::Copy {
                content: ts.selected_cell_text(),
            },
            KeyCode::Char('y') => {
                ts.pending_y = true;
                ComponentKeyResult::Consumed {
                    invalidates_height: false,
                }
            }
            _ => {
                let sel_col = ts.selected_col;
                match handler::handle_table_key(key, sel_col) {
                    Some(handler::TableAction::MoveSelection {
                        row_delta,
                        col_delta,
                    }) => {
                        ts.apply_move(row_delta, col_delta);
                        ComponentKeyResult::Consumed {
                            invalidates_height: false,
                        }
                    }
                    Some(handler::TableAction::ResizeCol { col, delta }) => {
                        ts.apply_resize(col, delta);
                        ComponentKeyResult::Consumed {
                            invalidates_height: true,
                        }
                    }
                    Some(handler::TableAction::ResetLayout) => {
                        ts.col_widths.clear();
                        ts.user_resized = false;
                        ComponentKeyResult::Consumed {
                            invalidates_height: true,
                        }
                    }
                    None => ComponentKeyResult::Unhandled,
                }
            }
        }
    }

    fn focused_line_range(&self, palette: &Palette) -> Option<(u16, u16)> {
        self.state().selected_row_line_range(palette)
    }

    fn on_viewport_width_changed(&mut self) {
        let ts = self.state_mut();
        if !ts.user_resized {
            ts.col_widths.clear();
        }
    }

    fn layout_pass(&mut self, available_width: u16, palette: &Palette) -> Option<u16> {
        let ts = self.state_mut();
        if ts.col_widths.is_empty() {
            ts.col_widths = compute_col_widths(&ts.data, available_width, palette);
        }
        ts.viewport_width = available_width;
        Some(compute_table_height(ts, palette))
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer, ctx: &ContentRenderContext<'_>) {
        use crate::theme::styles::MessageStyle;
        use ratatui::text::{Line, Span};
        let MessageStyle::Table(table_style) = ctx.style else {
            return;
        };
        if self.message.show_more {
            let ts = self.state();
            render::render_table(area, ts, table_style, buf, ctx);
        } else {
            let source = self.message.brief.as_deref().unwrap_or("Table");
            let (clipped, truncated) = super::brief::clip_brief(source, area.width as usize);
            let line = Line::from(Span::styled(
                clipped,
                table_style.cell.to_style(ctx.palette),
            ));
            super::brief::render_brief_line(area, buf, line, truncated, false, ctx.skip_lines);
        }
    }
}

// ── Layout ────────────────────────────────────────────────────────────────────

/// Natural (unpadded) display width of a cell: render as markdown, then take
/// the max `Line::width()` over all rendered lines.
fn natural_cell_width_md(text: &str, palette: &Palette) -> u16 {
    let rendered = render_markdown(text, palette);
    rendered
        .lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(0)
}

/// Compute initial column widths (including padding) from data, fitting within
/// `available_width` terminal columns.
pub fn compute_col_widths(data: &TableData, available_width: u16, palette: &Palette) -> Vec<u16> {
    let num_cols = data.headers.len();
    if num_cols == 0 {
        return vec![];
    }

    // Natural content widths (unpadded), measured on rendered markdown output.
    let mut natural: Vec<u16> = vec![0; num_cols];
    for (i, h) in data.headers.iter().enumerate() {
        natural[i] = natural[i].max(natural_cell_width_md(h, palette));
    }
    for row in &data.rows {
        for (i, cell) in row.iter().enumerate().take(num_cols) {
            natural[i] = natural[i].max(natural_cell_width_md(cell, palette));
        }
    }

    // Padded widths.
    let padded: Vec<u16> = natural.iter().map(|&n| n + 2 * CELL_PADDING).collect();
    let total_padded: u16 = padded.iter().sum::<u16>() + num_cols as u16 + 1; // +borders

    if total_padded <= available_width {
        return padded;
    }

    // Proportionally shrink to fit available_width.
    let border_overhead = num_cols as u16 + 1;
    let budget = available_width.saturating_sub(border_overhead);
    let total_natural: u16 = natural.iter().sum();
    let min_width = 5 + 2 * CELL_PADDING;

    if total_natural == 0 {
        return vec![min_width; num_cols];
    }

    natural
        .iter()
        .map(|&n| {
            let w = (budget as u32 * n as u32 / total_natural as u32) as u16;
            w.max(min_width)
        })
        .collect()
}

/// Compute rendered height for a single table row (0=header, 1..=n=data rows).
pub fn row_render_height(cells: &[String], col_widths: &[u16], palette: &Palette) -> u16 {
    cells
        .iter()
        .zip(col_widths.iter())
        .map(|(cell, &w)| {
            let content_w = w.saturating_sub(2 * CELL_PADDING);
            let rendered = render_markdown(cell, palette);
            measure_text_height(&rendered, content_w).max(1)
        })
        .max()
        .unwrap_or(1)
}

/// Total rendered height of the table grid including all borders and padding row.
pub fn compute_table_height(state: &TableState, palette: &Palette) -> u16 {
    if state.col_widths.is_empty() {
        return 3; // placeholder: top border + 1 row + bottom border
    }
    let num_display_rows = state.data.rows.len() + 1; // header + data rows
    let mut total = 0u16;
    // Top border + each row (separator + content height) + bottom padding
    total += 1; // top border
    for r in 0..num_display_rows {
        let cells: &[String] = if r == 0 {
            &state.data.headers
        } else {
            &state.data.rows[r - 1]
        };
        total += row_render_height(cells, &state.col_widths, palette);
        total += 1; // separator after row (or bottom border for last)
    }
    total + 1 // +1 bottom padding
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::theme::Theme;
    use crate::tree_scroll_view::state::MessageState;

    fn make_message() -> MessageState {
        let ts = TableState::new(TableData {
            headers: vec!["A".into(), "B".into()],
            rows: vec![
                vec!["a1".into(), "b1".into()],
                vec!["a2".into(), "b2".into()],
            ],
        });
        MessageState::new("test").ui_state(Box::new(ts))
    }

    fn state(msg: &MessageState) -> &TableState {
        msg.ui_state
            .as_ref()
            .unwrap()
            .as_any()
            .downcast_ref::<TableState>()
            .unwrap()
    }

    fn state_mut(msg: &mut MessageState) -> &mut TableState {
        msg.ui_state
            .as_mut()
            .unwrap()
            .as_any_mut()
            .downcast_mut::<TableState>()
            .unwrap()
    }

    // crossterm's KeyEvent::new defaults to KeyEventKind::Press.
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn natural_width_strips_markdown_syntax() {
        let palette = Theme::default_dark().palette;
        // Markdown syntax removed: "**bold**" renders as "bold" (4), not 8
        assert_eq!(natural_cell_width_md("**bold**", &palette), 4);
        // "`code`" renders as "code" (4), not 6
        assert_eq!(natural_cell_width_md("`code`", &palette), 4);
        // Plain text unchanged
        assert_eq!(natural_cell_width_md("hello", &palette), 5);
        // Soft break (\n in a cell) becomes a space → "hi longer" on one line = 9
        assert_eq!(natural_cell_width_md("**hi**\nlonger", &palette), 9);
    }

    // ── MessageComponent::handle_key ─────────────────────────────────────────

    #[test]
    fn handle_key_row_down_consumed_no_height_change() {
        let mut msg = make_message();
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Down));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: false
            }
        ));
        assert_eq!(state(&msg).selected_row, Some(0));
    }

    #[test]
    fn handle_key_row_up_consumed_no_height_change() {
        let mut msg = make_message();
        state_mut(&mut msg).selected_row = Some(1);
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Up));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: false
            }
        ));
        assert_eq!(state(&msg).selected_row, Some(0));
    }

    #[test]
    fn handle_key_col_right_consumed_no_height_change() {
        let mut msg = make_message();
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Right));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: false
            }
        ));
        assert_eq!(state(&msg).selected_col, 1);
    }

    #[test]
    fn handle_key_col_left_at_boundary_consumed_no_height_change() {
        let mut msg = make_message();
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Left));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: false
            }
        ));
        assert_eq!(state(&msg).selected_col, 0); // clamped
    }

    #[test]
    fn handle_key_vim_hjkl_navigation() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press(KeyCode::Char('j'))),
            ComponentKeyResult::Consumed {
                invalidates_height: false
            }
        ));
        assert_eq!(state(&msg).selected_row, Some(0));
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press(KeyCode::Char('l'))),
            ComponentKeyResult::Consumed {
                invalidates_height: false
            }
        ));
        assert_eq!(state(&msg).selected_col, 1);
    }

    #[test]
    fn handle_key_resize_plus_invalidates_height() {
        let mut msg = make_message();
        state_mut(&mut msg).col_widths = vec![10, 10];
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Char('+')));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: true
            }
        ));
        assert!(state(&msg).user_resized);
    }

    #[test]
    fn handle_key_resize_minus_invalidates_height() {
        let mut msg = make_message();
        state_mut(&mut msg).col_widths = vec![20, 20];
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Char('-')));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: true
            }
        ));
    }

    #[test]
    fn handle_key_reset_layout_invalidates_height() {
        let mut msg = make_message();
        state_mut(&mut msg).col_widths = vec![10, 10];
        state_mut(&mut msg).user_resized = true;
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Char('0')));
        assert!(matches!(
            r,
            ComponentKeyResult::Consumed {
                invalidates_height: true
            }
        ));
        assert!(state(&msg).col_widths.is_empty());
        assert!(!state(&msg).user_resized);
    }

    #[test]
    fn handle_key_unrecognised_returns_unhandled() {
        let mut msg = make_message();
        let r = TableComponent { message: &mut msg }.handle_key(press(KeyCode::Char('x')));
        assert!(matches!(r, ComponentKeyResult::Unhandled));
    }

    // ── Lifecycle and scroll keys ─────────────────────────────────────────────

    #[test]
    fn handle_key_esc_exits_interaction() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press(KeyCode::Esc)),
            ComponentKeyResult::ExitInteraction
        ));
    }

    #[test]
    fn handle_key_ctrl_c_exits_interaction() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press_ctrl(KeyCode::Char('c'))),
            ComponentKeyResult::ExitInteraction
        ));
    }

    #[test]
    fn handle_key_ctrl_n_passthrough() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press_ctrl(KeyCode::Char('n'))),
            ComponentKeyResult::Passthrough
        ));
    }

    #[test]
    fn handle_key_ctrl_p_passthrough() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press_ctrl(KeyCode::Char('p'))),
            ComponentKeyResult::Passthrough
        ));
    }

    #[test]
    fn handle_key_page_down_passthrough() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press(KeyCode::PageDown)),
            ComponentKeyResult::Passthrough
        ));
    }

    #[test]
    fn handle_key_page_up_passthrough() {
        let mut msg = make_message();
        assert!(matches!(
            TableComponent { message: &mut msg }.handle_key(press(KeyCode::PageUp)),
            ComponentKeyResult::Passthrough
        ));
    }

    // ── MessageComponent::on_viewport_width_changed ───────────────────────────

    #[test]
    fn on_viewport_width_changed_clears_when_not_user_resized() {
        let mut msg = make_message();
        state_mut(&mut msg).col_widths = vec![10, 20];
        state_mut(&mut msg).user_resized = false;
        TableComponent { message: &mut msg }.on_viewport_width_changed();
        assert!(state(&msg).col_widths.is_empty());
    }

    #[test]
    fn on_viewport_width_changed_preserves_when_user_resized() {
        let mut msg = make_message();
        state_mut(&mut msg).col_widths = vec![10, 20];
        state_mut(&mut msg).user_resized = true;
        TableComponent { message: &mut msg }.on_viewport_width_changed();
        assert_eq!(state(&msg).col_widths, vec![10, 20]);
    }
}
