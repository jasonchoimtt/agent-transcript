use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{List, ListItem, Paragraph, StatefulWidget, Widget},
};

use super::state::{PickerState, Tab};
use crate::status_bar::hints;
use crate::theme::Palette;

const H_PAD: u16 = 2;

pub struct PickerUi<'a> {
    pub palette: &'a Palette,
}

impl StatefulWidget for PickerUi<'_> {
    type State = PickerState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut PickerState) {
        let primary = self.palette.primary;
        let muted = self.palette.muted;
        let fg = self.palette.fg;

        let content = Rect {
            x: area.x + H_PAD,
            y: area.y,
            width: area.width.saturating_sub(H_PAD * 2),
            height: area.height,
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // spacer
                Constraint::Length(1), // tab bar
                Constraint::Length(1), // spacer
                Constraint::Min(0),    // list
                Constraint::Length(1), // spacer
                Constraint::Length(1), // legend
            ])
            .split(content);

        render_tab_bar(state, chunks[1], buf, primary, muted);
        render_list(state, chunks[3], buf, primary, muted, fg);
        render_legend(state, chunks[5], buf, primary, muted);
    }
}

fn render_tab_bar(state: &PickerState, area: Rect, buf: &mut Buffer, primary: Color, muted: Color) {
    let mut spans: Vec<Span> = vec![];
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        if *tab == state.tab {
            spans.push(Span::styled(
                format!(" {} ", tab.label()),
                Style::default()
                    .fg(Color::White)
                    .bg(primary)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {} ", tab.label()),
                Style::default().fg(muted),
            ));
        }
    }

    if state.is_loading {
        const LOADING: &str = "Loading…";
        let sub = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(LOADING.chars().count() as u16),
            ])
            .split(area);
        Paragraph::new(Line::from(spans)).render(sub[0], buf);
        Paragraph::new(Span::styled(
            LOADING,
            Style::default().add_modifier(Modifier::DIM),
        ))
        .render(sub[1], buf);
    } else {
        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}

fn render_list(
    state: &mut PickerState,
    area: Rect,
    buf: &mut Buffer,
    primary: Color,
    muted: Color,
    fg: Color,
) {
    state.list_height = area.height;
    let selected = state.list_state.selected();
    let show_all = state.show_all;

    let mut items: Vec<ListItem> = vec![];

    // "New chat" virtual item is always at list index 0.
    let nc_selected = selected == Some(0);
    let nc_caret = if nc_selected { "> " } else { "  " };
    let nc_label_fg = if nc_selected { primary } else { fg };
    items.push(ListItem::new(Text::from(vec![
        Line::from(vec![
            Span::styled(nc_caret, Style::default().fg(primary)),
            Span::styled("+ New chat", Style::default().fg(nc_label_fg)),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Start a new session", Style::default().fg(muted)),
        ]),
        Line::default(),
    ])));

    if state.filtered.is_empty() {
        items.push(ListItem::new(Text::from(vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    if show_all {
                        "No transcripts found."
                    } else {
                        "No transcripts found in this workspace."
                    },
                    Style::default().fg(muted),
                ),
            ]),
            Line::default(),
            Line::default(),
        ])));
    } else {
        for (idx, entry) in state.filtered.iter().enumerate() {
            // List index for this entry is idx + 1 (offset by the NewChat slot).
            let is_selected = Some(idx + 1) == selected;
            let caret = if is_selected { "> " } else { "  " };
            let title_fg = if is_selected { primary } else { fg };

            let date_str = entry
                .updated_at
                .unwrap_or(entry.mtime)
                .format("%b %d %H:%M")
                .to_string();
            let date_width = date_str.len() as u16;
            let title_width = area.width.saturating_sub(2 + date_width + 1);
            let title = truncate(&entry.title, title_width as usize);

            let line1 = Line::from(vec![
                Span::styled(caret, Style::default().fg(primary)),
                Span::styled(
                    format!("{:<width$}", title, width = title_width as usize),
                    Style::default().fg(title_fg),
                ),
                Span::raw(" "),
                Span::styled(date_str, Style::default().fg(muted)),
            ]);

            let counts_str = format!("≡ {}", entry.message_count);
            let counts_width = counts_str.chars().count() as u16;

            let dir_prefix = if show_all {
                entry
                    .workspace_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| format!("{}  ", n.to_string_lossy()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let dir_width = dir_prefix.chars().count() as u16;

            let msg_width = area.width.saturating_sub(2 + dir_width + counts_width + 1);
            let last_msg = truncate(
                entry.last_user_message.as_deref().unwrap_or(""),
                msg_width as usize,
            );

            let mut line2_spans = vec![Span::raw("  ")];
            if !dir_prefix.is_empty() {
                line2_spans.push(Span::styled(dir_prefix, Style::default().fg(muted)));
            }
            line2_spans.push(Span::styled(
                format!("{:<width$}", last_msg, width = msg_width as usize),
                Style::default().fg(muted),
            ));
            line2_spans.push(Span::raw(" "));
            line2_spans.push(Span::styled(counts_str, Style::default().fg(primary)));

            items.push(ListItem::new(Text::from(vec![
                line1,
                Line::from(line2_spans),
                Line::default(),
            ])));
        }
    }

    StatefulWidget::render(List::new(items), area, buf, &mut state.list_state);
}

fn render_legend(state: &PickerState, area: Rect, buf: &mut Buffer, primary: Color, muted: Color) {
    if let Some((msg, _)) = &state.flash_message {
        Paragraph::new(Span::styled(msg.clone(), Style::default().fg(primary))).render(area, buf);
        return;
    }
    let filter_label = if state.show_all { "workspace" } else { "all" };
    let pairs: &[(&str, &str)] = &[
        ("←→", "Tab"),
        ("Enter", "Open"),
        ("Ctrl-Y", "Resume"),
        ("Ctrl-T", "New"),
        ("Ctrl-F", filter_label),
        ("q", "Quit"),
    ];
    Paragraph::new(hints(pairs, primary, muted, false)).render(area, buf);
}

fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        format!("{}…", truncated)
    }
}
