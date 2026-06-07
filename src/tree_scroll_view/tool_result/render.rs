use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use super::{
    DiffLine, DiffLineKind, FileDeltaState, PatchHunk, ShellOutputState, ToolResultPayload,
    ToolResultUiState, build_diff_lines, collect_shell_lines,
};
use crate::theme::Palette;
use crate::theme::styles::ToolResultStyle;
use crate::tree_scroll_view::ansi::{clip_to_visual_width, visual_width};

/// Total chars used by the line-number prefix: `{4} {4}  ` = 11.
pub const LINE_NUM_PREFIX_LEN: usize = 11;

/// Max shell lines shown in compact (non-expanded) mode.
const SHELL_COMPACT_MAX: usize = 10;

fn shell_ellipsis_line(hidden: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("… ({hidden} lines hidden)"),
        Style::new().add_modifier(Modifier::DIM),
    ))
}

fn diff_ellipsis_line(hidden: usize, ln_style: Style) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    // Prefix aligns with the line-number columns: 4 (old) + 1 (sep) + 3 + ⋮ (new, right-aligned) + 2 = 11 chars.
    Line::from(vec![
        Span::styled("        ⋮  ", ln_style),
        Span::styled(format!("({hidden} lines hidden)"), dim),
    ])
}

// ── Height ────────────────────────────────────────────────────────────────────

pub fn compute_height(state: &ToolResultUiState, available_width: u16) -> u16 {
    match &state.payload {
        ToolResultPayload::FileDelta(fd) => {
            compute_file_delta_height(fd, state.expanded, state.wrap, available_width)
        }
        ToolResultPayload::ShellOutput(so) => {
            compute_shell_height(so, state.expanded, state.wrap, available_width)
        }
    }
}

fn wrapped_rows(content_visual_width: usize, column_width: usize) -> u16 {
    if column_width == 0 || content_visual_width == 0 {
        1
    } else {
        content_visual_width.div_ceil(column_width) as u16
    }
}

fn compute_file_delta_height(fd: &FileDeltaState, expanded: bool, wrap: bool, width: u16) -> u16 {
    // 1 header line.
    let mut h: u16 = 1;
    let content_width = (width as usize).saturating_sub(LINE_NUM_PREFIX_LEN) as u16;
    let hunks: &[_] = if expanded {
        &fd.hunks
    } else {
        fd.hunks
            .get(fd.current_hunk..=fd.current_hunk)
            .unwrap_or(&[])
    };
    for hunk in hunks {
        let (lines, hidden) = build_diff_lines(hunk, expanded, fd.context_lines);
        for dl in &lines {
            h += if wrap {
                Paragraph::new(Line::from(Span::raw(dl.content.clone())))
                    .wrap(Wrap { trim: false })
                    .line_count(content_width)
                    .max(1) as u16
            } else {
                1
            };
        }
        if hidden > 0 {
            h += 1;
        }
    }
    h.max(1) + 1
}

fn compute_shell_height(so: &ShellOutputState, expanded: bool, wrap: bool, width: u16) -> u16 {
    let max = if expanded {
        None
    } else {
        Some(SHELL_COMPACT_MAX)
    };
    let (lines, hidden) = collect_shell_lines(so, max);
    let mut h: u16 = lines
        .iter()
        .map(|(content, _)| {
            if wrap {
                wrapped_rows(visual_width(content), width as usize)
            } else {
                1
            }
        })
        .sum();
    if hidden > 0 {
        h += 1;
    }
    h.max(1) + 1
}

// ── Rendering ─────────────────────────────────────────────────────────────────

pub fn render_tool_result(
    text_area: Rect,
    state: &ToolResultUiState,
    _interaction: bool,
    palette: &Palette,
    style: &ToolResultStyle,
    buf: &mut Buffer,
    skip_lines: u16,
) {
    if text_area.height == 0 {
        return;
    }
    match &state.payload {
        ToolResultPayload::FileDelta(fd) => {
            render_file_delta(
                text_area,
                fd,
                state.expanded,
                state.wrap,
                palette,
                style,
                buf,
                skip_lines,
            );
        }
        ToolResultPayload::ShellOutput(so) => {
            render_shell_output(
                text_area,
                so,
                state.expanded,
                state.wrap,
                palette,
                style,
                buf,
                skip_lines,
            );
        }
    }
}

