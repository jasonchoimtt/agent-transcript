use super::predicates::nonzero_height;
use super::state::{MessageState, get_node};

fn nearest_non_hidden(items: &[MessageState], start: usize) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let clamped = start.min(items.len() - 1);
    for i in (0..=clamped).rev() {
        if !items[i].hidden.is_hidden() {
            return Some(i);
        }
    }
    ((clamped + 1)..items.len()).find(|&i| !items[i].hidden.is_hidden())
}

/// Compute visual depth for `path` by counting ancestors with `indent_children: true`.
fn compute_visual_depth(root: &[MessageState], path: &[usize]) -> usize {
    let mut depth = 0;
    let mut items = root;
    for &idx in path.iter().take(path.len().saturating_sub(1)) {
        let Some(node) = items.get(idx) else { break };
        if node.indent_children {
            depth += 1;
        }
        items = &node.children;
    }
    depth
}

// ── TreeCursor ────────────────────────────────────────────────────────────────

/// Lightweight cursor into a `MessageState` tree.
///
/// Holds only a path (no reference into the tree), so it never conflicts with
/// `&mut` borrows of `items` — callers pass `root` by reference on each call.
pub struct TreeCursor {
    path: Vec<usize>,
    visual_depth: usize,
}

impl TreeCursor {
    /// Point at `path`. Returns `None` if the path does not resolve to a node.
    pub fn at(root: &[MessageState], path: Vec<usize>) -> Option<Self> {
        get_node(root, &path)?;
        let visual_depth = compute_visual_depth(root, &path);
        Some(Self { path, visual_depth })
    }

    /// Point at the first non-hidden node satisfying `predicate`.
    pub fn first(root: &[MessageState], predicate: fn(&MessageState) -> bool) -> Option<Self> {
        let i = nearest_non_hidden(root, 0)?;
        let mut cur = Self {
            path: vec![i],
            visual_depth: 0,
        };
        if predicate(get_node(root, &cur.path).unwrap()) {
            return Some(cur);
        }
        if cur.advance(root, predicate) {
            Some(cur)
        } else {
            None
        }
    }

    /// Point at the closest visible node to `path`.
    ///
    /// Walks the path segment by segment, clamping out-of-bounds indices and
    /// snapping to the nearest non-hidden sibling at each level. Stops at a
    /// node that is collapsed or has no non-hidden children, so the returned
    /// cursor always points at a visible node.
    ///
    /// After resolving, if the landing node is a zero-height expanded group,
    /// advances to its first rendered descendant.
    ///
    /// Returns `None` only when there are no visible nodes at all.
    pub fn closest(root: &[MessageState], path: &[usize]) -> Option<Self> {
        let mut items = root;
        let mut resolved: Vec<usize> = Vec::new();

        for &idx in path {
            if items.is_empty() {
                break;
            }
            let clamped = idx.min(items.len() - 1);
            let actual = match nearest_non_hidden(items, clamped) {
                Some(i) => i,
                None => break,
            };
            resolved.push(actual);
            let node = &items[actual];
            if !node.expanded || node.children.is_empty() {
                break;
            }
            items = &node.children;
        }

        let mut cur = if resolved.is_empty() {
            let i = nearest_non_hidden(root, 0)?;
            Self {
                path: vec![i],
                visual_depth: 0,
            }
        } else {
            let visual_depth = compute_visual_depth(root, &resolved);
            Self {
                path: resolved,
                visual_depth,
            }
        };

        // If the resolved node is a zero-height expanded group, advance to its
        // first rendered descendant.
        if let Some(node) = get_node(root, &cur.path)
            && !nonzero_height(node)
        {
            cur.advance(root, nonzero_height);
        }

        Some(cur)
    }

    /// Point at the last node satisfying `predicate`.
    pub fn last(root: &[MessageState], predicate: fn(&MessageState) -> bool) -> Option<Self> {
        let i = root.iter().rposition(|n| !n.hidden.is_hidden())?;
        let mut cur = Self {
            path: vec![i],
            visual_depth: 0,
        };
        cur.descend_to_last(root);
        if predicate(get_node(root, &cur.path).unwrap()) {
            return Some(cur);
        }
        if cur.retreat(root, predicate) {
            Some(cur)
        } else {
            None
        }
    }

    pub fn path(&self) -> &[usize] {
        &self.path
    }

    /// Visual depth of the current node: counts ancestors with `indent_children: true`.
    pub fn depth(&self) -> usize {
        self.visual_depth
    }

