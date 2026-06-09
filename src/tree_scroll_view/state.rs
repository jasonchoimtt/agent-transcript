use std::collections::HashMap;

use ansi_to_tui::IntoText;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

use super::ansi::visual_width;
use super::cursor::TreeCursor;
use super::handler::{KeyParser, TreeAction};
use super::markdown::render_markdown;
use super::message_widget::{get_message_component, match_mouse_node};
use super::predicates::nonzero_height;
use crate::reader_op::ReaderOp;
use crate::theme::Theme;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::message_widget::component::{
    ComponentKeyResult, ComponentState, HoverState, MouseHitResult,
};

// ── MessageRenderInfo ─────────────────────────────────────────────────────────

pub struct MessageRenderInfo {
    pub path: Vec<usize>,
    pub widget_area: Rect,
    pub has_gap_row: bool,
    pub hidden_after: usize,
    /// Lines of node content skipped at the top of this widget_area (partial first node).
    pub skip_lines: u16,
    /// Visual depth used when rendering (counts only ancestors with `indent_children: true`).
    pub visual_depth: usize,
}

pub use super::search::{PendingSearch, SearchHighlight, SearchState, highlight_text_spans};

// ── HiddenState ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum HiddenState {
    #[default]
    NotHidden,
    /// Config-hidden; invisible to navigation and rendering.
    Hidden,
    /// Was `Hidden` but the user has explicitly revealed it; behaves like `NotHidden`.
    Revealed,
}

impl HiddenState {
    /// Returns `true` only for the `Hidden` variant (not `Revealed`).
    pub fn is_hidden(self) -> bool {
        self == HiddenState::Hidden
    }
}

// ── MessageType ───────────────────────────────────────────────────────────────

/// Semantic tag set by the provider on each `MessageState` node.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageType {
    UserMessage,
    AgentMessage,
    Thinking,
    ToolCall,
    ToolResult,
    TaskSummary,
    Container,
    System,
    Json,
    Table,
    Other,
}

impl MessageType {
    pub fn display_name(&self) -> &str {
        match self {
            MessageType::UserMessage => "user",
            MessageType::AgentMessage => "agent",
            MessageType::Thinking => "thinking",
            MessageType::ToolCall => "tool_call",
            MessageType::ToolResult => "tool_result",
            MessageType::TaskSummary => "task_summary",
            MessageType::Container => "container",
            MessageType::System => "system",
            MessageType::Json => "json",
            MessageType::Table => "table",
            MessageType::Other => "other",
        }
    }

    /// Rust enum variant name as used in config keys (e.g. `"AgentMessage"`).
    pub fn variant_name(&self) -> &str {
        match self {
            MessageType::UserMessage => "UserMessage",
            MessageType::AgentMessage => "AgentMessage",
            MessageType::Thinking => "Thinking",
            MessageType::ToolCall => "ToolCall",
            MessageType::ToolResult => "ToolResult",
            MessageType::TaskSummary => "TaskSummary",
            MessageType::Container => "Container",
            MessageType::System => "System",
            MessageType::Json => "Json",
            MessageType::Table => "Table",
            MessageType::Other => "Other",
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

pub fn measure_text_height(text: &Text<'_>, width: u16) -> u16 {
    if width == 0 {
        return text.lines.len().max(1) as u16;
    }
    Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

pub fn get_node<'a>(items: &'a [MessageState], path: &[usize]) -> Option<&'a MessageState> {
    if path.is_empty() {
        return None;
    }
    let node = items.get(path[0])?;
    if path.len() == 1 {
        Some(node)
    } else {
        get_node(&node.children, &path[1..])
    }
}

pub fn get_node_mut<'a>(
    items: &'a mut [MessageState],
    path: &[usize],
) -> Option<&'a mut MessageState> {
    if path.is_empty() {
        return None;
    }
    let node = items.get_mut(path[0])?;
    if path.len() == 1 {
        Some(node)
    } else {
        get_node_mut(&mut node.children, &path[1..])
    }
}

/// Returns true if switching a node from compact to full would reveal more content.
///
/// False when: no `brief` override, single-line text, and that line fits within the
/// compact display width — meaning compact and full render identically.
fn content_needs_show_more(node: &MessageState, viewport_width: u16, visual_depth: usize) -> bool {
    // brief is a deliberate summary — full mode always shows the underlying text
    if node.brief.is_some() {
        return true;
    }
    let text = node.text.as_deref().unwrap_or("");
    // col 0 is the selection gutter; prefix occupies "  ".repeat(depth) + indicator + space
    let prefix_len = visual_depth * 2 + 2;
    let available = (viewport_width as usize)
        .saturating_sub(1)
        .saturating_sub(prefix_len);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    lines.next().is_some() || visual_width(first) > available
}

fn clear_heights(items: &mut [MessageState]) {
    for item in items.iter_mut() {
        item.height = None;
        clear_heights(&mut item.children);
    }
}

/// Captured `show_more` / `expanded` / `hidden` flags from a node, used to
/// restore display state after a `Reset` + replay sequence.
struct NodeUiFlags {
    show_more: bool,
    expanded: bool,
    hidden: HiddenState,
}

fn capture_snapshot(items: &[MessageState], map: &mut HashMap<String, NodeUiFlags>) {
    for item in items {
        map.insert(
            item.id.clone(),
            NodeUiFlags {
                show_more: item.show_more,
                expanded: item.expanded,
                hidden: item.hidden,
            },
        );
        capture_snapshot(&item.children, map);
    }
}

/// Recursively overwrite `show_more`, `expanded`, and `hidden` on any node
/// whose ID appears in `snapshot`. Called on incoming nodes before insertion.
fn apply_snapshot(node: &mut MessageState, snapshot: &HashMap<String, NodeUiFlags>) {
    if let Some(flags) = snapshot.get(&node.id) {
        node.show_more = flags.show_more;
        node.expanded = flags.expanded;
        node.hidden = flags.hidden;
    }
    for child in &mut node.children {
        apply_snapshot(child, snapshot);
    }
}

/// Predicate for TreeCursor: any visible non-Container, non-terminal node.
fn is_nav_target(n: &MessageState) -> bool {
    !n.is_terminal && n.message_type != MessageType::Container
}

/// Predicate for TreeCursor: only UserMessage and AgentMessage nodes.
fn is_ua_nav_target(n: &MessageState) -> bool {
    matches!(
        n.message_type,
        MessageType::UserMessage | MessageType::AgentMessage
    )
}

// ── hidden-state helpers ──────────────────────────────────────────────────────

fn any_hidden_in_tree(items: &[MessageState]) -> bool {
    items
        .iter()
        .any(|n| n.hidden == HiddenState::Hidden || any_hidden_in_tree(&n.children))
}

fn reveal_all_hidden(items: &mut [MessageState]) {
    for node in items.iter_mut() {
        if node.hidden == HiddenState::Hidden {
            node.hidden = HiddenState::Revealed;
            node.height = None;
        }
        reveal_all_hidden(&mut node.children);
    }
}

fn hide_all_revealed(items: &mut [MessageState]) {
    for node in items.iter_mut() {
        if node.hidden == HiddenState::Revealed {
            node.hidden = HiddenState::Hidden;
            node.height = None;
        }
        hide_all_revealed(&mut node.children);
    }
}

/// Collect paths of the next `n` contiguous `Hidden` nodes in DFS order after `start`.
/// Returns the path of the last revealed node (or `None` if none found).
fn reveal_n_hidden_forward(
    items: &mut [MessageState],
    start: &[usize],
    n: usize,
) -> Option<Vec<usize>> {
    let paths = collect_hidden_run_forward(items, start, n);
    if paths.is_empty() {
        return None;
    }
    let last = paths.last().cloned();
    for path in paths {
        if let Some(node) = get_node_mut(items, &path) {
            node.hidden = HiddenState::Revealed;
            node.height = None;
        }
    }
    last
}

/// Collect paths of the previous `n` contiguous `Hidden` nodes in DFS order before `start`.
/// Returns the path of the first (earliest) revealed node.
fn reveal_n_hidden_backward(
    items: &mut [MessageState],
    start: &[usize],
    n: usize,
) -> Option<Vec<usize>> {
    let paths = collect_hidden_run_backward(items, start, n);
    if paths.is_empty() {
        return None;
    }
    let first = paths.first().cloned();
    for path in paths {
        if let Some(node) = get_node_mut(items, &path) {
            node.hidden = HiddenState::Revealed;
            node.height = None;
        }
    }
    first
}

/// Walk DFS starting just after `start`, collecting paths of consecutive `Hidden`
/// nodes (stop at first non-`Hidden` node or after collecting `limit` paths).
fn collect_hidden_run_forward(
    items: &[MessageState],
    start: &[usize],
    limit: usize,
) -> Vec<Vec<usize>> {
    let mut paths = Vec::new();
    let mut cur = match TreeCursor::at(items, start.to_vec()) {
        Some(c) => c,
        None => return paths,
    };
    loop {
        if !cur.advance_one_with(items, |_| true) {
            break;
        }
        let path = cur.path().to_vec();
        let node = match get_node(items, &path) {
            Some(n) => n,
            None => break,
        };
        if node.hidden != HiddenState::Hidden {
            break;
        }
        paths.push(path);
        if paths.len() >= limit {
            break;
        }
    }
    paths
}

fn collect_hidden_run_backward(
    items: &[MessageState],
    start: &[usize],
    limit: usize,
) -> Vec<Vec<usize>> {
    let mut paths = Vec::new();
    let mut cur = match TreeCursor::at(items, start.to_vec()) {
        Some(c) => c,
        None => return paths,
    };
    loop {
        if !cur.retreat_one_with(items, |_| true) {
            break;
        }
        let path = cur.path().to_vec();
        let node = match get_node(items, &path) {
            Some(n) => n,
            None => break,
        };
        if node.hidden != HiddenState::Hidden {
            break;
        }
        paths.push(path);
        if paths.len() >= limit {
            break;
        }
    }
    // Reverse so the earliest path is first.
    paths.reverse();
    paths
}