// ── File delta ────────────────────────────────────────────────────────────────

fn render_file_delta(
    area: Rect,
    fd: &FileDeltaState,
    expanded: bool,
    wrap: bool,
    palette: &Palette,
    style: &ToolResultStyle,
    buf: &mut Buffer,
    skip_lines: u16,
) {
    if wrap {
        render_file_delta_wrapped(area, fd, expanded, palette, style, buf, skip_lines);
        return;
    }

    let file_path_style = style.file_path.to_style(palette);
    let paginator_style = style.paginator.to_style(palette);
    let stat_removed_style = style.diff_stat_removed.to_style(palette);
    let stat_added_style = style.diff_stat_added.to_style(palette);
    let ln_style = style.line_num.to_style(palette);

    let n_hunks = fd.hunks.len();
    let header = build_header_line(
        fd,
        expanded,
        area.width,
        file_path_style,
        paginator_style,
        stat_removed_style,
        stat_added_style,
    );

    let mut content_lines: Vec<Line<'static>> = vec![header];

    if expanded {
        for hunk in &fd.hunks {
            let (diff_lines, hidden) = build_diff_lines(hunk, true, fd.context_lines);
            for dl in &diff_lines {
                content_lines.push(build_diff_line(dl, area.width, palette, style));
            }
            if hidden > 0 {
                content_lines.push(diff_ellipsis_line(hidden, ln_style));
            }
        }
    } else if n_hunks > 0
        && let Some(hunk) = fd.hunks.get(fd.current_hunk)
    {
        let (diff_lines, hidden) = build_diff_lines(hunk, false, fd.context_lines);
        for dl in &diff_lines {
            content_lines.push(build_diff_line(dl, area.width, palette, style));
        }
        if hidden > 0 {
            content_lines.push(diff_ellipsis_line(hidden, ln_style));
        }
    }

    Paragraph::new(content_lines)
        .scroll((skip_lines, 0))
        .render(area, buf);
}

