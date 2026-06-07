use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local};
use tracing::Level;
use tracing_subscriber::Layer;

pub struct LogEntry {
    pub level: Level,
    pub message: String,
    pub timestamp: DateTime<Local>,
}

struct Inner {
    entries: VecDeque<LogEntry>,
    capacity: usize,
}

#[derive(Clone)]
pub struct LogBuffer(Arc<Mutex<Inner>>);

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            entries: VecDeque::new(),
            capacity,
        })))
    }

    pub fn push(&self, entry: LogEntry) {
        let mut inner = self.0.lock().unwrap();
        if inner.entries.len() >= inner.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(entry);
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().entries.clear();
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        let inner = self.0.lock().unwrap();
        inner
            .entries
            .iter()
            .map(|e| LogEntry {
                level: e.level,
                message: e.message.clone(),
                timestamp: e.timestamp,
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct LogBufferLayer(pub LogBuffer);

impl<S: tracing::Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        // Only capture INFO and above from the providers module.
        if !meta.target().starts_with("agt::providers") {
            return;
        }
        if *meta.level() > Level::INFO {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.0.push(LogEntry {
            level: *meta.level(),
            message: visitor.format(),
            timestamp: Local::now(),
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl MessageVisitor {
    fn format(&self) -> String {
        if self.fields.is_empty() {
            self.message.clone()
        } else {
            format!("{} {}", self.message, self.fields.join(" "))
        }
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.fields.push(format!("{}={s}", field.name()));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.push(format!("{}={value}", field.name()));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.push(format!("{}={value}", field.name()));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields.push(format!("{}={value}", field.name()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_snapshot() {
        let buf = LogBuffer::new(3);
        buf.push(LogEntry {
            level: Level::INFO,
            message: "a".into(),
            timestamp: Local::now(),
        });
        buf.push(LogEntry {
            level: Level::WARN,
            message: "b".into(),
            timestamp: Local::now(),
        });
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].message, "a");
        assert_eq!(snap[1].message, "b");
    }

    #[test]
    fn capacity_drops_oldest() {
        let buf = LogBuffer::new(2);
        for msg in ["a", "b", "c"] {
            buf.push(LogEntry {
                level: Level::INFO,
                message: msg.into(),
                timestamp: Local::now(),
            });
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].message, "b");
        assert_eq!(snap[1].message, "c");
    }

    #[test]
    fn clear_empties_buffer() {
        let buf = LogBuffer::new(10);
        buf.push(LogEntry {
            level: Level::INFO,
            message: "x".into(),
            timestamp: Local::now(),
        });
        buf.clear();
        assert!(buf.is_empty());
    }
}