fn register_subtree(map: &mut HashMap<String, Vec<usize>>, base: &[usize], node: &MessageState) {
    map.insert(node.id.clone(), base.to_vec());
    for (i, child) in node.children.iter().enumerate() {
        let mut path = base.to_vec();
        path.push(i);
        register_subtree(map, &path, child);
    }
}

fn collect_subtree_ids(node: &MessageState) -> Vec<String> {
    let mut ids = vec![node.id.clone()];
    for child in &node.children {
        ids.extend(collect_subtree_ids(child));
    }
    ids
}

// ── data structures ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MessageState {
    pub id: String,

    /// Human-readable display text; None for pure-container nodes with no inline text.
    pub text: Option<String>,
    /// Optional one-line summary shown when `show_more` is false.
    pub brief: Option<String>,

    /// When false, the message-type indicator glyph is suppressed (space rendered instead).
    pub show_indicator: bool,
    /// XML tag stripped from the start of user message text (e.g. `"bash-input"`).
    /// Used to select the per-tag style override in `UserMessageStyle`.
    pub tag: Option<String>,
    /// Group nodes are zero-height when expanded (invisible structural container)
    /// and render one compact line when collapsed.
    pub group: bool,
    pub is_terminal: bool,
    pub message_type: MessageType,

    pub data: String,
    /// Structured tool call arguments; only set on ToolCall nodes.
    pub props: Option<serde_json::Value>,
    /// Parsed timestamp from the message, if available.
    pub timestamp: Option<chrono::DateTime<chrono::FixedOffset>>,

    /// When false, children render at the same visual depth as this node (no indentation).
    pub indent_children: bool,
    pub children: Vec<MessageState>,

    pub hidden: HiddenState,
    /// When false, display is truncated to one line (using `brief` if set, else first line of text).
    pub show_more: bool,
    pub expanded: bool,
    pub height: Option<u16>,

    /// Rich widget state (e.g. `TableState`). Preserved across Replace/Update ops via
    /// `MessageComponent::on_update`.
    pub ui_state: Option<Box<dyn ComponentState>>,
}

