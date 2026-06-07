use std::collections::{BTreeMap, HashMap, HashSet};

use crate::config::{ToolGroup, ToolGrouperConfig};
use crate::transforms::Transform;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{MessageState, MessageType};

/// Groups consecutive matching tool-call nodes into a collapsed Container node.
///
/// Uses an emit-then-restructure approach: tool calls are emitted immediately under
/// their original parent so they are always visible in real time, then restructured
/// into a Container the moment `min_count` is reached in the same batch.
///
/// Tools sharing the same parent_id are treated as one run; a different parent
/// (or a non-tool op) breaks the run and may seal the active container.
///
/// State machine:
/// - `Collecting` (buffer may be empty) — accumulating tool calls; no group committed yet.
///   An empty buffer is the initial / idle state (equivalent to the old `None`).
/// - `Grouped` (container in tree) — appending additional tools as children.
///
/// When a new tool call extends the buffer, the longest suffix of the buffer that all
/// match a single configured group is evaluated.  If that suffix length reaches the
/// group's `min_count`, immediately emit Remove(t2..tN) + Replace(t1 → Container)
/// and transition to Grouped.
pub struct ToolGrouper {
    groups: Vec<ToolGroup>,
    active: ActiveRun,
    allow_thinking: bool,
    /// Sealed containers for Remove propagation: container_id → child_ids.
    committed_groups: HashMap<String, HashSet<String>>,
    counter: usize,
}

enum ActiveRun {
    /// Accumulating tool calls. An empty buffer is the idle (no active run) state.
    /// No group is committed yet; `parent_id` is shared by all buffered tools and
    /// is irrelevant when the buffer is empty.
    Collecting {
        parent_id: Option<String>,
        buffer: Vec<(String, MessageState)>,
    },
    /// Container exists in the tree; more tools may still arrive.
    Grouped {
        group_idx: usize,
        /// Parent under which the container lives.
        parent_id: Option<String>,
        container_id: String,
        /// Children in insertion order — needed to reconstruct the sealed Replace.
        children: Vec<MessageState>,
        child_ids: HashSet<String>,
        /// Thinking messages emitted (under the container's parent) since the last
        /// tool call.  If a subsequent matching tool call arrives they are Remove'd
        /// and re-Append'd inside the container; if the container seals first they
        /// stay where they are.
        thinking_buffer: Vec<(String, MessageState)>,
    },
}

impl ToolGrouper {
    pub fn new(config: ToolGrouperConfig) -> Self {
        Self {
            groups: config.effective_groups(),
            allow_thinking: config.allow_thinking,
            active: ActiveRun::Collecting {
                parent_id: None,
                buffer: vec![],
            },
            committed_groups: HashMap::new(),
            counter: 0,
        }
    }

    fn next_container_id(&mut self) -> String {
        let id = format!("tool_group:{}", self.counter);
        self.counter += 1;
        id
    }

    /// Returns `Some((group_idx, parent_id))` if the op is a ToolCall Append matching a
    /// configured group. Works at any tree depth; the caller uses `parent_id` to ensure
    /// only tools under the same parent are grouped together.
    fn find_matching_group(&self, op: &TreeOperation) -> Option<(usize, Option<String>)> {
        let (parent_id, message) = match op {
            TreeOperation::Append { parent_id, message } => (parent_id, message),
            _ => return None,
        };
        if message.message_type != MessageType::ToolCall {
            return None;
        }
        let tool_name = extract_tool_name(&message.text);
        self.groups
            .iter()
            .position(|g| group_matches(g, tool_name))
            .map(|idx| (idx, parent_id.clone()))
    }

