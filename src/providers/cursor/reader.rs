use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use notify::Watcher;
use tracing::{debug, info, warn};

use super::bytes_to_hex;
use super::db::CursorDb;
use super::message::{ParseState, parse_blob, parse_pending_blob};
use super::proto::{extract_field1_blobs, extract_field4_bytes};
use crate::providers::TranscriptReader;
use crate::reader_op::ReaderOp;
use crate::tree_operation::TreeOperation;

pub(super) struct CursorTranscriptReader {
    rx: mpsc::Receiver<color_eyre::Result<ReaderOp>>,
}

impl CursorTranscriptReader {
    pub(super) fn new(rx: mpsc::Receiver<color_eyre::Result<ReaderOp>>) -> Self {
        Self { rx }
    }
}

impl TranscriptReader for CursorTranscriptReader {
    fn updates(&mut self) -> &mut mpsc::Receiver<color_eyre::Result<ReaderOp>> {
        &mut self.rx
    }
}

struct CursorReader {
    db: CursorDb,
    state: ParseState,
    /// Field-1 blob hashes from the last processed root; used to detect rewinds.
    last_field1_hashes: HashSet<[u8; 32]>,
    /// Root blob ID from the last processed update; used to skip no-op polls.
    last_root_id: String,
    watcher_rx: mpsc::UnboundedReceiver<()>,
    /// Kept alive so the watcher thread continues sending notifications.
    _watcher: notify::RecommendedWatcher,
}

