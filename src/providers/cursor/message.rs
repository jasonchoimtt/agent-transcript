use std::collections::{HashMap, HashSet};

use chrono::TimeZone as _;

use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{HiddenState, MessageState, MessageType};

pub struct ParseState {
    pub turn_n: usize,
    pub current_turn_has_agent_event: bool,
    /// Full 32-byte SHA256 IDs of blobs already processed.
    pub seen_blobs: HashSet<[u8; 32]>,
    /// IDs of container nodes (turn/user_turn/agent_turn) already appended.
    pub containers_emitted: HashSet<String>,
    /// toolCallId → the assistant tool-call block JSON (for generating call+result text).
    pub pending_tool_calls: HashMap<String, serde_json::Value>,
    /// ID of the current turn group node, for use as parent by agent_turn.
    pub current_turn_id: String,
    /// True after the first streaming:pending node has been emitted; switches Append → Replace.
    pub has_pending: bool,
}

impl Default for ParseState {
    fn default() -> Self {
        Self::new()
    }
}

impl ParseState {
    pub fn new() -> Self {
        Self {
            turn_n: 0,
            current_turn_has_agent_event: false,
            seen_blobs: HashSet::new(),
            containers_emitted: HashSet::new(),
            pending_tool_calls: HashMap::new(),
            current_turn_id: String::new(),
            has_pending: false,
        }
    }
}

