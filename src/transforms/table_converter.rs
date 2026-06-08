use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::config::TableConverterConfig;
use crate::transforms::Transform;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{MessageState, MessageType};
use crate::tree_scroll_view::table::{TableData, TableState};

/// Converts AgentMessage nodes whose text is a single GFM table block into
/// `MessageType::Table` nodes carrying a `TableState`.
///
/// Runs after `MarkdownSplitter` so each node is already a single block.
pub struct TableConverter {
    /// Maps original node id → column count for previously-converted table nodes.
    converted: std::collections::HashMap<String, usize>,
}

impl TableConverter {
    pub fn new(_config: TableConverterConfig) -> Self {
        Self {
            converted: std::collections::HashMap::new(),
        }
    }

    fn process_op(&mut self, op: TreeOperation, output: &mut Vec<TreeOperation>) {
        match op {
            TreeOperation::Append {
                ref parent_id,
                ref message,
            } if self.should_convert(message) => {
                if let Some(table_data) = parse_table(message.text.as_deref().unwrap_or("")) {
                    let col_count = table_data.headers.len();
                    let id = message.id.clone();
                    let converted = build_table_node(message, table_data);
                    self.converted.insert(id, col_count);
                    output.push(TreeOperation::Append {
                        parent_id: parent_id.clone(),
                        message: converted,
                    });
                } else {
                    output.push(op);
                }
            }

            TreeOperation::Replace {
                ref id,
                ref message,
            } if self.should_convert(message) => {
                if let Some(table_data) = parse_table(message.text.as_deref().unwrap_or("")) {
                    let col_count = table_data.headers.len();
                    let node_id = id.clone();
                    let converted = build_table_node(message, table_data);
                    self.converted.insert(node_id.clone(), col_count);
                    output.push(TreeOperation::Replace {
                        id: node_id,
                        message: converted,
                    });
                } else {
                    output.push(op);
                }
            }

            TreeOperation::Update {
                ref id,
                ref message,
            } if self.should_convert(message) => {
                if let Some(table_data) = parse_table(message.text.as_deref().unwrap_or("")) {
                    let col_count = table_data.headers.len();
                    let node_id = id.clone();
                    let converted = build_table_node(message, table_data);
                    self.converted.insert(node_id.clone(), col_count);
                    output.push(TreeOperation::Replace {
                        id: node_id,
                        message: converted,
                    });
                } else {
                    output.push(op);
                }
            }

            other => output.push(other),
        }
    }

    fn should_convert(&self, msg: &MessageState) -> bool {
        msg.message_type == MessageType::AgentMessage && msg.text.is_some()
    }
}

impl Transform for TableConverter {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        let mut output = Vec::with_capacity(ops.len());
        for op in ops {
            self.process_op(op, &mut output);
        }
        output
    }

    fn reset(&mut self) {
        self.converted.clear();
    }
}

fn build_table_node(source: &MessageState, data: TableData) -> MessageState {
    let rows = data.rows.len();
    let cols = data.headers.len();
    let brief = format!("Table: {}×{}", rows, cols);
    let ui = TableState::new(data);
    let mut node = MessageState::new(source.id.clone())
        .message_type(MessageType::Table)
        .show_more(true)
        .show_indicator(source.show_indicator)
        .brief(brief)
        .ui_state(Box::new(ui));
    if let Some(t) = source.text.as_deref() {
        node = node.text(t);
    }
    node
}

/// Parse `text` as GFM markdown. Returns `Some(TableData)` iff the text contains
/// exactly one top-level block and that block is a table.
pub fn parse_table(text: &str) -> Option<TableData> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let parser = Parser::new_ext(text, options);
    let events: Vec<Event<'_>> = parser.collect();

    // Verify the first top-level element is a table and it's the only one.
    let mut top_level_count = 0;
    let mut depth = 0usize;
    let mut has_table_at_top = false;

    for event in &events {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    top_level_count += 1;
                    if let Event::Start(Tag::Table(_)) = event {
                        has_table_at_top = true;
                    }
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    if top_level_count != 1 || !has_table_at_top {
        return None;
    }

    extract_table_data(&events)
}