    /// Returns `(group_idx, suffix_len)` for the longest suffix of `buffer` whose items
    /// all match a single configured group, provided that length ≥ `min_count`.
    /// When `allow_thinking` is set, Thinking messages within the suffix are skipped
    /// (they do not count toward `min_count`).  The returned suffix is also trimmed of
    /// any leading Thinking messages so the container never starts with a thinking node.
    /// Returns `None` if no group's threshold is met.
    fn find_group_for_suffix(&self, buffer: &[(String, MessageState)]) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        for (g_idx, group) in self.groups.iter().enumerate() {
            let mut tool_count = 0usize;
            let mut raw_len = 0usize;
            for (_, msg) in buffer.iter().rev() {
                if msg.message_type == MessageType::Thinking {
                    if self.allow_thinking {
                        raw_len += 1;
                    } else {
                        break;
                    }
                } else if group_matches(group, extract_tool_name(&msg.text)) {
                    tool_count += 1;
                    raw_len += 1;
                } else {
                    break;
                }
            }
            if tool_count < group.min_count {
                continue;
            }
            // Trim any leading Thinking items: the container must not start with a thinking.
            let run_start = buffer.len() - raw_len;
            let leading = buffer[run_start..]
                .iter()
                .take_while(|(_, m)| m.message_type == MessageType::Thinking)
                .count();
            let suffix_len = raw_len - leading;
            if suffix_len > 0 && best.is_none_or(|(_, bk)| suffix_len > bk) {
                best = Some((g_idx, suffix_len));
            }
        }
        best
    }

    /// Build Remove + Replace ops that create the container from `buffer`.
    /// Returns (container_id, children, child_ids, ops_to_emit).
    fn do_transition(
        &mut self,
        group_idx: usize,
        buffer: Vec<(String, MessageState)>,
    ) -> (
        String,
        Vec<MessageState>,
        HashSet<String>,
        Vec<TreeOperation>,
    ) {
        let container_id = self.next_container_id();
        let mut ops: Vec<TreeOperation> = Vec::new();

        // Remove all buffered nodes after the first from the tree.
        for (id, _) in buffer.iter().skip(1) {
            ops.push(TreeOperation::Remove { id: id.clone() });
        }

        let child_ids: HashSet<String> = buffer.iter().map(|(id, _)| id.clone()).collect();
        let children: Vec<MessageState> = buffer.into_iter().map(|(_, msg)| msg).collect();

        let group = &self.groups[group_idx];
        let container_msg = build_container(
            container_id.clone(),
            children.clone(),
            true,
            &group.name,
            group.shorten_as_glob,
        );
        let tool1_id = children[0].id.clone();
        ops.push(TreeOperation::Replace {
            id: tool1_id,
            message: container_msg,
        });

        (container_id, children, child_ids, ops)
    }

    /// Build the Replace op that seals an existing container (collapses it).
    fn do_seal(
        &self,
        container_id: &str,
        children: &[MessageState],
        group_idx: usize,
    ) -> TreeOperation {
        let group = &self.groups[group_idx];
        let container_msg = build_container(
            container_id.to_string(),
            children.to_vec(),
            false,
            &group.name,
            group.shorten_as_glob,
        );
        TreeOperation::Replace {
            id: container_id.to_string(),
            message: container_msg,
        }
    }

    /// Handle a Remove op against the active run state and committed groups.
    fn handle_remove(&mut self, id: String, output: &mut Vec<TreeOperation>) {
        match &mut self.active {
            ActiveRun::Collecting { buffer, .. } => {
                if let Some(pos) = buffer.iter().position(|(bid, _)| bid == &id) {
                    buffer.remove(pos);
                    output.push(TreeOperation::Remove { id });
                    return;
                }
            }
            ActiveRun::Grouped {
                container_id,
                child_ids,
                children,
                thinking_buffer,
                ..
            } => {
                // Remove from thinking_buffer if present; it was already emitted to the tree.
                if let Some(pos) = thinking_buffer.iter().position(|(tid, _)| tid == &id) {
                    thinking_buffer.remove(pos);
                    output.push(TreeOperation::Remove { id });
                    return;
                }
                let container_id = container_id.clone();
                if child_ids.remove(&id) {
                    children.retain(|c| c.id != id);
                    output.push(TreeOperation::Remove { id });
                    if child_ids.is_empty() {
                        output.push(TreeOperation::Remove {
                            id: container_id.clone(),
                        });
                        self.active = ActiveRun::Collecting {
                            parent_id: None,
                            buffer: vec![],
                        };
                    }
                    return;
                }
                if id == container_id {
                    self.active = ActiveRun::Collecting {
                        parent_id: None,
                        buffer: vec![],
                    };
                    output.push(TreeOperation::Remove { id });
                    return;
                }
            }
        }

        // Check committed (sealed) groups.
        let mut container_to_remove: Option<String> = None;
        for (cid, child_ids) in &mut self.committed_groups {
            if child_ids.remove(&id) {
                if child_ids.is_empty() {
                    container_to_remove = Some(cid.clone());
                }
                break;
            }
        }
        if let Some(cid) = container_to_remove {
            self.committed_groups.remove(&cid);
            output.push(TreeOperation::Remove { id });
            output.push(TreeOperation::Remove { id: cid });
        } else {
            output.push(TreeOperation::Remove { id });
        }
    }

    fn process_op(&mut self, op: TreeOperation, output: &mut Vec<TreeOperation>) {
        match op {
            TreeOperation::Remove { id } => {
                self.handle_remove(id, output);
                return;
            }
            TreeOperation::Update { .. } => {
                output.push(op);
                return;
            }
            _ => {}
        }

        let matched_group = self.find_matching_group(&op);
        let current_active = std::mem::replace(
            &mut self.active,
            ActiveRun::Collecting {
                parent_id: None,
                buffer: vec![],
            },
        );

        match current_active {
            ActiveRun::Collecting {
                parent_id: cur_parent,
                mut buffer,
            } => {
                // Thinking with a non-empty buffer: emit immediately (visible in tree) and
                // add to the buffer so it's included if/when the container forms.
                // Thinking with an empty buffer: pass through (a run cannot start with a thinking).
                if self.allow_thinking && is_thinking_append(&op) {
                    if buffer.is_empty() {
                        output.push(op);
                        self.active = ActiveRun::Collecting {
                            parent_id: None,
                            buffer,
                        };
                    } else {
                        let TreeOperation::Append {
                            parent_id: op_parent,
                            message,
                        } = op
                        else {
                            unreachable!()
                        };
                        let id = message.id.clone();
                        output.push(TreeOperation::Append {
                            parent_id: op_parent,
                            message: message.clone(),
                        });
                        buffer.push((id, message));
                        self.active = ActiveRun::Collecting {
                            parent_id: cur_parent,
                            buffer,
                        };
                    }
                    return;
                }

                match matched_group {
                    // Matching tool call with same parent (or empty buffer → first tool).
                    Some((_, ref pid)) if buffer.is_empty() || *pid == cur_parent => {
                        let new_parent = if buffer.is_empty() {
                            pid.clone()
                        } else {
                            cur_parent
                        };
                        let (id, msg) = extract_append(op);
                        output.push(TreeOperation::Append {
                            parent_id: new_parent.clone(),
                            message: msg.clone(),
                        });
                        buffer.push((id, msg));

                        if let Some((g_idx, suffix_len)) = self.find_group_for_suffix(&buffer) {
                            // Drain the matching suffix and build the container from it;
                            // any prefix items are already in the tree as individual nodes.
                            let suffix: Vec<_> =
                                buffer.drain(buffer.len() - suffix_len..).collect();
                            let (container_id, children, child_ids, ops) =
                                self.do_transition(g_idx, suffix);
                            output.extend(ops);
                            self.active = ActiveRun::Grouped {
                                group_idx: g_idx,
                                parent_id: new_parent,
                                container_id,
                                children,
                                child_ids,
                                thinking_buffer: vec![],
                            };
                        } else {
                            self.active = ActiveRun::Collecting {
                                parent_id: new_parent,
                                buffer,
                            };
                        }
                    }
                    _ => {
                        // Before clearing: let Replace/child-Append ops for buffered nodes
                        // pass through without breaking the run. This handles the pattern
                        // where each tool call is immediately followed by its tool result
                        // (Replace + Append(ToolResult)) before the next tool call arrives.
                        let buffer_ids: Vec<&str> =
                            buffer.iter().map(|(id, _)| id.as_str()).collect();
                        if is_run_related(&op, &buffer_ids) {
                            if let TreeOperation::Replace {
                                ref id,
                                ref message,
                            } = op
                            {
                                // Capture the updated message (e.g. success/error tag) so the
                                // container label and tag are correct when transition fires.
                                if let Some((_, stored)) =
                                    buffer.iter_mut().find(|(bid, _)| bid == id)
                                {
                                    *stored = message.clone();
                                }
                            }
                            // Mirror child Appends (e.g. ToolResult) onto the stored copy so
                            // that do_transition builds the container with full child trees.
                            if let TreeOperation::Append {
                                parent_id: Some(ref pid),
                                ref message,
                            } = op
                                && let Some((_, stored)) =
                                    buffer.iter_mut().find(|(bid, _)| bid == pid)
                            {
                                stored.children.push(message.clone());
                            }
                            output.push(op);
                            self.active = ActiveRun::Collecting {
                                parent_id: cur_parent,
                                buffer,
                            };
                        } else {
                            // Non-matching op: clear the buffer (individual nodes stay ungrouped).
                            if matched_group.is_some() {
                                self.process_op(op, output);
                            } else {
                                output.push(op);
                            }
                        }
                    }
                }
            }

            ActiveRun::Grouped {
                group_idx,
                parent_id,
                container_id,
                mut children,
                mut child_ids,
                mut thinking_buffer,
            } => {
                // Thinking arrives while grouped: emit it immediately so it stays visible,
                // but track it in thinking_buffer.  If a subsequent matching tool call
                // arrives the thinking is Removed and re-Appended inside the container;
                // if the container seals it stays in the tree under the container's parent.
                if self.allow_thinking && is_thinking_append(&op) {
                    let TreeOperation::Append {
                        parent_id: op_parent,
                        message,
                    } = op
                    else {
                        unreachable!()
                    };
                    let id = message.id.clone();
                    output.push(TreeOperation::Append {
                        parent_id: op_parent,
                        message: message.clone(),
                    });
                    thinking_buffer.push((id, message));
                    self.active = ActiveRun::Grouped {
                        group_idx,
                        parent_id,
                        container_id,
                        children,
                        child_ids,
                        thinking_buffer,
                    };
                    return;
                }

                match matched_group {
                    // Only extend the grouped run for the same group AND same parent.
                    Some((g, ref pid)) if g == group_idx && *pid == parent_id => {
                        // Move pending thinking messages into the container:
                        // Remove from their current position, re-Append under the container.
                        for (tid, tmsg) in std::mem::take(&mut thinking_buffer) {
                            output.push(TreeOperation::Remove { id: tid.clone() });
                            output.push(TreeOperation::Append {
                                parent_id: Some(container_id.clone()),
                                message: tmsg.clone(),
                            });
                            child_ids.insert(tid);
                            children.push(tmsg);
                        }
                        let (id, msg) = extract_append(op);
                        output.push(TreeOperation::Append {
                            parent_id: Some(container_id.clone()),
                            message: msg.clone(),
                        });
                        child_ids.insert(id);
                        children.push(msg);
                        // Update the container's summary line so it reflects the
                        // growing child count during live streaming (not just at seal time).
                        let group = &self.groups[group_idx];
                        output.push(TreeOperation::Replace {
                            id: container_id.clone(),
                            message: build_container(
                                container_id.clone(),
                                children.clone(),
                                true,
                                &group.name,
                                group.shorten_as_glob,
                            ),
                        });
                        self.active = ActiveRun::Grouped {
                            group_idx,
                            parent_id,
                            container_id,
                            children,
                            child_ids,
                            thinking_buffer,
                        };
                    }
                    _ => {
                        // Before sealing: let Replace/child-Append ops for grouped children
                        // pass through without breaking the run. Same reasoning as Collecting.
                        let child_id_refs: Vec<&str> =
                            child_ids.iter().map(|s| s.as_str()).collect();
                        if is_run_related(&op, &child_id_refs) {
                            if let TreeOperation::Replace {
                                ref id,
                                ref message,
                            } = op
                            {
                                // Update the stored child so seal/container updates are accurate.
                                if let Some(stored) = children.iter_mut().find(|c| c.id == *id) {
                                    *stored = message.clone();
                                }
                            }
                            // Mirror child Appends (e.g. ToolResult) onto the stored copy so
                            // that do_seal builds the container with full child trees.
                            if let TreeOperation::Append {
                                parent_id: Some(ref pid),
                                ref message,
                            } = op
                                && let Some(stored) = children.iter_mut().find(|c| c.id == *pid)
                            {
                                stored.children.push(message.clone());
                            }
                            output.push(op);
                            self.active = ActiveRun::Grouped {
                                group_idx,
                                parent_id,
                                container_id,
                                children,
                                child_ids,
                                thinking_buffer,
                            };
                        } else {
                            // Non-matching: thinking_buffer items are already in the tree
                            // under the container's parent — nothing extra to emit.
                            let seal_op = self.do_seal(&container_id, &children, group_idx);
                            output.push(seal_op);
                            self.committed_groups.insert(container_id, child_ids);
                            // Start a fresh run if this op is itself a matching tool.
                            if matched_group.is_some() {
                                self.process_op(op, output);
                            } else {
                                output.push(op);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Transform for ToolGrouper {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        let mut output = Vec::new();
        for op in ops {
            self.process_op(op, &mut output);
        }
        output
    }

    fn reset(&mut self) {
        self.active = ActiveRun::Collecting {
            parent_id: None,
            buffer: vec![],
        };
        self.committed_groups.clear();
        self.counter = 0;
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn container_tag(children: &[MessageState]) -> Option<String> {
    let tools: Vec<_> = children
        .iter()
        .filter(|c| c.message_type == MessageType::ToolCall)
        .collect();
    let any_error = tools.iter().any(|c| c.tag.as_deref() == Some("error"));
    if any_error {
        return Some("error".to_string());
    }
    let all_success =
        !tools.is_empty() && tools.iter().all(|c| c.tag.as_deref() == Some("success"));
    if all_success {
        Some("success".to_string())
    } else {
        None
    }
}

fn build_container(
    id: String,
    children: Vec<MessageState>,
    expanded: bool,
    name: &str,
    shorten_as_glob: bool,
) -> MessageState {
    let tag = container_tag(&children);
    let tool_children: Vec<_> = children
        .iter()
        .filter(|c| c.message_type == MessageType::ToolCall)
        .collect();
    let params: Vec<String> = tool_children
        .iter()
        .filter_map(|c| extract_params_from_text(&c.text))
        .collect();
    let label = if params.is_empty() {
        format!("{name}: {} tool calls", tool_children.len())
    } else if shorten_as_glob {
        format!("{name}: {}", compress_paths_to_glob(&params, 2))
    } else {
        format!("{name}: {}", params.join(", "))
    };
    let mut node = MessageState::new(id)
        .message_type(MessageType::ToolCall)
        .expanded(expanded)
        .show_more(false)
        .text(label)
        .children(children);
    if let Some(t) = tag {
        node = node.tag(t);
    }
    node
}

/// Extract the params string from the first line of a formatted tool call text.
/// "Read(src/foo.rs)\n..." → Some("src/foo.rs")
/// "Read" (no props) → None
fn extract_params_from_text(text: &Option<String>) -> Option<String> {
    let first_line = text.as_deref()?.lines().next()?;
    let paren_pos = first_line.find('(')?;
    let rest = &first_line[paren_pos + 1..];
    let params = rest.strip_suffix(')').unwrap_or(rest);
    if params.is_empty() {
        None
    } else {
        Some(params.to_string())
    }
}

// ── glob compression ─────────────────────────────────────────────────────────

struct TrieNode {
    children: BTreeMap<String, TrieNode>,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            children: BTreeMap::new(),
        }
    }

    fn insert(&mut self, segments: &[&str]) {
        if let Some((first, rest)) = segments.split_first() {
            self.children
                .entry(first.to_string())
                .or_insert_with(TrieNode::new)
                .insert(rest);
        }
    }
}

/// Collect all root-to-leaf path strings from `node`.
fn all_leaf_paths(node: &TrieNode) -> Vec<String> {
    if node.children.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    for (seg, child) in &node.children {
        for path in all_leaf_paths(child) {
            if path.is_empty() {
                result.push(seg.clone());
            } else {
                result.push(format!("{seg}/{path}"));
            }
        }
    }
    result
}

/// Returns path alternatives for `node`.
///
/// - Single-child chains are collapsed without incrementing `brace_depth`.
/// - When `brace_depth < max_brace_depth` and multiple children are found, returns
///   a single `{a,b,...}` string (one element Vec).
/// - When `brace_depth >= max_brace_depth` and multiple children, returns all paths
///   flat (multiple elements, no further braces).
fn render_subtrie(mut node: &TrieNode, brace_depth: usize, max_brace_depth: usize) -> Vec<String> {
    if node.children.is_empty() {
        return vec![String::new()];
    }

    // Collapse single-child chains.
    let mut prefix_parts: Vec<String> = Vec::new();
    while node.children.len() == 1 {
        let (seg, child) = node.children.iter().next().unwrap();
        prefix_parts.push(seg.clone());
        node = child;
    }
    let prefix = prefix_parts.join("/");

    if node.children.is_empty() {
        return vec![prefix];
    }

    let result: Vec<String> = if brace_depth >= max_brace_depth {
        // Beyond nesting limit: expand every remaining path flat.
        let mut all = Vec::new();
        for (seg, child) in &node.children {
            for path in all_leaf_paths(child) {
                if path.is_empty() {
                    all.push(seg.clone());
                } else {
                    all.push(format!("{seg}/{path}"));
                }
            }
        }
        all
    } else {
        let mut parts: Vec<String> = Vec::new();
        for (seg, child) in &node.children {
            let child_alts = render_subtrie(child, brace_depth + 1, max_brace_depth);
            match child_alts.as_slice() {
                [] => {}
                [single] if single.is_empty() => parts.push(seg.clone()),
                [single] => parts.push(format!("{seg}/{single}")),
                alts => {
                    // Child was flattened at the depth limit; its alternatives become
                    // siblings at this brace level.
                    for a in alts {
                        if a.is_empty() {
                            parts.push(seg.clone());
                        } else {
                            parts.push(format!("{seg}/{a}"));
                        }
                    }
                }
            }
        }
        let sep = if brace_depth == 0 { ", " } else { "," };
        let glob_expr = if parts.len() == 1 {
            parts.into_iter().next().unwrap()
        } else {
            format!("{{{}}}", parts.join(sep))
        };
        vec![glob_expr]
    };

    if prefix.is_empty() {
        result
    } else {
        result
            .into_iter()
            .map(|r| {
                if r.is_empty() {
                    prefix.clone()
                } else {
                    format!("{prefix}/{r}")
                }
            })
            .collect()
    }
}

/// Removes a single layer of outer `{…}` if the braces wrap the entire string.
fn strip_outer_braces(s: String) -> String {
    if !s.starts_with('{') || !s.ends_with('}') {
        return s;
    }
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return if i == s.len() - 1 {
                        s[1..s.len() - 1].to_string()
                    } else {
                        s
                    };
                }
            }
            _ => {}
        }
    }
    s
}

/// Compress a list of path strings into a brace-glob expression.
/// `max_brace_depth` limits nesting depth (2 means at most two levels of `{…}`).
fn compress_paths_to_glob(paths: &[String], max_brace_depth: usize) -> String {
    let mut unique: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    unique.sort_unstable();
    unique.dedup();

    if unique.is_empty() {
        return String::new();
    }
    if unique.len() == 1 {
        return unique[0].to_string();
    }

    let mut root = TrieNode::new();
    for path in &unique {
        let segs: Vec<&str> = path.split('/').collect();
        root.insert(&segs);
    }

    let alts = render_subtrie(&root, 0, max_brace_depth);
    let result = match alts.as_slice() {
        [] => String::new(),
        [single] => single.clone(),
        parts => format!("{{{}}}", parts.join(",")),
    };
    strip_outer_braces(result)
}

fn is_thinking_append(op: &TreeOperation) -> bool {
    matches!(op, TreeOperation::Append { message, .. } if message.message_type == MessageType::Thinking)
}

fn extract_tool_name(text: &Option<String>) -> &str {
    text.as_deref()
        .and_then(|t| t.split('(').next())
        .unwrap_or("")
}

/// Returns true if `op` is a Replace targeting a known ID, or an Append whose parent is a
/// known ID. Used to let tool-result ops (Replace + Append(ToolResult)) pass through without
/// breaking an active collecting or grouped run.
fn is_run_related(op: &TreeOperation, known_ids: &[&str]) -> bool {
    match op {
        TreeOperation::Replace { id, .. } => known_ids.contains(&id.as_str()),
        TreeOperation::Append {
            parent_id: Some(pid),
            ..
        } => known_ids.contains(&pid.as_str()),
        _ => false,
    }
}

fn group_matches(group: &ToolGroup, tool_name: &str) -> bool {
    group.tools.iter().any(|pattern| {
        glob::Pattern::new(pattern)
            .map(|p| p.matches(tool_name))
            .unwrap_or(false)
    })
}

/// Extract (id, message) from an Append op; panics on any other variant.
fn extract_append(op: TreeOperation) -> (String, MessageState) {
    match op {
        TreeOperation::Append { message, .. } => {
            let id = message.id.clone();
            (id, message)
        }
        _ => panic!("extract_append called on non-Append op"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolGrouperConfig;

    fn tool_op(id: &str, name: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id)
                .message_type(MessageType::ToolCall)
                .text(format!("{name}: args")),
        }
    }

    fn user_op(id: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id).message_type(MessageType::UserMessage),
        }
    }

    fn remove_op(id: &str) -> TreeOperation {
        TreeOperation::Remove { id: id.to_string() }
    }

    fn is_append_id(op: &TreeOperation, id: &str) -> bool {
        matches!(op, TreeOperation::Append { message, .. } if message.id == id)
    }

    fn is_remove_id(op: &TreeOperation, id: &str) -> bool {
        matches!(op, TreeOperation::Remove { id: rid } if rid == id)
    }

    fn is_replace_from(op: &TreeOperation, from_id: &str) -> bool {
        matches!(op, TreeOperation::Replace { id, .. } if id == from_id)
    }

    fn container_id_from_replace(op: &TreeOperation) -> Option<String> {
        match op {
            TreeOperation::Replace { message, .. } => Some(message.id.clone()),
            _ => None,
        }
    }

    fn is_append_to_parent(op: &TreeOperation, tool_id: &str, parent: &str) -> bool {
        matches!(
            op,
            TreeOperation::Append { parent_id: Some(pid), message }
            if pid == parent && message.id == tool_id
        )
    }

    fn config_min3() -> ToolGrouperConfig {
        ToolGrouperConfig {
            groups: vec![crate::config::ToolGroup {
                name: "Tool calls".to_string(),
                tools: vec!["*".to_string()],
                min_count: 3,
                expanded: false,
                shorten_as_glob: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn below_min_count_no_grouping() {
        // 1 tool then user_msg → no Remove/Replace emitted.
        let mut grouper = ToolGrouper::new(config_min3());
        let batch1 = grouper.process(vec![tool_op("t1", "read")]);
        let batch2 = grouper.process(vec![user_op("u1")]);

        assert_eq!(batch1.len(), 1);
        assert!(is_append_id(&batch1[0], "t1"));
        assert_eq!(batch2.len(), 1);
        assert!(is_append_id(&batch2[0], "u1"));
    }

    #[test]
    fn grouping_path_transition_in_first_batch() {
        // Per the plan: transition (Remove + Replace) happens immediately in the
        // same batch that reaches min_count.
        let mut grouper = ToolGrouper::new(config_min3());
        let batch1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        // batch1 should contain: A(t1), A(t2), A(t3), Remove(t2), Remove(t3), Replace(t1→Container)
        assert!(is_append_id(&batch1[0], "t1"));
        assert!(is_append_id(&batch1[1], "t2"));
        assert!(is_append_id(&batch1[2], "t3"));
        assert!(batch1.iter().any(|op| is_remove_id(op, "t2")));
        assert!(batch1.iter().any(|op| is_remove_id(op, "t3")));
        // Replace targets t1 and produces a container with a fresh id ≠ t1.
        let replace = batch1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .expect("Replace(t1→Container) missing");
        let cid = container_id_from_replace(replace).unwrap();
        assert_ne!(cid, "t1");

        // Second batch: user_msg seals the container.
        let batch2 = grouper.process(vec![user_op("u1")]);
        assert!(
            batch2.iter().any(|op| is_replace_from(op, &cid)),
            "seal Replace missing in batch2"
        );
        assert!(batch2.iter().any(|op| is_append_id(op, "u1")));
    }

    #[test]
    fn extended_run() {
        // After transition (which now happens in batch1), t4 arrives → Grouped state
        // emits Append{parent_id: container_id} AND a Replace updating the container count.
        let mut grouper = ToolGrouper::new(config_min3());
        let batch1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        // Get container_id from batch1.
        let cid = batch1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .expect("container Replace missing");

        let batch2 = grouper.process(vec![tool_op("t4", "list")]);
        // Append(t4 under container) + Replace(container with updated count)
        assert_eq!(
            batch2.len(),
            2,
            "should emit child Append and container update Replace"
        );
        assert!(is_append_to_parent(&batch2[0], "t4", &cid));
        assert!(
            is_replace_from(&batch2[1], &cid),
            "second op should be a Replace updating the container"
        );
    }

    #[test]
    fn container_label_updated_during_streaming() {
        // Simulate a live streaming scenario: 5 tools arrive in incremental batches.
        // The container label count must reflect the actual child count in every batch.
        let mut grouper = ToolGrouper::new(config_min3());

        // Batch 1: 3 tools → transition fires, container created with count=3.
        let b1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = b1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .expect("container Replace missing");

        let container_text = |batch: &[TreeOperation], cid: &str| -> Option<String> {
            batch.iter().find_map(|op| match op {
                TreeOperation::Replace { id, message } if id == cid => message.text.clone(),
                _ => None,
            })
        };

        // Batch 2: t4 extends the run.
        let b2 = grouper.process(vec![tool_op("t4", "list")]);
        let text = container_text(&b2, &cid).expect("no container Replace in batch 2");
        assert!(
            text.contains("4 tool calls"),
            "container should say 4 tool calls, got: {text}"
        );

        // Batch 3: t5 extends the run.
        let b3 = grouper.process(vec![tool_op("t5", "grep")]);
        let text = container_text(&b3, &cid).expect("no container Replace in batch 3");
        assert!(
            text.contains("5 tool calls"),
            "container should say 5 tool calls, got: {text}"
        );
    }

    #[test]
    fn remove_child_from_sealed_group() {
        let mut grouper = ToolGrouper::new(config_min3());
        let batch1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = batch1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .unwrap();
        // Seal by sending user_msg.
        grouper.process(vec![user_op("u1")]);
        // Remove one child: container should NOT be removed (still has t1, t3).
        let out = grouper.process(vec![remove_op("t2")]);
        assert!(out.iter().any(|op| is_remove_id(op, "t2")));
        assert!(!out.iter().any(|op| is_remove_id(op, &cid)));
    }

    #[test]
    fn remove_all_children_removes_container() {
        let mut grouper = ToolGrouper::new(config_min3());
        let batch1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = batch1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .unwrap();
        grouper.process(vec![user_op("u1")]);

        grouper.process(vec![remove_op("t1")]);
        grouper.process(vec![remove_op("t2")]);
        let out = grouper.process(vec![remove_op("t3")]);
        // Last removal emits Remove(t3) and Remove(container).
        assert!(out.iter().any(|op| is_remove_id(op, "t3")));
        assert!(out.iter().any(|op| is_remove_id(op, &cid)));
    }

    fn nested_tool_op(id: &str, name: &str, parent: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: Some(parent.to_string()),
            message: MessageState::new(id)
                .message_type(MessageType::ToolCall)
                .text(format!("{name}: args")),
        }
    }

    #[test]
    fn groups_nested_tool_calls() {
        // Tool calls appended under a parent container (e.g. agent_turn) should group.
        let mut grouper = ToolGrouper::new(config_min3());
        let batch1 = grouper.process(vec![
            nested_tool_op("t1", "read", "agent_turn:0"),
            nested_tool_op("t2", "write", "agent_turn:0"),
            nested_tool_op("t3", "exec", "agent_turn:0"),
        ]);
        assert!(is_append_id(&batch1[0], "t1"));
        assert!(is_append_id(&batch1[1], "t2"));
        assert!(is_append_id(&batch1[2], "t3"));
        assert!(batch1.iter().any(|op| is_remove_id(op, "t2")));
        assert!(batch1.iter().any(|op| is_remove_id(op, "t3")));
        assert!(batch1.iter().any(|op| is_replace_from(op, "t1")));
    }

    #[test]
    fn different_parents_not_grouped() {
        // Tools under different parents must not be combined into one group.
        let mut grouper = ToolGrouper::new(config_min3());
        let out = grouper.process(vec![
            nested_tool_op("t1", "read", "agent_turn:0"),
            nested_tool_op("t2", "write", "agent_turn:1"), // different parent — breaks the run
            nested_tool_op("t3", "exec", "agent_turn:0"),
        ]);
        // No grouping: t2 has a different parent, which resets the run.
        assert!(
            !out.iter()
                .any(|op| matches!(op, TreeOperation::Remove { .. }))
        );
        assert!(
            !out.iter()
                .any(|op| matches!(op, TreeOperation::Replace { .. }))
        );
    }

    fn replace_op(id: &str, name: &str, tag: &str) -> TreeOperation {
        TreeOperation::Replace {
            id: id.to_string(),
            message: MessageState::new(id)
                .message_type(MessageType::ToolCall)
                .text(format!("{name}: args"))
                .tag(tag),
        }
    }

    fn result_op(id: &str, parent: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: Some(parent.to_string()),
            message: MessageState::new(id).message_type(MessageType::ToolResult),
        }
    }

    #[test]
    fn interleaved_replace_and_result_does_not_break_collecting() {
        // Simulates the real Claude pattern where each tool call is immediately followed by
        // its Replace (adding success tag) and a ToolResult Append before the next call.
        // The run must survive to min_count despite these interleaved ops.
        let mut grouper = ToolGrouper::new(config_min3());

        // First tool + its result before the second tool arrives.
        let b1 = grouper.process(vec![
            tool_op("t1", "read"),
            replace_op("t1", "read", "success"),
            result_op("r1", "t1"),
        ]);
        // The Replace and ToolResult should pass through; run still alive (1 < min_count=3).
        assert!(is_append_id(&b1[0], "t1"));
        assert!(b1.iter().any(|op| is_replace_from(op, "t1")));

        let b2 = grouper.process(vec![
            tool_op("t2", "write"),
            replace_op("t2", "write", "success"),
            result_op("r2", "t2"),
        ]);
        assert!(is_append_id(&b2[0], "t2"));
        // Still collecting — no container yet (2 < 3).
        assert!(!b2.iter().any(|op| matches!(op, TreeOperation::Replace { message, .. } if message.id.starts_with("tool_group"))));

        // Third tool triggers transition.
        let b3 = grouper.process(vec![
            tool_op("t3", "exec"),
            replace_op("t3", "exec", "success"),
            result_op("r3", "t3"),
        ]);
        assert!(
            b3.iter().any(|op| is_remove_id(op, "t2")),
            "Remove(t2) expected in transition batch"
        );
        let replace = b3
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .expect("Replace(t1→Container) missing");
        let cid = container_id_from_replace(replace).unwrap();
        assert_ne!(cid, "t1");
    }

    #[test]
    fn interleaved_replace_and_result_does_not_break_grouped() {
        // After transition, interleaved Replace+ToolResult for each child must not seal.
        let mut grouper = ToolGrouper::new(config_min3());

        let b1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = b1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .expect("container missing");

        // Tool results for t1, t2, t3 arrive — must not seal the container.
        let b2 = grouper.process(vec![
            replace_op("t1", "read", "success"),
            result_op("r1", "t1"),
            replace_op("t2", "write", "success"),
            result_op("r2", "t2"),
            replace_op("t3", "exec", "success"),
            result_op("r3", "t3"),
        ]);
        // None of these should be a container Replace (seal).
        assert!(
            !b2.iter().any(|op| is_replace_from(op, &cid)),
            "container should NOT be sealed by tool result ops"
        );

        // Fourth tool should still extend the grouped run.
        let b3 = grouper.process(vec![tool_op("t4", "list")]);
        assert!(
            b3.iter().any(|op| is_append_to_parent(op, "t4", &cid)),
            "t4 should extend the existing container"
        );
    }

    // A self-contained catch-all config (min_count=3) independent of the default config.
    fn config_catchall_min3() -> ToolGrouperConfig {
        ToolGrouperConfig {
            groups: vec![crate::config::ToolGroup {
                name: "Tool calls".to_string(),
                tools: vec!["*".to_string()],
                min_count: 3,
                expanded: false,
                shorten_as_glob: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn update_does_not_break_active_run() {
        // An Update interleaved during collection must not seal the container.
        let mut grouper = ToolGrouper::new(config_catchall_min3());

        // Two tools — still collecting (min_count=3 not reached).
        grouper.process(vec![tool_op("t1", "read"), tool_op("t2", "write")]);

        // An Update to an earlier node arrives — must not break the run.
        let update_out = grouper.process(vec![TreeOperation::Update {
            id: "t1".to_string(),
            message: MessageState::new("t1")
                .message_type(MessageType::ToolCall)
                .text("read: updated args"),
        }]);
        assert_eq!(update_out.len(), 1, "Update should pass through unchanged");
        assert!(
            matches!(&update_out[0], TreeOperation::Update { id, .. } if id == "t1"),
            "Update op should be forwarded as-is"
        );

        // Third tool — should still trigger grouping (run was not broken).
        let batch = grouper.process(vec![tool_op("t3", "exec")]);
        assert!(
            batch
                .iter()
                .any(|op| matches!(op, TreeOperation::Replace { .. })),
            "third tool should trigger container creation despite interleaved Update"
        );
    }

    #[test]
    fn update_does_not_break_grouped_run() {
        let mut grouper = ToolGrouper::new(config_catchall_min3());
        // Reach Grouped state.
        let b1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = b1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .expect("container missing");

        // Update arrives — must not seal.
        let out = grouper.process(vec![TreeOperation::Update {
            id: "t1".to_string(),
            message: MessageState::new("t1")
                .message_type(MessageType::ToolCall)
                .text("read: updated"),
        }]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], TreeOperation::Update { .. }));

        // Fourth tool — should extend the grouped run, not start fresh.
        let b3 = grouper.process(vec![tool_op("t4", "list")]);
        assert!(
            b3.iter().any(|op| is_append_to_parent(op, "t4", &cid)),
            "t4 should still be appended to the existing container"
        );
    }

    // ── allow_thinking ──────────────────────────────────────────────────────

    fn thinking_op(id: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id).message_type(MessageType::Thinking),
        }
    }

    fn config_catchall_min3_no_thinking() -> ToolGrouperConfig {
        ToolGrouperConfig {
            groups: vec![crate::config::ToolGroup {
                name: "Tool calls".to_string(),
                tools: vec!["*".to_string()],
                min_count: 3,
                expanded: false,
                shorten_as_glob: false,
            }],
            allow_thinking: false,
            ..Default::default()
        }
    }

    #[test]
    fn thinking_alone_does_not_start_run() {
        // A thinking op with an empty buffer passes through and does not start a run.
        let mut grouper = ToolGrouper::new(config_catchall_min3());
        let out = grouper.process(vec![thinking_op("k1"), tool_op("t1", "read")]);
        // k1 passes through; t1 starts a collecting run.
        assert!(is_append_id(&out[0], "k1"));
        assert!(is_append_id(&out[1], "t1"));
        // No grouping yet (buffer len 1 < min_count 3).
        assert!(
            !out.iter()
                .any(|op| matches!(op, TreeOperation::Replace { .. }))
        );
    }

    #[test]
    fn thinking_between_tools_included_in_container() {
        // A thinking that arrives between tool calls is emitted, added to the buffer,
        // and included in the container when min_count is reached.
        let mut grouper = ToolGrouper::new(config_catchall_min3());
        let batch = grouper.process(vec![
            tool_op("t1", "read"),
            thinking_op("k1"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        // t1, k1, t2, t3 all emitted; then Remove(k1)+Remove(t2)+Remove(t3)+Replace(t1→Container).
        assert!(is_append_id(&batch[0], "t1"));
        assert!(is_append_id(&batch[1], "k1"));
        assert!(is_append_id(&batch[2], "t2"));
        assert!(is_append_id(&batch[3], "t3"));
        // Removes for k1, t2, t3 in any order.
        assert!(batch.iter().any(|op| is_remove_id(op, "k1")));
        assert!(batch.iter().any(|op| is_remove_id(op, "t2")));
        assert!(batch.iter().any(|op| is_remove_id(op, "t3")));
        // Replace targets t1 → container.
        let replace = batch
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .expect("Replace(t1→Container) missing");
        let cid = container_id_from_replace(replace).unwrap();
        assert_ne!(cid, "t1");
        // Container label counts only tool calls (3, not 4).
        if let TreeOperation::Replace { message, .. } = replace {
            let text = message.text.as_deref().unwrap_or("");
            assert!(
                text.contains("3 tool calls"),
                "label should count 3 tool calls: {text}"
            );
        }
    }

    #[test]
    fn thinking_does_not_count_toward_min_count() {
        // thinking + tool1 + tool2 — only 2 tool calls; should NOT trigger grouping (min=3).
        let mut grouper = ToolGrouper::new(config_catchall_min3());
        let out = grouper.process(vec![
            tool_op("t1", "read"),
            thinking_op("k1"),
            tool_op("t2", "write"),
        ]);
        // No container created — only 2 tools.
        assert!(
            !out.iter()
                .any(|op| matches!(op, TreeOperation::Replace { .. }))
        );
        // But run is still alive: next tool should trigger it.
        let out2 = grouper.process(vec![tool_op("t3", "exec")]);
        assert!(
            out2.iter().any(|op| is_replace_from(op, "t1")),
            "third tool should trigger container creation"
        );
    }

    #[test]
    fn thinking_in_grouped_held_until_next_tool() {
        // In Grouped state a thinking is emitted immediately but tracked in
        // thinking_buffer.  When the next matching tool arrives the thinking is
        // Remove'd from its original position and re-Append'd inside the container.
        let mut grouper = ToolGrouper::new(config_catchall_min3());
        let b1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = b1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .expect("container missing");

        // Thinking arrives — emitted immediately under the container's parent.
        let b2 = grouper.process(vec![thinking_op("k1")]);
        assert_eq!(b2.len(), 1, "thinking should be emitted immediately");
        assert!(is_append_id(&b2[0], "k1"), "should be an Append for k1");
        assert!(
            !matches!(&b2[0], TreeOperation::Append { parent_id: Some(pid), .. } if pid == &cid),
            "k1 should NOT be appended inside the container yet"
        );

        // Next tool: Remove(k1), Append(k1→cid), Append(t4→cid), Replace(cid).
        let b3 = grouper.process(vec![tool_op("t4", "read")]);
        assert!(
            is_remove_id(&b3[0], "k1"),
            "k1 must be removed from its original position"
        );
        assert!(
            is_append_to_parent(&b3[1], "k1", &cid),
            "k1 should be re-appended inside the container"
        );
        assert!(
            is_append_to_parent(&b3[2], "t4", &cid),
            "t4 should be appended to container"
        );
        assert!(
            is_replace_from(&b3[3], &cid),
            "container Replace should follow"
        );
        // Label still counts only tool calls.
        if let TreeOperation::Replace { message, .. } = &b3[3] {
            let text = message.text.as_deref().unwrap_or("");
            assert!(
                text.contains("4 tool calls"),
                "label should count 4 tool calls: {text}"
            );
        }
    }

    #[test]
    fn thinking_in_grouped_stays_under_parent_on_seal() {
        // When the container seals, thinking that was emitted under the container's
        // parent just stays there — no extra Remove or re-Append is emitted.
        let mut grouper = ToolGrouper::new(config_catchall_min3());
        let b1 = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            tool_op("t3", "exec"),
        ]);
        let cid = b1
            .iter()
            .find(|op| is_replace_from(op, "t1"))
            .and_then(container_id_from_replace)
            .expect("container missing");

        // Thinking arrives while grouped — already emitted.
        let b_think = grouper.process(vec![thinking_op("k1")]);
        assert!(b_think.iter().any(|op| is_append_id(op, "k1")));

        // User message seals the container.
        let b2 = grouper.process(vec![user_op("u1")]);
        // Seal Replace is present.
        assert!(b2.iter().any(|op| is_replace_from(op, &cid)));
        // k1 is NOT touched in the seal batch (it's already in the right place).
        assert!(
            !b2.iter().any(|op| is_append_id(op, "k1")),
            "k1 should not be re-emitted during seal"
        );
        assert!(
            !b2.iter().any(|op| is_remove_id(op, "k1")),
            "k1 should not be removed during seal"
        );
    }

    #[test]
    fn allow_thinking_false_thinking_breaks_run() {
        // With allow_thinking disabled, a Thinking op breaks the collecting run.
        let mut grouper = ToolGrouper::new(config_catchall_min3_no_thinking());
        let out = grouper.process(vec![
            tool_op("t1", "read"),
            tool_op("t2", "write"),
            thinking_op("k1"),
            tool_op("t3", "exec"),
            tool_op("t4", "list"),
            tool_op("t5", "grep"),
        ]);
        // k1 broke the run after t1,t2; no group should form from t1/t2.
        // t3,t4,t5 start a new run (min=3) and should produce a container.
        assert!(!out.iter().any(|op| is_remove_id(op, "t1")));
        assert!(!out.iter().any(|op| is_remove_id(op, "t2")));
        // t3 becomes the container.
        assert!(
            out.iter().any(|op| is_replace_from(op, "t3")),
            "t3,t4,t5 should form a container"
        );
    }

    // ── compress_paths_to_glob ───────────────────────────────────────────────

    fn glob(paths: &[&str]) -> String {
        compress_paths_to_glob(&paths.iter().map(|s| s.to_string()).collect::<Vec<_>>(), 2)
    }

    fn max_brace_depth(s: &str) -> usize {
        let (mut depth, mut max) = (0usize, 0usize);
        for c in s.chars() {
            match c {
                '{' => {
                    depth += 1;
                    max = max.max(depth);
                }
                '}' => {
                    depth -= 1;
                }
                _ => {}
            }
        }
        max
    }

    #[test]
    fn glob_single_path() {
        assert_eq!(glob(&["src/foo.rs"]), "src/foo.rs");
    }

    #[test]
    fn glob_flat_no_common_prefix() {
        assert_eq!(glob(&["bar.rs", "foo.rs"]), "bar.rs, foo.rs");
    }

    #[test]
    fn glob_common_prefix_two_files() {
        assert_eq!(
            glob(&["src/app.rs", "src/main.rs"]),
            "src/{app.rs, main.rs}"
        );
    }

    #[test]
    fn glob_example_from_spec() {
        // BTreeMap sorts "bar" (dir) before "bar.rs" (file), so bar/baz.rs comes first.
        // Outer brace uses ", "; inner braces use ",".
        assert_eq!(
            glob(&["src/app.rs", "src/foo/bar.rs", "src/foo/bar/baz.rs"]),
            "src/{app.rs, foo/{bar/baz.rs,bar.rs}}"
        );
    }

    #[test]
    fn glob_respects_max_depth_2() {
        let paths = &["a/b/c/1.rs", "a/b/c/2.rs", "a/b/d/e/3.rs", "a/b/d/f/4.rs"];
        let result = glob(paths);
        assert!(
            max_brace_depth(&result) <= 2,
            "brace depth exceeds 2: {result}"
        );
        assert_eq!(result, "a/b/{c/{1.rs,2.rs}, d/{e/3.rs,f/4.rs}}");
    }

    #[test]
    fn glob_deduplicates_identical_paths() {
        assert_eq!(glob(&["src/foo.rs", "src/foo.rs"]), "src/foo.rs");
    }

    #[test]
    fn glob_sorts_paths() {
        // Paths given out of order should still produce a sorted glob.
        assert_eq!(glob(&["src/z.rs", "src/a.rs"]), "src/{a.rs, z.rs}");
    }

    #[test]
    fn glob_deep_flatten_at_limit() {
        // A third branching level is flattened into siblings.
        let paths = &["a/b/c/d/1", "a/b/c/d/2", "a/b/c/e/3", "a/b/f/g/4"];
        let result = glob(paths);
        assert!(max_brace_depth(&result) <= 2, "too deep: {result}");
        assert_eq!(result, "a/b/{c/{d/1,d/2,e/3}, f/g/4}");
    }
}