/// Parse one message blob and return the resulting tree operations.
/// `blob_id` is the full lowercase hex SHA256 of the blob (64 chars).
pub fn parse_blob(blob_id: &str, data: &[u8], state: &mut ParseState) -> Vec<TreeOperation> {
    let mut ops = Vec::new();

    // Deduplicate by SHA256.
    if let Some(hash) = hex_to_bytes32(blob_id)
        && !state.seen_blobs.insert(hash)
    {
        return ops;
    }

    let obj: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(blob_id = %&blob_id[..blob_id.len().min(16)], "parse_blob JSON error: {}", e);
            return ops;
        }
    };

    let role = obj["role"].as_str().unwrap_or("");
    // first 32 hex chars = first 16 bytes of SHA256
    let blob_hex = &blob_id[..blob_id.len().min(32)];

    match role {
        "system" => {
            let text = extract_content_text(&obj["content"]);
            ops.push(TreeOperation::Append {
                parent_id: None,
                message: MessageState::new(format!("system:{}", blob_hex))
                    .text(text)
                    .data(obj.to_string())
                    .message_type(MessageType::System)
                    .brief("[System prompt]"),
            });
        }

        "user" => {
            let raw = extract_content_text(&obj["content"]);
            let (text, ts_str) = strip_message_tags(&raw);
            let timestamp = ts_str.as_deref().and_then(parse_timestamp);
            // No <user_query> and starts with <user_info> → pure Cursor context injection.
            let is_injected = text.trim().starts_with("<user_info>");
            let is_summary = obj["providerOptions"]["cursor"]["isSummary"]
                .as_bool()
                .unwrap_or(false);

            // Open a new turn if needed.
            if state.current_turn_has_agent_event {
                state.turn_n += 1;
                state.current_turn_has_agent_event = false;
            }

            let turn_group_id = format!("turn:{}", state.turn_n);
            let user_turn_id = format!("user_turn:{}", state.turn_n);

            // Emit the turn group container first (once per turn).
            if !state.containers_emitted.contains(&turn_group_id) {
                state.containers_emitted.insert(turn_group_id.clone());
                state.current_turn_id = turn_group_id.clone();
                let brief = if is_injected {
                    format!("Turn {}", state.turn_n)
                } else {
                    let first_line = text.lines().next().unwrap_or("").trim();
                    if first_line.is_empty() {
                        format!("Turn {}", state.turn_n)
                    } else {
                        first_line.to_string()
                    }
                };
                ops.push(TreeOperation::Append {
                    parent_id: None,
                    message: MessageState::new(turn_group_id.clone())
                        .brief(brief)
                        .group(true)
                        .message_type(MessageType::Container)
                        .tag("turn")
                        .indent_children(false),
                });
            }

            if !state.containers_emitted.contains(&user_turn_id) {
                state.containers_emitted.insert(user_turn_id.clone());
                ops.push(TreeOperation::Append {
                    parent_id: Some(turn_group_id),
                    message: MessageState::new(user_turn_id.clone())
                        .text("User")
                        .data(serde_json::json!({ "type": &user_turn_id }).to_string())
                        .message_type(MessageType::Container)
                        .tag("user-turn")
                        .indent_children(false),
                });
            }

            let mut user_msg = MessageState::new(format!("user_msg:{}", blob_hex))
                .text(text)
                .data(obj.to_string())
                .message_type(MessageType::UserMessage)
                .hidden(if is_injected {
                    HiddenState::Hidden
                } else {
                    HiddenState::NotHidden
                });
            if is_summary {
                user_msg = user_msg.tag("summary").brief("[Conversation summary]");
            }
            if let Some(ts) = timestamp {
                user_msg = user_msg.timestamp(ts);
            }
            ops.push(TreeOperation::Append {
                parent_id: Some(user_turn_id),
                message: user_msg,
            });
        }

        "assistant" => {
            let agent_turn_id = format!("agent_turn:{}", state.turn_n);

            if !state.containers_emitted.contains(&agent_turn_id) {
                state.containers_emitted.insert(agent_turn_id.clone());
                // Parent under the current turn group if one exists, else root.
                let parent = if state.current_turn_id.is_empty() {
                    None
                } else {
                    Some(state.current_turn_id.clone())
                };
                ops.push(TreeOperation::Append {
                    parent_id: parent,
                    message: MessageState::new(agent_turn_id.clone())
                        .text("Agent")
                        .data(serde_json::json!({ "type": &agent_turn_id }).to_string())
                        .message_type(MessageType::Container)
                        .tag("agent-turn")
                        .indent_children(false),
                });
            }

            let content_arr = obj["content"].as_array().cloned().unwrap_or_default();

            for (idx, block) in content_arr.iter().enumerate() {
                let block_type = block["type"].as_str().unwrap_or("");
                match block_type {
                    "reasoning" => {
                        let text = block["text"].as_str().unwrap_or("").to_string();
                        ops.push(TreeOperation::Append {
                            parent_id: Some(agent_turn_id.clone()),
                            message: MessageState::new(format!("thinking:{}:{}", blob_hex, idx))
                                .text(text)
                                .data(block.to_string())
                                .message_type(MessageType::Thinking),
                        });
                        state.current_turn_has_agent_event = true;
                    }
                    "redacted-reasoning" => {
                        ops.push(TreeOperation::Append {
                            parent_id: Some(agent_turn_id.clone()),
                            message: MessageState::new(format!(
                                "redacted_thinking:{}:{}",
                                blob_hex, idx
                            ))
                            .text("[redacted reasoning]")
                            .data(block.to_string())
                            .message_type(MessageType::Thinking)
                            .tag("redacted"),
                        });
                        state.current_turn_has_agent_event = true;
                    }
                    "text" => {
                        let text = block["text"].as_str().unwrap_or("").to_string();
                        ops.push(TreeOperation::Append {
                            parent_id: Some(agent_turn_id.clone()),
                            message: MessageState::new(format!("text:{}:{}", blob_hex, idx))
                                .text(text)
                                .data(block.to_string())
                                .message_type(MessageType::AgentMessage),
                        });
                        state.current_turn_has_agent_event = true;
                    }
                    "tool-call" => {
                        let tool_name = block["toolName"].as_str().unwrap_or("?");
                        let tool_call_id = block["toolCallId"].as_str().unwrap_or("");
                        let node_id = format!("tool_call:{}", tool_call_id);
                        let props = extract_args_props(&block["args"]);

                        let mut msg = MessageState::new(node_id)
                            .text(tool_name)
                            .data(block.to_string())
                            .message_type(MessageType::ToolCall);
                        if let Some(p) = props {
                            msg = msg.props(p);
                        }
                        ops.push(TreeOperation::Append {
                            parent_id: Some(agent_turn_id.clone()),
                            message: msg,
                        });
                        state
                            .pending_tool_calls
                            .insert(tool_call_id.to_string(), block.clone());
                        state.current_turn_has_agent_event = true;
                    }
                    _ => {}
                }
            }
        }

        "tool" => {
            let content_arr = obj["content"].as_array().cloned().unwrap_or_default();

            for block in &content_arr {
                if block["type"].as_str() != Some("tool-result") {
                    continue;
                }
                let tool_name = block["toolName"].as_str().unwrap_or("?");
                let tool_call_id = block["toolCallId"].as_str().unwrap_or("");

                let Some(call_block) = state.pending_tool_calls.remove(tool_call_id) else {
                    continue; // orphan result
                };

                let old_node_id = format!("tool_call:{}", tool_call_id);

                if tool_name == "Task" {
                    let result_text = extract_task_result_text(block);

                    // Check for conversationSteps in providerOptions.
                    let steps = block["providerOptions"]["cursor"]["highLevelToolCallResult"]
                        ["output"]["success"]["conversationSteps"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();

                    let failure = &block["providerOptions"]["cursor"]["highLevelToolCallResult"]["output"]
                        ["failure"];

                    if !failure.is_null() {
                        let failure_text = failure.as_str().unwrap_or("[task failed]").to_string();
                        let call_name = call_block["toolName"].as_str().unwrap_or(tool_name);
                        let call_props = extract_args_props(&call_block["args"]);
                        let mut replace_node = MessageState::new(old_node_id.clone())
                            .text(call_name)
                            .data(call_block.to_string())
                            .message_type(MessageType::ToolCall)
                            .tag("error");
                        if let Some(p) = call_props {
                            replace_node = replace_node.props(p);
                        }
                        ops.push(TreeOperation::Replace {
                            id: old_node_id.clone(),
                            message: replace_node,
                        });
                        ops.push(TreeOperation::Append {
                            parent_id: Some(old_node_id),
                            message: MessageState::new(format!("tool_result:{}", tool_call_id))
                                .text(failure_text)
                                .data(
                                    serde_json::json!({ "call": call_block, "result": block })
                                        .to_string(),
                                )
                                .message_type(MessageType::ToolResult)
                                .tag("error"),
                        });
                    } else {
                        let task_id = format!("task:{}", tool_call_id);
                        let mut task_children = vec![
                            MessageState::new(format!("task_summary:{}", tool_call_id))
                                .text(result_text.clone())
                                .data(block.to_string())
                                .message_type(MessageType::TaskSummary),
                        ];

                        for (i, step) in steps.iter().enumerate() {
                            if let Some(child) = build_subagent_node(tool_call_id, i, step) {
                                task_children.push(child);
                            }
                        }

                        ops.push(TreeOperation::Replace {
                            id: old_node_id,
                            message: MessageState::new(task_id)
                                .text(result_text)
                                .data(
                                    serde_json::json!({
                                        "call": call_block,
                                        "result": block
                                    })
                                    .to_string(),
                                )
                                .message_type(MessageType::Container)
                                .tag("task")
                                .indent_children(true)
                                .children(task_children),
                        });
                    }
                } else {
                    // Regular tool result: tag the ToolCall and append ToolResult as child.
                    let result_text = block["result"].as_str().unwrap_or("").to_string();
                    let call_name = call_block["toolName"].as_str().unwrap_or(tool_name);
                    let is_error = block["isError"].as_bool().unwrap_or(false);
                    let status_tag = if is_error { "error" } else { "success" };
                    let call_props = extract_args_props(&call_block["args"]);

                    let mut replace_node = MessageState::new(old_node_id.clone())
                        .text(call_name)
                        .data(call_block.to_string())
                        .message_type(MessageType::ToolCall)
                        .tag(status_tag);
                    if let Some(p) = call_props {
                        replace_node = replace_node.props(p);
                    }
                    ops.push(TreeOperation::Replace {
                        id: old_node_id.clone(),
                        message: replace_node,
                    });

                    let result_id = format!("tool_result:{}", tool_call_id);
                    ops.push(TreeOperation::Append {
                        parent_id: Some(old_node_id),
                        message: MessageState::new(result_id)
                            .text(result_text)
                            .data(
                                serde_json::json!({ "call": call_block, "result": block })
                                    .to_string(),
                            )
                            .message_type(MessageType::ToolResult)
                            .tag(status_tag),
                    });
                }
            }
        }

        _ => {}
    }

    ops
}