fn extract_table_data(events: &[Event<'_>]) -> Option<TableData> {
    let mut headers: Vec<String> = vec![];
    let mut rows: Vec<Vec<String>> = vec![];

    let mut in_head = false;
    let mut in_row = false;
    let mut in_cell = false;
    let mut current_cell = String::new();
    let mut current_row: Vec<String> = vec![];

    for event in events {
        match event {
            Event::Start(Tag::TableHead) => {
                in_head = true;
            }
            Event::End(TagEnd::TableHead) => {
                in_head = false;
                headers = std::mem::take(&mut current_row);
            }
            Event::Start(Tag::TableRow) => {
                in_row = true;
                current_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                in_row = false;
                if !current_row.is_empty() {
                    rows.push(std::mem::take(&mut current_row));
                }
            }
            Event::Start(Tag::TableCell) => {
                in_cell = true;
                current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                if in_head || in_row {
                    current_row.push(std::mem::take(&mut current_cell));
                }
            }
            Event::Text(t) | Event::Code(t) if in_cell => {
                current_cell.push_str(t);
            }
            Event::SoftBreak | Event::HardBreak if in_cell => {
                current_cell.push('\n');
            }
            _ => {}
        }
    }

    if headers.is_empty() {
        return None;
    }

    Some(TableData { headers, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent_msg(id: &str, text: &str) -> MessageState {
        MessageState::new(id)
            .message_type(MessageType::AgentMessage)
            .text(text)
            .show_more(true)
    }

    #[test]
    fn plain_text_passthrough() {
        let mut conv = TableConverter::new(TableConverterConfig::default());
        let op = TreeOperation::Append {
            parent_id: None,
            message: make_agent_msg("a", "hello world"),
        };
        let out = conv.process(vec![op]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0], TreeOperation::Append { ref message, .. } if message.message_type == MessageType::AgentMessage)
        );
    }

    #[test]
    fn table_converted() {
        let mut conv = TableConverter::new(TableConverterConfig::default());
        let table_md = "| H1 | H2 |\n|---|---|\n| a | b |\n| c | d |";
        let op = TreeOperation::Append {
            parent_id: None,
            message: make_agent_msg("t", table_md),
        };
        let out = conv.process(vec![op]);
        assert_eq!(out.len(), 1);
        if let TreeOperation::Append { ref message, .. } = out[0] {
            assert_eq!(message.message_type, MessageType::Table);
            let ts = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<TableState>()
                .unwrap();
            assert_eq!(ts.data.headers, vec!["H1", "H2"]);
            assert_eq!(ts.data.rows.len(), 2);
        } else {
            panic!("expected Append");
        }
    }

    #[test]
    fn multi_block_not_converted() {
        let mut conv = TableConverter::new(TableConverterConfig::default());
        let text = "Some paragraph\n\n| H |\n|---|\n| v |";
        let op = TreeOperation::Append {
            parent_id: None,
            message: make_agent_msg("m", text),
        };
        let out = conv.process(vec![op]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0], TreeOperation::Append { ref message, .. } if message.message_type == MessageType::AgentMessage)
        );
    }

    #[test]
    fn update_converts_table_node() {
        let mut conv = TableConverter::new(TableConverterConfig::default());
        let table_md = "| H1 | H2 |\n|---|---|\n| a | b |";
        let op = TreeOperation::Update {
            id: "t".to_string(),
            message: make_agent_msg("t", table_md),
        };
        let out = conv.process(vec![op]);
        // Update with table text → Replace emitted with Table type.
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], TreeOperation::Replace { id, message } if id == "t" && message.message_type == MessageType::Table),
            "Update with table text should produce a Replace(Table)"
        );
    }

    #[test]
    fn update_plain_text_passthrough() {
        let mut conv = TableConverter::new(TableConverterConfig::default());
        let op = TreeOperation::Update {
            id: "a".to_string(),
            message: make_agent_msg("a", "just some text"),
        };
        let out = conv.process(vec![op]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], TreeOperation::Update { .. }));
    }
}
