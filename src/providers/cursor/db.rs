use std::path::Path;

use color_eyre::eyre::Context;
use rusqlite::{Connection, OpenFlags};

use super::bytes_to_hex;
use super::proto::{extract_field1_blobs, extract_field13_refs};

pub struct CursorDb {
    conn: Connection,
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

    /// Returns (name, created_at_ms).
    pub fn session_meta(&self) -> color_eyre::Result<(String, i64)> {
        let obj = self.read_meta_json()?;
        let name = obj["name"].as_str().unwrap_or("").to_string();
        let created_at = obj["createdAt"].as_i64().unwrap_or(0);
        Ok((name, created_at))
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
        let (name, created_at) = db.session_meta().unwrap();
        assert!(!name.is_empty(), "session name should be non-empty");
        assert!(created_at > 0, "created_at should be positive");
        println!(
            "path={} name={name} created_at={created_at}",
            db_path.display()
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
