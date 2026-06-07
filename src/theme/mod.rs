pub mod palette;
pub mod styles;

pub use palette::{ColorVar, Palette};
pub use styles::{
    AgentMessageStyle, ContainerStyle, JsonStyle, MessageStyle, OtherStyle, SelectionStyle,
    SpanStyle, SystemStyle, TableStyle, TaggedContainerStyle, TaggedOtherStyle,
    TaggedUserMessageStyle, TaskSummaryStyle, ThinkingStyle, ToolCallStyle, ToolResultStyle,
    UserMessageStyle,
};

use std::path::Path;

use color_eyre::eyre::WrapErr as _;
use serde::Deserialize;

use crate::color::{RgbColor, relative_luminance};
use crate::config::ThemeConfig;
use crate::tree_scroll_view::state::MessageType;

#[derive(Clone, Deserialize)]
pub struct Theme {
    pub palette: Palette,
    pub user_message: UserMessageStyle,
    pub agent_message: AgentMessageStyle,
    pub thinking: ThinkingStyle,
    pub tool_call: ToolCallStyle,
    pub tool_result: ToolResultStyle,
    pub task_summary: TaskSummaryStyle,
    pub container: ContainerStyle,
    pub selection: SelectionStyle,
    pub system: SystemStyle,
    pub json: JsonStyle,
    pub table: TableStyle,
    pub other: OtherStyle,
}

impl Theme {
    pub fn default_dark() -> Self {
        // Palette (dark.toml) and styles (styles.toml) are separate so the same styles.toml
        // can later be paired with a light palette without duplication.
        toml::from_str(concat!(
            include_str!("dark.toml"),
            "\n",
            include_str!("styles.toml")
        ))
        .expect("built-in dark.toml + styles.toml must be valid")
    }

    /// Load the theme from config, detecting light vs dark from `host_bg` when `mode = "auto"`.
    /// Named palettes / styles are read from `{XDG_CONFIG_HOME}/agent-transcript/palettes/` and
    /// `styles/`. Unset names use the bundled defaults; missing or unreadable files are errors.
    pub fn load(config: &ThemeConfig, host_bg: Option<RgbColor>) -> color_eyre::Result<Self> {
        let is_light = match config.mode.as_deref().unwrap_or("auto") {
            "light" => true,
            "dark" => false,
            // "auto" (or any unrecognised value): infer from host background luminance.
            _ => host_bg
                .map(|bg| relative_luminance(bg) > 0.5)
                .unwrap_or(false),
        };

        let base_dir = crate::config::xdg_config_dir().join("agent-transcript");

        let palette_name = if is_light {
            config.light.as_deref()
        } else {
            config.dark.as_deref()
        };
        let bundled_palette = if is_light {
            include_str!("light.toml")
        } else {
            include_str!("dark.toml")
        };
        let palette_toml = read_theme_file(palette_name, &base_dir.join("palettes"))?
            .unwrap_or_else(|| bundled_palette.to_string());

        let styles_toml = read_theme_file(config.styles.as_deref(), &base_dir.join("styles"))?
            .unwrap_or_else(|| include_str!("styles.toml").to_string());

        let combined = format!("{palette_toml}\n{styles_toml}");
        let theme: Theme = toml::from_str(&combined).wrap_err("failed to parse theme TOML")?;
        Ok(theme)
    }

    pub fn style_for<'a>(&'a self, mt: &MessageType) -> MessageStyle<'a> {
        match mt {
            MessageType::UserMessage => MessageStyle::UserMessage(&self.user_message),
            MessageType::AgentMessage => MessageStyle::AgentMessage(&self.agent_message),
            MessageType::Thinking => MessageStyle::Thinking(&self.thinking),
            MessageType::ToolCall => MessageStyle::ToolCall(&self.tool_call),
            MessageType::ToolResult => MessageStyle::ToolResult(&self.tool_result),
            MessageType::TaskSummary => MessageStyle::TaskSummary(&self.task_summary),
            MessageType::Container => MessageStyle::Container(&self.container),
            MessageType::System => MessageStyle::System(&self.system),
            MessageType::Json => MessageStyle::Json(&self.json),
            MessageType::Table => MessageStyle::Table(&self.table),
            MessageType::Other => MessageStyle::Other(&self.other),
        }
    }
}

/// Read a named theme file from `dir/{name}.toml`. Returns `Ok(None)` when `name` is `None`
/// (caller should use the bundled default). Propagates I/O errors when a name is given.
fn read_theme_file(name: Option<&str>, dir: &Path) -> color_eyre::Result<Option<String>> {
    let Some(name) = name else { return Ok(None) };
    let path = dir.join(format!("{name}.toml"));
    std::fs::read_to_string(&path)
        .map(Some)
        .wrap_err_with(|| format!("could not read theme file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_parses() {
        let _ = Theme::default_dark();
    }

    #[test]
    fn light_theme_parses() {
        let _: Theme = toml::from_str(concat!(
            include_str!("light.toml"),
            "\n",
            include_str!("styles.toml")
        ))
        .expect("light.toml + styles.toml must parse as Theme");
    }
}
