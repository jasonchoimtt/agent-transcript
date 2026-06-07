/// A cursor-based scanner for vt100 screen rows.
///
/// The cursor starts at `rows` (one past the last row) and scans upward.
/// `take` moves the cursor up one row if the row above matches; `take_until` scans
/// upward to the first matching row. Both leave the cursor unchanged on failure.
/// `peek` performs the same scan as `take_until` without moving the cursor.
/// After a successful take, call `move_up(1)` to advance past the consumed row.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineMatcher {
    Blank,
    NonBlank,
    /// Non-blank text OR any cell with a non-Default background color.
    Visible,
    Divider,
    Backgrounded,
    Braille,
}

pub struct LineTokenizer<'a> {
    screen: &'a vt100::Screen,
    rows: u16,
    cols: u16,
    current_row: u16,
}

impl<'a> LineTokenizer<'a> {
    pub fn new(screen: &'a vt100::Screen) -> Self {
        let (rows, cols) = screen.size();
        Self {
            screen,
            rows,
            cols,
            current_row: rows,
        }
    }

    // ── Low-level API ─────────────────────────────────────────────────────────

    pub fn pos(&self) -> u16 {
        self.current_row
    }

    /// Move to `row`. Clamped to `[0, rows - 1]`.
    pub fn seek(&mut self, row: u16) {
        self.current_row = row.min(self.rows.saturating_sub(1));
    }

    /// Move up `n` rows (toward row 0). Saturates at 0.
    pub fn move_up(&mut self, n: u16) {
        self.current_row = self.current_row.saturating_sub(n);
    }

    /// Move down `n` rows (toward `rows - 1`). Clamped at `rows - 1`.
    pub fn move_down(&mut self, n: u16) {
        self.current_row = (self.current_row + n).min(self.rows.saturating_sub(1));
    }

    /// True if the full concatenated text of `row` contains `text` as a substring.
    pub fn row_contains(&self, row: u16, text: &str) -> bool {
        self.row_text(row).contains(text)
    }

    /// True if any cell in `row` satisfies `pred`.
    pub fn row_matches_any<F: Fn(&vt100::Cell) -> bool>(&self, row: u16, pred: F) -> bool {
        if row >= self.rows {
            return false;
        }
        (0..self.cols)
            .filter_map(|c| self.screen.cell(row, c))
            .any(pred)
    }

    /// Returns the `contents()` of the first non-blank cell in `row`, or `""` if all blank.
    pub fn row_first_non_blank_char(&self, row: u16) -> &str {
        if row >= self.rows {
            return "";
        }
        for col in 0..self.cols {
            if let Some(cell) = self.screen.cell(row, col) {
                let c = cell.contents();
                if !c.is_empty() && c != " " {
                    return c;
                }
            }
        }
        ""
    }

    /// Concatenated cell contents of `row` as a `String`.
    pub fn row_text(&self, row: u16) -> String {
        if row >= self.rows {
            return String::new();
        }
        (0..self.cols)
            .filter_map(|c| self.screen.cell(row, c))
            .map(|cell| cell.contents().to_string())
            .collect()
    }

    /// True if `current_row` matches `matcher`.
    pub fn is(&self, matcher: LineMatcher) -> bool {
        self.matches(self.current_row, matcher)
    }