/// Renders the file delta with text wrapping using a split-area approach.
///
/// The area is divided into a left panel (11 cols, line numbers, no wrap) and a right
/// panel (remaining width, content with ratatui word-wrap). Background colors are
/// pre-filled in the right panel before rendering so that each physical row produced
/// by word-wrap is fully covered by the diff color.
fn render_file_delta_wrapped(
    area: Rect,
    fd: &FileDeltaState,
    expanded: bool,
    palette: &Palette,
    style: &ToolResultStyle,
    buf: &mut Buffer,
    skip_lines: u16,
) {
    let content_width = (area.width as usize).saturating_sub(LINE_NUM_PREFIX_LEN) as u16;
    if content_width == 0 {
        return;
    }

    let file_path_style = style.file_path.to_style(palette);
    let paginator_style = style.paginator.to_style(palette);
    let stat_removed_style = style.diff_stat_removed.to_style(palette);
    let stat_added_style = style.diff_stat_added.to_style(palette);
    let ln_style = style.line_num.to_style(palette);

    // Row 0 is the header. If skip_lines == 0, render header at top and shrink content area.
    let (header_area, content_area, content_skip) = if skip_lines == 0 {
        let ha = Rect { height: 1, ..area };
        let ca = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        (Some(ha), ca, 0u16)
    } else {
        (None, area, skip_lines - 1)
    };

    if let Some(ha) = header_area {
        let header = build_header_line(
            fd,
            expanded,
            area.width,
            file_path_style,
            paginator_style,
            stat_removed_style,
            stat_added_style,
        );
        Paragraph::new(header).render(ha, buf);
    }

    if content_area.height == 0 {
        return;
    }

    // Build left (pre-expanded line numbers) and right (single logical lines) paragraphs.
    // Also track the background color for each physical row in the right panel so we can
    // pre-fill it before rendering.
    let mut left_lines: Vec<Line<'static>> = Vec::new();
    let mut right_lines: Vec<Line<'static>> = Vec::new();
    let mut phys_row_bgs: Vec<Option<Color>> = Vec::new();

    let hunks: &[PatchHunk] = if expanded {
        &fd.hunks
    } else {
        fd.hunks
            .get(fd.current_hunk..=fd.current_hunk)
            .unwrap_or(&[])
    };

    for hunk in hunks {
        let (diff_lines, hidden) = build_diff_lines(hunk, expanded, fd.context_lines);

        for dl in &diff_lines {
            let (line_style, ln_bg_style) = diff_line_styles(dl, palette, style);

            let rows = Paragraph::new(Line::from(Span::raw(dl.content.clone())))
                .wrap(Wrap { trim: false })
                .line_count(content_width)
                .max(1) as u16;

            let old_str = dl.old_num.map_or("    ".to_string(), |n| format!("{n:>4}"));
            let new_str = dl.new_num.map_or("    ".to_string(), |n| format!("{n:>4}"));
            let ln_prefix = format!("{old_str} {new_str}  ");

            left_lines.push(Line::from(Span::styled(ln_prefix, ln_bg_style)));
            for _ in 1..rows {
                // Continuation rows: blank but with same background.
                left_lines.push(Line::from(Span::styled("           ", ln_bg_style)));
            }

            right_lines.push(Line::from(Span::styled(dl.content.clone(), line_style)));

            for _ in 0..rows {
                phys_row_bgs.push(line_style.bg);
            }
        }

        if hidden > 0 {
            let dim = Style::new().add_modifier(Modifier::DIM);
            left_lines.push(Line::from(Span::styled("        ⋮  ", ln_style)));
            right_lines.push(Line::from(Span::styled(
                format!("({hidden} lines hidden)"),
                dim,
            )));
            phys_row_bgs.push(None);
        }
    }

    // Pre-fill the right panel background so that end-of-row cells (after word-wrap breaks)
    // have the correct diff color, not just the cells covered by text spans.
    let right_x = content_area.x + LINE_NUM_PREFIX_LEN as u16;
    let right_end_x = (right_x + content_width).min(buf.area.right());
    for (row_y, bg) in (content_area.y..).zip(phys_row_bgs.iter().skip(content_skip as usize)) {
        if row_y >= content_area.y + content_area.height {
            break;
        }
        if let Some(color) = bg {
            let fill_style = Style::new().bg(*color);
            for col in right_x..right_end_x {
                buf[(col, row_y)].set_style(fill_style);
            }
        }
    }

    // Left panel: line numbers, no wrap.
    let left_rect = Rect {
        width: LINE_NUM_PREFIX_LEN as u16,
        ..content_area
    };
    Paragraph::new(left_lines)
        .scroll((content_skip, 0))
        .render(left_rect, buf);

    // Right panel: content with word-wrap.
    let right_rect = Rect {
        x: right_x,
        width: content_width,
        y: content_area.y,
        height: content_area.height,
    };
    Paragraph::new(right_lines)
        .wrap(Wrap { trim: false })
        .scroll((content_skip, 0))
        .render(right_rect, buf);
}

fn count_file_stats(hunks: &[PatchHunk]) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for hunk in hunks {
        for line in &hunk.lines {
            if line.starts_with('+') {
                added += 1;
            } else if line.starts_with('-') {
                removed += 1;
            }
        }
    }
    (added, removed)
}

