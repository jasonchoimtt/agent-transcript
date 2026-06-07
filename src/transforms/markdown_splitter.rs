use std::collections::HashMap;

use pulldown_cmark::{Event, Options, Parser};

use crate::config::MarkdownSplitterConfig;
use crate::transforms::Transform;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{MessageState, MessageType};

/// Rewrites AgentMessage (or other configured) text nodes by splitting on
/// CommonMark block boundaries into paragraph children under a Container.
///
/// Disabled by default; enabled via config (`markdown_splitter.enabled = true`).
pub struct MarkdownSplitter {
    /// Variant names of MessageTypes to split.
    types: Vec<String>,
    /// Tracks split nodes: original_id → SplitRecord.
    split_nodes: HashMap<String, SplitRecord>,
}

struct SplitRecord {
    paragraph_texts: Vec<String>,
    count: usize,
}

impl MarkdownSplitter {
    pub fn new(config: MarkdownSplitterConfig) -> Self {
        Self {
            types: config.types,
            split_nodes: HashMap::new(),
        }
    }

    fn should_split(&self, message_type: &MessageType) -> bool {
        let name = message_type.variant_name();
        self.types.iter().any(|t| t == name)
    }

    /// Emit ops for a new split node: an Append of the container at `parent_id` followed by
    /// an Append per paragraph. Falls back to a plain Append if the text has fewer than 2 blocks.
    fn create_split_node(
        &mut self,
        parent_id: Option<String>,
        message: MessageState,
        output: &mut Vec<TreeOperation>,
    ) {
        let text = message.text.as_deref().unwrap_or_default();
        let paragraphs = split_paragraphs(text);
        if paragraphs.len() < 2 {
            output.push(TreeOperation::Append { parent_id, message });
            return;
        }
        let container_id = get_container_id(&message.id);
        output.push(TreeOperation::Append {
            parent_id,
            message: build_container(container_id.clone(), &message),
        });
        for (i, text_slice) in paragraphs.iter().enumerate() {
            output.push(TreeOperation::Append {
                parent_id: Some(container_id.clone()),
                message: build_para(&message.id, i, text_slice),
            });
        }
        self.split_nodes.insert(
            message.id,
            SplitRecord {
                paragraph_texts: paragraphs.iter().map(|s| s.to_string()).collect(),
                count: paragraphs.len(),
            },
        );
    }

    /// Re-diff the paragraph children of an already-split node against new text.
    /// Emits Remove/Replace/Append ops for paragraph children as needed and updates the record.
    fn diff_split_node(&mut self, id: &str, new_text: &str, output: &mut Vec<TreeOperation>) {
        let new_paragraphs = split_paragraphs(new_text);
        let container_id = get_container_id(id);
        let record = self.split_nodes.get(id).unwrap();

        let common = record
            .paragraph_texts
            .iter()
            .zip(new_paragraphs.iter())
            .take_while(|(a, b)| a.as_str() == **b)
            .count();

        for i in new_paragraphs.len()..record.count {
            output.push(TreeOperation::Remove {
                id: get_para_id(&id, i),
            });
        }

        for (abs_i, new_text_slice) in new_paragraphs.iter().enumerate().skip(common) {
            output.push(TreeOperation::Replace {
                id: get_para_id(&id, abs_i),
                message: build_para(&id, abs_i, new_text_slice),
            });
        }

        for (abs_i, new_text_slice) in new_paragraphs.iter().enumerate().skip(record.count) {
            output.push(TreeOperation::Append {
                parent_id: Some(container_id.clone()),
                message: build_para(&id, abs_i, new_text_slice),
            });
        }

        let record = self.split_nodes.get_mut(id).unwrap();
        record.paragraph_texts = new_paragraphs.iter().map(|s| s.to_string()).collect();
        record.count = new_paragraphs.len();
    }

    /// Promote an unsplit node to a container if `new_text` has 2+ paragraphs.
    /// Returns true if promotion happened (caller should not also push the original op).
    fn promote_to_container(
        &mut self,
        message: &MessageState,
        new_text: &str,
        output: &mut Vec<TreeOperation>,
    ) -> bool {
        let new_paragraphs = split_paragraphs(new_text);
        if new_paragraphs.len() < 2 {
            return false;
        }
        let original_id = &message.id;
        let container_id = get_container_id(original_id);
        output.push(TreeOperation::Replace {
            id: original_id.clone(),
            message: build_container(container_id.clone(), message),
        });
        for (i, text_slice) in new_paragraphs.iter().enumerate() {
            output.push(TreeOperation::Append {
                parent_id: Some(container_id.clone()),
                message: build_para(&message.id, i, text_slice),
            });
        }
        self.split_nodes.insert(
            original_id.clone(),
            SplitRecord {
                paragraph_texts: new_paragraphs.iter().map(|s| s.to_string()).collect(),
                count: new_paragraphs.len(),
            },
        );
        true
    }

