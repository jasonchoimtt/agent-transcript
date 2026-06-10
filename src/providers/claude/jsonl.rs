use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// A stateful JSONL reader that re-orders entries so that a parent UUID is
/// always emitted before any child that references it.
///
/// Claude transcripts occasionally write a child entry (e.g. an `attachment`)
/// one line *before* the parent entry it references. `JsonlReader` absorbs
/// that by buffering out-of-order entries and releasing them only once their
/// parent has been seen.
///
/// Normal (in-order) entries are passed through immediately with no copying.
pub struct JsonlReader {
    reader: BufReader<File>,
    pub byte_offset: usize,
    seen_uuids: HashSet<String>,
    /// Entries whose parents are now known; ready to emit in arrival order.
    pub queued: VecDeque<(usize, Value)>,
    /// parent_uuid → entries waiting for that parent to be seen.
    pub unresolved_messages: HashMap<String, Vec<(usize, Value)>>,
}

impl JsonlReader {
    pub fn new(path: &Path) -> color_eyre::Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            byte_offset: 0,
            seen_uuids: HashSet::new(),
            queued: VecDeque::new(),
            unresolved_messages: HashMap::new(),
        })
    }

    /// Returns the next `(byte_offset, value)` pair in parent-before-child order,
    /// or `Ok(None)` when the file has no more complete lines.
    ///
    /// The `byte_offset` is the position of the *start* of the line in the file.
    /// Out-of-order entries are buffered until their parent is seen; once the
    /// parent is emitted all buffered children are queued and returned on
    /// subsequent calls.
    ///
    /// If an incomplete line is encountered (no trailing newline), the reader
    /// seeks back to the start of that line so the same bytes can be re-read
    /// on the next call once more data has been written.
    pub fn try_recv(&mut self) -> color_eyre::Result<Option<(usize, Value)>> {
        loop {
            // Drain the ready queue first.
            if let Some((offset, value)) = self.queued.pop_front() {
                self.on_emit(&value);
                return Ok(Some((offset, value)));
            }

            // Read the next line from disk.
            let mut line = String::new();
            let line_byte_len = self.reader.read_line(&mut line)?;

            if line_byte_len == 0 {
                // EOF.
                return Ok(None);
            }

            if !(line.ends_with('\n') || line.ends_with('\r')) {
                // Incomplete line — seek back so it is re-read when the writer
                // appends the rest.
                self.reader.seek(SeekFrom::Start(self.byte_offset as u64))?;
                return Ok(None);
            }

            let offset = self.byte_offset;
            self.byte_offset += line_byte_len;

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                continue;
            }

            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                // Skip non-JSON lines.
                continue;
            };

            let parent_uuid = self.extract_parent_uuid(&value);

            if parent_uuid
                .as_deref()
                .is_none_or(|p| self.seen_uuids.contains(p))
            {
                // Parent is known (or absent) — emit directly.
                self.on_emit(&value);
                return Ok(Some((offset, value)));
            }

            // Parent not yet seen — buffer under the parent UUID.
            self.unresolved_messages
                .entry(parent_uuid.unwrap())
                .or_default()
                .push((offset, value));
        }
    }

    /// Mark the uuid of `value` as seen and move any children waiting for it
    /// into the ready queue.
    fn on_emit(&mut self, value: &Value) {
        let uuid = value["uuid"].as_str().unwrap_or("");
        if uuid.is_empty() {
            return;
        }
        self.seen_uuids.insert(uuid.to_string());
        if let Some(children) = self.unresolved_messages.remove(uuid) {
            self.queued.extend(children);
        }
    }

    fn extract_parent_uuid(&self, value: &Value) -> Option<String> {
        value["parentUuid"]
            .as_str()
            .or_else(|| value["logicalParentUuid"].as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn make_entry(uuid: &str, parent_uuid: Option<&str>) -> String {
        match parent_uuid {
            Some(p) => format!(r#"{{"uuid":"{uuid}","parentUuid":"{p}"}}"#),
            None => format!(r#"{{"uuid":"{uuid}","parentUuid":null}}"#),
        }
    }

    fn write_jsonl(lines: &[String]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        (dir, path)
    }

    /// In-order entries are passed through unchanged.
    #[test]
    fn test_in_order_passthrough() {
        let lines = vec![
            make_entry("root", None),
            make_entry("child", Some("root")),
            make_entry("grandchild", Some("child")),
        ];
        let (_dir, path) = write_jsonl(&lines);
        let mut r = JsonlReader::new(&path).unwrap();

        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "root");
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "child");
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "grandchild");
        assert!(r.try_recv().unwrap().is_none());
    }

    /// A child that appears before its parent is buffered and emitted after
    /// the parent.
    #[test]
    fn test_child_before_parent_reordered() {
        let lines = vec![
            make_entry("root", None),
            make_entry("child", Some("parent")), // parent not yet seen
            make_entry("parent", Some("root")),
        ];
        let (_dir, path) = write_jsonl(&lines);
        let mut r = JsonlReader::new(&path).unwrap();

        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "root");
        // "child" is buffered, so "parent" comes next
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "parent");
        // now "child" is released
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "child");
        assert!(r.try_recv().unwrap().is_none());
    }

    /// The full out-of-order scenario from the bug report: attachment written
    /// before the tool_result it is a child of.
    #[test]
    fn test_attachment_before_tool_result() {
        let lines = vec![
            make_entry("root", None),
            make_entry("asst-a", Some("root")),
            make_entry("attachment", Some("tool-result")), // child before parent
            make_entry("tool-result", Some("asst-a")),
            make_entry("continuation", Some("attachment")),
        ];
        let (_dir, path) = write_jsonl(&lines);
        let mut r = JsonlReader::new(&path).unwrap();

        let uuids: Vec<String> = std::iter::from_fn(|| r.try_recv().unwrap())
            .map(|(_, v)| v["uuid"].as_str().unwrap().to_string())
            .collect();

        // "attachment" must come after "tool-result"
        let pos_attachment = uuids.iter().position(|u| u == "attachment").unwrap();
        let pos_tool_result = uuids.iter().position(|u| u == "tool-result").unwrap();
        assert!(
            pos_tool_result < pos_attachment,
            "tool-result must precede attachment; got order: {uuids:?}"
        );
        // "continuation" must come after "attachment"
        let pos_continuation = uuids.iter().position(|u| u == "continuation").unwrap();
        assert!(pos_attachment < pos_continuation);
    }

    /// Byte offsets returned are the start-of-line positions in the file.
    #[test]
    fn test_byte_offsets() {
        let line0 = make_entry("root", None);
        let line1 = make_entry("child", Some("root"));
        let lines = vec![line0.clone(), line1.clone()];
        let (_dir, path) = write_jsonl(&lines);
        let mut r = JsonlReader::new(&path).unwrap();

        let (off0, _) = r.try_recv().unwrap().unwrap();
        let (off1, _) = r.try_recv().unwrap().unwrap();

        assert_eq!(off0, 0);
        assert_eq!(off1, line0.len() + 1); // +1 for the '\n'
    }

    /// Multiple children waiting for the same parent are released in arrival
    /// order once the parent appears.
    #[test]
    fn test_multiple_children_released_in_order() {
        let lines = vec![
            make_entry("root", None),
            make_entry("child1", Some("parent")),
            make_entry("child2", Some("parent")),
            make_entry("parent", Some("root")),
        ];
        let (_dir, path) = write_jsonl(&lines);
        let mut r = JsonlReader::new(&path).unwrap();

        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "root");
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "parent");
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "child1");
        assert_eq!(r.try_recv().unwrap().unwrap().1["uuid"], "child2");
        assert!(r.try_recv().unwrap().is_none());
    }

    /// An incomplete last line (no trailing newline) causes the reader to seek
    /// back so the line can be re-read once more data is appended.
    #[test]
    fn test_incomplete_line_rewinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        // Write a complete line followed by an incomplete one (no newline).
        {
            let mut file = File::create(&path).unwrap();
            write!(file, "{}\n", make_entry("root", None)).unwrap();
            write!(file, "{}", make_entry("child", Some("root"))).unwrap(); // no trailing newline
        }

        let mut r = JsonlReader::new(&path).unwrap();
        let (_, v) = r.try_recv().unwrap().unwrap();
        assert_eq!(v["uuid"], "root");

        let saved_offset = r.byte_offset;
        // Incomplete line — returns None and seeks back.
        assert!(r.try_recv().unwrap().is_none());
        assert_eq!(
            r.byte_offset, saved_offset,
            "byte_offset must not advance on incomplete line"
        );

        // Now complete the line by appending a newline.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file).unwrap();
        }

        // Re-read: the child line is now complete.
        let (_, v) = r.try_recv().unwrap().unwrap();
        assert_eq!(v["uuid"], "child");
        assert!(r.try_recv().unwrap().is_none());
    }
}
