use crate::tree_operation::TreeOperation;

/// Top-level operation type that flows from transcript readers through the
/// transform pipeline to `TreeScrollViewState`.
///
/// `TreeOperation` covers pure tree mutations (Append/Replace/Remove/Update).
/// `ReaderOp` wraps those and adds the reset lifecycle variants, which the
/// pipeline handles specially — transforms never see `Reset` or `ResetDone`.
#[allow(clippy::large_enum_variant)]
pub enum ReaderOp {
    /// A pure tree mutation; forwarded to the transform pipeline as-is.
    Tree(TreeOperation),
    /// Clears all content nodes and resets viewport/selection state.
    /// `id` is the UUID of the entry that triggered the reset (e.g. the
    /// rewind message), if known.
    Reset { id: Option<String> },
    /// Signals that the replay batch following a `Reset` is complete.
    /// Not forwarded to transforms; used by `TreeScrollViewState` to
    /// discard the UI-flag snapshot taken at reset time.
    ResetDone,
}