/// Parse a partial field-4 assistant JSON blob and emit `streaming:pending`.
/// Emits nothing (or Remove if the node already exists) when the content is empty.
/// On the first non-empty call emits Append; subsequent non-empty calls emit Replace.
pub fn parse_pending_blob(data: &[u8], state: &mut ParseState) -> Vec<TreeOperation> {
    let obj: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(
                "parse_pending_blob: JSON parse error (partial blob expected): {}",
                e
            );
            return vec![];
        }
    };
    if obj["role"].as_str() != Some("assistant") {
        return vec![];
    }

    // Ensure the agent_turn container exists before appending under it.
    let agent_turn_id = format!("agent_turn:{}", state.turn_n);
    let mut ops = Vec::new();
    if !state.containers_emitted.contains(&agent_turn_id) {
        state.containers_emitted.insert(agent_turn_id.clone());
        let parent = if state.current_turn_id.is_empty() {
            None
        } else {
            Some(state.current_turn_id.clone())
        };
        ops.push(TreeOperation::Append {
            parent_id: parent,
            message: MessageState::new(agent_turn_id.clone())
                .text("Agent")
                .data(serde_json::json!({ "type": &agent_turn_id }).to_string())
                .message_type(MessageType::Container)
                .tag("agent-turn")
                .indent_children(false),
        });
    }

    // Extract text from type:"text" blocks only. redacted-reasoning and tool-call
    // blocks in field 4 have no displayable text and are silently skipped.
    let text = extract_content_text(&obj["content"]);
    let text = text.trim().to_string();

    if text.is_empty() {
        // No displayable content yet: remove an existing pending node or stay silent.
        if state.has_pending {
            state.has_pending = false;
            ops.push(TreeOperation::Remove {
                id: "streaming:pending".to_string(),
            });
        }
        return ops;
    }

    let pending_node = MessageState::new("streaming:pending")
        .text(text)
        .data(obj.to_string())
        .message_type(MessageType::AgentMessage);

    if state.has_pending {
        ops.push(TreeOperation::Replace {
            id: "streaming:pending".to_string(),
            message: pending_node,
        });
    } else {
        state.has_pending = true;
        ops.push(TreeOperation::Append {
            parent_id: Some(agent_turn_id),
            message: pending_node,
        });
    }
    ops
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