    pub fn node<'a>(&self, root: &'a [MessageState]) -> &'a MessageState {
        get_node(root, &self.path).expect("TreeCursor path must be valid")
    }

    /// Advance to the next node satisfying `predicate` in depth-first order.
    /// Returns `false` when no such node exists after the current position.
    pub fn advance(&mut self, root: &[MessageState], predicate: fn(&MessageState) -> bool) -> bool {
        while self.advance_one(root) {
            if predicate(get_node(root, &self.path).unwrap()) {
                return true;
            }
        }
        false
    }

    /// Retreat to the previous node satisfying `predicate` in depth-first order.
    /// Returns `false` when no such node exists before the current position.
    pub fn retreat(&mut self, root: &[MessageState], predicate: fn(&MessageState) -> bool) -> bool {
        while self.retreat_one(root) {
            if predicate(get_node(root, &self.path).unwrap()) {
                return true;
            }
        }
        false
    }

    /// Advance past the current subtree, skipping to the next visible node.
    ///
    /// Tries the next non-hidden sibling at the current level first (groups are
    /// acceptable). If none exists, backtracks through ancestors; the first
    /// non-hidden candidate must satisfy `nonzero_height` — if not, advances
    /// past it via `advance(nonzero_height)`.
    pub fn advance_sibling(&mut self, root: &[MessageState]) -> bool {
        let i = match self.path.last() {
            Some(&i) => i,
            None => return false,
        };
        let siblings: &[MessageState] = if self.path.len() == 1 {
            root
        } else {
            &get_node(root, &self.path[..self.path.len() - 1])
                .unwrap()
                .children
        };

        // Step 1: next non-hidden sibling at the same level (groups are fine).
        if let Some(offset) = siblings[i + 1..].iter().position(|s| !s.hidden.is_hidden()) {
            *self.path.last_mut().unwrap() = i + 1 + offset;
            return true;
        }

        // Step 2: no next sibling — backtrack through ancestors, require
        // nonzero_height on the landing node (advance past it if not).
        loop {
            let i = match self.path.pop() {
                Some(i) => i,
                None => return false,
            };
            if !self.path.is_empty()
                && get_node(root, &self.path)
                    .map(|n| n.indent_children)
                    .unwrap_or(false)
            {
                self.visual_depth = self.visual_depth.saturating_sub(1);
            }
            let siblings: &[MessageState] = if self.path.is_empty() {
                root
            } else {
                &get_node(root, &self.path).unwrap().children
            };
            if let Some(offset) = siblings[i + 1..].iter().position(|s| !s.hidden.is_hidden()) {
                self.path.push(i + 1 + offset);
                if nonzero_height(self.node(root)) {
                    return true;
                }
                return self.advance(root, nonzero_height);
            }
        }
    }

    /// Retreat to the previous non-hidden sibling (without descending into it),
    /// or to the parent if there is no previous sibling. Used for level-only
    /// navigation when the cursor is sitting on a group node.
    pub fn retreat_sibling(&mut self, root: &[MessageState]) -> bool {
        let i = match self.path.last() {
            Some(&i) => i,
            None => return false,
        };
        let siblings: &[MessageState] = if self.path.len() == 1 {
            root
        } else {
            &get_node(root, &self.path[..self.path.len() - 1])
                .unwrap()
                .children
        };
        if let Some(prev_i) = siblings[..i].iter().rposition(|s| !s.hidden.is_hidden()) {
            *self.path.last_mut().unwrap() = prev_i;
            return true;
        }
        // No previous sibling: move to parent.
        if self.path.len() == 1 {
            return false;
        }
        let parent_len = self.path.len() - 1;
        if get_node(root, &self.path[..parent_len])
            .map(|n| n.indent_children)
            .unwrap_or(false)
        {
            self.visual_depth = self.visual_depth.saturating_sub(1);
        }
        self.path.pop();
        true
    }

    /// Step to the next node matching `include` in DFS order. Returns `false` at end.
    pub fn advance_one_with(
        &mut self,
        root: &[MessageState],
        include: fn(&MessageState) -> bool,
    ) -> bool {
        // 1. Descend into first child matching `include` if expanded.
        {
            let node = get_node(root, &self.path).unwrap();
            if node.expanded
                && let Some(ci) = node.children.iter().position(include)
            {
                if node.indent_children {
                    self.visual_depth += 1;
                }
                self.path.push(ci);
                return true;
            }
        }

        // 2. Backtrack until we find a next sibling matching `include`.
        loop {
            let i = match self.path.pop() {
                Some(i) => i,
                None => return false,
            };
            let siblings: &[MessageState] = if self.path.is_empty() {
                root
            } else {
                &get_node(root, &self.path).unwrap().children
            };
            if let Some(offset) = siblings[i + 1..].iter().position(include) {
                self.path.push(i + 1 + offset);
                return true;
            }
            // No sibling at this level: we're genuinely ascending. Adjust depth.
            if !self.path.is_empty()
                && get_node(root, &self.path)
                    .map(|n| n.indent_children)
                    .unwrap_or(false)
            {
                self.visual_depth = self.visual_depth.saturating_sub(1);
            }
        }
    }

    /// Step to the previous node matching `include` in DFS order. Returns `false` at start.
    pub fn retreat_one_with(
        &mut self,
        root: &[MessageState],
        include: fn(&MessageState) -> bool,
    ) -> bool {
        let i = match self.path.last() {
            Some(&i) => i,
            None => return false,
        };

        let siblings: &[MessageState] = if self.path.len() == 1 {
            root
        } else {
            &get_node(root, &self.path[..self.path.len() - 1])
                .unwrap()
                .children
        };

        if let Some(prev_i) = siblings[..i].iter().rposition(include) {
            *self.path.last_mut().unwrap() = prev_i;
            self.descend_to_last_with(root, include);
            return true;
        }

        if self.path.len() == 1 {
            return false;
        }
        let parent_len = self.path.len() - 1;
        if get_node(root, &self.path[..parent_len])
            .map(|n| n.indent_children)
            .unwrap_or(false)
        {
            self.visual_depth = self.visual_depth.saturating_sub(1);
        }
        self.path.pop();
        true
    }

    fn descend_to_last_with(&mut self, root: &[MessageState], include: fn(&MessageState) -> bool) {
        loop {
            let node = get_node(root, &self.path).unwrap();
            if !node.expanded || node.children.is_empty() {
                return;
            }
            match node.children.iter().rposition(include) {
                Some(last_i) => {
                    if node.indent_children {
                        self.visual_depth += 1;
                    }
                    self.path.push(last_i);
                }
                None => return,
            }
        }
    }

    fn advance_one(&mut self, root: &[MessageState]) -> bool {
        self.advance_one_with(root, |n| !n.hidden.is_hidden())
    }

    fn retreat_one(&mut self, root: &[MessageState]) -> bool {
        self.retreat_one_with(root, |n| !n.hidden.is_hidden())
    }

    fn descend_to_last(&mut self, root: &[MessageState]) {
        self.descend_to_last_with(root, |n| !n.hidden.is_hidden())
    }

    /// Count `Hidden`-state nodes in DFS order between the current position and
    /// the next non-hidden node. Used to render the braille indicator in the padding row.
    pub fn count_hidden_to_next(&self, root: &[MessageState]) -> usize {
        use super::state::HiddenState;
        let mut probe = Self {
            path: self.path.clone(),
            visual_depth: self.visual_depth,
        };
        let mut count = 0;
        loop {
            if !probe.advance_one_with(root, |_| true) {
                break;
            }
            let node = match get_node(root, &probe.path) {
                Some(n) => n,
                None => break,
            };
            if node.hidden == HiddenState::Hidden {
                count += 1;
            } else {
                break;
            }
        }
        count
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_scroll_view::state::MessageState;

    fn leaf(id: &str) -> MessageState {
        MessageState::new(id).text(id).expanded(true)
    }

    fn expanded(id: &str, children: Vec<MessageState>) -> MessageState {
        MessageState::new(id)
            .text(id)
            .expanded(true)
            .children(children)
    }

    /// Siblings of an `indent_children=true` parent must all be rendered at the
    /// same depth. Previously `advance_one` decremented depth before checking for
    /// a next sibling, so sibling[1..] of such a parent appeared one level too shallow.
    #[test]
    fn siblings_under_indent_parent_have_same_depth() {
        let root = vec![
            expanded("parent", vec![leaf("c0"), leaf("c1"), leaf("c2")]).indent_children(true),
        ];

        // Walk all nodes and collect depths.
        let mut cur = TreeCursor::at(&root, vec![0]).unwrap();
        let mut depths: Vec<(String, usize)> = vec![(cur.node(&root).id.clone(), cur.depth())];
        while cur.advance(&root, |_| true) {
            depths.push((cur.node(&root).id.clone(), cur.depth()));
        }

        let parent_depth = depths.iter().find(|(id, _)| id == "parent").unwrap().1;
        for child_id in ["c0", "c1", "c2"] {
            let child_depth = depths.iter().find(|(id, _)| id == child_id).unwrap().1;
            assert_eq!(
                child_depth,
                parent_depth + 1,
                "child '{child_id}' should be one level deeper than parent"
            );
        }
    }

    /// After leaving a subtree nested under an `indent_children=true` parent, the
    /// cursor must return to the correct depth for subsequent siblings.
    #[test]
    fn depth_restored_correctly_after_leaving_indent_parent() {
        // Structure: A (indent=true) { B, C }, D
        let root = vec![
            expanded("A", vec![leaf("B"), leaf("C")]).indent_children(true),
            leaf("D"),
        ];

        let mut cur = TreeCursor::at(&root, vec![0]).unwrap(); // A, depth 0
        let mut seen: Vec<(String, usize)> = vec![(cur.node(&root).id.clone(), cur.depth())];
        while cur.advance(&root, |_| true) {
            seen.push((cur.node(&root).id.clone(), cur.depth()));
        }

        let depth_of = |id: &str| seen.iter().find(|(i, _)| i == id).unwrap().1;
        assert_eq!(depth_of("A"), 0);
        assert_eq!(depth_of("B"), 1);
        assert_eq!(depth_of("C"), 1);
        assert_eq!(
            depth_of("D"),
            0,
            "D is a sibling of A, should be at depth 0"
        );
    }
}