fn build_header_line(
    fd: &FileDeltaState,
    expanded: bool,
    width: u16,
    file_path_style: Style,
    paginator_style: Style,
    stat_removed_style: Style,
    stat_added_style: Style,
) -> Line<'static> {
    let n_hunks = fd.hunks.len();
    let (added, removed) = count_file_stats(&fd.hunks);
    let stat_rm = format!(" -{removed}");
    let stat_add = format!(" +{added}");
    let stat_len = stat_rm.chars().count() + stat_add.chars().count();
    let width = width as usize;

    if expanded || n_hunks <= 1 {
        let available_for_path = width.saturating_sub(stat_len);
        let displayed_path = clip_path(&fd.file_path, available_for_path);
        return Line::from(vec![
            Span::styled(displayed_path, file_path_style),
            Span::styled(stat_rm, stat_removed_style),
            Span::styled(stat_add, stat_added_style),
        ]);
    }

    // Multi-hunk: build paginator on the right.
    let current = fd.current_hunk + 1;
    let mid = format!(" {} / {} ", current, n_hunks);
    let paginator_len = 1 + mid.chars().count() + 1; // ◀ + mid + ▶

    // Truncate path so that "path stat padding paginator" fits in width.
    let available_for_path = width.saturating_sub(paginator_len + 1 + stat_len);
    let displayed_path = clip_path(&fd.file_path, available_for_path);

    let left_col_len = displayed_path.chars().count() + stat_len;
    let gap = width.saturating_sub(left_col_len + paginator_len);
    let padding = " ".repeat(gap);

    let at_first = fd.current_hunk == 0;
    let at_last = fd.current_hunk + 1 >= n_hunks;
    let left_arrow_style = if at_first {
        paginator_style.add_modifier(Modifier::DIM)
    } else {
        paginator_style
    };
    let right_arrow_style = if at_last {
        paginator_style.add_modifier(Modifier::DIM)
    } else {
        paginator_style
    };

    Line::from(vec![
        Span::styled(displayed_path, file_path_style),
        Span::styled(stat_rm, stat_removed_style),
        Span::styled(stat_add, stat_added_style),
        Span::raw(padding),
        Span::styled("◀", left_arrow_style),
        Span::styled(mid, paginator_style),
        Span::styled("▶", right_arrow_style),
    ])
}

fn clip_path(path: &str, available: usize) -> String {
    if visual_width(path) > available {
        let clipped = clip_to_visual_width(path, available.saturating_sub(1)).0;
        format!("{clipped}…")
    } else {
        path.to_owned()
    }
}

/// Returns `(content_style, line_number_style)` for a diff line.
fn diff_line_styles(dl: &DiffLine, palette: &Palette, style: &ToolResultStyle) -> (Style, Style) {
    match dl.kind {
        DiffLineKind::Added => (
            style.diff_added.to_style(palette),
            style
                .diff_added
                .to_style(palette)
                .add_modifier(Modifier::DIM),
        ),
        DiffLineKind::Removed => (
            style.diff_removed.to_style(palette),
            style
                .diff_removed
                .to_style(palette)
                .add_modifier(Modifier::DIM),
        ),
        DiffLineKind::Changed => (
            style.diff_changed.to_style(palette),
            style
                .diff_changed
                .to_style(palette)
                .add_modifier(Modifier::DIM),
        ),
        DiffLineKind::Context => (
            style.diff_context.to_style(palette),
            style.line_num.to_style(palette),
        ),
    }
}

fn build_diff_line(
    dl: &DiffLine,
    width: u16,
    palette: &Palette,
    style: &ToolResultStyle,
) -> Line<'static> {
    let (line_style, ln_bg_style) = diff_line_styles(dl, palette, style);

    let old_str = dl.old_num.map_or("    ".to_string(), |n| format!("{n:>4}"));
    let new_str = dl.new_num.map_or("    ".to_string(), |n| format!("{n:>4}"));
    let ln_prefix = format!("{old_str} {new_str}  ");

    // Pad content to fill the remaining width so the background colour covers the full line.
    let content_width = (width as usize).saturating_sub(LINE_NUM_PREFIX_LEN);
    let content = format!("{:<width$}", dl.content, width = content_width);

    Line::from(vec![
        Span::styled(ln_prefix, ln_bg_style),
        Span::styled(content, line_style),
    ])
}

// ── Shell output ──────────────────────────────────────────────────────────────

fn render_shell_output(
    area: Rect,
    so: &ShellOutputState,
    expanded: bool,
    wrap: bool,
    palette: &Palette,
    style: &ToolResultStyle,
    buf: &mut Buffer,
    skip_lines: u16,
) {
    let max = if expanded {
        None
    } else {
        Some(SHELL_COMPACT_MAX)
    };
    let (lines, hidden) = collect_shell_lines(so, max);

    let stderr_style = style.stderr.to_style(palette);
    let stdout_style = style.stdout.to_style(palette);

    let mut rendered: Vec<Line<'static>> = Vec::new();

    if hidden > 0 {
        rendered.push(shell_ellipsis_line(hidden));
    }

    for (content, is_stderr) in lines {
        let s = if is_stderr {
            stderr_style
        } else {
            stdout_style
        };
        rendered.push(Line::from(Span::styled(content, s)));
    }

    let para = Paragraph::new(rendered).scroll((skip_lines, 0));
    if wrap {
        para.wrap(Wrap { trim: false }).render(area, buf);
    } else {
        para.render(area, buf);
    }
}

