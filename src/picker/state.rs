use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::widgets::ListState;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::event::Event;
use crate::index;
use crate::providers::{
    Provider, ProviderKind, TranscriptEntry, claude::ClaudeProvider, cursor::CursorProvider,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    All,
    Cursor,
    Claude,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::All, Tab::Cursor, Tab::Claude];

    pub fn label(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Cursor => "Cursor",
            Tab::Claude => "Claude",
        }
    }
}

pub struct PickerState {
    pub tab: Tab,
    all_entries: Vec<TranscriptEntry>,
    pub filtered: Vec<TranscriptEntry>,
    pub list_state: ListState,
    pub show_all: bool,
    pub is_loading: bool,
    pub cwd: Option<PathBuf>,
    pub flash_message: Option<(String, std::time::Instant)>,
    /// Height of the list area in rows, updated each render frame.
    pub list_height: u16,
    refresh_task: Option<JoinHandle<()>>,
}

impl Default for PickerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PickerState {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().ok();
        let mut state = Self {
            tab: Tab::All,
            all_entries: vec![],
            filtered: vec![],
            list_state: ListState::default(),
            show_all: false,
            is_loading: true,
            cwd,
            flash_message: None,
            list_height: 0,
            refresh_task: None,
        };
        state.apply_filter();
        state
    }

    pub fn default_providers() -> Arc<Vec<Box<dyn Provider>>> {
        Arc::new(vec![Box::new(ClaudeProvider), Box::new(CursorProvider)])
    }

    /// Start a background index refresh, aborting any previous one.
    pub(crate) fn start_refresh(
        &mut self,
        providers: Arc<Vec<Box<dyn Provider>>>,
        cwd: Option<PathBuf>,
        event_tx: UnboundedSender<Event>,
    ) {
        if let Some(h) = self.refresh_task.take() {
            h.abort();
        }
        self.refresh_task = Some(index::start_refresh(providers, cwd, event_tx));
    }

    /// Abort the in-flight refresh task, if any.
    pub(crate) fn abort_refresh(&mut self) {
        if let Some(h) = self.refresh_task.take() {
            h.abort();
        }
    }

    /// Merge a batch of entries received from a background refresh task.
    /// Replaces existing entries by ID, inserts new ones, sorts by mtime
    /// descending, and preserves the current selection by entry ID.
    pub fn append_entries(&mut self, entries: Vec<TranscriptEntry>) {
        let new_chat_selected = self.is_new_chat_selected();
        let selected_id = self.selected_entry().map(|e| e.id.clone());

        let mut id_to_idx: HashMap<String, usize> = self
            .all_entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        for entry in entries {
            if let Some(&pos) = id_to_idx.get(&entry.id) {
                self.all_entries[pos] = entry;
            } else {
                id_to_idx.insert(entry.id.clone(), self.all_entries.len());
                self.all_entries.push(entry);
            }
        }
        self.all_entries
            .sort_by_key(|e| std::cmp::Reverse(e.updated_at.unwrap_or(e.mtime)));
        self.rebuild_filtered();

        let new_idx = if new_chat_selected {
            Some(0)
        } else if let Some(id) = selected_id {
            self.filtered
                .iter()
                .position(|e| e.id == id)
                .map(|pos| pos + 1)
                .or(Some(if self.filtered.is_empty() { 0 } else { 1 }))
        } else if !self.filtered.is_empty() {
            Some(1)
        } else {
            Some(0)
        };
        self.list_state.select(new_idx);
    }

    pub fn finish_loading(&mut self) {
        self.is_loading = false;
    }

    pub fn restart_loading(&mut self) {
        self.is_loading = true;
    }

    /// Returns true when the "New chat" virtual item (index 0) is selected.
    pub fn is_new_chat_selected(&self) -> bool {
        self.list_state.selected() == Some(0)
    }

    /// Toggle the show_all flag.  Returns `true` if the caller must start a
    /// background refresh (enabling show_all), or `false` if re-filtering
    /// the existing entries is sufficient (disabling show_all).
    pub fn toggle_show_all(&mut self) -> bool {
        self.show_all = !self.show_all;
        if self.show_all {
            // Discard the workspace-filtered set; the caller will start a full reload.
            self.all_entries.clear();
            self.is_loading = true;
            self.apply_filter();
            true
        } else {
            // Already have the full entry set; just re-filter in place.
            self.is_loading = false;
            self.apply_filter();
            false
        }
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.apply_filter();
    }

    pub fn next_tab(&mut self) {
        let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = Tab::ALL[(idx + 1) % Tab::ALL.len()];
        self.set_tab(next);
    }

    pub fn prev_tab(&mut self) {
        let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        let prev = Tab::ALL[(idx + Tab::ALL.len() - 1) % Tab::ALL.len()];
        self.set_tab(prev);
    }

    pub fn move_down(&mut self) {
        // Total items: 1 (NewChat) + filtered entries.
        let total = 1 + self.filtered.len();
        let selected = self.list_state.selected().unwrap_or(0);
        if selected + 1 < total {
            self.list_state.select(Some(selected + 1));
        }
    }

    pub fn move_up(&mut self) {
        let selected = self.list_state.selected().unwrap_or(0);
        if selected > 0 {
            self.list_state.select(Some(selected - 1));
        }
    }

    pub fn move_to_top(&mut self) {
        self.list_state.select(Some(0));
    }

    pub fn move_to_bottom(&mut self) {
        self.list_state.select(Some(self.filtered.len()));
    }

    pub fn move_half_page_up(&mut self) {
        // Each item is 3 rows; half page = list_height / 6, minimum 1.
        let half = ((self.list_height / 6) as usize).max(1);
        let selected = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some(selected.saturating_sub(half)));
        *self.list_state.offset_mut() = self.list_state.offset().saturating_sub(half);
    }

    pub fn move_half_page_down(&mut self) {
        let half = ((self.list_height / 6) as usize).max(1);
        let total = 1 + self.filtered.len();
        let selected = self.list_state.selected().unwrap_or(0);
        self.list_state
            .select(Some((selected + half).min(total - 1)));
        *self.list_state.offset_mut() += half;
    }

    pub fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash_message = Some((msg.into(), std::time::Instant::now()));
    }

    pub fn tick_flash(&mut self) {
        if let Some((_, since)) = &self.flash_message
            && since.elapsed() >= std::time::Duration::from_secs(5)
        {
            self.flash_message = None;
        }
    }

    /// Returns the selected transcript entry, or `None` when "New chat" (index 0) is selected.
    pub fn selected_entry(&self) -> Option<&TranscriptEntry> {
        let idx = self.list_state.selected()?;
        if idx == 0 {
            None
        } else {
            self.filtered.get(idx - 1)
        }
    }

    /// Rebuild `filtered` and reset selection to index 0 (the "New chat" item).
    fn apply_filter(&mut self) {
        self.rebuild_filtered();
        self.list_state.select(Some(0));
    }

    fn rebuild_filtered(&mut self) {
        let show_all = self.show_all;
        let cwd = &self.cwd;
        let tab = self.tab;
        self.filtered = self
            .all_entries
            .iter()
            .filter(|e| {
                if !show_all
                    && let Some(cwd) = cwd
                    && e.workspace_path.as_ref() != Some(cwd)
                {
                    return false;
                }
                match tab {
                    Tab::All => true,
                    Tab::Cursor => e.provider == ProviderKind::Cursor,
                    Tab::Claude => e.provider == ProviderKind::Claude,
                }
            })
            .cloned()
            .collect();
    }
}
