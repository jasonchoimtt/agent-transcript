use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::tree_scroll_view::state::TreeScrollViewState;

// ── JumpPosition ──────────────────────────────────────────────────────────────

pub struct JumpPosition {
    pub top_index: Vec<usize>,
    pub top_offset: u16,
    pub selection_index: Vec<usize>,
}

// ── JumpList ──────────────────────────────────────────────────────────────────

const JUMP_LIST_CAP: usize = 100;

pub struct JumpList {
    stack: Vec<JumpPosition>,
}

impl Default for JumpList {
    fn default() -> Self {
        Self::new()
    }
}

impl JumpList {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    pub fn push(&mut self, pos: JumpPosition) {
        if self.stack.len() == JUMP_LIST_CAP {
            self.stack.remove(0);
        }
        self.stack.push(pos);
    }

    pub fn pop(&mut self) -> Option<JumpPosition> {
        self.stack.pop()
    }
}

// ── Marks ─────────────────────────────────────────────────────────────────────

pub struct Marks {
    /// char → message ID
    map: HashMap<char, String>,
    /// message ID → char (reverse lookup for O(1) gutter render)
    id_to_mark: HashMap<String, char>,
}

impl Default for Marks {
    fn default() -> Self {
        Self::new()
    }
}

impl Marks {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            id_to_mark: HashMap::new(),
        }
    }

    pub fn set(&mut self, ch: char, id: String) {
        // Remove old reverse entry for this char if present.
        if let Some(old_id) = self.map.get(&ch) {
            self.id_to_mark.remove(old_id.as_str());
        }
        // Remove old char entry for this id if present (id reuse across chars).
        if let Some(old_ch) = self.id_to_mark.get(&id) {
            self.map.remove(old_ch);
        }
        self.id_to_mark.insert(id.clone(), ch);
        self.map.insert(ch, id);
    }

    pub fn get(&self, ch: char) -> Option<&str> {
        self.map.get(&ch).map(String::as_str)
    }

    /// Returns the mark char assigned to `id`, if any.
    pub fn mark_for_id(&self, id: &str) -> Option<char> {
        self.id_to_mark.get(id).copied()
    }

    pub fn delete(&mut self, ch: char) {
        if let Some(id) = self.map.remove(&ch) {
            self.id_to_mark.remove(&id);
        }
    }

    pub fn load(path: &Path) -> color_eyre::Result<Self> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e.into()),
            Ok(json) => {
                let map: HashMap<String, String> = serde_json::from_str(&json)?;
                let mut marks = Self::new();
                for (k, v) in map {
                    let mut chars = k.chars();
                    if let (Some(ch), None) = (chars.next(), chars.next()) {
                        marks.set(ch, v);
                    }
                }
                Ok(marks)
            }
        }
    }

    pub fn save(&self, path: &Path) -> color_eyre::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serializable: HashMap<String, &str> = self
            .map
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_str()))
            .collect();
        let json = serde_json::to_string_pretty(&serializable)?;
        // Atomic write: tmp file + rename.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ── Path helper ───────────────────────────────────────────────────────────────

pub fn marks_path(provider: &str, session_id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{home}/.local/share"));
    PathBuf::from(data_home)
        .join("agent-transcript")
        .join("marks")
        .join(provider)
        .join(format!("{session_id}.json"))
}

// ── TreeScrollViewState impl ──────────────────────────────────────────────────

impl TreeScrollViewState {
    /// Record a mark on the currently selected message.
    pub fn set_mark(&mut self, ch: char) {
        let Some(node) =
            crate::tree_scroll_view::state::get_node(&self.items, &self.selection_index)
        else {
            return;
        };
        if node.is_terminal {
            return;
        }
        self.marks.set(ch, node.id.clone());
    }

    /// Jump to the message marked with `ch`. Returns `false` if the mark is not set
    /// or the target node no longer exists.
    pub fn goto_mark(&mut self, ch: char) -> bool {
        let Some(id) = self.marks.get(ch).map(str::to_owned) else {
            return false;
        };
        let Some(path) = self.id_to_path.get(&id).cloned() else {
            return false;
        };
        self.selection_index = path.clone();
        self.top_index = path;
        self.top_offset = 0;
        self.precedence = crate::tree_scroll_view::state::Precedence::Top;
        true
    }

    /// Push the current viewport position onto the jump list.
    pub fn push_jump(&mut self) {
        self.jump_list.push(JumpPosition {
            top_index: self.top_index.clone(),
            top_offset: self.top_offset,
            selection_index: self.selection_index.clone(),
        });
    }

