use glob::Pattern;

use crate::config::{ToolFormatterConfig, ToolFormatterRule};
use crate::providers::ProviderKind;
use crate::transforms::Transform;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{MessageState, MessageType};

pub struct ToolFormatter {
    /// Compiled (patterns, template) pairs, already filtered for this provider.
    rules: Vec<(Vec<Pattern>, String)>,
    workspace_path: Option<std::path::PathBuf>,
}

impl ToolFormatter {
    pub fn new(
        config: ToolFormatterConfig,
        provider: &ProviderKind,
        workspace_path: Option<std::path::PathBuf>,
    ) -> Self {
        let provider_str = provider.to_string();

        let all_rules = config.effective_rules();

        let rules = all_rules
            .into_iter()
            .filter(|r| rule_matches_provider(r, &provider_str))
            .map(|r| {
                let patterns = r
                    .tools
                    .iter()
                    .filter_map(|t| Pattern::new(t).ok())
                    .collect();
                (patterns, r.template)
            })
            .collect();
        Self {
            rules,
            workspace_path,
        }
    }

    fn format_node(&self, msg: &mut MessageState) {
        // Recurse into embedded children first (e.g. tool calls inside a group container).
        for child in &mut msg.children {
            self.format_node(child);
        }

        if msg.message_type != MessageType::ToolCall {
            return;
        }
        // Skip nodes without props — they are either no-arg tools (showing just the name
        // is appropriate) or group container nodes built by tool_grouper (which must not
        // have their text rewritten).
        if msg.props.is_none() {
            return;
        }
        let tool_name = match &msg.text {
            Some(t) => t.clone(),
            None => return,
        };

        let props = msg.props.as_ref();

        // Find the first matching rule.
        let template = self.rules.iter().find_map(|(patterns, tmpl)| {
            if patterns.iter().any(|p| p.matches(&tool_name)) {
                Some(tmpl.as_str())
            } else {
                None
            }
        });

        // Build the first-line summary and per-prop lines.
        let (summary, prop_lines) = build_display(props);

        let first_line = match template {
            Some(tmpl) => {
                let rendered = render_template(tmpl, props, self.workspace_path.as_deref());
                format!("{tool_name}({rendered})")
            }
            None => format!("{tool_name}({summary})"),
        };

        let mut text = first_line;
        for line in prop_lines {
            text.push('\n');
            text.push_str(&line);
        }
        msg.text = Some(text);
    }
}

fn rule_matches_provider(rule: &ToolFormatterRule, provider: &str) -> bool {
    match &rule.providers {
        None => true,
        Some(list) => list.iter().any(|p| p == provider),
    }
}

impl Transform for ToolFormatter {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        ops.into_iter()
            .map(|op| match op {
                TreeOperation::Append {
                    parent_id,
                    mut message,
                } => {
                    self.format_node(&mut message);
                    TreeOperation::Append { parent_id, message }
                }
                TreeOperation::Replace { id, mut message } => {
                    self.format_node(&mut message);
                    TreeOperation::Replace { id, message }
                }
                TreeOperation::Update { id, mut message } => {
                    self.format_node(&mut message);
                    TreeOperation::Update { id, message }
                }
                other => other,
            })
            .collect()
    }
}