/// Strips `<timestamp>…</timestamp>` and `<user_query>…</user_query>` wrappers
/// from user message text.  Returns `(display_text, timestamp_string)`.
fn strip_message_tags(text: &str) -> (String, Option<String>) {
    let (ts_str, rest) = if let Some(start) = text.find("<timestamp>") {
        if let Some(end_rel) = text[start..].find("</timestamp>") {
            let inner = text[start + "<timestamp>".len()..start + end_rel].to_string();
            let before = &text[..start];
            let after = &text[start + end_rel + "</timestamp>".len()..];
            (Some(inner), format!("{}{}", before, after))
        } else {
            (None, text.to_string())
        }
    } else {
        (None, text.to_string())
    };

    let display = if let Some(start) = rest.find("<user_query>") {
        if let Some(end_rel) = rest[start..].find("</user_query>") {
            rest[start + "<user_query>".len()..start + end_rel]
                .trim()
                .to_string()
        } else {
            rest.trim().to_string()
        }
    } else {
        rest.trim().to_string()
    };

    (display, ts_str)
}

/// Parses a timestamp string like `"Saturday, May 2, 2026, 3:00 PM (UTC+8)"`.
fn parse_timestamp(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    // Extract "(UTC±N)" or "(UTC±H:MM)" suffix.
    let tz_start = s.rfind("(UTC")?;
    let tz_inner = s[tz_start..]
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim_start_matches("UTC");

    let (sign, digits) = if let Some(rest) = tz_inner.strip_prefix('+') {
        (1i32, rest)
    } else if let Some(rest) = tz_inner.strip_prefix('-') {
        (-1i32, rest)
    } else {
        return None;
    };

    let (hours, minutes) = if let Some(colon) = digits.find(':') {
        let h: i32 = digits[..colon].parse().ok()?;
        let m: i32 = digits[colon + 1..].parse().ok()?;
        (h, m)
    } else {
        let h: i32 = digits.parse().ok()?;
        (h, 0)
    };

    let offset_secs = sign * (hours * 3600 + minutes * 60);
    let tz = chrono::FixedOffset::east_opt(offset_secs)?;

    // Strip everything from "(UTC" onward, then strip the leading "DayOfWeek, ".
    let datetime_str = s[..tz_start].trim();
    let after_dow = datetime_str
        .find(", ")
        .map_or(datetime_str, |i| &datetime_str[i + 2..]);

    let naive = chrono::NaiveDateTime::parse_from_str(after_dow, "%B %-d, %Y, %I:%M %p").ok()?;
    tz.from_local_datetime(&naive).single()
}

