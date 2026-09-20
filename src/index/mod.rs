use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use rusqlite::{Connection, params};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::event::{AppEvent, Event};
use crate::providers::{Provider, ProviderKind, TranscriptCacheKey, TranscriptEntry};

const REFRESH_BATCH_SIZE: usize = 10;

/// Bump whenever the `transcripts` table layout changes. The table is a pure cache, so an
/// index with a different `user_version` is dropped and rebuilt on the next refresh.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA_SQL: &str = "PRAGMA journal_mode = WAL;
     CREATE TABLE IF NOT EXISTS transcripts (
         path              TEXT    PRIMARY KEY,
         provider          TEXT    NOT NULL,
         id                TEXT    NOT NULL,
         title             TEXT    NOT NULL,
         mtime_secs        INTEGER NOT NULL,
         mtime_nanos       INTEGER NOT NULL,
         updated_at_ms     INTEGER,
         size              INTEGER,
         last_user_message TEXT,
         message_count     INTEGER NOT NULL DEFAULT 0,
         workspace_path    TEXT,
         is_subagent       INTEGER NOT NULL DEFAULT 0
     ) STRICT;
     CREATE INDEX IF NOT EXISTS idx_transcripts_mtime ON transcripts (mtime_secs DESC);";

/// Spawn a background task that scans all providers, processes entries in mtime-descending
/// order (newest first), and sends batches of up to `REFRESH_BATCH_SIZE` entries back via
/// the app event channel.
///
/// The task opens its own SQLite connection (rusqlite::Connection is not Send) and exits
/// early if the event channel is closed.
pub fn start_refresh(
    providers: Arc<Vec<Box<dyn Provider>>>,
    cwd: Option<PathBuf>,
    event_tx: UnboundedSender<Event>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let index = match TranscriptIndex::open() {
            Ok(idx) => idx,
            Err(e) => {
                warn!("picker index open failed: {e}");
                let _ = event_tx.send(Event::App(AppEvent::PickerDone));
                return;
            }
        };
        do_refresh(&index, providers, cwd, event_tx);
    })
}

fn do_refresh(
    index: &TranscriptIndex,
    providers: Arc<Vec<Box<dyn Provider>>>,
    cwd: Option<PathBuf>,
    event_tx: UnboundedSender<Event>,
) {
    // Collect (path, cache_key, priority, provider_index) across all providers.
    let mut all_paths: Vec<(PathBuf, TranscriptCacheKey, i64, usize)> = providers
        .iter()
        .enumerate()
        .flat_map(|(i, p)| {
            p.scan_paths(cwd.as_deref())
                .into_iter()
                .map(move |(path, key, priority)| (path, key, priority, i))
        })
        .collect();

    // Ascending priority: more-negative values (newer entries) come first.
    // Priority 0 (WAL-active Cursor sessions) sorts last and is always re-read.
    all_paths.sort_by_key(|(_, _, priority, _)| *priority);

    let total = all_paths.len();
    debug!(total, "picker refresh: paths collected");

    let (mut cache_hits, mut cache_misses, mut force_rereads) = (0usize, 0usize, 0usize);

    // Channel shared by the main loop (cache hits) and rayon workers (reads).
    // Each message carries the original mtime-sorted index so results can be
    // re-assembled in order regardless of which worker finishes first.
    let (result_tx, result_rx) = std::sync::mpsc::channel::<(usize, Option<TranscriptEntry>)>();

    // Indices that came from an actual read (WAL or miss) and need index.upsert.
    // Cache hits are already in the DB with the correct mtime — never re-upsert them.
    let mut needs_upsert = HashSet::<usize>::new();

    for (chunk_idx, chunk) in all_paths.chunks(REFRESH_BATCH_SIZE).enumerate() {
        let base_idx = chunk_idx * REFRESH_BATCH_SIZE;
        let cached = index.lookup_cached_batch(chunk);

        for (i, (path, _, priority, provider_idx)) in chunk.iter().enumerate() {
            let idx = base_idx + i;
            // Priority 0 means the provider wants a forced re-read (WAL active).
            let force_reread = *priority == 0;
            let cached_entry = if force_reread {
                None
            } else {
                cached.get(path).cloned()
            };

            if let Some(entry) = cached_entry {
                cache_hits += 1;
                debug!(path = %path.display(), "picker refresh: cache hit");
                result_tx.send((idx, Some(entry))).ok();
            } else {
                if force_reread {
                    force_rereads += 1;
                    debug!(path = %path.display(), "picker refresh: WAL active — force reread");
                } else {
                    cache_misses += 1;
                    debug!(path = %path.display(), "picker refresh: cache miss — reading");
                }
                needs_upsert.insert(idx);
                let tx = result_tx.clone();
                let path = path.clone();
                let provider = Arc::clone(&providers);
                let provider_idx = *provider_idx;
                rayon::spawn(move || {
                    let entry = provider[provider_idx].read_entry(&path);
                    tx.send((idx, entry)).ok();
                });
            }
        }
    }
    // Drop the main-loop sender so the channel closes once all rayon tasks finish.
    drop(result_tx);

    // Reorder buffer: emit entries in original priority-sorted order.
    // An entry at index i is only forwarded to the UI once all entries 0..i are ready,
    // ensuring the list never reorders under the user.
    let mut buffer: HashMap<usize, Option<TranscriptEntry>> = HashMap::new();
    let mut next_emit: usize = 0;
    let mut batch: Vec<TranscriptEntry> = Vec::with_capacity(REFRESH_BATCH_SIZE);
    let mut app_exited = false;

    'recv: for (idx, entry) in result_rx {
        if needs_upsert.contains(&idx)
            && let Some(e) = &entry
            && let Err(err) = index.upsert(e, &all_paths[idx].1)
        {
            warn!(
                "index upsert failed for {}: {err}",
                all_paths[idx].0.display()
            );
        }
        buffer.insert(idx, entry);
        while let Some(e) = buffer.remove(&next_emit) {
            next_emit += 1;
            if let Some(e) = e
                && !e.is_subagent
            {
                batch.push(e);
                if batch.len() >= REFRESH_BATCH_SIZE
                    && event_tx
                        .send(Event::App(AppEvent::PickerEntries {
                            entries: std::mem::take(&mut batch),
                        }))
                        .is_err()
                {
                    app_exited = true;
                    break 'recv;
                }
            }
        }
    }

    debug!(
        total,
        cache_hits, cache_misses, force_rereads, "picker refresh: done"
    );

    if app_exited {
        return;
    }

    // Flush any entries still sitting in the reorder buffer (can happen when the
    // tail of all_paths consists entirely of reads that arrived out of order).
    while let Some(e) = buffer.remove(&next_emit) {
        next_emit += 1;
        if let Some(e) = e
            && !e.is_subagent
        {
            batch.push(e);
        }
    }

    if !batch.is_empty()
        && event_tx
            .send(Event::App(AppEvent::PickerEntries { entries: batch }))
            .is_err()
    {
        return;
    }

    let _ = event_tx.send(Event::App(AppEvent::PickerDone));
}

