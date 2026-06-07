mod handler;
pub mod render;

use crossterm::event::KeyEvent;

use super::state::{ComponentKeyResult, MessageComponent, MessageState, UiState};
use crate::theme::Palette;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolResultUiState {
    pub payload: ToolResultPayload,
    /// Space-toggled: expand to show all hunks / full output.
    pub expanded: bool,
    /// w-toggled: wrap long lines instead of clipping.
    pub wrap: bool,
}

#[derive(Debug, Clone)]
pub enum ToolResultPayload {
    FileDelta(FileDeltaState),
    ShellOutput(ShellOutputState),
}

#[derive(Debug, Clone)]
pub struct FileDeltaState {
    pub file_path: String,
    pub hunks: Vec<PatchHunk>,
    /// None = show all context lines; Some(n) = at most n per change block.
    pub context_lines: Option<usize>,
    pending_y: bool,
}

#[derive(Debug, Clone)]
pub struct PatchHunk {
    /// Raw unified diff lines, each with ' ', '+', or '-' prefix.
    pub lines: Vec<String>,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
}

#[derive(Debug, Clone)]
pub struct ShellOutputState {
    pub stderr: String,
    pub stdout: String,
}

impl ToolResultUiState {
    pub fn file_delta(
        file_path: String,
        hunks: Vec<PatchHunk>,
        context_lines: Option<usize>,
    ) -> Self {
        Self {
            payload: ToolResultPayload::FileDelta(FileDeltaState {
                file_path,
                hunks,
                context_lines,
                pending_y: false,
            }),
            expanded: false,
            wrap: false,
        }
    }

    pub fn shell_output(stderr: String, stdout: String) -> Self {
        Self {
            payload: ToolResultPayload::ShellOutput(ShellOutputState { stderr, stdout }),
            expanded: false,
            wrap: false,
        }
    }
}

// ── UiState / MessageComponent ────────────────────────────────────────────────

impl UiState for ToolResultUiState {
    fn clone_box(&self) -> Box<dyn UiState> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn on_update(&self, new_message: &MessageState) -> Option<Box<dyn UiState>> {
        let new_state = new_message
            .ui_state
            .as_ref()?
            .as_any()
            .downcast_ref::<ToolResultUiState>()?;

        let mut preserved = new_state.clone();
        preserved.expanded = self.expanded;
        preserved.wrap = self.wrap;

        if let (ToolResultPayload::FileDelta(old), ToolResultPayload::FileDelta(new_fd)) =
            (&self.payload, &mut preserved.payload)
        {
            new_fd.context_lines = old.context_lines;
        }

        Some(Box::new(preserved))
    }

    fn as_component(&self) -> Option<&dyn MessageComponent> {
        Some(self)
    }

    fn as_component_mut(&mut self) -> Option<&mut dyn MessageComponent> {
        Some(self)
    }
}

impl MessageComponent for ToolResultUiState {
    fn supports_interaction(&self) -> bool {
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> ComponentKeyResult {
        handler::handle_tool_result_key(key, self)
    }

    fn focused_line_range(&self, _palette: &Palette) -> Option<(u16, u16)> {
        None
    }

    fn on_viewport_width_changed(&mut self) {}

    fn layout_pass(&mut self, available_width: u16, _palette: &Palette) -> Option<u16> {
        let height = render::compute_height(self, available_width);
        Some(height)
    }
}

pub fn format_unified_diff(file_path: &str, hunks: &[PatchHunk]) -> String {
    let path = file_path.trim_start_matches('/');
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for hunk in hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines,
        ));
        for line in &hunk.lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ── Context filtering ─────────────────────────────────────────────────────────

