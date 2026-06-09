use std::collections::{HashMap, HashSet};

use tracing::{debug, info, warn};

/// A parse state to determine the message path. The parser works by a full
/// backward pass followed by a forward pass. The backward pass is used to
/// determine the current message fork to use. The forward pass should apply to
/// all messages at load time, as well as new messages coming in. When the
/// forward() method returns ForwardPathResult::Rewind, the parse state should
/// be reset, followed by a full backward pass followed by a forward pass
/// again.
///
/// The Claude JSONL format represents all messages as a tree. The active tail
/// determines the current message fork. All messages on the path from the root
/// to the active tail should be parsed.
///
/// However, there is a complication with regards to tool results: These
/// messages do not always lie on the path from the root; instead, they may
/// dangle from a message on the active path. This is a quirk due to the
/// implementation of parallel tool calls.
///
/// Another complication: The message IDs are not unique, when compaction
/// boundaries are involved. Hence, during the initial read, messages should be
/// identified by byte offsets rather than message IDs.
pub struct MessagePath {
    /// UUIDs on the active path; excludes tool results which may dangle from
    /// the active path.
    pub active_path: HashSet<String>,

    /// tool_result UUID → its parent UUID; lets can_reach_tail traverse
    /// parallel results.
    pub active_tool_results: HashMap<String, String>,

    /// Tip of the active path (last non-tool-result UUID); new non-tool-result
    /// entries must chain from this UUID directly or via active_tool_results.
    pub active_tail: Option<String>,

    /// Byte offset of the first message read by the backward pass
    pub initial_offset: Option<usize>,

    /// Byte offsets of all messages known to be on the active path during the
    /// backward pass.
    pub initial_active_offsets: HashSet<usize>,

    /// Whether a root node has been emitted during the forward pass
    pub root_emitted: bool,
}

pub struct MessageNode<'a> {
    pub uuid: &'a str,
    pub parent_uuid: Option<&'a str>,
    pub is_tool_result: bool,
    pub byte_offset: usize,
}

#[derive(Debug)]
pub enum ForwardPathResult {
    Ingest,
    Rewind,
    Drop,
}

impl MessagePath {
    pub fn new() -> Self {
        Self {
            active_path: HashSet::new(),
            active_tool_results: HashMap::new(),
            active_tail: None,
            initial_offset: None,
            initial_active_offsets: HashSet::new(),
            root_emitted: false,
        }
    }

    pub fn backward(&mut self, node: &MessageNode) {
        if self.initial_offset.is_none() {
            self.initial_offset = Some(node.byte_offset);
        }

        if self.active_path.contains(node.uuid) {
            self.initial_active_offsets.insert(node.byte_offset);
            if let Some(p) = node.parent_uuid {
                self.active_path.insert(p.to_string());
            }
        } else if self.active_tail.is_none() && !node.is_tool_result {
            self.active_tail = Some(node.uuid.to_string());
            self.active_path.insert(node.uuid.to_string());
            self.initial_active_offsets.insert(node.byte_offset);
            if let Some(p) = node.parent_uuid {
                self.active_path.insert(p.to_string());
            }
        }
    }

    pub fn forward(&mut self, node: &MessageNode) -> ForwardPathResult {
        match node.parent_uuid {
            Some(p) if !p.is_empty() => {
                if node.is_tool_result {
                    if self.is_dangling(p) {
                        // Tool results: Do not add to active_tail yet, since it may be a
                        // parallel tool result that may or may not become a leaf.
                        debug!(
                            node.uuid,
                            parent_uuid = p,
                            "tool_result, adding to active_tool_results"
                        );
                        self.active_path.insert(node.uuid.to_string());
                        self.active_tool_results
                            .insert(node.uuid.to_string(), p.to_string());
                        ForwardPathResult::Ingest
                    } else {
                        ForwardPathResult::Drop
                    }
                } else if self.initial_active_offsets.contains(&node.byte_offset)
                    || self.can_reach_tail(p)
                {
                    // Already known to be on active path from backward pass,
                    // or can reach tail from parent
                    self.active_path.insert(node.uuid.to_string());
                    self.set_active_tail(node.uuid);
                    ForwardPathResult::Ingest
                } else {
                    if let Some(initial_offset) = self.initial_offset
                        && node.byte_offset < initial_offset
                    {
                        // Known orphan branch, drop
                        ForwardPathResult::Drop
                    } else {
                        info!(node.uuid, parent_uuid = p, "rewind detected");
                        ForwardPathResult::Rewind
                    }
                }
            }
            _ => {
                if !self.root_emitted {
                    if self.active_path.is_empty() {
                        self.active_path.insert(node.uuid.to_string());
                        self.active_tail = Some(node.uuid.to_string());
                        self.root_emitted = true;
                        ForwardPathResult::Ingest
                    } else if self.initial_active_offsets.contains(&node.byte_offset) {
                        self.root_emitted = true;
                        ForwardPathResult::Ingest
                    } else {
                        ForwardPathResult::Drop
                    }
                } else {
                    warn!(
                        node.uuid,
                        "unexpected new root entry when chain non-empty, skipping"
                    );
                    ForwardPathResult::Drop
                }
            }
        }
    }

