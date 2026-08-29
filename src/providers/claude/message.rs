use std::collections::{HashMap, HashSet};

use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{HiddenState, MessageState, MessageType};

#[derive(Default)]
pub struct ParseState {
    pub seen_uuids: HashSet<String>,
    pub seen_block_ids: HashSet<String>,
    pub pending_tool_calls: HashMap<String, serde_json::Value>,
    pub id_prefix: String,
    /// UUIDs of `compact_boundary` system entries; used to tag the injected summary user message.
    pub compact_boundary_uuids: HashSet<String>,
}

/// Parse one JSONL line and return the resulting tree operations.
pub fn parse_entry(line: &str, state: &mut ParseState) -> color_eyre::Result<Vec<TreeOperation>> {
    parse_entry_cb(line, state, |_, _, _| Ok(vec![]))
}

/// Like [`parse_entry`] but calls `on_agent_tool(tool_use_id, value, is_result)` each time an
/// `Agent` tool_use block is encountered, before returning. Used by the live-streaming layer to
/// detect new Agent calls without a separate post-hoc scan of `pending_tool_calls`.
pub(super) fn parse_entry_cb(
    line: &str,
    state: &mut ParseState,
    mut on_agent_tool: impl FnMut(&str, &str, bool) -> color_eyre::Result<Vec<TreeOperation>>,
) -> color_eyre::Result<Vec<TreeOperation>> {
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(vec![]);
    };

    let entry_type = obj["type"].as_str().unwrap_or("");
    let uuid = obj["uuid"].as_str().unwrap_or("").to_string();
    let prefix = state.id_prefix.clone();

    match entry_type {
        "summary" if !uuid.is_empty() => {
            let text = obj["summary"].as_str().unwrap_or("").to_string();
            let node_id = format!("{}task_summary:{}", prefix, uuid);
            Ok(vec![TreeOperation::Append {
                parent_id: None,
                message: MessageState::new(node_id)
                    .text(text)
                    .data(obj.to_string())
                    .message_type(MessageType::TaskSummary),
            }])
        }

        "user" => {
            let msg = &obj["message"];
            let content = &msg["content"];

            if let Some(arr) = content.as_array() {
                let has_tool_result = arr
                    .iter()
                    .any(|b| b["type"].as_str() == Some("tool_result"));
                if has_tool_result {
                    return handle_tool_results(
                        &obj,
                        arr.clone(),
                        state,
                        &prefix,
                        &mut on_agent_tool,
                    );
                }
            }

            // Task-notification: a system-injected user turn that reports a bg agent result.
            if let Some(text) = content.as_str()
                && text.trim_start().starts_with("<task-notification>")
            {
                if uuid.is_empty() {
                    return Ok(vec![]);
                }
                return Ok(handle_task_notification(text, &obj, uuid, state, &prefix));
            }

            // Plain user message
            if uuid.is_empty() {
                return Ok(vec![]);
            }
            Ok(emit_user_message(&obj, content, uuid, state, &prefix))
        }

        "assistant" => {
            let msg = &obj["message"];
            let msg_id = msg["id"].as_str().unwrap_or("").to_string();
            if msg_id.is_empty() {
                return Ok(vec![]);
            }
            emit_assistant_blocks(msg, msg_id, uuid, state, &prefix, &mut on_agent_tool)
        }

        "attachment" if !uuid.is_empty() => Ok(emit_attachment_node(&obj, uuid, state, &prefix)),

        "system" => {
            // Track compact_boundary entries
            if obj["subtype"].as_str() == Some("compact_boundary") {
                state.compact_boundary_uuids.insert(uuid.clone());
            }

            Ok(vec![])
        }

        _ => Ok(vec![]),
    }
}

/// Extract the text content of `<tag>…</tag>` from `xml`. Returns `None` when the
/// tag is absent.  Handles only the first occurrence; does not unescape entities.
fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(open.as_str())? + open.len();
    let end = xml[start..].find(close.as_str())? + start;
    Some(&xml[start..end])
}

/// Handle a `<task-notification>` user message: show just the `<summary>` tag text
/// as the visible user message, and backfill the matching TaskSummary node with the
/// `<result>` from the notification.
fn handle_task_notification(
    text: &str,
    obj: &serde_json::Value,
    uuid: String,
    state: &mut ParseState,
    prefix: &str,
) -> Vec<TreeOperation> {
    let summary = extract_xml_tag(text, "summary")
        .unwrap_or("Agent task completed")
        .to_string();
    let result = extract_xml_tag(text, "result").map(|s| s.to_string());
    let tool_use_id = extract_xml_tag(text, "tool-use-id").map(|s| s.to_string());

    let summary_val = serde_json::Value::String(summary);
    let mut ops = emit_user_message(obj, &summary_val, uuid, state, prefix);

    if let (Some(result_text), Some(tuid)) = (result, tool_use_id) {
        let summary_id = format!("{}task_summary:{}", prefix, tuid);
        ops.push(TreeOperation::Replace {
            id: summary_id.clone(),
            message: MessageState::new(summary_id)
                .text(result_text)
                .data(obj.to_string())
                .message_type(MessageType::TaskSummary),
        });
    }

    ops
}