pub struct TranscriptIndex {
    conn: Connection,
}

impl TranscriptIndex {
    pub fn open() -> color_eyre::Result<Self> {
        let home = std::env::var("HOME").unwrap_or_default();
        let dir = PathBuf::from(format!("{}/.local/share/agent-transcript", home));
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("index.db");
        let conn = Connection::open(&db_path)?;
        init_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Fetch cached entries for a chunk of paths in a single query.
    /// Returns only entries whose stored cache key exactly matches the scan-time key —
    /// mismatches are silently dropped, leaving them as cache misses for the caller.
    fn lookup_cached_batch(
        &self,
        chunk: &[(PathBuf, TranscriptCacheKey, i64, usize)],
    ) -> HashMap<PathBuf, TranscriptEntry> {
        use rusqlite::types::Value as SqlValue;

        if chunk.is_empty() {
            return HashMap::new();
        }

        // Expected cache key per path for validation after the query.
        let expected: HashMap<String, &TranscriptCacheKey> = chunk
            .iter()
            .map(|(p, key, _, _)| (p.to_string_lossy().into_owned(), key))
            .collect();

        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT path, provider, id, title, mtime_secs, mtime_nanos, \
             last_user_message, message_count, workspace_path, updated_at_ms, size, \
             is_subagent \
             FROM transcripts \
             WHERE path IN ({placeholders})"
        );

        let params: Vec<SqlValue> = chunk
            .iter()
            .map(|(p, _, _, _)| SqlValue::Text(p.to_string_lossy().into_owned()))
            .collect();

        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                warn!("lookup_cached_batch prepare failed: {e}");
                return HashMap::new();
            }
        };

        let rows = match stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, i64>(11)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!("lookup_cached_batch query failed: {e}");
                return HashMap::new();
            }
        };

        let mut result = HashMap::new();
        for row in rows.flatten() {
            let (
                path_str,
                provider_str,
                id,
                title,
                db_secs,
                db_nanos,
                last_user_message,
                message_count,
                workspace_path_str,
                updated_at_ms,
                db_size,
                is_subagent,
            ) = row;

            // Validate stored cache key against the scan-time key.
            let is_hit = match expected.get(&path_str) {
                Some(TranscriptCacheKey::Mtime(t)) => {
                    let (exp_secs, exp_nanos) = system_time_to_parts(*t);
                    db_secs == exp_secs as i64 && db_nanos == exp_nanos as i64
                }
                Some(TranscriptCacheKey::Size(s)) => db_size == Some(*s as i64),
                None => false,
            };
            if !is_hit {
                continue;
            }

            let path = PathBuf::from(&path_str);
            let mtime =
                SystemTime::UNIX_EPOCH + std::time::Duration::new(db_secs as u64, db_nanos as u32);
            let provider = provider_str.parse().unwrap_or(ProviderKind::Claude);
            let updated_at = updated_at_ms
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|dt| dt.with_timezone(&chrono::Local));

            result.insert(
                path.clone(),
                TranscriptEntry {
                    path,
                    id,
                    title,
                    mtime: chrono::DateTime::<chrono::Local>::from(mtime),
                    updated_at,
                    size: db_size.map(|s| s as u64),
                    last_user_message,
                    message_count: message_count as usize,
                    workspace_path: workspace_path_str.map(PathBuf::from),
                    provider,
                    is_subagent: is_subagent != 0,
                },
            );
        }

        result
    }

    fn upsert(
        &self,
        entry: &TranscriptEntry,
        cache_key: &TranscriptCacheKey,
    ) -> color_eyre::Result<()> {
        // For Mtime keys, store the exact scanned mtime so future lookups can validate it.
        // For Size keys, fall back to the entry's filesystem mtime (informational only).
        let (secs, nanos) = match cache_key {
            TranscriptCacheKey::Mtime(t) => {
                let (s, n) = system_time_to_parts(*t);
                (s as i64, n as i64)
            }
            TranscriptCacheKey::Size(_) => {
                let (s, n) = system_time_to_parts(SystemTime::from(entry.mtime));
                (s as i64, n as i64)
            }
        };
        // For Size keys, use the scanned size as the authoritative value.
        let size = match cache_key {
            TranscriptCacheKey::Size(s) => Some(*s as i64),
            TranscriptCacheKey::Mtime(_) => entry.size.map(|s| s as i64),
        };
        let updated_at_ms = entry.updated_at.map(|dt| dt.timestamp_millis());
        self.conn.execute(
            "INSERT OR REPLACE INTO transcripts
             (path, provider, id, title, mtime_secs, mtime_nanos,
              updated_at_ms, size, last_user_message, message_count, workspace_path,
              is_subagent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                entry.path.to_string_lossy().as_ref(),
                entry.provider.to_string(),
                entry.id,
                entry.title,
                secs,
                nanos,
                updated_at_ms,
                size,
                entry.last_user_message.as_deref(),
                entry.message_count as i64,
                entry
                    .workspace_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                entry.is_subagent as i64,
            ],
        )?;
        Ok(())
    }
}

