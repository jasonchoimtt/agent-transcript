use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect, Spacing};
use ratatui::symbols::merge::MergeStrategy;
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::super::super::markdown::render_markdown;
use super::{CELL_PADDING, TableState, row_render_height};
use crate::theme::styles::TableStyle;
use crate::tree_scroll_view::message_widget::component::ContentRenderContext;

/// Render the table grid into `text_area`, skipping the first `ctx.skip_lines` rows.
///
/// Each cell is a `Block` with `merge_borders(MergeStrategy::Exact)` so junction
/// characters (┼ ├ ┤ ┬ ┴) are resolved automatically from adjacent borders.
///
/// Both axes are computed via `Layout` on virtual rects sized to the table's natural
/// dimensions — this prevents the constraint solver from shrinking any column or row.
/// The row layout uses table-space (y=0 at table top); screen y is recovered via
/// `text_area.y as i32 + table_y as i32 - ctx.skip_lines as i32`.
pub fn render_table(
    text_area: Rect,
    state: &TableState,
    style: &TableStyle,
    buf: &mut Buffer,
    ctx: &ContentRenderContext<'_>,
) {
    if state.col_widths.is_empty() || text_area.height == 0 {
        return;
    }
    let data = &state.data;
    let num_cols = data.headers.len();
    if num_cols == 0 {
        return;
    }

    let col_widths = &state.col_widths;
    let first_col = state.scroll_x as usize;
    let num_display_rows = data.rows.len() + 1;

    // ── Row heights ───────────────────────────────────────────────────────────
    let row_heights: Vec<u16> = (0..num_display_rows)
        .map(|r| {
            let cells: &[String] = if r == 0 {
                &data.headers
            } else {
                &data.rows[r - 1]
            };
            row_render_height(cells, col_widths, ctx.palette)
        })
        .collect();

    // ── Vertical layout — table-space (y = 0 at table top) ───────────────────
    // Each row block: Constraint::Length(row_height + 2) — content + top/bottom borders.
    // Spacing::Overlap(1) makes adjacent rows share a border row, producing ├─┤ junctions.
    // Natural table height = Σ(row_height + 1) + 1 (the final +1 for the last bottom border).
    let table_h: u16 = row_heights.iter().map(|&h| h + 1).sum::<u16>() + 1;
    let row_areas = Layout::vertical(
        row_heights
            .iter()
            .map(|&h| Constraint::Length(h + 2))
            .collect::<Vec<_>>(),
    )
    .spacing(Spacing::Overlap(1))
    .split(Rect {
        x: 0,
        y: 0,
        width: 1,
        height: table_h,
    });
    // row_areas[r].y  — table-space top of row r's block
    // row_areas[r].height — row_heights[r] + 2

    // ── Horizontal layout — screen-space (x = text_area.x) ───────────────────
    // Each column block: Constraint::Length(col_widths[col] + 2).
    // Columns from first_col onward; natural width used as the layout area so the solver
    // never shrinks columns to fit text_area.width. Columns extending past text_area.width
    // are simply skipped or clipped during rendering.
    let h_constraints: Vec<Constraint> = col_widths[first_col..]
        .iter()
        .map(|&cw| Constraint::Length(cw + 2))
        .collect();
    let table_w: u16 = col_widths[first_col..]
        .iter()
        .enumerate()
        .map(|(ci, &cw)| cw + if ci == 0 { 2 } else { 1 })
        .sum();
    let col_areas = Layout::horizontal(&h_constraints)
        .spacing(Spacing::Overlap(1))
        .split(Rect {
            x: text_area.x,
            y: 0,
            width: table_w,
            height: 1,
        });
    // col_areas[ci].x   — screen x of column ci's left border
    // col_areas[ci].width — col_widths[first_col+ci] + 2

    let has_left_overflow = first_col > 0;
    let has_right_overflow = col_areas
        .last()
        .map(|a| a.x + a.width > text_area.x + text_area.width)
        .unwrap_or(false)
        || first_col + col_areas.len() < num_cols;

    // ── Styles ────────────────────────────────────────────────────────────────
    let border_style = style.border.to_style(ctx.palette);
    let border_sel_style = style.border_selected.to_style(ctx.palette);
    let header_style = style.header.to_style(ctx.palette);
    let scroll_ind_style = style.scroll_indicator.to_style(ctx.palette);

    // Convert table-space y → screen y (i32 arithmetic avoids underflow).
    let table_offset: i32 = text_area.y as i32 - ctx.skip_lines as i32;
    let area_top = text_area.y as i32;
    let area_bot = (text_area.y + text_area.height) as i32;

    // ── Render cells ──────────────────────────────────────────────────────────
    for r in 0..num_display_rows {
        let block_top = table_offset + row_areas[r].y as i32;
        let block_bot = block_top + row_areas[r].height as i32;

        if block_bot <= area_top || block_top >= area_bot {
            continue;
        }

        let top_clipped = block_top < area_top;
        let bot_clipped = block_bot > area_bot;

        let borders = {
            let t = if top_clipped {
                Borders::empty()
            } else {
                Borders::TOP
            };
            let b = if bot_clipped {
                Borders::empty()
            } else {
                Borders::BOTTOM
            };
            t | b | Borders::LEFT | Borders::RIGHT
        };

        let vis_top = block_top.max(area_top) as u16;
        let vis_bot = block_bot.min(area_bot) as u16;

        let is_selected_row = ctx.interaction
            && match state.selected_row {
                None => r == 0,
                Some(dr) => r == dr + 1,
            };

        for ci in 0..col_areas.len() {
            let col = first_col + ci;
            let ca = col_areas[ci];

            // Skip columns that start beyond the right edge of text_area.
            if ca.x >= text_area.x + text_area.width {
                break;
            }

            // Clip the block's right edge to text_area.
            let block_w = (ca.x + ca.width).min(text_area.x + text_area.width) - ca.x;

            let cell_area = Rect {
                x: ca.x,
                y: vis_top,
                width: block_w,
                height: vis_bot - vis_top,
            };

            if cell_area.height == 0 {
                continue;
            }

            // Ratatui's Block::render_sides computes bottom_inset = (area.bottom()-1) - 1, which
            // overflows when area.bottom() <= 1 and the BOTTOM border is present with
            // MergeStrategy::Exact. Strip the BOTTOM border in that case.
            let safe_borders = if cell_area.bottom() <= 1 {
                borders - Borders::BOTTOM
            } else {
                borders
            };

            let col_bs = if ctx.interaction && col == state.selected_col {
                border_sel_style
            } else {
                border_style
            };
            let is_selected_cell = is_selected_row && ctx.interaction && col == state.selected_col;

            let block = Block::new()
                .borders(safe_borders)
                .border_style(col_bs)
                .merge_borders(MergeStrategy::Exact);

            if is_selected_cell {
                block
                    .style(style.cell_selected.to_style(ctx.palette))
                    .render(cell_area, buf);
            } else {
                block.render(cell_area, buf);
            }

            // ── Cell content ─────────────────────────────────────────────────
            let inner = Block::new().borders(safe_borders).inner(cell_area);
            if inner.height == 0 || inner.width < 2 * CELL_PADDING + 1 {
                continue;
            }

            // When the top border is clipped, content lines above the viewport must be scrolled past.
            let content_skip: u16 = if top_clipped {
                (area_top - (block_top + 1)).max(0) as u16
            } else {
                0
            };

            let cell_str: &str = if r == 0 {
                data.headers.get(col).map(|s| s.as_str()).unwrap_or("")
            } else {
                data.rows
                    .get(r - 1)
                    .and_then(|row| row.get(col))
                    .map(|s| s.as_str())
                    .unwrap_or("")
            };

            let rendered = render_markdown(cell_str, ctx.palette);
            let text: Text<'_> = if r == 0 {
                let lines = rendered
                    .lines
                    .into_iter()
                    .map(|mut l| {
                        for span in &mut l.spans {
                            span.style = span.style.patch(header_style);
                        }
                        l
                    })
                    .collect::<Vec<_>>();
                Text::from(lines)
            } else {
                rendered
            };

            let content_area = Rect {
                x: inner.x + CELL_PADDING,
                y: inner.y,
                width: inner.width.saturating_sub(2 * CELL_PADDING),
                height: inner.height,
            };

            let mut para = Paragraph::new(text).wrap(Wrap { trim: false });
            if content_skip > 0 {
                para = para.scroll((content_skip, 0));
            }
            if is_selected_cell {
                para = para.style(style.cell_selected.to_style(ctx.palette));
            }
            para.render(content_area, buf);
        }
    }

    // Fix the right border of the selected column: it was overwritten by the next column's
    // left border (rendered with normal style). Re-apply the selected style post-render.
    if ctx.interaction {
        let sel_ci = state.selected_col.checked_sub(first_col);
        if let Some(ci) = sel_ci
            && ci < col_areas.len()
        {
            let shared_x = col_areas[ci].x + col_areas[ci].width - 1;
            if shared_x < text_area.x + text_area.width {
                for r in 0..num_display_rows {
                    let block_top = table_offset + row_areas[r].y as i32;
                    let block_bot = block_top + row_areas[r].height as i32;
                    let vis_top = block_top.max(area_top) as u16;
                    let vis_bot = block_bot.min(area_bot) as u16;
                    for y in vis_top..vis_bot {
                        if let Some(cell) = buf.cell_mut((shared_x, y)) {
                            cell.set_style(border_sel_style);
                        }
                    }
                }
            }
        }
    }

    // ── Overflow indicators ───────────────────────────────────────────────────
    let indicator_y = (table_offset).max(area_top) as u16;
    if indicator_y < text_area.y + text_area.height {
        if has_left_overflow && let Some(cell) = buf.cell_mut((text_area.x, indicator_y)) {
            cell.set_symbol("◂").set_style(scroll_ind_style);
        }
        if has_right_overflow
            && text_area.width > 0
            && let Some(cell) = buf.cell_mut((text_area.x + text_area.width - 1, indicator_y))
        {
            cell.set_symbol("▸").set_style(scroll_ind_style);
        }
    }
}
