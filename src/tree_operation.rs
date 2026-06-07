use crate::tree_scroll_view::state::MessageState;

#[derive(Clone)]
pub enum TreeOperation {
    /// Insert `message` as a child of the node with the given ID.
    /// If `parent_id` is None, append at the top level.
    Append {
        parent_id: Option<String>,
        message: MessageState,
    },
    /// Replace the node with the given ID (and all its descendants) with `message`.
    Replace { id: String, message: MessageState },
    /// Remove the node with the given ID and all its descendants.
    Remove { id: String },
    /// Patch the fields of the node with the given ID in-place.
    /// `message.children` must be empty; existing children in the tree are preserved.
    Update { id: String, message: MessageState },
}
