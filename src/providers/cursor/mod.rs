pub mod db;
pub mod message;
pub mod proto;
mod reader;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::sync::mpsc;

use tracing::info;

use self::db::CursorDb;
use self::proto::{extract_field1_blobs, extract_field9_bytes, extract_field26_varint};
use crate::reader_op::ReaderOp;

use super::{
    LoadConfig, Provider, ProviderKind, TranscriptCacheKey, TranscriptEntry, TranscriptReader,
};

pub struct CursorProvider;

#[async_trait]
impl Provider for CursorProvider {
    fn scan_paths(&self, cwd: Option<&Path>) -> Vec<(PathBuf, TranscriptCacheKey, i64)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let pattern = if let Some(cwd) = cwd {
            let workspace_str = cwd.to_string_lossy();
            let hash = format!("{:x}", md5::compute(workspace_str.as_bytes()));
            format!("{}/.cursor/chats/{}/*/store.db", home, hash)
        } else {
            format!("{}/.cursor/chats/*/*/store.db", home)
        };
        let Ok(paths) = glob::glob(&pattern) else {
            return vec![];
        };
        paths
            .flatten()
            .filter_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                let size = meta.len();
                // Priority 0 signals "always re-read": used when a WAL file is present,
                // meaning Cursor is actively writing and the db may be mid-transaction.
                // Otherwise encode recency as -mtime_secs (ascending sort = newest first).
                let priority = if wal_active(&path) {
                    0i64
                } else {
                    let mtime = meta.modified().ok()?;
                    -(mtime
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0))
                };
                Some((path, TranscriptCacheKey::Size(size), priority))
            })
            .collect()
    }

    fn find_transcript_path(
        &self,
        session_id: &str,
        workspace_path: Option<&Path>,
    ) -> Option<PathBuf> {
        if let Some(wp) = workspace_path {
            let path = compute_session_db(session_id, wp);
            if path.exists() {
                return Some(path);
            }
        }
        find_session_db(session_id)
    }

    fn compute_transcript_path(
        &self,
        session_id: &str,
        workspace_path: Option<&Path>,
    ) -> Option<PathBuf> {
        workspace_path.map(|wp| compute_session_db(session_id, wp))
    }

    fn read_entry(&self, path: &Path) -> Option<TranscriptEntry> {
        let db = CursorDb::open(path).ok()?;
        let (name, _created_at) = db.session_meta().ok()?;

        let session_id = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let root_id = db.latest_root_blob_id().ok()?;
        let root_data = db.fetch_blob(&root_id).ok()?;

        let meta = std::fs::metadata(path).ok();
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Local>::from)
            .unwrap_or_else(chrono::Local::now);
        let size = meta.as_ref().map(|m| m.len());

        let updated_at = extract_field26_varint(&root_data)
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
            .map(|dt| dt.with_timezone(&chrono::Local));

        let workspace_path = extract_field9_bytes(&root_data)
            .and_then(|v| String::from_utf8(v).ok())
            .and_then(|uri| {
                uri.strip_prefix("file://")
                    .map(|s| PathBuf::from(s.to_string()))
            });

        let msg_hashes = extract_field1_blobs(&root_data);
        let message_count = msg_hashes.len();
        let last_user_message = find_last_user_message(&db, &msg_hashes);

        Some(TranscriptEntry {
            path: path.to_owned(),
            id: session_id,
            title: if name.is_empty() {
                "(unnamed)".to_string()
            } else {
                name
            },
            mtime,
            updated_at,
            size,
            last_user_message,
            message_count,
            workspace_path,
            provider: ProviderKind::Cursor,
        })
    }

    async fn open_reader(
        &self,
        path: &Path,
        config: LoadConfig,
    ) -> color_eyre::Result<Box<dyn TranscriptReader>> {
        let db_path = path.to_owned();
        let snapshot = config.snapshot;
        info!(path = %db_path.display(), snapshot, "opening cursor reader");
        let (tx, rx) = mpsc::channel::<color_eyre::Result<ReaderOp>>(256);

        tokio::spawn(async move {
            reader::cursor_reader_task(db_path, tx, snapshot).await;
        });

        Ok(Box::new(reader::CursorTranscriptReader::new(rx)))
    }
}

