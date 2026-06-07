use super::state::MessageState;

/// Nodes that actually occupy screen lines: non-hidden, and not an expanded group
/// (which is invisible at zero height). Used as the default predicate for all
/// viewport arithmetic and DFS navigation.
pub fn nonzero_height(s: &MessageState) -> bool {
    !s.hidden.is_hidden() && (!s.group || !s.expanded)
}
