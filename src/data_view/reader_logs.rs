use tracing::Level;

use crate::log_buffer::LogBuffer;
use crate::tree_scroll_view::{MessageState, MessageType};

pub fn build_reader_log_nodes(log_buffer: &LogBuffer) -> Vec<MessageState> {
    let entries = log_buffer.snapshot();

    if entries.is_empty() {
        return vec![
            MessageState::new("reader_logs.empty")
                .text("(no reader logs)")
                .message_type(MessageType::Other)
                .tag("log_entry"),
        ];
    }

    entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            let level_prefix = match entry.level {
                Level::ERROR => "[ERROR]",
                Level::WARN => "[WARN] ",
                Level::INFO => "[INFO] ",
                Level::DEBUG => "[DEBUG]",
                Level::TRACE => "[TRACE]",
            };
            let timestamp = entry.timestamp.format("%H:%M:%S").to_string();
            let text = format!("{timestamp} {level_prefix} {}", entry.message);
            MessageState::new(format!("reader_logs.{i}"))
                .text(text)
                .message_type(MessageType::Other)
                .tag("log_entry")
        })
        .collect()
}
