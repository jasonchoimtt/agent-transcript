use crate::config::FileDeltaWidgetConfig;
use crate::providers::ProviderKind;
use crate::transforms::Transform;
use crate::transforms::tool_formatter::filter_path;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::message_widget::component::ComponentState;
use crate::tree_scroll_view::state::{MessageState, MessageType};
use crate::tree_scroll_view::tool_result::{PatchHunk, ToolResultState, render::make_brief};

/// Detects ToolResult nodes carrying structured tool output JSON and attaches a
/// [`ToolResultState`] so they render as rich widgets. Supports Claude and Cursor providers.
pub struct ToolResultEnricher {
    context_lines: Option<usize>,
    workspace_path: Option<std::path::PathBuf>,
    provider: ProviderKind,
}

impl ToolResultEnricher {
    pub fn new(
        config: &FileDeltaWidgetConfig,
        workspace_path: Option<std::path::PathBuf>,
        provider: ProviderKind,
    ) -> Self {
        Self {
            context_lines: config.context_lines,
            workspace_path,
            provider,
        }
    }

    fn try_enrich(&self, msg: &MessageState) -> Option<MessageState> {
        match self.provider {
            ProviderKind::Claude => self.try_enrich_claude(msg),
            ProviderKind::Cursor => self.try_enrich_cursor(msg),
        }
    }

    fn try_enrich_claude(&self, msg: &MessageState) -> Option<MessageState> {
        let json: serde_json::Value = serde_json::from_str(&msg.data).ok()?;
        let result = json.get("toolUseResult")?;

        let ui_state: Box<dyn ComponentState> =
            if let Some(state) = self.try_parse_file_delta(result) {
                Box::new(state)
            } else if let Some(state) = try_parse_shell_output(result) {
                Box::new(state)
            } else {
                return None;
            };

        self.make_enriched(msg, ui_state)
    }

    fn try_enrich_cursor(&self, msg: &MessageState) -> Option<MessageState> {
        let json: serde_json::Value = serde_json::from_str(&msg.data).ok()?;
        let output =
            json.pointer("/message/providerOptions/cursor/highLevelToolCallResult/output")?;

        let ui_state: Box<dyn ComponentState> =
            if let Some(state) = self.try_parse_cursor_file_delta(output) {
                Box::new(state)
            } else if let Some(state) = try_parse_cursor_shell_output(output) {
                Box::new(state)
            } else {
                return None;
            };

        self.make_enriched(msg, ui_state)
    }

    fn make_enriched(
        &self,
        msg: &MessageState,
        ui_state: Box<dyn ComponentState>,
    ) -> Option<MessageState> {
        let brief = ui_state
            .as_any()
            .downcast_ref::<ToolResultState>()
            .map(make_brief)
            .unwrap_or_default();

        let mut enriched = msg.clone();
        enriched.show_more = true;
        enriched.brief = Some(brief);
        enriched.height = None;
        enriched.ui_state = Some(ui_state);
        Some(enriched)
    }

    fn try_parse_file_delta(&self, result: &serde_json::Value) -> Option<ToolResultState> {
        let raw_path = result.get("filePath")?.as_str()?.to_string();
        let file_path = filter_path(raw_path, self.workspace_path.as_deref());
        let patch_arr = result.get("structuredPatch")?.as_array()?;

        let mut hunks: Vec<PatchHunk> = patch_arr.iter().filter_map(parse_hunk).collect();

        if hunks.is_empty() && result.get("type").and_then(|v| v.as_str()) == Some("create") {
            if let Some(content) = result.get("content").and_then(|v| v.as_str()) {
                if !content.is_empty() {
                    hunks = vec![make_create_hunk(content)];
                }
            }
        }

        if hunks.is_empty() {
            return None;
        }

        Some(ToolResultState::file_delta(
            file_path,
            hunks,
            self.context_lines,
        ))
    }

