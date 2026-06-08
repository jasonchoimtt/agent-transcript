use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, StatefulWidget, Widget};

use super::state::DataViewState;
use crate::terminal::pane_ref::{PlaceholderInfo, TerminalPaneRef};
use crate::theme::Theme;
use crate::tree_scroll_view::TreeScrollView;

pub struct DataViewUi<'a> {
    pub theme: &'a Theme,
}

impl StatefulWidget for DataViewUi<'_> {
    type State = DataViewState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let popup = centered(area, 80, 80);
        state.popup_area = popup;

        Clear.render(popup, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" data ")
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(popup);
        block.render(popup, buf);

        TreeScrollView {
            terminal: TerminalPaneRef::Placeholder(PlaceholderInfo {
                provider_name: "",
                session_id: None,
                directory: None,
                exit_code: None,
            }),
            scrollback_available: 0,
            terminal_expanded: false,
            terminal_active: false,
            theme: self.theme,
            message_interaction: false,
        }
        .render(inner, buf, &mut state.tree);
    }
}

/// Return a `Rect` centered within `area` at `pct_w`% width and `pct_h`% height,
/// each clamped to at least 10 cells.
fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).max(10).min(area.width);
    let h = (area.height * pct_h / 100).max(10).min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn render_valid_json() {
        let theme = Theme::default_dark();
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = DataViewState::new(r#"{"key":"value","n":42}"#);
        term.draw(|f| {
            DataViewUi { theme: &theme }.render(f.area(), f.buffer_mut(), &mut state);
        })
        .unwrap();
    }

    #[test]
    fn render_invalid_json() {
        let theme = Theme::default_dark();
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = DataViewState::new("not valid json at all");
        term.draw(|f| {
            DataViewUi { theme: &theme }.render(f.area(), f.buffer_mut(), &mut state);
        })
        .unwrap();
    }
}
