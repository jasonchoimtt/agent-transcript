use ratatui::layout::Rect;
use serde_json::Value;

use crate::tree_scroll_view::state::get_node;
use crate::tree_scroll_view::{MessageState, MessageType, TreeScrollViewState};

const KIND_OBJECT: &str = "object";
const KIND_ARRAY: &str = "array";
const KIND_STRING: &str = "string";
const KIND_NUMBER: &str = "number";
const KIND_BOOL: &str = "bool";
const KIND_NULL: &str = "null";
const KIND_RAW: &str = "raw";

pub struct DataViewState {
    pub tree: TreeScrollViewState,
    /// Popup rect (including border) set during each render pass; used for mouse hit-testing.
    pub popup_area: Rect,
}

impl DataViewState {
    pub fn new(data: &str) -> Self {
        Self {
            tree: build_tree(data),
            popup_area: Rect::default(),
        }
    }

    pub fn from_nodes(nodes: Vec<MessageState>) -> Self {
        Self {
            tree: TreeScrollViewState::new_without_terminal(nodes),
            popup_area: Rect::default(),
        }
    }

    pub fn toggle_display(&mut self) {
        let path = self.tree.selection_index.clone();
        let is_container = get_node(&self.tree.items, &path)
            .map(|n| matches!(n.data.as_str(), KIND_OBJECT | KIND_ARRAY))
            .unwrap_or(false);
        if is_container {
            self.tree.toggle_expand();
        } else {
            self.tree.toggle_show_more();
        }
    }
}

fn build_tree(data: &str) -> TreeScrollViewState {
    let nodes = match serde_json::from_str::<Value>(data) {
        Ok(value) => vec![json_to_node(&value, None, "")],
        Err(_) => vec![
            MessageState::new("root")
                .text(data.to_string())
                .data(KIND_RAW.to_string())
                .message_type(MessageType::Json),
        ],
    };
    TreeScrollViewState::new_without_terminal(nodes)
}

/// Build a `MessageState` node for `value`.
/// `key` is the parent object key or array index label, used for the display prefix.
/// `path` is the dot-separated JSON path used as the node ID (e.g. `"a.b.0"`).
fn json_to_node(value: &Value, key: Option<&str>, path: &str) -> MessageState {
    let id = if path.is_empty() {
        "root".to_string()
    } else {
        path.to_string()
    };
    let key_prefix = key.map(|k| format!("{k}: ")).unwrap_or_default();

    match value {
        Value::Object(map) => {
            let one_line = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
            let children = map
                .iter()
                .map(|(k, v)| {
                    let child_path = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    json_to_node(v, Some(k.as_str()), &child_path)
                })
                .collect();
            MessageState::new(id)
                .text(format!("{key_prefix}{one_line}"))
                .data(KIND_OBJECT.to_string())
                .message_type(MessageType::Json)
                .children(children)
                .expanded(true)
        }
        Value::Array(arr) => {
            let one_line = serde_json::to_string(value).unwrap_or_else(|_| "[]".into());
            let children = arr
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let child_path = if path.is_empty() {
                        i.to_string()
                    } else {
                        format!("{path}.{i}")
                    };
                    json_to_node(v, Some(&i.to_string()), &child_path)
                })
                .collect();
            MessageState::new(id)
                .text(format!("{key_prefix}{one_line}"))
                .data(KIND_ARRAY.to_string())
                .message_type(MessageType::Json)
                .children(children)
                .expanded(true)
        }
        Value::String(s) => {
            let quoted = serde_json::to_string(value).unwrap_or_else(|_| format!("\"{s}\""));
            // text: key on its own line, raw value on subsequent lines so the
            // height calculation and show_more rendering both get the right line count.
            let text = match key {
                Some(k) => format!("{k}:\n{s}"),
                None => s.clone(),
            };
            let brief = format!("{key_prefix}{quoted}");
            MessageState::new(id)
                .text(text)
                .brief(brief)
                .data(KIND_STRING.to_string())
                .message_type(MessageType::Json)
                .show_more(false)
        }
        Value::Bool(_) => MessageState::new(id)
            .text(format!("{key_prefix}{value}"))
            .data(KIND_BOOL.to_string())
            .message_type(MessageType::Json),
        Value::Null => MessageState::new(id)
            .text(format!("{key_prefix}null"))
            .data(KIND_NULL.to_string())
            .message_type(MessageType::Json),
        _ => MessageState::new(id)
            .text(format!("{key_prefix}{value}"))
            .data(KIND_NUMBER.to_string())
            .message_type(MessageType::Json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tree_nested_object() {
        let json = r#"{"name":"alice","age":30,"tags":["a","b"]}"#;
        let state = build_tree(json);
        assert_eq!(state.items.len(), 1);
        let root = &state.items[0];
        assert_eq!(root.id, "root");
        assert_eq!(root.message_type, MessageType::Json);
        assert_eq!(root.data, KIND_OBJECT);
        assert!(root.expanded);
        assert!(!root.show_more);
        assert!(root.text.as_deref().unwrap().contains("alice"));
        // three children: name, age, tags
        assert_eq!(root.children.len(), 3);
        // children use key as path
        let tags = root.children.iter().find(|c| c.id == "tags");
        assert!(tags.is_some());
        let tags = tags.unwrap();
        assert_eq!(tags.message_type, MessageType::Json);
        assert_eq!(tags.data, KIND_ARRAY);
        assert_eq!(tags.children.len(), 2);
        // nested array children use indexed path
        assert_eq!(tags.children[0].id, "tags.0");
        assert_eq!(tags.children[1].id, "tags.1");
    }

    #[test]
    fn build_tree_invalid_json() {
        let state = build_tree("not json");
        assert_eq!(state.items.len(), 1);
        let node = &state.items[0];
        assert_eq!(node.id, "root");
        assert_eq!(node.message_type, MessageType::Json);
        assert_eq!(node.data, KIND_RAW);
        assert_eq!(node.text.as_deref(), Some("not json"));
    }

    #[test]
    fn build_tree_string_node_has_brief() {
        let json = r#"{"msg":"hello\nworld"}"#;
        let state = build_tree(json);
        let root = &state.items[0];
        let child = &root.children[0];
        assert_eq!(child.id, "msg");
        assert_eq!(child.message_type, MessageType::Json);
        assert_eq!(child.data, KIND_STRING);
        // text: key line + newline + raw unescaped value
        assert_eq!(child.text.as_deref(), Some("msg:\nhello\nworld"));
        // brief is JSON-quoted (escaped)
        assert!(child.brief.as_deref().unwrap().contains("\\n"));
        assert!(!child.show_more);
    }

    #[test]
    fn nested_object_path() {
        let json = r#"{"a":{"b":{"c":1}}}"#;
        let state = build_tree(json);
        let root = &state.items[0];
        assert_eq!(root.id, "root");
        let a = &root.children[0];
        assert_eq!(a.id, "a");
        let b = &a.children[0];
        assert_eq!(b.id, "a.b");
        let c = &b.children[0];
        assert_eq!(c.id, "a.b.c");
    }
}