    fn try_parse_cursor_file_delta(&self, output: &serde_json::Value) -> Option<ToolResultState> {
        let success = output.get("success")?;
        let diff_str = success.get("diffString")?.as_str()?;
        let raw_path = success.get("path")?.as_str()?.to_string();

        if diff_str.is_empty() {
            return None;
        }

        let file_path = filter_path(raw_path, self.workspace_path.as_deref());
        let hunks = parse_unified_diff(diff_str);

        if hunks.is_empty() {
            return None;
        }

        Some(ToolResultState::file_delta(
            file_path,
            hunks,
            self.context_lines,
        ))
    }
}

fn make_create_hunk(content: &str) -> PatchHunk {
    let lines: Vec<String> = content.lines().map(|l| format!("+{l}")).collect();
    let new_lines = lines.len() as u32;
    PatchHunk {
        lines,
        old_start: 0,
        old_lines: 0,
        new_start: 1,
        new_lines,
    }
}

fn parse_hunk(v: &serde_json::Value) -> Option<PatchHunk> {
    let lines = v.get("lines")?.as_array()?;
    let old_start = v.get("oldStart")?.as_u64()? as u32;
    let old_lines = v.get("oldLines")?.as_u64()? as u32;
    let new_start = v.get("newStart")?.as_u64()? as u32;
    let new_lines = v.get("newLines")?.as_u64()? as u32;

    let line_strs: Vec<String> = lines
        .iter()
        .filter_map(|l| l.as_str().map(|s| s.to_string()))
        .collect();

    Some(PatchHunk {
        lines: line_strs,
        old_start,
        old_lines,
        new_start,
        new_lines,
    })
}

fn try_parse_shell_output(result: &serde_json::Value) -> Option<ToolResultState> {
    // Must have at least one of stdout/stderr keys present.
    let has_stdout = result.get("stdout").is_some();
    let has_stderr = result.get("stderr").is_some();
    if !has_stdout && !has_stderr {
        return None;
    }

    let stdout = result
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stderr = result
        .get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Skip if both are empty — not worth rendering a special widget.
    if stdout.is_empty() && stderr.is_empty() {
        return None;
    }

    Some(ToolResultState::shell_output(stderr, stdout))
}

fn try_parse_cursor_shell_output(output: &serde_json::Value) -> Option<ToolResultState> {
    let success = output.get("success")?;
    let interleaved = success.get("interleavedOutput")?.as_str()?;

    if interleaved.is_empty() {
        return None;
    }

    Some(ToolResultState::shell_output(
        String::new(),
        interleaved.to_string(),
    ))
}

fn parse_unified_diff(diff_str: &str) -> Vec<PatchHunk> {
    let mut hunks = Vec::new();
    let mut lines = diff_str.lines().peekable();

    // Skip past any header lines (diff --git, ---, +++) until we hit a @@ hunk header.
    while let Some(line) = lines.peek() {
        if line.starts_with("@@") {
            break;
        }
        lines.next();
    }

    while let Some(hunk_header) = lines.next() {
        if !hunk_header.starts_with("@@") {
            continue;
        }

        let Some((old_start, old_lines, new_start, new_lines)) =
            parse_unified_hunk_header(hunk_header)
        else {
            continue;
        };

        let mut hunk_lines: Vec<String> = Vec::new();
        while let Some(line) = lines.peek() {
            if line.starts_with("@@") {
                break;
            }
            let line = lines.next().unwrap();
            // Include context (' '), added ('+'), removed ('-') lines only.
            if line.starts_with(' ') || line.starts_with('+') || line.starts_with('-') {
                hunk_lines.push(line.to_string());
            }
        }

        if !hunk_lines.is_empty() {
            hunks.push(PatchHunk {
                lines: hunk_lines,
                old_start,
                old_lines,
                new_start,
                new_lines,
            });
        }
    }

    hunks
}