fn extract_task_result_text(block: &serde_json::Value) -> String {
    // Prefer experimental_content[0].text, fall back to result string.
    if let Some(arr) = block["experimental_content"].as_array()
        && let Some(text) = arr.first().and_then(|b| b["text"].as_str())
    {
        return text.to_string();
    }
    block["result"].as_str().unwrap_or("").to_string()
}

/// Normalize a cursor tool-call `args` field into a JSON object for use as `props`.
/// Handles `args` being an object, a JSON string, or a plain string.
fn extract_args_props(args: &serde_json::Value) -> Option<serde_json::Value> {
    match args {
        serde_json::Value::Object(_) => Some(args.clone()),
        serde_json::Value::String(s) => {
            let parsed = serde_json::from_str::<serde_json::Value>(s).ok();
            if let Some(obj) = parsed.filter(|v| v.is_object()) {
                Some(obj)
            } else {
                Some(serde_json::json!({ "_": s }))
            }
        }
        serde_json::Value::Null => None,
        other => Some(serde_json::json!({ "_": other.to_string() })),
    }
}

fn build_subagent_node(
    tool_call_id: &str,
    i: usize,
    step: &serde_json::Value,
) -> Option<MessageState> {
    if !step["thinkingMessage"].is_null() {
        let text = step["thinkingMessage"]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        return Some(
            MessageState::new(format!("subagent_think:{}:{}", tool_call_id, i))
                .text(text)
                .data(step.to_string())
                .message_type(MessageType::Thinking),
        );
    }
    if !step["assistantMessage"].is_null() {
        let text = step["assistantMessage"]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        return Some(
            MessageState::new(format!("subagent_text:{}:{}", tool_call_id, i))
                .text(text)
                .data(step.to_string())
                .message_type(MessageType::AgentMessage),
        );
    }
    if !step["toolCall"].is_null() {
        let tc = &step["toolCall"];
        let name = tc["toolName"]
            .as_str()
            .or_else(|| tc["name"].as_str())
            .unwrap_or("?");
        let args = tc["args"].as_str().unwrap_or("");
        let result = tc["result"].as_str().unwrap_or("");
        let text = format!("{}: {}\n→ {}", name, args, result);
        return Some(
            MessageState::new(format!("subagent_tool:{}:{}", tool_call_id, i))
                .text(text)
                .data(step.to_string())
                .message_type(MessageType::ToolCall),
        );
    }
    None
}