    /// Pop the most recent jump position and restore it. Returns `false` if the
    /// list is empty.
    pub fn pop_jump(&mut self) -> bool {
        let Some(pos) = self.jump_list.pop() else {
            return false;
        };
        self.top_index = pos.top_index;
        self.top_offset = pos.top_offset;
        self.selection_index = pos.selection_index;
        self.precedence = crate::tree_scroll_view::state::Precedence::Top;
        true
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Marks ──────────────────────────────────────────────────────────────────

    #[test]
    fn marks_set_get_roundtrip() {
        let mut m = Marks::new();
        m.set('a', "id-1".into());
        assert_eq!(m.get('a'), Some("id-1"));
        assert_eq!(m.mark_for_id("id-1"), Some('a'));
    }

    #[test]
    fn marks_uppercase_roundtrip() {
        let mut m = Marks::new();
        m.set('Z', "id-Z".into());
        assert_eq!(m.get('Z'), Some("id-Z"));
        assert_eq!(m.mark_for_id("id-Z"), Some('Z'));
    }

    #[test]
    fn marks_overwrite_char_clears_old_reverse() {
        let mut m = Marks::new();
        m.set('a', "id-1".into());
        m.set('a', "id-2".into());
        assert_eq!(m.get('a'), Some("id-2"));
        assert_eq!(m.mark_for_id("id-1"), None);
        assert_eq!(m.mark_for_id("id-2"), Some('a'));
    }

    #[test]
    fn marks_same_id_reused_for_different_char() {
        // Setting mark 'b' to "id-1" when 'a' already points to "id-1" removes 'a'.
        let mut m = Marks::new();
        m.set('a', "id-1".into());
        m.set('b', "id-1".into());
        assert_eq!(m.get('a'), None);
        assert_eq!(m.get('b'), Some("id-1"));
        assert_eq!(m.mark_for_id("id-1"), Some('b'));
    }

    #[test]
    fn marks_delete_removes_both_maps() {
        let mut m = Marks::new();
        m.set('a', "id-1".into());
        m.delete('a');
        assert_eq!(m.get('a'), None);
        assert_eq!(m.mark_for_id("id-1"), None);
    }

    #[test]
    fn marks_delete_unknown_char_is_noop() {
        let mut m = Marks::new();
        m.delete('z'); // should not panic
    }

    #[test]
    fn marks_unknown_char_returns_none() {
        let m = Marks::new();
        assert_eq!(m.get('x'), None);
        assert_eq!(m.mark_for_id("nope"), None);
    }

    #[test]
    fn marks_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marks.json");

        let mut m = Marks::new();
        m.set('a', "id-1".into());
        m.set('Z', "id-2".into());
        m.save(&path).unwrap();

        let loaded = Marks::load(&path).unwrap();
        assert_eq!(loaded.get('a'), Some("id-1"));
        assert_eq!(loaded.get('Z'), Some("id-2"));
        assert_eq!(loaded.mark_for_id("id-1"), Some('a'));
        assert_eq!(loaded.mark_for_id("id-2"), Some('Z'));
    }

    #[test]
    fn marks_load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let m = Marks::load(&path).unwrap();
        assert_eq!(m.get('a'), None);
    }

    // ── JumpList ───────────────────────────────────────────────────────────────

    #[test]
    fn jump_list_push_pop_roundtrip() {
        let mut jl = JumpList::new();
        jl.push(JumpPosition {
            top_index: vec![1],
            top_offset: 3,
            selection_index: vec![2],
        });
        let pos = jl.pop().unwrap();
        assert_eq!(pos.top_index, vec![1]);
        assert_eq!(pos.top_offset, 3);
        assert_eq!(pos.selection_index, vec![2]);
    }

    #[test]
    fn jump_list_pop_empty_returns_none() {
        let mut jl = JumpList::new();
        assert!(jl.pop().is_none());
    }

    #[test]
    fn jump_list_cap_enforced() {
        let mut jl = JumpList::new();
        for i in 0..=JUMP_LIST_CAP {
            jl.push(JumpPosition {
                top_index: vec![i],
                top_offset: 0,
                selection_index: vec![i],
            });
        }
        assert_eq!(jl.stack.len(), JUMP_LIST_CAP);
        // The first entry (top_index=[0]) should have been evicted.
        let oldest = jl.stack.first().unwrap();
        assert_eq!(oldest.top_index, vec![1]);
    }
}