impl MessageState {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: None,
            data: String::new(),
            props: None,
            group: false,
            hidden: HiddenState::NotHidden,
            expanded: true,
            height: None,
            is_terminal: false,
            children: vec![],
            message_type: MessageType::Other,
            show_more: false,
            brief: None,
            indent_children: true,
            show_indicator: true,
            tag: None,
            timestamp: None,
            ui_state: None,
        }
    }

    pub fn timestamp(mut self, ts: chrono::DateTime<chrono::FixedOffset>) -> Self {
        self.timestamp = Some(ts);
        self
    }

    pub fn text(mut self, t: impl Into<String>) -> Self {
        self.text = Some(t.into());
        self
    }

    pub fn data(mut self, d: impl Into<String>) -> Self {
        self.data = d.into();
        self
    }

    pub fn props(mut self, p: serde_json::Value) -> Self {
        self.props = Some(p);
        self
    }

    pub fn message_type(mut self, mt: MessageType) -> Self {
        self.message_type = mt;
        self
    }

    pub fn show_more(mut self, v: bool) -> Self {
        self.show_more = v;
        self
    }

    pub fn brief(mut self, b: impl Into<String>) -> Self {
        self.brief = Some(b.into());
        self
    }

    pub fn indent_children(mut self, v: bool) -> Self {
        self.indent_children = v;
        self
    }

    pub fn show_indicator(mut self, v: bool) -> Self {
        self.show_indicator = v;
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn group(mut self, v: bool) -> Self {
        self.group = v;
        self
    }

    pub fn hidden(mut self, v: HiddenState) -> Self {
        self.hidden = v;
        self
    }

    pub fn expanded(mut self, v: bool) -> Self {
        self.expanded = v;
        self
    }

    pub fn children(mut self, c: Vec<MessageState>) -> Self {
        self.children = c;
        self
    }

    pub fn ui_state(mut self, s: Box<dyn ComponentState>) -> Self {
        self.ui_state = Some(s);
        self
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Precedence {
    Top,
    Selection,
    /// Keep the half-open line range `[line_range.0, line_range.1)` within the node at
    /// `path` visible in the viewport. Used by rich widgets (e.g. table row selection)
    /// without requiring the scroll logic to know anything about the widget's internals.
    InnerFocus {
        path: Vec<usize>,
        line_range: (u16, u16),
    },
}

// ── TreeScrollViewState ───────────────────────────────────────────────────────

pub struct TreeScrollViewState {
    pub items: Vec<MessageState>,
    pub id_to_path: HashMap<String, Vec<usize>>,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub top_index: Vec<usize>,
    pub top_offset: u16,
    pub selection_index: Vec<usize>,
    pub at_bottom: bool,
    pub precedence: Precedence,
    pub terminal_expanded: bool,
    pub terminal_pty_rows: u16,
    pub terminal_scrollback_available: u16,
    /// Collapsed-view content height from the most recent crop detection (`None` = full pty_rows).
    pub terminal_collapsed_crop_height: Option<u16>,
    /// Screen position of the terminal widget from the last render: (x, y, area_height, skip).
    /// Set by TreeScrollView::render; used by app.rs to place the PTY cursor.
    pub terminal_render_info: Option<(u16, u16, u16, u16)>,
    /// Geometry of the prompt overlay from the last render, used for mouse translation.
    /// `(area_x, prompt_rows_y, prompt_height, pty_prompt_start_row)` — `None` when hidden.
    pub prompt_overlay_render_info: Option<(u16, u16, u16, u16)>,
    /// Per-message render rectangles from the last render pass, used for mouse hit-testing.
    pub render_rects: Vec<MessageRenderInfo>,
    /// Current mouse hover state; updated on every MouseMoved event.
    pub hover: Option<HoverState>,
    pub theme: Theme,
    pub key_parser: KeyParser,
    /// Committed search (set on Enter).
    pub search: Option<SearchState>,
    /// In-progress search (set while typing in SearchInput mode).
    pub pending_search: Option<PendingSearch>,
    /// UI-flag snapshot taken at `Reset` time; cleared on `ResetDone`.
    /// Incoming nodes (Append/Replace) merge flags from this map before insertion.
    reset_snapshot: HashMap<String, NodeUiFlags>,
    pub marks: super::marks::Marks,
    pub jump_list: super::marks::JumpList,
}

fn make_terminal_node() -> MessageState {
    MessageState {
        id: "terminal".into(),
        text: None,
        data: String::new(),
        props: None,
        group: false,
        hidden: HiddenState::NotHidden,
        expanded: false,
        height: None,
        is_terminal: true,
        children: vec![],
        message_type: MessageType::Other,
        show_more: false,
        brief: None,
        indent_children: true,
        show_indicator: true,
        tag: None,
        timestamp: None,
        ui_state: None,
    }
}

impl TreeScrollViewState {
    pub fn new(items: Vec<MessageState>) -> Self {
        let mut items = items;
        if !items.last().is_some_and(|n| n.is_terminal) {
            items.push(make_terminal_node());
        }

        let mut id_to_path = HashMap::new();
        for (i, item) in items.iter().enumerate() {
            register_subtree(&mut id_to_path, &[i], item);
        }

        let mut state = Self {
            items,
            id_to_path,
            viewport_width: 80,
            viewport_height: 24,
            top_index: vec![],
            top_offset: 0,
            selection_index: vec![],
            at_bottom: false,
            precedence: Precedence::Selection,
            terminal_expanded: false,
            terminal_pty_rows: 20,
            terminal_scrollback_available: 0,
            terminal_collapsed_crop_height: None,
            terminal_render_info: None,
            prompt_overlay_render_info: None,
            render_rects: vec![],
            hover: None,
            theme: Theme::default_dark(),
            key_parser: KeyParser::new(),
            search: None,
            pending_search: None,
            reset_snapshot: HashMap::new(),
            marks: super::marks::Marks::new(),
            jump_list: super::marks::JumpList::new(),
        };
        state.initialize_selection();
        state
    }

    /// Set `top_index` to the first visible node and `selection_index` to the
    /// first non-terminal node, falling back to the terminal if no content exists.
    /// Only updates each index if it is currently empty, so it is safe to call
    /// repeatedly as items stream in via `apply`.
    fn initialize_selection(&mut self) {
        if self.top_index.is_empty()
            && let Some(cur) = TreeCursor::first(&self.items, nonzero_height)
        {
            self.top_index = cur.path().to_vec();
        }
        if self.selection_index.is_empty() {
            let Some(mut cur) = TreeCursor::first(&self.items, nonzero_height) else {
                return;
            };
            loop {
                let path = cur.path().to_vec();
                if !cur.node(&self.items).is_terminal {
                    self.selection_index = path;
                    return;
                }
                if !cur.advance(&self.items, nonzero_height) {
                    // No content nodes exist; fall back to selecting the terminal.
                    self.selection_index = path;
                    return;
                }
            }
        }
    }

    pub fn apply(&mut self, ops: Vec<ReaderOp>) {
        for op in ops {
            self.apply_one(op);
        }
    }

    fn apply_one(&mut self, op: ReaderOp) {
        match op {
            ReaderOp::Tree(tree_op) => self.apply_tree_op(tree_op),
            ReaderOp::Reset { .. } => {
                // Snapshot all current UI flags before wiping the tree, so they
                // can be re-applied to nodes that reappear during replay.
                // Clear first in case a prior Reset fired without a matching ResetDone.
                self.reset_snapshot.clear();
                capture_snapshot(&self.items, &mut self.reset_snapshot);
                // Keep only the terminal node; clear everything else.
                let terminal = self
                    .items
                    .drain(..)
                    .find(|n| n.is_terminal)
                    .unwrap_or_else(make_terminal_node);
                self.items = vec![terminal];
                self.id_to_path.clear();
                if let Some(node) = self.items.first() {
                    self.id_to_path.insert(node.id.clone(), vec![0]);
                }
                self.top_index = vec![];
                self.top_offset = 0;
                self.selection_index = vec![];
                self.at_bottom = false;
            }
            ReaderOp::ResetDone => {
                self.reset_snapshot.clear();
            }
        }
        if self.selection_index.is_empty() {
            self.initialize_selection();
        }
    }

    fn apply_tree_op(&mut self, op: TreeOperation) {
        match op {
            TreeOperation::Append {
                parent_id: None,
                mut message,
            } => {
                apply_snapshot(&mut message, &self.reset_snapshot);
                // Insert before the terminal node so content stays above it.
                let insert_idx = if self.items.last().is_some_and(|n| n.is_terminal) {
                    self.items.len() - 1
                } else {
                    self.items.len()
                };
                let path = vec![insert_idx];
                register_subtree(&mut self.id_to_path, &path, &message);
                self.items.insert(insert_idx, message);
                // The terminal shifted one position; update its id_to_path entry and
                // any viewport/selection indices that were pointing at it.
                if let Some(terminal) = self.items.last()
                    && terminal.is_terminal
                    && insert_idx < self.items.len() - 1
                {
                    let id = terminal.id.clone();
                    self.id_to_path.insert(id, vec![self.items.len() - 1]);
                    if self.selection_index.first() == Some(&insert_idx) {
                        self.selection_index[0] += 1;
                    }
                    if self.top_index.first() == Some(&insert_idx) {
                        self.top_index[0] += 1;
                    }
                }
            }
            TreeOperation::Append {
                parent_id: Some(ref pid),
                mut message,
            } => {
                apply_snapshot(&mut message, &self.reset_snapshot);
                if let Some(parent_path) = self.id_to_path.get(pid).cloned() {
                    let child_idx = get_node(&self.items, &parent_path)
                        .map(|n| n.children.len())
                        .unwrap_or(0);
                    let mut child_path = parent_path.clone();
                    child_path.push(child_idx);
                    register_subtree(&mut self.id_to_path, &child_path, &message);
                    if let Some(parent) = get_node_mut(&mut self.items, &parent_path) {
                        parent.children.push(message);
                    }
                }
            }
            TreeOperation::Replace {
                ref id,
                mut message,
            } => {
                apply_snapshot(&mut message, &self.reset_snapshot);
                if let Some(path) = self.id_to_path.get(id).cloned() {
                    // Collect subtree IDs from the old node before replacing.
                    if let Some(old_node) = get_node(&self.items, &path) {
                        let old_ids = collect_subtree_ids(old_node);
                        for old_id in old_ids {
                            self.id_to_path.remove(&old_id);
                        }
                    }
                    register_subtree(&mut self.id_to_path, &path, &message);
                    if let Some(node) = get_node_mut(&mut self.items, &path) {
                        // Merge ui_state: let old state decide how to blend with new data.
                        let merged_ui = node
                            .ui_state
                            .as_ref()
                            .and_then(|s| s.on_update(&message))
                            .or_else(|| message.ui_state.clone());
                        *node = message;
                        node.ui_state = merged_ui;
                        clear_heights(std::slice::from_mut(node));
                    }
                }
            }
            TreeOperation::Update {
                ref id,
                mut message,
            } => {
                if let Some(path) = self.id_to_path.get(id).cloned()
                    && let Some(node) = get_node_mut(&mut self.items, &path)
                {
                    let existing_children = std::mem::take(&mut node.children);
                    let merged_ui = node
                        .ui_state
                        .as_ref()
                        .and_then(|s| s.on_update(&message))
                        .or_else(|| message.ui_state.take());
                    *node = message;
                    node.children = existing_children;
                    node.ui_state = merged_ui;
                    clear_heights(std::slice::from_mut(node));
                }
            }
            TreeOperation::Remove { ref id } => {
                if let Some(path) = self.id_to_path.get(id).cloned() {
                    // Before removing: if selection is inside the subtree being removed,
                    // find the nearest neighbor to recover selection to afterwards.
                    // We prefer the previous DFS node; fall back to the next one.
                    let recovery_id: Option<String> = if self.selection_index.starts_with(&path) {
                        let prev = TreeCursor::at(&self.items, path.clone()).and_then(|mut cur| {
                            cur.retreat(&self.items, nonzero_height)
                                .then(|| cur.node(&self.items).id.clone())
                        });
                        prev.or_else(|| {
                            // No predecessor: find first visible node after the subtree.
                            TreeCursor::at(&self.items, path.clone()).and_then(|mut cur| {
                                loop {
                                    if !cur.advance(&self.items, nonzero_height) {
                                        return None;
                                    }
                                    if !cur.path().starts_with(&path) {
                                        return Some(cur.node(&self.items).id.clone());
                                    }
                                }
                            })
                        })
                    } else {
                        None
                    };

                    // Deregister the entire subtree.
                    if let Some(node) = get_node(&self.items, &path) {
                        let ids = collect_subtree_ids(node);
                        for old_id in ids {
                            self.id_to_path.remove(&old_id);
                        }
                    }
                    // Splice the node out of its parent's children (or top-level items).
                    if path.len() == 1 {
                        let idx = path[0];
                        self.items.remove(idx);
                        // Shift id_to_path entries for top-level nodes that moved.
                        for entry in self.id_to_path.values_mut() {
                            if !entry.is_empty() && entry[0] > idx {
                                entry[0] -= 1;
                            }
                        }
                        // Fix selection and top indices.
                        if self.selection_index.first() == Some(&idx) {
                            self.selection_index.clear();
                        } else if self.selection_index.first().is_some_and(|&i| i > idx) {
                            self.selection_index[0] -= 1;
                        }
                        if self.top_index.first() == Some(&idx) {
                            self.top_index.clear();
                        } else if self.top_index.first().is_some_and(|&i| i > idx) {
                            self.top_index[0] -= 1;
                        }
                    } else {
                        let parent_path = &path[..path.len() - 1];
                        let child_idx = path[path.len() - 1];
                        if let Some(parent) = get_node_mut(&mut self.items, parent_path) {
                            parent.children.remove(child_idx);
                        }
                        // Shift sibling entries in id_to_path.
                        for entry in self.id_to_path.values_mut() {
                            if entry.len() > parent_path.len()
                                && entry[..parent_path.len()] == *parent_path
                                && entry[parent_path.len()] > child_idx
                            {
                                entry[parent_path.len()] -= 1;
                            }
                        }
                        // Fix selection/top if they pointed into the removed subtree.
                        if self.selection_index.starts_with(&path) {
                            self.selection_index.clear();
                        } else if self.selection_index.len() > parent_path.len()
                            && self.selection_index[..parent_path.len()] == *parent_path
                            && self.selection_index[parent_path.len()] > child_idx
                        {
                            self.selection_index[parent_path.len()] -= 1;
                        }
                        if self.top_index.starts_with(&path) {
                            self.top_index.clear();
                        } else if self.top_index.len() > parent_path.len()
                            && self.top_index[..parent_path.len()] == *parent_path
                            && self.top_index[parent_path.len()] > child_idx
                        {
                            self.top_index[parent_path.len()] -= 1;
                        }
                    }

                    // Restore selection to the saved neighbor if it was cleared.
                    if self.selection_index.is_empty()
                        && let Some(ref rid) = recovery_id
                        && let Some(p) = self.id_to_path.get(rid).cloned()
                    {
                        self.selection_index = p;
                    }
                }
            }
        }
    }

    // ── sizing ────────────────────────────────────────────────────────────────

    pub fn size_node(&mut self, path: &[usize], depth: usize) -> u16 {
        let Some(node) = get_node_mut(&mut self.items, path) else {
            return 0;
        };

        if let Some(h) = node.height {
            return h;
        }

        if let Some(ref mut comp) = get_message_component(node) {
            comp.on_viewport_width_changed();
        }

        let h = if node.is_terminal {
            let content = if self.terminal_expanded {
                self.terminal_scrollback_available + self.terminal_pty_rows
            } else {
                self.terminal_collapsed_crop_height
                    .unwrap_or(self.terminal_pty_rows)
            };
            content + 1 // +1 bottom padding
        } else {
            // Expanded groups are zero-height: invisible structural containers.
            if node.group && node.expanded {
                node.height = Some(0);
                return 0;
            }
            if !node.show_more {
                2 // 1 content line + 1 bottom padding
            } else {
                let prefix_len = (depth * 2 + 2) as u16;
                let available = self
                    .viewport_width
                    .saturating_sub(1)
                    .saturating_sub(prefix_len);

                // Give the component a chance to own layout (e.g. table col-width init).
                // Disjoint field borrows: palette borrows self.theme, items borrows self.items.
                let palette = &self.theme.palette;
                let layout_h =
                    get_message_component(node).and_then(|mut c| c.layout_pass(available, palette));

                if let Some(h) = layout_h {
                    node.height = Some(h);
                    return h;
                }

                // Text-height fallback for nodes without a component.
                let display_text = node.text.as_deref().unwrap_or("").to_string();
                let h = if self
                    .theme
                    .style_for(&node.message_type)
                    .uses_markdown(node.tag.as_deref())
                {
                    let text = render_markdown(&display_text, &self.theme.palette);
                    measure_text_height(&text, available)
                } else {
                    let text = display_text.into_text().unwrap_or_default();
                    measure_text_height(&text, available)
                };
                h.max(1) + 1
            }
        };
        node.height = Some(h);
        h
    }

    pub fn is_terminal_selected(&self) -> bool {
        get_node(&self.items, &self.selection_index)
            .map(|n| n.is_terminal)
            .unwrap_or(false)
    }

    /// Returns true if the currently selected node has a `ComponentState` that
    /// supports interactive mode.
    pub fn is_interaction_supported(&self) -> bool {
        get_node(&self.items, &self.selection_index)
            .and_then(|n| n.ui_state.as_ref())
            .is_some_and(|s| s.supports_interaction())
    }

    /// Dispatch a key event to the selected node's `MessageComponent` and
    /// return the result. Invalidates the node's cached height when indicated
    /// and updates `InnerFocus` after any consumed key.
    pub fn apply_component_key(&mut self, key: KeyEvent) -> ComponentKeyResult {
        let path = self.selection_index.clone();

        let result = if let Some(node) = get_node_mut(&mut self.items, &path) {
            let r = get_message_component(node)
                .map(|mut c| c.handle_key(key))
                .unwrap_or(ComponentKeyResult::Unhandled);
            if matches!(
                r,
                ComponentKeyResult::Consumed {
                    invalidates_height: true
                }
            ) {
                node.height = None;
            }
            r
        } else {
            ComponentKeyResult::Unhandled
        };

        if matches!(result, ComponentKeyResult::Consumed { .. }) {
            self.update_inner_focus();
        }

        result
    }

    /// Set `Precedence::InnerFocus` for the selected node's component.
    /// Call when entering interaction mode.
    pub fn enter_component_focus(&mut self) {
        self.update_inner_focus();
    }

    /// Recompute `InnerFocus` from the selected node's component state's focused line range.
    fn update_inner_focus(&mut self) {
        let path = self.selection_index.clone();
        let palette = &self.theme.palette;
        if let Some(line_range) = get_node_mut(&mut self.items, &path)
            .and_then(get_message_component)
            .and_then(|s| s.focused_line_range(palette))
        {
            self.precedence = Precedence::InnerFocus { path, line_range };
        }
    }

    /// Scroll the viewport so `line_range` within the node at `path` is visible.
    /// This is the generic backing for `Precedence::InnerFocus`.
    pub fn ensure_inner_focus_visible(&mut self, path: Vec<usize>, line_range: (u16, u16)) {
        let (focus_top, focus_bot) = line_range;
        let vp = self.viewport_height as i64;

        if path < self.top_index {
            self.top_index = path;
            self.top_offset = focus_top;
            return;
        }

        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return;
        };
        let mut node_start: i64 = -(self.top_offset as i64);

        loop {
            let cur_path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&cur_path, depth) as i64;

            if cur_path == path {
                let screen_top = node_start + focus_top as i64;
                let screen_bot = node_start + focus_bot as i64;
                if screen_top < 0 {
                    self.top_index = path;
                    self.top_offset = focus_top;
                } else if screen_bot > vp {
                    self.advance_top_by((screen_bot - vp) as u64);
                }
                return;
            }

            node_start += h;
            if !cur.advance(&self.items, nonzero_height) {
                break;
            }
        }
    }

    /// Move selection to the terminal node; does not set `active`.
    pub fn select_terminal_node(&mut self) {
        if let Some(cur) = TreeCursor::last(&self.items, nonzero_height) {
            self.selection_index = cur.path().to_vec();
        }
        self.snap_to_bottom(false);
        self.at_bottom = true;
        self.precedence = Precedence::Selection;
    }

    /// Synchronize the layout cache from `TerminalPanel`'s authoritative values.
    /// Only invalidates the cached terminal node height when a value actually changed.
    pub fn sync_terminal_layout(
        &mut self,
        expanded: bool,
        scrollback: u16,
        collapsed_crop_height: Option<u16>,
        pty_rows: u16,
    ) {
        if self.terminal_expanded == expanded
            && self.terminal_scrollback_available == scrollback
            && self.terminal_collapsed_crop_height == collapsed_crop_height
            && self.terminal_pty_rows == pty_rows
        {
            return;
        }
        self.terminal_expanded = expanded;
        self.terminal_scrollback_available = scrollback;
        self.terminal_collapsed_crop_height = collapsed_crop_height;
        self.terminal_pty_rows = pty_rows;
        self.invalidate_terminal_height();
    }

    /// Update the collapsed crop height immediately (called after recompute_crop in the event loop).
    /// Only invalidates when the value changes and the terminal is collapsed.
    pub fn set_terminal_collapsed_crop_height(&mut self, height: Option<u16>) {
        if self.terminal_collapsed_crop_height == height {
            return;
        }
        self.terminal_collapsed_crop_height = height;
        if !self.terminal_expanded {
            self.invalidate_terminal_height();
        }
    }

    fn invalidate_terminal_height(&mut self) {
        if let Some(cur) = TreeCursor::last(&self.items, nonzero_height) {
            let path = cur.path().to_vec();
            if let Some(node) = get_node_mut(&mut self.items, &path)
                && node.is_terminal
            {
                node.height = None;
            }
        }
    }

    /// Count how many rows at the bottom of the current viewport are blank
    /// (i.e. not covered by any content node).
    fn blank_lines_at_bottom(&mut self) -> u64 {
        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return self.viewport_height as u64;
        };
        let first_h = self.size_node(cur.path(), cur.depth()) as u64;
        let first_visible = first_h.saturating_sub(self.top_offset as u64);
        let mut remaining = self.viewport_height as u64;
        if remaining <= first_visible {
            return 0;
        }
        remaining -= first_visible;
        loop {
            if !cur.advance(&self.items, nonzero_height) {
                return remaining;
            }
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth) as u64;
            if remaining <= h {
                return 0;
            }
            remaining -= h;
        }
    }

    /// Snap `top_index:top_offset` so that the last line of the tree sits at
    /// the bottom of the viewport.  Walks backward from the last visible node,
    /// O(k) where k is the number of nodes that fit inside the viewport.
    ///
    /// If `prefer_down_only` is true and the computed destination is before the
    /// current `top_index:top_offset` position, the update is skipped unless
    /// there are more than 15 blank rows at the bottom of the current viewport
    /// (in which case the upward snap is allowed to recover from significant
    /// content shrinkage).
    pub fn snap_to_bottom(&mut self, prefer_down_only: bool) {
        let Some(mut cur) = TreeCursor::last(&self.items, nonzero_height) else {
            return;
        };
        let mut remaining = self.viewport_height as u64;
        let (new_index, new_offset) = loop {
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth) as u64;
            if remaining <= h {
                break (path, (h - remaining) as u16);
            }
            remaining -= h;
            if !cur.retreat(&self.items, nonzero_height) {
                break (path, 0);
            }
        };
        if prefer_down_only
            && (&new_index, new_offset) < (&self.top_index, self.top_offset)
            && self.blank_lines_at_bottom() <= 15
        {
            return;
        }
        self.top_index = new_index;
        self.top_offset = new_offset;
    }

    // ── viewport ─────────────────────────────────────────────────────────────

    pub fn set_viewport_size(&mut self, width: u16, height: u16) {
        if self.viewport_width != width {
            self.viewport_width = width;
            clear_heights(&mut self.items);
        }
        self.viewport_height = height;
    }

    pub fn ensure_selection_visible(&mut self) {
        let sel_idx = self.selection_index.clone();

        if sel_idx.is_empty() || !self.is_path_visible(&sel_idx) {
            return;
        }

        // For an expanded group (zero-height) we want the whole block of rendered
        // descendants visible, not just the first one. Compute:
        //   target_start — first rendered descendant (where the block begins in the viewport)
        //   override_h   — Some(total_h) for groups so the whole block is kept on-screen;
        //                  None to fall back to the node's own height from the walk below.
        let (target_start, override_h): (Vec<usize>, Option<i64>) =
            if get_node(&self.items, &sel_idx)
                .map(|n| !nonzero_height(n))
                .unwrap_or(false)
            {
                let Some(mut cur) = TreeCursor::at(&self.items, sel_idx.clone()) else {
                    return;
                };
                if !cur.advance(&self.items, nonzero_height) {
                    return;
                }
                let first = cur.path().to_vec();
                if !first.starts_with(&sel_idx) {
                    return;
                }
                let mut total_h = self.size_node(&first, cur.depth()) as i64;
                while cur.advance(&self.items, nonzero_height) {
                    let p = cur.path().to_vec();
                    if !p.starts_with(&sel_idx) {
                        break;
                    }
                    total_h += self.size_node(&p, cur.depth()) as i64;
                }
                (first, Some(total_h))
            } else {
                (sel_idx.clone(), None)
            };

        if target_start < self.top_index {
            self.top_index = target_start;
            self.top_offset = 0;
            return;
        }

        // Selection is at or after top_index: walk forward from the viewport top to find it.
        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return;
        };
        let vp = self.viewport_height as i64;
        // node_start = signed lines from viewport top to this node's first line.
        // Negative for the first node because top_offset lines of it sit above the viewport.
        let mut node_start: i64 = -(self.top_offset as i64);

        loop {
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth) as i64;

            if path == target_start {
                // For a group use the pre-computed total block height; for a regular
                // node use the height from size_node.
                let block_h = override_h.unwrap_or(h);
                let block_end = node_start + block_h;
                if node_start < 0 {
                    if block_h <= vp {
                        // Block fits: reset so it starts at the viewport top.
                        self.top_index = target_start;
                        self.top_offset = 0;
                    }
                    // else: block taller than viewport and already partly shown — leave it.
                } else if block_end > vp {
                    if block_h <= vp {
                        // Block fits: scroll just enough to show the last line.
                        self.advance_top_by((block_end - vp) as u64);
                    } else {
                        // Block taller than viewport: snap its beginning to the top.
                        self.top_index = target_start;
                        self.top_offset = 0;
                    }
                }
                // else: already fully visible.
                return;
            }

            node_start += h;

            if !cur.advance(&self.items, nonzero_height) {
                return;
            }
        }
    }

    /// Returns true if every node along `path` exists, is not hidden, and (for
    /// non-leaf segments) is expanded — i.e. the node is reachable by the cursor.
    fn is_path_visible(&self, path: &[usize]) -> bool {
        let mut items: &[MessageState] = &self.items;
        for (i, &idx) in path.iter().enumerate() {
            let Some(node) = items.get(idx) else {
                return false;
            };
            if node.hidden.is_hidden() {
                return false;
            }
            if i + 1 < path.len() {
                if !node.expanded {
                    return false;
                }
                items = &node.children;
            }
        }
        true
    }

    pub fn update_at_bottom(&mut self) {
        // at_bottom requires selection to be on the last visible leaf.
        let last_path = TreeCursor::last(&self.items, nonzero_height).map(|c| c.path().to_vec());
        if last_path.as_deref() != Some(self.selection_index.as_slice()) {
            self.at_bottom = false;
            return;
        }

        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            self.at_bottom = true;
            return;
        };
        let vp = self.viewport_height as u64;
        let top_path = cur.path().to_vec();
        let top_depth = cur.depth();
        let top_h = self.size_node(&top_path, top_depth);
        let mut remaining = (top_h as u64).saturating_sub(self.top_offset as u64);
        loop {
            if remaining > vp {
                self.at_bottom = false;
                return;
            }
            if !cur.advance(&self.items, nonzero_height) {
                break;
            }
            let path = cur.path().to_vec();
            let depth = cur.depth();
            remaining += self.size_node(&path, depth) as u64;
        }
        self.at_bottom = remaining <= vp;
    }

    // ── viewport-relative helpers ─────────────────────────────────────────────

    /// Walk visible nodes from `top_index+top_offset` and return `(path, row_start,
    /// visible_rows)` for every selectable node that fits in the viewport.
    fn visible_rendered_nodes(&mut self) -> Vec<(Vec<usize>, u16, u16)> {
        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return vec![];
        };
        let mut out = Vec::new();
        let mut row: u16 = 0;
        let mut is_first = true;
        loop {
            if row >= self.viewport_height {
                break;
            }
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth);
            let skip = if is_first {
                is_first = false;
                self.top_offset.min(h.saturating_sub(1))
            } else {
                0
            };
            let visible = h.saturating_sub(skip).min(self.viewport_height - row);
            if visible > 0 {
                out.push((path, row, visible));
                row += visible;
            }
            if !cur.advance(&self.items, nonzero_height) {
                break;
            }
        }
        out
    }

    /// Set `top_index/top_offset` so the current selection's last line sits at
    /// the viewport bottom.
    fn scroll_to_put_selection_at_bottom(&mut self) {
        let sel = self.selection_index.clone();
        let sel_depth = sel.len().saturating_sub(1);
        let sel_h = self.size_node(&sel, sel_depth);
        self.top_index = sel;
        self.top_offset = 0;
        let retreat = (self.viewport_height as u64).saturating_sub(sel_h as u64);
        if retreat > 0 {
            self.retreat_top_by(retreat);
        }
        self.at_bottom = false;
        self.precedence = Precedence::Top;
    }

    // H – move selection to first visible item
    pub fn select_viewport_top(&mut self) {
        let nodes = self.visible_rendered_nodes();
        if let Some((path, _, _)) = nodes.into_iter().next() {
            self.selection_index = path;
            self.at_bottom = false;
            self.precedence = Precedence::Top;
        }
    }

    // M – move selection to middle visible item (by vertical midpoint)
    pub fn select_viewport_middle(&mut self) {
        let nodes = self.visible_rendered_nodes();
        if nodes.is_empty() {
            return;
        }
        let mid = self.viewport_height / 2;
        let best = nodes
            .into_iter()
            .min_by_key(|(_, start, h)| {
                let center = start + h / 2;
                center.abs_diff(mid)
            })
            .map(|(p, _, _)| p);
        if let Some(path) = best {
            self.selection_index = path;
            self.at_bottom = false;
            self.precedence = Precedence::Top;
        }
    }

    // L – move selection to last visible item
    pub fn select_viewport_bottom(&mut self) {
        let nodes = self.visible_rendered_nodes();
        if let Some((path, _, _)) = nodes.into_iter().last() {
            self.selection_index = path;
            self.at_bottom = false;
            self.precedence = Precedence::Top;
        }
    }

    // t (zt) – scroll so selection is at top of viewport
    pub fn scroll_selection_to_top(&mut self) {
        self.top_index = self.selection_index.clone();
        self.top_offset = 0;
        self.at_bottom = false;
        self.precedence = Precedence::Top;
    }

    // z (zz) – scroll so selection is vertically centred
    pub fn scroll_selection_to_middle(&mut self) {
        let sel = self.selection_index.clone();
        let sel_depth = sel.len().saturating_sub(1);
        let sel_h = self.size_node(&sel, sel_depth);
        self.top_index = sel;
        self.top_offset = 0;
        let retreat = (self.viewport_height / 2).saturating_sub(sel_h / 2) as u64;
        if retreat > 0 {
            self.retreat_top_by(retreat);
        }
        self.at_bottom = false;
        self.precedence = Precedence::Top;
    }

    // b (zb) – scroll so selection is at bottom of viewport
    pub fn scroll_selection_to_bottom(&mut self) {
        self.scroll_to_put_selection_at_bottom();
    }

    // g – select first non-terminal item
    pub fn select_first(&mut self) {
        let Some(mut cur) = TreeCursor::first(&self.items, nonzero_height) else {
            return;
        };
        loop {
            if !cur.node(&self.items).is_terminal {
                self.selection_index = cur.path().to_vec();
                self.at_bottom = false;
                self.precedence = Precedence::Selection;
                return;
            }
            if !cur.advance(&self.items, nonzero_height) {
                break;
            }
        }
    }

    // G – select last non-terminal item, put it at the bottom of the viewport
    pub fn select_last_content(&mut self) {
        let Some(mut cur) = TreeCursor::last(&self.items, nonzero_height) else {
            return;
        };
        loop {
            let node = cur.node(&self.items);
            if !node.is_terminal {
                self.selection_index = cur.path().to_vec();
                self.scroll_to_put_selection_at_bottom();
                return;
            }
            if !cur.retreat(&self.items, nonzero_height) {
                break;
            }
        }
    }

    // ── navigation ────────────────────────────────────────────────────────────

    pub fn select_next(&mut self) {
        let is_group = get_node(&self.items, &self.selection_index)
            .map(|n| n.group)
            .unwrap_or(false);
        let Some(mut cur) = TreeCursor::at(&self.items, self.selection_index.clone()) else {
            return;
        };
        // From a group: stay at the current depth level (skip entire subtree).
        // From a regular node: normal DFS forward skipping zero-height nodes.
        let moved = if is_group {
            cur.advance_sibling(&self.items)
        } else {
            cur.advance(&self.items, nonzero_height)
        };
        if moved {
            self.at_bottom = false;
            self.selection_index = cur.path().to_vec();
            self.precedence = Precedence::Selection;
        }
    }

    pub fn select_prev(&mut self) {
        let is_group = get_node(&self.items, &self.selection_index)
            .map(|n| n.group)
            .unwrap_or(false);
        let Some(mut cur) = TreeCursor::at(&self.items, self.selection_index.clone()) else {
            return;
        };
        // From a group: retreat to previous sibling (no descent) or parent.
        // From a regular node: normal DFS backward skipping zero-height nodes.
        let moved = if is_group {
            cur.retreat_sibling(&self.items)
        } else {
            cur.retreat(&self.items, nonzero_height)
        };
        if moved {
            self.at_bottom = false;
            self.selection_index = cur.path().to_vec();
            self.precedence = Precedence::Selection;
        }
    }

    pub fn select_child(&mut self) {
        self.at_bottom = false;
        let sel_path = self.selection_index.clone();
        let (has_children, is_expanded) = match get_node(&self.items, &sel_path) {
            Some(n) => (!n.children.is_empty(), n.expanded),
            None => return,
        };
        if !has_children {
            return;
        }
        if !is_expanded {
            let node = get_node_mut(&mut self.items, &sel_path).unwrap();
            node.expanded = true;
            node.height = None;
            self.precedence = Precedence::Selection;
            return;
        }
        let first_child_idx = {
            let node = get_node(&self.items, &sel_path).unwrap();
            node.children
                .iter()
                .enumerate()
                .find(|(_, c)| !c.hidden.is_hidden())
                .map(|(i, _)| i)
        };
        if let Some(i) = first_child_idx {
            let mut child_path = sel_path;
            child_path.push(i);
            self.selection_index = child_path;
            self.precedence = Precedence::Selection;
        }
    }

    pub fn select_parent(&mut self) {
        self.at_bottom = false;
        if self.selection_index.len() <= 1 {
            return;
        }
        self.selection_index.pop();
        self.precedence = Precedence::Selection;
    }

    pub fn clear_hover(&mut self) {
        self.hover = None;
    }

    // ── scrolling ─────────────────────────────────────────────────────────────

    pub fn scroll_down(&mut self, lines: u16) {
        self.precedence = Precedence::Top;
        self.advance_top_by(lines as u64);
        if self.blank_lines_at_bottom() > 0 {
            self.snap_to_bottom(false);
        }
        self.clamp_selection_to_viewport();
    }

    /// Advance `top_index:top_offset` forward by `lines` content lines.
    /// Uses `u64` to avoid overflow when called from `ensure_selection_visible`.
    fn advance_top_by(&mut self, lines: u64) {
        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return;
        };
        // Absorb the existing top_offset so the loop works from node boundaries.
        let mut remaining = lines + self.top_offset as u64;
        loop {
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth) as u64;
            if remaining < h {
                self.top_index = path;
                self.top_offset = remaining as u16;
                return;
            }
            remaining -= h;
            if !cur.advance(&self.items, nonzero_height) {
                // Past the last node: land on its last line so blank_lines_at_bottom > 0.
                self.top_index = path;
                self.top_offset = (h as u16).saturating_sub(1);
                return;
            }
        }
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.at_bottom = false;
        self.precedence = Precedence::Top;
        self.retreat_top_by(lines as u64);
        self.clamp_selection_to_viewport();
    }

    fn retreat_top_by(&mut self, lines: u64) {
        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return;
        };
        let mut remaining = lines;
        if self.top_offset as u64 >= remaining {
            self.top_offset -= remaining as u16;
            return;
        }
        remaining -= self.top_offset as u64;
        loop {
            if !cur.retreat(&self.items, nonzero_height) {
                self.top_index = cur.path().to_vec();
                self.top_offset = 0;
                return;
            }
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth) as u64;
            if remaining <= h {
                self.top_index = path;
                self.top_offset = (h - remaining) as u16;
                return;
            }
            remaining -= h;
        }
    }

    /// After a viewport-only scroll (precedence=Top), snap the selection to the
    /// first or last visible selectable node if the current selection scrolled
    /// off-screen.
    fn clamp_selection_to_viewport(&mut self) {
        let Some(mut cur) = TreeCursor::at(&self.items, self.top_index.clone()) else {
            return;
        };

        let mut first_sel: Option<Vec<usize>> = None;
        let mut last_sel: Option<Vec<usize>> = None;
        let mut selection_visible = false;
        let mut rows_left = self.viewport_height;
        let mut is_first = true;

        loop {
            if rows_left == 0 {
                break;
            }
            let path = cur.path().to_vec();
            let depth = cur.depth();
            let h = self.size_node(&path, depth);
            let skip = if is_first {
                is_first = false;
                self.top_offset.min(h.saturating_sub(1))
            } else {
                0
            };
            let visible_rows = h.saturating_sub(skip).min(rows_left);
            if visible_rows > 0 {
                if first_sel.is_none() {
                    first_sel = Some(path.clone());
                }
                last_sel = Some(path.clone());
                if path == self.selection_index {
                    selection_visible = true;
                }
                rows_left -= visible_rows;
            }
            if !cur.advance(&self.items, nonzero_height) {
                break;
            }
        }

        if selection_visible {
            return;
        }

        // Lexicographic path order == DFS order: selection before top_index means above.
        if self.selection_index < self.top_index {
            if let Some(p) = first_sel {
                self.selection_index = p;
            }
        } else if let Some(p) = last_sel {
            self.selection_index = p;
        }
    }

    // ── tree mutations ────────────────────────────────────────────────────────

    pub fn expand(&mut self, path: &[usize]) {
        self.at_bottom = false;
        if let Some(node) = get_node_mut(&mut self.items, path) {
            node.expanded = true;
            node.height = None;
        }
    }

    pub fn collapse(&mut self, path: &[usize]) {
        self.at_bottom = false;
        if let Some(node) = get_node_mut(&mut self.items, path) {
            node.expanded = false;
            node.height = None;
        }
    }

    /// Returns true when the selected node has content that would be hidden in compact mode,
    /// i.e. toggling show_more would actually change what is rendered.
    pub fn selection_can_show_more(&self) -> bool {
        let path = &self.selection_index;
        let visual_depth = TreeCursor::at(&self.items, path.clone())
            .map(|c| c.depth())
            .unwrap_or(0);
        get_node(&self.items, path)
            .is_some_and(|n| content_needs_show_more(n, self.viewport_width, visual_depth))
    }

    pub fn toggle_show_more(&mut self) {
        self.at_bottom = false;
        let path = self.selection_index.clone();
        if let Some(node) = get_node_mut(&mut self.items, &path) {
            node.show_more = !node.show_more;
            node.height = None;
        }
    }

    pub fn toggle_expand(&mut self) {
        self.at_bottom = false;
        let path = self.selection_index.clone();
        let is_expanded = get_node(&self.items, &path)
            .map(|n| n.expanded)
            .unwrap_or(false);
        if is_expanded {
            self.collapse(&path);
        } else {
            self.expand(&path);
        }
        self.rectify_selection_and_top();
    }

    /// Space key: cycle through compact → full-text → expanded-children → compact.
    /// The full-text step is skipped when compact and full would render identically.
    pub fn cycle_display(&mut self) {
        self.at_bottom = false;
        let path = self.selection_index.clone();

        let visual_depth = TreeCursor::at(&self.items, path.clone())
            .map(|c| c.depth())
            .unwrap_or(0);
        let viewport_width = self.viewport_width;
        if let Some(node) = get_node_mut(&mut self.items, &path) {
            if content_needs_show_more(node, viewport_width, visual_depth) {
                if !node.show_more && !node.group {
                    // Step 1: reveal full text (only when there's actually more to show)
                    node.show_more = true;
                    node.height = None;
                } else if !node.expanded && !node.children.is_empty() {
                    // Step 2: expand children
                    node.expanded = true;
                    node.height = None;
                } else {
                    // Step 3: collapse back to compact
                    node.show_more = false;
                    node.expanded = false;
                    node.height = None;
                }
            } else {
                if !node.show_more {
                    // Inconsistent state, fix it
                    node.show_more = true;
                    node.height = None;
                }
                if !node.children.is_empty() {
                    node.expanded = !node.expanded;
                    node.height = None;
                }
            }
        }
        self.rectify_selection_and_top();
    }

    pub fn set_hidden(&mut self, path: &[usize], hidden: HiddenState) {
        self.at_bottom = false;
        if let Some(node) = get_node_mut(&mut self.items, path) {
            node.hidden = hidden;
        }
        self.rectify_selection_and_top();
    }

    /// Expand the current node (no-op if already expanded).
    pub fn expand_node(&mut self) {
        let path = self.selection_index.clone();
        self.expand(&path);
        self.rectify_selection_and_top();
    }

    /// Collapse the current node.
    pub fn collapse_node(&mut self) {
        let path = self.selection_index.clone();
        self.collapse(&path);
        self.rectify_selection_and_top();
    }

    /// Expand the current node and reveal all `Hidden` direct children.
    pub fn expand_reveal_children(&mut self) {
        self.at_bottom = false;
        let path = self.selection_index.clone();
        if let Some(node) = get_node_mut(&mut self.items, &path) {
            node.expanded = true;
            node.height = None;
            for child in &mut node.children {
                if child.hidden == HiddenState::Hidden {
                    child.hidden = HiddenState::Revealed;
                    child.height = None;
                }
            }
        }
        self.rectify_selection_and_top();
    }

    /// Collapse the current node and set all `Revealed` direct children back to `Hidden`.
    pub fn collapse_hide_children(&mut self) {
        self.at_bottom = false;
        let path = self.selection_index.clone();
        if let Some(node) = get_node_mut(&mut self.items, &path) {
            node.expanded = false;
            node.height = None;
            for child in &mut node.children {
                if child.hidden == HiddenState::Revealed {
                    child.hidden = HiddenState::Hidden;
                    child.height = None;
                }
            }
        }
        self.rectify_selection_and_top();
    }

    /// Toggle all hidden nodes globally: if any `Hidden` nodes exist, reveal all;
    /// otherwise set all `Revealed` nodes back to `Hidden`.
    pub fn toggle_all_hidden(&mut self) {
        self.at_bottom = false;
        let has_hidden = any_hidden_in_tree(&self.items);
        if has_hidden {
            reveal_all_hidden(&mut self.items);
        } else {
            hide_all_revealed(&mut self.items);
        }
        self.rectify_selection_and_top();
    }

    /// Select the given path, updating precedence so it scrolls into view.
    pub fn select_path(&mut self, path: Vec<usize>) {
        self.selection_index = path;
        self.at_bottom = false;
        self.precedence = Precedence::Selection;
    }

    /// Reveal the next `n` contiguous `Hidden` nodes in DFS order after the
    /// current selection, and select the last one revealed.
    pub fn reveal_next_n_hidden(&mut self, n: usize) {
        self.at_bottom = false;
        let start = self.selection_index.clone();
        let revealed = reveal_n_hidden_forward(&mut self.items, &start, n);
        if let Some(last_path) = revealed {
            self.selection_index = last_path;
        }
        self.rectify_selection_and_top();
    }

    /// Reveal the previous `n` contiguous `Hidden` nodes in DFS order before the
    /// current selection, and select the first one revealed.
    pub fn reveal_prev_n_hidden(&mut self, n: usize) {
        self.at_bottom = false;
        let start = self.selection_index.clone();
        let revealed = reveal_n_hidden_backward(&mut self.items, &start, n);
        if let Some(first_path) = revealed {
            self.selection_index = first_path;
        }
        self.rectify_selection_and_top();
    }

    /// Reveal ALL contiguous `Hidden` nodes following the current selection and
    /// move to the first visible node after the revealed run.
    pub fn reveal_jump_forward(&mut self) {
        self.at_bottom = false;
        let start = self.selection_index.clone();
        let revealed = collect_hidden_run_forward(&self.items, &start, usize::MAX);
        // Apply reveals.
        for path in &revealed {
            if let Some(node) = get_node_mut(&mut self.items, path) {
                node.hidden = HiddenState::Revealed;
                node.height = None;
            }
        }
        // Jump to the first visible node AFTER the revealed run.
        let jump_from = revealed.last().unwrap_or(&start).clone();
        if let Some(mut cur) = TreeCursor::at(&self.items, jump_from)
            && cur.advance(&self.items, nonzero_height)
        {
            self.selection_index = cur.path().to_vec();
        }
        self.rectify_selection_and_top();
    }

    /// Reveal ALL contiguous `Hidden` nodes preceding the current selection and
    /// move to the last visible node before the revealed run.
    pub fn reveal_jump_backward(&mut self) {
        self.at_bottom = false;
        let start = self.selection_index.clone();
        let revealed = collect_hidden_run_backward(&self.items, &start, usize::MAX);
        for path in &revealed {
            if let Some(node) = get_node_mut(&mut self.items, path) {
                node.hidden = HiddenState::Revealed;
                node.height = None;
            }
        }
        let jump_from = revealed.first().unwrap_or(&start).clone();
        if let Some(mut cur) = TreeCursor::at(&self.items, jump_from)
            && cur.retreat(&self.items, nonzero_height)
        {
            self.selection_index = cur.path().to_vec();
        }
        self.rectify_selection_and_top();
    }

    fn rectify_selection_and_top(&mut self) {
        if let Some(c) = TreeCursor::closest(&self.items, &self.selection_index.clone()) {
            self.selection_index = c.path().to_vec();
        }
        if let Some(c) = TreeCursor::closest(&self.items, &self.top_index.clone()) {
            let new_path = c.path().to_vec();
            if new_path != self.top_index {
                self.top_index = new_path;
                self.top_offset = 0;
            }
        }
    }

    /// Returns the display type label for the currently selected node.
    pub fn selected_node_type_label(&self) -> &str {
        get_node(&self.items, &self.selection_index)
            .map(|n| {
                if n.group {
                    "group"
                } else {
                    n.message_type.display_name()
                }
            })
            .unwrap_or("")
    }

    pub fn selected_node_id(&self) -> &str {
        get_node(&self.items, &self.selection_index)
            .map(|n| n.id.as_str())
            .unwrap_or("")
    }

    pub fn selected_data(&self) -> &str {
        for len in (1..=self.selection_index.len()).rev() {
            if let Some(node) = get_node(&self.items, &self.selection_index[..len])
                && !node.data.is_empty()
            {
                return &node.data;
            }
        }
        ""
    }

    /// Returns the display text of the selected node (markdown), falling back to `data`.
    pub fn selected_text(&self) -> &str {
        if let Some(node) = get_node(&self.items, &self.selection_index) {
            node.text.as_deref().unwrap_or(&node.data)
        } else {
            ""
        }
    }

    // ── advanced navigation ───────────────────────────────────────────────────

    // Ctrl-D / Ctrl-U
    pub fn scroll_down_half(&mut self, n: u16) {
        self.scroll_down(n);
    }

    pub fn scroll_up_half(&mut self, n: u16) {
        self.scroll_up(n);
    }

    // ) – next same-type run start
    pub fn select_next_type_start(&mut self) {
        self.cursor_advance_run_start(is_nav_target);
    }

    // ( – prev same-type run start
    pub fn select_prev_type_start(&mut self) {
        self.cursor_retreat_run_start(is_nav_target);
    }

    // } – next user/agent run start
    pub fn select_next_user_agent(&mut self) {
        self.cursor_advance_run_start(is_ua_nav_target);
    }

    // { – prev user/agent run start
    pub fn select_prev_user_agent(&mut self) {
        self.cursor_retreat_run_start(is_ua_nav_target);
    }

    // ]] – first non-Container message in next turn
    pub fn select_next_turn_start(&mut self) {
        let turn_idx = self.selection_index.first().copied().unwrap_or(0);
        let next = turn_idx + 1;
        if next >= self.items.len() {
            return;
        }
        let path = self.turn_start_path(next);
        self.set_selection(path);
        self.top_index = self.selection_index.clone();
        self.top_offset = 0;
    }

    // ][ – last non-Container message in current turn; if already at or past the run start, advance to next turn end
    pub fn select_next_turn_end(&mut self) {
        let turn_idx = self.selection_index.first().copied().unwrap_or(0);
        let end = self.turn_end_run_start_path(turn_idx);
        if self.selection_index >= end {
            let next_end = self.turn_end_run_start_path(turn_idx + 1);
            self.set_selection(next_end);
        } else {
            self.set_selection(end);
        }
        self.top_index = self.selection_index.clone();
        self.top_offset = 0;
    }

    // [[ – first non-Container message in current turn; if already there, retreat to previous turn first non-Container message
    pub fn select_prev_turn_start(&mut self) {
        let raw = self.selection_index.first().copied().unwrap_or(0);
        let turn_idx = if self.is_terminal_selected() {
            raw.saturating_sub(1)
        } else {
            raw
        };
        let start = self.turn_start_path(turn_idx);
        if self.selection_index == start {
            if turn_idx == 0 {
                return;
            }
            let prev_start = self.turn_start_path(turn_idx - 1);
            self.set_selection(prev_start);
        } else {
            self.set_selection(start);
        }
        self.top_index = self.selection_index.clone();
        self.top_offset = 0;
    }

    // [] – last non-Container message in current turn; if already at or past the run start, retreat to previous turn end
    pub fn select_prev_turn_end(&mut self) {
        let raw = self.selection_index.first().copied().unwrap_or(0);
        let is_terminal = self.is_terminal_selected();
        let turn_idx = if is_terminal {
            raw.saturating_sub(1)
        } else {
            raw
        };
        let end = self.turn_end_run_start_path(turn_idx);
        // "Past the end going backwards" means selection is at or before the run-start.
        // Guard against is_terminal: [n] is always lexicographically greater than any
        // content path, so we must not treat it as "not yet past end".
        if !is_terminal && self.selection_index <= end {
            if turn_idx == 0 {
                return;
            }
            let prev_end = self.turn_end_run_start_path(turn_idx - 1);
            self.set_selection(prev_end);
        } else {
            self.set_selection(end);
        }
        self.top_index = self.selection_index.clone();
        self.top_offset = 0;
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn set_selection(&mut self, path: Vec<usize>) {
        if !path.is_empty() {
            self.selection_index = path;
            self.at_bottom = false;
            self.precedence = Precedence::Selection;
        }
    }

    fn cursor_advance_run_start(&mut self, predicate: fn(&MessageState) -> bool) {
        let cur_type = match get_node(&self.items, &self.selection_index) {
            Some(n) if predicate(n) => Some(n.message_type.clone()),
            _ => None,
        };
        let Some(mut cur) = TreeCursor::at(&self.items, self.selection_index.clone()) else {
            return;
        };
        if let Some(cur_type) = cur_type {
            // Step one visible node at a time. A non-predicate visible node (e.g. a
            // ToolCall between two AgentMessages) breaks the run, so that the next
            // same-type node is treated as a new run start rather than a continuation.
            let mut run_broken = false;
            loop {
                if !cur.advance(&self.items, nonzero_height) {
                    return;
                }
                let node = cur.node(&self.items);
                if predicate(node) {
                    if run_broken || node.message_type != cur_type {
                        break; // new run start
                    }
                    // else: still in same run
                } else {
                    run_broken = true;
                }
            }
        } else if !cur.advance(&self.items, predicate) {
            return;
        }
        self.set_selection(cur.path().to_vec());
    }

    fn cursor_retreat_run_start(&mut self, predicate: fn(&MessageState) -> bool) {
        let cur_type = match get_node(&self.items, &self.selection_index) {
            Some(n) if predicate(n) => Some(n.message_type.clone()),
            _ => None,
        };
        let Some(mut cur) = TreeCursor::at(&self.items, self.selection_index.clone()) else {
            return;
        };
        // If not in list, retreat to nearest predicate node then find its run start
        // using predicate-based scanning (which skips non-predicate nodes like Containers).
        // We do NOT use the nonzero_height separator logic here because Container nodes
        // between turns would falsely declare every first-in-turn node as a run start.
        if cur_type.is_none() {
            if !cur.retreat(&self.items, predicate) {
                return;
            }
            let run_type = cur.node(&self.items).message_type.clone();
            let mut run_start = cur.path().to_vec();
            loop {
                let mut probe = TreeCursor::at(&self.items, run_start.clone()).unwrap();
                if !probe.retreat(&self.items, predicate) {
                    break;
                }
                if probe.node(&self.items).message_type != run_type {
                    break;
                }
                run_start = probe.path().to_vec();
            }
            self.set_selection(run_start);
            return;
        }
        let cur_type = cur_type.unwrap();
        // Check if we're at run start: the preceding visible node is absent, non-predicate,
        // or a different type (including when a ToolCall sits between same-type UA messages).
        let at_run_start = {
            let mut probe = TreeCursor::at(&self.items, cur.path().to_vec()).unwrap();
            if probe.retreat(&self.items, nonzero_height) {
                let prev = probe.node(&self.items);
                !predicate(prev) || prev.message_type != cur_type
            } else {
                true
            }
        };
        if !at_run_start {
            // Mid-run: scan backward one visible step at a time to the run start.
            let mut run_start = cur.path().to_vec();
            loop {
                let mut probe = TreeCursor::at(&self.items, run_start.clone()).unwrap();
                if !probe.retreat(&self.items, nonzero_height) {
                    break;
                }
                let prev = probe.node(&self.items);
                if predicate(prev) && prev.message_type == cur_type {
                    run_start = probe.path().to_vec();
                } else {
                    break;
                }
            }
            self.set_selection(run_start);
        } else {
            // At run start: step back past any non-predicate nodes to find the previous run,
            // then scan that run backward to its start.
            let mut back = cur;
            loop {
                if !back.retreat(&self.items, nonzero_height) {
                    return;
                }
                if predicate(back.node(&self.items)) {
                    break;
                }
            }
            let prev_type = back.node(&self.items).message_type.clone();
            let mut prev_run_start = back.path().to_vec();
            loop {
                let mut probe = TreeCursor::at(&self.items, prev_run_start.clone()).unwrap();
                if !probe.retreat(&self.items, nonzero_height) {
                    break;
                }
                let prev = probe.node(&self.items);
                if predicate(prev) && prev.message_type == prev_type {
                    prev_run_start = probe.path().to_vec();
                } else {
                    break;
                }
            }
            self.set_selection(prev_run_start);
        }
    }

    /// DFS-first non-Container visible path within turn at `turn_idx`.
    /// Falls back to the turn group node itself if no such path exists.
    fn turn_start_path(&self, turn_idx: usize) -> Vec<usize> {
        let turn_item = match self.items.get(turn_idx) {
            Some(t) if !t.is_terminal => t,
            _ => return vec![],
        };
        TreeCursor::first(&turn_item.children, is_nav_target)
            .map(|cur| {
                let mut p = vec![turn_idx];
                p.extend_from_slice(cur.path());
                p
            })
            .unwrap_or_else(|| vec![turn_idx])
    }

    /// DFS-last non-Container visible path within turn at `turn_idx`.
    /// Falls back to the turn group node itself if no such path exists.
    fn turn_end_path(&self, turn_idx: usize) -> Vec<usize> {
        let turn_item = match self.items.get(turn_idx) {
            Some(t) if !t.is_terminal => t,
            _ => return vec![],
        };
        TreeCursor::last(&turn_item.children, is_nav_target)
            .map(|cur| {
                let mut p = vec![turn_idx];
                p.extend_from_slice(cur.path());
                p
            })
            .unwrap_or_else(|| vec![turn_idx])
    }

    /// Like `turn_end_path`, but retreats to the start of the same-type sibling
    /// run at the found node's level (e.g. `[U,T,A,T,A,A,A]` → items[4]).
    fn turn_end_run_start_path(&self, turn_idx: usize) -> Vec<usize> {
        let end_path = self.turn_end_path(turn_idx);
        if end_path.is_empty() {
            return end_path;
        }
        let end_type = match get_node(&self.items, &end_path) {
            Some(n) => n.message_type.clone(),
            None => return end_path,
        };
        let parent_depth = end_path.len() - 1;
        let last_idx = *end_path.last().unwrap();
        let siblings: &[MessageState] = if parent_depth == 0 {
            &self.items
        } else {
            match get_node(&self.items, &end_path[..parent_depth]) {
                Some(n) => &n.children,
                None => return end_path,
            }
        };
        let mut run_start = last_idx;
        while run_start > 0 {
            let prev = &siblings[run_start - 1];
            // Table nodes are treated as part of an AgentMessage run (workaround).
            let in_run = is_nav_target(prev)
                && (prev.message_type == end_type
                    || (end_type == MessageType::AgentMessage
                        && prev.message_type == MessageType::Table));
            if !in_run {
                break;
            }
            run_start -= 1;
        }
        let mut path = end_path[..parent_depth].to_vec();
        path.push(run_start);
        path
    }

    /// Apply a `TreeAction` to this state, handling all pure-tree actions.
    /// `Quit`, `TerminalActivate`, and `None` are no-ops here; callers handle
    /// them with their own app-level logic.
    pub fn apply_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::SelectNext => self.select_next(),
            TreeAction::SelectPrev => self.select_prev(),
            TreeAction::SelectChild => self.select_child(),
            TreeAction::SelectParent => self.select_parent(),
            TreeAction::ToggleExpand => self.toggle_expand(),
            TreeAction::CycleDisplay => self.cycle_display(),
            TreeAction::ScrollDown(n) => self.scroll_down(n),
            TreeAction::ScrollUp(n) => self.scroll_up(n),
            TreeAction::ScrollDownHalf(n) => self.scroll_down_half(n),
            TreeAction::ScrollUpHalf(n) => self.scroll_up_half(n),
            TreeAction::SelectViewportTop => self.select_viewport_top(),
            TreeAction::SelectViewportMiddle => self.select_viewport_middle(),
            TreeAction::SelectViewportBottom => self.select_viewport_bottom(),
            TreeAction::ScrollSelectionToTop => self.scroll_selection_to_top(),
            TreeAction::ScrollSelectionToMiddle => self.scroll_selection_to_middle(),
            TreeAction::ScrollSelectionToBottom => self.scroll_selection_to_bottom(),
            TreeAction::SelectFirst => self.select_first(),
            TreeAction::SelectLastContent => self.select_last_content(),
            TreeAction::SelectNextTypeStart => self.select_next_type_start(),
            TreeAction::SelectPrevTypeStart => self.select_prev_type_start(),
            TreeAction::SelectNextUserAgent => self.select_next_user_agent(),
            TreeAction::SelectPrevUserAgent => self.select_prev_user_agent(),
            TreeAction::SelectNextTurnStart => self.select_next_turn_start(),
            TreeAction::SelectNextTurnEnd => self.select_next_turn_end(),
            TreeAction::SelectPrevTurnStart => self.select_prev_turn_start(),
            TreeAction::SelectPrevTurnEnd => self.select_prev_turn_end(),
            TreeAction::OpenNode => self.expand_node(),
            TreeAction::CloseNode => self.collapse_node(),
            TreeAction::OpenRevealHidden => self.expand_reveal_children(),
            TreeAction::CloseHideRevealed => self.collapse_hide_children(),
            TreeAction::RevealNextFive => self.reveal_next_n_hidden(5),
            TreeAction::RevealPrevFive => self.reveal_prev_n_hidden(5),
            TreeAction::RevealJumpForward => self.reveal_jump_forward(),
            TreeAction::RevealJumpBackward => self.reveal_jump_backward(),
            TreeAction::ToggleAllHidden => self.toggle_all_hidden(),
            TreeAction::ToggleShowMore => {} // handled in App::apply_tree_action
            TreeAction::TerminalActivate
            | TreeAction::Quit
            | TreeAction::None
            | TreeAction::CopyMarkdown
            | TreeAction::CopyPlainText
            | TreeAction::CopyRawData
            | TreeAction::SetMark(_)
            | TreeAction::DeleteMark(_)
            | TreeAction::GotoMark(_)
            | TreeAction::PopJump => {}
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent, area_height: u16) -> TreeAction {
        self.key_parser.process(key, area_height)
    }

    /// Create a state from `items` without appending a terminal sentinel node.
    /// Use this for tree views that have no embedded PTY (e.g. the data view popup).
    pub fn new_without_terminal(items: Vec<MessageState>) -> Self {
        let mut id_to_path = HashMap::new();
        for (i, item) in items.iter().enumerate() {
            register_subtree(&mut id_to_path, &[i], item);
        }
        let mut state = Self {
            items,
            id_to_path,
            viewport_width: 80,
            viewport_height: 24,
            top_index: vec![],
            top_offset: 0,
            selection_index: vec![],
            at_bottom: false,
            precedence: Precedence::Selection,
            terminal_expanded: false,
            terminal_pty_rows: 20,
            terminal_scrollback_available: 0,
            terminal_collapsed_crop_height: None,
            terminal_render_info: None,
            prompt_overlay_render_info: None,
            render_rects: vec![],
            hover: None,
            theme: Theme::default_dark(),
            key_parser: KeyParser::new(),
            search: None,
            pending_search: None,
            reset_snapshot: HashMap::new(),
            marks: super::marks::Marks::new(),
            jump_list: super::marks::JumpList::new(),
        };
        state.initialize_selection();
        state
    }

    /// Translate a host-terminal mouse event into PTY-relative coordinates.
    ///
    /// The host terminal delivers `(column, row)` in absolute screen space
    /// (0-based, top-left of the full terminal window). The PTY child expects
    /// coordinates relative to its own screen (0-based, top-left of the live
    /// PTY area). Returns `None` if the event lands outside the live PTY area
    /// (button bar, scrollback, or entirely outside the pane).
    pub fn translate_mouse_to_pty(&self, ev: MouseEvent) -> Option<MouseEvent> {
        let (tx, ty, _th, tskip) = self.terminal_render_info?;

        // Col 0 is the selection gutter; PTY content starts at tx+1.
        let pty_col = ev.column.checked_sub(tx + 1)?;
        let pane_i = ev.row.checked_sub(ty)?; // row index within the pane

        // Determine the "block row" — the logical row index within the terminal
        // content area (scrollback rows first when expanded, then live PTY rows).
        let sb = self.terminal_scrollback_available;
        let expanded = self.terminal_expanded;
        let block_row = tskip.saturating_add(pane_i);

        // Scrollback rows are not live PTY rows.
        if expanded && block_row < sb {
            return None;
        }

        // Convert to a 0-based live PTY row.
        let live_row = if expanded {
            block_row.saturating_sub(sb)
        } else {
            block_row
        };

        if live_row >= self.terminal_pty_rows {
            return None;
        }

        Some(crossterm::event::MouseEvent {
            kind: ev.kind,
            column: pty_col,
            row: live_row,
            modifiers: ev.modifiers,
        })
    }

    /// Hit-test a screen coordinate against the last render pass's rectangles.
    pub fn hit_test(&mut self, x: u16, y: u16) -> MouseHitResult {
        // 1. Check terminal area.
        if let Some((tx, ty, th, _skip)) = self.terminal_render_info
            && x >= tx
            && x < tx + self.viewport_width
            && y >= ty
            && y < ty + th
        {
            return MouseHitResult::Terminal;
        }

        // 2. Iterate render rects.
        for i in 0..self.render_rects.len() {
            let wa = self.render_rects[i].widget_area;
            if x < wa.x || x >= wa.x + wa.width || y < wa.y || y >= wa.y + wa.height {
                continue;
            }

            let depth = self.render_rects[i].visual_depth;
            let indicator_x = wa.x + 1 + (depth * 2) as u16;
            let has_gap_row = self.render_rects[i].has_gap_row;
            let hidden_after = self.render_rects[i].hidden_after;
            let skip_lines = self.render_rects[i].skip_lines;
            let path = self.render_rects[i].path.clone();

            // a. Gap row (last row of the widget, only when hidden nodes follow).
            if has_gap_row && hidden_after > 0 && y == wa.y + wa.height - 1 {
                return MouseHitResult::GapRow { path, hidden_after };
            }

            // b. Indicator area (indicator col + space col).
            if x >= indicator_x && x < indicator_x + 2 {
                return MouseHitResult::IndicatorArea { path };
            }

            // c. Inner component hit-test (via MessageComponent trait — no downcast).
            let prefix_len = depth * 2 + 2;
            let content_x = wa.x + 1 + prefix_len as u16;
            if x >= content_x {
                let rel_x = x - content_x;
                let rel_y = (y - wa.y) + skip_lines;
                if let Some(node) = get_node_mut(&mut self.items, &path)
                    && let Some(hit) = match_mouse_node(node, rel_x, rel_y)
                {
                    return MouseHitResult::InnerComponent { path, hit };
                }
            }

            // d. Generic message body.
            return MouseHitResult::Message { path };
        }

        MouseHitResult::Outside
    }
}
