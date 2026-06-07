use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a crossterm key event into the byte sequence a PTY expects.
/// Returns `None` for keys that have no PTY representation.
///
/// `application_cursor` switches arrow-key encoding from CSI (`\x1b[A`) to
/// SS3 (`\x1bOA`) — set when the child process enables DECCKM (`?1h`).
///
/// `application_keypad` is accepted for future numpad encoding; F1–F4 already
/// use SS3 unconditionally, which is correct for VT200+ mode.
pub fn key_event_to_bytes(
    key: &KeyEvent,
    application_cursor: bool,
    _application_keypad: bool,
) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) if ctrl => {
            if c.is_ascii_alphabetic() {
                Some(vec![(c.to_ascii_uppercase() as u8) - b'A' + 1])
            } else if c == ' ' {
                Some(vec![0x00])
            } else {
                None
            }
        }
        KeyCode::Char(c) if alt => Some(format!("\x1b{c}").into_bytes()),
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Enter if alt => Some(b"\x1b\r".to_vec()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace if alt => Some(b"\x1b\x7f".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab if alt => Some(b"\x1b\t".to_vec()),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up if alt => Some(b"\x1b[1;3A".to_vec()),
        KeyCode::Up => Some(if application_cursor {
            b"\x1bOA".to_vec()
        } else {
            b"\x1b[A".to_vec()
        }),
        KeyCode::Down if alt => Some(b"\x1b[1;3B".to_vec()),
        KeyCode::Down => Some(if application_cursor {
            b"\x1bOB".to_vec()
        } else {
            b"\x1b[B".to_vec()
        }),
        KeyCode::Right if alt => Some(b"\x1b[1;3C".to_vec()),
        KeyCode::Right => Some(if application_cursor {
            b"\x1bOC".to_vec()
        } else {
            b"\x1b[C".to_vec()
        }),
        KeyCode::Left if alt => Some(b"\x1b[1;3D".to_vec()),
        KeyCode::Left => Some(if application_cursor {
            b"\x1bOD".to_vec()
        } else {
            b"\x1b[D".to_vec()
        }),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(1) => Some(b"\x1bOP".to_vec()),
        KeyCode::F(2) => Some(b"\x1bOQ".to_vec()),
        KeyCode::F(3) => Some(b"\x1bOR".to_vec()),
        KeyCode::F(4) => Some(b"\x1bOS".to_vec()),
        KeyCode::F(5) => Some(b"\x1b[15~".to_vec()),
        KeyCode::F(6) => Some(b"\x1b[17~".to_vec()),
        KeyCode::F(7) => Some(b"\x1b[18~".to_vec()),
        KeyCode::F(8) => Some(b"\x1b[19~".to_vec()),
        KeyCode::F(9) => Some(b"\x1b[20~".to_vec()),
        KeyCode::F(10) => Some(b"\x1b[21~".to_vec()),
        KeyCode::F(11) => Some(b"\x1b[23~".to_vec()),
        KeyCode::F(12) => Some(b"\x1b[24~".to_vec()),
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press_mod(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn encode(key: KeyEvent) -> Option<Vec<u8>> {
        key_event_to_bytes(&key, false, false)
    }

    fn encode_app_cursor(key: KeyEvent) -> Option<Vec<u8>> {
        key_event_to_bytes(&key, true, false)
    }

    // ── Arrow keys – normal mode ───────────────────────────────────────────────

    #[test]
    fn arrow_up_normal() {
        assert_eq!(encode(press(KeyCode::Up)).unwrap(), b"\x1b[A");
    }

    #[test]
    fn arrow_down_normal() {
        assert_eq!(encode(press(KeyCode::Down)).unwrap(), b"\x1b[B");
    }

    #[test]
    fn arrow_right_normal() {
        assert_eq!(encode(press(KeyCode::Right)).unwrap(), b"\x1b[C");
    }

    #[test]
    fn arrow_left_normal() {
        assert_eq!(encode(press(KeyCode::Left)).unwrap(), b"\x1b[D");
    }

    // ── Arrow keys – application cursor mode ──────────────────────────────────

    #[test]
    fn arrow_up_application_cursor() {
        assert_eq!(encode_app_cursor(press(KeyCode::Up)).unwrap(), b"\x1bOA");
    }

    #[test]
    fn arrow_down_application_cursor() {
        assert_eq!(encode_app_cursor(press(KeyCode::Down)).unwrap(), b"\x1bOB");
    }

    #[test]
    fn arrow_right_application_cursor() {
        assert_eq!(encode_app_cursor(press(KeyCode::Right)).unwrap(), b"\x1bOC");
    }

    #[test]
    fn arrow_left_application_cursor() {
        assert_eq!(encode_app_cursor(press(KeyCode::Left)).unwrap(), b"\x1bOD");
    }

    // ── Special keys ──────────────────────────────────────────────────────────

    #[test]
    fn enter() {
        assert_eq!(encode(press(KeyCode::Enter)).unwrap(), b"\r");
    }

    #[test]
    fn backspace() {
        assert_eq!(encode(press(KeyCode::Backspace)).unwrap(), b"\x7f");
    }

    #[test]
    fn tab() {
        assert_eq!(encode(press(KeyCode::Tab)).unwrap(), b"\t");
    }

    #[test]
    fn esc() {
        assert_eq!(encode(press(KeyCode::Esc)).unwrap(), b"\x1b");
    }

    #[test]
    fn home() {
        assert_eq!(encode(press(KeyCode::Home)).unwrap(), b"\x1b[H");
    }

    #[test]
    fn end() {
        assert_eq!(encode(press(KeyCode::End)).unwrap(), b"\x1b[F");
    }

    #[test]
    fn page_up() {
        assert_eq!(encode(press(KeyCode::PageUp)).unwrap(), b"\x1b[5~");
    }

    #[test]
    fn page_down() {
        assert_eq!(encode(press(KeyCode::PageDown)).unwrap(), b"\x1b[6~");
    }

    #[test]
    fn delete() {
        assert_eq!(encode(press(KeyCode::Delete)).unwrap(), b"\x1b[3~");
    }

    #[test]
    fn insert() {
        assert_eq!(encode(press(KeyCode::Insert)).unwrap(), b"\x1b[2~");
    }

    // ── Ctrl combinations ─────────────────────────────────────────────────────

    #[test]
    fn ctrl_a() {
        assert_eq!(
            encode(press_mod(KeyCode::Char('a'), KeyModifiers::CONTROL)).unwrap(),
            b"\x01"
        );
    }

    #[test]
    fn ctrl_c() {
        assert_eq!(
            encode(press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)).unwrap(),
            b"\x03"
        );
    }

    #[test]
    fn ctrl_z() {
        assert_eq!(
            encode(press_mod(KeyCode::Char('z'), KeyModifiers::CONTROL)).unwrap(),
            b"\x1a"
        );
    }

    #[test]
    fn ctrl_space() {
        assert_eq!(
            encode(press_mod(KeyCode::Char(' '), KeyModifiers::CONTROL)).unwrap(),
            b"\x00"
        );
    }

    // ── Alt combinations ──────────────────────────────────────────────────────

    #[test]
    fn alt_char() {
        assert_eq!(
            encode(press_mod(KeyCode::Char('x'), KeyModifiers::ALT)).unwrap(),
            b"\x1bx"
        );
    }

    #[test]
    fn alt_enter() {
        assert_eq!(
            encode(press_mod(KeyCode::Enter, KeyModifiers::ALT)).unwrap(),
            b"\x1b\r"
        );
    }

    #[test]
    fn alt_backspace() {
        assert_eq!(
            encode(press_mod(KeyCode::Backspace, KeyModifiers::ALT)).unwrap(),
            b"\x1b\x7f"
        );
    }

    #[test]
    fn alt_tab() {
        assert_eq!(
            encode(press_mod(KeyCode::Tab, KeyModifiers::ALT)).unwrap(),
            b"\x1b\t"
        );
    }

    #[test]
    fn alt_arrow_keys() {
        assert_eq!(
            encode(press_mod(KeyCode::Up, KeyModifiers::ALT)).unwrap(),
            b"\x1b[1;3A"
        );
        assert_eq!(
            encode(press_mod(KeyCode::Down, KeyModifiers::ALT)).unwrap(),
            b"\x1b[1;3B"
        );
        assert_eq!(
            encode(press_mod(KeyCode::Right, KeyModifiers::ALT)).unwrap(),
            b"\x1b[1;3C"
        );
        assert_eq!(
            encode(press_mod(KeyCode::Left, KeyModifiers::ALT)).unwrap(),
            b"\x1b[1;3D"
        );
    }

    // ── Function keys ─────────────────────────────────────────────────────────

    #[test]
    fn f1_through_f4_ss3() {
        assert_eq!(encode(press(KeyCode::F(1))).unwrap(), b"\x1bOP");
        assert_eq!(encode(press(KeyCode::F(2))).unwrap(), b"\x1bOQ");
        assert_eq!(encode(press(KeyCode::F(3))).unwrap(), b"\x1bOR");
        assert_eq!(encode(press(KeyCode::F(4))).unwrap(), b"\x1bOS");
    }

    #[test]
    fn f5_through_f12() {
        assert_eq!(encode(press(KeyCode::F(5))).unwrap(), b"\x1b[15~");
        assert_eq!(encode(press(KeyCode::F(6))).unwrap(), b"\x1b[17~");
        assert_eq!(encode(press(KeyCode::F(7))).unwrap(), b"\x1b[18~");
        assert_eq!(encode(press(KeyCode::F(8))).unwrap(), b"\x1b[19~");
        assert_eq!(encode(press(KeyCode::F(9))).unwrap(), b"\x1b[20~");
        assert_eq!(encode(press(KeyCode::F(10))).unwrap(), b"\x1b[21~");
        assert_eq!(encode(press(KeyCode::F(11))).unwrap(), b"\x1b[23~");
        assert_eq!(encode(press(KeyCode::F(12))).unwrap(), b"\x1b[24~");
    }
}