/// Compute the maximum consecutive context lines in a hunk.
pub fn max_context_in_hunk(hunk: &PatchHunk) -> usize {
    let mut max = 0usize;
    let mut run = 0usize;
    for line in &hunk.lines {
        if line.starts_with(' ') {
            run += 1;
            if run > max {
                max = run;
            }
        } else {
            run = 0;
        }
    }
    max
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffLineKind {
    Added,
    Removed,
    /// Added line in new-version-only mode (removed lines hidden). Rendered with a "changed" color.
    Changed,
    /// Removed line in new-version-only mode: advances `old_n` but is not rendered.
    RemovedHidden,
    Context,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub old_num: Option<u32>,
    pub new_num: Option<u32>,
    pub kind: DiffLineKind,
    /// Line content without the leading diff marker (+/-/ ).
    pub content: String,
}

/// Build the list of `DiffLine`s to render for a single hunk.
///
/// Walks the hunk lines splitting them into context runs and change blocks (contiguous
/// runs of `+`/`-` lines). Each change block independently decides between full-diff and
/// new-version-only mode:
/// - ≤10 lines in block, or pure deletion (no adds) → full diff
/// - >10 lines with adds → new-version mode (`RemovedHidden` + `Changed`; cap 30)
///
/// `context_limit` trims context lines to at most that many per change-block boundary.
/// `None` means keep all context.
///
/// Returns `(lines, hidden_count)` where `hidden_count > 0` means truncation occurred.
pub fn build_diff_lines(
    hunk: &PatchHunk,
    show_full: bool,
    context_limit: Option<usize>,
) -> (Vec<DiffLine>, usize) {
    // Walk hunk lines splitting into context runs and change blocks. Decide
    // full-diff vs new-version independently for each change block.
    let mut raw_lines: Vec<(DiffLineKind, &str)> = Vec::new();
    let lines = &hunk.lines;
    let mut i = 0;

    while i < lines.len() {
        if let Some(rest) = lines[i].strip_prefix(' ') {
            raw_lines.push((DiffLineKind::Context, rest));
            i += 1;
            continue;
        }

        // Collect a contiguous change block (+/- lines).
        let block_start = i;
        while i < lines.len() && !lines[i].starts_with(' ') {
            i += 1;
        }
        let block = &lines[block_start..i];

        let added_count = block.iter().filter(|l| l.starts_with('+')).count();
        let use_full = show_full || block.len() <= 10 || added_count == 0;

        for line in block {
            if let Some(rest) = line.strip_prefix('+') {
                let kind = if use_full {
                    DiffLineKind::Added
                } else {
                    DiffLineKind::Changed
                };
                raw_lines.push((kind, rest));
            } else if let Some(rest) = line.strip_prefix('-') {
                let kind = if use_full {
                    DiffLineKind::Removed
                } else {
                    DiffLineKind::RemovedHidden
                };
                raw_lines.push((kind, rest));
            }
        }
    }

    // Filter context runs if a limit is set.
    let filtered = apply_context_limit(&raw_lines, context_limit);

    // Truncate Changed lines at 30 total.
    let total_changed = filtered
        .iter()
        .filter(|(k, _)| *k == DiffLineKind::Changed)
        .count();
    let truncate_at = if total_changed > 30 {
        Some(30usize)
    } else {
        None
    };

    // Assign line numbers and apply truncation.
    let mut old_n = hunk.old_start;
    let mut new_n = hunk.new_start;
    let mut changed_seen = 0usize;
    let mut truncated = 0usize;
    let mut result = Vec::new();

    for (kind, content) in &filtered {
        // RemovedHidden advances old_n but is not rendered.
        if *kind == DiffLineKind::RemovedHidden {
            old_n += 1;
            continue;
        }

        if let Some(limit) = truncate_at
            && *kind == DiffLineKind::Changed
        {
            if changed_seen >= limit {
                truncated += 1;
                continue;
            }
            changed_seen += 1;
        }

        let (old_num, new_num) = match kind {
            DiffLineKind::Context => {
                let o = Some(old_n);
                let n = Some(new_n);
                old_n += 1;
                new_n += 1;
                (o, n)
            }
            DiffLineKind::Removed => {
                let o = Some(old_n);
                old_n += 1;
                (o, None)
            }
            DiffLineKind::Added | DiffLineKind::Changed => {
                let n = Some(new_n);
                new_n += 1;
                (None, n)
            }
            DiffLineKind::RemovedHidden => unreachable!(),
        };

        result.push(DiffLine {
            old_num,
            new_num,
            kind: kind.clone(),
            content: content.to_string(),
        });
    }

    (result, truncated)
}

fn apply_context_limit<'a>(
    lines: &[(DiffLineKind, &'a str)],
    limit: Option<usize>,
) -> Vec<(DiffLineKind, &'a str)> {
    let Some(limit) = limit else {
        return lines.to_vec();
    };

    // Context semantics: each change block has `limit` lines of context on each side.
    // - Leading context (before first change): keep the LAST `limit` lines (closest to change).
    // - Trailing context (after last change): keep the FIRST `limit` lines (closest to change).
    // - Middle context (between two change groups): keep first `limit` AND last `limit`.
    let first_change = lines.iter().position(|(k, _)| *k != DiffLineKind::Context);
    let last_change = lines.iter().rposition(|(k, _)| *k != DiffLineKind::Context);

    let n = lines.len();
    let mut keep = vec![false; n];

    let mut i = 0;
    while i < n {
        if lines[i].0 != DiffLineKind::Context {
            keep[i] = true;
            i += 1;
            continue;
        }
        let block_start = i;
        while i < n && lines[i].0 == DiffLineKind::Context {
            i += 1;
        }
        let block_end = i; // exclusive
        let block_len = block_end - block_start;

        let is_leading = first_change.is_none_or(|fc| block_end <= fc);
        let is_trailing = last_change.is_none_or(|lc| block_start > lc);

        if is_leading {
            // Keep last `limit` lines.
            let start = block_end.saturating_sub(limit);
            keep[start..block_end].fill(true);
        } else if is_trailing {
            // Keep first `limit` lines.
            keep[block_start..(block_start + limit.min(block_len))].fill(true);
        } else {
            // Middle block: keep first `limit` (after-context) AND last `limit` (before-context).
            keep[block_start..(block_start + limit.min(block_len))].fill(true);
            keep[block_end.saturating_sub(limit)..block_end].fill(true);
        }
    }

    lines
        .iter()
        .enumerate()
        .filter(|(idx, _)| keep[*idx])
        .map(|(_, v)| v.clone())
        .collect()
}

// ── Shell output helpers ──────────────────────────────────────────────────────

/// Collect lines for shell output rendering.
/// Returns `(lines, is_truncated)` where each entry is `(content, is_stderr)`.
/// In compact mode, returns the tail of at most `max_lines` lines total.
/// Returns `(lines, hidden_count)` where `hidden_count > 0` means the output was trimmed.
/// When truncated, the tail (`max_lines` lines) is returned.
pub fn collect_shell_lines(
    state: &ShellOutputState,
    max_lines: Option<usize>,
) -> (Vec<(String, bool)>, usize) {
    let mut all: Vec<(String, bool)> = Vec::new();
    for line in state.stderr.lines() {
        all.push((line.to_string(), true));
    }
    for line in state.stdout.lines() {
        all.push((line.to_string(), false));
    }

    match max_lines {
        None => (all, 0),
        Some(limit) => {
            if all.len() <= limit {
                (all, 0)
            } else {
                let hidden = all.len() - limit;
                (all[hidden..].to_vec(), hidden)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn make_hunk(lines: &[&str]) -> PatchHunk {
        PatchHunk {
            lines: lines.iter().map(|s| s.to_string()).collect(),
            old_start: 1,
            old_lines: lines.iter().filter(|l| !l.starts_with('+')).count() as u32,
            new_start: 1,
            new_lines: lines.iter().filter(|l| !l.starts_with('-')).count() as u32,
        }
    }

    fn make_fd_state(hunks: Vec<PatchHunk>) -> ToolResultUiState {
        ToolResultUiState::file_delta("src/foo.rs".to_string(), hunks, None)
    }

    // ── Parsing / detection ───────────────────────────────────────────────────

    #[test]
    fn enricher_detects_file_delta() {
        let hunk = make_hunk(&[" ctx", "-old", "+new"]);
        let state = ToolResultUiState::file_delta("a.rs".to_string(), vec![hunk], None);
        assert!(matches!(state.payload, ToolResultPayload::FileDelta(_)));
    }

    #[test]
    fn enricher_detects_shell_output() {
        let state = ToolResultUiState::shell_output("err".to_string(), "out".to_string());
        assert!(matches!(state.payload, ToolResultPayload::ShellOutput(_)));
    }

    // ── Context filtering ─────────────────────────────────────────────────────

    #[test]
    fn context_limit_0_strips_all_context() {
        let hunk = make_hunk(&[" ctx1", " ctx2", "+added", " ctx3", " ctx4"]);
        let (lines, _) = build_diff_lines(&hunk, true, Some(0));
        assert!(lines.iter().all(|l| l.kind != DiffLineKind::Context));
        assert_eq!(lines.len(), 1); // only the added line
    }

    #[test]
    fn context_limit_2_keeps_at_most_2_per_block() {
        // 4 context, then a change, then 4 context
        let hunk = make_hunk(&[
            " c1", " c2", " c3", " c4", "+add", " c5", " c6", " c7", " c8",
        ]);
        let (lines, _) = build_diff_lines(&hunk, true, Some(2));
        let ctx_count = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Context)
            .count();
        // At most 2 from start of first block + 2 from end of first block = up to 4 (but block is 4 so we get 4)
        // Before change: 4 ctx → keep last 2 (since it's just before a change block, we want the 2 closest)
        // After change: 4 ctx → keep first 2 (closest to the change)
        // Total: 4 context lines
        assert_eq!(ctx_count, 4);
    }

    #[test]
    fn context_limit_none_keeps_all() {
        let hunk = make_hunk(&[" c1", " c2", " c3", "+add"]);
        let (lines, _) = build_diff_lines(&hunk, true, None);
        let ctx_count = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Context)
            .count();
        assert_eq!(ctx_count, 3);
    }

    // ── Truncation thresholds ─────────────────────────────────────────────────

    #[test]
    fn eleven_changed_lines_triggers_new_version_mode() {
        // 11 removed + 11 added = 22 changed total → new-version mode (strip removed)
        let mut line_data: Vec<&str> = Vec::new();
        for _ in 0..11 {
            line_data.push("-removed");
            line_data.push("+added");
        }
        let hunk = make_hunk(&line_data);
        let (lines, _) = build_diff_lines(&hunk, false, None);
        // No removed lines should appear; all added lines should be Changed (new-version mode).
        assert!(lines.iter().all(|l| l.kind != DiffLineKind::Removed));
        assert!(lines.iter().all(|l| l.kind != DiffLineKind::Added));
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind == DiffLineKind::Changed)
                .count(),
            11
        );
    }

    #[test]
    fn ten_changed_lines_keeps_full_diff() {
        let mut line_data: Vec<&str> = Vec::new();
        for _ in 0..5 {
            line_data.push("-removed");
            line_data.push("+added");
        }
        let hunk = make_hunk(&line_data);
        let (lines, _) = build_diff_lines(&hunk, false, None);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind == DiffLineKind::Removed)
                .count(),
            5
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind == DiffLineKind::Added)
                .count(),
            5
        );
    }

    #[test]
    fn thirty_one_added_lines_truncated_to_30() {
        // 31 added lines: should be truncated
        let mut line_data: Vec<&str> = Vec::new();
        // make changed count > 10 to trigger new-version mode
        for _ in 0..31 {
            line_data.push("-old");
            line_data.push("+new");
        }
        let hunk = make_hunk(&line_data);
        let (lines, hidden) = build_diff_lines(&hunk, false, None);
        assert!(hidden > 0);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind == DiffLineKind::Changed)
                .count(),
            30
        );
    }

    #[test]
    fn thirty_added_lines_not_truncated() {
        let mut line_data: Vec<&str> = Vec::new();
        for _ in 0..30 {
            line_data.push("-old");
            line_data.push("+new");
        }
        let hunk = make_hunk(&line_data);
        let (lines, hidden) = build_diff_lines(&hunk, false, None);
        assert_eq!(hidden, 0);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind == DiffLineKind::Changed)
                .count(),
            30
        );
    }

    #[test]
    fn pure_deletion_shows_removed_lines() {
        // >10 removed lines with no added lines must still display (not be hidden).
        let lines: Vec<&str> = (0..12).flat_map(|_| ["-removed"].into_iter()).collect();
        let hunk = make_hunk(&lines);
        let (result, _) = build_diff_lines(&hunk, false, None);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.kind == DiffLineKind::Removed)
                .count(),
            12
        );
    }

    #[test]
    fn changed_mode_context_line_numbers_are_correct() {
        // Force new-version mode by providing a large changed_count via show_full=false
        // with 11+ changes. Here we have only 2 changed lines (<=10), so use a bigger hunk
        // to trigger new-version mode. Build one with 6 pairs (12 changed).
        let mut big_lines = vec![" ctx".to_string()];
        for _ in 0..6 {
            big_lines.push("-old".to_string());
            big_lines.push("+new".to_string());
        }
        big_lines.push(" after".to_string());
        let big_hunk = PatchHunk {
            lines: big_lines,
            old_start: 10,
            old_lines: 8, // 1 ctx + 6 removed + 1 after
            new_start: 10,
            new_lines: 8, // 1 ctx + 6 added + 1 after
        };
        let (result, _) = build_diff_lines(&big_hunk, false, None);
        // Context line after the changed block: old should be 10+1+6=17, new should be 10+1+6=17
        let after = result.last().unwrap();
        assert_eq!(after.kind, DiffLineKind::Context);
        assert_eq!(after.old_num, Some(17));
        assert_eq!(after.new_num, Some(17));
    }

    #[test]
    fn mixed_blocks_pure_deletion_visible() {
        // One hunk containing: a pure-deletion block (11 lines), context, and a replacement block.
        // With per-hunk logic the deletion block was hidden (hunk had adds overall).
        // With per-block logic each block is decided independently.
        let mut lines: Vec<&str> = Vec::new();
        for _ in 0..11 {
            lines.push("-deleted");
        }
        lines.push(" context");
        for _ in 0..6 {
            lines.push("-old");
            lines.push("+new");
        }
        let hunk = make_hunk(&lines);
        let (result, _) = build_diff_lines(&hunk, false, None);

        // Block 1: 11 removes, 0 adds → pure deletion → full diff → Removed
        let removed = result
            .iter()
            .filter(|l| l.kind == DiffLineKind::Removed)
            .count();
        assert_eq!(removed, 11, "pure-deletion block must remain visible");

        // Block 2: 12 lines, 6 adds → >10 and adds present → new-version mode → Changed
        let changed = result
            .iter()
            .filter(|l| l.kind == DiffLineKind::Changed)
            .count();
        assert_eq!(changed, 6);

        // No line should be RemovedHidden (it must not escape the builder)
        assert!(result.iter().all(|l| l.kind != DiffLineKind::RemovedHidden));
    }

    #[test]
    fn space_toggles_expanded() {
        let h = make_hunk(&["+a"]);
        let mut state = make_fd_state(vec![h]);
        assert!(!state.expanded);
        state.handle_key(press(KeyCode::Char(' ')));
        assert!(state.expanded);
        state.handle_key(press(KeyCode::Char(' ')));
        assert!(!state.expanded);
    }

    #[test]
    fn esc_exits_interaction() {
        let h = make_hunk(&["+a"]);
        let mut state = make_fd_state(vec![h]);
        assert!(matches!(
            state.handle_key(press(KeyCode::Esc)),
            ComponentKeyResult::ExitInteraction
        ));
    }

    #[test]
    fn ctrl_c_exits_interaction() {
        let h = make_hunk(&["+a"]);
        let mut state = make_fd_state(vec![h]);
        assert!(matches!(
            state.handle_key(press_ctrl(KeyCode::Char('c'))),
            ComponentKeyResult::ExitInteraction
        ));
    }

    #[test]
    fn ctrl_n_passthrough() {
        let h = make_hunk(&["+a"]);
        let mut state = make_fd_state(vec![h]);
        assert!(matches!(
            state.handle_key(press_ctrl(KeyCode::Char('n'))),
            ComponentKeyResult::Passthrough
        ));
    }

    // ── Copy ──────────────────────────────────────────────────────────────────

    #[test]
    fn yy_copies_unified_diff() {
        let hunk = make_hunk(&[" ctx", "-old", "+new"]);
        let mut state = ToolResultUiState::file_delta("src/foo.rs".to_string(), vec![hunk], None);
        state.handle_key(press(KeyCode::Char('y'))); // pending_y set
        let r = state.handle_key(press(KeyCode::Char('y')));
        if let ComponentKeyResult::Copy { content } = r {
            assert!(content.contains("--- a/src/foo.rs"));
            assert!(content.contains("+++ b/src/foo.rs"));
            assert!(content.contains("@@"));
            assert!(content.contains("-old"));
            assert!(content.contains("+new"));
        } else {
            panic!("expected Copy result");
        }
    }

    #[test]
    fn capital_y_copies_unified_diff() {
        let hunk = make_hunk(&["+line"]);
        let mut state = ToolResultUiState::file_delta("a.rs".to_string(), vec![hunk], None);
        let r = state.handle_key(press(KeyCode::Char('Y')));
        assert!(matches!(r, ComponentKeyResult::Copy { .. }));
    }

    #[test]
    fn copy_always_includes_all_hunks() {
        let h1 = make_hunk(&["+hunk1"]);
        let h2 = make_hunk(&["+hunk2"]);
        let mut state = ToolResultUiState::file_delta("f.rs".to_string(), vec![h1, h2], None);
        let r = state.handle_key(press(KeyCode::Char('Y')));
        if let ComponentKeyResult::Copy { content } = r {
            assert!(content.contains("+hunk1"));
            assert!(content.contains("+hunk2"));
        } else {
            panic!("expected Copy result");
        }
    }

    // ── Context adjustment ────────────────────────────────────────────────────

    #[test]
    fn minus_from_none_steps_down_from_max() {
        let hunk = make_hunk(&[" c1", " c2", " c3", "+add"]);
        let mut state = make_fd_state(vec![hunk]);
        // context_lines starts as None (show all → 3 context lines)
        state.handle_key(press(KeyCode::Char('-')));
        if let ToolResultPayload::FileDelta(fd) = &state.payload {
            assert_eq!(fd.context_lines, Some(2)); // max=3, step to 2
        }
    }

    #[test]
    fn equals_from_max_snaps_to_none() {
        let hunk = make_hunk(&[" c1", " c2", "+add"]);
        let mut state = make_fd_state(vec![hunk]);
        // max_context = 2; set context_lines = Some(2) manually
        if let ToolResultPayload::FileDelta(fd) = &mut state.payload {
            fd.context_lines = Some(2);
        }
        state.handle_key(press(KeyCode::Char('=')));
        if let ToolResultPayload::FileDelta(fd) = &state.payload {
            assert_eq!(fd.context_lines, None); // snapped back
        }
    }
}