impl CursorReader {
    /// Set up the filesystem watcher and wait until the DB exists and has a
    /// root blob.
    async fn init(db_path: &Path) -> color_eyre::Result<Self> {
        let (watcher_tx, mut watcher_rx) = mpsc::unbounded_channel::<()>();
        let mut watcher = notify::RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if let Ok(evt) = res
                    && matches!(
                        evt.kind,
                        notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                    )
                    && evt.paths.iter().any(|p| {
                        matches!(
                            p.file_name().and_then(|n| n.to_str()),
                            Some("store.db" | "store.db-wal")
                        )
                    })
                {
                    let _ = watcher_tx.send(());
                }
            },
            notify::Config::default(),
        )?;

        // Watch the session dir if it already exists; otherwise watch the
        // deepest existing ancestor so we catch directory+file creation.
        let db_dir = db_path.parent().expect("db_path has no parent");
        let ancestor_watch: Option<PathBuf> = if db_dir.exists() {
            watcher.watch(db_dir, notify::RecursiveMode::NonRecursive)?;
            info!(dir = %db_dir.display(), "watching directory for changes");
            None
        } else {
            let mut ancestor = db_dir;
            loop {
                ancestor = ancestor.parent().expect("must have an existing ancestor");
                if ancestor.exists() {
                    break;
                }
            }
            watcher.watch(ancestor, notify::RecursiveMode::Recursive)?;
            info!(dir = %ancestor.display(), "db not found, watching ancestor for creation");
            Some(ancestor.to_owned())
        };

        // Wait until the DB exists and its meta (latestRootBlobId) is
        // initialised.  Cursor creates the SQLite file before writing the
        // first message, so the file may exist but have an empty meta briefly.
        let (db, initial_root_id) = loop {
            if db_path.exists() {
                match CursorDb::open(db_path) {
                    Ok(db) => match db.latest_root_blob_id() {
                        Ok(id) if !id.is_empty() => {
                            info!("db is ready");
                            break (db, id);
                        }
                        Ok(_) => debug!("db exists but root not yet initialised"),
                        Err(e) => debug!("db meta not ready: {e}"),
                    },
                    Err(e) => debug!("db not yet openable: {e}"),
                }
            } else {
                debug!("waiting for db to be created");
            }
            match watcher_rx.recv().await {
                Some(()) => debug!("fs event while waiting for db"),
                None => color_eyre::eyre::bail!("watcher channel closed before db was ready"),
            }
        };

        // Switch from the broad ancestor watch to the specific session dir
        // now that db_dir exists.
        if let Some(ancestor) = ancestor_watch {
            let _ = watcher.unwatch(&ancestor);
            watcher.watch(db_dir, notify::RecursiveMode::NonRecursive)?;
            info!(dir = %db_dir.display(), "switched to watching session directory");
        }

        Ok(Self {
            db,
            state: ParseState::new(),
            last_field1_hashes: HashSet::new(),
            last_root_id: initial_root_id,
            watcher_rx,
            _watcher: watcher,
        })
    }

    /// Collect all TreeOperations for the given root blob.
    /// Updates `self.last_field1_hashes` for rewind detection on the next call.
    fn collect_ops(&mut self, root_id: &str) -> Vec<TreeOperation> {
        let root_data = match self.db.fetch_blob(root_id) {
            Ok(d) => d,
            Err(e) => {
                warn!(root_id = %&root_id[..root_id.len().min(16)], "fetch root blob: {}", e);
                return vec![];
            }
        };
        let blob_hashes = extract_field1_blobs(&root_data);
        debug!(blobs = blob_hashes.len(), "field-1 blobs in root");
        let mut all_ops = Vec::new();

        // Remove committed pending node if it was present.
        if self.state.has_pending {
            debug!("removing pending node (committed)");
            all_ops.push(TreeOperation::Remove {
                id: "streaming:pending".to_string(),
            });
            self.state.has_pending = false;
        }

        // System blob pre-pass: if the first blob in the current root is a system message,
        // emit it before the field-13 recovery loop so it appears at the top of the tree.
        if let Some(first_hash) = blob_hashes.first()
            && !self.state.seen_blobs.contains(first_hash)
        {
            let blob_id = bytes_to_hex(first_hash);
            match self.db.fetch_blob(&blob_id) {
                Ok(data) => match serde_json::from_slice::<serde_json::Value>(&data) {
                    Ok(obj) if obj["role"].as_str() == Some("system") => {
                        let ops = parse_blob(&blob_id, &data, &mut self.state);
                        all_ops.extend(ops);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(blob_id = %&blob_id[..16], "system blob pre-pass JSON error: {}", e);
                    }
                },
                Err(e) => {
                    warn!(blob_id = %&blob_id[..16], "system blob pre-pass fetch error: {}", e);
                }
            }
        }

        // Recover pre-summary history via field-13 back-references.
        // Each entry is a pre-summary snapshot whose messages were dropped when Cursor
        // summarized the context. We process them first (oldest to newest) so recovered
        // turns receive correct turn numbers that precede the current root's turns.
        // seen_blobs dedup suppresses any overlap; last_field1_hashes is unaffected.
        match self.db.fetch_pre_summary_snapshots(&root_data) {
            Ok(snapshots) => {
                for historical_hashes in snapshots {
                    for hash in &historical_hashes {
                        if self.state.seen_blobs.contains(hash) {
                            continue;
                        }
                        let blob_id = bytes_to_hex(hash);
                        let data = match self.db.fetch_blob(&blob_id) {
                            Ok(d) => d,
                            Err(e) => {
                                warn!(blob_id = %&blob_id[..16], "fetch pre-summary blob: {}", e);
                                continue;
                            }
                        };
                        let ops = parse_blob(&blob_id, &data, &mut self.state);
                        all_ops.extend(ops);
                    }
                }
            }
            Err(e) => {
                warn!("fetch_pre_summary_snapshots failed: {}", e);
            }
        }

        for hash in &blob_hashes {
            if self.state.seen_blobs.contains(hash) {
                continue;
            }
            let blob_id = bytes_to_hex(hash);
            let data = match self.db.fetch_blob(&blob_id) {
                Ok(d) => d,
                Err(e) => {
                    warn!(blob_id = %&blob_id[..16], "fetch blob: {}", e);
                    self.state.seen_blobs.insert(*hash);
                    continue;
                }
            };
            debug!(blob_id = %&blob_id[..16], "parsing blob");
            let ops = parse_blob(&blob_id, &data, &mut self.state);
            debug!(blob_id = %&blob_id[..16], ops = ops.len(), "parsed blob");
            all_ops.extend(ops);
        }

        self.last_field1_hashes = blob_hashes.into_iter().collect();

        // Handle field-4 streaming (partial assistant blob).
        if let Some(field4) = extract_field4_bytes(&root_data) {
            let pending_ops = parse_pending_blob(&field4, &mut self.state);
            all_ops.extend(pending_ops);
        }

        all_ops
    }

    /// Called after each debounce period.  Returns the ops to emit, or `None`
    /// if the root blob is unchanged or a transient error occurred (caller
    /// should continue the loop).
    fn handle_db_change(&mut self) -> Option<Vec<ReaderOp>> {
        let new_root_id = match self.db.latest_root_blob_id() {
            Ok(id) => id,
            Err(e) => {
                warn!("latest_root_blob_id: {}", e);
                return None;
            }
        };
        if new_root_id == self.last_root_id {
            debug!("root unchanged, skipping");
            return None;
        }
        debug!(
            old_root = %&self.last_root_id[..self.last_root_id.len().min(16)],
            new_root = %&new_root_id[..new_root_id.len().min(16)],
            "new root blob detected"
        );

        // Rewind detection: compare field-1 hashes before processing.
        // last_root_id is only committed after this fetch succeeds, so a
        // transient DB error here does not permanently stall the reader.
        let new_root_data = match self.db.fetch_blob(&new_root_id) {
            Ok(d) => d,
            Err(e) => {
                warn!("fetch root blob for rewind check: {}", e);
                return None;
            }
        };
        let new_field1_hashes: HashSet<[u8; 32]> =
            extract_field1_blobs(&new_root_data).into_iter().collect();

        self.last_root_id = new_root_id.clone();
        let mut ops: Vec<ReaderOp> = Vec::new();
        let rewind = !self.last_field1_hashes.is_subset(&new_field1_hashes);
        if rewind {
            info!("rewind detected: resetting parse state");
            ops.push(ReaderOp::Reset { id: None });
            self.state = ParseState::new();
            self.last_field1_hashes = HashSet::new();
        }
        ops.extend(
            self.collect_ops(&new_root_id)
                .into_iter()
                .map(ReaderOp::Tree),
        );
        if rewind {
            ops.push(ReaderOp::ResetDone);
        }
        Some(ops)
    }

    /// Main event loop: emit the initial burst then watch for DB changes.
    async fn run(mut self, tx: mpsc::Sender<color_eyre::Result<ReaderOp>>, snapshot: bool) {
        if let Err(e) = self.run_inner(&tx, snapshot).await {
            let _ = tx.send(Err(e)).await;
        }
    }

    async fn run_inner(
        &mut self,
        tx: &mpsc::Sender<color_eyre::Result<ReaderOp>>,
        snapshot: bool,
    ) -> color_eyre::Result<()> {
        // Initial burst: load and emit all ops from the first root.
        let initial_root_id = self.last_root_id.clone();
        debug!(root_id = %&initial_root_id[..initial_root_id.len().min(16)], "initial root blob id");
        let ops = self.collect_ops(&initial_root_id);
        info!(ops = ops.len(), root_id = %&initial_root_id[..initial_root_id.len().min(16)], "initial load complete");
        for op in ops {
            if tx.send(Ok(ReaderOp::Tree(op))).await.is_err() {
                return Ok(());
            }
        }

        if snapshot {
            return Ok(());
        }

        loop {
            let Some(()) = self.watcher_rx.recv().await else {
                color_eyre::eyre::bail!("watcher channel closed unexpectedly");
            };
            debug!("fs watcher event received");

            // Debounce: drain queued events for 50 ms.
            let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    Some(()) = self.watcher_rx.recv() => {},
                }
            }
            debug!("debounce complete");

            let Some(new_ops) = self.handle_db_change() else {
                continue;
            };

            debug!(ops = new_ops.len(), "sending ops");
            for op in new_ops {
                if tx.send(Ok(op)).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    #[cfg(test)]
    fn new_for_test(db: CursorDb) -> Self {
        let (_tx, watcher_rx) = mpsc::unbounded_channel();
        let watcher = notify::RecommendedWatcher::new(|_| {}, notify::Config::default()).unwrap();
        Self {
            db,
            state: ParseState::new(),
            last_field1_hashes: HashSet::new(),
            last_root_id: String::new(),
            watcher_rx,
            _watcher: watcher,
        }
    }
}

