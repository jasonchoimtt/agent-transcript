use std::collections::HashMap;

use ratatui::style::{Modifier, Style};
use serde::Deserialize;

use super::palette::{ColorVar, Palette};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SpanStyle {
    pub fg: Option<ColorVar>,
    pub bg: Option<ColorVar>,
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub underlined: bool,
}

impl SpanStyle {
    pub fn to_style(&self, palette: &Palette) -> Style {
        let mut style = Style::default();
        if let Some(fg) = &self.fg {
            style = style.fg(palette.resolve(fg));
        }
        if let Some(bg) = &self.bg {
            style = style.bg(palette.resolve(bg));
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.dim {
            style = style.add_modifier(Modifier::DIM);
        }
        if self.underlined {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }
}

/// Per-tag style for a user message. Used as the `default` and each entry in `tag_styles`
/// on `UserMessageStyle`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TaggedUserMessageStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
    pub uses_markdown: bool,
}

/// Style for user messages. Supports per-xml-tag overrides: when a message carries an
/// `xml_tag`, its style is looked up in `tag_styles` with `default` as fallback.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserMessageStyle {
    pub default: TaggedUserMessageStyle,
    /// Keyed by XML tag name (e.g. `"bash-input"`).
    pub tag_styles: HashMap<String, TaggedUserMessageStyle>,
}

impl UserMessageStyle {
    /// Returns the `TaggedUserMessageStyle` for `xml_tag`, falling back to `default`.
    pub fn resolve<'a>(&'a self, xml_tag: Option<&str>) -> &'a TaggedUserMessageStyle {
        xml_tag
            .and_then(|tag| self.tag_styles.get(tag))
            .unwrap_or(&self.default)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentMessageStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
    pub uses_markdown: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThinkingStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TaskSummaryStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

/// Per-tag style for a container message. Used as the `default` and each entry in `tag_styles`
/// on `ContainerStyle`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TaggedContainerStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

/// Style for container messages. Supports per-structural-tag overrides: when a message carries
/// a `tag`, its style is looked up in `tag_styles` with `default` as fallback.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContainerStyle {
    pub default: TaggedContainerStyle,
    /// Keyed by structural tag (e.g. `"turn"`, `"user-turn"`, `"agent-turn"`, `"task"`).
    pub tag_styles: HashMap<String, TaggedContainerStyle>,
}

impl ContainerStyle {
    /// Returns the `TaggedContainerStyle` for `tag`, falling back to `default`.
    pub fn resolve<'a>(&'a self, tag: Option<&str>) -> &'a TaggedContainerStyle {
        tag.and_then(|t| self.tag_styles.get(t))
            .unwrap_or(&self.default)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolCallStyle {
    pub name: SpanStyle,
    pub params: SpanStyle,
    pub name_max_width: Option<u16>,
    pub show_params_in_brief: bool,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
    pub error_indicator: Option<String>,
    pub error_indicator_style: Option<SpanStyle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolResultStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
    pub error_indicator: Option<String>,
    pub error_indicator_style: Option<SpanStyle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SelectionStyle {
    pub indicator: SpanStyle,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SystemStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JsonStyle {
    /// Object key prefix (e.g. `name: `).
    pub key: SpanStyle,
    pub string: SpanStyle,
    pub number: SpanStyle,
    pub bool_null: SpanStyle,
    /// Objects and arrays (the inline one-line JSON).
    pub container: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TableStyle {
    /// Box-drawing border characters (normal state).
    pub border: SpanStyle,
    /// Border redrawn in this style for the selected cell's edges.
    pub border_selected: SpanStyle,
    /// Header row cell content (bold/color applied over markdown style).
    pub header: SpanStyle,
    /// Body cell content (fallback when not rendering markdown).
    pub cell: SpanStyle,
    /// Style applied to the background of the selected cell's content area.
    pub cell_selected: SpanStyle,
    /// ◂ / ▸ horizontal scroll overflow indicators.
    pub scroll_indicator: SpanStyle,
    /// Message-type indicator glyph shown at the node's tree position.
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

/// Per-tag style for a `MessageType::Other` node. Used as `default` and each entry in
/// `styles` on `OtherStyle`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TaggedOtherStyle {
    pub content: SpanStyle,
    pub indicator: Option<String>,
    pub indicator_style: Option<SpanStyle>,
}

/// Style for `MessageType::Other` nodes. Supports per-tag overrides keyed by the `tag`
/// field on the `MessageState` (e.g. `"label"`, `"value"`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OtherStyle {
    pub default: TaggedOtherStyle,
    /// Keyed by `MessageState.tag`.
    pub styles: HashMap<String, TaggedOtherStyle>,
}

impl OtherStyle {
    /// Returns the `TaggedOtherStyle` for `tag`, falling back to `default`.
    pub fn resolve<'a>(&'a self, tag: Option<&str>) -> &'a TaggedOtherStyle {
        tag.and_then(|t| self.styles.get(t))
            .unwrap_or(&self.default)
    }
}