    fn process_op(&mut self, op: TreeOperation, output: &mut Vec<TreeOperation>) {
        match op {
            TreeOperation::Append { parent_id, message }
                if self.should_split(&message.message_type) =>
            {
                self.create_split_node(parent_id, message, output);
            }

            TreeOperation::Replace { id, message } if self.split_nodes.contains_key(&id) => {
                let new_text = message.text.as_deref().unwrap_or_default();
                self.diff_split_node(&id, new_text, output);
            }

            // Replace for a message not yet split: if it now has 2+ paragraphs, promote it
            // to a container in-place (e.g. streaming:pending growing past one paragraph).
            TreeOperation::Replace { id, message } if self.should_split(&message.message_type) => {
                let new_text = message.text.as_deref().unwrap_or_default();
                if !self.promote_to_container(&message, new_text, output) {
                    output.push(TreeOperation::Replace { id, message });
                }
            }

            TreeOperation::Update { id, message } if self.split_nodes.contains_key(&id) => {
                let new_text = message.text.as_deref().unwrap_or_default();
                self.diff_split_node(&id, new_text, output);
            }

            TreeOperation::Update { id, message } if self.should_split(&message.message_type) => {
                let new_text = message.text.as_deref().unwrap_or_default();
                if !self.promote_to_container(&message, new_text, output) {
                    output.push(TreeOperation::Update { id, message });
                }
            }

            TreeOperation::Remove { ref id } => {
                // Provider removed the original message: remove the container (its children
                // are tree-children, so one Remove cascades through them in the tree state).
                if self.split_nodes.remove(id).is_some() {
                    output.push(TreeOperation::Remove {
                        id: get_container_id(id),
                    });
                    return;
                }
                output.push(op);
            }

            other => {
                output.push(other);
            }
        }
    }
}

impl Transform for MarkdownSplitter {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        let mut output = Vec::new();
        for op in ops {
            self.process_op(op, &mut output);
        }
        output
    }

    fn reset(&mut self) {
        self.split_nodes.clear();
    }
}

