use std::collections::{HashMap, HashSet};

use chrono::TimeZone as _;

use crate::providers::extract_xml_tag;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{HiddenState, MessageState, MessageType};

pub struct ParseState {
    /// Full 32-byte SHA256 IDs of blobs already processed.
    pub seen_blobs: HashSet<[u8; 32]>,
    /// toolCallId → the assistant tool-call block JSON (for generating call+result text).
    pub pending_tool_calls: HashMap<String, serde_json::Value>,
    /// True after the first streaming:pending node has been emitted; switches Append → Replace.
    pub has_pending: bool,
    /// Background task ID (`shellId` / `agentId`) → the tool call that started it, so a
    /// later `<system_notification>` can attach the outcome to that call.
    pub background_tasks: HashMap<String, BackgroundTask>,
}

pub struct BackgroundTask {
    pub tool_call_id: String,
    /// True for a background `Task` (subagent) call; false for a background shell.
    pub is_subagent: bool,
}

impl Default for ParseState {
    fn default() -> Self {
        Self::new()
    }
}

impl ParseState {
    pub fn new() -> Self {
        Self {
            seen_blobs: HashSet::new(),
            pending_tool_calls: HashMap::new(),
            has_pending: false,
            background_tasks: HashMap::new(),
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
            let (untimed, _) = strip_timestamp(&raw);
            let notification = TaskNotification::parse(&untimed);
            let (text, ts_str) = strip_message_tags(&raw);
            let text = notification.as_ref().map_or(text, TaskNotification::summary);
            let timestamp = ts_str.as_deref().and_then(parse_timestamp);
            // No <user_query> and starts with <user_info> → pure Cursor context injection.
            let is_injected = text.trim().starts_with("<user_info>");
            let is_summary = obj["providerOptions"]["cursor"]["isSummary"]
                .as_bool()
                .unwrap_or(false);

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
            } else if notification.is_some() {
                user_msg = user_msg.tag("system_notification");
            }
            if let Some(ts) = timestamp {
                user_msg = user_msg.timestamp(ts);
            }
            ops.push(TreeOperation::Append {
                parent_id: None,
                message: user_msg,
            });
            if let Some(n) = &notification {
                ops.extend(n.link_ops(state, &obj));
            }
        }