pub enum MessageStyle<'a> {
    UserMessage(&'a UserMessageStyle),
    AgentMessage(&'a AgentMessageStyle),
    Thinking(&'a ThinkingStyle),
    ToolCall(&'a ToolCallStyle),
    ToolResult(&'a ToolResultStyle),
    TaskSummary(&'a TaskSummaryStyle),
    Container(&'a ContainerStyle),
    System(&'a SystemStyle),
    Json(&'a JsonStyle),
    Table(&'a TableStyle),
    Other(&'a OtherStyle),
}

impl<'a> MessageStyle<'a> {
    /// Returns whether this message type should be rendered as markdown.
    /// For `UserMessage`, `xml_tag` selects the per-tag override.
    pub fn uses_markdown(&self, xml_tag: Option<&str>) -> bool {
        match self {
            MessageStyle::UserMessage(s) => s.resolve(xml_tag).uses_markdown,
            MessageStyle::AgentMessage(s) => s.uses_markdown,
            _ => false,
        }
    }

    /// Returns `(indicator_char, indicator_style)` for this message type.
    /// For `UserMessage`, `xml_tag` selects the per-tag override (falling back to the default).
    /// `indicator_style` being `None` means "fall back to the content style".
    pub fn indicator(&self, xml_tag: Option<&str>) -> (Option<&'a str>, Option<&'a SpanStyle>) {
        match self {
            MessageStyle::UserMessage(s) => {
                let t = s.resolve(xml_tag);
                (t.indicator.as_deref(), t.indicator_style.as_ref())
            }
            MessageStyle::AgentMessage(s) => (s.indicator.as_deref(), s.indicator_style.as_ref()),
            MessageStyle::Thinking(s) => (s.indicator.as_deref(), s.indicator_style.as_ref()),
            MessageStyle::ToolCall(s) => {
                if xml_tag == Some("error") {
                    (
                        s.error_indicator.as_deref().or(s.indicator.as_deref()),
                        s.error_indicator_style
                            .as_ref()
                            .or(s.indicator_style.as_ref()),
                    )
                } else {
                    (s.indicator.as_deref(), s.indicator_style.as_ref())
                }
            }
            MessageStyle::ToolResult(s) => {
                if xml_tag == Some("error") {
                    (
                        s.error_indicator.as_deref().or(s.indicator.as_deref()),
                        s.error_indicator_style
                            .as_ref()
                            .or(s.indicator_style.as_ref()),
                    )
                } else {
                    (s.indicator.as_deref(), s.indicator_style.as_ref())
                }
            }
            MessageStyle::TaskSummary(s) => (s.indicator.as_deref(), s.indicator_style.as_ref()),
            MessageStyle::Container(s) => {
                let t = s.resolve(xml_tag);
                (t.indicator.as_deref(), t.indicator_style.as_ref())
            }
            MessageStyle::System(s) => (s.indicator.as_deref(), s.indicator_style.as_ref()),
            MessageStyle::Json(s) => (s.indicator.as_deref(), s.indicator_style.as_ref()),
            MessageStyle::Table(s) => (s.indicator.as_deref(), s.indicator_style.as_ref()),
            MessageStyle::Other(s) => {
                let t = s.resolve(xml_tag);
                (t.indicator.as_deref(), t.indicator_style.as_ref())
            }
        }
    }
}