/// Render a `{{key}}` or `{{key|filter}}` template against the props JSON object.
/// Missing keys render as empty string. Unknown filters are a no-op.
fn render_template(
    template: &str,
    props: Option<&serde_json::Value>,
    workspace_path: Option<&std::path::Path>,
) -> String {
    let props_obj = props.and_then(|v| v.as_object());
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' && chars.peek() == Some(&'{') {
            chars.next(); // consume second '{'
            let mut inner_str = String::new();
            let mut closed = false;
            while let Some(inner) = chars.next() {
                if inner == '}' && chars.peek() == Some(&'}') {
                    chars.next(); // consume second '}'
                    closed = true;
                    break;
                }
                inner_str.push(inner);
            }
            if closed {
                let (key, filter) = parse_placeholder(inner_str.trim());
                let value = props_obj
                    .and_then(|o| o.get(key))
                    .map(value_to_display)
                    .unwrap_or_default();
                let value = apply_filter(filter, value, workspace_path);
                result.push_str(&value);
            } else {
                // Malformed placeholder — emit as-is.
                result.push_str("{{");
                result.push_str(&inner_str);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Split `"key|filter"` into `("key", Some("filter"))`, or `("key", None)` when no pipe.
fn parse_placeholder(s: &str) -> (&str, Option<&str>) {
    match s.splitn(2, '|').collect::<Vec<_>>()[..] {
        [key, filter] => (key.trim(), Some(filter.trim())),
        _ => (s.trim(), None),
    }
}

fn apply_filter(
    filter: Option<&str>,
    value: String,
    workspace_path: Option<&std::path::Path>,
) -> String {
    match filter {
        None => value,
        Some("path") => filter_path(value, workspace_path),
        Some(_) => value, // unknown filter — pass through unchanged
    }
}

/// Convert an absolute path to a shorter display form:
/// 1. Workspace-relative if under the session's workspace directory (shortest — often inside ~).
/// 2. Home-relative (`~/foo/bar`) if under $HOME.
/// 3. Original absolute path as fallback.
pub(crate) fn filter_path(value: String, workspace_path: Option<&std::path::Path>) -> String {
    use std::path::Path;

    let path = Path::new(&value);
    if !path.is_absolute() {
        return value;
    }

    // Workspace-relative takes priority — it is often inside ~, so it produces shorter paths.
    if let Some(ws) = workspace_path
        && let Ok(rel) = path.strip_prefix(ws)
    {
        let rel_str = rel.to_string_lossy().into_owned();
        if rel_str.len() < value.len() {
            return rel_str;
        }
    }

    // Fall back to home-relative.
    let home = std::env::var("HOME").ok();
    if let Some(home_str) = &home {
        let home_path = Path::new(home_str);
        if let Ok(rel) = path.strip_prefix(home_path) {
            let rel_str = rel.to_string_lossy();
            return if rel_str.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rel_str}")
            };
        }
    }

    value
}

/// Build `(key=val, key=val, …)` inline summary and individual `key=value` prop lines.
/// Returns `(summary_content, vec_of_lines)`.
fn build_display(props: Option<&serde_json::Value>) -> (String, Vec<String>) {
    let Some(obj) = props.and_then(|v| v.as_object()) else {
        return (String::new(), vec![]);
    };

    let mut prop_lines: Vec<String> = Vec::new();
    let mut inline_parts: Vec<String> = Vec::new();

    for (k, v) in obj {
        let val_str = value_to_display(v);
        prop_lines.push(format!("{k}={val_str}"));
        // Inline summary: truncate each value to 60 chars.
        let short: String = val_str.chars().take(60).collect();
        inline_parts.push(format!("{k}={short}"));
    }

    let summary = inline_parts.join(", ");
    (summary, prop_lines)
}

fn value_to_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolFormatterRule;
    use crate::providers::ProviderKind;
    use crate::tree_scroll_view::state::MessageType;

    fn make_tool_call(name: &str, props: Option<serde_json::Value>) -> MessageState {
        let mut m = MessageState::new("tc:1")
            .text(name)
            .message_type(MessageType::ToolCall);
        if let Some(p) = props {
            m = m.props(p);
        }
        m
    }

    #[test]
    fn parse_placeholder_no_filter() {
        assert_eq!(parse_placeholder("file_path"), ("file_path", None));
    }

    #[test]
    fn parse_placeholder_with_filter() {
        assert_eq!(
            parse_placeholder("file_path|path"),
            ("file_path", Some("path"))
        );
    }

    #[test]
    fn parse_placeholder_trims_spaces() {
        assert_eq!(
            parse_placeholder(" file_path | path "),
            ("file_path", Some("path"))
        );
    }

    #[test]
    fn filter_path_home_relative() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        let abs = format!("{home}/projects/foo.rs");
        let result = filter_path(abs, None);
        assert_eq!(result, "~/projects/foo.rs");
    }

    #[test]
    fn filter_path_non_absolute_unchanged() {
        assert_eq!(
            filter_path("relative/path".to_string(), None),
            "relative/path"
        );
    }

    #[test]
    fn filter_path_workspace_relative() {
        let result = filter_path(
            "/workspace/src/main.rs".to_string(),
            Some(std::path::Path::new("/workspace")),
        );
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn render_template_with_path_filter() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        let abs = format!("{home}/src/main.rs");
        let result = render_template(
            "{{file_path|path}}",
            Some(&serde_json::json!({"file_path": abs})),
            None,
        );
        assert_eq!(result, "~/src/main.rs");
    }

    #[test]
    fn render_template_unknown_filter_passthrough() {
        let result = render_template(
            "{{key|unknown}}",
            Some(&serde_json::json!({"key": "value"})),
            None,
        );
        assert_eq!(result, "value");
    }

    #[test]
    fn render_template_substitutes_key() {
        let result = render_template(
            "{{command}}",
            Some(&serde_json::json!({"command": "git status"})),
            None,
        );
        assert_eq!(result, "git status");
    }

    #[test]
    fn render_template_missing_key_is_empty() {
        let result = render_template(
            "{{missing}}",
            Some(&serde_json::json!({"other": "x"})),
            None,
        );
        assert_eq!(result, "");
    }

    #[test]
    fn render_template_literal_passes_through() {
        let result = render_template("no placeholders", None, None);
        assert_eq!(result, "no placeholders");
    }

    #[test]
    fn formatter_applies_matching_rule() {
        let config = ToolFormatterConfig {
            rules: vec![ToolFormatterRule {
                providers: None,
                tools: vec!["Bash".to_string()],
                template: "{{command}}".to_string(),
            }],
            disable_defaults: true,
            ..Default::default()
        };
        let mut fmt = ToolFormatter::new(config, &ProviderKind::Claude, None);
        let msg = make_tool_call("Bash", Some(serde_json::json!({"command": "git status"})));
        let ops = fmt.process(vec![TreeOperation::Append {
            parent_id: None,
            message: msg,
        }]);
        let TreeOperation::Append { message, .. } = &ops[0] else {
            panic!("expected Append");
        };
        let text = message.text.as_deref().unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("Bash(git status)"));
        assert_eq!(lines.next(), Some("command=git status"));
    }

    #[test]
    fn formatter_default_fallback_when_no_rule() {
        let cfg = ToolFormatterConfig {
            disable_defaults: true,
            ..Default::default()
        };
        let mut fmt = ToolFormatter::new(cfg, &ProviderKind::Claude, None);
        let msg = make_tool_call(
            "Read",
            Some(serde_json::json!({"path": "/foo", "limit": "10"})),
        );
        let ops = fmt.process(vec![TreeOperation::Append {
            parent_id: None,
            message: msg,
        }]);
        let TreeOperation::Append { message, .. } = &ops[0] else {
            panic!("expected Append");
        };
        let text = message.text.as_deref().unwrap();
        let first_line = text.lines().next().unwrap();
        assert!(first_line.starts_with("Read("), "first line: {first_line}");
        assert!(first_line.ends_with(')'), "first line: {first_line}");
        // prop lines should be present
        let lines: Vec<_> = text.lines().skip(1).collect();
        assert!(!lines.is_empty());
    }

    #[test]
    fn formatter_no_props_leaves_text_unchanged() {
        let mut fmt =
            ToolFormatter::new(ToolFormatterConfig::default(), &ProviderKind::Claude, None);
        let msg = make_tool_call("Unknown", None);
        let ops = fmt.process(vec![TreeOperation::Append {
            parent_id: None,
            message: msg,
        }]);
        let TreeOperation::Append { message, .. } = &ops[0] else {
            panic!("expected Append");
        };
        // No props → text is left as the plain tool name (not corrupted with "()").
        assert_eq!(message.text.as_deref(), Some("Unknown"));
    }

    #[test]
    fn formatter_recurses_into_children() {
        use crate::tree_scroll_view::state::MessageType as MT;
        let config = ToolFormatterConfig {
            rules: vec![crate::config::ToolFormatterRule {
                providers: None,
                tools: vec!["Bash".to_string()],
                template: "{{command}}".to_string(),
            }],
            disable_defaults: true,
            ..Default::default()
        };
        let mut fmt = ToolFormatter::new(config, &ProviderKind::Claude, None);
        // Simulate a container node (group) that has a ToolCall child embedded in it.
        let child = make_tool_call("Bash", Some(serde_json::json!({"command": "ls"})));
        let container = MessageState::new("container:0")
            .message_type(MT::ToolCall) // as built by tool_grouper
            .text("Tool calls: 1 tool calls")
            .children(vec![child]);
        let ops = fmt.process(vec![TreeOperation::Replace {
            id: "old:0".to_string(),
            message: container,
        }]);
        let TreeOperation::Replace { message, .. } = &ops[0] else {
            panic!("expected Replace");
        };
        // Container text must not be corrupted.
        assert_eq!(message.text.as_deref(), Some("Tool calls: 1 tool calls"));
        // Child must be formatted.
        let child = &message.children[0];
        assert_eq!(
            child.text.as_deref().and_then(|t| t.lines().next()),
            Some("Bash(ls)")
        );
    }

    #[test]
    fn provider_filter_excludes_other_providers() {
        let config = ToolFormatterConfig {
            rules: vec![ToolFormatterRule {
                providers: Some(vec!["claude".to_string()]),
                tools: vec!["WebSearch".to_string()],
                template: "{{query}}".to_string(),
            }],
            disable_defaults: true,
            ..Default::default()
        };
        // Rule is for "claude" only; cursor provider should not apply it.
        let mut fmt = ToolFormatter::new(config, &ProviderKind::Cursor, None);
        let msg = make_tool_call(
            "WebSearch",
            Some(serde_json::json!({"query": "rust async"})),
        );
        let ops = fmt.process(vec![TreeOperation::Append {
            parent_id: None,
            message: msg,
        }]);
        let TreeOperation::Append { message, .. } = &ops[0] else {
            panic!("expected Append");
        };
        // Rule was excluded, so falls back to key=val default format.
        let first_line = message.text.as_deref().unwrap().lines().next().unwrap();
        assert!(
            first_line.starts_with("WebSearch("),
            "first line: {first_line}"
        );
        assert!(first_line.contains("query="), "first line: {first_line}");
    }

    #[test]
    fn default_rules_applied_for_claude() {
        // With defaults enabled, Bash should use {{command}} template automatically.
        let mut fmt = ToolFormatter::new(
            crate::config::Config::default().transforms.tool_formatter,
            &ProviderKind::Claude,
            None,
        );
        let msg = make_tool_call("Bash", Some(serde_json::json!({"command": "ls -la"})));
        let ops = fmt.process(vec![TreeOperation::Append {
            parent_id: None,
            message: msg,
        }]);
        let TreeOperation::Append { message, .. } = &ops[0] else {
            panic!("expected Append");
        };
        let first_line = message.text.as_deref().unwrap().lines().next().unwrap();
        assert_eq!(first_line, "Bash(ls -la)");
    }

    #[test]
    fn default_bash_rule_excluded_for_cursor() {
        // Bash default rule has providers = ["claude"]; should not fire for cursor.
        let mut fmt = ToolFormatter::new(
            crate::config::Config::default().transforms.tool_formatter,
            &ProviderKind::Cursor,
            None,
        );
        let msg = make_tool_call(
            "Bash",
            Some(serde_json::json!({"command": "echo hello world"})),
        );
        let ops = fmt.process(vec![TreeOperation::Append {
            parent_id: None,
            message: msg,
        }]);
        let TreeOperation::Append { message, .. } = &ops[0] else {
            panic!("expected Append");
        };
        // Falls back to key=val format, NOT "Bash(echo hello world)".
        let first_line = message.text.as_deref().unwrap().lines().next().unwrap();
        assert!(first_line.starts_with("Bash("));
        assert!(first_line.contains("command="));
    }

    #[test]
    fn formatter_applies_rule_to_update() {
        let config = ToolFormatterConfig {
            rules: vec![ToolFormatterRule {
                providers: None,
                tools: vec!["Bash".to_string()],
                template: "{{command}}".to_string(),
            }],
            disable_defaults: true,
            ..Default::default()
        };
        let mut fmt = ToolFormatter::new(config, &ProviderKind::Claude, None);
        let msg = make_tool_call("Bash", Some(serde_json::json!({"command": "ls -la"})));
        let ops = fmt.process(vec![TreeOperation::Update {
            id: "old-id".to_string(),
            message: msg,
        }]);
        let TreeOperation::Update { message, .. } = &ops[0] else {
            panic!("expected Update");
        };
        let first_line = message.text.as_deref().unwrap().lines().next().unwrap();
        assert_eq!(
            first_line, "Bash(ls -la)",
            "Update tool text should be formatted"
        );
    }
}
