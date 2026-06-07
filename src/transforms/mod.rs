use tokio::sync::mpsc;

use crate::config::TransformsConfig;
use crate::event::Event;
use crate::providers::ProviderKind;
use crate::reader_op::ReaderOp;
use crate::tree_operation::TreeOperation;

#[cfg(feature = "lua")]
pub mod lua_transform;
pub mod markdown_splitter;
pub mod table_converter;
pub mod tool_formatter;
pub mod tool_grouper;
pub mod ui_initializer;

/// Build the ordered transform list from a `TransformsConfig`.
///
/// Order: `UiInitializer` → `ToolFormatter` → `ToolGrouper` → `MarkdownSplitter` (opt-in) → `LuaTransform` (opt-in).
/// ToolFormatter runs before ToolGrouper so that container labels can be derived from already-formatted child text.
pub fn build_transforms(
    cfg: &TransformsConfig,
    provider: &ProviderKind,
    workspace_path: Option<&std::path::Path>,
) -> Vec<Box<dyn Transform>> {
    let mut transforms: Vec<Box<dyn Transform>> = Vec::new();
    transforms.push(Box::new(ui_initializer::UiInitializer::new(
        cfg.ui_initializer.clone(),
    )));
    transforms.push(Box::new(tool_formatter::ToolFormatter::new(
        cfg.tool_formatter.clone(),
        provider,
        workspace_path.map(|p| p.to_path_buf()),
    )));
    transforms.push(Box::new(tool_grouper::ToolGrouper::new(
        cfg.tool_grouper.clone(),
    )));
    if cfg.markdown_splitter.enabled {
        transforms.push(Box::new(markdown_splitter::MarkdownSplitter::new(
            cfg.markdown_splitter.clone(),
        )));
    }
    if cfg.table_converter.enabled {
        transforms.push(Box::new(table_converter::TableConverter::new(
            cfg.table_converter.clone(),
        )));
    }
    #[cfg(feature = "lua")]
    if let Some(lua_cfg) = &cfg.lua {
        match lua_transform::LuaTransform::new(lua_cfg) {
            Ok(t) => transforms.push(Box::new(t)),
            Err(e) => tracing::warn!("lua_transform init failed: {e}"),
        }
    }
    transforms
}

/// A synchronous, batch-oriented stream-processing stage in the transform pipeline.
pub trait Transform: Send + 'static {
    /// Transform a batch of incoming operations into zero or more outgoing ones.
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation>;
    /// Clear all internal state. Called by the pipeline when a Reset op is received.
    fn reset(&mut self) {}
}

/// Spawn the pipeline task. Drains batches from `input`, folds each through `transforms`
/// in order, and forwards the output as `Event::ReaderOp` to `output`.
///
/// `Reset` and `ResetDone` are handled specially and never passed to `Transform::process()`:
/// - `Reset`: flush pending tree ops through transforms, call `reset()` on every transform,
///   then forward the `ReaderOp::Reset` downstream.
/// - `ResetDone`: flush, forward `ReaderOp::ResetDone` downstream (no transform reset).
pub fn build_pipeline(
    mut input: mpsc::Receiver<ReaderOp>,
    mut transforms: Vec<Box<dyn Transform>>,
    output: mpsc::UnboundedSender<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(first) = input.recv().await else {
                break;
            };
            let mut batch = vec![first];
            while let Ok(op) = input.try_recv() {
                batch.push(op);
            }

            let output_ops = apply_batch(batch, &mut transforms);

            for op in output_ops {
                if output.send(Event::ReaderOp(op)).is_err() {
                    return;
                }
            }
        }
    })
}