// ── Compact (show_more=false) one-liner ───────────────────────────────────────

/// Render the compact (show_more=false) one-liner with per-span styling.
pub fn render_compact(
    area: Rect,
    state: &ToolResultUiState,
    palette: &Palette,
    style: &ToolResultStyle,
    buf: &mut Buffer,
    collapsed: bool,
    skip_lines: u16,
) {
    match &state.payload {
        ToolResultPayload::FileDelta(fd) => {
            let file_path_style = style.file_path.to_style(palette);
            let stat_removed_style = style.diff_stat_removed.to_style(palette);
            let stat_added_style = style.diff_stat_added.to_style(palette);
            let (added, removed) = count_file_stats(&fd.hunks);
            let stat_rm = format!(" -{removed}");
            let stat_add = format!(" +{added}");
            let stat_len = stat_rm.chars().count() + stat_add.chars().count();
            // Reserve 1 for the ▾ suffix when collapsed and not truncated.
            let suffix_reserve = if collapsed { 1 } else { 0 };
            let available_for_path =
                (area.width as usize).saturating_sub(stat_len + suffix_reserve);
            let path_w = visual_width(&fd.file_path);
            let (displayed_path, truncated) = if path_w > available_for_path {
                (clip_path(&fd.file_path, available_for_path), true)
            } else {
                (fd.file_path.clone(), false)
            };
            let mut spans = vec![
                Span::styled(displayed_path, file_path_style),
                Span::styled(stat_rm, stat_removed_style),
                Span::styled(stat_add, stat_added_style),
            ];
            if !truncated && collapsed {
                spans.push(Span::styled("▾", Style::new().dim()));
            }
            Paragraph::new(Line::from(spans))
                .scroll((skip_lines, 0))
                .render(area, buf);
        }
        ToolResultPayload::ShellOutput(so) => {
            let summary = build_shell_brief(so);
            let available = area.width as usize;
            let (clipped, truncated) = if visual_width(&summary) > available {
                (
                    clip_to_visual_width(&summary, available.saturating_sub(1))
                        .0
                        .to_owned(),
                    true,
                )
            } else {
                (summary, false)
            };
            let mut spans = vec![Span::styled(clipped, style.content.to_style(palette))];
            if truncated {
                spans.push(Span::styled("…", Style::new().dim()));
            } else if collapsed {
                spans.push(Span::styled("▾", Style::new().dim()));
            }
            Paragraph::new(Line::from(spans))
                .scroll((skip_lines, 0))
                .render(area, buf);
        }
    }
}

fn build_shell_brief(so: &ShellOutputState) -> String {
    if !so.stderr.is_empty() {
        let first = so.stderr.lines().next().unwrap_or("");
        return format!("stderr: {first}");
    }
    if !so.stdout.is_empty() {
        let n = so.stdout.lines().count();
        return format!("stdout: {n} line{}", if n == 1 { "" } else { "s" });
    }
    "(no output)".to_string()
}

/// One-liner brief for use as the `brief` field in MessageState.
pub fn make_brief(state: &ToolResultUiState) -> String {
    match &state.payload {
        ToolResultPayload::FileDelta(fd) => {
            let n_hunks = fd.hunks.len();
            let added: u32 = fd
                .hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .filter(|l| l.starts_with('+'))
                .count() as u32;
            let removed: u32 = fd
                .hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .filter(|l| l.starts_with('-'))
                .count() as u32;
            if n_hunks == 1 {
                format!("{} -{removed} +{added}", fd.file_path)
            } else {
                format!("{} -{removed} +{added} ({n_hunks} hunks)", fd.file_path)
            }
        }
        ToolResultPayload::ShellOutput(so) => build_shell_brief(so),
    }
}
