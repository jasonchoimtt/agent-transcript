use ansi_to_tui::IntoText;
use ratatui::text::Text;
use regex::Regex;

use super::cursor::TreeCursor;
use super::markdown::render_markdown;
use super::state::{
    MessageState, Precedence, TreeScrollViewState, get_node, get_node_mut, measure_text_height,
};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Committed search state — set when the user presses Enter.
pub struct SearchState {
    pub query: String,
    pub backward: bool,
    pub found_path: Vec<usize>,
    pub found_char_index: usize,
    /// Char length of the actual regex match (may differ from query char count due to case folding).
    pub found_match_len: usize,
}

/// In-progress search state — held while the user is typing in SearchInput mode.
pub struct PendingSearch {
    pub query: String,
    pub backward: bool,
    /// Viewport position captured the moment `/` or `?` was pressed.
    pub start_top_index: Vec<usize>,
    pub start_top_offset: u16,
    /// Empty path means no match was found.
    pub found_path: Vec<usize>,
    pub found_char_index: usize,
    /// Char length of the actual regex match.
    pub found_match_len: usize,
}

/// Passed to `MessageWidget` and the prose renderer to highlight a match.
#[derive(Clone)]
pub struct SearchHighlight {
    pub char_index: usize,
    pub query_len: usize,
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Concatenate all rendered span contents with `\n` between `Text::Line`s.
fn plaintext_of_rendered(rendered: &Text<'_>) -> String {
    let mut result = String::new();
    for (i, line) in rendered.lines.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        for span in &line.spans {
            result.push_str(&span.content);
        }
    }
    result
}

/// Build the case-insensitive regex for `needle`, or return `None` on error.
fn make_pattern(needle: &str) -> Option<Regex> {
    Regex::new(&format!("(?i){}", regex::escape(needle))).ok()
}

/// First case-insensitive match anywhere in `haystack`.
/// Returns `(char_index, match_char_len)`.
fn search_in_plaintext(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    search_in_plaintext_after(haystack, needle, 0)
}

/// First match whose char_index is >= `start_char`.
fn search_in_plaintext_after(
    haystack: &str,
    needle: &str,
    start_char: usize,
) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let pattern = make_pattern(needle)?;
    // Convert char offset to byte offset for slicing.
    let byte_start = haystack
        .char_indices()
        .nth(start_char)
        .map(|(b, _)| b)
        .unwrap_or(haystack.len());
    let m = pattern.find(&haystack[byte_start..])?;
    let char_index = haystack[..byte_start + m.start()].chars().count();
    let match_char_len = haystack[byte_start + m.start()..byte_start + m.end()]
        .chars()
        .count();
    Some((char_index, match_char_len))
}

/// Last match whose char_index is strictly less than `before_char`.
/// Iterates all matches to find the latest one, which is required for correct
/// backward navigation within a node.
fn search_in_plaintext_before(
    haystack: &str,
    needle: &str,
    before_char: usize,
) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let pattern = make_pattern(needle)?;
    // Track byte and char positions incrementally to avoid O(N²) char counting.
    let mut char_pos = 0usize;
    let mut byte_pos = 0usize;
    // Store byte offsets of the last qualifying match; char conversion happens once after the loop.
    let mut last: Option<(usize, usize, usize)> = None; // (char_index, byte_start, byte_end)
    for m in pattern.find_iter(haystack) {
        char_pos += haystack[byte_pos..m.start()].chars().count();
        if char_pos >= before_char {
            break;
        }
        last = Some((char_pos, m.start(), m.end()));
        char_pos += haystack[m.start()..m.end()].chars().count();
        byte_pos = m.end();
    }
    let (char_index, bs, be) = last?;
    Some((char_index, haystack[bs..be].chars().count()))
}

/// Last match anywhere in `haystack`.
fn search_in_plaintext_last(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    search_in_plaintext_before(haystack, needle, usize::MAX)
}

