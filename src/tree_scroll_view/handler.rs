use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub enum TreeAction {
    SelectNext,
    SelectPrev,
    SelectChild,
    SelectParent,
    ToggleExpand,
    CycleDisplay,
    TerminalActivate,
    ScrollDown(u16),
    ScrollUp(u16),
    ScrollDownHalf(u16),
    ScrollUpHalf(u16),
    // viewport-relative selection (H / M / L)
    SelectViewportTop,
    SelectViewportMiddle,
    SelectViewportBottom,
    // scroll to reposition selection on screen (zt / zz / zb)
    ScrollSelectionToTop,
    ScrollSelectionToMiddle,
    ScrollSelectionToBottom,
    // jump to first / last content item (g / G)
    SelectFirst,
    SelectLastContent,
    // message-type run navigation ( ) { }
    SelectNextTypeStart,
    SelectPrevTypeStart,
    SelectNextUserAgent,
    SelectPrevUserAgent,
    // turn-boundary navigation ]] ][ [[ []
    SelectNextTurnStart,
    SelectNextTurnEnd,
    SelectPrevTurnStart,
    SelectPrevTurnEnd,
    // clipboard copy
    CopyMarkdown,
    CopyPlainText,
    CopyRawData,
    // open / close with optional hidden-reveal
    OpenNode,
    CloseNode,
    OpenRevealHidden,
    CloseHideRevealed,
    // step into / past hidden nodes
    RevealNextFive,
    RevealPrevFive,
    RevealJumpForward,
    RevealJumpBackward,
    // global hidden toggle
    ToggleAllHidden,
    Quit,
    None,
}

pub struct KeyParser {
    pending: Option<KeyCode>,
}

impl Default for KeyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyParser {
    pub fn new() -> Self {
        Self { pending: None }
    }

    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// Returns the pending prefix character if a multi-key sequence is in progress.
    pub fn pending_char(&self) -> Option<char> {
        match self.pending {
            Some(KeyCode::Char(c)) => Some(c),
            _ => None,
        }
    }

