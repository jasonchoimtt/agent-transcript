use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::{AppMode, ConfirmKind};
use crate::terminal::crop::CollapsedCrop;
use crate::tree_scroll_view::TreeScrollViewState;
use crate::tree_scroll_view::state::Precedence;

pub struct StatusBar<'a> {
    /// Transient flash: (message, is_warning). Warning uses yellow; non-warning uses base style.
    pub flash: Option<(&'a str, bool)>,
    pub mode: &'a AppMode,
    /// True when a live PTY is running (regardless of active/focused state).
    pub terminal_live: bool,
    /// True when the terminal pane is expanded (showing extra scrollback rows).
    pub terminal_expanded: bool,
    /// True when the raw data-view overlay is open.
    pub data_view_open: bool,
    /// When true, show the debug info bar instead of key hints (toggled by Shift-D).
    pub debug: bool,
    pub tree_state: &'a TreeScrollViewState,
    /// Right-side session label shown in debug mode, e.g. "claude:abc123".
    pub session_label: Option<String>,
    /// Current collapsed crop for the terminal pane (shown in debug mode).
    pub collapsed_crop: Option<CollapsedCrop>,
    /// Pending first key of an app-level composite sequence (e.g. `!`).
    pub pending_app_key: Option<char>,
    /// When true, show a `P` pinned indicator in the hint bar.
    pub prompt_pinned: bool,
    /// Theme primary color, used for key-name highlights and confirm prompt bg.
    pub primary: Color,
    /// Theme muted color, used for hint text and status backgrounds.
    pub muted: Color,
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Flash overrides everything.
        if let Some((msg, warn)) = self.flash {
            let style = if warn {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default().fg(Color::White).bg(Color::Rgb(55, 55, 55))
            };
            Paragraph::new(msg).style(style).render(area, buf);
            return;
        }

        // Search input overrides the entire bar.
        if let AppMode::SearchInput { query, backward } = self.mode {
            let prefix = if *backward { "?" } else { "/" };
            let no_match = self
                .tree_state
                .pending_search
                .as_ref()
                .is_some_and(|ps| ps.found_path.is_empty() && !ps.query.is_empty());
            let left_text = format!(" {prefix}{query}▌");
            let right_text = if no_match { " no match " } else { "" };
            let search_style = Style::default().fg(Color::White).bg(Color::Rgb(30, 30, 80));
            let warn_style = Style::default().fg(Color::Black).bg(Color::Yellow);
            if right_text.is_empty() {
                Paragraph::new(left_text)
                    .style(search_style)
                    .render(area, buf);
            } else {
                let right_w = (right_text.len() as u16).min(area.width);
                let left_w = area.width.saturating_sub(right_w);
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(left_w), Constraint::Length(right_w)])
                    .split(area);
                Paragraph::new(left_text)
                    .style(search_style)
                    .render(chunks[0], buf);
                Paragraph::new(right_text)
                    .style(warn_style)
                    .render(chunks[1], buf);
            }
            return;
        }

        // Confirmation prompts override hints.
        let confirm_style = Style::default().fg(Color::White).bg(self.primary);
        if let AppMode::Confirm(prompt) = self.mode {
            let text = match prompt {
                ConfirmKind::Kill => " Kill session? (Y/n)",
                ConfirmKind::SessionSwitch(_) | ConfirmKind::SessionSwitchAndResume(_) => {
                    " The current session will be terminated. Continue? (Y/n)"
                }
                ConfirmKind::NewSession(_) => {
                    " The current session will be terminated. Start new session? (Y/n)"
                }
                ConfirmKind::ReaderRestart => " Restart reader? (Y/n)",
                ConfirmKind::DebugLog => " Start writing logs to /tmp/agent-transcript.log? (Y/n)",
            };
            Paragraph::new(text).style(confirm_style).render(area, buf);
            return;
        }

        // Debug mode: show the internal tree/viewport state (toggled by Shift-D).
        if self.debug {
            self.render_debug(area, buf);
            return;
        }

        // Normal mode: transparent background, dimmed key-hint bar.
        let (left, right) = self.hint_content();

        if let Some(right_line) = right {
            let right_w = line_display_width(&right_line).min(area.width);
            let left_w = area.width.saturating_sub(right_w);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(left_w), Constraint::Length(right_w)])
                .split(area);
            Paragraph::new(left).render(chunks[0], buf);
            Paragraph::new(right_line).render(chunks[1], buf);
        } else {
            Paragraph::new(left).render(area, buf);
        }
    }
}