fn find_last_user_message(db: &CursorDb, msg_hashes: &[[u8; 32]]) -> Option<String> {
    for hash in msg_hashes.iter().rev() {
        let blob_id = bytes_to_hex(hash);
        let Ok(data) = db.fetch_blob(&blob_id) else {
            continue;
        };
        let Ok(obj) = serde_json::from_slice::<serde_json::Value>(&data) else {
            continue;
        };
        if obj.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let content = obj.get("content")?;
        let text = if let Some(s) = content.as_str() {
            s.to_string()
        } else if let Some(arr) = content.as_array() {
            arr.iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            continue;
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

pub(super) fn bytes_to_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute the expected DB path from a session ID and workspace path.
///
/// Cursor names the chats subdirectory after the MD5 of the workspace root
/// path string, so the DB location can be derived without a filesystem glob.
/// The file may not exist yet when a new session is just starting.
fn compute_session_db(session_id: &str, workspace_path: &std::path::Path) -> PathBuf {
    let workspace_str = workspace_path.to_string_lossy();
    let hash = format!("{:x}", md5::compute(workspace_str.as_bytes()));
    let home = std::env::var("HOME").unwrap_or_default();
    let path = PathBuf::from(format!(
        "{}/.cursor/chats/{}/{}/store.db",
        home, hash, session_id
    ));
    info!(
        session_id = %session_id,
        workspace = %workspace_str,
        path = %path.display(),
        "computed session db path"
    );
    path
}

/// Locate a session by ID via glob: `~/.cursor/chats/*/{id}/store.db`.
///
/// Used when no workspace path is available (e.g. `--resume` without a hook firing).
pub fn find_session_db(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let pattern = format!("{}/.cursor/chats/*/{}/store.db", home, session_id);
    let result = glob::glob(&pattern).ok()?.filter_map(|r| r.ok()).next();
    match &result {
        Some(path) => info!(session_id = %session_id, path = %path.display(), "found session db"),
        None => info!(session_id = %session_id, pattern = %pattern, "session db not found"),
    }
    result
}

/// Returns true when a non-empty WAL file exists alongside `path`, indicating
/// Cursor is actively writing to the database.  An empty WAL (post-checkpoint
/// artifact left by SQLite) is not considered active.
fn wal_active(path: &Path) -> bool {
    let mut wal = path.to_string_lossy().into_owned();
    wal.push_str("-wal");
    std::fs::metadata(wal).map(|m| m.len() > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn find_any_cursor_db() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        glob::glob(&format!("{}/.cursor/chats/*/*/store.db", home))
            .ok()?
            .flatten()
            .filter(|p| p.exists())
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    }

    #[test]
    #[ignore = "requires Cursor installation"]
    fn test_read_entry_real() {
        let Some(db_path) = find_any_cursor_db() else {
            println!("no cursor DB found, skipping");
            return;
        };
        let entry = CursorProvider.read_entry(&db_path).unwrap();
        println!("title: {}", entry.title);
        println!("mtime: {}", entry.mtime);
        println!("updated_at: {:?}", entry.updated_at);
        println!("size: {:?}", entry.size);
        println!("workspace_path: {:?}", entry.workspace_path);
        println!("message_count: {}", entry.message_count);
        println!(
            "last_user_message: {:?}",
            entry
                .last_user_message
                .as_deref()
                .map(|s| &s[..s.len().min(80)])
        );
        assert!(!entry.title.is_empty());
        assert!(entry.message_count > 0);
        assert!(entry.workspace_path.is_some());
        assert!(entry.size.is_some());

        // updated_at should come from field-26 snapshot timestamp (ms since epoch) — verify it's a plausible recent date.
        let epoch_2024 = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Local);
        let updated_at = entry.updated_at.expect("updated_at should be present");
        assert!(
            updated_at > epoch_2024,
            "updated_at should be after 2024-01-01, got {}",
            updated_at
        );
    }
}
