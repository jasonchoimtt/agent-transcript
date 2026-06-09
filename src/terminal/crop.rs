use crate::terminal::line_tokenizer::{LineMatcher, LineTokenizer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollapsedCrop {
    /// First screen row to render (0-based from top of the live screen).
    pub start_row: u16,
    /// Number of rows to display.
    pub height: u16,
    /// First screen row of the prompt box itself (>= start_row).
    /// Rows start_row..prompt_start_row are content above the prompt (e.g. queued messages,
    /// running-command output). Used to position the floating/pinned overlay.
    pub prompt_start_row: u16,
}

pub trait CropDetector: Send + Sync {
    fn detect(&self, screen: &vt100::Screen) -> Option<CollapsedCrop>;
}

pub struct NullCropDetector;
pub struct ClaudeCropDetector;
pub struct CursorCropDetector;

impl CropDetector for NullCropDetector {
    fn detect(&self, _screen: &vt100::Screen) -> Option<CollapsedCrop> {
        None
    }
}

fn is_blank_row(screen: &vt100::Screen, row: u16) -> bool {
    let (_, cols) = screen.size();
    for col in 0..cols {
        if let Some(cell) = screen.cell(row, col) {
            let c = cell.contents();
            if !c.is_empty() && c != " " {
                return false;
            }
        }
    }
    true
}

/// Walk upward from `anchor_row - 1`, skip blank rows, then continue upward
/// while rows are non-blank. Returns the first row of that paragraph.
/// If `anchor_row == 0` or all rows above are blank, returns `anchor_row`.
pub fn find_paragraph_above(screen: &vt100::Screen, anchor_row: u16) -> u16 {
    if anchor_row == 0 {
        return anchor_row;
    }
    let mut r = anchor_row;

    // Walk upward, skipping blank rows first
    loop {
        if r == 0 {
            return anchor_row; // all rows above are blank
        }
        r -= 1;
        if !is_blank_row(screen, r) {
            break;
        }
    }

    // r is on the bottommost non-blank row of the paragraph; walk up while non-blank
    while r > 0 && !is_blank_row(screen, r - 1) {
        r -= 1;
    }
    r
}

const RUNNING: &str = "Running\u{2026}";

impl CropDetector for ClaudeCropDetector {
    fn detect(&self, screen: &vt100::Screen) -> Option<CollapsedCrop> {
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return None;
        }

        let mut tokenizer = LineTokenizer::new(screen);
        let end_row = tokenizer.peek(LineMatcher::NonBlank).unwrap_or(0);

        let bottom_divider = tokenizer.take_until(LineMatcher::Divider)?;

        let top_divider = tokenizer.take_until(LineMatcher::Divider);

        match top_divider {
            // Prompt box found between two dividers.
            Some(top_div_row) => {
                // Queued messages
                while tokenizer.take(LineMatcher::NonBlank).is_some() {}

                let mut candidate = tokenizer.peek_paragraph();

                // Feedback paragraph
                if let Some((para_start, para_end)) = candidate
                    && para_end - para_start < 2
                    && tokenizer
                        .row_text(para_start)
                        .trim_start()
                        .starts_with("● How is Claude doing this session?")
                {
                    tokenizer.seek(para_start);
                    candidate = tokenizer.peek_paragraph();
                }

                // Status line or running command
                if let Some((para_start, para_end)) = candidate {
                    if tokenizer.row_first_non_blank_char(para_start) == "!" {
                        // Check last 2 lines for the Running text
                        let command_running = tokenizer.row_first_non_blank_char(para_start) == "!"
                            && (tokenizer.row_text(para_end).contains(RUNNING)
                                || (para_end != para_start
                                    && tokenizer.row_text(para_end - 1).contains(RUNNING)));

                        if command_running {
                            tokenizer.seek(para_start);
                        } else {
                            // Not running -- not included
                        }
                    } else {
                        tokenizer.seek(para_start);
                    }
                }

                Some(CollapsedCrop {
                    start_row: tokenizer.pos(),
                    height: end_row.saturating_sub(tokenizer.pos()) + 1,
                    // Prompt box starts at the upper divider; content above it
                    // (queued messages, running command) is above-prompt content.
                    prompt_start_row: top_div_row,
                })
            }
            None => Some(CollapsedCrop {
                start_row: bottom_divider,
                height: rows - bottom_divider,
                // Single divider only — skip the divider row itself.
                prompt_start_row: bottom_divider + 1,
            }),
        }
    }
}

