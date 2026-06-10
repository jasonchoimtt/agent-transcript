pub mod jsonl;
pub mod message;
mod path;
mod reader;
mod subagent;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::info;

use crate::reader_op::ReaderOp;

use super::{
    LoadConfig, Provider, ProviderKind, TranscriptCacheKey, TranscriptEntry, TranscriptReader,
};

// ── JSONL record types for read_entry ────────────────────────────────────────

#[derive(Deserialize)]
struct UserMessage {
    content: Option<MessageContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum JournalRecord {
    User {
        cwd: Option<String>,
        message: Option<UserMessage>,
        #[serde(rename = "toolUseResult")]
        tool_use_result: Option<serde_json::Value>,
    },
    Assistant,
    AiTitle {
        #[serde(rename = "aiTitle")]
        ai_title: String,
    },
    CustomTitle {
        title: String,
    },
    #[serde(other)]
    Other,
}

fn extract_message_text(msg: &UserMessage) -> Option<String> {
    match msg.content.as_ref()? {
        MessageContent::Text(s) if !s.is_empty() => Some(s.clone()),
        MessageContent::Blocks(blocks) => {
            let text: String = blocks
                .iter()
                .filter(|b| b.block_type == "text")
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

/// Encode a workspace path to the directory name Claude uses under `~/.claude/projects/`.
/// Claude replaces every character that is not alphanumeric or `-` with `-`.
fn encode_workspace(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Find a Claude session JSONL by its stem (session UUID) by scanning all project
/// directories under `~/.claude/projects/`.  Returns the path to the `.jsonl` file.
pub fn find_session(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let projects_dir = PathBuf::from(format!("{}/.claude/projects", home));
    let target_name = format!("{}.jsonl", session_id);

    let Ok(project_dirs) = std::fs::read_dir(&projects_dir) else {
        return None;
    };
    for proj_entry in project_dirs.flatten() {
        let candidate = proj_entry.path().join(&target_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

pub struct ClaudeProvider;

#[async_trait]
impl Provider for ClaudeProvider {
    fn scan_paths(&self, cwd: Option<&Path>) -> Vec<(PathBuf, TranscriptCacheKey, i64)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let projects_dir = PathBuf::from(format!("{}/.claude/projects", home));

        let project_dirs: Vec<PathBuf> = if let Some(cwd) = cwd {
            let encoded = encode_workspace(cwd);
            let candidate = projects_dir.join(&encoded);
            if candidate.is_dir() {
                vec![candidate]
            } else {
                vec![]
            }
        } else {
            std::fs::read_dir(&projects_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        };

        let mut result = Vec::new();
        for proj_dir in project_dirs {
            let Ok(files) = std::fs::read_dir(&proj_dir) else {
                continue;
            };
            for file_entry in files.flatten() {
                let path = file_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                // Skip sub-agent JSONL files (agent-<id>.jsonl inside subagents/)
                if path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("agent-"))
                {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                let Ok(mtime) = meta.modified() else {
                    continue;
                };
                let priority = -(mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0));
                result.push((path, TranscriptCacheKey::Mtime(mtime), priority));
            }
        }
        result
    }

    fn find_transcript_path(
        &self,
        session_id: &str,
        _workspace_path: Option<&Path>,
    ) -> Option<PathBuf> {
        find_session(session_id)
    }

    fn compute_transcript_path(
        &self,
        session_id: &str,
        workspace_path: Option<&Path>,
    ) -> Option<PathBuf> {
        let wp = workspace_path?;
        let home = std::env::var("HOME").unwrap_or_default();
        Some(PathBuf::from(format!(
            "{}/.claude/projects/{}/{}.jsonl",
            home,
            encode_workspace(wp),
            session_id
        )))
    }

    fn read_entry(&self, path: &Path) -> Option<TranscriptEntry> {
        let meta = std::fs::metadata(path).ok();
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Local>::from)
            .unwrap_or_else(chrono::Local::now);
        let size = meta.as_ref().map(|m| m.len());

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let file = std::fs::File::open(path).ok()?;
        let reader = BufReader::new(file);

        let mut workspace_path: Option<PathBuf> = None;
        let mut title: Option<String> = None;
        let mut last_user_message: Option<String> = None;
        let mut message_count: usize = 0;

        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(&line) {
                Ok(JournalRecord::User {
                    cwd,
                    message,
                    tool_use_result,
                }) => {
                    message_count += 1;
                    if workspace_path.is_none()
                        && let Some(c) = cwd
                    {
                        workspace_path = Some(PathBuf::from(c));
                    }
                    if tool_use_result.is_none()
                        && let Some(msg) = &message
                        && let Some(text) = extract_message_text(msg)
                    {
                        last_user_message = Some(text);
                    }
                }
                Ok(JournalRecord::Assistant) => {
                    message_count += 1;
                }
                Ok(JournalRecord::AiTitle { ai_title }) => {
                    title = Some(ai_title);
                }
                Ok(JournalRecord::CustomTitle { title: t }) => {
                    title = Some(t);
                }
                Ok(JournalRecord::Other) | Err(_) => {}
            }
        }

        Some(TranscriptEntry {
            path: path.to_owned(),
            id,
            title: title.unwrap_or_else(|| "(untitled)".to_string()),
            mtime,
            updated_at: None,
            size,
            last_user_message,
            message_count,
            workspace_path,
            provider: ProviderKind::Claude,
        })
    }

    async fn open_reader(
        &self,
        path: &Path,
        config: LoadConfig,
    ) -> color_eyre::Result<Box<dyn TranscriptReader>> {
        let jsonl_path = path.to_owned();
        let snapshot = config.snapshot;
        let waterfall = config.waterfall;
        let initial_loaded = config.initial_loaded;
        info!(path = %jsonl_path.display(), snapshot, waterfall, "opening claude reader");
        let (tx, rx) = mpsc::channel::<color_eyre::Result<ReaderOp>>(256);

        tokio::spawn(async move {
            reader::claude_reader_task(jsonl_path, tx, snapshot, waterfall, initial_loaded).await;
        });

        Ok(Box::new(reader::ClaudeTranscriptReader::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn encode_workspace_replaces_non_alphanumeric() {
        // Slashes, underscores, and multi-byte Unicode chars all become '-'.
        let path = Path::new("/home/user/abc_你好");
        assert_eq!(encode_workspace(path), "-home-user-abc---");
    }

    #[test]
    #[ignore = "requires Claude installation at ~/.claude"]
    fn test_scan_and_read_real() {
        let provider = ClaudeProvider;
        let paths = provider.scan_paths(None);
        assert!(!paths.is_empty(), "should find at least one transcript");
        // No subagent files
        for (path, _, _) in &paths {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            assert!(
                !stem.starts_with("agent-"),
                "subagent files must not appear"
            );
        }
        // read_entry works on the first path
        let entry = provider.read_entry(&paths[0].0).unwrap();
        println!("title: {}", entry.title);
        println!("workspace_path: {:?}", entry.workspace_path);
        println!("message_count: {}", entry.message_count);
        assert!(!entry.title.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Claude installation at ~/.claude"]
    async fn test_snapshot_read_real() {
        let provider = ClaudeProvider;
        let paths = provider.scan_paths(None);
        assert!(!paths.is_empty());
        let entries: Vec<_> = paths
            .iter()
            .filter_map(|(p, _, _)| provider.read_entry(p))
            .collect();
        assert!(!entries.is_empty());

        let entry = &entries[0];
        let config = LoadConfig {
            initial_loaded: 0,
            waterfall: false,
            snapshot: true,
        };
        let mut reader = provider.open_reader(&entry.path, config).await.unwrap();
        let rx = reader.updates();

        let mut ops = Vec::new();
        while let Ok(Ok(op)) = rx.try_recv() {
            ops.push(op);
        }
        // Wait a bit for the async task to produce ops
        tokio::time::sleep(Duration::from_millis(200)).await;
        while let Ok(Ok(op)) = rx.try_recv() {
            ops.push(op);
        }

        assert!(
            !ops.is_empty(),
            "snapshot read should produce at least one op"
        );
        let has_user = ops.iter().any(|op| match op {
            crate::reader_op::ReaderOp::Tree(crate::tree_operation::TreeOperation::Append {
                message,
                ..
            }) => message.message_type == crate::tree_scroll_view::state::MessageType::UserMessage,
            _ => false,
        });
        assert!(has_user, "should have at least one UserMessage");
    }
}