    pub fn reset(&mut self) {
        self.active_path.clear();
        self.active_tool_results.clear();
        self.active_tail = None;
    }

    fn set_active_tail(&mut self, start: &str) {
        self.active_tail = Some(start.to_string());

        // Use a loop so that intermediate tool results that were dangling is incorporated into the
        // active_path too
        let mut cur = start;
        loop {
            if self.active_path.contains(cur) {
                return;
            }
            self.active_path.insert(cur.to_string());
            match self.active_tool_results.get(cur) {
                Some(parent) => cur = parent.as_str(),
                None => return,
            }
        }
    }

    fn is_dangling(&self, start: &str) -> bool {
        let mut cur = start;
        loop {
            if self.active_path.contains(cur) {
                return true;
            }
            match self.active_tool_results.get(cur) {
                Some(parent) => cur = parent.as_str(),
                None => return false,
            }
        }
    }

    fn can_reach_tail(&self, start: &str) -> bool {
        let mut cur = start;
        loop {
            if let Some(ref tail) = self.active_tail
                && cur == tail
            {
                return true;
            }
            match self.active_tool_results.get(cur) {
                Some(parent) => cur = parent.as_str(),
                None => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n<'a>(
        uuid: &'a str,
        parent: Option<&'a str>,
        is_tool_result: bool,
        byte_offset: usize,
    ) -> MessageNode<'a> {
        MessageNode {
            uuid,
            parent_uuid: parent,
            is_tool_result,
            byte_offset,
        }
    }

    /// Run a full backward pass (entries in document order, iterated in reverse)
    /// followed by a forward pass (document order). Returns the resulting path and
    /// the forward result for each entry in document order.
    fn run_passes<'a>(
        path: &mut MessagePath,
        entries: &[MessageNode<'a>],
    ) -> Vec<ForwardPathResult> {
        for node in entries.iter().rev() {
            path.backward(node);
        }
        entries.iter().map(|node| path.forward(node)).collect()
    }

    /// Basic scenario: a simple linear chain.
    ///
    /// Document order: root → msg1 → msg2
    ///
    /// Backward pass sets active_tail=msg2 and traces the whole chain into active_path.
    /// Forward pass: root has no parent but active_path is already populated from the
    /// backward pass, so it is Dropped with a warning. msg1 and msg2 are Ingested because
    /// they are already known to be on the active path via initial_active_offsets.
    #[test]
    fn test_basic_single_chain() {
        let mut path = MessagePath::new();
        let entries = [
            n("root", None, false, 0),
            n("msg1", Some("root"), false, 1),
            n("msg2", Some("msg1"), false, 2),
        ];
        let results = run_passes(&mut path, &entries);

        assert_eq!(path.active_tail.as_deref(), Some("msg2"));
        assert!(path.active_path.contains("root"));
        assert!(path.active_path.contains("msg1"));
        assert!(path.active_path.contains("msg2"));

        assert!(
            matches!(results[0], ForwardPathResult::Ingest),
            "root should be ingested"
        );
        assert!(
            matches!(results[1], ForwardPathResult::Ingest),
            "msg1 should be ingested"
        );
        assert!(
            matches!(results[2], ForwardPathResult::Ingest),
            "msg2 should be ingested"
        );
    }

    /// Rewind scenario: two branches from the same parent.
    ///
    /// Document order: root(0), branch1(1), branch2(2).
    /// The backward pass sees branch2 first (highest offset) and picks it as the active tail;
    /// initial_offset is set to 2. branch1 has offset 1 < initial_offset so it is dropped
    /// in the forward pass as a known-orphan branch without triggering a Rewind.
    #[test]
    fn test_rewind_initial_read_drops_inactive_branch() {
        let mut path = MessagePath::new();
        let entries = [
            n("root", None, false, 0),
            n("branch1", Some("root"), false, 1),
            n("branch2", Some("root"), false, 2), // same parent as branch1 → old branch
        ];
        let results = run_passes(&mut path, &entries);

        assert_eq!(path.active_tail.as_deref(), Some("branch2"));
        assert!(path.active_path.contains("root"));
        assert!(path.active_path.contains("branch2"));
        assert!(!path.active_path.contains("branch1"));

        assert!(
            matches!(results[0], ForwardPathResult::Ingest),
            "root must be ingested"
        );
        assert!(
            matches!(results[1], ForwardPathResult::Drop),
            "branch1 must be dropped (offset < initial_offset)"
        );
        assert!(
            matches!(results[2], ForwardPathResult::Ingest),
            "active branch2 must be ingested"
        );
    }

    /// Live rewind detection: after an initial read of root → branch1, appending
    /// branch2 (parent=root, same level as branch1) must trigger ForwardPathResult::Rewind
    /// because branch2's parent cannot reach the current active tail (branch1) and its
    /// offset (2) is not less than initial_offset (1).
    #[test]
    fn test_live_rewind_detection() {
        let mut path = MessagePath::new();

        // Simulate initial read: root(0) → branch1(1)
        // Backward iterates in reverse: branch1 first, sets initial_offset=1.
        path.backward(&n("branch1", Some("root"), false, 1));
        path.backward(&n("root", None, false, 0));
        path.forward(&n("root", None, false, 0)); // Drop
        let r = path.forward(&n("branch1", Some("root"), false, 1));
        assert!(matches!(r, ForwardPathResult::Ingest));
        assert_eq!(path.active_tail.as_deref(), Some("branch1"));

        // Live: branch2 (offset 2) shares the same parent as branch1.
        // can_reach_tail("root") = false, and 2 >= initial_offset(1) → Rewind.
        let r = path.forward(&n("branch2", Some("root"), false, 2));
        assert!(
            matches!(r, ForwardPathResult::Rewind),
            "branch at the same depth as the active tail must trigger Rewind"
        );
    }

    /// Parallel tool call scenario.
    ///
    /// Claude Code writes parallel tool calls as:
    ///   asst-a (tool_use tu-1) → asst-b (tool_use tu-2, parent=asst-a)
    ///   result-1 (tool_result for tu-1, parent=asst-a — NOT the chain tail asst-b)
    ///   result-2 (tool_result for tu-2, parent=asst-b)
    ///   asst-final (parent=result-2)
    ///
    /// result-1's parent (asst-a) is on the active path so is_dangling returns true.
    /// Both tool results must be ingested; the final assistant message must chain through.
    #[test]
    fn test_parallel_tool_calls_full_sequence() {
        let mut path = MessagePath::new();
        let entries = [
            n("root", None, false, 0),
            n("asst-a", Some("root"), false, 1),
            n("asst-b", Some("asst-a"), false, 2),
            n("result-1", Some("asst-a"), true, 3), // parallel: parent is asst-a, not the tail
            n("result-2", Some("asst-b"), true, 4), // chain: parent is the tail asst-b
            n("asst-final", Some("result-2"), false, 5),
        ];
        let results = run_passes(&mut path, &entries);

        assert_eq!(path.active_tail.as_deref(), Some("asst-final"));

        assert!(
            matches!(results[0], ForwardPathResult::Ingest),
            "root ingested"
        );
        assert!(
            matches!(results[1], ForwardPathResult::Ingest),
            "asst-a ingested"
        );
        assert!(
            matches!(results[2], ForwardPathResult::Ingest),
            "asst-b ingested"
        );
        assert!(
            matches!(results[3], ForwardPathResult::Ingest),
            "parallel result-1 (parent=asst-a, not tail) must be ingested, not dropped or rewound"
        );
        assert!(
            matches!(results[4], ForwardPathResult::Ingest),
            "result-2 ingested"
        );
        assert!(
            matches!(results[5], ForwardPathResult::Ingest),
            "asst-final ingested"
        );

        assert!(
            path.active_tool_results.contains_key("result-1"),
            "result-1 tracked in active_tool_results"
        );
        assert!(
            path.active_tool_results.contains_key("result-2"),
            "result-2 tracked in active_tool_results"
        );
    }

    /// Mid-parallel state: the initial read captures the file while result-1 (parallel,
    /// parent=asst-a) is the last entry — result-2 (parent=asst-b) has not yet been written.
    ///
    /// The backward pass traces from result-1 through asst-a, but asst-b is NOT a parent
    /// of result-1, so it must be picked up as the active tail (last non-tool-result).
    /// asst-b must remain on the active path so that when result-2 arrives live it is
    /// correctly ingested instead of being silently dropped as an orphan.
    #[test]
    fn test_mid_parallel_initial_then_live_continuation() {
        let mut path = MessagePath::new();

        // Initial read: result-2 intentionally absent to reproduce the mid-parallel snapshot.
        let initial = [
            n("root", None, false, 0),
            n("asst-a", Some("root"), false, 1),
            n("asst-b", Some("asst-a"), false, 2),
            n("result-1", Some("asst-a"), true, 3), // parallel, last entry; sets initial_offset=3
        ];
        run_passes(&mut path, &initial);

        // asst-b must be the active tail and on the active path even though result-1
        // (whose parent is asst-a) was the last document entry.
        assert_eq!(path.active_tail.as_deref(), Some("asst-b"));
        assert!(
            path.active_path.contains("asst-b"),
            "asst-b must be on active_path for live continuation to work; got: {:?}",
            path.active_path
        );

        // Live: result-2 arrives (parent=asst-b, tool_result).
        // is_dangling(asst-b) → active_path.contains(asst-b) = true → Ingest.
        let r = path.forward(&n("result-2", Some("asst-b"), true, 4));
        assert!(
            matches!(r, ForwardPathResult::Ingest),
            "result-2 must be ingested (asst-b is on active_path)"
        );

        // Live: asst-final arrives (parent=result-2, non-tool-result).
        // can_reach_tail(result-2): result-2 → asst-b (via active_tool_results) == active_tail → true.
        let r = path.forward(&n("asst-final", Some("result-2"), false, 5));
        assert!(
            matches!(r, ForwardPathResult::Ingest),
            "asst-final must be ingested after parallel tool results resolve"
        );
        assert_eq!(path.active_tail.as_deref(), Some("asst-final"));
    }
}