fn find_cursor_divider_crop(screen: &vt100::Screen) -> Option<CollapsedCrop> {
    let mut tokenizer = LineTokenizer::new(screen);

    let bottommost_divider = tokenizer.peek(LineMatcher::Divider);

    // Try divider-flanked title
    while tokenizer.take_until(LineMatcher::Divider).is_some() {
        let Some(_) = tokenizer.take(LineMatcher::NonBlank) else {
            continue;
        };
        let Some(top_divider) = tokenizer.take(LineMatcher::Divider) else {
            continue;
        };

        return Some(CollapsedCrop {
            start_row: top_divider,
            height: tokenizer.rows().saturating_sub(top_divider),
            prompt_start_row: top_divider,
        });
    }

    // Try single divider
    bottommost_divider.map(|bottom_divider| CollapsedCrop {
        start_row: bottom_divider,
        height: tokenizer.rows().saturating_sub(bottom_divider),
        prompt_start_row: bottom_divider,
    })
}

impl CropDetector for CursorCropDetector {
    fn detect(&self, screen: &vt100::Screen) -> Option<CollapsedCrop> {
        let (rows, cols) = screen.size();
        if rows == 0 || cols == 0 {
            return None;
        }

        if let Some(crop) = find_cursor_divider_crop(screen) {
            return Some(crop);
        }

        let mut tokenizer = LineTokenizer::new(screen);
        let end_row = tokenizer.peek(LineMatcher::Visible).unwrap_or(0);

        // Find prompt box
        if let Some((prompt_start, prompt_end)) = tokenizer.take_backgrounded_box() {
            if !(prompt_start..=prompt_end).any(|i| tokenizer.row_first_non_blank_char(i) == "→")
            {
                // Not prompt box
                return None;
            }

            // Include any braille status line
            if let Some((para_start, _)) = tokenizer.peek_paragraph()
                && tokenizer.is_at(para_start, LineMatcher::Braille)
            {
                tokenizer.seek(para_start);
            }

            // Include any prompt box immediately above
            if let Some((box_start, box_end)) = tokenizer.peek_backgrounded_box()
                && tokenizer.peek(LineMatcher::NonBlank).unwrap_or(0) <= box_end
            {
                tokenizer.seek(box_start);
            }

            let start = tokenizer.pos();
            return Some(CollapsedCrop {
                start_row: start,
                height: end_row.saturating_sub(start) + 1,
                // The entire detected region is prompt UI (braille + upper box + main box).
                prompt_start_row: start,
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures are captured PTY output. Feed through vt100::Parser — do NOT read as plain text.
    // Always render at the fixture's native capture width so content doesn't wrap and create
    // false gaps or dilute character-density thresholds.
    fn load_fixture(provider: &str, name: &str, cols: u16, rows: u16) -> vt100::Screen {
        let path = format!("src/terminal/fixtures/screenshots/{provider}/{name}.txt");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(&bytes);
        parser.screen().clone()
    }

    fn make_screen(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(rows, cols, 0)
    }

    // ── find_paragraph_above ──────────────────────────────────────────────────

    #[test]
    fn find_paragraph_above_blank_gap_then_paragraph() {
        // Rows:  0="hello"  1="world"  2=""  3=""  anchor=4
        // Expected: 0 (first non-blank row of the paragraph above the blanks)
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[1;1Hhello");
        p.process(b"\x1b[2;1Hworld");
        let s = p.screen().clone();
        assert_eq!(find_paragraph_above(&s, 4), 0);
    }

    #[test]
    fn find_paragraph_above_all_blank_above() {
        // No content on screen; anchor=5 → returns 5
        let p = make_screen(10, 80);
        let s = p.screen().clone();
        assert_eq!(find_paragraph_above(&s, 5), 5);
    }

    #[test]
    fn find_paragraph_above_anchor_zero() {
        let p = make_screen(10, 80);
        let s = p.screen().clone();
        assert_eq!(find_paragraph_above(&s, 0), 0);
    }

    #[test]
    fn find_paragraph_above_contiguous_paragraph() {
        // Rows 2-4 non-blank; anchor=5 → returns 2
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[3;1Hline1");
        p.process(b"\x1b[4;1Hline2");
        p.process(b"\x1b[5;1Hline3");
        let s = p.screen().clone();
        assert_eq!(find_paragraph_above(&s, 5), 2);
    }

    // ── Claude 95% tolerance ──────────────────────────────────────────────────

    #[test]
    fn claude_divider_row_95_percent_tolerance() {
        // 80 cols: 76 '─' + 4 spaces still ≥ 95% (76/80 = 95%)
        let mut p = make_screen(20, 80);
        let divider: String = "─".repeat(76) + "    ";
        let cmd = format!("\x1b[19;1H{divider}");
        p.process(cmd.as_bytes());
        let s = p.screen().clone();
        let result = ClaudeCropDetector.detect(&s);
        assert!(result.is_some(), "95% divider should be recognized");
        let crop = result.unwrap();
        assert_eq!(crop.start_row, 18);
        assert_eq!(crop.height, 2);
        // Single divider → prompt starts after the divider row.
        assert_eq!(crop.prompt_start_row, 19);
    }

    // ── Claude command-output exclusion ──────────────────────────────────────

    #[test]
    fn claude_command_output_paragraph_excluded() {
        // Paragraph above top divider starts with '!' → excluded; start_row = top divider.
        let mut p = make_screen(20, 80);
        let top_div: String = "─".repeat(80);
        let bot_div: String = "─".repeat(80);
        p.process(b"\x1b[1;1H! some bash output");
        p.process(b"\x1b[2;1Houtput line 2");
        p.process(format!("\x1b[5;1H{top_div}").as_bytes()); // top divider at row 4
        p.process(format!("\x1b[15;1H{bot_div}").as_bytes()); // bottom divider at row 14
        let s = p.screen().clone();
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("should detect two dividers");
        assert_eq!(
            crop.start_row, 4,
            "command output paragraph should be excluded"
        );
        // start_row == top divider (row 4) when content above is excluded.
        assert_eq!(crop.prompt_start_row, 4);
    }

    #[test]
    fn claude_non_command_paragraph_included() {
        // Paragraph above top divider does NOT start with '!' → included as usual.
        let mut p = make_screen(20, 80);
        let top_div: String = "─".repeat(80);
        let bot_div: String = "─".repeat(80);
        p.process(b"\x1b[1;1Hsome assistant text");
        p.process(format!("\x1b[5;1H{top_div}").as_bytes()); // top divider at row 4
        p.process(format!("\x1b[15;1H{bot_div}").as_bytes()); // bottom divider at row 14
        let s = p.screen().clone();
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("should detect two dividers");
        assert_eq!(
            crop.start_row, 0,
            "non-command paragraph should be included"
        );
        // top divider is at row 4 (`\x1b[5;1H` = 1-based row 5 = 0-based row 4).
        assert_eq!(
            crop.prompt_start_row, 4,
            "prompt starts at top divider, not paragraph"
        );
    }

    // ── Claude no-divider → None ─────────────────────────────────────────────

    #[test]
    fn claude_no_divider_returns_none() {
        // Screen with only plain text and no divider chars → None
        let mut p = make_screen(20, 80);
        p.process(b"\x1b[1;1Hsome text on the screen");
        p.process(b"\x1b[2;1Hmore text here");
        let s = p.screen().clone();
        assert!(ClaudeCropDetector.detect(&s).is_none());
    }

    // ── Claude fixture tests (94 cols = divider fills the terminal width) ─────
    // At 94 cols, the 94-char Claude divider line registers as 100% dividers.

    #[test]
    fn claude_working_crop_detected() {
        // Shows Claude "Working…" with the bottom prompt box → start_row=16, height=4.
        let s = load_fixture("claude", "working", 94, 20);
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("claude/working.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 16);
        assert_eq!(crop.height, 4);
    }

    #[test]
    fn claude_approval_crop_detected() {
        // Shows a pending approval prompt → start_row=18, height=2.
        let s = load_fixture("claude", "approval", 94, 20);
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("claude/approval.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 18);
        assert_eq!(crop.height, 2);
    }

    #[test]
    fn claude_bash_mode_crop_detected() {
        // Shows the bash-mode input box (two dividers) + paragraph above → start_row=13, height=6.
        let s = load_fixture("claude", "bash-mode", 94, 20);
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("claude/bash-mode.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 13);
        assert_eq!(crop.height, 6);
    }

    #[test]
    fn claude_idle_crop_detected() {
        // Idle screen shows the two-divider prompt box → crop detects it.
        let s = load_fixture("claude", "idle", 94, 20);
        assert!(
            ClaudeCropDetector.detect(&s).is_some(),
            "claude/idle.txt should produce Some(crop)"
        );
    }

    #[test]
    fn claude_running_command_crop_detected() {
        // "⎿  Running…" on the last paragraph line → include the paragraph.
        // start_row=9 (paragraph start), height=8 (rows 9..=16: command + prompt box + status).
        let s = load_fixture("claude", "running-command", 94, 20);
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("claude/running-command.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 9);
        assert_eq!(crop.height, 8);
        // Running command paragraph is included (start_row=9), but the prompt box itself
        // starts at the top divider (row 12) — same row as ran-command's start_row.
        assert_eq!(crop.prompt_start_row, 12);
    }

    #[test]
    fn claude_ran_command_crop_excluded() {
        // "⎿  (Bash completed with no output)" on last line → exclude the paragraph.
        // start_row=12 (top divider), height=5 (rows 12..=16: prompt box + status).
        let s = load_fixture("claude", "ran-command", 94, 20);
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("claude/ran-command.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 12);
        assert_eq!(crop.height, 5);
        // Paragraph excluded → start_row == top divider == prompt_start_row.
        assert_eq!(crop.prompt_start_row, 12);
    }

    #[test]
    fn claude_queued_running_command_crop_detected() {
        let s = load_fixture("claude", "with-queued", 90, 20);
        let crop = ClaudeCropDetector
            .detect(&s)
            .expect("claude/with-queued.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 5);
        assert_eq!(crop.height, 11);
    }

    // ── Cursor fixture tests ──────────────────────────────────────────────────
    // Each fixture was captured at a specific terminal width; use that native width
    // so the highlighted rows are contiguous (rendering at narrower widths wraps
    // content and creates false gaps). Native widths: approval=93, working=93,
    // idle=93, running-command=93, idle-dark=99.

    #[test]
    fn cursor_working_bg_rectangle_detected() {
        // Bottommost highlighted row (row 10) is detected; height extends to last non-empty row.
        // start_row=10, height=4.
        let s = load_fixture("cursor", "working", 93, 20);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/working.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 10);
        assert_eq!(crop.height, 4);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_user_pending_extended_crop_detected() {
        // User typed "hiya" into the input box (rows 5-7) while the agent is "Composing"
        // (braille row 9). The braille bridges the lower highlighted block (follow-up +
        // status bar, rows 10-15). extend_start_for_upper_block pulls in the upper "hiya"
        // box → start_row=5, height=11 (rows 5..=15). Fixture is 20 rows × 89 cols.
        let s = load_fixture("cursor", "user_pending", 89, 20);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/user_pending.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 5);
        assert_eq!(crop.height, 11);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_approval_bg_rectangle_detected() {
        // Block rows 14-19 (approval box); row 19 has background but no text.
        // Visible end_row reaches row 19 → start_row=14, height=6.
        let s = load_fixture("cursor", "approval", 93, 20);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/approval.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 14);
        assert_eq!(crop.height, 6);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_idle_dark_bg_rectangle_detected() {
        // Block rows 9-11 (input box); all blank above → start_row = 9, height = 5.
        let s = load_fixture("cursor", "idle-dark", 99, 20);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/idle-dark.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 9);
        assert_eq!(crop.height, 5);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_idle_bg_rectangle_detected() {
        // Block rows 8-10 (input box); all blank above → start_row = 8, height = 5.
        let s = load_fixture("cursor", "idle", 93, 20);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/idle.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 8);
        assert_eq!(crop.height, 5);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_running_command_bg_rectangle_detected() {
        // Block rows 6-8 (follow-up input); no braille above → start_row = 6, height = 5.
        let s = load_fixture("cursor", "running-command", 93, 20);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/running-command.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 6);
        assert_eq!(crop.height, 5);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_resume_title_flanked_dividers_detected() {
        // At 46 rows the fixture fits without scrolling: divider at row 0,
        // title at row 1, divider at row 2, items, bottom divider at row 44, nav bar at row 45.
        let s = load_fixture("cursor", "resume", 90, 46);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/resume.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 0);
        assert_eq!(crop.height, 46);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    #[test]
    fn cursor_rewind_single_divider_detected() {
        // Rewind modal: single ─ divider at row 24 (0-indexed), no flanking divider below.
        // Despite highlighted conversation boxes above, divider check wins. start_row=24, height=11.
        let s = load_fixture("cursor", "rewind", 90, 35);
        let crop = CursorCropDetector
            .detect(&s)
            .expect("cursor/rewind.txt should produce Some(crop)");
        assert_eq!(crop.start_row, 24);
        assert_eq!(crop.height, 11);
        assert_eq!(crop.prompt_start_row, crop.start_row);
    }

    // ── Cursor synthetic tests ────────────────────────────────────────────────

    #[test]
    fn cursor_paragraph_without_braille_excluded() {
        // Block rows 12-14; plain text above (no braille) → start_row stays at 12.
        let mut p = make_screen(20, 80);
        for row in [13u16, 14, 15] {
            let cmd = format!("\x1b[{};1H\x1b[48;5;34m{}\x1b[0m", row, " ".repeat(80));
            p.process(cmd.as_bytes());
        }
        p.process(b"\x1b[13;1H\x1b[48;5;34m\xe2\x86\x92\x1b[0m"); // → at row 12 col 0
        p.process(b"\x1b[11;1Hsome text"); // row 10 (0-indexed): non-braille paragraph
        let s = p.screen().clone();
        let crop = CursorCropDetector.detect(&s).expect("should detect block");
        assert_eq!(
            crop.start_row, 12,
            "paragraph without braille should not be included"
        );
    }

    #[test]
    fn cursor_braille_paragraph_included() {
        // Block rows 12-14; braille spinner line just above → start_row = 10.
        let mut p = make_screen(20, 80);
        for row in [13u16, 14, 15] {
            let cmd = format!("\x1b[{};1H\x1b[48;5;34m{}\x1b[0m", row, " ".repeat(80));
            p.process(cmd.as_bytes());
        }
        p.process(b"\x1b[13;1H\x1b[48;5;34m\xe2\x86\x92\x1b[0m"); // → at row 12 col 0
        p.process(b"\x1b[11;1H\xe2\xa0\xa3\xe2\xa0\x84 Running"); // row 10: ⠣⠄ Running
        let s = p.screen().clone();
        let crop = CursorCropDetector.detect(&s).expect("should detect block");
        assert_eq!(
            crop.start_row, 10,
            "braille working line should be pulled in"
        );
    }

    #[test]
    fn cursor_braille_bridges_to_upper_highlighted_block() {
        // Upper HL block at rows 6-8 sits above braille (row 11) above the main block (12-14).
        // Since braille was found, extend_start_for_upper_block reaches the upper HL block.
        // Lower block rows 12-14 have background; end_row = 14 (Visible reaches bottom of block).
        // height = 14 - 6 + 1 = 9.
        let mut p = make_screen(20, 80);
        for row in [7u16, 8, 9, 13, 14, 15] {
            let cmd = format!("\x1b[{};1H\x1b[48;5;34m{}\x1b[0m", row, " ".repeat(80));
            p.process(cmd.as_bytes());
        }
        p.process(b"\x1b[13;1H\x1b[48;5;34m\xe2\x86\x92\x1b[0m"); // → at row 12 col 0 (lower block)
        p.process(b"\x1b[12;1H\xe2\xa0\xa3\xe2\xa0\x84 Running"); // row 11: ⠣⠄ Running (between blocks)
        let s = p.screen().clone();
        let crop = CursorCropDetector
            .detect(&s)
            .expect("should detect lower block");
        assert_eq!(
            crop.start_row, 6,
            "should extend up to upper highlighted block"
        );
        assert_eq!(crop.height, 9);
    }

    #[test]
    fn cursor_single_row_rectangle_detected() {
        // Span of exactly 1 highlighted row → Some (minimum span is 1)
        // \x1b[18;1H = 0-indexed row 17 → span = 1
        let mut p = make_screen(20, 80);
        let cmd = format!("\x1b[18;1H\x1b[48;5;34m{}\x1b[0m", " ".repeat(80));
        p.process(cmd.as_bytes());
        p.process(b"\x1b[18;1H\x1b[48;5;34m\xe2\x86\x92\x1b[0m"); // → at row 17 col 0
        let s = p.screen().clone();
        assert!(
            CursorCropDetector.detect(&s).is_some(),
            "span of 1 should be detected"
        );
    }

    // ── Sticky fallback: recompute_crop retains previous value on None ────────

    struct ToggleDetector {
        call_count: std::sync::atomic::AtomicU32,
        first_result: CollapsedCrop,
    }

    impl CropDetector for ToggleDetector {
        fn detect(&self, _screen: &vt100::Screen) -> Option<CollapsedCrop> {
            let n = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Some(self.first_result)
            } else {
                None
            }
        }
    }

    #[test]
    fn recompute_crop_sticky_on_none() {
        use crate::event::Event;
        use crate::terminal::state::TerminalState;
        use portable_pty::CommandBuilder;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::unbounded_channel::<Event>();
        let initial = CollapsedCrop {
            start_row: 5,
            height: 12,
            prompt_start_row: 5,
        };
        let detector = Box::new(ToggleDetector {
            call_count: std::sync::atomic::AtomicU32::new(0),
            first_result: initial,
        });
        let mut state =
            TerminalState::new_with_cmd(CommandBuilder::new("/bin/sh"), None, detector, tx, 0)
                .expect("failed to spawn PTY");

        // First call: detector returns Some → both values set.
        state.recompute_crop();
        assert_eq!(state.collapsed_crop, Some(initial));
        assert_eq!(
            state.prompt_box_start_row,
            Some(initial.prompt_start_row),
            "prompt_box_start_row should be set on successful detection"
        );

        // Second call: detector returns None → collapsed_crop retained (sticky),
        // but prompt_box_start_row cleared (non-sticky).
        state.recompute_crop();
        assert_eq!(
            state.collapsed_crop,
            Some(initial),
            "collapsed_crop should be retained when detection returns None"
        );
        assert_eq!(
            state.prompt_box_start_row, None,
            "prompt_box_start_row should be cleared when detection returns None"
        );
    }
}