fn attachment_label(att: &serde_json::Value) -> String {
    let att_type = att["type"].as_str().unwrap_or("");
    match att_type {
        "file" | "already_read_file" => {
            let path = att_display_path(att, "filename");
            let header = format!("[File: {path}]");
            match att["content"]["file"]["content"].as_str() {
                Some(content) if !content.is_empty() => format!("{header}\n{content}"),
                _ => header,
            }
        }
        "compact_file_reference" => {
            format!("[File: {}]", att_display_path(att, "filename"))
        }
        "edited_text_file" => {
            let name = att_display_path(att, "filename");
            let header = format!("[File: {name} (edited)]");
            match att["snippet"].as_str() {
                Some(snippet) if !snippet.is_empty() => format!("{header}\n{snippet}"),
                _ => header,
            }
        }
        "directory" => {
            let path = att_display_path(att, "path");
            let header = format!("[Directory: {path}]");
            match att["content"].as_str() {
                Some(content) if !content.is_empty() => format!("{header}\n{content}"),
                _ => header,
            }
        }
        "skill_listing" => {
            let n = att["skillCount"].as_u64().unwrap_or(0);
            let header = format!("[Skill listing: {n} skills]");
            match att["content"].as_str() {
                Some(content) if !content.is_empty() => format!("{header}\n{content}"),
                _ => header,
            }
        }
        "deferred_tools_delta" => {
            let names: Vec<&str> = att["addedNames"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .take(5)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if names.is_empty() {
                "[Tools loaded]".to_string()
            } else {
                format!("[Tools: +{}]", names.join(", +"))
            }
        }
        "command_permissions" => "[Command permissions]".to_string(),
        "task_reminder" => {
            let n = att["itemCount"].as_u64().unwrap_or(0);
            format!("[Task reminder: {n} tasks]")
        }
        "queued_command" => {
            let first_line = att["prompt"]
                .as_str()
                .and_then(|s| s.lines().next())
                .unwrap_or("command");
            format!("[Queued: {first_line}]")
        }
        "invoked_skills" => {
            let names: Vec<&str> = att["skills"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s["name"].as_str())
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            if names.is_empty() {
                "[Invoked skills]".to_string()
            } else {
                format!("[Skills: {}]", names.join(", "))
            }
        }
        "date_change" => {
            let d = att["newDate"].as_str().unwrap_or("?");
            format!("[Date: {d}]")
        }
        other => format!("[{other}]"),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Returns `displayPath` if present, otherwise the basename of `fallback_field`.
fn att_display_path(att: &serde_json::Value, fallback_field: &str) -> String {
    att["displayPath"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            att[fallback_field]
                .as_str()
                .map(basename)
                .unwrap_or(fallback_field)
                .to_string()
        })
}

fn emit_attachment_node(
    obj: &serde_json::Value,
    uuid: String,
    state: &mut ParseState,
    prefix: &str,
) -> Vec<TreeOperation> {
    let att = &obj["attachment"];
    let text = attachment_label(att);
    let node_id = format!("{}attachment:{}", prefix, uuid);

    let node = MessageState::new(node_id.clone())
        .text(text)
        .data(obj.to_string())
        .message_type(MessageType::UserMessage)
        .tag("attachment");

    if state.seen_uuids.insert(uuid) {
        vec![TreeOperation::Append {
            parent_id: None,
            message: node,
        }]
    } else {
        vec![TreeOperation::Replace {
            id: node_id,
            message: node,
        }]
    }
}

fn emit_user_message(
    obj: &serde_json::Value,
    content: &serde_json::Value,
    uuid: String,
    state: &mut ParseState,
    prefix: &str,
) -> Vec<TreeOperation> {
    let raw_text = extract_content_text(content);
    let (text, xml_tag) = match process_xml_tags(&raw_text) {
        Some((display, tag)) => (display, Some(tag)),
        None => (raw_text, None),
    };
    let user_msg_id = format!("{}user_msg:{}", prefix, uuid);
    let is_meta = obj["isMeta"].as_bool().unwrap_or(false);
    let is_compaction_summary = obj["parentUuid"]
        .as_str()
        .map(|p| state.compact_boundary_uuids.contains(p))
        .unwrap_or(false);

    let mut node = MessageState::new(user_msg_id.clone())
        .text(text)
        .data(obj.to_string())
        .message_type(MessageType::UserMessage)
        .hidden(if is_meta {
            HiddenState::Hidden
        } else {
            HiddenState::NotHidden
        });
    if let Some(tag) = xml_tag {
        node = node.tag(tag);
    }
    if is_compaction_summary {
        node = node.tag("summary").brief("[Conversation summary]");
    }

    if state.seen_uuids.insert(uuid) {
        vec![TreeOperation::Append {
            parent_id: None,
            message: node,
        }]
    } else {
        vec![TreeOperation::Replace {
            id: user_msg_id,
            message: node,
        }]
    }
}

fn emit_assistant_blocks(
    msg: &serde_json::Value,
    msg_id: String,
    uuid: String,
    state: &mut ParseState,
    prefix: &str,
    on_agent_tool: &mut impl FnMut(&str, &str, bool) -> color_eyre::Result<Vec<TreeOperation>>,
) -> color_eyre::Result<Vec<TreeOperation>> {
    let content_arr = msg["content"].as_array().cloned().unwrap_or_default();
    if !uuid.is_empty() {
        state.seen_uuids.insert(uuid);
    }

    let mut ops = Vec::new();

    for (idx, block) in content_arr.iter().enumerate() {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "thinking" => {
                let text = block["thinking"].as_str().unwrap_or("").to_string();
                let node_id = format!("{}thinking:{}:{}", prefix, msg_id, idx);
                let node = MessageState::new(node_id.clone())
                    .text(text)
                    .data(block.to_string())
                    .message_type(MessageType::Thinking);
                if state.seen_block_ids.insert(node_id.clone()) {
                    ops.push(TreeOperation::Append {
                        parent_id: None,
                        message: node,
                    });
                } else {
                    ops.push(TreeOperation::Replace {
                        id: node_id,
                        message: node,
                    });
                }
            }
            "text" => {
                let text = block["text"].as_str().unwrap_or("").to_string();
                let node_id = format!("{}text:{}:{}", prefix, msg_id, idx);
                let node = MessageState::new(node_id.clone())
                    .text(text)
                    .data(block.to_string())
                    .message_type(MessageType::AgentMessage);
                if state.seen_block_ids.insert(node_id.clone()) {
                    ops.push(TreeOperation::Append {
                        parent_id: None,
                        message: node,
                    });
                } else {
                    ops.push(TreeOperation::Replace {
                        id: node_id,
                        message: node,
                    });
                }
            }
            "tool_use" => {
                let tool_id = block["id"].as_str().unwrap_or("").to_string();
                let tool_name = block["name"].as_str().unwrap_or("?");
                let node_id = format!("{}tool_call:{}", prefix, tool_id);
                let mut node = MessageState::new(node_id.clone())
                    .text(tool_name)
                    .data(block.to_string())
                    .message_type(MessageType::ToolCall);
                if !block["input"].is_null() {
                    node = node.props(block["input"].clone());
                }
                if state.seen_block_ids.insert(node_id.clone()) {
                    ops.push(TreeOperation::Append {
                        parent_id: None,
                        message: node,
                    });
                } else {
                    ops.push(TreeOperation::Replace {
                        id: node_id,
                        message: node,
                    });
                }
                if !tool_id.is_empty() {
                    state
                        .pending_tool_calls
                        .insert(tool_id.clone(), block.clone());
                    if tool_name == "Agent" {
                        let description = block["input"]["description"].as_str().unwrap_or("");
                        ops.extend(on_agent_tool(&tool_id, description, false)?);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(ops)
}

fn handle_tool_results(
    entry: &serde_json::Value,
    content_arr: Vec<serde_json::Value>,
    state: &mut ParseState,
    prefix: &str,
    mut on_agent_tool: impl FnMut(&str, &str, bool) -> color_eyre::Result<Vec<TreeOperation>>,
) -> color_eyre::Result<Vec<TreeOperation>> {
    let mut ops = Vec::new();

    for block in &content_arr {
        if block["type"].as_str() != Some("tool_result") {
            continue;
        }
        let tool_use_id = block["tool_use_id"].as_str().unwrap_or("");
        if tool_use_id.is_empty() {
            continue;
        }

        let Some(call_block) = state.pending_tool_calls.remove(tool_use_id) else {
            continue; // orphan result
        };

        let old_node_id = format!("{}tool_call:{}", prefix, tool_use_id);

        if let Some(agent_id) = entry["toolUseResult"]["agentId"].as_str() {
            ops.extend(on_agent_tool(tool_use_id, agent_id, true)?);

            let is_async = entry["toolUseResult"]["isAsync"].as_bool().unwrap_or(false);

            let result_text = if is_async {
                "Async agent launched".to_string()
            } else {
                entry["toolUseResult"]["content"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|b| b["text"].as_str())
                    .unwrap_or("")
                    .to_string()
            };

            let task_summary_id = format!("{}task_summary:{}", prefix, tool_use_id);

            let description = call_block["input"]["description"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let call_name = call_block["name"].as_str().unwrap_or("?");
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            let status_tag = if is_error { "error" } else { "success" };

            ops.push(TreeOperation::Update {
                id: old_node_id.clone(),
                message: MessageState::new(old_node_id.clone())
                    .text(format!("{call_name}({description})"))
                    .data(call_block.to_string())
                    .message_type(MessageType::ToolCall)
                    .tag(status_tag)
                    .indent_children(true),
            });
            ops.push(TreeOperation::Append {
                parent_id: Some(old_node_id),
                message: MessageState::new(task_summary_id)
                    .text(result_text.clone())
                    .data(entry.to_string())
                    .message_type(MessageType::TaskSummary),
            })
        } else {
            // Regular tool result: tag the ToolCall and append ToolResult as child.
            let result_text = extract_tool_result_text(&block["content"]);
            let call_name = call_block["name"].as_str().unwrap_or("?");
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            let status_tag = if is_error { "error" } else { "success" };

            let mut replace_node = MessageState::new(old_node_id.clone())
                .text(call_name)
                .data(call_block.to_string())
                .message_type(MessageType::ToolCall)
                .tag(status_tag);
            if !call_block["input"].is_null() {
                replace_node = replace_node.props(call_block["input"].clone());
            }
            ops.push(TreeOperation::Replace {
                id: old_node_id.clone(),
                message: replace_node,
            });

            let result_id = format!("{}tool_result:{}", prefix, tool_use_id);
            ops.push(TreeOperation::Append {
                parent_id: Some(old_node_id),
                message: MessageState::new(result_id)
                    .text(result_text)
                    .data(entry.to_string())
                    .message_type(MessageType::ToolResult)
                    .tag(status_tag),
            });
        }
    }

    Ok(ops)
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Parse one XML element at the start of `text`.
/// Returns `(tag_name, inner_content_trimmed, remaining_text)`.
/// Only handles simple tags (no attributes, no self-closing, no comments).
fn parse_one_xml_tag(text: &str) -> Option<(&str, &str, &str)> {
    if !text.starts_with('<') {
        return None;
    }
    let close_bracket = text[1..].find('>')? + 1;
    let tag_name = &text[1..close_bracket];
    if tag_name.is_empty()
        || tag_name.contains(' ')
        || tag_name.starts_with('/')
        || tag_name.starts_with('!')
        || tag_name.ends_with('/')
    {
        return None;
    }
    let inner_start = close_bracket + 1;
    let close_tag = format!("</{tag_name}>");
    let close_at = text[inner_start..].find(close_tag.as_str())? + inner_start;
    let inner = text[inner_start..close_at].trim();
    let rest = &text[close_at + close_tag.len()..];
    Some((tag_name, inner, rest))
}

/// Collect all consecutive XML tags from the start of `text` (whitespace between tags is ignored).
/// Returns `(tags, remainder)` where `remainder` is any unparsed text after the tags.
/// Returns `None` if no tags are found.
fn collect_leading_xml_tags(text: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut result = Vec::new();
    let mut remaining = text.trim_start();
    while remaining.starts_with('<') {
        match parse_one_xml_tag(remaining) {
            Some((tag, inner, rest)) => {
                result.push((tag.to_string(), inner.to_string()));
                remaining = rest.trim_start();
            }
            None => break,
        }
    }
    if result.is_empty() {
        None
    } else {
        Some((result, remaining))
    }
}

/// Process XML tag(s) at the start of `text` into `(display_text, primary_tag)`.
///
/// Rules applied in order:
/// - `command-name` present: format as `"{command-name} {command-args}"`, tag `"command-name"`.
///   (`command-message` content is dropped.)
/// - `bash-stdout` present: join stdout and stderr with `\n`, tag `"bash-stdout"`.
/// - General: concatenate non-empty inner content with `\n`, use first tag name.
///
/// Returns `None` if the text doesn't start with any XML tags.
fn process_xml_tags(text: &str) -> Option<(String, String)> {
    let (tags, remainder) = collect_leading_xml_tags(text)?;

    if tags.iter().any(|(t, _)| t == "command-name") {
        let name = tags
            .iter()
            .find(|(t, _)| t == "command-name")
            .map(|(_, c)| c.as_str())
            .unwrap_or("");
        let args = tags
            .iter()
            .find(|(t, _)| t == "command-args")
            .map(|(_, c)| c.as_str())
            .unwrap_or("");
        let display = if args.is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
        return Some((display, "command-name".to_string()));
    }

    if tags.iter().any(|(t, _)| t == "bash-stdout") {
        let stdout = tags
            .iter()
            .find(|(t, _)| t == "bash-stdout")
            .map(|(_, c)| c.as_str())
            .unwrap_or("");
        let stderr = tags
            .iter()
            .find(|(t, _)| t == "bash-stderr")
            .map(|(_, c)| c.as_str())
            .unwrap_or("");
        let display = [stdout, stderr]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        return Some((display, "bash-stdout".to_string()));
    }

    let first_tag = tags[0].0.clone();
    let mut parts: Vec<String> = tags
        .iter()
        .map(|(_, c)| c.clone())
        .filter(|c| !c.is_empty())
        .collect();
    if !remainder.is_empty() {
        parts.push(remainder.to_string());
    }
    Some((parts.join("\n"), first_tag))
}

fn extract_content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                if b["type"].as_str() == Some("text") {
                    b["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_tool_result_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.chars().take(500).collect(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                if b["type"].as_str() == Some("text") {
                    b["text"]
                        .as_str()
                        .map(|s| s.chars().take(500).collect::<String>())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn append_ids(ops: &[TreeOperation]) -> Vec<String> {
        ops.iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                _ => None,
            })
            .collect()
    }

    fn all_appends_at_root(ops: &[TreeOperation]) -> bool {
        ops.iter().all(|op| match op {
            TreeOperation::Append { parent_id, .. } => parent_id.is_none(),
            _ => true,
        })
    }

    fn has_container(ops: &[TreeOperation]) -> bool {
        ops.iter().any(|op| match op {
            TreeOperation::Append { message, .. } => message.message_type == MessageType::Container,
            _ => false,
        })
    }

    #[test]
    fn test_plain_user_message_appends_at_root() {
        let entry = serde_json::json!({
            "type": "user",
            "uuid": "uuid-001",
            "parentUuid": null,
            "isSidechain": false,
            "message": { "role": "user", "content": "Hello world" }
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();

        let ids: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                _ => None,
            })
            .collect();

        assert!(ids.iter().any(|id| id.starts_with("user_msg:")));
        assert!(
            all_appends_at_root(&ops),
            "user message should be appended at root level"
        );
        assert!(!has_container(&ops), "no Container nodes should be emitted");
    }

    #[test]
    fn test_user_message_visible_by_default() {
        let entry = serde_json::json!({
            "type": "user",
            "uuid": "uuid-002",
            "parentUuid": null,
            "message": { "role": "user", "content": "Real message" }
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();

        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        assert!(!user_msg.unwrap().hidden.is_hidden());
    }

    #[test]
    fn test_meta_user_message_hidden() {
        let entry = serde_json::json!({
            "type": "user",
            "uuid": "uuid-003",
            "parentUuid": null,
            "isMeta": true,
            "message": { "role": "user", "content": "<local-command-caveat>caveat</local-command-caveat>" }
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();

        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        assert!(user_msg.unwrap().hidden.is_hidden());
    }

    #[test]
    fn test_assistant_text_block() {
        let entry = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-a01",
            "parentUuid": "uuid-001",
            "message": {
                "role": "assistant",
                "id": "msg_001",
                "content": [{"type": "text", "text": "Hi there"}]
            }
        })
        .to_string();

        let mut state = ParseState::default();

        let ops = parse_entry(&entry, &mut state).unwrap();
        let ids: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                _ => None,
            })
            .collect();
        assert!(ids.iter().any(|id| id.starts_with("text:")));
    }

    #[test]
    fn test_assistant_thinking_block() {
        let entry = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-a02",
            "parentUuid": "uuid-001",
            "message": {
                "role": "assistant",
                "id": "msg_002",
                "content": [{"type": "thinking", "thinking": "I'm thinking"}]
            }
        })
        .to_string();

        let mut state = ParseState::default();

        let ops = parse_entry(&entry, &mut state).unwrap();
        let msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("thinking:") => {
                Some(message)
            }
            _ => None,
        });
        assert!(msg.is_some());
        assert_eq!(msg.unwrap().message_type, MessageType::Thinking);
    }

    #[test]
    fn test_tool_call_emitted() {
        let entry = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-a03",
            "parentUuid": "uuid-001",
            "message": {
                "role": "assistant",
                "id": "msg_003",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "Bash",
                    "input": {"command": "ls"}
                }]
            }
        })
        .to_string();

        let mut state = ParseState::default();

        let ops = parse_entry(&entry, &mut state).unwrap();
        let tool_op = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id == "tool_call:toolu_abc" => {
                Some(message)
            }
            _ => None,
        });
        assert!(tool_op.is_some());
        assert!(state.pending_tool_calls.contains_key("toolu_abc"));
    }

    #[test]
    fn test_tool_result_replaces_call_and_appends_result() {
        let mut state = ParseState::default();
        state.pending_tool_calls.insert(
            "toolu_abc".to_string(),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_abc",
                "name": "Bash",
                "input": {"command": "ls"}
            }),
        );

        let entry = serde_json::json!({
            "type": "user",
            "uuid": "uuid-tr01",
            "parentUuid": "uuid-a03",
            "message": {
                "role": "user",
                "content": [{
                    "tool_use_id": "toolu_abc",
                    "type": "tool_result",
                    "content": "file1.txt\nfile2.txt",
                    "is_error": false
                }]
            }
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();
        assert!(!ops.is_empty());

        // First op: Replace the ToolCall node (call text only, tagged success).
        match &ops[0] {
            TreeOperation::Replace { id, message } => {
                assert_eq!(id, "tool_call:toolu_abc");
                assert_eq!(message.tag.as_deref(), Some("success"));
                assert!(
                    !message.text.as_deref().unwrap_or("").contains("file1.txt"),
                    "ToolCall text should be call-only, not merged with result"
                );
            }
            _ => panic!("expected Replace as first op"),
        }

        // Second op: Append ToolResult as child of ToolCall.
        match &ops[1] {
            TreeOperation::Append { parent_id, message } => {
                assert_eq!(parent_id.as_deref(), Some("tool_call:toolu_abc"));
                assert_eq!(message.message_type, MessageType::ToolResult);
                assert_eq!(message.tag.as_deref(), Some("success"));
                assert!(message.text.as_deref().unwrap_or("").contains("file1.txt"));
            }
            _ => panic!("expected Append as second op"),
        }
    }

    #[test]
    fn test_consecutive_user_messages_both_append_at_root() {
        let mut state = ParseState::default();

        let u1 = serde_json::json!({
            "type": "user",
            "uuid": "u1",
            "parentUuid": null,
            "message": {"role": "user", "content": "hello"}
        })
        .to_string();
        let u2 = serde_json::json!({
            "type": "user",
            "uuid": "u2",
            "parentUuid": "u1",
            "message": {"role": "user", "content": "follow up"}
        })
        .to_string();

        let mut ops = parse_entry(&u1, &mut state).unwrap();
        ops.extend(parse_entry(&u2, &mut state).unwrap());

        assert_eq!(
            append_ids(&ops),
            vec!["user_msg:u1", "user_msg:u2"],
            "back-to-back user messages must not be split or wrapped"
        );
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops));
    }

    #[test]
    fn test_tool_result_nests_under_its_tool_call() {
        let mut state = ParseState::default();
        state.pending_tool_calls.insert(
            "toolu_xyz".to_string(),
            serde_json::json!({"type": "tool_use", "id": "toolu_xyz", "name": "Read", "input": {}}),
        );

        let entry = serde_json::json!({
            "type": "user",
            "uuid": "tr-uuid",
            "parentUuid": "some-parent",
            "message": {
                "role": "user",
                "content": [{"tool_use_id": "toolu_xyz", "type": "tool_result", "content": "ok"}]
            }
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();

        // A tool result is not a new user prompt: it produces no root-level node.
        for op in &ops {
            if let TreeOperation::Append { parent_id, .. } = op {
                assert_eq!(parent_id.as_deref(), Some("tool_call:toolu_xyz"));
            }
        }
        assert!(!has_container(&ops));
    }

    /// A scripted round-trip: everything the agent emits lands at root level, except
    /// tool results, which nest under the tool call they answer.
    #[test]
    fn test_round_trip_emits_flat_tree() {
        let mut state = ParseState::default();
        let entries = [
            serde_json::json!({
                "type": "user", "uuid": "rt-u1", "parentUuid": null,
                "message": {"role": "user", "content": "please read the file"}
            }),
            serde_json::json!({
                "type": "assistant", "uuid": "rt-a1", "parentUuid": "rt-u1",
                "message": {"role": "assistant", "id": "rt_msg1", "content": [
                    {"type": "text", "text": "sure"},
                    {"type": "tool_use", "id": "rt_tool", "name": "Read", "input": {}}
                ]}
            }),
            serde_json::json!({
                "type": "user", "uuid": "rt-tr", "parentUuid": "rt-a1",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "rt_tool", "content": "contents"}
                ]}
            }),
            serde_json::json!({
                "type": "user", "uuid": "rt-u2", "parentUuid": "rt-tr",
                "message": {"role": "user", "content": "thanks"}
            }),
        ];

        let mut ops = Vec::new();
        for entry in &entries {
            ops.extend(parse_entry(&entry.to_string(), &mut state).unwrap());
        }

        assert!(!has_container(&ops), "no turn containers should be emitted");
        for op in &ops {
            if let TreeOperation::Append { parent_id, message } = op {
                match parent_id.as_deref() {
                    None => {}
                    Some("tool_call:rt_tool") => {
                        assert_eq!(message.message_type, MessageType::ToolResult)
                    }
                    Some(other) => panic!("unexpected parent {other} for {}", message.id),
                }
            }
        }
    }

    #[test]
    fn test_subagent_prefix_still_flattens() {
        let entry = serde_json::json!({
            "type": "user",
            "uuid": "sa-uuid-001",
            "parentUuid": null,
            "message": {"role": "user", "content": "do something"}
        })
        .to_string();

        let mut state = ParseState {
            id_prefix: "sa:abc:".to_string(),
            ..Default::default()
        };
        let ops = parse_entry(&entry, &mut state).unwrap();

        assert_eq!(append_ids(&ops), vec!["sa:abc:user_msg:sa-uuid-001"]);
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops));
    }

    #[test]
    fn test_assistant_blocks_append_at_root() {
        let entry = serde_json::json!({
            "type": "assistant",
            "uuid": "sa-a01",
            "parentUuid": "sa-uuid-001",
            "message": {
                "role": "assistant",
                "id": "sa-msg-001",
                "content": [
                    {"type": "thinking", "thinking": "hmm"},
                    {"type": "text", "text": "result"}
                ]
            }
        })
        .to_string();

        let mut state = ParseState {
            id_prefix: "sa:abc:".to_string(),
            ..Default::default()
        };
        let ops = parse_entry(&entry, &mut state).unwrap();

        assert!(!has_container(&ops));
        assert!(all_appends_at_root(&ops));

        let thinking_id = "sa:abc:thinking:sa-msg-001:0";
        let text_id = "sa:abc:text:sa-msg-001:1";
        let ids: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&thinking_id.to_string()));
        assert!(ids.contains(&text_id.to_string()));
    }

    #[test]
    fn test_duplicate_uuid_emits_replace() {
        let entry = serde_json::json!({
            "type": "user",
            "uuid": "dup-uuid",
            "parentUuid": null,
            "message": {"role": "user", "content": "hello again"}
        })
        .to_string();

        let mut state = ParseState::default();
        // Pre-populate as if already seen
        state.seen_uuids.insert("dup-uuid".to_string());

        let ops = parse_entry(&entry, &mut state).unwrap();
        let has_replace = ops
            .iter()
            .any(|op| matches!(op, TreeOperation::Replace { .. }));
        assert!(has_replace, "duplicate uuid should emit Replace");
    }

    #[test]
    fn test_duplicate_block_id_emits_replace() {
        let mut state = ParseState::default();
        // Pre-populate as if the block was already appended to the tree.
        state.seen_block_ids.insert("text:msg_dup:0".to_string());

        let entry = serde_json::json!({
            "type": "assistant",
            "uuid": "new-uuid",
            "parentUuid": "prev",
            "message": {
                "role": "assistant",
                "id": "msg_dup",
                "content": [{"type": "text", "text": "updated text"}]
            }
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();
        let has_replace = ops
            .iter()
            .any(|op| matches!(op, TreeOperation::Replace { id, .. } if id.starts_with("text:")));
        assert!(has_replace, "already-seen block id should emit Replace");
    }

    #[test]
    fn test_split_msg_id_appends_new_blocks() {
        // Claude Code streams thinking and text as separate JSONL entries sharing a msg_id.
        // The second entry (text-only) must be Append'd, not Replace'd.
        let mut state = ParseState::default();

        let thinking_entry = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-a",
            "parentUuid": "uuid-user",
            "message": {
                "role": "assistant",
                "id": "msg_shared",
                "content": [{"type": "thinking", "thinking": "hmm"}]
            }
        })
        .to_string();

        let text_entry = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-b",
            "parentUuid": "uuid-a",
            "message": {
                "role": "assistant",
                "id": "msg_shared",
                "content": [{"type": "text", "text": "answer"}]
            }
        })
        .to_string();

        parse_entry(&thinking_entry, &mut state).unwrap();
        let ops = parse_entry(&text_entry, &mut state).unwrap();

        let text_appended = ops.iter().any(|op| match op {
            TreeOperation::Append { message, .. } => message.id == "text:msg_shared:0",
            _ => false,
        });
        assert!(
            text_appended,
            "text block on second entry with same msg_id must be Append'd"
        );
    }

    #[test]
    fn test_agent_tool_result_expands_subagent() {
        let agent_id = "deadbeef1234";
        let tool_use_id = "toolu_agent_test";

        let mut state = ParseState::default();
        state.pending_tool_calls.insert(
            tool_use_id.to_string(),
            serde_json::json!({
                "type": "tool_use",
                "id": tool_use_id,
                "name": "Agent",
                "input": {"description": "do the thing", "prompt": "..."}
            }),
        );

        let entry = serde_json::json!({
            "type": "user",
            "uuid": "result-uuid",
            "parentUuid": "prev",
            "toolUseResult": {
                "agentId": agent_id,
                "content": [{"type": "text", "text": "Final result from agent"}]
            },
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": "Final result from agent"
                }]
            }
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();

        let append_op = ops.iter().find(|op| {
            matches!(op, TreeOperation::Append { parent_id: Some(p), .. } if p == &format!("tool_call:{}", tool_use_id))
        });
        assert!(
            append_op.is_some(),
            "should emit Append on tool_call node; got {} ops",
            ops.len()
        );

        if let Some(TreeOperation::Append { message, .. }) = append_op {
            assert_eq!(message.message_type, MessageType::TaskSummary);
            assert!(message.indent_children);

            assert!(
                message
                    .text
                    .as_deref()
                    .unwrap_or("")
                    .contains("Final result"),
                "task_summary should carry the result text"
            );
        }
    }

    #[test]
    fn test_rewind_initial_read_excludes_abandoned_branch() {
        // Two branches from the same parent, simulating a rewind
        let line_a = serde_json::json!({
            "type": "user", "uuid": "root", "parentUuid": null,
            "message": {"role": "user", "content": "hi"}
        })
        .to_string();
        let line_b1 = serde_json::json!({
            "type": "user", "uuid": "branch1", "parentUuid": "root",
            "message": {"role": "user", "content": "old message"}
        })
        .to_string();
        let line_b2 = serde_json::json!({
            "type": "user", "uuid": "branch2", "parentUuid": "root",
            "message": {"role": "user", "content": "new message after rewind"}
        })
        .to_string();

        // Build parent_of as the two-pass reader would
        let mut parent_of = HashMap::new();
        let mut last_uuid = None;
        for line in [&line_a, &line_b1, &line_b2] {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(uuid) = obj["uuid"].as_str() {
                    parent_of.insert(
                        uuid.to_string(),
                        obj["parentUuid"].as_str().map(|s| s.to_string()),
                    );
                    last_uuid = Some(uuid.to_string());
                }
            }
        }

        // Build active_path by tracing backward from last_uuid to root
        let mut active_path: HashSet<String> = HashSet::new();
        if let Some(ref last) = last_uuid {
            let mut current = Some(last.as_str());
            while let Some(uuid) = current {
                active_path.insert(uuid.to_string());
                current = parent_of.get(uuid).and_then(|p| p.as_deref());
            }
        }

        assert!(active_path.contains("root"));
        assert!(
            !active_path.contains("branch1"),
            "abandoned branch1 must be excluded"
        );
        assert!(
            active_path.contains("branch2"),
            "post-rewind branch2 must be included"
        );
    }

    #[test]
    fn test_task_notification_shows_summary_and_updates_task_summary_node() {
        let mut state = ParseState::default();

        let tool_use_id = "toolu_async_001";
        let notification_content = format!(
            "<task-notification>\n\
             <task-id>agent123</task-id>\n\
             <tool-use-id>{tool_use_id}</tool-use-id>\n\
             <status>completed</status>\n\
             <summary>Agent \"My task\" completed</summary>\n\
             <result>The actual result content here.</result>\n\
             </task-notification>"
        );

        let entry = serde_json::json!({
            "type": "user",
            "uuid": "notif-uuid",
            "parentUuid": "prev",
            "message": {"role": "user", "content": notification_content}
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();

        // Should produce a UserMessage with just the summary text.
        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        assert!(user_msg.is_some(), "should emit a UserMessage");
        assert_eq!(
            user_msg.unwrap().text.as_deref(),
            Some("Agent \"My task\" completed"),
            "user message text should be the summary"
        );

        // Should also emit a Replace for the TaskSummary node.
        let append_op = ops.iter().find(|op| {
            matches!(op, TreeOperation::Replace { message, .. }
            if message.id == format!("task_summary:{tool_use_id}"))
        });
        assert!(
            append_op.is_some(),
            "should emit Replace for task_summary:{tool_use_id}"
        );

        if let Some(TreeOperation::Replace { message, .. }) = append_op {
            assert_eq!(
                message.text.as_deref(),
                Some("The actual result content here."),
                "task_summary should contain the <result> text"
            );
            assert_eq!(message.message_type, MessageType::TaskSummary);
        }
    }

    #[test]
    fn test_compaction_summary_user_gets_tag() {
        let boundary_uuid = "boundary-001";
        let summary_uuid = "summary-001";

        let mut state = ParseState::default();
        state
            .compact_boundary_uuids
            .insert(boundary_uuid.to_string());

        let entry = serde_json::json!({
            "type": "user",
            "uuid": summary_uuid,
            "parentUuid": boundary_uuid,
            "message": {
                "role": "user",
                "content": "This session is being continued from a previous conversation. Summary: ..."
            }
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();

        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        let msg = user_msg.expect("should emit a user message node");
        assert_eq!(
            msg.tag.as_deref(),
            Some("summary"),
            "should have summary tag"
        );
        assert_eq!(
            msg.brief.as_deref(),
            Some("[Conversation summary]"),
            "should have compaction summary brief"
        );
    }

    #[test]
    fn test_process_xml_tags_command_pattern() {
        let text = "<command-message>make-plan</command-message>\n<command-name>/make-plan</command-name>\n<command-args>args go here\ncan be multiple lines</command-args>";
        let (display, tag) = process_xml_tags(text).expect("should parse command pattern");
        assert_eq!(tag, "command-name");
        assert_eq!(display, "/make-plan args go here\ncan be multiple lines");
    }

    #[test]
    fn test_process_xml_tags_command_name_only() {
        let text = "<command-message>foo</command-message>\n<command-name>/foo</command-name>";
        let (display, tag) = process_xml_tags(text).expect("should parse command without args");
        assert_eq!(tag, "command-name");
        assert_eq!(display, "/foo");
    }

    #[test]
    fn test_process_xml_tags_bash_stdout_and_stderr() {
        let text = "<bash-stdout>line 1\nline2</bash-stdout><bash-stderr>err</bash-stderr>";
        let (display, tag) = process_xml_tags(text).expect("should parse bash stdout+stderr");
        assert_eq!(tag, "bash-stdout");
        assert_eq!(display, "line 1\nline2\nerr");
    }

    #[test]
    fn test_process_xml_tags_bash_stdout_only() {
        let text = "<bash-stdout>output</bash-stdout>";
        let (display, tag) = process_xml_tags(text).expect("should parse bash stdout only");
        assert_eq!(tag, "bash-stdout");
        assert_eq!(display, "output");
    }

    #[test]
    fn test_process_xml_tags_bash_stderr_only() {
        let text = "<bash-stderr>err only</bash-stderr>";
        // bash-stderr alone doesn't trigger the bash-stdout branch; falls into general case
        let (display, tag) = process_xml_tags(text).expect("should parse bash stderr only");
        assert_eq!(tag, "bash-stderr");
        assert_eq!(display, "err only");
    }

    #[test]
    fn test_process_xml_tags_general_single() {
        let text = "<local-command-caveat>caveat text</local-command-caveat>";
        let (display, tag) = process_xml_tags(text).expect("should parse single general tag");
        assert_eq!(tag, "local-command-caveat");
        assert_eq!(display, "caveat text");
    }

    #[test]
    fn test_process_xml_tags_general_multiple() {
        let text = "<foo>hello</foo>\n<bar>world</bar>";
        let (display, tag) = process_xml_tags(text).expect("should parse multiple general tags");
        assert_eq!(tag, "foo");
        assert_eq!(display, "hello\nworld");
    }

    #[test]
    fn test_process_xml_tags_general_with_remainder() {
        let text = "<foo>hello</foo>\ntrailing plain text";
        let (display, tag) = process_xml_tags(text).expect("should parse tag with remainder");
        assert_eq!(tag, "foo");
        assert_eq!(display, "hello\ntrailing plain text");
    }

    #[test]
    fn test_process_xml_tags_no_tags() {
        assert!(process_xml_tags("plain text").is_none());
        assert!(process_xml_tags("").is_none());
    }

    // ── attachment tests ──────────────────────────────────────────────────────

    #[test]
    fn test_attachment_file_emits_user_message_with_tag() {
        let entry = serde_json::json!({
            "type": "attachment",
            "uuid": "att-uuid-001",
            "parentUuid": null,
            "attachment": {
                "type": "file",
                "filename": "/home/user/project/src/main.rs",
                "displayPath": "src/main.rs",
                "content": {"type": "text"}
            }
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();

        let att_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id == "attachment:att-uuid-001" => {
                Some(message)
            }
            _ => None,
        });
        let msg = att_msg.expect("should emit attachment node");
        assert_eq!(msg.message_type, MessageType::UserMessage);
        assert_eq!(msg.tag.as_deref(), Some("attachment"));
        assert!(
            msg.text
                .as_deref()
                .unwrap_or("")
                .starts_with("[File: src/main.rs]"),
            "should use [File: displayPath] label"
        );
    }

    #[test]
    fn test_attachment_appends_at_root() {
        let entry = serde_json::json!({
            "type": "attachment",
            "uuid": "att-uuid-002",
            "parentUuid": null,
            "attachment": {"type": "task_reminder", "content": [], "itemCount": 0}
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();

        assert_eq!(append_ids(&ops), vec!["attachment:att-uuid-002"]);
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops));
    }

    #[test]
    fn test_attachment_duplicate_uuid_emits_replace() {
        let mut state = ParseState::default();
        state.seen_uuids.insert("att-dup".to_string());

        let entry = serde_json::json!({
            "type": "attachment",
            "uuid": "att-dup",
            "parentUuid": null,
            "attachment": {"type": "task_reminder", "itemCount": 0}
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();
        let has_replace = ops.iter().any(
            |op| matches!(op, TreeOperation::Replace { id, .. } if id == "attachment:att-dup"),
        );
        assert!(has_replace, "duplicate attachment uuid should emit Replace");
    }

    #[test]
    fn test_attachment_deferred_tools_label() {
        let entry = serde_json::json!({
            "type": "attachment",
            "uuid": "att-uuid-dt",
            "parentUuid": null,
            "attachment": {
                "type": "deferred_tools_delta",
                "addedNames": ["TaskCreate", "TaskUpdate", "WebSearch"],
                "removedNames": []
            }
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();
        let msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id == "attachment:att-uuid-dt" => {
                Some(message)
            }
            _ => None,
        });
        let text = msg.unwrap().text.as_deref().unwrap_or("");
        assert!(
            text.starts_with("[Tools: +"),
            "deferred_tools_delta label should start with '[Tools: +', got: {text}"
        );
    }

    #[test]
    fn test_attachment_unknown_type_uses_type_verbatim() {
        let entry = serde_json::json!({
            "type": "attachment",
            "uuid": "att-uuid-unk",
            "parentUuid": null,
            "attachment": {"type": "some_future_type"}
        })
        .to_string();

        let mut state = ParseState::default();
        let ops = parse_entry(&entry, &mut state).unwrap();
        let msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id == "attachment:att-uuid-unk" => {
                Some(message)
            }
            _ => None,
        });
        assert_eq!(
            msg.unwrap().text.as_deref(),
            Some("[some_future_type]"),
            "unknown type should be wrapped in brackets"
        );
    }

    #[test]
    fn test_attachment_with_subagent_prefix_at_root() {
        let entry = serde_json::json!({
            "type": "attachment",
            "uuid": "att-sa-001",
            "parentUuid": null,
            "attachment": {"type": "skill_listing", "skillCount": 5}
        })
        .to_string();

        let mut state = ParseState {
            id_prefix: "sa:xyz:".to_string(),
            ..Default::default()
        };
        let ops = parse_entry(&entry, &mut state).unwrap();

        assert_eq!(append_ids(&ops), vec!["sa:xyz:attachment:att-sa-001"]);
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops));
    }

    #[test]
    fn test_non_compaction_user_unaffected() {
        let mut state = ParseState::default();
        state
            .compact_boundary_uuids
            .insert("boundary-001".to_string());

        let entry = serde_json::json!({
            "type": "user",
            "uuid": "regular-001",
            "parentUuid": "some-other-parent",
            "message": {"role": "user", "content": "A normal user message"}
        })
        .to_string();

        let ops = parse_entry(&entry, &mut state).unwrap();

        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        let msg = user_msg.expect("should emit a user message node");
        assert_ne!(
            msg.tag.as_deref(),
            Some("summary"),
            "regular user message must not get summary tag"
        );
        assert!(
            msg.brief.is_none(),
            "regular user message must not get a brief"
        );
    }
}
