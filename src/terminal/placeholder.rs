use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::Widget,
};

use super::pane_ref::PlaceholderInfo;

pub struct PlaceholderWidget<'a> {
    pub info: &'a PlaceholderInfo,
    pub selected: bool,
    pub primary: Color,
    pub muted: Color,
}

impl Widget for PlaceholderWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        // Left gutter column: same ▌ style as message widget selection indicator.
        let gutter_color = if self.selected {
            Some(self.primary)
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

        let content = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(1),
            height: area.height,
        };

        if content.width == 0 {
            return;
        }

        let session = self.info.session_id.as_deref().unwrap_or("—");
        let dir = self
            .info
            .directory
            .as_ref()
            .and_then(|p| p.to_str())
            .unwrap_or("—");

        let mut lines = vec![
            format!("Agent CLI:  {}", self.info.provider_name),
            format!("Session ID: {session}"),
            format!("Directory:  {dir}"),
            String::new(),
        ];

        if let Some(code) = self.info.exit_code {
            let code_str = if code >= 0 {
                format!("{code}")
            } else {
                "unknown".to_string()
            };
            lines.push(format!("Command exited with code {code_str}"));
            lines.push(String::new());
        }

        lines.push("[Ctrl-Y] Resume session".to_string());

        let total = lines.len() as u16;
        let start_y = if content.height > total {
            content.y + (content.height - total) / 2
        } else {
            content.y
        };

        let dim = Style::default().fg(self.muted);
        for (i, line) in lines.iter().enumerate() {
            let row_y = start_y + i as u16;
            if row_y >= content.y + content.height {
                break;
            }
            let line_w = line.chars().count() as u16;
            let start_x = if content.width > line_w {
                content.x + (content.width - line_w) / 2
            } else {
                content.x
            };
            for (j, ch) in line.chars().enumerate() {
                let col = start_x + j as u16;
                if col >= content.x + content.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((col, row_y)) {
                    cell.set_symbol(&ch.to_string()).set_style(dim);
                }
            }
        }
    }
}
