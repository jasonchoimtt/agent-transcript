use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::PathBuf;

use tracing::{debug, info};

use super::message::{ParseState, parse_entry};
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{MessageState, MessageType};

/// Tracks a sub-agent whose JSONL is being (or will be) streamed live.
struct SubagentWatcher {
    /// ID of the parent `Agent` tool_use block. `None` until matched via description.
    tool_use_id: Option<String>,
    /// Description from meta.json; used to reverse-lookup the tool_use_id when
    /// on_subagent_tool_use fires after the watcher was created.
    description: Option<String>,
    placeholder_emitted: bool,
    /// How many bytes of the sub-agent JSONL have already been parsed.
    byte_offset: usize,
    /// Parsing state for this sub-agent (id_prefix + suppress_containers set).
    parse_state: ParseState,
}

pub struct ClaudeSubagentManager {
    session_dir: PathBuf,
    /// description → tool_use_id for Agent tool_uses seen in the main JSONL.
    tool_uses_by_description: HashMap<String, String>,
    /// agent_id → tool_use_id for Agent tool_result seen in the main JSONL.
    tool_uses_by_agent_id: HashMap<String, String>,
    /// agent_id → watcher for all known sub-agents; entries are never removed (only cleared
    /// on rewind) so completion events for already-finished agents remain resolvable.
    subagents: HashMap<String, SubagentWatcher>,
}

impl ClaudeSubagentManager {
    pub fn new(session_dir: PathBuf) -> ClaudeSubagentManager {
        ClaudeSubagentManager {
            session_dir,
            tool_uses_by_description: HashMap::new(),
            tool_uses_by_agent_id: HashMap::new(),
            subagents: HashMap::new(),
        }
    }

    /// Called on initial read to back-fill all subagents
    pub fn on_init(&mut self) -> color_eyre::Result<Vec<TreeOperation>> {
        let mut ops = Vec::new();

        let subagents_dir = self.session_dir.join("subagents");
        if fs::exists(&subagents_dir)? {
            for entry in fs::read_dir(&subagents_dir)? {
                let entry = entry?;
                if let Some(ext) = entry.path().extension()
                    && ext == "jsonl"
                {
                    ops.extend(self.on_subagent_event(entry.path())?);
                }
            }
        }

        Ok(ops)
    }

    /// Called when an `Agent` tool_use block is first encountered in the main JSONL.
    /// Registers `description → tool_use_id` in `tool_uses_by_description` and resolves any
    /// watcher that was created by a meta.json event that raced ahead of the main reader.
    pub fn on_subagent_tool_use(
        &mut self,
        tool_use_id: &str,
        description: &str,
    ) -> color_eyre::Result<Vec<TreeOperation>> {
        info!("detected tool use {tool_use_id} with description {description}");
        self.tool_uses_by_description
            .insert(description.to_string(), tool_use_id.to_string());

        // Find an unmatched watcher created by a meta.json event that arrived first.
        let agent_id = self
            .subagents
            .iter()
            .find(|(_, w)| w.description.as_deref() == Some(description) && w.tool_use_id.is_none())
            .map(|(id, _)| id.clone());

        for (_, sa) in self.subagents.iter() {
            info!(
                "dbg {:?} {:?} {:?}",
                sa.description,
                sa.description.as_deref() == Some(description),
                sa.tool_use_id
            );
        }

        info!("matching agent_id: {:?}", agent_id);

        let Some(agent_id) = agent_id else {
            return Ok(vec![]);
        };

        self.subagents.get_mut(&agent_id).unwrap().tool_use_id = Some(tool_use_id.to_string());
        info!(%agent_id, %tool_use_id, "resolved waiting subagent watcher via on_subagent_tool_use");

        self.read_subagent_jsonl(&agent_id)
    }

    pub fn on_subagent_tool_result(
        &mut self,
        tool_use_id: &str,
        agent_id: &str,
    ) -> color_eyre::Result<Vec<TreeOperation>> {
        info!("detected tool use {tool_use_id} with agent ID {agent_id}");
        self.tool_uses_by_agent_id
            .insert(agent_id.to_string(), tool_use_id.to_string());

        let Some(subagent) = self.subagents.get_mut(agent_id) else {
            return Ok(vec![]);
        };

        subagent.tool_use_id = Some(tool_use_id.to_string());
        info!(%agent_id, %tool_use_id, "resolved waiting subagent watcher via on_subagent_tool_result");

        self.read_subagent_jsonl(&agent_id)
    }

    /// Dispatch a sub-agent file event (meta.json or agent JSONL).
    /// Returns ops to emit; an empty vec means the event was a no-op.
    pub fn on_subagent_event(&mut self, path: PathBuf) -> color_eyre::Result<Vec<TreeOperation>> {
        debug!("on_subagent_event on {path:?}");
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let Some(agent_id) =
            parse_meta_filename(&filename).or_else(|| parse_sa_jsonl_filename(&filename))
        else {
            return Ok(vec![]);
        };

        self.detect_subagent(&agent_id, &path);

        self.read_subagent_jsonl(&agent_id)
    }