pub(super) async fn cursor_reader_task(
    db_path: PathBuf,
    tx: mpsc::Sender<color_eyre::Result<ReaderOp>>,
    snapshot: bool,
) {
    info!(path = %db_path.display(), "cursor reader task started");
    match CursorReader::init(&db_path).await {
        Ok(reader) => reader.run(tx, snapshot).await,
        Err(e) => {
            let _ = tx.send(Err(e)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::providers::cursor::db::CursorDb;
    use crate::providers::cursor::proto::extract_field13_refs;

    fn find_any_cursor_db() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        glob::glob(&format!("{}/.cursor/chats/*/*/store.db", home))
            .ok()?
            .flatten()
            .filter(|p| p.exists())
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    }

    fn find_summary_cursor_db() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        glob::glob(&format!("{}/.cursor/chats/*/*/store.db", home))
            .ok()?
            .flatten()
            .filter(|p| p.exists())
            .find(|p| {
                CursorDb::open(p)
                    .ok()
                    .and_then(|db| {
                        let root_id = db.latest_root_blob_id().ok()?;
                        let root_data = db.fetch_blob(&root_id).ok()?;
                        Some(!extract_field13_refs(&root_data).is_empty())
                    })
                    .unwrap_or(false)
            })
    }

    #[test]
    #[ignore = "requires Cursor installation"]
    fn test_full_parse_real_session() {
        let Some(db_path) = find_any_cursor_db() else {
            println!("no cursor DB found, skipping");
            return;
        };
        let db = CursorDb::open(&db_path).unwrap();
        let mut reader = CursorReader::new_for_test(db);
        let root_id = reader.db.latest_root_blob_id().unwrap();
        let ops = reader.collect_ops(&root_id);
        println!("Generated {} TreeOperations", ops.len());
        assert!(
            !ops.is_empty(),
            "should generate operations from a non-empty session"
        );

        for op in ops.iter().take(10) {
            match op {
                crate::tree_operation::TreeOperation::Append { parent_id, message } => {
                    println!(
                        "Append(parent={:?}) id={} text={:?}",
                        parent_id,
                        message.id,
                        message.text.as_deref().map(|t| &t[..t.len().min(60)])
                    );
                }
                crate::tree_operation::TreeOperation::Replace { id, message } => {
                    println!(
                        "Replace(id={}) → id={} text={:?}",
                        id,
                        message.id,
                        message.text.as_deref().map(|t| &t[..t.len().min(60)])
                    );
                }
                crate::tree_operation::TreeOperation::Remove { id } => {
                    println!("Remove(id={})", id);
                }
                crate::tree_operation::TreeOperation::Update { id, message } => {
                    println!(
                        "Update(id={}) text={:?}",
                        id,
                        message.text.as_deref().map(|t| &t[..t.len().min(60)])
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires Cursor installation"]
    fn test_summary_session_recovery() {
        use crate::tree_operation::TreeOperation;

        let Some(db_path) = find_summary_cursor_db() else {
            println!("no cursor DB with summaries found, skipping");
            return;
        };
        let db = CursorDb::open(&db_path).unwrap();
        let mut reader = CursorReader::new_for_test(db);
        let root_id = reader.db.latest_root_blob_id().unwrap();
        let ops = reader.collect_ops(&root_id);

        // Collect append IDs and tags in emission order.
        let appends: Vec<(String, Option<String>)> = ops
            .iter()
            .filter_map(|op| match op {
                TreeOperation::Append { message, .. } => {
                    Some((message.id.clone(), message.tag.clone()))
                }
                _ => None,
            })
            .collect();

        // At least one summary node must be present.
        let summary_pos = appends
            .iter()
            .position(|(_, tag)| tag.as_deref() == Some("summary"))
            .expect("should have at least one summary node");

        // Field-13 recovery emits pre-summary messages before the summary node.
        // Any user message before the summary confirms recovery is working.
        let pre_summary_user_pos = appends
            .iter()
            .position(|(id, _)| id.starts_with("user_msg:"))
            .expect("should have at least one user message before the summary");

        assert!(
            pre_summary_user_pos < summary_pos,
            "user message (pos {pre_summary_user_pos}) should precede summary (pos {summary_pos})"
        );

        println!(
            "OK: {} ops, summary at pos {summary_pos}, first user msg at pos {pre_summary_user_pos} (path={})",
            ops.len(),
            db_path.display()
        );
    }
}
