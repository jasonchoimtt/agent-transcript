use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub transforms: TransformsConfig,
    pub agents: AgentsConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default)]
    pub widgets: WidgetsConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct WidgetsConfig {
    #[serde(default)]
    pub tool_result: ToolResultWidgetConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ToolResultWidgetConfig {
    #[serde(default)]
    pub file_delta: FileDeltaWidgetConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct FileDeltaWidgetConfig {
    /// Number of context lines to show around each change block. None = show all.
    pub context_lines: Option<usize>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ThemeConfig {
    /// "auto" | "light" | "dark". None / "auto" detects from the host terminal background.
    pub mode: Option<String>,
    /// Name of the palette to use in dark mode. Resolved from
    /// `{config_dir}/agent-transcript/palettes/{name}.toml`. None uses the bundled dark palette.
    pub dark: Option<String>,
    /// Name of the palette to use in light mode. None uses the bundled dark palette.
    pub light: Option<String>,
    /// Name of the styles file. Resolved from `{config_dir}/agent-transcript/styles/{name}.toml`.
    /// None uses the bundled styles.
    pub styles: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(include_str!("default.toml")).expect("built-in default.toml must be valid")
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentsConfig {
    pub claude: AgentConfig,
    pub cursor: AgentConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub binary: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// When true, skip the automatic `--plugin-dir` injection for Claude and rely
    /// on the hook being installed in `~/.claude/settings.json` instead.
    #[serde(default)]
    pub disable_plugin: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TransformsConfig {
    #[serde(default)]
    pub ui_initializer: UiInitializerConfig,
    pub tool_grouper: ToolGrouperConfig,
    #[serde(default)]
    pub tool_formatter: ToolFormatterConfig,
    #[serde(default)]
    pub markdown_splitter: MarkdownSplitterConfig,
    #[serde(default)]
    pub table_converter: TableConverterConfig,
    pub lua: Option<LuaTransformConfig>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct UiInitializerConfig {
    /// Per-MessageType overrides; keys are the enum variant name as a string.
    #[serde(default)]
    pub types: HashMap<String, TypeEntry>,
    /// Fallback flags for unrecognised types.
    #[serde(default)]
    pub default: UiFlags,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiFlags {
    pub expanded: bool,
    pub show_more: bool,
}

impl Default for UiFlags {
    fn default() -> Self {
        Self {
            expanded: true,
            show_more: false,
        }
    }
}

/// Per-MessageType entry: base UI flags plus optional per-tag overrides for this type.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TypeEntry {
    pub expanded: bool,
    pub show_more: bool,
    /// Per-tag overrides for this message type. Each field is optional so an entry can
    /// selectively override only some flags.
    #[serde(default)]
    pub tags: HashMap<String, TagFlags>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TagFlags {
    pub expanded: Option<bool>,
    pub show_more: Option<bool>,
    pub hidden: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolGrouperConfig {
    /// User-defined groups prepended before the built-in defaults.
    #[serde(default)]
    pub groups: Vec<ToolGroup>,
    /// Built-in default groups (populated from default.toml); appended after user groups.
    #[serde(default)]
    pub default_groups: Vec<ToolGroup>,
    /// When true, built-in default groups are excluded entirely.
    #[serde(default)]
    pub disable_defaults: bool,
    /// When true, Thinking messages that arrive between tool calls are absorbed into the
    /// tool group rather than breaking the run.  They do not count toward `min_count`.
    #[serde(default = "ToolGrouperConfig::default_allow_thinking")]
    pub allow_thinking: bool,
}

impl Default for ToolGrouperConfig {
    fn default() -> Self {
        Self {
            groups: vec![],
            default_groups: vec![],
            disable_defaults: false,
            allow_thinking: true,
        }
    }
}

impl ToolGrouperConfig {
    fn default_allow_thinking() -> bool {
        true
    }

    pub fn effective_groups(&self) -> Vec<ToolGroup> {
        let mut groups = self.groups.clone();
        if !self.disable_defaults {
            groups.extend_from_slice(&self.default_groups);
        }
        groups
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolGroup {
    pub name: String,
    pub tools: Vec<String>,
    /// Minimum consecutive matches before grouping fires; default 2.
    #[serde(default = "ToolGroup::default_min_count")]
    pub min_count: usize,
    /// Override the sealed container's `expanded` (children-visible) state.
    /// `true` = always expand; `false` = always collapse; absent = error-aware
    /// (expand if any child has an error tag, otherwise collapse).
    #[serde(default)]
    pub expanded: Option<bool>,
    /// When true, the container label compresses child params as a brace-glob
    /// (e.g. `src/{app.rs,foo/bar.rs}`) instead of a comma-joined list.
    #[serde(default)]
    pub shorten_as_glob: bool,
}

impl ToolGroup {
    fn default_min_count() -> usize {
        2
    }
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct ToolFormatterConfig {
    /// User-defined rules prepended before the built-in defaults.
    #[serde(default)]
    pub rules: Vec<ToolFormatterRule>,
    /// Built-in default rules (populated from default.toml); appended after user rules.
    #[serde(default)]
    pub default_rules: Vec<ToolFormatterRule>,
    /// When true, built-in default rules are excluded entirely.
    #[serde(default)]
    pub disable_defaults: bool,
}

impl ToolFormatterConfig {
    pub fn effective_rules(&self) -> Vec<ToolFormatterRule> {
        let mut rules = self.rules.clone();
        if !self.disable_defaults {
            rules.extend_from_slice(&self.default_rules);
        }
        rules
    }
}

/// A formatting rule for one or more tool names.
#[derive(Debug, Deserialize, Clone)]
pub struct ToolFormatterRule {
    /// Restrict this rule to specific providers (e.g. `["claude", "cursor"]`).
    /// Omit to apply to all providers.
    pub providers: Option<Vec<String>>,
    /// Glob patterns matched against the tool name (e.g. `["Bash", "Read"]`).
    pub tools: Vec<String>,
    /// Template string using `{{key}}` placeholders resolved against the tool's `props`.
    pub template: String,
    /// Override the matched ToolCall's `show_more` state.
    /// `true` = expand even on success; `false` = collapse even on failure; absent = no override.
    #[serde(default)]
    pub expanded: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MarkdownSplitterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub types: Vec<String>,
}

impl Default for MarkdownSplitterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TableConverterConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for TableConverterConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LuaTransformConfig {
    pub script: Option<String>,
    pub script_path: Option<PathBuf>,
}

impl Config {
    pub fn load() -> Self {
        let path = std::env::var("AGENT_TRANSCRIPT_CONFIG")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                xdg_config_dir()
                    .join("agent-transcript")
                    .join("config.toml")
            });

        let base: toml::Value = toml::from_str(include_str!("default.toml"))
            .expect("built-in default.toml must be valid");

        let merged = match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<toml::Value>(&content) {
                Ok(user) => merge_toml(base, user),
                Err(e) => {
                    tracing::warn!("config parse error in {}: {e}", path.display());
                    base
                }
            },
            Err(_) => base,
        };

        merged.try_into().expect("merged config must be valid")
    }
}

/// Recursively merge two TOML values. Tables are merged key-by-key (right wins on conflict);
/// all other types (scalars, arrays) are replaced wholesale by the right-hand side.
fn merge_toml(base: toml::Value, over: toml::Value) -> toml::Value {
    match (base, over) {
        (toml::Value::Table(mut b), toml::Value::Table(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => merge_toml(bv, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            toml::Value::Table(b)
        }
        (_, o) => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let _: Config = toml::from_str(include_str!("default.toml"))
            .expect("default.toml must parse as Config");
    }
}

/// Resolve `$XDG_CONFIG_HOME` or fall back to `$HOME/.config`.
pub(crate) fn xdg_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config")
}