    pub fn clear(&mut self) {
        self.subagents.clear();
        self.tool_uses_by_description.clear();
    }

    fn detect_subagent(&mut self, agent_id: &str, path: &PathBuf) {
        if !self.subagents.contains_key(agent_id) {
            info!("starting subagent watcher for {agent_id}");

            self.subagents.insert(
                agent_id.to_string(),
                SubagentWatcher {
                    tool_use_id: None,
                    description: None,
                    placeholder_emitted: false,
                    byte_offset: 0,
                    parse_state: ParseState {
                        id_prefix: format!("sa:{}:", agent_id),
                        suppress_containers: true,
                        ..Default::default()
                    },
                },
            );
        }
        let subagent = self.subagents.get_mut(agent_id).unwrap();

        if subagent.description.is_none() {
            subagent.description = std::fs::read_to_string(&path)
                .ok()
                .and_then(|meta_text| serde_json::from_str::<serde_json::Value>(&meta_text).ok())
                .and_then(|meta_obj| meta_obj["description"].as_str().map(|s| s.to_string()));
            if let Some(ref description) = subagent.description {
                info!("subagent {agent_id} detected description {description}");
            }
        }

        if subagent.tool_use_id.is_none()
            && let Some(ref description) = subagent.description
        {
            subagent.tool_use_id = self
                .tool_uses_by_description
                .get(description)
                .map(|d| d.clone());
            if let Some(ref tool_use_id) = subagent.tool_use_id {
                info!("subagent {agent_id} matched with tool use {tool_use_id}");
            }
        }

        if subagent.tool_use_id.is_none() {
            subagent.tool_use_id = self.tool_uses_by_agent_id.get(agent_id).map(|d| d.clone());
            if let Some(ref tool_use_id) = subagent.tool_use_id {
                info!("subagent {agent_id} matched with tool use {tool_use_id}");
            }
        }
    }

    /// Read new bytes from `session_dir/subagents/agent-{agent_id}.jsonl`, parse complete lines,
    /// and re-parent every `Append` op under `tool_call:{tool_use_id}`.
    /// Returns `[]` immediately if the watcher has no `tool_use_id` yet (bytes are preserved
    /// at offset 0 so they are not silently dropped).
    fn read_subagent_jsonl(&mut self, agent_id: &str) -> color_eyre::Result<Vec<TreeOperation>> {
        let Some(subagent) = self.subagents.get_mut(agent_id) else {
            return Ok(vec![]);
        };
        let Some(tool_use_id) = subagent.tool_use_id.as_ref() else {
            return Ok(vec![]);
        };

        let mut ops = Vec::new();

        if !subagent.placeholder_emitted {
            subagent.placeholder_emitted = true;
            ops.push(TreeOperation::Append {
                parent_id: Some(format!("tool_call:{}", tool_use_id)),
                message: MessageState::new(format!("task_summary:{}", tool_use_id))
                    .text(format!("Agent ID: {agent_id}"))
                    .message_type(MessageType::TaskSummary),
            });
        }

        let path = self
            .session_dir
            .join("subagents")
            .join(format!("agent-{}.jsonl", agent_id));

        let tool_call_id = format!("tool_call:{}", tool_use_id);

        let file = match File::open(&path) {
            Ok(f) => f,
            Err(error) => match error.kind() {
                ErrorKind::NotFound => return Ok(ops),
                _ => Err(error)?,
            },
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut consumed_count = 0;

        reader.seek(SeekFrom::Start(subagent.byte_offset as u64))?;
        loop {
            line.clear();
            let line_byte_len = reader.read_line(&mut line)?;

            if !(line.ends_with('\r') || line.ends_with('\n')) {
                debug!("incomplete last line, stopping here");
                break;
            }

            let line_ops = parse_entry(line.trim(), &mut subagent.parse_state)?;
            for op in line_ops {
                ops.push(match op {
                    TreeOperation::Append { message, .. } => TreeOperation::Append {
                        parent_id: Some(tool_call_id.clone()),
                        message,
                    },
                    other => other,
                });
            }
            subagent.byte_offset += line_byte_len;
            consumed_count += 1;
        }

        debug!(
            agent_id,
            count = consumed_count,
            "streamed sub-agent JSONL messages"
        );
        Ok(ops)
    }
}

/// Extract `agent_id` from `agent-{id}.meta.json`, or `None` if the name doesn't match.
fn parse_meta_filename(name: &str) -> Option<String> {
    name.strip_prefix("agent-")
        .and_then(|s| s.strip_suffix(".meta.json"))
        .map(|s| s.to_string())
}

/// Extract `agent_id` from `agent-{id}.jsonl`, or `None` if the name doesn't match.
fn parse_sa_jsonl_filename(name: &str) -> Option<String> {
    name.strip_prefix("agent-")
        .and_then(|s| s.strip_suffix(".jsonl"))
        .map(|s| s.to_string())
}