/// Create the schema, first dropping a `transcripts` table left by a different
/// `SCHEMA_VERSION` (`CREATE TABLE IF NOT EXISTS` would otherwise keep its old columns).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch("DROP TABLE IF EXISTS transcripts;")?;
    }
    conn.execute_batch(SCHEMA_SQL)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
}

fn system_time_to_parts(t: SystemTime) -> (u64, u32) {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use std::path::Path;

    use super::*;
    use crate::providers::ProviderKind;

    fn make_index() -> TranscriptIndex {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        TranscriptIndex { conn }
    }

    fn make_providers(jsonl: &Path, count: Arc<AtomicUsize>) -> Arc<Vec<Box<dyn Provider>>> {
        make_providers_with(jsonl, count, false)
    }

    fn make_providers_with(
        jsonl: &Path,
        count: Arc<AtomicUsize>,
        is_subagent: bool,
    ) -> Arc<Vec<Box<dyn Provider>>> {
        Arc::new(vec![Box::new(CountingProvider {
            jsonl_path: jsonl.to_path_buf(),
            call_count: count,
            is_subagent,
        }) as Box<dyn Provider>])
    }

    /// A provider that counts how many times read_entry is called.
    struct CountingProvider {
        jsonl_path: PathBuf,
        call_count: Arc<AtomicUsize>,
        is_subagent: bool,
    }

    #[async_trait::async_trait]
    impl Provider for CountingProvider {
        fn scan_paths(&self, _cwd: Option<&Path>) -> Vec<(PathBuf, TranscriptCacheKey, i64)> {
            let mtime = std::fs::metadata(&self.jsonl_path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let priority = -(mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0));
            vec![(
                self.jsonl_path.clone(),
                TranscriptCacheKey::Mtime(mtime),
                priority,
            )]
        }

        fn read_entry(&self, path: &Path) -> Option<TranscriptEntry> {
            self.call_count.fetch_add(1, SeqCst);
            Some(TranscriptEntry {
                path: path.to_owned(),
                id: "test-id".into(),
                title: "Test".into(),
                mtime: chrono::Local::now(),
                updated_at: None,
                size: None,
                last_user_message: None,
                message_count: 1,
                workspace_path: None,
                provider: ProviderKind::Claude,
                is_subagent: self.is_subagent,
            })
        }

        async fn open_reader(
            &self,
            _path: &Path,
            _config: crate::providers::LoadConfig,
        ) -> color_eyre::Result<Box<dyn crate::providers::TranscriptReader>> {
            unimplemented!()
        }
    }

    // do_refresh is synchronous: it blocks on the std::sync::mpsc recv loop until all
    // rayon tasks complete, so tests can call it directly without a Tokio runtime.
    // The event receiver must stay alive for the duration of the call; dropping it early
    // would close the channel and cause do_refresh to exit via the app_exited path.

    #[test]
    fn second_refresh_uses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("session.jsonl");
        std::fs::write(&jsonl, b"{\"type\":\"user\",\"cwd\":\"/tmp\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();

        let index = make_index();

        let count = Arc::new(AtomicUsize::new(0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(&index, make_providers(&jsonl, count.clone()), None, tx);
        assert_eq!(count.load(SeqCst), 1, "first refresh reads");

        // Same file, same mtime — second refresh must hit the cache.
        let count2 = Arc::new(AtomicUsize::new(0));
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(&index, make_providers(&jsonl, count2.clone()), None, tx2);
        assert_eq!(
            count2.load(SeqCst),
            0,
            "second refresh with same mtime should not re-read"
        );
    }

    #[test]
    fn changed_mtime_triggers_reread() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("session.jsonl");
        std::fs::write(&jsonl, b"{\"type\":\"user\",\"cwd\":\"/tmp\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();

        let index = make_index();

        let count = Arc::new(AtomicUsize::new(0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(&index, make_providers(&jsonl, count.clone()), None, tx);

        // Touch the file to bump its mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .unwrap();
        writeln!(f, "{{\"type\":\"assistant\"}}").unwrap();
        drop(f);

        let count2 = Arc::new(AtomicUsize::new(0));
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(&index, make_providers(&jsonl, count2.clone()), None, tx2);
        assert_eq!(
            count2.load(SeqCst),
            1,
            "changed mtime should trigger re-read"
        );
    }

    /// Collect every entry sent to the picker by a finished refresh.
    fn picker_entries(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    ) -> Vec<TranscriptEntry> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::App(AppEvent::PickerEntries { entries }) = ev {
                out.extend(entries);
            }
        }
        out
    }

    #[test]
    fn subagent_entries_are_cached_but_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("session.jsonl");
        std::fs::write(&jsonl, b"{}\n").unwrap();

        let index = make_index();

        let count = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(
            &index,
            make_providers_with(&jsonl, count.clone(), true),
            None,
            tx,
        );
        assert_eq!(count.load(SeqCst), 1, "first refresh reads");
        assert!(
            picker_entries(&mut rx).is_empty(),
            "subagent must not be listed"
        );

        // The flag round-trips through the cache: no re-read, still hidden.
        let count2 = Arc::new(AtomicUsize::new(0));
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(
            &index,
            make_providers_with(&jsonl, count2.clone(), true),
            None,
            tx2,
        );
        assert_eq!(
            count2.load(SeqCst),
            0,
            "subagent entry should be served from cache"
        );
        assert!(
            picker_entries(&mut rx2).is_empty(),
            "cached subagent must not be listed"
        );
    }

    #[test]
    fn non_subagent_entries_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("session.jsonl");
        std::fs::write(&jsonl, b"{}\n").unwrap();

        let index = make_index();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        do_refresh(
            &index,
            make_providers(&jsonl, Arc::new(AtomicUsize::new(0))),
            None,
            tx,
        );
        assert_eq!(picker_entries(&mut rx).len(), 1);
    }

    #[test]
    fn outdated_schema_is_rebuilt() {
        let conn = Connection::open_in_memory().unwrap();
        // Pre-versioning layout: no is_subagent column, user_version 0.
        conn.execute_batch(
            "CREATE TABLE transcripts (path TEXT PRIMARY KEY, provider TEXT NOT NULL);
             INSERT INTO transcripts VALUES ('/old', 'claude');",
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let has_column: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('transcripts') WHERE name = 'is_subagent'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        assert!(has_column, "rebuilt table should have is_subagent");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcripts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "stale cache rows should be dropped");

        // Re-opening at the current version keeps the data.
        conn.execute("INSERT INTO transcripts (path, provider, id, title, mtime_secs, mtime_nanos) VALUES ('/new', 'claude', 'x', 't', 0, 0)", [])
            .unwrap();
        init_schema(&conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcripts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "same-version reopen must not drop the cache");
    }
}
