use std::collections::HashMap;

use crate::config::{TypeEntry, UiInitializerConfig};
use crate::transforms::Transform;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{HiddenState, MessageType};

/// Sets `expanded`, `show_more`, and `hidden` on each Append/Replace/Update message based on its
/// `MessageType` and tag, using the per-type and per-tag rules from `UiInitializerConfig`
/// (populated from `default.toml` and user config overrides).
pub struct UiInitializer {
    /// Merged table of built-in defaults + user overrides. Key = variant name string.
    flags: HashMap<String, TypeEntry>,
    /// Fallback when variant name not in table.
    default_expanded: bool,
    default_show_more: bool,
}

impl UiInitializer {
    pub fn new(config: UiInitializerConfig) -> Self {
        Self {
            flags: config.types,
            default_expanded: config.default.expanded,
            default_show_more: config.default.show_more,
        }
    }

    fn apply(
        &self,
        message_type: &MessageType,
        tag: Option<&str>,
        expanded: &mut bool,
        show_more: &mut bool,
        hidden: &mut HiddenState,
    ) {
        let key = message_type.variant_name();
        let entry = self.flags.get(key);
        *expanded = entry.map_or(self.default_expanded, |e| e.expanded);
        *show_more = entry.map_or(self.default_show_more, |e| e.show_more);

        if let Some(tag) = tag
            && let Some(tf) = entry.and_then(|e| e.tags.get(tag))
        {
            if let Some(h) = tf.hidden {
                *hidden = if h {
                    HiddenState::Hidden
                } else {
                    HiddenState::NotHidden
                };
            }
            if let Some(e) = tf.expanded {
                *expanded = e;
            }
            if let Some(s) = tf.show_more {
                *show_more = s;
            }
        }
    }
}