/// Find which visual line `target_char` (in rendered-plaintext char space) falls on,
/// given `rendered` text and the available column width for wrapping.
fn char_to_visual_line(rendered: &Text<'_>, target_char: usize, available: u16) -> u16 {
    let mut visual_line_acc = 0u16;
    let mut char_acc = 0usize;

    for line in &rendered.lines {
        let line_content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let line_char_count = line_content.chars().count();
        let end_char = char_acc + line_char_count;

        if target_char <= end_char {
            let offset_in_line = target_char - char_acc;
            if offset_in_line == 0 {
                return visual_line_acc;
            }
            let prefix: String = line_content.chars().take(offset_in_line).collect();
            let h = measure_text_height(&Text::raw(prefix), available);
            return visual_line_acc + h.saturating_sub(1);
        }

        let h = measure_text_height(&Text::raw(line_content), available).max(1);
        visual_line_acc += h;
        char_acc = end_char + 1; // +1 for the \n separator between lines
    }

    visual_line_acc
}

/// Apply a search highlight to a rendered `Text`, splitting spans at the match boundaries.
/// Returns a `Text<'static>` with the matched region styled with yellow-on-black.
pub fn highlight_text_spans(text: Text<'_>, char_index: usize, query_len: usize) -> Text<'static> {
    use ratatui::style::Color;
    let highlight_style = ratatui::style::Style::default()
        .bg(Color::Yellow)
        .fg(Color::Black);
    let start = char_index;
    let end = char_index + query_len;

    let mut char_count = 0usize;
    let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();

    for line in text.lines {
        let mut new_spans: Vec<ratatui::text::Span<'static>> = Vec::new();

        for span in line.spans {
            let content: &str = &span.content;
            let span_len = content.chars().count();
            let span_start = char_count;
            let span_end = span_start + span_len;

            if span_end <= start || span_start >= end {
                // Outside the highlight range
                new_spans.push(ratatui::text::Span::styled(content.to_owned(), span.style));
            } else {
                let hl_start_in_span = start.saturating_sub(span_start);
                let hl_end_in_span = (end - span_start).min(span_len);

                if hl_start_in_span > 0 {
                    let before: String = content.chars().take(hl_start_in_span).collect();
                    new_spans.push(ratatui::text::Span::styled(before, span.style));
                }

                let hl_text: String = content
                    .chars()
                    .skip(hl_start_in_span)
                    .take(hl_end_in_span - hl_start_in_span)
                    .collect();
                new_spans.push(ratatui::text::Span::styled(hl_text, highlight_style));

                if hl_end_in_span < span_len {
                    let after: String = content.chars().skip(hl_end_in_span).collect();
                    new_spans.push(ratatui::text::Span::styled(after, span.style));
                }
            }

            char_count += span_len;
        }

        char_count += 1; // newline separator between lines
        lines.push(ratatui::text::Line::from(new_spans));
    }

    Text::from(lines)
}

// ── impl TreeScrollViewState ──────────────────────────────────────────────────

impl TreeScrollViewState {
    /// Render a node's text as `Text` using the same markdown/plain branch as `size_node`.
    fn node_rendered_text<'a>(&'a self, node: &'a MessageState) -> Text<'a> {
        let text = node.text.as_deref().unwrap_or("");
        let is_md = self
            .theme
            .style_for(&node.message_type)
            .uses_markdown(node.tag.as_deref());
        if is_md {
            render_markdown(text, &self.theme.palette)
        } else {
            text.into_text().unwrap_or_else(|_| Text::raw(text))
        }
    }

    /// Concatenate rendered span content for searching.
    fn node_plaintext(&self, node: &MessageState) -> String {
        plaintext_of_rendered(&self.node_rendered_text(node))
    }

    fn node_is_searchable(node: &MessageState) -> bool {
        !node.is_terminal && !node.group && !node.hidden.is_hidden() && node.text.is_some()
    }

    /// First match in node. Returns `(path, char_index, match_char_len)`.
    fn try_match_node(&self, path: &[usize], query: &str) -> Option<(Vec<usize>, usize, usize)> {
        let node = get_node(&self.items, path)?;
        if !Self::node_is_searchable(node) {
            return None;
        }
        search_in_plaintext(&self.node_plaintext(node), query)
            .map(|(ci, ml)| (path.to_vec(), ci, ml))
    }

    /// First match at or after `start_char` in node.
    fn try_match_node_after(
        &self,
        path: &[usize],
        query: &str,
        start_char: usize,
    ) -> Option<(Vec<usize>, usize, usize)> {
        let node = get_node(&self.items, path)?;
        if !Self::node_is_searchable(node) {
            return None;
        }
        search_in_plaintext_after(&self.node_plaintext(node), query, start_char)
            .map(|(ci, ml)| (path.to_vec(), ci, ml))
    }

    /// Last match with char_index < `before_char` in node.
    fn try_match_node_before(
        &self,
        path: &[usize],
        query: &str,
        before_char: usize,
    ) -> Option<(Vec<usize>, usize, usize)> {
        let node = get_node(&self.items, path)?;
        if !Self::node_is_searchable(node) {
            return None;
        }
        search_in_plaintext_before(&self.node_plaintext(node), query, before_char)
            .map(|(ci, ml)| (path.to_vec(), ci, ml))
    }

    /// Last match anywhere in node.
    fn try_match_node_last(
        &self,
        path: &[usize],
        query: &str,
    ) -> Option<(Vec<usize>, usize, usize)> {
        let node = get_node(&self.items, path)?;
        if !Self::node_is_searchable(node) {
            return None;
        }
        search_in_plaintext_last(&self.node_plaintext(node), query)
            .map(|(ci, ml)| (path.to_vec(), ci, ml))
    }

    /// Find the first node path in search-DFS order (first non-hidden top-level node).
    fn search_first_path(&self) -> Option<Vec<usize>> {
        let i = self.items.iter().position(|n| !n.hidden.is_hidden())?;
        Some(vec![i])
    }

    /// DFS search traversal. `start_inclusive` controls whether the start node is checked.
    /// `prefer_last` selects the last occurrence per node instead of the first; used when
    /// navigating backward so that entering a node from the "end" lands on its last match.
    /// Returns `(path, char_index, match_char_len)` for the first qualifying match, wrapping once.
    fn do_search(
        &self,
        query: &str,
        backward: bool,
        start_path: &[usize],
        start_inclusive: bool,
        prefer_last: bool,
    ) -> Option<(Vec<usize>, usize, usize)> {
        if query.is_empty() {
            return None;
        }

        let mut cur = TreeCursor::at(&self.items, start_path.to_vec())?;

        if start_inclusive {
            let result = if prefer_last {
                self.try_match_node_last(cur.path(), query)
            } else {
                self.try_match_node(cur.path(), query)
            };
            if result.is_some() {
                return result;
            }
        }

        let mut wrapped = false;
        loop {
            let stepped = if backward {
                cur.retreat_search(&self.items)
            } else {
                cur.advance_search(&self.items)
            };

            if !stepped {
                if wrapped {
                    return None;
                }
                wrapped = true;
                cur = if backward {
                    TreeCursor::search_last(&self.items)?
                } else {
                    let first = self.search_first_path()?;
                    TreeCursor::at(&self.items, first)?
                };
            }

            // Stop when we've circled back to the start node.
            if wrapped && cur.path() == start_path {
                return None;
            }

            let result = if prefer_last {
                self.try_match_node_last(cur.path(), query)
            } else {
                self.try_match_node(cur.path(), query)
            };
            if result.is_some() {
                return result;
            }
        }
    }

    /// Walk from `path` toward the root, returning the deepest path reachable
    /// through the current expanded state (i.e., the deepest visible ancestor).
    fn find_visible_ancestor(&self, path: &[usize]) -> Vec<usize> {
        let mut result = Vec::new();
        let mut items = &self.items[..];

        for (i, &idx) in path.iter().enumerate() {
            let Some(node) = items.get(idx) else { break };
            if node.hidden.is_hidden() {
                break;
            }
            result.push(idx);
            if i + 1 == path.len() {
                break;
            }
            if !node.expanded {
                break;
            }
            items = &node.children;
        }

        result
    }

    /// Set `show_more = true` on the node at `path` so the full content is visible.
    fn show_found_node(&mut self, path: &[usize]) {
        if let Some(node) = get_node_mut(&mut self.items, path)
            && !node.show_more
        {
            node.show_more = true;
            node.height = None;
        }
    }

    /// Expand all non-leaf ancestors of `path` and clear their cached heights.
    fn expand_ancestors(&mut self, path: &[usize]) {
        for prefix_len in 1..path.len() {
            let prefix = &path[..prefix_len];
            if let Some(node) = get_node_mut(&mut self.items, prefix)
                && !node.expanded
            {
                node.expanded = true;
                node.height = None;
            }
        }
        // Also rebuild id_to_path in case new nodes became reachable.
        // (expand_ancestors is called before selection changes, so paths remain valid.)
    }

    /// Compute the `Precedence::InnerFocus` line range for a match at `char_index`
    /// with `match_len` chars, using word-wrap–accurate visual line counting.
    fn compute_line_range(
        &self,
        path: &[usize],
        char_index: usize,
        match_len: usize,
    ) -> (u16, u16) {
        let Some(node) = get_node(&self.items, path) else {
            return (0, 1);
        };
        let depth = TreeCursor::at(&self.items, path.to_vec())
            .map(|c| c.depth())
            .unwrap_or(0);
        let prefix_len = (depth * 2 + 2) as u16;
        let available = self
            .viewport_width
            .saturating_sub(1)
            .saturating_sub(prefix_len);

        if available == 0 {
            return (0, 1);
        }

        let rendered = self.node_rendered_text(node);
        let start_line = char_to_visual_line(&rendered, char_index, available);
        let end_line = char_to_visual_line(&rendered, char_index + match_len, available);
        (start_line, end_line + 1)
    }

    /// Called on each keystroke while in `SearchInput` mode.
    /// Searches from the captured start position and scrolls the viewport to preview.
    pub fn search_pending(&mut self, query: &str, backward: bool) {
        let (start_top_index, start_top_offset) = match &self.pending_search {
            Some(ps) => (ps.start_top_index.clone(), ps.start_top_offset),
            None => (self.top_index.clone(), self.top_offset),
        };

        let result = self.do_search(query, backward, &start_top_index.clone(), true, false);

        match result {
            Some((found_path, char_index, match_len)) => {
                let visible_path = self.find_visible_ancestor(&found_path);
                let is_direct = visible_path == found_path;
                let (focus_path, line_range) = if is_direct {
                    let lr = self.compute_line_range(&found_path, char_index, match_len);
                    (found_path.clone(), lr)
                } else {
                    (visible_path, (0u16, 1u16))
                };

                self.pending_search = Some(PendingSearch {
                    query: query.to_owned(),
                    backward,
                    start_top_index,
                    start_top_offset,
                    found_path,
                    found_char_index: char_index,
                    found_match_len: match_len,
                });

                self.at_bottom = false;
                self.precedence = Precedence::InnerFocus {
                    path: focus_path,
                    line_range,
                };
            }
            None => {
                self.pending_search = Some(PendingSearch {
                    query: query.to_owned(),
                    backward,
                    start_top_index: start_top_index.clone(),
                    start_top_offset,
                    found_path: vec![],
                    found_char_index: 0,
                    found_match_len: 0,
                });
                // Restore viewport to the pre-search position.
                self.top_index = start_top_index;
                self.top_offset = start_top_offset;
                self.at_bottom = false;
                self.precedence = Precedence::Top;
            }
        }
    }

    /// Called on Enter: expands ancestors, moves selection, and commits the search.
    pub fn commit_search(&mut self) {
        let Some(ps) = self.pending_search.take() else {
            return;
        };
        if ps.found_path.is_empty() {
            return;
        }
        self.expand_ancestors(&ps.found_path);
        self.show_found_node(&ps.found_path);
        self.selection_index = ps.found_path.clone();
        let line_range =
            self.compute_line_range(&ps.found_path, ps.found_char_index, ps.found_match_len);
        self.search = Some(SearchState {
            query: ps.query,
            backward: ps.backward,
            found_path: ps.found_path.clone(),
            found_char_index: ps.found_char_index,
            found_match_len: ps.found_match_len,
        });
        self.precedence = Precedence::InnerFocus {
            path: ps.found_path,
            line_range,
        };
        self.at_bottom = false;
    }

    /// Called on Esc: restores the viewport to the position before search was opened.
    pub fn cancel_search(&mut self) {
        if let Some(ref ps) = self.pending_search {
            self.top_index = ps.start_top_index.clone();
            self.top_offset = ps.start_top_offset;
            self.at_bottom = false;
            self.precedence = Precedence::Top;
        }
        self.pending_search = None;
    }

    /// Navigate to the next match in the committed search direction.
    pub fn search_next(&mut self) {
        self.navigate_search(false);
    }

    /// Navigate to the previous match (opposite of committed search direction).
    pub fn search_prev(&mut self) {
        self.navigate_search(true);
    }

    /// Shared implementation for `search_next` / `search_prev`.
    /// `reverse` flips the committed search direction for this step only.
    ///
    /// For forward steps: tries the next occurrence within the current node first
    /// (after the current match end), then walks to subsequent nodes.
    /// For backward steps: tries the previous occurrence within the current node
    /// (before the current match start), then walks backward using the last
    /// occurrence in each visited node.
    fn navigate_search(&mut self, reverse: bool) {
        let Some(search) = self.search.take() else {
            return;
        };
        if search.query.is_empty() || search.found_path.is_empty() {
            self.search = Some(search);
            return;
        }
        let backward = search.backward ^ reverse;
        let selection = self.selection_index.clone();
        let anchored = selection == search.found_path;

        let result = if !anchored {
            // The user moved the selection since the last search — restart from
            // the current selection, discarding the stored char position.
            self.do_search(&search.query, backward, &selection, true, backward)
        } else if backward {
            // Try the match that comes just before the current one in the same node.
            self.try_match_node_before(&search.found_path, &search.query, search.found_char_index)
                .or_else(|| {
                    // No earlier match in this node — walk backward, taking the last
                    // occurrence in each node visited.
                    self.do_search(&search.query, true, &search.found_path, false, true)
                })
        } else {
            // Try the match that comes just after the current one in the same node.
            let after = search.found_char_index + search.found_match_len;
            self.try_match_node_after(&search.found_path, &search.query, after)
                .or_else(|| {
                    // No later match in this node — walk forward, taking the first
                    // occurrence in each node visited.
                    self.do_search(&search.query, false, &search.found_path, false, false)
                })
        };

        if let Some((found_path, char_index, match_len)) = result {
            self.expand_ancestors(&found_path);
            self.show_found_node(&found_path);
            let line_range = self.compute_line_range(&found_path, char_index, match_len);
            self.selection_index = found_path.clone();
            self.search = Some(SearchState {
                query: search.query,
                backward: search.backward,
                found_path,
                found_char_index: char_index,
                found_match_len: match_len,
            });
            self.precedence = Precedence::InnerFocus {
                path: self.selection_index.clone(),
                line_range,
            };
            self.at_bottom = false;
        } else {
            self.search = Some(search);
        }
    }

    /// Clear all search state (both pending and committed).
    pub fn clear_search(&mut self) {
        self.search = None;
        self.pending_search = None;
    }

    /// Returns the active search query (pending takes precedence over committed).
    pub fn active_search_query(&self) -> Option<&str> {
        self.pending_search
            .as_ref()
            .map(|ps| ps.query.as_str())
            .or_else(|| self.search.as_ref().map(|s| s.query.as_str()))
    }

    /// Returns the highlight for a given path, if it is the current search match.
    pub fn search_highlight_for(&self, path: &[usize]) -> Option<SearchHighlight> {
        let (found_path, char_index, match_len) = self
            .pending_search
            .as_ref()
            .filter(|ps| !ps.found_path.is_empty())
            .map(|ps| {
                (
                    ps.found_path.as_slice(),
                    ps.found_char_index,
                    ps.found_match_len,
                )
            })
            .or_else(|| {
                self.search
                    .as_ref()
                    .filter(|s| !s.found_path.is_empty())
                    .map(|s| {
                        (
                            s.found_path.as_slice(),
                            s.found_char_index,
                            s.found_match_len,
                        )
                    })
            })?;

        if found_path != path {
            return None;
        }

        Some(SearchHighlight {
            char_index,
            query_len: match_len,
        })
    }
}