/// Split markdown text into top-level CommonMark block slices.
/// Returns <2 items for single-block or empty text (no splitting needed).
pub fn split_paragraphs(text: &str) -> Vec<&str> {
    let iter = Parser::new_ext(text, Options::all()).into_offset_iter();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut block_start = 0usize;

    for (event, range) in iter {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    block_start = range.start;
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    let slice = text[block_start..range.end].trim();
                    if !slice.is_empty() {
                        out.push(slice);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn get_container_id(original_id: &str) -> String {
    format!("{original_id}:md")
}

fn build_container(id: String, source: &MessageState) -> MessageState {
    let mut container = source.clone();
    container.id = id;
    container
        .message_type(MessageType::Container)
        .group(true)
        .expanded(true)
        .indent_children(false)
}

fn get_para_id(original_id: &str, index: usize) -> String {
    format!("{original_id}:{index}")
}

fn build_para(original_id: &str, index: usize, text_slice: &str) -> MessageState {
    let id = get_para_id(original_id, index);
    MessageState::new(id)
        .text(text_slice.to_string())
        .message_type(MessageType::AgentMessage)
        .show_more(true)
        .show_indicator(index == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_paragraph_passthrough() {
        let config = MarkdownSplitterConfig {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        };
        let mut splitter = MarkdownSplitter::new(config);
        let op = TreeOperation::Append {
            parent_id: None,
            message: MessageState::new("a")
                .message_type(MessageType::AgentMessage)
                .text("hello world"),
        };
        let out = splitter.process(vec![op]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], TreeOperation::Append { .. }));
    }

    #[test]
    fn two_paragraphs_split() {
        let config = MarkdownSplitterConfig {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        };
        let mut splitter = MarkdownSplitter::new(config);
        let op = TreeOperation::Append {
            parent_id: None,
            message: MessageState::new("a")
                .message_type(MessageType::AgentMessage)
                .text("para1\n\npara2"),
        };
        let out = splitter.process(vec![op]);
        // Expect: Append(Container at root), Append(para1 child), Append(para2 child).
        // No Replace — the original Append is NOT emitted first.
        assert!(
            !out.iter()
                .any(|op| matches!(op, TreeOperation::Replace { .. }))
        );
        let root_appends: Vec<_> = out
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    TreeOperation::Append {
                        parent_id: None,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            root_appends.len(),
            1,
            "exactly one root Append (the Container)"
        );
        let child_appends: Vec<_> = out
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    TreeOperation::Append {
                        parent_id: Some(_),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(child_appends.len(), 2);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn split_paragraphs_fn() {
        let text = "first\n\nsecond\n\nthird";
        let parts = split_paragraphs(text);
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn replace_single_to_multi_splits_in_place() {
        let config = MarkdownSplitterConfig {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        };
        let mut splitter = MarkdownSplitter::new(config);

        // Seed the tree with a single-paragraph AgentMessage (not split).
        let append_op = TreeOperation::Append {
            parent_id: None,
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("only one para"),
        };
        let out = splitter.process(vec![append_op]);
        assert_eq!(out.len(), 1, "single-para Append should pass through");

        // Now replace it with a two-paragraph message.
        let replace_op = TreeOperation::Replace {
            id: "msg".to_string(),
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("para1\n\npara2"),
        };
        let out = splitter.process(vec![replace_op]);

        // Expect: Replace(msg → container), Append(para1), Append(para2).
        assert_eq!(
            out.len(),
            3,
            "should produce container Replace + 2 child Appends"
        );
        assert!(
            matches!(&out[0], TreeOperation::Replace { id, .. } if id == "msg"),
            "first op should Replace the original node with the container"
        );
        let child_appends: Vec<_> = out
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    TreeOperation::Append {
                        parent_id: Some(_),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            child_appends.len(),
            2,
            "two paragraph children should be appended"
        );

        // A further Replace with 3 paragraphs should use the existing split record.
        let replace_op2 = TreeOperation::Replace {
            id: "msg".to_string(),
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("para1\n\npara2\n\npara3"),
        };
        let out2 = splitter.process(vec![replace_op2]);
        let appends2: Vec<_> = out2
            .iter()
            .filter(|op| matches!(op, TreeOperation::Append { .. }))
            .collect();
        assert_eq!(
            appends2.len(),
            1,
            "only the new third paragraph should be appended"
        );
    }

    #[test]
    fn update_rediffs_split_node() {
        // Seed a split node via Append.
        let config = MarkdownSplitterConfig {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        };
        let mut splitter = MarkdownSplitter::new(config);
        splitter.process(vec![TreeOperation::Append {
            parent_id: None,
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("para1\n\npara2"),
        }]);

        // Update with a third paragraph added.
        let out = splitter.process(vec![TreeOperation::Update {
            id: "msg".to_string(),
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("para1\n\npara2\n\npara3"),
        }]);

        // para1 and para2 are unchanged (common=2); only para3 is appended.
        let appends: Vec<_> = out
            .iter()
            .filter(|op| matches!(op, TreeOperation::Append { .. }))
            .collect();
        assert_eq!(appends.len(), 1, "only new para3 should be appended");
        assert!(
            !out.iter()
                .any(|op| matches!(op, TreeOperation::Remove { .. })),
            "no paragraphs should be removed"
        );
    }

    #[test]
    fn update_promotes_unsplit_node_when_text_grows() {
        let config = MarkdownSplitterConfig {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        };
        let mut splitter = MarkdownSplitter::new(config);

        // Seed with single-paragraph node (not split).
        splitter.process(vec![TreeOperation::Append {
            parent_id: None,
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("only one para"),
        }]);

        // Update grows it to 2 paragraphs → should promote to container.
        let out = splitter.process(vec![TreeOperation::Update {
            id: "msg".to_string(),
            message: MessageState::new("msg")
                .message_type(MessageType::AgentMessage)
                .text("para1\n\npara2"),
        }]);

        // Expect: Replace(msg → container), Append(para1), Append(para2).
        assert_eq!(
            out.len(),
            3,
            "should produce container Replace + 2 child Appends"
        );
        assert!(
            matches!(&out[0], TreeOperation::Replace { id, .. } if id == "msg"),
            "first op should replace the original node with a container"
        );
    }

    #[test]
    fn update_passthrough_for_non_split_type() {
        let config = MarkdownSplitterConfig {
            enabled: true,
            types: vec!["AgentMessage".to_string()],
        };
        let mut splitter = MarkdownSplitter::new(config);
        let op = TreeOperation::Update {
            id: "u".to_string(),
            message: MessageState::new("u").message_type(MessageType::UserMessage),
        };
        let out = splitter.process(vec![op]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], TreeOperation::Update { .. }));
    }
}