fn parse_unified_hunk_header(header: &str) -> Option<(u32, u32, u32, u32)> {
    // @@ -old_start[,old_lines] +new_start[,new_lines] @@
    let rest = header.strip_prefix("@@")?.trim_start();
    let old_part = rest.strip_prefix('-')?;
    let space = old_part.find(' ')?;
    let (old_start, old_lines) = parse_range(&old_part[..space])?;

    let new_part = old_part[space..].trim_start().strip_prefix('+')?;
    let end = new_part.find(' ').unwrap_or(new_part.len());
    let (new_start, new_lines) = parse_range(&new_part[..end])?;

    Some((old_start, old_lines, new_start, new_lines))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    if let Some(comma) = s.find(',') {
        let start: u32 = s[..comma].parse().ok()?;
        let count: u32 = s[comma + 1..].parse().ok()?;
        Some((start, count))
    } else {
        let start: u32 = s.parse().ok()?;
        Some((start, 1))
    }
}

impl Transform for ToolResultEnricher {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        let mut output = Vec::with_capacity(ops.len());
        for op in ops {
            self.process_op(op, &mut output);
        }
        output
    }
}

impl ToolResultEnricher {
    fn process_op(&mut self, op: TreeOperation, output: &mut Vec<TreeOperation>) {
        match op {
            TreeOperation::Append {
                ref parent_id,
                ref message,
            } if message.message_type == MessageType::ToolResult => {
                if let Some(enriched) = self.try_enrich(message) {
                    output.push(TreeOperation::Append {
                        parent_id: parent_id.clone(),
                        message: enriched,
                    });
                } else {
                    output.push(op);
                }
            }

            TreeOperation::Replace {
                ref id,
                ref message,
            } if message.message_type == MessageType::ToolResult => {
                if let Some(enriched) = self.try_enrich(message) {
                    output.push(TreeOperation::Replace {
                        id: id.clone(),
                        message: enriched,
                    });
                } else {
                    output.push(op);
                }
            }

            TreeOperation::Update {
                ref id,
                ref message,
            } if message.message_type == MessageType::ToolResult => {
                if let Some(enriched) = self.try_enrich(message) {
                    // Update → Replace so the new ui_state takes effect.
                    output.push(TreeOperation::Replace {
                        id: id.clone(),
                        message: enriched,
                    });
                } else {
                    output.push(op);
                }
            }

            other => output.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileDeltaWidgetConfig;
    use crate::providers::ProviderKind;
    use crate::tree_scroll_view::tool_result::{ToolResultPayload, ToolResultState};

    fn enricher() -> ToolResultEnricher {
        ToolResultEnricher::new(
            &FileDeltaWidgetConfig {
                context_lines: None,
            },
            None,
            ProviderKind::Claude,
        )
    }

    fn cursor_enricher() -> ToolResultEnricher {
        ToolResultEnricher::new(
            &FileDeltaWidgetConfig {
                context_lines: None,
            },
            None,
            ProviderKind::Cursor,
        )
    }

    fn tool_result_msg(id: &str, data: &str) -> MessageState {
        MessageState::new(id)
            .message_type(MessageType::ToolResult)
            .data(data)
    }

    const FILE_DELTA_JSON: &str = r#"{
        "toolUseResult": {
            "filePath": "/src/foo.rs",
            "structuredPatch": [{
                "lines": [" ctx", "-old", "+new"],
                "oldStart": 1,
                "oldLines": 2,
                "newStart": 1,
                "newLines": 2
            }]
        }
    }"#;

    const SHELL_JSON: &str = r#"{
        "toolUseResult": {
            "stdout": "hello\nworld",
            "stderr": ""
        }
    }"#;

