use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::bail;
use notify::Watcher as _;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::message::{ParseState, parse_entry, parse_entry_cb};
use crate::providers::TranscriptReader;
use crate::providers::claude::path::{ForwardPathResult, MessageNode, MessagePath};
use crate::providers::claude::subagent::ClaudeSubagentManager;
use crate::reader_op::ReaderOp;
use crate::tree_operation::TreeOperation;

pub(super) struct ClaudeTranscriptReader {
    rx: mpsc::Receiver<color_eyre::Result<ReaderOp>>,
}

impl ClaudeTranscriptReader {
    pub(super) fn new(rx: mpsc::Receiver<color_eyre::Result<ReaderOp>>) -> Self {
        Self { rx }
    }
}

impl TranscriptReader for ClaudeTranscriptReader {
    fn updates(&mut self) -> &mut mpsc::Receiver<color_eyre::Result<ReaderOp>> {
        &mut self.rx
    }
}

struct ClaudeReader {
    jsonl_path: PathBuf,
    /// Parse state for the main JSONL. This is reset when we re-parse all entries during rewind
    /// handling.
    state: ParseState,

    /// Message path state; used for rewind detection and path tracking.
    message_path: MessagePath,

    /// How many bytes of the main JSONL have been processed.
    byte_offset: usize,

    subagent_manager: ClaudeSubagentManager,

    watcher_rx: mpsc::UnboundedReceiver<()>,
    /// Kept alive so the watcher thread continues sending to watcher_rx.
    _watcher: notify::RecommendedWatcher,
    sa_rx: mpsc::UnboundedReceiver<PathBuf>,
    sa_watcher: Option<notify::RecommendedWatcher>,
    /// The directory currently watched by sa_watcher. Starts as session_dir if it exists at
    /// startup, otherwise jsonl_dir; upgraded to session_dir on first matching event.
    sa_watched_dir: PathBuf,
    /// Ops from the initial JSONL read; drained at the start of run().
    initial_ops: Vec<TreeOperation>,

    /// When true, simulate live streaming one entry at a time.
    waterfall: bool,
    /// In waterfall/initial_loaded mode, the max number of UUID-bearing JSONL entries to process.
    /// `usize::MAX` means no limit (process all).
    waterfall_message_limit: usize,
}

