use std::path::Path;

use color_eyre::eyre::Context;
use rusqlite::{Connection, OpenFlags};

use super::bytes_to_hex;
use super::proto::{extract_field1_blobs, extract_field13_refs};

pub struct CursorDb {
    conn: Connection,
}

/// Session-level fields from the `meta` table.
pub struct SessionMeta {
    pub name: String,
    pub created_at_ms: i64,
    /// Set when the session was spawned by a parent agent (meta has `subagentInfo`).
    pub is_subagent: bool,
}

impl CursorDb {
    pub fn open(path: &Path) -> color_eyre::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("opening store.db")?;
        Ok(Self { conn })
    }

    fn read_meta_json(&self) -> color_eyre::Result<serde_json::Value> {
        let hex_val: String = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
                row.get(0)
            })
            .context("reading meta key 0")?;
        let bytes = hex_decode(&hex_val)
            .map_err(|e| color_eyre::eyre::eyre!("hex-decoding meta value: {}", e))?;
        let obj: serde_json::Value = serde_json::from_slice(&bytes).context("parsing meta JSON")?;
        Ok(obj)
    }

    pub fn session_meta(&self) -> color_eyre::Result<SessionMeta> {
        let obj = self.read_meta_json()?;
        Ok(SessionMeta {
            name: obj["name"].as_str().unwrap_or("").to_string(),
            created_at_ms: obj["createdAt"].as_i64().unwrap_or(0),
            is_subagent: obj.get("subagentInfo").is_some_and(|v| !v.is_null()),
        })
    }

    pub fn latest_root_blob_id(&self) -> color_eyre::Result<String> {
        let obj = self.read_meta_json()?;
        let id = obj["latestRootBlobId"].as_str().unwrap_or("").to_string();
        Ok(id)
    }

    pub fn fetch_blob(&self, id: &str) -> color_eyre::Result<Vec<u8>> {
        let data: Vec<u8> = self
            .conn
            .query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .with_context(|| format!("fetching blob {}", &id[..id.len().min(16)]))?;
        Ok(data)
    }

    /// Returns the field-1 message blob ID lists of every field-13 pre-summary snapshot,
    /// in order (oldest first). Returns an empty vec if the root has no field-13 entries.
    pub fn fetch_pre_summary_snapshots(
        &self,
        root_data: &[u8],
    ) -> color_eyre::Result<Vec<Vec<[u8; 32]>>> {
        let refs = extract_field13_refs(root_data);
        let mut result = Vec::new();
        for hash in refs {
            let blob_id = bytes_to_hex(&hash);
            let data = self.fetch_blob(&blob_id)?;
            result.push(extract_field1_blobs(&data));
        }
        Ok(result)
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {}", s.len()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Write a store.db whose only content is a meta row holding `meta_json`.
    fn db_with_meta(dir: &Path, meta_json: &str) -> PathBuf {
        let path = dir.join("store.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        let hex: String = meta_json.bytes().map(|b| format!("{b:02x}")).collect();
        conn.execute("INSERT INTO meta VALUES ('0', ?1)", [hex])
            .unwrap();
        path
    }

    #[test]
    fn session_meta_detects_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_with_meta(
            dir.path(),
            r#"{"name":"New Agent","createdAt":1,"subagentInfo":{"parentAgentId":"p","rootParentAgentId":"p","toolCallId":"t","typeName":"generalPurpose"}}"#,
        );
        let meta = CursorDb::open(&path).unwrap().session_meta().unwrap();
        assert!(meta.is_subagent);
        assert_eq!(meta.name, "New Agent");
    }

    #[test]
    fn session_meta_without_subagent_info_is_not_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_with_meta(dir.path(), r#"{"name":"Parent","createdAt":1}"#);
        let meta = CursorDb::open(&path).unwrap().session_meta().unwrap();
        assert!(!meta.is_subagent);
    }

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
    fn test_real_session_meta() {
        let Some(db_path) = find_any_cursor_db() else {
            println!("no cursor DB found, skipping");
            return;
        };
        let db = CursorDb::open(&db_path).unwrap();
        let meta = db.session_meta().unwrap();
        assert!(!meta.name.is_empty(), "session name should be non-empty");
        assert!(meta.created_at_ms > 0, "created_at should be positive");
        println!(
            "path={} name={} created_at={}",
            db_path.display(),
            meta.name,
            meta.created_at_ms
        );
    }

    #[test]
    #[ignore = "requires Cursor installation"]
    fn test_real_root_blob_id() {
        let Some(db_path) = find_any_cursor_db() else {
            println!("no cursor DB found, skipping");
            return;
        };
        let db = CursorDb::open(&db_path).unwrap();
        let root_id = db.latest_root_blob_id().unwrap();
        assert_eq!(root_id.len(), 64, "root blob ID should be 64-char hex");
        println!("path={} root_id={root_id}", db_path.display());
    }
}