    const SHELL_BOTH_JSON: &str = r#"{
        "toolUseResult": {
            "stdout": "out",
            "stderr": "err"
        }
    }"#;

    #[test]
    fn detects_file_delta() {
        let mut e = enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("a", FILE_DELTA_JSON),
        };
        let out = e.process(vec![op]);
        assert_eq!(out.len(), 1);
        if let TreeOperation::Append { message, .. } = &out[0] {
            assert!(message.show_more);
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            assert!(matches!(ui.payload, ToolResultPayload::FileDelta(_)));
        } else {
            panic!("expected Append");
        }
    }

    #[test]
    fn detects_shell_output() {
        let mut e = enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("b", SHELL_JSON),
        };
        let out = e.process(vec![op]);
        if let TreeOperation::Append { message, .. } = &out[0] {
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            assert!(matches!(ui.payload, ToolResultPayload::ShellOutput(_)));
        } else {
            panic!("expected Append");
        }
    }

    #[test]
    fn plain_text_passthrough() {
        let mut e = enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("c", "just plain text"),
        };
        let out = e.process(vec![op]);
        assert!(
            matches!(&out[0], TreeOperation::Append { message, .. } if message.ui_state.is_none())
        );
    }

    #[test]
    fn empty_stdout_stderr_passthrough() {
        let mut e = enricher();
        let json = r#"{"toolUseResult": {"stdout": "", "stderr": ""}}"#;
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("d", json),
        };
        let out = e.process(vec![op]);
        assert!(
            matches!(&out[0], TreeOperation::Append { message, .. } if message.ui_state.is_none())
        );
    }

    #[test]
    fn replace_op_enriched() {
        let mut e = enricher();
        let op = TreeOperation::Replace {
            id: "x".to_string(),
            message: tool_result_msg("x", FILE_DELTA_JSON),
        };
        let out = e.process(vec![op]);
        assert!(
            matches!(&out[0], TreeOperation::Replace { message, .. } if message.ui_state.is_some())
        );
    }

    #[test]
    fn update_op_becomes_replace() {
        let mut e = enricher();
        let op = TreeOperation::Update {
            id: "y".to_string(),
            message: tool_result_msg("y", FILE_DELTA_JSON),
        };
        let out = e.process(vec![op]);
        assert!(matches!(&out[0], TreeOperation::Replace { .. }));
    }

    #[test]
    fn file_path_in_hunk_metadata() {
        let mut e = enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("fp", FILE_DELTA_JSON),
        };
        let out = e.process(vec![op]);
        if let TreeOperation::Append { message, .. } = &out[0] {
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            if let ToolResultPayload::FileDelta(fd) = &ui.payload {
                assert_eq!(fd.file_path, "/src/foo.rs");
                assert_eq!(fd.hunks.len(), 1);
                assert_eq!(fd.hunks[0].old_start, 1);
            }
        }
    }

    #[test]
    fn create_type_generates_hunk_from_content() {
        let mut e = enricher();
        let json = r#"{
            "toolUseResult": {
                "content": "fn main() {}\n",
                "filePath": "/src/main.rs",
                "originalFile": null,
                "structuredPatch": [],
                "type": "create",
                "userModified": false
            }
        }"#;
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("cr", json),
        };
        let out = e.process(vec![op]);
        if let TreeOperation::Append { message, .. } = &out[0] {
            assert!(message.show_more);
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            if let ToolResultPayload::FileDelta(fd) = &ui.payload {
                assert_eq!(fd.file_path, "/src/main.rs");
                assert_eq!(fd.hunks.len(), 1);
                assert_eq!(fd.hunks[0].old_start, 0);
                assert_eq!(fd.hunks[0].old_lines, 0);
                assert_eq!(fd.hunks[0].new_start, 1);
                assert_eq!(fd.hunks[0].lines, vec!["+fn main() {}"]);
            } else {
                panic!("expected FileDelta");
            }
        } else {
            panic!("expected Append");
        }
    }

    #[test]
    fn create_type_empty_content_passthrough() {
        let mut e = enricher();
        let json = r#"{
            "toolUseResult": {
                "content": "",
                "filePath": "/src/empty.rs",
                "structuredPatch": [],
                "type": "create"
            }
        }"#;
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("cre", json),
        };
        let out = e.process(vec![op]);
        assert!(
            matches!(&out[0], TreeOperation::Append { message, .. } if message.ui_state.is_none())
        );
    }

    #[test]
    fn both_stdout_stderr_detected() {
        let mut e = enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("s", SHELL_BOTH_JSON),
        };
        let out = e.process(vec![op]);
        if let TreeOperation::Append { message, .. } = &out[0] {
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            if let ToolResultPayload::ShellOutput(so) = &ui.payload {
                assert_eq!(so.stderr, "err");
                assert_eq!(so.stdout, "out");
            } else {
                panic!("expected ShellOutput");
            }
        }
    }

    // ── Cursor format tests ────────────────────────────────────────────────────

    const CURSOR_SHELL_JSON: &str = r#"{
        "call": {},
        "result": {},
        "message": {
            "providerOptions": {
                "cursor": {
                    "highLevelToolCallResult": {
                        "isError": false,
                        "output": {
                            "success": {
                                "command": "echo hello",
                                "interleavedOutput": "hello\n"
                            }
                        }
                    }
                }
            }
        }
    }"#;

    const CURSOR_FILE_DELTA_JSON: &str = r#"{
        "call": {},
        "result": {},
        "message": {
            "providerOptions": {
                "cursor": {
                    "highLevelToolCallResult": {
                        "isError": false,
                        "output": {
                            "success": {
                                "path": "/src/foo.ts",
                                "diffString": "--- a//src/foo.ts\n+++ b//src/foo.ts\n@@ -1,3 +1,3 @@\n context\n-old line\n+new line\n"
                            }
                        }
                    }
                }
            }
        }
    }"#;

    #[test]
    fn cursor_detects_shell_output() {
        let mut e = cursor_enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("cs", CURSOR_SHELL_JSON),
        };
        let out = e.process(vec![op]);
        if let TreeOperation::Append { message, .. } = &out[0] {
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            if let ToolResultPayload::ShellOutput(so) = &ui.payload {
                assert_eq!(so.stdout, "hello\n");
                assert!(so.stderr.is_empty());
            } else {
                panic!("expected ShellOutput");
            }
        } else {
            panic!("expected Append");
        }
    }

    #[test]
    fn cursor_detects_file_delta() {
        let mut e = cursor_enricher();
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("cf", CURSOR_FILE_DELTA_JSON),
        };
        let out = e.process(vec![op]);
        if let TreeOperation::Append { message, .. } = &out[0] {
            assert!(message.show_more);
            let ui = message
                .ui_state
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ToolResultState>()
                .unwrap();
            if let ToolResultPayload::FileDelta(fd) = &ui.payload {
                assert_eq!(fd.file_path, "/src/foo.ts");
                assert_eq!(fd.hunks.len(), 1);
                assert_eq!(fd.hunks[0].old_start, 1);
                assert_eq!(fd.hunks[0].old_lines, 3);
                assert_eq!(fd.hunks[0].new_start, 1);
                assert_eq!(fd.hunks[0].new_lines, 3);
                assert_eq!(
                    fd.hunks[0].lines,
                    vec![" context", "-old line", "+new line"]
                );
            } else {
                panic!("expected FileDelta");
            }
        } else {
            panic!("expected Append");
        }
    }

    #[test]
    fn cursor_empty_interleaved_passthrough() {
        let mut e = cursor_enricher();
        let json = r#"{
            "call": {}, "result": {},
            "message": {
                "providerOptions": {"cursor": {"highLevelToolCallResult": {"output": {"success": {"interleavedOutput": ""}}}}}
            }
        }"#;
        let op = TreeOperation::Append {
            parent_id: None,
            message: tool_result_msg("ce", json),
        };
        let out = e.process(vec![op]);
        assert!(
            matches!(&out[0], TreeOperation::Append { message, .. } if message.ui_state.is_none())
        );
    }

    #[test]
    fn parse_unified_diff_multiple_hunks() {
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n@@ -10,2 +10,2 @@\n ctx2\n-old2\n+new2\n";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 10);
    }
}