pub(crate) fn apply_batch(
    batch: Vec<ReaderOp>,
    transforms: &mut Vec<Box<dyn Transform>>,
) -> Vec<ReaderOp> {
    let mut result: Vec<ReaderOp> = Vec::new();
    let mut remaining = batch;

    loop {
        // Find the next Reset or ResetDone that needs special handling.
        let sentinel_pos = remaining
            .iter()
            .position(|op| matches!(op, ReaderOp::Reset { .. } | ReaderOp::ResetDone));

        match sentinel_pos {
            None => {
                // All remaining ops are tree mutations — fold through transforms normally.
                let tree_ops: Vec<_> = remaining
                    .into_iter()
                    .filter_map(|op| match op {
                        ReaderOp::Tree(t) => Some(t),
                        _ => unreachable!(),
                    })
                    .collect();
                result.extend(
                    fold_through(tree_ops, transforms)
                        .into_iter()
                        .map(ReaderOp::Tree),
                );
                break;
            }
            Some(pos) => {
                let before: Vec<_> = remaining.drain(..pos).collect();
                let sentinel = remaining.remove(0);

                if !before.is_empty() {
                    let tree_ops: Vec<_> = before
                        .into_iter()
                        .filter_map(|op| match op {
                            ReaderOp::Tree(t) => Some(t),
                            _ => unreachable!(),
                        })
                        .collect();
                    result.extend(
                        fold_through(tree_ops, transforms)
                            .into_iter()
                            .map(ReaderOp::Tree),
                    );
                }

                if matches!(sentinel, ReaderOp::Reset { .. }) {
                    for t in transforms.iter_mut() {
                        t.reset();
                    }
                }
                result.push(sentinel); // forward Reset or ResetDone unchanged
                // continue processing remaining ops through the (possibly reset) transforms
            }
        }
    }

    result
}

fn fold_through(
    ops: Vec<TreeOperation>,
    transforms: &mut Vec<Box<dyn Transform>>,
) -> Vec<TreeOperation> {
    transforms.iter_mut().fold(ops, |acc, t| t.process(acc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_scroll_view::state::{MessageState, MessageType};

    fn dummy_op(id: &str) -> ReaderOp {
        ReaderOp::Tree(TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id).message_type(MessageType::UserMessage),
        })
    }

    fn op_id(op: &ReaderOp) -> Option<&str> {
        match op {
            ReaderOp::Tree(TreeOperation::Append { message, .. }) => Some(&message.id),
            _ => None,
        }
    }

    #[tokio::test]
    async fn passthrough_empty_transforms() {
        let (tx, rx) = mpsc::channel(32);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let handle = build_pipeline(rx, vec![], out_tx);

        tx.send(dummy_op("a")).await.unwrap();
        tx.send(dummy_op("b")).await.unwrap();
        drop(tx);
        handle.await.unwrap();

        let mut ids = vec![];
        while let Ok(Event::ReaderOp(op)) = out_rx.try_recv() {
            if let Some(id) = op_id(&op) {
                ids.push(id.to_string());
            }
        }
        assert_eq!(ids, ["a", "b"]);
    }

    #[tokio::test]
    async fn pipeline_shuts_down_on_sender_drop() {
        let (tx, rx) = mpsc::channel(32);
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let handle = build_pipeline(rx, vec![], out_tx);
        drop(tx);
        // Should complete without panic.
        handle.await.unwrap();
    }

    struct StatefulTransform {
        seen: Vec<String>,
    }

    impl Transform for StatefulTransform {
        fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
            for op in &ops {
                if let TreeOperation::Append { message, .. } = op {
                    self.seen.push(message.id.clone());
                }
            }
            ops
        }
        fn reset(&mut self) {
            self.seen.clear();
        }
    }

    #[test]
    fn reset_clears_transform_state() {
        let mut transforms: Vec<Box<dyn Transform>> =
            vec![Box::new(StatefulTransform { seen: vec![] })];

        let ops = vec![
            dummy_op("t1"),
            dummy_op("t2"),
            ReaderOp::Reset { id: None },
            dummy_op("t3"),
        ];

        let output = apply_batch(ops, &mut transforms);

        // Reset should appear in output and t3 should follow.
        let has_reset = output.iter().any(|op| matches!(op, ReaderOp::Reset { .. }));
        assert!(has_reset);
        let ids: Vec<_> = output.iter().filter_map(|op| op_id(op)).collect();
        assert_eq!(ids, ["t1", "t2", "t3"]);
    }

    #[tokio::test]
    async fn update_passthrough_no_transforms() {
        let (tx, rx) = mpsc::channel(32);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let handle = build_pipeline(rx, vec![], out_tx);

        let msg = crate::tree_scroll_view::state::MessageState::new("u")
            .message_type(crate::tree_scroll_view::state::MessageType::AgentMessage)
            .text("hello");
        tx.send(ReaderOp::Tree(TreeOperation::Update {
            id: "u".to_string(),
            message: msg,
        }))
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        let mut ops = vec![];
        while let Ok(crate::event::Event::ReaderOp(op)) = out_rx.try_recv() {
            ops.push(op);
        }
        assert_eq!(ops.len(), 1);
        assert!(
            matches!(&ops[0], ReaderOp::Tree(TreeOperation::Update { id, .. }) if id == "u"),
            "Update should pass through the pipeline unchanged"
        );
    }
}