impl Transform for UiInitializer {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        ops.into_iter()
            .map(|op| match op {
                TreeOperation::Append {
                    parent_id,
                    mut message,
                } => {
                    self.apply(
                        &message.message_type,
                        message.tag.as_deref(),
                        &mut message.expanded,
                        &mut message.show_more,
                        &mut message.hidden,
                    );
                    TreeOperation::Append { parent_id, message }
                }
                TreeOperation::Replace { id, mut message } => {
                    self.apply(
                        &message.message_type,
                        message.tag.as_deref(),
                        &mut message.expanded,
                        &mut message.show_more,
                        &mut message.hidden,
                    );
                    TreeOperation::Replace { id, message }
                }
                TreeOperation::Update { id, mut message } => {
                    self.apply(
                        &message.message_type,
                        message.tag.as_deref(),
                        &mut message.expanded,
                        &mut message.show_more,
                        &mut message.hidden,
                    );
                    TreeOperation::Update { id, message }
                }
                other => other,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, UiInitializerConfig};
    use crate::tree_scroll_view::state::{HiddenState, MessageState, MessageType};

    fn default_init() -> UiInitializer {
        UiInitializer::new(Config::default().transforms.ui_initializer)
    }

    fn make_append(id: &str, mt: MessageType) -> TreeOperation {
        TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id).message_type(mt),
        }
    }

    fn make_append_with_tag(id: &str, mt: MessageType, tag: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id).message_type(mt).tag(tag),
        }
    }

    fn get_message(op: TreeOperation) -> crate::tree_scroll_view::state::MessageState {
        match op {
            TreeOperation::Append { message, .. } => message,
            TreeOperation::Replace { message, .. } => message,
            _ => panic!("not an Append/Replace"),
        }
    }

    #[test]
    fn tool_call_collapsed_by_default() {
        let mut init = default_init();
        let ops = vec![make_append("t", MessageType::ToolCall)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(!msg.show_more);
        assert!(!msg.expanded);
        assert_eq!(msg.hidden, HiddenState::NotHidden);
    }

    #[test]
    fn tool_call_error_tag_expands() {
        let mut init = default_init();
        let ops = vec![make_append_with_tag("t", MessageType::ToolCall, "error")];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(msg.expanded);
        assert_eq!(msg.hidden, HiddenState::NotHidden);
    }

    #[test]
    fn tool_result_collapsed() {
        let mut init = default_init();
        let ops = vec![make_append("r", MessageType::ToolResult)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(!msg.show_more);
        assert!(!msg.expanded);
        assert_eq!(msg.hidden, HiddenState::NotHidden);
    }

    #[test]
    fn thinking_redacted_hidden() {
        let mut init = default_init();
        let ops = vec![make_append_with_tag(
            "th",
            MessageType::Thinking,
            "redacted",
        )];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert_eq!(msg.hidden, HiddenState::Hidden);
    }

    #[test]
    fn thinking_not_redacted_not_hidden() {
        let mut init = default_init();
        let ops = vec![make_append("th", MessageType::Thinking)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert_eq!(msg.hidden, HiddenState::NotHidden);
        assert!(!msg.expanded);
    }

    #[test]
    fn agent_message_show_more_true() {
        let mut init = default_init();
        let ops = vec![make_append("a", MessageType::AgentMessage)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(msg.show_more);
        assert!(msg.expanded);
    }

    #[test]
    fn user_message_show_more_true() {
        let mut init = default_init();
        let ops = vec![make_append("u", MessageType::UserMessage)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(msg.show_more);
        assert!(msg.expanded);
    }

    #[test]
    fn thinking_expanded_false() {
        let mut init = default_init();
        let ops = vec![make_append("th", MessageType::Thinking)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(!msg.expanded);
        assert!(!msg.show_more);
    }

    #[test]
    fn system_expanded_false() {
        let mut init = default_init();
        let ops = vec![make_append("s", MessageType::System)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(!msg.expanded);
        assert!(!msg.show_more);
    }

    #[test]
    fn attachment_tag_collapsed_by_default() {
        let mut init = default_init();
        let ops = vec![make_append_with_tag(
            "a",
            MessageType::UserMessage,
            "attachment",
        )];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert_eq!(
            msg.hidden,
            HiddenState::NotHidden,
            "attachment nodes should be visible"
        );
        assert!(!msg.expanded, "attachment nodes should be collapsed");
        assert!(!msg.show_more, "attachment nodes should not show_more");
    }

    #[test]
    fn attachment_tag_can_be_overridden_via_config() {
        use crate::config::{TagFlags, TypeEntry};
        let mut config = UiInitializerConfig::default();
        let mut tags = std::collections::HashMap::new();
        tags.insert(
            "attachment".to_string(),
            TagFlags {
                expanded: Some(true),
                show_more: Some(true),
                hidden: None,
            },
        );
        config.types.insert(
            "UserMessage".to_string(),
            TypeEntry {
                expanded: true,
                show_more: true,
                tags,
            },
        );
        let mut init = UiInitializer::new(config);
        let ops = vec![make_append_with_tag(
            "a",
            MessageType::UserMessage,
            "attachment",
        )];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(
            msg.expanded,
            "user config should be able to expand attachment nodes"
        );
        assert!(msg.show_more);
    }

    #[test]
    fn custom_config_overrides_default() {
        use crate::config::TypeEntry;
        let mut config = UiInitializerConfig::default();
        config.types.insert(
            "ToolCall".to_string(),
            TypeEntry {
                expanded: false,
                show_more: true,
                tags: Default::default(),
            },
        );
        let mut init = UiInitializer::new(config);
        let ops = vec![make_append("t", MessageType::ToolCall)];
        let out = init.process(ops);
        let msg = get_message(out.into_iter().next().unwrap());
        assert!(!msg.expanded);
        assert!(msg.show_more);
    }

    #[test]
    fn update_applies_same_ui_flags_as_replace() {
        let mut init = default_init();
        let op = TreeOperation::Update {
            id: "a".to_string(),
            message: MessageState::new("a").message_type(MessageType::AgentMessage),
        };
        let out = init.process(vec![op]);
        assert_eq!(out.len(), 1);
        let msg = match out.into_iter().next().unwrap() {
            TreeOperation::Update { message, .. } => message,
            _ => panic!("expected Update"),
        };
        assert!(
            msg.show_more,
            "AgentMessage should get show_more=true via Update"
        );
        assert!(msg.expanded);
    }

    #[test]
    fn update_tool_call_error_tag_expands() {
        let mut init = default_init();
        let op = TreeOperation::Update {
            id: "t".to_string(),
            message: MessageState::new("t")
                .message_type(MessageType::ToolCall)
                .tag("error"),
        };
        let out = init.process(vec![op]);
        let msg = match out.into_iter().next().unwrap() {
            TreeOperation::Update { message, .. } => message,
            _ => panic!("expected Update"),
        };
        assert!(msg.expanded, "error tag should expand tool call via Update");
    }
}