fn hex_to_bytes32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_blob_id() -> String {
        "a".repeat(64)
    }

    #[test]
    fn test_parse_system_blob_emits_system_node() {
        let blob = serde_json::json!({ "role": "system", "content": "You are an AI." });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TreeOperation::Append { parent_id, message } => {
                assert!(parent_id.is_none());
                assert_eq!(message.message_type, MessageType::System);
                assert_eq!(message.brief.as_deref(), Some("[System prompt]"));
                assert!(!message.expanded || message.text.as_deref() == Some("You are an AI."));
            }
            _ => panic!("expected Append"),
        }
    }

    #[test]
    fn test_injected_user_message_is_hidden() {
        let content = "<user_info>\nOS: linux\n</user_info>\n<rules>some rules</rules>";
        let blob = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": content}]
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        assert!(
            user_msg.unwrap().hidden.is_hidden(),
            "injected user message should be hidden"
        );
    }

    #[test]
    fn test_real_user_message_is_visible() {
        let content = "<timestamp>Saturday, May 2, 2026, 3:00 PM (UTC+8)</timestamp>\n<user_query>Hello world</user_query>";
        let blob = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": content}]
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        assert!(
            !user_msg.unwrap().hidden.is_hidden(),
            "real user message should not be hidden"
        );
    }

    #[test]
    fn test_strip_message_tags() {
        let input = "<timestamp>Saturday, May 2, 2026, 3:00 PM (UTC+8)</timestamp>\n<user_query>Hello world</user_query>";
        let (display, ts) = strip_message_tags(input);
        assert_eq!(display, "Hello world");
        assert_eq!(
            ts.as_deref(),
            Some("Saturday, May 2, 2026, 3:00 PM (UTC+8)")
        );
    }

    #[test]
    fn test_parse_timestamp() {
        let ts = parse_timestamp("Saturday, May 2, 2026, 3:00 PM (UTC+8)").unwrap();
        assert_eq!(ts.offset().local_minus_utc(), 8 * 3600);
    }

    #[test]
    fn test_parse_user_blob_opens_turn() {
        let blob = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "Hello world"}]
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        // Should emit: turn:0 group (top-level), user_turn:0 child, user_msg leaf.
        let ids: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                TreeOperation::Replace { message, .. } => Some(message.id.clone()),
                TreeOperation::Update { message, .. } => Some(message.id.clone()),
                TreeOperation::Remove { .. } => None,
            })
            .collect();
        assert!(
            ids.contains(&"turn:0".to_string()),
            "turn:n group wrapper should be present"
        );
        assert!(ids.contains(&"user_turn:0".to_string()));
        assert!(ids.iter().any(|id| id.starts_with("user_msg:")));
        // turn:0 should be a group node
        let turn_op = ops.iter().find(|op| match op {
            TreeOperation::Append { message, .. } => message.id == "turn:0",
            _ => false,
        });
        if let Some(TreeOperation::Append { message, parent_id }) = turn_op {
            assert!(message.group, "turn:0 should be a group node");
            assert!(parent_id.is_none(), "turn:0 should be a root node");
        }
    }

    #[test]
    fn test_parse_assistant_blob_emits_text_node() {
        let id = "b".repeat(64);
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "Hi there"}]
        });
        let mut state = ParseState::new();
        // Seed agent_turn container so we only get the text node in ops.
        state.containers_emitted.insert("agent_turn:0".to_string());
        let ops = parse_blob(&id, blob.to_string().as_bytes(), &mut state);
        let ids: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                TreeOperation::Replace { message, .. } => Some(message.id.clone()),
                TreeOperation::Update { message, .. } => Some(message.id.clone()),
                TreeOperation::Remove { .. } => None,
            })
            .collect();
        assert!(ids.iter().any(|id| id.starts_with("text:")));
        assert!(state.current_turn_has_agent_event);
    }

    #[test]
    fn test_tool_result_tags_call_and_appends_result() {
        let tc_id = "toolu_abc";
        let blob_id = "c".repeat(64);

        // Simulate having seen the tool-call leaf.
        let mut state = ParseState::new();
        state.pending_tool_calls.insert(
            tc_id.to_string(),
            serde_json::json!({"type": "tool-call", "toolName": "Read", "toolCallId": tc_id, "args": {}}),
        );

        let tool_blob = serde_json::json!({
            "role": "tool",
            "content": [{
                "type": "tool-result",
                "toolName": "Read",
                "toolCallId": tc_id,
                "result": "file content"
            }]
        });
        let ops = parse_blob(&blob_id, tool_blob.to_string().as_bytes(), &mut state);
        assert!(ops.len() >= 2);

        // First op: Replace the ToolCall with call-only text and success tag.
        match &ops[0] {
            TreeOperation::Replace { id, message } => {
                assert_eq!(id, &format!("tool_call:{}", tc_id));
                assert_eq!(message.tag.as_deref(), Some("success"));
                assert!(
                    !message
                        .text
                        .as_deref()
                        .unwrap_or("")
                        .contains("file content"),
                    "ToolCall text should not include result"
                );
            }
            _ => panic!("expected Replace as first op"),
        }

        // Second op: Append ToolResult as child of ToolCall.
        match &ops[1] {
            TreeOperation::Append { parent_id, message } => {
                assert_eq!(
                    parent_id.as_deref(),
                    Some(format!("tool_call:{}", tc_id).as_str())
                );
                assert_eq!(message.message_type, MessageType::ToolResult);
                assert_eq!(message.tag.as_deref(), Some("success"));
                assert!(
                    message
                        .text
                        .as_deref()
                        .unwrap_or("")
                        .contains("file content")
                );
            }
            _ => panic!("expected Append as second op"),
        }
    }

    #[test]
    fn test_new_turn_on_agent_event_then_user() {
        let mut state = ParseState::new();

        // First user message: opens user_turn:0
        let u1 = serde_json::json!({"role": "user", "content": "hello"});
        let u1_id = "a".repeat(64);
        parse_blob(&u1_id, u1.to_string().as_bytes(), &mut state);
        assert_eq!(state.turn_n, 0);

        // Agent responds: sets current_turn_has_agent_event
        state.current_turn_has_agent_event = true;

        // Second user message: should open user_turn:1
        let u2 = serde_json::json!({"role": "user", "content": "follow-up"});
        let u2_id = "b".repeat(64);
        let ops = parse_blob(&u2_id, u2.to_string().as_bytes(), &mut state);
        assert_eq!(state.turn_n, 1);
        let ids: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => Some(message.id.clone()),
                TreeOperation::Replace { message, .. } => Some(message.id.clone()),
                TreeOperation::Update { message, .. } => Some(message.id.clone()),
                TreeOperation::Remove { .. } => None,
            })
            .collect();
        assert!(
            ids.contains(&"turn:1".to_string()),
            "turn:n group wrapper should be present"
        );
        assert!(ids.contains(&"user_turn:1".to_string()));
    }

    #[test]
    fn test_parse_pending_blob_first_call_emits_append() {
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "partial response"}]
        });
        let mut state = ParseState::new();
        let ops = parse_pending_blob(blob.to_string().as_bytes(), &mut state);
        assert!(state.has_pending);
        let pending_op = ops.iter().find(|op| match op {
            TreeOperation::Append { message, .. } => message.id == "streaming:pending",
            _ => false,
        });
        assert!(
            pending_op.is_some(),
            "first call should emit Append for streaming:pending"
        );
        match pending_op.unwrap() {
            TreeOperation::Append { message, .. } => {
                assert_eq!(message.text.as_deref(), Some("partial response"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_parse_pending_blob_second_call_emits_replace() {
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "updated response"}]
        });
        let mut state = ParseState::new();
        state.has_pending = true;
        state.containers_emitted.insert("agent_turn:0".to_string());
        let ops = parse_pending_blob(blob.to_string().as_bytes(), &mut state);
        let replace_op = ops.iter().find(|op| {
            matches!(
                op, TreeOperation::Replace { id, .. } if id == "streaming:pending"
            )
        });
        assert!(
            replace_op.is_some(),
            "second call should emit Replace for streaming:pending"
        );
    }

    #[test]
    fn test_parse_pending_blob_non_text_blocks_skipped() {
        // redacted-reasoning and tool-call have no displayable text in field 4.
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "redacted-reasoning", "data": "opaque"},
                {"type": "text", "text": "visible text"}
            ]
        });
        let mut state = ParseState::new();
        state.containers_emitted.insert("agent_turn:0".to_string());
        let ops = parse_pending_blob(blob.to_string().as_bytes(), &mut state);
        let append_op = ops.iter().find(|op| match op {
            TreeOperation::Append { message, .. } => message.id == "streaming:pending",
            _ => false,
        });
        assert!(append_op.is_some());
        match append_op.unwrap() {
            TreeOperation::Append { message, .. } => {
                assert_eq!(message.text.as_deref(), Some("visible text"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_parse_pending_blob_empty_content_no_append() {
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "redacted-reasoning", "data": "opaque"}]
        });
        let mut state = ParseState::new();
        state.containers_emitted.insert("agent_turn:0".to_string());
        let ops = parse_pending_blob(blob.to_string().as_bytes(), &mut state);
        assert!(
            !state.has_pending,
            "has_pending should stay false when content is empty"
        );
        assert!(
            !ops.iter().any(|op| matches!(op, TreeOperation::Append { message, .. } if message.id == "streaming:pending")),
            "no Append for streaming:pending when content is empty"
        );
    }

    #[test]
    fn test_parse_pending_blob_empty_content_removes_existing() {
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "redacted-reasoning", "data": "opaque"}]
        });
        let mut state = ParseState::new();
        state.has_pending = true;
        state.containers_emitted.insert("agent_turn:0".to_string());
        let ops = parse_pending_blob(blob.to_string().as_bytes(), &mut state);
        assert!(
            !state.has_pending,
            "has_pending cleared when content goes empty"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, TreeOperation::Remove { id } if id == "streaming:pending")),
            "Remove emitted for streaming:pending when content becomes empty"
        );
    }

    #[test]
    fn test_summary_blob_gets_tag_and_brief() {
        let blob = serde_json::json!({
            "role": "user",
            "providerOptions": { "cursor": { "isSummary": true } },
            "content": "Your conversation was summarized due to context constraints."
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        let msg = user_msg.unwrap();
        assert_eq!(msg.tag.as_deref(), Some("summary"));
        assert_eq!(msg.brief.as_deref(), Some("[Conversation summary]"));
        assert!(
            !msg.hidden.is_hidden(),
            "summary message should not be hidden"
        );
    }

    #[test]
    fn test_non_summary_user_blob_unaffected() {
        let blob = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "Hello world"}]
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        let user_msg = ops.iter().find_map(|op| match op {
            TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                Some(message)
            }
            _ => None,
        });
        let msg = user_msg.unwrap();
        assert!(
            msg.tag.as_deref() != Some("summary"),
            "regular user message should not have summary tag"
        );
        assert!(
            msg.brief.is_none(),
            "regular user message should have no brief override"
        );
    }
}
