pub mod claude;
pub mod cursor;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Determines how a cached index entry is validated against the filesystem.
///
/// Claude sessions use `Mtime` because their JSONL files are append-only and
/// the filesystem mtime is a reliable change signal.  Cursor sessions use `Size`
/// because Cursor periodically touches `store.db` (polluting its mtime), so the
/// file size is a more stable proxy for actual content changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptCacheKey {
    Mtime(SystemTime),
    Size(u64),
}

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::reader_op::ReaderOp;
use crate::terminal::crop::{ClaudeCropDetector, CropDetector, CursorCropDetector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Cursor,
}

impl std::str::FromStr for ProviderKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "cursor" => Ok(Self::Cursor),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claude => write!(f, "claude"),
            Self::Cursor => write!(f, "cursor"),
        }
    }
}

impl ProviderKind {
    pub fn cli_command(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Cursor => "agent",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor Agent",
        }
    }

    pub fn crop_detector(&self) -> Box<dyn CropDetector> {
        match self {
            Self::Claude => Box::new(ClaudeCropDetector),
            Self::Cursor => Box::new(CursorCropDetector),
        }
    }

    pub fn as_provider(&self) -> Box<dyn Provider> {
        match self {
            Self::Claude => Box::new(claude::ClaudeProvider),
            Self::Cursor => Box::new(cursor::CursorProvider),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub path: PathBuf,
    pub id: String,
    pub title: String,
    pub mtime: chrono::DateTime<chrono::Local>,
    pub updated_at: Option<chrono::DateTime<chrono::Local>>,
    pub size: Option<u64>,
    pub last_user_message: Option<String>,
    pub message_count: usize,
    pub workspace_path: Option<PathBuf>,
    pub provider: ProviderKind,
}

pub struct LoadConfig {
    pub initial_loaded: usize,
    /// When true, simulate live streaming by revealing one entry at a time (used by `agt parse --waterfall`).
    pub waterfall: bool,
    /// When true the reader exits after the initial bulk load instead of watching for live updates.
    pub snapshot: bool,
}

pub trait TranscriptReader: Send + Sync {
    fn updates(&mut self) -> &mut mpsc::Receiver<color_eyre::Result<ReaderOp>>;
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Cheap filesystem scan.  Returns `(path, cache_key, priority)` triples.
    ///
    /// `cache_key` determines how the index validates a cached entry (mtime or
    /// file size).  `priority` controls processing order: ascending sort puts
    /// high-priority entries first.  Providers encode recency as `-mtime_secs`
    /// (more negative = newer = higher priority); a special value of `0` signals
    /// "always re-read" (used by Cursor when a WAL file is present).
    fn scan_paths(&self, cwd: Option<&Path>) -> Vec<(PathBuf, TranscriptCacheKey, i64)>;

    /// Read full metadata for a single session path.  Returns None on error.
    fn read_entry(&self, path: &Path) -> Option<TranscriptEntry>;

    /// Locate the transcript file for a session ID.  `workspace_path` is an
    /// optional hint that lets providers that derive storage paths from the
    /// workspace root (e.g. Cursor) skip the glob scan.  Returns `None` if
    /// not found.
    fn find_transcript_path(
        &self,
        _session_id: &str,
        _workspace_path: Option<&Path>,
    ) -> Option<PathBuf> {
        None
    }

    /// Compute the expected transcript path for a session that may not yet
    /// exist on disk.  Unlike `find_transcript_path`, no existence check is
    /// performed — the returned path is passed straight to the reader, which
    /// waits for the file to appear.  Returns `None` when the path cannot be
    /// determined without a filesystem search (e.g. no `workspace_path` hint).
    fn compute_transcript_path(
        &self,
        _session_id: &str,
        _workspace_path: Option<&Path>,
    ) -> Option<PathBuf> {
        None
    }

    async fn open_reader(
        &self,
        path: &Path,
        config: LoadConfig,
    ) -> color_eyre::Result<Box<dyn TranscriptReader>>;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::sync::mpsc;

    use super::*;

    struct MockReader {
        rx: mpsc::Receiver<color_eyre::Result<ReaderOp>>,
    }

    impl TranscriptReader for MockReader {
        fn updates(&mut self) -> &mut mpsc::Receiver<color_eyre::Result<ReaderOp>> {
            &mut self.rx
        }
    }

    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn scan_paths(&self, _cwd: Option<&Path>) -> Vec<(PathBuf, TranscriptCacheKey, i64)> {
            vec![]
        }
        fn read_entry(&self, _path: &Path) -> Option<TranscriptEntry> {
            None
        }
        async fn open_reader(
            &self,
            _path: &Path,
            _config: LoadConfig,
        ) -> color_eyre::Result<Box<dyn TranscriptReader>> {
            let (_tx, rx) = mpsc::channel::<color_eyre::Result<ReaderOp>>(1);
            Ok(Box::new(MockReader { rx }))
        }
    }

    #[test]
    fn transcript_reader_is_object_safe() {
        let (_tx, rx) = mpsc::channel::<color_eyre::Result<ReaderOp>>(1);
        let _boxed: Box<dyn TranscriptReader> = Box::new(MockReader { rx });
    }

    #[test]
    fn provider_is_object_safe() {
        let _boxed: Box<dyn Provider> = Box::new(MockProvider);
    }
}