impl ClaudeReader {
    /// Set up watchers, wait for the JSONL file to exist, perform the initial
    /// read, and pre-populate sub-agent tracking from the initial parse state.
    async fn init(
        jsonl_path: PathBuf,
        waterfall: bool,
        initial_loaded: usize,
    ) -> color_eyre::Result<Self> {
        let session_dir = jsonl_path.with_extension("");

        let (watcher_tx, mut watcher_rx) = mpsc::unbounded_channel::<()>();
        let mut watcher = notify::RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if let Ok(evt) = res
                    && matches!(
                        evt.kind,
                        notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                    )
                {
                    let _ = watcher_tx.send(());
                }
            },
            notify::Config::default(),
        )?;

        // Watch the file directly, or an ancestor if it doesn't exist yet.
        let jsonl_dir = jsonl_path.parent().expect("jsonl_path must have parent");
        let ancestor_watch: Option<PathBuf> = if jsonl_path.exists() {
            watcher.watch(&jsonl_path, notify::RecursiveMode::NonRecursive)?;
            info!(path = %jsonl_path.display(), "watching jsonl file for changes");
            None
        } else if jsonl_dir.exists() {
            watcher.watch(jsonl_dir, notify::RecursiveMode::NonRecursive)?;
            info!(dir = %jsonl_dir.display(), "file not found, watching parent dir");
            Some(jsonl_dir.to_owned())
        } else {
            let mut ancestor = jsonl_dir;
            loop {
                ancestor = ancestor.parent().expect("must have an existing ancestor");
                if ancestor.exists() {
                    break;
                }
            }
            watcher.watch(ancestor, notify::RecursiveMode::Recursive)?;
            info!(dir = %ancestor.display(), "watching ancestor for file creation");
            Some(ancestor.to_owned())
        };

        // Wait until the file actually exists.
        while !jsonl_path.exists() {
            debug!("waiting for jsonl file to be created");
            match watcher_rx.recv().await {
                Some(()) => {}
                None => color_eyre::eyre::bail!("watcher channel closed before jsonl was created"),
            }
        }

        if let Some(ancestor) = ancestor_watch {
            let _ = watcher.unwatch(&ancestor);
            watcher.watch(&jsonl_path, notify::RecursiveMode::NonRecursive)?;
            info!(path = %jsonl_path.display(), "switched to watching jsonl file");
        }

        // Set up sub-agent file watcher before creating ClaudeReader.
        // If session_dir doesn't exist yet, watch jsonl_dir so we don't miss events written
        // before session_dir is created; the watch is upgraded to session_dir on first match.
        let (sa_tx, sa_rx) = mpsc::unbounded_channel::<PathBuf>();
        let sa_watched_dir = if session_dir.exists() {
            session_dir.clone()
        } else {
            jsonl_dir.to_owned()
        };
        let sa_cb_tx = sa_tx.clone();
        let sa_watcher: Option<notify::RecommendedWatcher> = {
            let mut sw = notify::RecommendedWatcher::new(
                move |res: notify::Result<notify::Event>| {
                    if let Ok(evt) = res
                        && matches!(
                            evt.kind,
                            notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                        )
                    {
                        for path in evt.paths {
                            let _ = sa_cb_tx.send(path);
                        }
                    }
                },
                notify::Config::default(),
            )
            .ok();
            if let Some(ref mut sw_ref) = sw
                && sw_ref
                    .watch(&sa_watched_dir, notify::RecursiveMode::Recursive)
                    .is_ok()
            {
                info!(dir = %sa_watched_dir.display(), "watching for sub-agent files");
            }
            sw
        };
        drop(sa_tx);

        let waterfall_message_limit = if waterfall {
            1
        } else if initial_loaded > 0 {
            initial_loaded
        } else {
            usize::MAX
        };

        // Create ClaudeReader and perform initial read
        let mut reader = Self {
            jsonl_path: jsonl_path.clone(),
            state: ParseState::default(),
            message_path: MessagePath::new(),
            byte_offset: 0,
            subagent_manager: ClaudeSubagentManager::new(session_dir),
            watcher_rx,
            _watcher: watcher,
            sa_rx,
            sa_watcher,
            sa_watched_dir,
            initial_ops: Vec::new(),
            waterfall,
            waterfall_message_limit,
        };

        let initial_ops = reader.do_initial_read()?;
        info!(
            ops = initial_ops.len(),
            bytes = reader.byte_offset,
            "initial read complete"
        );

        reader.initial_ops = initial_ops;

        Ok(reader)
    }

    /// Read the JSONL file, parse entries, and build the message path and initial operations.
    /// Respects `self.waterfall_message_limit`: only the first N UUID-bearing entries are
    /// processed; `self.byte_offset` is set to the byte position after the N-th entry so that
    /// `handle_main_event` naturally picks up from there.
    /// Updates self.state, self.message_path, and self.byte_offset directly.
    fn do_initial_read(&mut self) -> color_eyre::Result<Vec<TreeOperation>> {
        let file = File::open(&self.jsonl_path)?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();

        // Collect UUID-bearing entries with the byte offset after each entry's newline.
        // Lines without a trailing '\n' are incomplete and are discarded, consistent with
        // handle_main_event which also stops at the final incomplete line.
        // Stop early as soon as waterfall_message_limit entries have been collected.
        let mut entries: Vec<(String, usize, serde_json::Value)> = Vec::new();
        let limit_cap = self.waterfall_message_limit; // usize::MAX means no cap
        // `bytes_consumed` tracks the byte position after the last '\n' seen so far.
        // When the limit is hit we break mid-loop, leaving bytes_consumed pointing
        // past the last processed entry's newline — exactly where handle_main_event
        // should resume.  When the loop exhausts normally, bytes_consumed sits after
        // the last complete line (any trailing incomplete line is excluded).
        let mut bytes_consumed = 0usize;
        loop {
            line.clear();
            let line_byte_len = reader.read_line(&mut line)?;

            if !(line.ends_with('\r') || line.ends_with('\n')) {
                break;
            }

            let byte_offset = bytes_consumed;
            bytes_consumed += line_byte_len;

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            let Some(uuid) = obj["uuid"].as_str() else {
                continue;
            };
            entries.push((uuid.to_string(), byte_offset, obj));
            if entries.len() >= limit_cap {
                break; // reached the limit; remaining entries are future steps
            }
        }
        // Any bytes after the last '\n' are an incomplete line — discarded.

        // Build MessagePath through backward then forward pass.
        let mut message_path = MessagePath::new();

        // Backward pass: process entries in reverse order to establish the active tail and path.
        for (_, byte_offset, obj) in entries.iter().rev() {
            let Some(node) = value_to_message_node(obj, *byte_offset) else {
                continue;
            };
            message_path.backward(&node);
        }

        // Forward pass: process entries in document order.
        let mut ops = Vec::new();
        for (_, byte_offset, obj) in entries.iter() {
            let Some(node) = value_to_message_node(obj, *byte_offset) else {
                continue;
            };
            match message_path.forward(&node) {
                ForwardPathResult::Ingest => {
                    ops.extend(parse_entry_cb(
                        &serde_json::to_string(&obj).unwrap_or_default(),
                        &mut self.state,
                        |tu_id, value, is_result| {
                            if is_result {
                                self.subagent_manager.on_subagent_tool_result(tu_id, value)
                            } else {
                                self.subagent_manager.on_subagent_tool_use(tu_id, value)
                            }
                        },
                    )?);
                }
                ForwardPathResult::Rewind => {
                    bail!(
                        "unexpected rewind during initial read forward pass, node: {}",
                        node.uuid
                    );
                }
                ForwardPathResult::Drop => {}
            }
        }

        self.message_path = message_path;
        self.byte_offset = bytes_consumed;

        ops.extend(self.subagent_manager.on_init()?);

        Ok(ops)
    }

    /// Read new bytes from the main JSONL, parse them, and handle rewinding.
    /// In waterfall mode, processes exactly one UUID-bearing entry per call.
    /// Returns ops to emit; an empty vec means nothing new or a recoverable error.
    fn handle_main_event(&mut self) -> color_eyre::Result<Vec<ReaderOp>> {
        let file = File::open(&self.jsonl_path)?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();

        reader.seek(SeekFrom::Start(self.byte_offset as u64))?;

        let mut new_ops: Vec<ReaderOp> = Vec::new();
        let mut cur_offset = self.byte_offset;
        let mut rewind_detected = false;
        let mut rewind_id: Option<String> = None;

        loop {
            line.clear();
            let line_byte_len = reader.read_line(&mut line)?;

            if !(line.ends_with('\r') || line.ends_with('\n')) {
                debug!("incomplete last line, stopping here");
                break;
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                cur_offset += line_byte_len;
                continue;
            }

            let obj: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => {
                    debug!("json parse error on new line, stopping");
                    break;
                }
            };

            if let Some(node) = value_to_message_node(&obj, cur_offset) {
                let should_ingest = match self.message_path.forward(&node) {
                    ForwardPathResult::Ingest => true,
                    ForwardPathResult::Rewind => {
                        info!(
                            uuid = node.uuid,
                            parent_uuid = node.parent_uuid.unwrap_or(""),
                            "rewind detected"
                        );
                        rewind_id = Some(node.uuid.to_string());
                        rewind_detected = true;
                        cur_offset += line_byte_len;
                        break;
                    }
                    ForwardPathResult::Drop => false,
                };

                if should_ingest {
                    new_ops.extend(
                        parse_entry_cb(line.trim(), &mut self.state, |tu_id, value, is_result| {
                            if is_result {
                                self.subagent_manager.on_subagent_tool_result(tu_id, value)
                            } else {
                                self.subagent_manager.on_subagent_tool_use(tu_id, value)
                            }
                        })?
                        .into_iter()
                        .map(ReaderOp::Tree),
                    );
                }
            } else {
                new_ops.extend(
                    parse_entry(line.trim(), &mut self.state)?
                        .into_iter()
                        .map(ReaderOp::Tree),
                );
            }

            cur_offset += line_byte_len;

            // In waterfall mode, process exactly one entry per call.
            if self.waterfall {
                break;
            }
        }

        if rewind_detected {
            new_ops.clear();
            if self.waterfall {
                self.waterfall_message_limit = self.waterfall_message_limit.saturating_add(1);
            }
            new_ops.push(ReaderOp::Reset { id: rewind_id });
            self.state = ParseState::default();
            self.message_path.reset();
            // Reset sub-agent tracking too — the rewind may change which tool_uses exist.
            self.subagent_manager.clear();
            let result = self.do_initial_read();
            match result {
                Ok(ops) => {
                    let replay_count = ops.len();
                    new_ops.extend(ops.into_iter().map(ReaderOp::Tree));
                    new_ops.push(ReaderOp::ResetDone);
                    info!(ops = replay_count, "re-read after rewind complete");
                }
                Err(e) => warn!("re-read after rewind error: {}", e),
            }
        } else {
            if self.waterfall && cur_offset > self.byte_offset {
                self.waterfall_message_limit = self.waterfall_message_limit.saturating_add(1);
            }
            self.byte_offset = cur_offset;
        }

        Ok(new_ops)
    }

    /// Emit the initial ops, then drive the main event loop.
    /// Sends `Err(e)` on the channel before returning if a terminal error occurs.
    async fn run(mut self, tx: mpsc::Sender<color_eyre::Result<ReaderOp>>, snapshot: bool) {
        info!(path = %self.jsonl_path.display(), "reader started");
        if let Err(e) = self.run_inner(&tx, snapshot).await {
            let _ = tx.send(Err(e)).await;
        }
    }

    async fn run_inner(
        &mut self,
        tx: &mpsc::Sender<color_eyre::Result<ReaderOp>>,
        snapshot: bool,
    ) -> color_eyre::Result<()> {
        for op in std::mem::take(&mut self.initial_ops) {
            if tx.send(Ok(ReaderOp::Tree(op))).await.is_err() {
                return Ok(());
            }
        }

        if snapshot && self.waterfall {
            // Waterfall + snapshot: drive handle_main_event until all entries are exhausted.
            // Break when byte_offset stops advancing (no new bytes or unrecoverable parse error),
            // not when ops is empty — some entries (e.g. system/local_command) produce no ops.
            loop {
                let prev_offset = self.byte_offset;
                let ops = self.handle_main_event()?;
                for op in ops {
                    if tx.send(Ok(op)).await.is_err() {
                        return Ok(());
                    }
                }
                if self.byte_offset == prev_offset {
                    break;
                }
            }
            return Ok(());
        }

        if snapshot {
            // Run one iteration of handle_main_event to catch bytes written after initial read.
            let ops = self.handle_main_event()?;
            for op in ops {
                if tx.send(Ok(op)).await.is_err() {
                    return Ok(());
                }
            }
            return Ok(());
        }

        loop {
            tokio::select! {
                // ── Main JSONL file events ──────────────────────────────────────
                msg = self.watcher_rx.recv() => {
                    let Some(()) = msg else {
                        color_eyre::eyre::bail!("watcher channel closed unexpectedly");
                    };
                    debug!("fs event received");

                    // Debounce: drain queued events for 50 ms.
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => break,
                            Some(()) = self.watcher_rx.recv() => {}
                        }
                    }
                    debug!("debounce complete");

                    let ops = self.handle_main_event()?;
                    debug!(ops = ops.len(), "sending incremental ops");
                    for op in ops {
                        if tx.send(Ok(op)).await.is_err() {
                            return Ok(());
                        }
                    }
                }

                // ── Sub-agent file events ───────────────────────────────────────
                Some(path) = self.sa_rx.recv() => {
                    // If we're watching jsonl_dir (because session_dir didn't exist at startup)
                    // and an event under session_dir arrives, upgrade to watching session_dir.
                    let session_dir = self.jsonl_path.with_extension("");
                    if self.sa_watched_dir != session_dir && path.starts_with(&session_dir)
                        && let Some(ref mut watcher) = self.sa_watcher
                    {
                        // Must unwatch first; seems unwatch would apply to
                        // descendant watches too
                        let _ = watcher.unwatch(&self.sa_watched_dir);
                        if watcher.watch(&session_dir, notify::RecursiveMode::Recursive).is_ok() {
                            info!(dir = %session_dir.display(), "upgraded SA watcher to session_dir");
                            self.sa_watched_dir = session_dir;
                        }
                    }
                    for op in self.subagent_manager.on_subagent_event(path)? {
                        if tx.send(Ok(ReaderOp::Tree(op))).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

fn value_to_message_node(obj: &serde_json::Value, byte_offset: usize) -> Option<MessageNode<'_>> {
    let uuid = obj["uuid"].as_str()?;

    let mut parent_uuid = obj["parentUuid"].as_str();
    if parent_uuid.is_none() {
        parent_uuid = obj["logicalParentUuid"].as_str();
    }

    let is_tool_result = obj["message"]["content"]
        .as_array()
        .is_some_and(|a| a.iter().any(|b| b["type"].as_str() == Some("tool_result")));

    Some(MessageNode {
        uuid,
        parent_uuid,
        is_tool_result,
        byte_offset,
    })
}

pub(super) async fn claude_reader_task(
    jsonl_path: PathBuf,
    tx: mpsc::Sender<color_eyre::Result<ReaderOp>>,
    snapshot: bool,
    waterfall: bool,
    initial_loaded: usize,
) {
    match ClaudeReader::init(jsonl_path, waterfall, initial_loaded).await {
        Ok(reader) => reader.run(tx, snapshot).await,
        Err(e) => {
            let _ = tx.send(Err(e)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;

    /// Integration test: writing a rewind entry to a live-watched JSONL must emit a Reset op.
    #[tokio::test]
    async fn test_rewind_live_watch_emits_reset() {
        use std::io::Write as _;

        let temp_dir = std::env::temp_dir().join(format!(
            "agt_test_rewind_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let jsonl_path = temp_dir.join("session.jsonl");

        // Initial content: root → branch1
        {
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "root",
                    "parentUuid": null,
                    "message": {"role": "user", "content": "hello"}
                })
            )
            .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "branch1",
                    "parentUuid": "root",
                    "message": {"role": "user", "content": "original message"}
                })
            )
            .unwrap();
        }

        let (tx, mut rx) = mpsc::channel(256);
        let path_clone = jsonl_path.clone();
        tokio::spawn(async move {
            let _ = claude_reader_task(path_clone, tx, false, false, 0).await;
        });

        // Allow the reader task to complete its initial read.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut initial_ops = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            initial_ops.push(op);
        }
        assert!(
            !initial_ops.is_empty(),
            "should produce ops from initial read"
        );
        assert!(
            !initial_ops
                .iter()
                .any(|op| matches!(op, ReaderOp::Reset { .. })),
            "no Reset expected in initial read"
        );

        // Append a rewind line: branch2 has the same parent as branch1 (root), not branch1 itself.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "branch2",
                    "parentUuid": "root",
                    "message": {"role": "user", "content": "message after rewind"}
                })
            )
            .unwrap();
        }

        // Allow the live-watch debounce + processing to complete.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut post_ops = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            post_ops.push(op);
        }

        let op_names: Vec<&str> = post_ops
            .iter()
            .map(|op| match op {
                ReaderOp::Reset { .. } => "Reset",
                ReaderOp::ResetDone => "ResetDone",
                ReaderOp::Tree(TreeOperation::Append { .. }) => "Append",
                ReaderOp::Tree(TreeOperation::Replace { .. }) => "Replace",
                ReaderOp::Tree(TreeOperation::Remove { .. }) => "Remove",
                ReaderOp::Tree(TreeOperation::Update { .. }) => "Update",
            })
            .collect();

        assert!(
            post_ops
                .iter()
                .any(|op| matches!(op, ReaderOp::Reset { .. })),
            "rewind should produce a Reset op; got: {:?}",
            op_names
        );

        // After Reset there should be re-emitted ops for the active (post-rewind) path.
        let reset_pos = post_ops
            .iter()
            .position(|op| matches!(op, ReaderOp::Reset { .. }))
            .unwrap();
        assert!(
            post_ops.len() > reset_pos + 1,
            "ops after Reset should include re-emitted content; got: {:?}",
            op_names
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Parallel tool calls: Claude Code writes assistant_A (tool_use_1) → assistant_B
    /// (tool_use_2, parent=A), then tool_result_1 arrives with parent=A (before result_2).
    /// This must NOT trigger a rewind; the stream must continue normally after result_2.
    #[tokio::test]
    async fn test_parallel_tool_results_no_false_rewind() {
        use std::io::Write as _;

        let temp_dir = std::env::temp_dir().join(format!(
            "agt_test_parallel_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let jsonl_path = temp_dir.join("session.jsonl");

        // Initial content: root → assistant with first tool_use.
        {
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "root",
                    "parentUuid": null,
                    "message": {"role": "user", "content": "hello"}
                })
            )
            .unwrap();
            // assistant_A: tool_use_1
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "asst-a",
                    "parentUuid": "root",
                    "message": {
                        "id": "msg_asst_a",
                        "role": "assistant",
                        "content": [{"type": "tool_use", "id": "tu-1", "name": "Read", "input": {}}]
                    }
                })
            )
            .unwrap();
            // assistant_B: tool_use_2 (parent=asst-a)
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "asst-b",
                    "parentUuid": "asst-a",
                    "message": {
                        "id": "msg_asst_b",
                        "role": "assistant",
                        "content": [{"type": "tool_use", "id": "tu-2", "name": "Read", "input": {}}]
                    }
                })
            )
            .unwrap();
        }

        let (tx, mut rx) = mpsc::channel(256);
        let path_clone = jsonl_path.clone();
        tokio::spawn(async move {
            let _ = claude_reader_task(path_clone, tx, false, false, 0).await;
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut initial_ops = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            initial_ops.push(op);
        }
        assert!(
            !initial_ops.is_empty(),
            "should produce ops from initial read"
        );
        assert!(
            !initial_ops
                .iter()
                .any(|op| matches!(op, ReaderOp::Reset { .. })),
            "no Reset expected in initial read"
        );

        // Append tool_result_1 (parent=asst-a, i.e. NOT the chain tail asst-b).
        // This should NOT trigger a rewind.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "result-1",
                    "parentUuid": "asst-a",
                    "message": {
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "file A"}]
                    }
                })
            )
            .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut after_result1 = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            after_result1.push(op);
        }
        assert!(
            !after_result1
                .iter()
                .any(|op| matches!(op, ReaderOp::Reset { .. })),
            "parallel tool_result_1 (parent=asst-a, not tail) must NOT emit Reset; got: {:?}",
            after_result1
                .iter()
                .map(|op| match op {
                    ReaderOp::Reset { .. } => "Reset",
                    ReaderOp::Tree(TreeOperation::Append { .. }) => "Append",
                    ReaderOp::Tree(TreeOperation::Replace { .. }) => "Replace",
                    _ => "Other",
                })
                .collect::<Vec<_>>()
        );

        // Now append tool_result_2 (parent=asst-b) and an assistant continuation.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "uuid": "result-2",
                    "parentUuid": "asst-b",
                    "message": {
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": "tu-2", "content": "file B"}]
                    }
                })
            )
            .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "asst-final",
                    "parentUuid": "result-2",
                    "message": {
                        "id": "msg_asst_final",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "done"}]
                    }
                })
            )
            .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut after_result2 = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            after_result2.push(op);
        }
        assert!(
            !after_result2
                .iter()
                .any(|op| matches!(op, ReaderOp::Reset { .. })),
            "no Reset after result_2 or final assistant"
        );
        // The final assistant message should be streamed.
        // Node ID is "text:{message_id}:{idx}" where message_id = msg["id"].
        assert!(
            after_result2.iter().any(|op| match op {
                ReaderOp::Tree(TreeOperation::Append { message, .. }) => {
                    message.id.contains("msg_asst_final")
                }
                _ => false,
            }),
            "final assistant node should be streamed after parallel results; got: {:?}",
            after_result2
                .iter()
                .map(|op| match op {
                    ReaderOp::Reset { .. } => "Reset".to_string(),
                    ReaderOp::Tree(TreeOperation::Append { message, .. }) => {
                        format!("Append({})", message.id)
                    }
                    ReaderOp::Tree(TreeOperation::Replace { id, .. }) => {
                        format!("Replace({})", id)
                    }
                    _ => "Other".to_string(),
                })
                .collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Initial read: parallel tool_result (parent=asst-a, not on active_path) must still be
    /// emitted so its tool_call node gets resolved and the result appears in the UI.
    #[test]
    fn test_initial_read_includes_parallel_tool_results() {
        use std::io::Write as _;

        let temp_dir = std::env::temp_dir().join(format!(
            "agt_test_initial_parallel_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let jsonl_path = temp_dir.join("session.jsonl");

        {
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user", "uuid": "root", "parentUuid": null,
                    "message": {"role": "user", "content": "hello"}
                })
            )
            .unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "assistant", "uuid": "asst-a", "parentUuid": "root",
                "message": {"id": "msg_asst_a", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu-1", "name": "Read", "input": {}}]}
            })).unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "assistant", "uuid": "asst-b", "parentUuid": "asst-a",
                "message": {"id": "msg_asst_b", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu-2", "name": "Read", "input": {}}]}
            })).unwrap();
            // parallel: parent=asst-a (not the chain tail asst-b)
            writeln!(f, "{}", serde_json::json!({
                "type": "user", "uuid": "result-1", "parentUuid": "asst-a",
                "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "file A"}]}
            })).unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "user", "uuid": "result-2", "parentUuid": "asst-b",
                "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tu-2", "content": "file B"}]}
            })).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "assistant", "uuid": "asst-final", "parentUuid": "result-2",
                    "message": {"id": "msg_asst_final", "role": "assistant",
                        "content": [{"type": "text", "text": "done"}]}
                })
            )
            .unwrap();
        }

        let session_dir = temp_dir.clone();
        let (sa_tx, sa_rx) = mpsc::unbounded_channel();
        drop(sa_tx);

        let (watcher_tx, watcher_rx) = mpsc::unbounded_channel();
        let watcher = notify::RecommendedWatcher::new(
            move |_: notify::Result<notify::Event>| {},
            notify::Config::default(),
        )
        .unwrap();
        drop(watcher_tx);

        let mut reader = ClaudeReader {
            jsonl_path: jsonl_path.clone(),
            state: ParseState::default(),
            message_path: MessagePath::new(),
            byte_offset: 0,
            subagent_manager: ClaudeSubagentManager::new(session_dir.clone()),
            watcher_rx,
            _watcher: watcher,
            sa_rx,
            sa_watcher: None,
            sa_watched_dir: session_dir,
            initial_ops: Vec::new(),
            waterfall: false,
            waterfall_message_limit: usize::MAX,
        };

        let ops = reader.do_initial_read().unwrap();

        let op_ids: Vec<String> = ops
            .iter()
            .map(|op| match op {
                TreeOperation::Append { message, .. } => format!("Append({})", message.id),
                TreeOperation::Replace { id, .. } => format!("Replace({})", id),
                TreeOperation::Remove { id } => format!("Remove({})", id),
                TreeOperation::Update { id, .. } => format!("Update({})", id),
            })
            .collect();

        assert!(
            ops.iter()
                .any(|op| matches!(op, TreeOperation::Append { message, .. } if message.id == "tool_result:tu-1")),
            "parallel tool_result for tu-1 must be emitted in initial read; ops: {op_ids:?}"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, TreeOperation::Append { message, .. } if message.id == "tool_result:tu-2")),
            "on-chain tool_result for tu-2 must be emitted; ops: {op_ids:?}"
        );
        assert!(
            ops.iter().any(|op| matches!(op, TreeOperation::Append { message, .. } if message.id.contains("msg_asst_final"))),
            "final assistant node must be emitted; ops: {op_ids:?}"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// When `do_initial_read` runs while the file contains entries up to `result-1`
    /// (a parallel tool_result whose parent is `asst-a`) but NOT yet `result-2`
    /// (whose parent is `asst-b`), `build_active_chain` traces backward from
    /// `result-1` through `asst-a`, skipping `asst-b` entirely.
    ///
    /// The returned `active_chain` must still contain `asst-b` so that when
    /// `result-2 (parent=asst-b)` arrives in the live stream it is recognised as
    /// a normal continuation rather than dropped as an "orphaned sidechain".
    ///
    /// Additionally, `tu-2` (the tool_use from `asst-b`) must be in
    /// `pending_tool_calls` so the result can actually be resolved.
    #[test]
    fn test_mid_parallel_initial_read_active_chain_includes_sibling_assistant() {
        use std::io::Write as _;

        let temp_dir = std::env::temp_dir().join(format!(
            "agt_test_mid_parallel_chain_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let jsonl_path = temp_dir.join("session.jsonl");

        // Write only the first three entries of the parallel pattern plus result-1.
        // result-2 (parent=asst-b) has NOT been written yet — this is the snapshot
        // that causes the stall.
        {
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user", "uuid": "root", "parentUuid": null,
                    "message": {"role": "user", "content": "hello"}
                })
            )
            .unwrap();
            // asst-a: issues tool_use tu-1
            writeln!(f, "{}", serde_json::json!({
                "type": "assistant", "uuid": "asst-a", "parentUuid": "root",
                "message": {"id": "msg_asst_a", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu-1", "name": "Read", "input": {}}]}
            })).unwrap();
            // asst-b: issues tool_use tu-2, chains off asst-a (Claude Code's parallel pattern)
            writeln!(f, "{}", serde_json::json!({
                "type": "assistant", "uuid": "asst-b", "parentUuid": "asst-a",
                "message": {"id": "msg_asst_b", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu-2", "name": "Read", "input": {}}]}
            })).unwrap();
            // result-1: parallel result for tu-1, parent=asst-a (NOT asst-b).
            // This is the LAST entry — result-2 has not been written yet.
            writeln!(f, "{}", serde_json::json!({
                "type": "user", "uuid": "result-1", "parentUuid": "asst-a",
                "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "file A"}]}
            })).unwrap();
        }

        let session_dir = temp_dir.clone();
        let (sa_tx, sa_rx) = mpsc::unbounded_channel();
        drop(sa_tx);

        let (watcher_tx, watcher_rx) = mpsc::unbounded_channel();
        let watcher = notify::RecommendedWatcher::new(
            move |_: notify::Result<notify::Event>| {},
            notify::Config::default(),
        )
        .unwrap();
        drop(watcher_tx);

        let mut reader = ClaudeReader {
            jsonl_path: jsonl_path.clone(),
            state: ParseState::default(),
            message_path: MessagePath::new(),
            byte_offset: 0,
            subagent_manager: ClaudeSubagentManager::new(session_dir.clone()),
            watcher_rx,
            _watcher: watcher,
            sa_rx,
            sa_watcher: None,
            sa_watched_dir: session_dir,
            initial_ops: Vec::new(),
            waterfall: false,
            waterfall_message_limit: usize::MAX,
        };

        let _ = reader.do_initial_read().unwrap();

        // active_path must include asst-b so that when result-2 (parent=asst-b)
        // arrives live it is treated as a normal continuation, not an orphan.
        assert!(
            reader
                .message_path
                .active_path
                .contains(&"asst-b".to_string()),
            "active_path must include asst-b (sibling of the parallel result-1); \
             got: {:?}",
            reader.message_path.active_path
        );

        // tu-2 must have been parsed from asst-b so that result-2 can resolve it.
        assert!(
            reader.state.pending_tool_calls.contains_key("tu-2"),
            "tu-2 must be in pending_tool_calls after initial read so result-2 can resolve it; \
             pending: {:?}",
            reader.state.pending_tool_calls.keys().collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// End-to-end stall regression: if `do_initial_read` captures the file while
    /// it contains `root → asst-a → asst-b` plus the parallel `result-1`
    /// (parent=asst-a) as the last entry, the live stream must still emit ops
    /// for `result-2` (parent=asst-b) and any subsequent entries — no stall.
    #[tokio::test]
    async fn test_live_parallel_no_stall_when_initial_read_captures_mid_state() {
        use std::io::Write as _;

        let temp_dir = std::env::temp_dir().join(format!(
            "agt_test_live_mid_parallel_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let jsonl_path = temp_dir.join("session.jsonl");

        // Initial content: root, asst-a (tu-1), asst-b (tu-2, parent=asst-a),
        // result-1 (parallel result for tu-1, parent=asst-a).
        // result-2 is intentionally absent so that do_initial_read builds its
        // active_chain from result-1 backward — the scenario that causes the stall.
        {
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "user", "uuid": "root", "parentUuid": null,
                    "message": {"role": "user", "content": "hello"}
                })
            )
            .unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "assistant", "uuid": "asst-a", "parentUuid": "root",
                "message": {"id": "msg_asst_a", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu-1", "name": "Read", "input": {}}]}
            })).unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "assistant", "uuid": "asst-b", "parentUuid": "asst-a",
                "message": {"id": "msg_asst_b", "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu-2", "name": "Read", "input": {}}]}
            })).unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "user", "uuid": "result-1", "parentUuid": "asst-a",
                "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "file A"}]}
            })).unwrap();
        }

        let (tx, mut rx) = mpsc::channel(256);
        let path_clone = jsonl_path.clone();
        tokio::spawn(async move {
            claude_reader_task(path_clone, tx, false, false, 0).await;
        });

        // Allow the initial read to complete.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut initial_ops = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            initial_ops.push(op);
        }
        assert!(
            !initial_ops.is_empty(),
            "should produce ops from initial read"
        );
        assert!(
            !initial_ops
                .iter()
                .any(|op| matches!(op, ReaderOp::Reset { .. })),
            "no Reset expected during initial read"
        );

        // Append result-2: the result for tu-2, parent=asst-b.
        // With the bug, asst-b is not in active_chain so this entry is silently
        // dropped as an "orphaned sidechain" and no ops are ever emitted.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .unwrap();
            writeln!(f, "{}", serde_json::json!({
                "type": "user", "uuid": "result-2", "parentUuid": "asst-b",
                "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tu-2", "content": "file B"}]}
            })).unwrap();
        }

        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut after_result2 = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            after_result2.push(op);
        }

        let op_ids: Vec<String> = after_result2
            .iter()
            .map(|op| match op {
                ReaderOp::Reset { .. } => "Reset".to_string(),
                ReaderOp::Tree(TreeOperation::Append { message, .. }) => {
                    format!("Append({})", message.id)
                }
                ReaderOp::Tree(TreeOperation::Replace { id, .. }) => format!("Replace({})", id),
                ReaderOp::Tree(TreeOperation::Remove { id }) => format!("Remove({})", id),
                ReaderOp::Tree(TreeOperation::Update { id, .. }) => format!("Update({})", id),
                _ => "Other".to_string(),
            })
            .collect();

        // result-2 must have been processed: tool_result:tu-2 appended, no stall.
        assert!(
            after_result2.iter().any(|op| match op {
                ReaderOp::Tree(TreeOperation::Append { message, .. }) => {
                    message.id == "tool_result:tu-2"
                }
                _ => false,
            }),
            "tool_result:tu-2 must be emitted after result-2 arrives; got: {op_ids:?}"
        );

        // Append a final assistant message chaining from result-2.
        // With the bug, this is also never emitted because result-2 was dropped.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl_path)
                .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "type": "assistant", "uuid": "asst-final", "parentUuid": "result-2",
                    "message": {"id": "msg_asst_final", "role": "assistant",
                        "content": [{"type": "text", "text": "done"}]}
                })
            )
            .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut after_final = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            after_final.push(op);
        }

        let final_ids: Vec<String> = after_final
            .iter()
            .map(|op| match op {
                ReaderOp::Reset { .. } => "Reset".to_string(),
                ReaderOp::Tree(TreeOperation::Append { message, .. }) => {
                    format!("Append({})", message.id)
                }
                ReaderOp::Tree(TreeOperation::Replace { id, .. }) => format!("Replace({})", id),
                ReaderOp::Tree(TreeOperation::Remove { id }) => format!("Remove({})", id),
                ReaderOp::Tree(TreeOperation::Update { id, .. }) => format!("Update({})", id),
                _ => "Other".to_string(),
            })
            .collect();

        assert!(
            after_final.iter().any(|op| match op {
                ReaderOp::Tree(TreeOperation::Append { message, .. }) => {
                    message.id.contains("msg_asst_final")
                }
                _ => false,
            }),
            "final assistant node must be emitted after continuation; got: {final_ids:?}"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    // ── Sub-agent integration test helpers ────────────────────────────────────

    fn sa_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agt_sa_{}_{}",
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append_json(path: &std::path::Path, json: serde_json::Value) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{}", json).unwrap();
    }

    fn write_meta(sa_dir: &std::path::Path, agent_id: &str, description: &str) {
        use std::io::Write as _;
        let path = sa_dir.join(format!("agent-{}.meta.json", agent_id));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", serde_json::json!({"description": description})).unwrap();
    }

    fn drain_ops(rx: &mut mpsc::Receiver<color_eyre::Result<ReaderOp>>) -> Vec<ReaderOp> {
        let mut ops = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            ops.push(op);
        }
        ops
    }

    fn op_summary(ops: &[ReaderOp]) -> Vec<String> {
        ops.iter()
            .map(|op| match op {
                ReaderOp::Tree(TreeOperation::Append { parent_id, message }) => {
                    format!("Append({}, parent={:?})", message.id, parent_id.as_deref())
                }
                ReaderOp::Tree(TreeOperation::Replace { id, message }) => {
                    format!("Replace({}, text={:?})", id, message.text.as_deref())
                }
                ReaderOp::Tree(TreeOperation::Remove { id }) => format!("Remove({})", id),
                ReaderOp::Tree(TreeOperation::Update { id, .. }) => format!("Update({})", id),
                ReaderOp::Reset { id } => format!("Reset({:?})", id),
                ReaderOp::ResetDone => "ResetDone".to_string(),
            })
            .collect()
    }

    fn make_reader(jsonl: &std::path::Path) -> ClaudeReader {
        let session_dir = jsonl.with_extension("");
        let (watcher_tx, watcher_rx) = mpsc::unbounded_channel();
        let watcher = notify::RecommendedWatcher::new(
            move |_: notify::Result<notify::Event>| {},
            notify::Config::default(),
        )
        .unwrap();
        drop(watcher_tx);
        let (sa_tx, sa_rx) = mpsc::unbounded_channel();
        drop(sa_tx);
        ClaudeReader {
            jsonl_path: jsonl.to_owned(),
            state: ParseState::default(),
            message_path: MessagePath::new(),
            byte_offset: 0,
            subagent_manager: ClaudeSubagentManager::new(session_dir.clone()),
            watcher_rx,
            _watcher: watcher,
            sa_rx,
            sa_watcher: None,
            sa_watched_dir: session_dir,
            initial_ops: Vec::new(),
            waterfall: false,
            waterfall_message_limit: usize::MAX,
        }
    }

    // ── 1. Normal order — sync agent live stream ──────────────────────────────

    /// meta.json fires after the Agent tool_use has been processed; JSONL content
    /// streams live; completion arrives as a sync tool_result.
    #[tokio::test]
    async fn test_sa_sync_normal_order() {
        let td = sa_temp_dir("sync_normal");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut ops = drain_ops(&mut rx);

        // Write meta.json
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        ops.extend(drain_ops(&mut rx));

        // Should emit Agent ID placeholder parented under tool_call:tu-1
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { message, parent_id }) = op {
                    message.id == "task_summary:tu-1"
                        && parent_id.as_deref() == Some("tool_call:tu-1")
                        && message
                            .text
                            .as_deref()
                            .is_some_and(|t| t.starts_with("Agent ID:"))
                } else {
                    false
                }
            }),
            "meta.json should emit Agent ID placeholder; ops: {:?}",
            op_summary(&ops)
        );

        // Append a content line to SA JSONL: should be re-parented under tool_call:tu-1
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "Working on it"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { parent_id, .. }) = op {
                    parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "SA JSONL content must be re-parented under tool_call:tu-1; ops: {:?}",
            op_summary(&ops)
        );

        // Append sync tool_result: should Append task_summary child to tool_call:tu-1
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "result-1", "parentUuid": "asst-1",
                "toolUseResult": {"agentId": "abc", "content": [{"type": "text", "text": "Done!"}]},
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu-1", "content": "Done!"}
                ]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append {
                    parent_id: Some(p),
                    message,
                }) = op
                {
                    p == "tool_call:tu-1"
                        && message.id == "task_summary:tu-1"
                        && message.text.as_deref().unwrap_or("").contains("Done!")
                } else {
                    false
                }
            }),
            "sync tool_result should Append tool_call:tu-1 with Done! in task_summary child; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 2. Race — meta.json fires before main JSONL processes tool_call ───────

    /// Sub-agent meta.json is written before the Agent tool_use entry appears in
    /// the main JSONL.  Once the tool_use is appended the watcher must be matched
    /// and the placeholder emitted.
    #[tokio::test]
    async fn test_sa_race_meta_before_tool_use() {
        let td = sa_temp_dir("meta_before_tu");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        // Start with root only so the reader has nothing in tool_uses_by_description
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Write meta.json BEFORE the tool_use entry exists in main JSONL
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            !ops.iter().any(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { message, .. })
                    if message.id == "task_summary:tu-1")
            }),
            "no task_summary should appear before the tool_use is processed; ops: {:?}",
            op_summary(&ops)
        );

        // Now append the Agent tool_use — on_subagent_tool_use should find the waiting watcher
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { message, parent_id }) = op {
                    message.id == "task_summary:tu-1"
                        && parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "tool_use processing should emit task_summary:tu-1 placeholder by matching waiting watcher; ops: {:?}",
            op_summary(&ops)
        );

        // SA JSONL content should now flow correctly
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "running"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { parent_id, .. }) = op {
                    parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "SA content after race should be parented under tool_call:tu-1; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 3. Race — JSONL bytes arrive before meta.json ─────────────────────────

    /// agent-abc.jsonl is written before agent-abc.meta.json.  No ops should be
    /// emitted while the watcher is unmatched; once meta.json appears all buffered
    /// bytes must be emitted.
    #[tokio::test]
    async fn test_sa_race_jsonl_before_meta() {
        let td = sa_temp_dir("jsonl_before_meta");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Write JSONL bytes before meta.json — no watcher exists, nothing should be emitted
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "early line"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            !ops.iter().any(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { parent_id: Some(pid), .. })
                    if pid == "tool_call:tu-1")
            }),
            "JSONL bytes without a watcher must not emit SA ops; ops: {:?}",
            op_summary(&ops)
        );

        // Now write meta.json — should emit placeholder AND all buffered bytes
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { message, parent_id }) = op {
                    message.id == "task_summary:tu-1"
                        && parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "meta.json should emit task_summary:tu-1 placeholder; ops: {:?}",
            op_summary(&ops)
        );
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { parent_id, .. }) = op {
                    parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "buffered JSONL bytes must be emitted after meta.json; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 4. Async agent — normal order ─────────────────────────────────────────

    /// async_launched tool_result carries isAsync:true; sub-agent streams live
    /// and completes with a task-notification user message.
    #[tokio::test]
    async fn test_sa_async_normal_order() {
        let td = sa_temp_dir("async_normal");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        // Write root + Agent tool_use + async_launched tool_result
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "launched-1", "parentUuid": "asst-1",
                "toolUseResult": {"agentId": "abc", "isAsync": true},
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu-1", "content": "\u{2026}"}
                ]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Write meta.json — should emit placeholder
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { message, parent_id }) = op {
                    message.id == "task_summary:tu-1"
                        && parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "async meta.json should emit task_summary:tu-1 placeholder; ops: {:?}",
            op_summary(&ops)
        );

        // Stream SA JSONL lines
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "step 1"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { parent_id, .. }) = op {
                    parent_id.as_deref() == Some("tool_call:tu-1")
                } else {
                    false
                }
            }),
            "async SA JSONL must be parented under tool_call:tu-1; ops: {:?}",
            op_summary(&ops)
        );

        // Final result via task-notification
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "notify-1", "parentUuid": "launched-1",
                "message": {"role": "user",
                    "content": "<task-notification><summary>done</summary><result>All finished!</result><tool-use-id>tu-1</tool-use-id></task-notification>"}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Replace { message, .. }) = op {
                    message.id == "task_summary:tu-1"
                        && message
                            .text
                            .as_deref()
                            .unwrap_or("")
                            .contains("All finished!")
                } else {
                    false
                }
            }),
            "sync tool_result should Replace task_sumary:tu-1 with All finished!; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 5. Initial read with already-completed sub-agent ──────────────────────

    /// All entries (tool_use, SA JSONL, sync tool_result) exist before the reader
    /// starts.  do_initial_read must produce sub-agent content ops and the final
    /// task_summary text without a Reset.
    #[test]
    fn test_sa_initial_read_completed_sync() {
        let td = sa_temp_dir("initial_completed");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "result-1", "parentUuid": "asst-1",
                "toolUseResult": {"agentId": "abc", "content": [{"type": "text", "text": "Finished!"}]},
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu-1", "content": "Finished!"}
                ]}
            }),
        );

        // Pre-populate SA JSONL
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "I did the work"}]}
            }),
        );

        let mut reader = make_reader(&jsonl);
        let ops = reader.do_initial_read().unwrap();

        let op_ids: Vec<_> = op_summary(
            &ops.iter()
                .map(|op| ReaderOp::Tree(op.clone()))
                .collect::<Vec<_>>(),
        );

        // Sub-agent content must appear
        assert!(
            ops.iter().any(|op| {
                if let TreeOperation::Append { parent_id, message } = op {
                    parent_id.as_deref() == Some("tool_call:tu-1")
                        && message.id.starts_with("sa:abc:")
                } else {
                    false
                }
            }),
            "initial read must include sub-agent content parented under tool_call:tu-1; ops: {op_ids:?}"
        );

        // task_summary must carry the final result (not "Agent ID:")
        let summary_text = ops
            .iter()
            .filter_map(|op| {
                if let TreeOperation::Append {
                    parent_id: Some(p),
                    message,
                } = op
                {
                    if p == "tool_call:tu-1" {
                        message.text.as_deref()
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .next();
        assert!(
            summary_text.as_deref().unwrap_or("").contains("Finished!"),
            "task_summary should carry final result text, not placeholder; got: {summary_text:?}"
        );

        // No Reset op
        assert!(
            !ops.iter()
                .any(|op| matches!(op, TreeOperation::Remove { .. })),
            "no Remove/Reset ops expected"
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 6. Initial read with in-progress async sub-agent, then continuation ───

    /// async_launched was processed during initial read with N SA JSONL lines
    /// already present.  After start, new lines appended to SA JSONL must stream
    /// without duplicating the first N lines.
    #[tokio::test]
    async fn test_sa_initial_read_async_in_progress_then_continuation() {
        let td = sa_temp_dir("async_inprogress");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "launched-1", "parentUuid": "asst-1",
                "toolUseResult": {"agentId": "abc", "isAsync": true},
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu-1", "content": "\u{2026}"}
                ]}
            }),
        );

        // Pre-populate 2 SA JSONL lines
        write_meta(&sa_dir, "abc", "TaskA");
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "line one"}]}
            }),
        );
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-2", "parentUuid": "sa-1",
                "message": {"id": "sa-msg-2", "role": "assistant",
                            "content": [{"type": "text", "text": "line two"}]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let initial_ops = drain_ops(&mut rx);

        // The first 2 SA lines must appear in initial_ops
        let initial_sa_count = initial_ops
            .iter()
            .filter(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { parent_id: Some(pid), .. })
                    if pid == "tool_call:tu-1")
            })
            .count();
        assert!(
            initial_sa_count >= 2,
            "initial read should include the 2 pre-existing SA lines; got {initial_sa_count}; ops: {:?}",
            op_summary(&initial_ops)
        );

        // Append a third SA line — must not duplicate first two
        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-3", "parentUuid": "sa-2",
                "message": {"id": "sa-msg-3", "role": "assistant",
                            "content": [{"type": "text", "text": "line three"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);

        let new_sa_ops: Vec<_> = ops
            .iter()
            .filter(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { parent_id: Some(pid), .. })
                    if pid == "tool_call:tu-1")
            })
            .collect();
        assert_eq!(
            new_sa_ops.len(),
            1,
            "only the new (third) SA line must be emitted — no duplication; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 7. Completion seen before sub-agent streaming is set up ───────────────

    /// tool_result (sync) arrives in main JSONL before meta.json was ever written.
    /// Pre-populated SA JSONL should still appear in the initial read via the
    /// tool_uses_by_description path; no crash.
    #[tokio::test]
    async fn test_sa_completion_before_streaming() {
        let td = sa_temp_dir("completion_before_stream");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "result-1", "parentUuid": "asst-1",
                "toolUseResult": {"agentId": "abc", "content": [{"type": "text", "text": "Done!"}]},
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu-1", "content": "Done!"}
                ]}
            }),
        );

        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "completed work"}]}
            }),
        );

        info!("WROTE TO {:?}", &sa_dir.join("agent-abc.jsonl"));

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);

        let op_ids: Vec<_> = op_summary(&ops);

        assert!(
            ops.iter().any(|op| {
                if let ReaderOp::Tree(TreeOperation::Append { message, parent_id }) = op {
                    parent_id.as_deref() == Some("tool_call:tu-1")
                        && message.id.starts_with("sa:abc:")
                } else {
                    false
                }
            }),
            "SA content must appear even without meta.json; ops: {op_ids:?}"
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 8. Rewind clears subagents and tool_uses_by_description ────────────────────

    /// A rewind entry in main JSONL during an active sub-agent stream causes a
    /// Reset; subsequent entries are handled cleanly without stale watcher state.
    #[tokio::test]
    async fn test_sa_rewind_clears_subagent_state() {
        let td = sa_temp_dir("rewind_clears");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Set up sub-agent
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx); // consume placeholder

        append_json(
            &sa_dir.join("agent-abc.jsonl"),
            serde_json::json!({
                "type": "assistant", "uuid": "sa-1", "parentUuid": null,
                "message": {"id": "sa-msg-1", "role": "assistant",
                            "content": [{"type": "text", "text": "running"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Append a rewind: same parent as asst-1 (root), different uuid
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-rewind", "parentUuid": "root",
                "message": {"id": "msg-rewind", "role": "assistant",
                            "content": [{"type": "text", "text": "restarting"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            ops.iter().any(|op| matches!(op, ReaderOp::Reset { .. })),
            "rewind must emit Reset; ops: {:?}",
            op_summary(&ops)
        );

        // Stream a clean continuation — no stale task_summary from tu-1
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-cont", "parentUuid": "asst-rewind",
                "message": {"id": "msg-cont", "role": "assistant",
                            "content": [{"type": "text", "text": "clean continuation"}]}
            }),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            !ops.iter().any(|op| matches!(op, ReaderOp::Reset { .. })),
            "no second Reset after clean continuation; ops: {:?}",
            op_summary(&ops)
        );
        assert!(
            ops.iter().any(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { message, .. })
                    if message.id.contains("msg-cont"))
            }),
            "clean continuation must be streamed; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 9. Duplicate meta.json events are idempotent ──────────────────────────

    /// The filesystem fires multiple events for the same meta.json.
    /// task_summary:tu-1 must appear exactly once.
    #[tokio::test]
    async fn test_sa_duplicate_meta_events_idempotent() {
        let td = sa_temp_dir("dup_meta");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Write meta.json once
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Write same content again to trigger a second event
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);

        let task_summary_appends = ops
            .iter()
            .filter(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { message, .. })
                    if message.id == "task_summary:tu-1")
            })
            .count();
        assert_eq!(
            task_summary_appends,
            1,
            "task_summary:tu-1 must appear exactly once despite duplicate meta.json events; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }

    // ── 10. No bytes read when tool_use_id is None ────────────────────────────

    /// JSONL events fire before the watcher is matched (no meta.json yet).
    /// All bytes must be buffered at offset 0 and emitted in full once meta.json arrives.
    #[tokio::test]
    async fn test_sa_no_bytes_read_without_tool_use_id() {
        let td = sa_temp_dir("no_bytes_unmatched");
        let jsonl = td.join("session.jsonl");
        let sa_dir = td.join("session").join("subagents");
        std::fs::create_dir_all(&sa_dir).unwrap();

        append_json(
            &jsonl,
            serde_json::json!({
                "type": "user", "uuid": "root", "parentUuid": null,
                "message": {"role": "user", "content": "hi"}
            }),
        );
        append_json(
            &jsonl,
            serde_json::json!({
                "type": "assistant", "uuid": "asst-1", "parentUuid": "root",
                "message": {"id": "msg-1", "role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu-1", "name": "Agent",
                     "input": {"description": "TaskA", "prompt": "..."}}
                ]}
            }),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let jsonl2 = jsonl.clone();
        tokio::spawn(async move { claude_reader_task(jsonl2, tx, false, false, 0).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_ops(&mut rx);

        // Write 3 JSONL lines with no meta.json — watcher does not exist yet
        for i in 1..=3usize {
            append_json(
                &sa_dir.join("agent-abc.jsonl"),
                serde_json::json!({
                    "type": "assistant",
                    "uuid": format!("sa-{}", i),
                    "parentUuid": if i == 1 { serde_json::Value::Null } else { serde_json::json!(format!("sa-{}", i - 1)) },
                    "message": {"id": format!("sa-msg-{}", i), "role": "assistant",
                                "content": [{"type": "text", "text": format!("line {}", i)}]}
                }),
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);
        assert!(
            !ops.iter().any(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { parent_id: Some(pid), .. })
                    if pid == "tool_call:tu-1")
            }),
            "no SA ops must be emitted before meta.json exists; ops: {:?}",
            op_summary(&ops)
        );

        // Now write meta.json — all 3 buffered lines must appear
        write_meta(&sa_dir, "abc", "TaskA");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ops = drain_ops(&mut rx);

        let sa_content_ops = ops
            .iter()
            .filter(|op| {
                matches!(op, ReaderOp::Tree(TreeOperation::Append { parent_id: Some(pid), .. })
                    if pid == "tool_call:tu-1")
            })
            .count();
        assert!(
            sa_content_ops >= 3,
            "all 3 buffered SA lines must be emitted after meta.json; got {sa_content_ops}; ops: {:?}",
            op_summary(&ops)
        );

        std::fs::remove_dir_all(&td).ok();
    }
}
