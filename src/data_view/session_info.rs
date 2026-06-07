use std::path::PathBuf;

use crate::providers::{ProviderKind, TranscriptEntry};
use crate::tree_scroll_view::{MessageState, MessageType};

/// Build a flat list of label+value node pairs from a `TranscriptEntry`.
pub fn build_session_info_nodes(entry: &TranscriptEntry) -> Vec<MessageState> {
    let mut nodes = Vec::new();

    nodes.push(info_row("Agent", entry.provider.display_name()));
    nodes.push(info_row("Session ID", &entry.id));
    nodes.push(info_row("Title", &entry.title));

    let workspace = entry
        .workspace_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "—".to_string());
    nodes.push(info_row("Workspace path", &workspace));

    nodes.push(info_row("Session path", &entry.path.to_string_lossy()));

    if let Some(jsonl) = cursor_jsonl_path(entry) {
        nodes.push(info_row(
            "Readable transcript path",
            &jsonl.to_string_lossy(),
        ));
    }

    nodes.push(info_row(
        "Last modified",
        &entry.mtime.format("%Y-%m-%d %H:%M:%S").to_string(),
    ));
    if let Some(updated_at) = entry.updated_at {
        nodes.push(info_row(
            "Updated at",
            &updated_at.format("%Y-%m-%d %H:%M:%S").to_string(),
        ));
    }
    if let Some(size) = entry.size {
        nodes.push(info_row("File size", &format_size(size)));
    }
    nodes.push(info_row("Message count", &entry.message_count.to_string()));

    if let Some(ref msg) = entry.last_user_message {
        nodes.push(info_row("Last user message", msg));
    }

    nodes
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Build a label node whose single child is the value node.
fn info_row(label: &str, value: &str) -> MessageState {
    let id_base = label.to_lowercase().replace(' ', "_");
    let child = MessageState::new(format!("info.{id_base}.value"))
        .text(value.to_string())
        .data("value".to_string())
        .message_type(MessageType::Other)
        .tag("value");
    MessageState::new(format!("info.{id_base}"))
        .text(label.to_string())
        .data("label".to_string())
        .message_type(MessageType::Other)
        .tag("label")
        .children(vec![child])
        .expanded(true)
}

/// Derive the Cursor JSONL transcript path from workspace_path + session id.
///
/// Cursor writes `~/.cursor/projects/{encoded_workspace}/agent-transcripts/{id}/{id}.jsonl`
/// where `encoded_workspace` strips the leading `/` and replaces `/` with `-`.
fn cursor_jsonl_path(entry: &TranscriptEntry) -> Option<PathBuf> {
    if entry.provider != ProviderKind::Cursor {
        return None;
    }
    let workspace_path = entry.workspace_path.as_ref()?;
    let home = std::env::var("HOME").ok()?;
    let encoded = workspace_path
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-");
    Some(PathBuf::from(format!(
        "{home}/.cursor/projects/{encoded}/agent-transcripts/{id}/{id}.jsonl",
        id = entry.id,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(provider: ProviderKind, workspace: Option<&str>) -> TranscriptEntry {
        TranscriptEntry {
            path: std::path::PathBuf::from("/some/path/store.db"),
            id: "test-id-123".to_string(),
            title: "Test session".to_string(),
            mtime: chrono::Local::now(),
            updated_at: None,
            size: Some(102_400),
            last_user_message: Some("hello".to_string()),
            message_count: 42,
            workspace_path: workspace.map(std::path::PathBuf::from),
            provider,
        }
    }

    #[test]
    fn session_info_nodes_label_value_structure() {
        let entry = make_entry(ProviderKind::Claude, Some("/home/user/proj"));
        let nodes = build_session_info_nodes(&entry);
        assert!(!nodes.is_empty());
        for node in &nodes {
            assert_eq!(node.message_type, MessageType::Other);
            assert_eq!(node.tag.as_deref(), Some("label"));
            assert_eq!(node.data, "label");
            assert_eq!(node.children.len(), 1);
            let child = &node.children[0];
            assert_eq!(child.message_type, MessageType::Other);
            assert_eq!(child.tag.as_deref(), Some("value"));
            assert_eq!(child.data, "value");
        }
    }

    #[test]
    fn session_info_nodes_required_fields_present() {
        let entry = make_entry(ProviderKind::Claude, Some("/home/user/proj"));
        let nodes = build_session_info_nodes(&entry);
        let labels: Vec<_> = nodes
            .iter()
            .map(|n| n.text.as_deref().unwrap_or(""))
            .collect();
        assert!(labels.contains(&"Agent"));
        assert!(labels.contains(&"Session ID"));
        assert!(labels.contains(&"Title"));
        assert!(labels.contains(&"Workspace path"));
        assert!(labels.contains(&"Session path"));
        assert!(labels.contains(&"Last modified"));
        assert!(labels.contains(&"Message count"));
        assert!(labels.contains(&"Last user message"));
    }

    #[test]
    fn session_info_nodes_last_user_message_omitted_when_none() {
        let mut entry = make_entry(ProviderKind::Claude, None);
        entry.last_user_message = None;
        let nodes = build_session_info_nodes(&entry);
        let labels: Vec<_> = nodes
            .iter()
            .map(|n| n.text.as_deref().unwrap_or(""))
            .collect();
        assert!(!labels.contains(&"Last user message"));
    }

    #[test]
    fn cursor_jsonl_path_with_workspace() {
        let entry = make_entry(ProviderKind::Cursor, Some("/home/agent/workspaces/nexus"));
        let path = cursor_jsonl_path(&entry).expect("should derive path");
        let s = path.to_string_lossy();
        assert!(s.contains("home-agent-workspaces-nexus"), "got: {s}");
        assert!(
            s.contains("agent-transcripts/test-id-123/test-id-123.jsonl"),
            "got: {s}"
        );
    }

    #[test]
    fn cursor_jsonl_path_none_without_workspace() {
        let entry = make_entry(ProviderKind::Cursor, None);
        assert!(cursor_jsonl_path(&entry).is_none());
    }

    #[test]
    fn cursor_jsonl_path_none_for_claude() {
        let entry = make_entry(ProviderKind::Claude, Some("/home/agent/workspaces/proj"));
        assert!(cursor_jsonl_path(&entry).is_none());
    }

    #[test]
    fn session_info_nodes_cursor_has_readable_path() {
        let entry = make_entry(ProviderKind::Cursor, Some("/home/agent/workspaces/nexus"));
        let nodes = build_session_info_nodes(&entry);
        let labels: Vec<_> = nodes
            .iter()
            .map(|n| n.text.as_deref().unwrap_or(""))
            .collect();
        assert!(labels.contains(&"Readable transcript path"));
    }

    #[test]
    fn session_info_nodes_claude_no_readable_path() {
        let entry = make_entry(ProviderKind::Claude, Some("/home/agent/workspaces/proj"));
        let nodes = build_session_info_nodes(&entry);
        let labels: Vec<_> = nodes
            .iter()
            .map(|n| n.text.as_deref().unwrap_or(""))
            .collect();
        assert!(!labels.contains(&"Readable transcript path"));
    }
}