        "assistant" => {
            let content_arr = obj["content"].as_array().cloned().unwrap_or_default();

            for (idx, block) in content_arr.iter().enumerate() {
                let block_type = block["type"].as_str().unwrap_or("");
                match block_type {
                    "reasoning" => {
                        let text = block["text"].as_str().unwrap_or("").to_string();
                        ops.push(TreeOperation::Append {
                            parent_id: None,
                            message: MessageState::new(format!("thinking:{}:{}", blob_hex, idx))
                                .text(text)
                                .data(block.to_string())
                                .message_type(MessageType::Thinking),
                        });
                    }
                    "redacted-reasoning" => {
                        ops.push(TreeOperation::Append {
                            parent_id: None,
                            message: MessageState::new(format!(
                                "redacted_thinking:{}:{}",
                                blob_hex, idx
                            ))
                            .text("[redacted reasoning]")
                            .data(block.to_string())
                            .message_type(MessageType::Thinking)
                            .tag("redacted"),
                        });
                    }
                    "text" => {
                        let text = block["text"].as_str().unwrap_or("").to_string();
                        ops.push(TreeOperation::Append {
                            parent_id: None,
                            message: MessageState::new(format!("text:{}:{}", blob_hex, idx))
                                .text(text)
                                .data(block.to_string())
                                .message_type(MessageType::AgentMessage),
                        });
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
                            parent_id: None,
                            message: msg,
                        });
                        state
                            .pending_tool_calls
                            .insert(tool_call_id.to_string(), block.clone());
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

                // Structured result details live on the tool *message*, not the block.
                // Shell reports `isBackground` beside `success`, Task inside it.  Shells
                // backgrounded after a timeout carry only `backgroundReason`.
                let high_level = &obj["providerOptions"]["cursor"]["highLevelToolCallResult"];
                let output = &high_level["output"];
                let success = &output["success"];
                let is_background = output["isBackground"].as_bool().unwrap_or(false)
                    || success["isBackground"].as_bool().unwrap_or(false)
                    || success["backgroundReason"].is_string();
                let background_id = match (&success["shellId"], &success["agentId"]) {
                    (serde_json::Value::Number(n), _) => Some(n.to_string()),
                    (_, serde_json::Value::String(s)) => Some(s.clone()),
                    _ => None,
                };
                if is_background && let Some(id) = background_id {
                    state.background_tasks.insert(
                        id,
                        BackgroundTask {
                            tool_call_id: tool_call_id.to_string(),
                            is_subagent: tool_name == "Task",
                        },
                    );
                }

                if tool_name == "Task" {
                    // The Task call stays a ToolCall; its outcome (and, on success, the
                    // subagent's conversation steps) become its children.
                    let error = &output["error"];
                    let is_error =
                        !error.is_null() || high_level["isError"].as_bool().unwrap_or(false);

                    let children = if is_error {
                        let failure_text = error["error"]
                            .as_str()
                            .or_else(|| block["result"].as_str())
                            .unwrap_or("[task failed]")
                            .to_string();
                        vec![
                            MessageState::new(format!("tool_result:{}", tool_call_id))
                                .text(failure_text)
                                .data(
                                    serde_json::json!({ "call": call_block, "result": block, "message": &obj })
                                        .to_string(),
                                )
                                .message_type(MessageType::ToolResult)
                                .tag("error"),
                        ]
                    } else {
                        // Full step list only in legacy sessions; see `build_subagent_node`.
                        let steps = success["conversationSteps"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        let summary_text = extract_task_result_text(block);
                        // The subagent's final message is already quoted in the summary
                        // (in new sessions it's the only step), so don't repeat it.
                        let steps = steps.iter().enumerate().filter(|(_, step)| {
                            step["assistantMessage"]["text"]
                                .as_str()
                                .is_none_or(|t| !summary_text.contains(t.trim()))
                        });
                        let summary = MessageState::new(format!("task_summary:{}", tool_call_id))
                            .text(summary_text.clone())
                            .data(block.to_string())
                            .message_type(MessageType::TaskSummary);
                        std::iter::once(summary)
                            .chain(steps.filter_map(|(i, step)| {
                                build_subagent_node(tool_call_id, i, step)
                            }))
                            .collect()
                    };

                    let call_name = call_block["toolName"].as_str().unwrap_or(tool_name);
                    let mut task_node = MessageState::new(old_node_id.clone())
                        .text(call_name)
                        .data(call_block.to_string())
                        .message_type(MessageType::ToolCall)
                        .tag(if is_error { "error" } else { "success" })
                        .indent_children(true)
                        .children(children);
                    if let Some(p) = extract_args_props(&call_block["args"]) {
                        task_node = task_node.props(p);
                    }
                    ops.push(TreeOperation::Replace {
                        id: old_node_id,
                        message: task_node,
                    });
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
                                serde_json::json!({ "call": call_block, "result": block, "message": &obj })
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

    let mut ops = Vec::new();

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
            parent_id: None,
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

/// Removes the `<timestamp>…</timestamp>` block from user message text.
/// Returns `(remaining_text, timestamp_string)`.
fn strip_timestamp(text: &str) -> (String, Option<String>) {
    if let Some(start) = text.find("<timestamp>")
        && let Some(end_rel) = text[start..].find("</timestamp>")
    {
        let inner = text[start + "<timestamp>".len()..start + end_rel].to_string();
        let before = &text[..start];
        let after = &text[start + end_rel + "</timestamp>".len()..];
        return (format!("{}{}", before, after), Some(inner));
    }
    (text.to_string(), None)
}

/// A background-task completion notice from a Cursor `<system_notification>` user
/// message.  The `<task>` block holds `key: value` header lines, optionally followed
/// by free text (a subagent's full report):
///
/// ```text
/// <task>
/// kind: shell
/// status: error
/// task_id: 825186
/// title: Start dev server
/// detail: exit_code=143
/// </task>
/// ```
struct TaskNotification<'a> {
    /// Matches the `shellId` / `agentId` of the background tool call that started the task.
    task_id: Option<&'a str>,
    title: &'a str,
    /// Human-readable status, e.g. `"Completed"` or `"Failed"`.
    label: String,
    is_error: bool,
    detail: Option<&'a str>,
    /// Text after the header lines; empty for shell tasks.
    body: &'a str,
}

impl<'a> TaskNotification<'a> {
    /// Parses `text` (timestamp already stripped).  Returns `None` when `text` is not
    /// a notification or lacks a `<task>` block with a `title`.
    fn parse(text: &'a str) -> Option<Self> {
        const HEADER_KEYS: [&str; 5] = ["kind", "status", "task_id", "title", "detail"];

        let text = text.trim_start();
        if !text.starts_with("<system_notification>") {
            return None;
        }
        let task = extract_xml_tag(text, "task")?.trim_start_matches('\n');

        // Header lines run until the first line that isn't a known `key: value`; the
        // remainder is the body.  Known keys only, so a report line like `Note: …`
        // stays in the body.
        let mut fields = HashMap::new();
        let mut body_start = task.len();
        let mut offset = 0;
        for line in task.split_inclusive('\n') {
            match line.trim_end().split_once(": ") {
                Some((k, v)) if HEADER_KEYS.contains(&k) => {
                    fields.insert(k, v.trim());
                }
                _ => {
                    body_start = offset;
                    break;
                }
            }
            offset += line.len();
        }

        let title = fields.get("title").copied().filter(|t| !t.is_empty())?;
        let status = fields.get("status").copied().unwrap_or("");
        let label = match status {
            "success" | "completed" => "Completed".to_string(),
            "error" | "failure" | "failed" => "Failed".to_string(),
            "cancelled" | "canceled" | "aborted" => "Cancelled".to_string(),
            "" => "Finished".to_string(),
            other => {
                let mut chars = other.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }
        };
        Some(Self {
            task_id: fields.get("task_id").copied(),
            title,
            is_error: label == "Failed",
            label,
            detail: fields.get("detail").copied().filter(|d| !d.is_empty()),
            body: task[body_start..].trim(),
        })
    }

    /// One-line summary for the user-message node, e.g. `"Completed: Start dev server"`.
    fn summary(&self) -> String {
        format!("{}: {}", self.label, self.title)
    }

    /// Ops that attach the task's outcome to the tool call that started it.
    /// Returns nothing when the originating call wasn't seen in this transcript.
    fn link_ops(&self, state: &ParseState, data: &serde_json::Value) -> Vec<TreeOperation> {
        let Some(task) = self.task_id.and_then(|id| state.background_tasks.get(id)) else {
            return vec![];
        };
        let tool_call_id = &task.tool_call_id;

        if task.is_subagent {
            // A background Task call's summary child still says "running in the
            // background"; fill it with the report.
            let summary_id = format!("task_summary:{tool_call_id}");
            let text = if self.body.is_empty() {
                self.summary()
            } else {
                self.body.to_string()
            };
            vec![TreeOperation::Replace {
                id: summary_id.clone(),
                message: MessageState::new(summary_id)
                    .text(text)
                    .data(data.to_string())
                    .message_type(MessageType::TaskSummary),
            }]
        } else {
            // Shell: the call's own result only says "started"; add the final outcome.
            let text = match self.detail {
                Some(detail) => format!("{} ({detail})", self.label),
                None => self.label.clone(),
            };
            vec![TreeOperation::Append {
                parent_id: Some(format!("tool_call:{tool_call_id}")),
                message: MessageState::new(format!("background_result:{tool_call_id}"))
                    .text(text)
                    .data(data.to_string())
                    .message_type(MessageType::ToolResult)
                    .tag(if self.is_error { "error" } else { "success" }),
            }]
        }
    }
}

/// Strips `<timestamp>…</timestamp>` and `<user_query>…</user_query>` wrappers
/// from user message text.  Returns `(display_text, timestamp_string)`.
fn strip_message_tags(text: &str) -> (String, Option<String>) {
    let (rest, ts_str) = strip_timestamp(text);

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

/// Converts one inlined `conversationSteps` entry of a Task result into a child node.
///
/// **Legacy format.**  Older Cursor versions (sessions up to ~June 2026) inlined the
/// subagent's whole conversation in the Task result.  Newer versions write each subagent
/// to its own chat DB (`~/.cursor/chats/<workspace>/<agentId>/store.db`, whose meta
/// carries `subagentInfo.{parentAgentId, toolCallId}`) and inline only the final
/// assistant message here.  Reading those separate subagent DBs is not implemented yet
/// (unlike the Claude provider's `subagent.rs`), so for new sessions a Task shows just
/// its summary.
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
    // Tool steps are keyed by kind: `{"toolCall": {"shellToolCall": {"args": {…}, "result": {…}}}}`.
    if let Some((kind, call)) = step["toolCall"]
        .as_object()
        .and_then(|tc| tc.iter().find(|(k, _)| k.ends_with("ToolCall")))
    {
        // `readToolCall` → `Read`, `webFetchToolCall` → `WebFetch`.
        let kind = kind.trim_end_matches("ToolCall");
        let mut chars = kind.chars();
        let name: String = chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or_default();

        let result = &call["result"];
        let tag = if !result["success"].is_null() {
            Some("success")
        } else if ["failure", "error", "permissionDenied"]
            .iter()
            .any(|k| !result[*k].is_null())
        {
            Some("error")
        } else {
            None
        };

        let mut node = MessageState::new(format!("subagent_tool:{}:{}", tool_call_id, i))
            .text(name)
            .data(step.to_string())
            .message_type(MessageType::ToolCall);
        // ToolFormatter renders the name + props into the call's display line.
        if let Some(serde_json::Value::Object(mut args)) = extract_args_props(&call["args"]) {
            args.remove("toolCallId");
            node = node.props(serde_json::Value::Object(args));
        }
        if let Some(tag) = tag {
            node = node.tag(tag);
        }
        return Some(node);
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

    fn notification_content(status: &str) -> String {
        format!(
            "<timestamp>Friday, Sep 18, 2026, 3:43 PM (UTC+8)</timestamp>\n\
             <system_notification>\n\
             The following task has finished. If you were already aware, ignore this notification and do not restate prior responses.\n\n\
             <task>\nkind: shell\nstatus: {status}\ntask_id: 795680\ntitle: Restart worktree DAI dev server\n</task>\n\
             </system_notification>\n\
             <user_query>Briefly inform the user about the task result and perform any follow-up actions (if needed).</user_query>"
        )
    }

    fn parse_single_user_msg(content: &str) -> MessageState {
        let blob = serde_json::json!({"role": "user", "content": content});
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);
        ops.into_iter()
            .find_map(|op| match op {
                TreeOperation::Append { message, .. } if message.id.starts_with("user_msg:") => {
                    Some(message)
                }
                _ => None,
            })
            .expect("user message")
    }

    #[test]
    fn test_system_notification_is_summarized() {
        let msg = parse_single_user_msg(&notification_content("success"));
        assert_eq!(
            msg.text.as_deref(),
            Some("Completed: Restart worktree DAI dev server")
        );
        assert_eq!(msg.tag.as_deref(), Some("system_notification"));
        assert_eq!(msg.message_type, MessageType::UserMessage);
        assert!(msg.timestamp.is_some());
        assert!(!msg.hidden.is_hidden());
    }

    #[test]
    fn test_system_notification_status_labels() {
        let cases = [
            ("error", "Failed"),
            ("cancelled", "Cancelled"),
            ("timeout", "Timeout"),
        ];
        for (status, label) in cases {
            let msg = parse_single_user_msg(&notification_content(status));
            assert_eq!(
                msg.text.as_deref(),
                Some(format!("{label}: Restart worktree DAI dev server").as_str()),
                "status {status}"
            );
        }
    }

    #[test]
    fn test_system_notification_without_task_falls_back() {
        let content = "<system_notification>\nSomething happened\n</system_notification>\n<user_query>Do the thing</user_query>";
        let msg = parse_single_user_msg(content);
        assert_eq!(msg.text.as_deref(), Some("Do the thing"));
        assert_eq!(msg.tag, None);
    }

    #[test]
    fn test_system_notification_mid_message_not_detected() {
        let content = format!(
            "<user_query>Why does this show up?\n{}</user_query>",
            "<system_notification><task>\nstatus: success\ntitle: X\n</task></system_notification>"
        );
        let msg = parse_single_user_msg(&content);
        assert!(msg.text.as_deref().unwrap().starts_with("Why does this show up?"));
        assert_eq!(msg.tag, None);
    }

    #[test]
    fn test_task_notification_splits_header_and_body() {
        let text = "<system_notification>\n<task>\nkind: subagent\nstatus: success\n\
                    task_id: abc\ntitle: Explore\ndetail: Exploring things\n\
                    # Report\nNote: keep me\n</task>\n</system_notification>";
        let n = TaskNotification::parse(text).unwrap();
        assert_eq!(n.task_id, Some("abc"));
        assert_eq!(n.detail, Some("Exploring things"));
        assert_eq!(n.body, "# Report\nNote: keep me");
    }

    /// Feeds a background tool call + its result through `parse_blob`.
    /// `output` is the tool message's `highLevelToolCallResult.output`.
    fn start_background_call(
        state: &mut ParseState,
        tool_name: &str,
        tc_id: &str,
        output: serde_json::Value,
    ) {
        state.pending_tool_calls.insert(
            tc_id.to_string(),
            serde_json::json!({"type": "tool-call", "toolName": tool_name, "toolCallId": tc_id, "args": {}}),
        );
        let tool_blob = serde_json::json!({
            "role": "tool",
            "content": [{"type": "tool-result", "toolName": tool_name, "toolCallId": tc_id, "result": "started"}],
            "providerOptions": {"cursor": {"highLevelToolCallResult": {"output": output}}}
        });
        // Helpers reuse blob IDs; forget them so repeated calls aren't deduplicated.
        state.seen_blobs.clear();
        parse_blob(&"c".repeat(64), tool_blob.to_string().as_bytes(), state);
    }

    fn parse_notification(state: &mut ParseState, task: &str) -> Vec<TreeOperation> {
        let content =
            format!("<system_notification>\nFinished.\n\n<task>\n{task}\n</task>\n</system_notification>");
        let blob = serde_json::json!({"role": "user", "content": content});
        state.seen_blobs.clear();
        parse_blob(&"e".repeat(64), blob.to_string().as_bytes(), state)
    }

    #[test]
    fn test_shell_notification_appends_outcome_to_originating_call() {
        let mut state = ParseState::new();
        start_background_call(
            &mut state,
            "Shell",
            "tc_shell",
            serde_json::json!({"success": {"shellId": 825186}, "isBackground": true}),
        );
        // Backgrounded after a timeout: no `isBackground`, only `backgroundReason`.
        start_background_call(
            &mut state,
            "Shell",
            "tc_timeout",
            serde_json::json!({"success": {"shellId": 7, "backgroundReason": "SHELL_BACKGROUND_REASON_TIMEOUT"}}),
        );
        let ops = parse_notification(
            &mut state,
            "kind: shell\nstatus: error\ntask_id: 825186\ntitle: Start server\ndetail: exit_code=143",
        );
        assert_eq!(ops.len(), 2, "user message + linked result");
        match &ops[1] {
            TreeOperation::Append { parent_id, message } => {
                assert_eq!(parent_id.as_deref(), Some("tool_call:tc_shell"));
                assert_eq!(message.text.as_deref(), Some("Failed (exit_code=143)"));
                assert_eq!(message.tag.as_deref(), Some("error"));
                assert_eq!(message.message_type, MessageType::ToolResult);
            }
            _ => panic!("expected Append"),
        }

        let ops = parse_notification(
            &mut state,
            "kind: shell\nstatus: success\ntask_id: 7\ntitle: Slow build",
        );
        assert!(
            matches!(&ops[1], TreeOperation::Append { parent_id, message }
                if parent_id.as_deref() == Some("tool_call:tc_timeout")
                    && message.text.as_deref() == Some("Completed")),
            "timeout-backgrounded shell should link"
        );
    }

    #[test]
    fn test_task_result_keeps_tool_call_with_summary_child() {
        let mut state = ParseState::new();
        state.pending_tool_calls.insert(
            "tc2".to_string(),
            serde_json::json!({"type": "tool-call", "toolName": "Task", "toolCallId": "tc2",
                               "args": {"description": "Explore"}}),
        );
        let tool_blob = serde_json::json!({
            "role": "tool",
            "content": [{"type": "tool-result", "toolName": "Task", "toolCallId": "tc2", "result": "done"}]
        });
        let ops = parse_blob(&"d".repeat(64), tool_blob.to_string().as_bytes(), &mut state);

        let [TreeOperation::Replace { id, message }] = ops.as_slice() else {
            panic!("expected a single Replace");
        };
        assert_eq!(id, "tool_call:tc2");
        assert_eq!(message.id, "tool_call:tc2");
        assert_eq!(message.message_type, MessageType::ToolCall);
        assert_eq!(message.tag.as_deref(), Some("success"));
        assert!(message.props.is_some(), "args kept for ToolFormatter");
        assert_eq!(message.children.len(), 1);
        assert_eq!(message.children[0].id, "task_summary:tc2");
        assert_eq!(message.children[0].text.as_deref(), Some("done"));
    }

    /// Parses a Task tool result whose message-level `highLevelToolCallResult` is `hl`.
    fn parse_task_result(result: &str, hl: serde_json::Value) -> MessageState {
        let mut state = ParseState::new();
        state.pending_tool_calls.insert(
            "tc".to_string(),
            serde_json::json!({"type": "tool-call", "toolName": "Task", "toolCallId": "tc",
                               "args": {"description": "Explore"}}),
        );
        let tool_blob = serde_json::json!({
            "role": "tool",
            "content": [{"type": "tool-result", "toolName": "Task", "toolCallId": "tc", "result": result}],
            "providerOptions": {"cursor": {"highLevelToolCallResult": hl}}
        });
        let ops = parse_blob(&"d".repeat(64), tool_blob.to_string().as_bytes(), &mut state);
        match ops.into_iter().next() {
            Some(TreeOperation::Replace { message, .. }) => message,
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn test_task_result_renders_conversation_steps() {
        let task = parse_task_result(
            "Report: done",
            serde_json::json!({"output": {"success": {"conversationSteps": [
                {"thinkingMessage": {"text": "hmm", "durationMs": 5}},
                {"assistantMessage": {"text": "Looking."}},
                // Final message, already quoted in the summary → skipped.
                {"assistantMessage": {"text": "done"}},
                {"toolCall": {"shellToolCall": {
                    "args": {"command": "ls", "toolCallId": "x"},
                    "result": {"success": {"exitCode": 0}}}}},
                {"toolCall": {"webFetchToolCall": {
                    "args": {"url": "https://example.com"},
                    "result": {"failure": {"message": "nope"}}}}},
            ]}}}),
        );
        assert_eq!(task.tag.as_deref(), Some("success"));
        let kids: Vec<_> = task
            .children
            .iter()
            .map(|c| (c.message_type.clone(), c.text.clone().unwrap_or_default(), c.tag.clone()))
            .collect();
        assert_eq!(
            kids,
            vec![
                (MessageType::TaskSummary, "Report: done".to_string(), None),
                (MessageType::Thinking, "hmm".to_string(), None),
                (MessageType::AgentMessage, "Looking.".to_string(), None),
                (MessageType::ToolCall, "Shell".to_string(), Some("success".to_string())),
                (MessageType::ToolCall, "WebFetch".to_string(), Some("error".to_string())),
            ]
        );
        assert_eq!(
            task.children[3].props,
            Some(serde_json::json!({"command": "ls"})),
            "args become props, minus toolCallId"
        );
    }

    #[test]
    fn test_task_error_result_is_tagged_error() {
        let task = parse_task_result(
            "Error: Invalid arguments",
            serde_json::json!({"output": {"error": {"error": "Invalid arguments:\nbad subagent_type"}},
                               "isError": true}),
        );
        assert_eq!(task.tag.as_deref(), Some("error"));
        assert_eq!(task.children.len(), 1);
        assert_eq!(task.children[0].message_type, MessageType::ToolResult);
        assert_eq!(
            task.children[0].text.as_deref(),
            Some("Invalid arguments:\nbad subagent_type")
        );
    }

    #[test]
    fn test_subagent_notification_fills_task_summary() {
        let mut state = ParseState::new();
        start_background_call(
            &mut state,
            "Task",
            "tc_task",
            serde_json::json!({"success": {"agentId": "agent-1", "isBackground": true}}),
        );
        let ops = parse_notification(
            &mut state,
            "kind: subagent\nstatus: success\ntask_id: agent-1\ntitle: Explore\n\
             detail: Exploring\n# Report\nAll good.",
        );
        assert_eq!(ops.len(), 2, "user message + linked summary");
        match &ops[1] {
            TreeOperation::Replace { id, message } => {
                assert_eq!(id, "task_summary:tc_task");
                assert_eq!(message.text.as_deref(), Some("# Report\nAll good."));
                assert_eq!(message.message_type, MessageType::TaskSummary);
            }
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn test_notification_without_background_origin_is_not_linked() {
        let mut state = ParseState::new();
        // Foreground shell: carries a shellId but no isBackground flag.
        start_background_call(
            &mut state,
            "Shell",
            "tc_fg",
            serde_json::json!({"success": {"shellId": 1}}),
        );
        for task_id in ["1", "999"] {
            let ops = parse_notification(
                &mut state,
                &format!("kind: shell\nstatus: success\ntask_id: {task_id}\ntitle: X"),
            );
            assert_eq!(ops.len(), 1, "task_id {task_id}: only the user message");
        }
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
    fn test_parse_user_blob_appends_at_root() {
        let blob = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "Hello world"}]
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&fake_blob_id(), blob.to_string().as_bytes(), &mut state);

        assert!(
            append_ids(&ops)
                .iter()
                .any(|id| id.starts_with("user_msg:"))
        );
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops), "no turn containers should be emitted");
    }

    #[test]
    fn test_parse_assistant_blob_emits_text_node() {
        let id = "b".repeat(64);
        let blob = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "Hi there"}]
        });
        let mut state = ParseState::new();
        let ops = parse_blob(&id, blob.to_string().as_bytes(), &mut state);

        assert!(append_ids(&ops).iter().any(|id| id.starts_with("text:")));
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops));
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
    fn test_consecutive_user_blobs_both_append_at_root() {
        let mut state = ParseState::new();

        let u1 = serde_json::json!({"role": "user", "content": "hello"});
        let u1_id = "a".repeat(64);
        let u2 = serde_json::json!({"role": "user", "content": "follow-up"});
        let u2_id = "b".repeat(64);

        let mut ops = parse_blob(&u1_id, u1.to_string().as_bytes(), &mut state);
        ops.extend(parse_blob(&u2_id, u2.to_string().as_bytes(), &mut state));

        let ids = append_ids(&ops);
        assert_eq!(
            ids.len(),
            2,
            "back-to-back user messages must not be wrapped"
        );
        assert!(ids[0].starts_with("user_msg:a"));
        assert!(ids[1].starts_with("user_msg:b"));
        assert!(all_appends_at_root(&ops));
        assert!(!has_container(&ops));
    }

    /// A scripted round-trip: everything lands at root level, except tool results,
    /// which nest under the tool call they answer.
    #[test]
    fn test_round_trip_emits_flat_tree() {
        let mut state = ParseState::new();
        let blobs = [
            (
                "a".repeat(64),
                serde_json::json!({"role": "user", "content": "please read the file"}),
            ),
            (
                "b".repeat(64),
                serde_json::json!({"role": "assistant", "content": [
                    {"type": "text", "text": "sure"},
                    {"type": "tool-call", "toolName": "Read", "toolCallId": "tc1", "args": {}}
                ]}),
            ),
            (
                "c".repeat(64),
                serde_json::json!({"role": "tool", "content": [
                    {"type": "tool-result", "toolName": "Read", "toolCallId": "tc1", "result": "contents"}
                ]}),
            ),
            (
                "d".repeat(64),
                serde_json::json!({"role": "user", "content": "thanks"}),
            ),
        ];

        let mut ops = Vec::new();
        for (id, blob) in &blobs {
            ops.extend(parse_blob(id, blob.to_string().as_bytes(), &mut state));
        }

        assert!(!has_container(&ops), "no turn containers should be emitted");
        for op in &ops {
            if let TreeOperation::Append { parent_id, message } = op {
                match parent_id.as_deref() {
                    None => {}
                    Some("tool_call:tc1") => {
                        assert_eq!(message.message_type, MessageType::ToolResult)
                    }
                    Some(other) => panic!("unexpected parent {other} for {}", message.id),
                }
            }
        }
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