impl StatusBar<'_> {
    /// Returns `(left_line, optional_right_line)` based on the current app mode.
    fn hint_content(&self) -> (Line<'static>, Option<Line<'static>>) {
        let (left_pairs, right_pairs) = self.hint_pairs();
        let left = hints(&left_pairs, self.primary, self.muted, false);
        let right = right_pairs.map(|pairs| {
            let mut line = hints(&pairs, self.primary, self.muted, true);
            // Prepend pending-key indicator when a multi-key prefix is active.
            let pending: Option<String> = self
                .pending_app_key
                .map(|c| c.to_string())
                .or_else(|| self.tree_state.key_parser.pending_prefix());
            if let Some(prefix) = pending {
                let style = Style::default().add_modifier(Modifier::DIM);
                let mut spans = vec![Span::styled(format!(" {prefix}…  "), style)];
                spans.extend(line.spans);
                line = Line::from(spans);
            }
            // Prepend pinned-prompt indicator when prompt is pinned.
            if self.prompt_pinned {
                let style = Style::default()
                    .fg(self.primary)
                    .add_modifier(Modifier::DIM);
                let mut spans = vec![Span::styled(" [P] ".to_string(), style)];
                spans.extend(line.spans);
                line = Line::from(spans);
            }
            line
        });
        (left, right)
    }

    /// Returns raw `(key, description)` pairs for the left and optional right hint regions.
    fn hint_pairs(
        &self,
    ) -> (
        Vec<(&'static str, &'static str)>,
        Option<Vec<(&'static str, &'static str)>>,
    ) {
        if self.data_view_open
            || self.mode == &AppMode::MessageInteraction
            || matches!(self.mode, AppMode::SearchInput { .. })
        {
            // (c) data view or interaction mode: only escape available
            (vec![("Esc", "Back")], None)
        } else if self.mode == &AppMode::Terminal {
            // (a) terminal is focused — only way out is Ctrl-O
            (vec![("Ctrl-O", "Normal")], None)
        } else if self.terminal_live {
            // (b) browsing transcript while terminal is running
            let expand_label = if self.tree_state.is_terminal_selected() {
                if self.terminal_expanded {
                    "Collapse chat"
                } else {
                    "Expand chat"
                }
            } else {
                "Drill down"
            };
            (
                vec![
                    ("Esc", "Chat"),
                    ("Space", expand_label),
                    ("r", "Raw"),
                    ("I", "Session info"),
                ],
                Some(vec![("Ctrl-X", "Select chat")]),
            )
        } else {
            // (d) no live terminal — offer resume / picker
            (
                vec![("Ctrl-Y", "Resume chat"), ("I", "Session info")],
                Some(vec![("Ctrl-X", "Select chat")]),
            )
        }
    }

    fn render_debug(&self, area: Rect, buf: &mut Buffer) {
        let base_style = Style::default().fg(Color::White).bg(Color::Rgb(55, 55, 55));
        let s = self.tree_state;
        let top_idx = s
            .top_index
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sel_idx = s
            .selection_index
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let prec = if s.precedence == Precedence::Selection {
            "Sel"
        } else {
            "Top"
        };
        let node_type = s.selected_node_type_label();
        let node_id = s.selected_node_id();
        let crop_text = match self.collapsed_crop {
            Some(c) => format!(" crop={}+{}", c.start_row, c.height),
            None => " crop=none".to_string(),
        };
        let left_text = format!(
            " top=[{}]+{} sel=[{}] bot={} prec={} type={} id={}{}",
            top_idx, s.top_offset, sel_idx, s.at_bottom, prec, node_type, node_id, crop_text,
        );

        if let Some(right_text) = &self.session_label {
            let right_w = (right_text.len() as u16).min(area.width);
            let left_w = area.width.saturating_sub(right_w);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(left_w), Constraint::Length(right_w)])
                .split(area);
            Paragraph::new(Span::raw(left_text))
                .style(base_style)
                .render(chunks[0], buf);
            Paragraph::new(Span::raw(right_text.as_str()))
                .style(Style::default().fg(Color::White).bg(Color::Black))
                .render(chunks[1], buf);
        } else {
            Paragraph::new(Span::raw(left_text))
                .style(base_style)
                .render(area, buf);
        }
    }
}

/// Build a `Line` of `[Key] Description` hints separated by two spaces.
/// Key names are rendered in the primary color (dimmed); descriptions are dimmed plain text.
/// No background is set — the terminal's default background shows through.
pub(crate) fn hints(
    pairs: &[(&str, &str)],
    primary: Color,
    muted: Color,
    right: bool,
) -> Line<'static> {
    let key_style = Style::default().fg(primary);
    let text_style = Style::default().fg(muted);

    let mut spans: Vec<Span<'static>> = vec![];
    if !right {
        spans.push(Span::styled(" ", text_style));
    }
    for (i, (key, desc)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", text_style));
        }
        spans.push(Span::styled(format!("[{key}]"), key_style));
        spans.push(Span::styled(format!(" {desc}"), text_style));
    }
    if right {
        spans.push(Span::styled(" ", text_style));
    }
    Line::from(spans)
}

/// Sum of display widths of all spans in a line.
/// Uses char count rather than byte length so multi-byte characters like `…` (3 bytes, 1 column)
/// are measured correctly.
fn line_display_width(line: &Line<'_>) -> u16 {
    line.spans
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum()
}