    pub fn process(&mut self, key: KeyEvent, area_height: u16) -> TreeAction {
        if key.kind != KeyEventKind::Press {
            self.pending = None;
            return TreeAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = area_height.saturating_sub(1).max(1);

        // Handle pending prefix
        if let Some(prefix) = self.pending.take() {
            return match prefix {
                KeyCode::Char('z') => match key.code {
                    KeyCode::Char('t') => TreeAction::ScrollSelectionToTop,
                    KeyCode::Char('z') => TreeAction::ScrollSelectionToMiddle,
                    KeyCode::Char('b') => TreeAction::ScrollSelectionToBottom,
                    KeyCode::Char('h') => TreeAction::ToggleAllHidden,
                    KeyCode::Char('J') => TreeAction::RevealJumpForward,
                    KeyCode::Char('K') => TreeAction::RevealJumpBackward,
                    _ => TreeAction::None,
                },
                KeyCode::Char('y') => match key.code {
                    KeyCode::Char('y') => TreeAction::CopyMarkdown,
                    KeyCode::Char('t') => TreeAction::CopyPlainText,
                    KeyCode::Char('r') => TreeAction::CopyRawData,
                    _ => TreeAction::None,
                },
                KeyCode::Char(']') => match key.code {
                    KeyCode::Char(']') => TreeAction::SelectNextTurnStart,
                    KeyCode::Char('[') => TreeAction::SelectNextTurnEnd,
                    _ => TreeAction::None,
                },
                KeyCode::Char('[') => match key.code {
                    KeyCode::Char('[') => TreeAction::SelectPrevTurnStart,
                    KeyCode::Char(']') => TreeAction::SelectPrevTurnEnd,
                    _ => TreeAction::None,
                },
                _ => TreeAction::None,
            };
        }

        match key.code {
            KeyCode::Char('q') => TreeAction::Quit,
            KeyCode::Char('c' | 'C') if ctrl => TreeAction::Quit,

            KeyCode::Down | KeyCode::Char('j') => TreeAction::SelectNext,
            KeyCode::Up | KeyCode::Char('k') => TreeAction::SelectPrev,
            KeyCode::Right | KeyCode::Char('l') => TreeAction::SelectChild,
            KeyCode::Left | KeyCode::Char('h') => TreeAction::SelectParent,

            KeyCode::Char('J') => TreeAction::RevealNextFive,
            KeyCode::Char('K') => TreeAction::RevealPrevFive,

            KeyCode::Char('o') if !ctrl => TreeAction::OpenNode,
            KeyCode::Char('c') if !ctrl => TreeAction::CloseNode,
            KeyCode::Char('O') => TreeAction::OpenRevealHidden,
            KeyCode::Char('C') => TreeAction::CloseHideRevealed,

            KeyCode::Enter => TreeAction::ToggleExpand,
            KeyCode::Char(' ') => TreeAction::CycleDisplay,
            KeyCode::Char('o') if ctrl => TreeAction::TerminalActivate,

            KeyCode::Char('n') if ctrl => TreeAction::ScrollDown(3),
            KeyCode::Char('p') if ctrl => TreeAction::ScrollUp(3),
            KeyCode::Char('d') if ctrl => TreeAction::ScrollDownHalf(page / 2),
            KeyCode::Char('u') if ctrl => TreeAction::ScrollUpHalf(page / 2),
            KeyCode::PageDown => TreeAction::ScrollDown(page),
            KeyCode::PageUp => TreeAction::ScrollUp(page),

            // viewport-relative selection
            KeyCode::Char('H') => TreeAction::SelectViewportTop,
            KeyCode::Char('M') => TreeAction::SelectViewportMiddle,
            KeyCode::Char('L') => TreeAction::SelectViewportBottom,

            // Y: immediate copy markdown; y prefix: yy/yt/yr for copy variants
            KeyCode::Char('Y') => TreeAction::CopyMarkdown,
            KeyCode::Char('y') => {
                self.pending = Some(KeyCode::Char('y'));
                TreeAction::None
            }

            // z / [ / ] prefix: wait for second key
            KeyCode::Char('z') => {
                self.pending = Some(KeyCode::Char('z'));
                TreeAction::None
            }
            KeyCode::Char(']') => {
                self.pending = Some(KeyCode::Char(']'));
                TreeAction::None
            }
            KeyCode::Char('[') => {
                self.pending = Some(KeyCode::Char('['));
                TreeAction::None
            }

            // jump to first / last content item
            KeyCode::Char('g') => TreeAction::SelectFirst,
            KeyCode::Char('G') => TreeAction::SelectLastContent,

            // message-type run navigation
            KeyCode::Char(')') => TreeAction::SelectNextTypeStart,
            KeyCode::Char('(') => TreeAction::SelectPrevTypeStart,
            KeyCode::Char('}') => TreeAction::SelectNextUserAgent,
            KeyCode::Char('{') => TreeAction::SelectPrevUserAgent,

            _ => TreeAction::None,
        }
    }
}

/// Stateless shim for callers that don't need persistent key state.
pub fn handle_key_event(key: KeyEvent, area_height: u16) -> TreeAction {
    KeyParser::new().process(key, area_height)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{KeyParser, TreeAction};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::empty(), KeyEventKind::Release)
    }

    fn is_none(a: &TreeAction) -> bool {
        matches!(a, TreeAction::None)
    }

    // ── existing z-prefix tests ───────────────────────────────────────────────

    #[test]
    fn z_alone_returns_none_and_sets_pending() {
        let mut p = KeyParser::new();
        let action = p.process(press(KeyCode::Char('z')), 24);
        assert!(is_none(&action));
        assert!(p.pending.is_some());
    }

    #[test]
    fn zt_returns_scroll_to_top() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('z')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char('t')), 24),
            TreeAction::ScrollSelectionToTop
        ));
        assert!(p.pending.is_none());
    }

    #[test]
    fn zz_returns_scroll_to_middle() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('z')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char('z')), 24),
            TreeAction::ScrollSelectionToMiddle
        ));
        assert!(p.pending.is_none());
    }

    #[test]
    fn zb_returns_scroll_to_bottom() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('z')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char('b')), 24),
            TreeAction::ScrollSelectionToBottom
        ));
        assert!(p.pending.is_none());
    }

    #[test]
    fn z_then_unknown_drops_both() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('z')), 24);
        let action = p.process(press(KeyCode::Char('x')), 24);
        assert!(is_none(&action));
        assert!(p.pending.is_none());
    }

    #[test]
    fn unrelated_keys_still_work() {
        let mut p = KeyParser::new();
        assert!(matches!(
            p.process(press(KeyCode::Char('j')), 24),
            TreeAction::SelectNext
        ));
        assert!(matches!(
            p.process(press(KeyCode::Char('k')), 24),
            TreeAction::SelectPrev
        ));
        assert!(matches!(
            p.process(press(KeyCode::Char('g')), 24),
            TreeAction::SelectFirst
        ));
        assert!(matches!(
            p.process(press(KeyCode::Char('G')), 24),
            TreeAction::SelectLastContent
        ));
    }

    #[test]
    fn non_press_events_return_none_and_clear_pending() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('z')), 24);
        assert!(p.pending.is_some());
        let action = p.process(release(KeyCode::Char('t')), 24);
        assert!(is_none(&action));
        assert!(p.pending.is_none());
    }

    // ── Ctrl-D / Ctrl-U ───────────────────────────────────────────────────────

    #[test]
    fn ctrl_d_returns_scroll_down_half() {
        let mut p = KeyParser::new();
        assert!(matches!(
            p.process(ctrl('d'), 24),
            TreeAction::ScrollDownHalf(_)
        ));
    }

    #[test]
    fn ctrl_u_returns_scroll_up_half() {
        let mut p = KeyParser::new();
        assert!(matches!(
            p.process(ctrl('u'), 24),
            TreeAction::ScrollUpHalf(_)
        ));
    }

    // ── () {} single-key actions ───────────────────────────────────────────────

    #[test]
    fn paren_and_brace_map_to_correct_actions() {
        let mut p = KeyParser::new();
        assert!(matches!(
            p.process(press(KeyCode::Char(')')), 24),
            TreeAction::SelectNextTypeStart
        ));
        assert!(matches!(
            p.process(press(KeyCode::Char('(')), 24),
            TreeAction::SelectPrevTypeStart
        ));
        assert!(matches!(
            p.process(press(KeyCode::Char('}')), 24),
            TreeAction::SelectNextUserAgent
        ));
        assert!(matches!(
            p.process(press(KeyCode::Char('{')), 24),
            TreeAction::SelectPrevUserAgent
        ));
    }

    // ── turn navigation ]]  ][ [[ [] ─────────────────────────────────────────

    #[test]
    fn double_bracket_next_turn_start() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char(']')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char(']')), 24),
            TreeAction::SelectNextTurnStart
        ));
        assert!(p.pending.is_none());
    }

    #[test]
    fn bracket_close_open_next_turn_end() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char(']')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char('[')), 24),
            TreeAction::SelectNextTurnEnd
        ));
    }

    #[test]
    fn double_bracket_prev_turn_start() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('[')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char('[')), 24),
            TreeAction::SelectPrevTurnStart
        ));
    }

    #[test]
    fn bracket_open_close_prev_turn_end() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char('[')), 24);
        assert!(matches!(
            p.process(press(KeyCode::Char(']')), 24),
            TreeAction::SelectPrevTurnEnd
        ));
    }

    #[test]
    fn bracket_then_unknown_drops_both() {
        let mut p = KeyParser::new();
        p.process(press(KeyCode::Char(']')), 24);
        assert!(is_none(&p.process(press(KeyCode::Char('x')), 24)));
        p.process(press(KeyCode::Char('[')), 24);
        assert!(is_none(&p.process(press(KeyCode::Char('x')), 24)));
    }
}