    /// True if `row` matches `matcher` (does not move `current_row`).
    pub fn is_at(&self, row: u16, matcher: LineMatcher) -> bool {
        self.matches(row, matcher)
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    // ── High-level scanning ───────────────────────────────────────────────────

    /// If the row directly above `current_row` matches `matcher`, move up one and return
    /// `Some(row)`. Returns `None` and leaves `current_row` unchanged otherwise.
    pub fn take(&mut self, matcher: LineMatcher) -> Option<u16> {
        if self.current_row == 0 {
            return None;
        }
        let above = self.current_row - 1;
        if self.matches(above, matcher) {
            self.current_row = above;
            Some(above)
        } else {
            None
        }
    }

    /// Scan upward from the row above `current_row`. Move `current_row` to the first
    /// matching row and return `Some(row)`, or return `None` without moving if not found.
    pub fn take_until(&mut self, matcher: LineMatcher) -> Option<u16> {
        if self.current_row == 0 {
            return None;
        }
        if let Some(row) = self.scan_up(self.current_row - 1, matcher) {
            self.current_row = row;
            Some(row)
        } else {
            None
        }
    }

    /// Scan upward from the row above `current_row`. Return the row index of the first
    /// match without moving `current_row`, or `None` if not found.
    pub fn peek(&self, matcher: LineMatcher) -> Option<u16> {
        if self.current_row == 0 {
            return None;
        }
        self.scan_up(self.current_row - 1, matcher)
    }

    /// Find the paragraph above `current_row`: skip blank rows upward, then span the
    /// contiguous non-blank block. Move `current_row` to the block's top row and return
    /// `(start_row, end_row)`. Returns `None` if all rows above are blank.
    pub fn take_paragraph(&mut self) -> Option<(u16, u16)> {
        let result = self.find_paragraph_above(self.current_row);
        if let Some((start, end)) = result {
            self.current_row = start;
            Some((start, end))
        } else {
            None
        }
    }

    /// Like `take_paragraph` but does not move `current_row`.
    pub fn peek_paragraph(&self) -> Option<(u16, u16)> {
        self.find_paragraph_above(self.current_row)
    }

    /// Find the bg-highlighted box above `current_row`: skip non-highlighted rows upward,
    /// then span the contiguous highlighted block. Move `current_row` to the block's top
    /// row and return `(start_row, end_row)`. Returns `None` if no highlighted rows exist above.
    pub fn take_backgrounded_box(&mut self) -> Option<(u16, u16)> {
        let result = self.find_backgrounded_box_above(self.current_row);
        if let Some((start, end)) = result {
            self.current_row = start;
            Some((start, end))
        } else {
            None
        }
    }

    /// Like `take_backgrounded_box` but does not move `current_row`.
    pub fn peek_backgrounded_box(&self) -> Option<(u16, u16)> {
        self.find_backgrounded_box_above(self.current_row)
    }

    /// Return the highest-indexed (bottommost) non-blank row, or `None` if all blank.
    pub fn last_non_blank_row(&self) -> Option<u16> {
        (0..self.rows).rev().find(|&r| !self.is_blank_row(r))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn matches(&self, row: u16, matcher: LineMatcher) -> bool {
        if row >= self.rows {
            return false;
        }
        match matcher {
            LineMatcher::Blank => self.is_blank_row(row),
            LineMatcher::NonBlank => !self.is_blank_row(row),
            LineMatcher::Visible => !self.is_blank_row(row) || self.has_background_cell(row),
            LineMatcher::Divider => self.is_divider_row(row),
            LineMatcher::Backgrounded => self.is_backgrounded_row(row),
            LineMatcher::Braille => self.is_braille_row(row),
        }
    }

    fn scan_up(&self, start: u16, matcher: LineMatcher) -> Option<u16> {
        if self.rows == 0 {
            return None;
        }
        let mut r = start.min(self.rows - 1);
        loop {
            if self.matches(r, matcher) {
                return Some(r);
            }
            if r == 0 {
                break;
            }
            r -= 1;
        }
        None
    }

    fn is_blank_row(&self, row: u16) -> bool {
        for col in 0..self.cols {
            if let Some(cell) = self.screen.cell(row, col) {
                let c = cell.contents();
                if !c.is_empty() && c != " " {
                    return false;
                }
            }
        }
        true
    }

    /// ≥ 95% of the row's columns must be `─`; any non-`─`/non-whitespace cell
    /// immediately disqualifies the row.
    fn is_divider_row(&self, row: u16) -> bool {
        let threshold = self.cols as f32 * 0.95;
        let mut divider_count = 0u16;
        for col in 0..self.cols {
            if let Some(cell) = self.screen.cell(row, col) {
                let c = cell.contents();
                if c == "─" {
                    divider_count += 1;
                } else if c != " " && !c.is_empty() {
                    return false;
                }
            }
            let remaining = self.cols - col - 1;
            if ((divider_count + remaining) as f32) < threshold {
                return false;
            }
        }
        divider_count as f32 >= threshold
    }

    /// True when ≥ 80% of the row's cells have a non-Default background color.
    fn is_backgrounded_row(&self, row: u16) -> bool {
        let threshold = self.cols as f32 * 0.8;
        let mut count = 0u16;
        for col in 0..self.cols {
            if let Some(cell) = self.screen.cell(row, col) {
                if cell.bgcolor() != vt100::Color::Default {
                    count += 1;
                }
            }
            let remaining = self.cols - col - 1;
            if ((count + remaining) as f32) < threshold {
                return false;
            }
        }
        count as f32 >= threshold
    }

    fn has_background_cell(&self, row: u16) -> bool {
        if row >= self.rows {
            return false;
        }
        (0..self.cols)
            .filter_map(|c| self.screen.cell(row, c))
            .any(|cell| cell.bgcolor() != vt100::Color::Default)
    }

    /// True when the first non-blank cell in `row` starts with a braille character
    /// (U+2800–U+28FF).
    fn is_braille_row(&self, row: u16) -> bool {
        for col in 0..self.cols {
            if let Some(cell) = self.screen.cell(row, col) {
                let c = cell.contents();
                if c.is_empty() || c == " " {
                    continue;
                }
                return c
                    .chars()
                    .next()
                    .is_some_and(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch));
            }
        }
        false
    }

    /// Walk upward from `anchor` (exclusive), skip non-highlighted rows, then span the
    /// contiguous bg-highlighted block above. Returns `(start, end)`, or `None` if no
    /// highlighted rows exist above `anchor`.
    fn find_backgrounded_box_above(&self, anchor: u16) -> Option<(u16, u16)> {
        if anchor == 0 || self.rows == 0 {
            return None;
        }
        let mut r = anchor;

        loop {
            if r == 0 {
                return None;
            }
            r -= 1;
            if self.is_backgrounded_row(r) {
                break;
            }
        }

        let end = r;
        while r > 0 && self.is_backgrounded_row(r - 1) {
            r -= 1;
        }
        Some((r, end))
    }

    /// Walk upward from `anchor` (exclusive), skip blank rows, then span the contiguous
    /// non-blank block above. Returns `(start, end)` of the block, or `None` if all
    /// rows above `anchor` are blank.
    fn find_paragraph_above(&self, anchor: u16) -> Option<(u16, u16)> {
        if anchor == 0 || self.rows == 0 {
            return None;
        }
        let mut r = anchor;

        loop {
            if r == 0 {
                return None;
            }
            r -= 1;
            if !self.is_blank_row(r) {
                break;
            }
        }

        let end = r;
        while r > 0 && !self.is_blank_row(r - 1) {
            r -= 1;
        }
        Some((r, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_screen(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(rows, cols, 0)
    }

    // ── Position management ───────────────────────────────────────────────────

    #[test]
    fn new_positions_past_last_row() {
        let p = make_screen(10, 80);
        let tok = LineTokenizer::new(p.screen());
        assert_eq!(tok.current_row, 10);
    }

    #[test]
    fn seek_clamps_to_bounds() {
        let p = make_screen(10, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(100);
        assert_eq!(tok.current_row, 9);
        tok.seek(5);
        assert_eq!(tok.current_row, 5);
    }

    #[test]
    fn move_up_saturates_at_zero() {
        let p = make_screen(10, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(2);
        tok.move_up(5);
        assert_eq!(tok.current_row, 0);
    }

    #[test]
    fn move_down_clamps_at_last_row() {
        let p = make_screen(10, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        tok.move_down(100);
        assert_eq!(tok.current_row, 9);
    }

    // ── is / row predicates ───────────────────────────────────────────────────

    #[test]
    fn is_blank_and_non_blank() {
        let mut p = make_screen(5, 80);
        p.process(b"\x1b[2;1Hhello");
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert!(tok.is(LineMatcher::Blank));
        tok.seek(1);
        assert!(tok.is(LineMatcher::NonBlank));
        assert!(!tok.is(LineMatcher::Blank));
    }

    #[test]
    fn is_divider() {
        let mut p = make_screen(5, 80);
        let divider: String = "─".repeat(80);
        p.process(format!("\x1b[3;1H{divider}").as_bytes());
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(2); // row 2 = \x1b[3;...
        assert!(tok.is(LineMatcher::Divider));
        tok.seek(0);
        assert!(!tok.is(LineMatcher::Divider));
    }

    #[test]
    fn is_divider_trailing_spaces_ok() {
        // 76 ─ + 4 spaces is a valid divider.
        let mut p = make_screen(5, 80);
        let divider: String = "─".repeat(76) + "    ";
        p.process(format!("\x1b[1;1H{divider}").as_bytes());
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert!(tok.is(LineMatcher::Divider));
    }

    #[test]
    fn is_braille_first_non_blank_char() {
        let mut p = make_screen(5, 80);
        p.process(b"\x1b[1;1H\xe2\xa0\xa3\xe2\xa0\x84 Working"); // ⠣⠄ Working — first char is braille
        p.process(b"\x1b[2;1HWorking \xe2\xa0\xa3"); //  Working ⠣ — braille not first
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert!(
            tok.is(LineMatcher::Braille),
            "row starting with braille should match"
        );
        tok.seek(1);
        assert!(
            !tok.is(LineMatcher::Braille),
            "braille not in first position should not match"
        );
        tok.seek(2);
        assert!(!tok.is(LineMatcher::Braille), "blank row should not match");
    }

    #[test]
    fn is_divider_below_95_pct_rejected() {
        // 74 ─ + 6 spaces = 74/80 = 92.5% < 95% → not a divider.
        let mut p = make_screen(5, 80);
        let not_divider: String = "─".repeat(74) + "      ";
        p.process(format!("\x1b[1;1H{not_divider}").as_bytes());
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert!(!tok.is(LineMatcher::Divider));
    }

    #[test]
    fn row_contains_text() {
        let mut p = make_screen(5, 80);
        p.process(b"\x1b[2;1HRunning\xe2\x80\xa6"); // Running…
        let tok = LineTokenizer::new(p.screen());
        assert!(tok.row_contains(1, "Running"));
        assert!(!tok.row_contains(0, "Running"));
    }

    // ── take / take_until / peek ─────────────────────────────────────

    #[test]
    fn take_moves_up_one_when_row_above_matches() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[10;1Habc"); // row 9
        let mut tok = LineTokenizer::new(p.screen());
        // current_row = 10; row above (9) is non-blank → move up
        assert_eq!(tok.take(LineMatcher::NonBlank), Some(9));
        assert_eq!(tok.current_row, 9);
    }

    #[test]
    fn take_returns_none_when_row_above_does_not_match() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[9;1Habc"); // row 8 non-blank, row 9 blank
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(8); // current_row = 8; row above (7) is blank
        assert_eq!(tok.take(LineMatcher::NonBlank), None);
        assert_eq!(tok.current_row, 8, "position unchanged on failure");
    }

    #[test]
    fn take_returns_none_at_row_zero() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[1;1Hhi"); // row 0
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert_eq!(tok.take(LineMatcher::NonBlank), None);
    }

    #[test]
    fn take_until_moves_to_matching_row() {
        // Rows 2 and 5 are non-blank. Start past bottom (10); take_until(NonBlank) → row 5.
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[3;1Habc"); // row 2
        p.process(b"\x1b[6;1Hxyz"); // row 5
        let mut tok = LineTokenizer::new(p.screen());
        assert_eq!(tok.take_until(LineMatcher::NonBlank), Some(5));
        assert_eq!(tok.current_row, 5);
    }

    #[test]
    fn take_until_returns_none_when_not_found() {
        let p = make_screen(10, 80);
        let mut tok = LineTokenizer::new(p.screen());
        assert_eq!(tok.take_until(LineMatcher::NonBlank), None);
        assert_eq!(tok.current_row, 10, "position unchanged on failure");
    }

    #[test]
    fn take_until_excludes_current_row() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[6;1Hhi"); // row 5 only non-blank row
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5); // sit on the only non-blank row
        // scanning starts above current_row, so row 5 must not be found
        assert_eq!(tok.take_until(LineMatcher::NonBlank), None);
        assert_eq!(tok.current_row, 5, "position unchanged");
    }

    #[test]
    fn peek_does_not_move_cursor() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[3;1Habc"); // row 2
        let tok = LineTokenizer::new(p.screen());
        let found = tok.peek(LineMatcher::NonBlank);
        assert_eq!(found, Some(2));
        assert_eq!(tok.current_row, 10, "cursor must not move");
    }

    #[test]
    fn peek_returns_none_when_not_found() {
        let p = make_screen(10, 80);
        let tok = LineTokenizer::new(p.screen());
        assert_eq!(tok.peek(LineMatcher::NonBlank), None);
    }

    #[test]
    fn take_until_two_dividers_with_move_up() {
        let mut p = make_screen(20, 80);
        let div: String = "─".repeat(80);
        p.process(format!("\x1b[6;1H{div}").as_bytes()); // row 5
        p.process(format!("\x1b[16;1H{div}").as_bytes()); // row 15
        let mut tok = LineTokenizer::new(p.screen());

        // Find bottom divider.
        assert_eq!(tok.take_until(LineMatcher::Divider), Some(15));
        tok.move_up(1);

        // Find top divider.
        assert_eq!(tok.take_until(LineMatcher::Divider), Some(5));
    }

    // ── take_paragraph / peek_paragraph ──────────────────────────────────────

    #[test]
    fn take_paragraph_finds_block_above() {
        // Rows 1-3 non-blank, row 4 blank, anchor at 5.
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[2;1Hline a"); // row 1
        p.process(b"\x1b[3;1Hline b"); // row 2
        p.process(b"\x1b[4;1Hline c"); // row 3
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5);
        let result = tok.take_paragraph();
        assert_eq!(result, Some((1, 3)));
        assert_eq!(tok.current_row, 1);
    }

    #[test]
    fn take_paragraph_skips_blank_gap() {
        // Rows 1-2 non-blank, rows 3-4 blank, anchor at 6.
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[2;1Hhello"); // row 1
        p.process(b"\x1b[3;1Hworld"); // row 2
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(6);
        let result = tok.take_paragraph();
        assert_eq!(result, Some((1, 2)));
        assert_eq!(tok.current_row, 1);
    }

    #[test]
    fn take_paragraph_returns_none_when_all_blank() {
        let p = make_screen(10, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5);
        assert_eq!(tok.take_paragraph(), None);
        assert_eq!(tok.current_row, 5, "position unchanged on None");
    }

    #[test]
    fn take_paragraph_returns_none_at_row_zero() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[1;1Hhi"); // row 0
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert_eq!(tok.take_paragraph(), None);
    }

    #[test]
    fn peek_paragraph_does_not_move() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[2;1Hstuff"); // row 1
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5);
        let result = tok.peek_paragraph();
        assert_eq!(result, Some((1, 1)));
        assert_eq!(tok.current_row, 5, "cursor must not move");
    }

    // ── take_backgrounded_box / peek_backgrounded_box ─────────────────

    fn write_bg_row(p: &mut vt100::Parser, row: u16, cols: u16) {
        let content = format!(
            "\x1b[{};1H\x1b[42m{}\x1b[0m",
            row + 1,
            " ".repeat(cols as usize)
        );
        p.process(content.as_bytes());
    }

    #[test]
    fn take_backgrounded_box_finds_block_above() {
        // Rows 1-3 highlighted, row 4 plain, anchor at 5.
        let mut p = make_screen(10, 80);
        write_bg_row(&mut p, 1, 80);
        write_bg_row(&mut p, 2, 80);
        write_bg_row(&mut p, 3, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5);
        assert_eq!(tok.take_backgrounded_box(), Some((1, 3)));
        assert_eq!(tok.current_row, 1);
    }

    #[test]
    fn take_backgrounded_box_skips_non_highlighted_gap() {
        // Rows 1-2 highlighted, rows 3-4 plain, anchor at 6.
        let mut p = make_screen(10, 80);
        write_bg_row(&mut p, 1, 80);
        write_bg_row(&mut p, 2, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(6);
        assert_eq!(tok.take_backgrounded_box(), Some((1, 2)));
        assert_eq!(tok.current_row, 1);
    }

    #[test]
    fn take_backgrounded_box_returns_none_when_no_highlighted_rows() {
        let p = make_screen(10, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5);
        assert_eq!(tok.take_backgrounded_box(), None);
        assert_eq!(tok.current_row, 5, "position unchanged on None");
    }

    #[test]
    fn take_backgrounded_box_returns_none_at_row_zero() {
        let mut p = make_screen(10, 80);
        write_bg_row(&mut p, 0, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(0);
        assert_eq!(tok.take_backgrounded_box(), None);
    }

    #[test]
    fn peek_backgrounded_box_does_not_move() {
        let mut p = make_screen(10, 80);
        write_bg_row(&mut p, 1, 80);
        let mut tok = LineTokenizer::new(p.screen());
        tok.seek(5);
        assert_eq!(tok.peek_backgrounded_box(), Some((1, 1)));
        assert_eq!(tok.current_row, 5, "cursor must not move");
    }

    // ── last_non_blank_row ────────────────────────────────────────────────────

    #[test]
    fn last_non_blank_row_basic() {
        let mut p = make_screen(10, 80);
        p.process(b"\x1b[3;1Habc"); // row 2
        p.process(b"\x1b[5;1Hxyz"); // row 4
        let tok = LineTokenizer::new(p.screen());
        assert_eq!(tok.last_non_blank_row(), Some(4));
    }

    #[test]
    fn last_non_blank_row_all_blank() {
        let p = make_screen(10, 80);
        let tok = LineTokenizer::new(p.screen());
        assert_eq!(tok.last_non_blank_row(), None);
    }
}
