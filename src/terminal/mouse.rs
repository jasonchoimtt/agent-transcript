use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::osc::{MouseEncoding, MouseMode};

/// Encode a crossterm mouse event into PTY input bytes using the encoding the
/// child process requested. Returns `None` if the event should not be forwarded
/// (mode Off, coordinate overflow in X10, or motion not in AnyEvent mode).
pub fn encode_mouse_event(ev: MouseEvent, mode: MouseMode, enc: MouseEncoding) -> Option<Vec<u8>> {
    if mode == MouseMode::Off {
        return None;
    }

    // 1-based coordinates (crossterm gives 0-based).
    let col = ev.column + 1;
    let row = ev.row + 1;

    let mut btn = match ev.kind {
        MouseEventKind::Down(button) => button_code(button),
        MouseEventKind::Up(_) => {
            if enc == MouseEncoding::X10 {
                return None; // X10 has no release events
            }
            3
        }
        // Drag only forwarded in ButtonEvent/AnyEvent mode.
        MouseEventKind::Drag(_)
            if !matches!(mode, MouseMode::ButtonEvent | MouseMode::AnyEvent) =>
        {
            return None;
        }
        MouseEventKind::Drag(button) => button_code(button) + 32,
        MouseEventKind::Moved if mode != MouseMode::AnyEvent => return None,
        MouseEventKind::Moved => 32, // motion with no button held
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };

    // Modifier bits.
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        btn += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        btn += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        btn += 16;
    }

    match enc {
        MouseEncoding::X10 => {
            if col > 223 || row > 223 {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                (btn + 32) as u8,
                (col + 32) as u8,
                (row + 32) as u8,
            ])
        }
        MouseEncoding::Sgr => {
            let final_byte = if matches!(ev.kind, MouseEventKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{btn};{col};{row}{final_byte}").into_bytes())
        }
        MouseEncoding::Urxvt => {
            // URXVT uses btn+32 (same as X10) but decimal coords with no limit.
            Some(format!("\x1b[{};{};{}M", btn + 32, col, row).into_bytes())
        }
    }
}

fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::*;

    fn ev(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn ev_mod(kind: MouseEventKind, col: u16, row: u16, mods: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: mods,
        }
    }

    // ── Off mode always returns None ──────────────────────────────────────────

    #[test]
    fn mode_off_returns_none() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 0, 0),
            MouseMode::Off,
            MouseEncoding::X10,
        );
        assert!(r.is_none());
    }

    // ── X10 encoding ─────────────────────────────────────────────────────────

    #[test]
    fn x10_left_press() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 0, 0),
            MouseMode::Normal,
            MouseEncoding::X10,
        )
        .unwrap();
        // col=1, row=1, btn=0 → bytes: 0x1b [ M (0+32) (1+32) (1+32)
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 32, 33, 33]);
    }

    #[test]
    fn x10_right_press() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Right), 4, 9),
            MouseMode::Normal,
            MouseEncoding::X10,
        )
        .unwrap();
        // btn=2, col=5, row=10
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 34, 37, 42]);
    }

    #[test]
    fn x10_no_release_events() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Up(MouseButton::Left), 0, 0),
            MouseMode::Normal,
            MouseEncoding::X10,
        );
        assert!(r.is_none());
    }

    #[test]
    fn x10_coord_overflow_returns_none() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 223, 0), // col=224 > 223
            MouseMode::Normal,
            MouseEncoding::X10,
        );
        assert!(r.is_none());
    }

    #[test]
    fn x10_coord_at_limit() {
        // col=222 → 1-based 223, which is the max
        let r = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 222, 0),
            MouseMode::Normal,
            MouseEncoding::X10,
        );
        assert!(r.is_some());
    }

    #[test]
    fn x10_scroll_up() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::ScrollUp, 0, 0),
            MouseMode::Normal,
            MouseEncoding::X10,
        )
        .unwrap();
        // btn=64, col=1, row=1 → btn+32=96
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 96, 33, 33]);
    }

    #[test]
    fn x10_scroll_down() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::ScrollDown, 0, 0),
            MouseMode::Normal,
            MouseEncoding::X10,
        )
        .unwrap();
        // btn=65+32=97
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 97, 33, 33]);
    }

    // ── SGR encoding ─────────────────────────────────────────────────────────

    #[test]
    fn sgr_left_press() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 4, 9),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        // btn=0, col=5, row=10, press → M
        assert_eq!(bytes, b"\x1b[<0;5;10M");
    }

    #[test]
    fn sgr_left_release() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Up(MouseButton::Left), 4, 9),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        // release → m
        assert_eq!(bytes, b"\x1b[<3;5;10m");
    }

    #[test]
    fn sgr_large_coords() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 999, 499),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<0;1000;500M");
    }

    #[test]
    fn sgr_middle_press() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Middle), 0, 0),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<1;1;1M");
    }

    // ── URXVT encoding ────────────────────────────────────────────────────────

    #[test]
    fn urxvt_left_press() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::Down(MouseButton::Left), 4, 9),
            MouseMode::Normal,
            MouseEncoding::Urxvt,
        )
        .unwrap();
        // btn+32=32, col=5, row=10
        assert_eq!(bytes, b"\x1b[32;5;10M");
    }

    #[test]
    fn urxvt_scroll_up() {
        let bytes = encode_mouse_event(
            ev(MouseEventKind::ScrollUp, 0, 0),
            MouseMode::Normal,
            MouseEncoding::Urxvt,
        )
        .unwrap();
        // btn=64+32=96
        assert_eq!(bytes, b"\x1b[96;1;1M");
    }

    // ── Modifier bits ─────────────────────────────────────────────────────────

    #[test]
    fn sgr_shift_modifier() {
        let bytes = encode_mouse_event(
            ev_mod(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::SHIFT,
            ),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        // btn=0+4=4
        assert_eq!(bytes, b"\x1b[<4;1;1M");
    }

    #[test]
    fn sgr_alt_modifier() {
        let bytes = encode_mouse_event(
            ev_mod(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::ALT,
            ),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        // btn=0+8=8
        assert_eq!(bytes, b"\x1b[<8;1;1M");
    }

    #[test]
    fn sgr_ctrl_modifier() {
        let bytes = encode_mouse_event(
            ev_mod(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::CONTROL,
            ),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        )
        .unwrap();
        // btn=0+16=16
        assert_eq!(bytes, b"\x1b[<16;1;1M");
    }

    // ── Button event / any event mode filtering ───────────────────────────────

    #[test]
    fn drag_requires_button_event_mode() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Drag(MouseButton::Left), 0, 0),
            MouseMode::Normal,
            MouseEncoding::Sgr,
        );
        assert!(r.is_none());
    }

    #[test]
    fn drag_works_in_button_event_mode() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Drag(MouseButton::Left), 0, 0),
            MouseMode::ButtonEvent,
            MouseEncoding::Sgr,
        );
        assert!(r.is_some());
        // btn=0+32=32 (drag)
        assert_eq!(r.unwrap(), b"\x1b[<32;1;1M");
    }

    #[test]
    fn moved_requires_any_event_mode() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Moved, 0, 0),
            MouseMode::ButtonEvent,
            MouseEncoding::Sgr,
        );
        assert!(r.is_none());
    }

    #[test]
    fn moved_works_in_any_event_mode() {
        let r = encode_mouse_event(
            ev(MouseEventKind::Moved, 4, 9),
            MouseMode::AnyEvent,
            MouseEncoding::Sgr,
        );
        assert!(r.is_some());
        // btn=32 (motion, no button), col=5, row=10
        assert_eq!(r.unwrap(), b"\x1b[<32;5;10M");
    }

    // ── Button mapping ────────────────────────────────────────────────────────

    #[test]
    fn button_mapping_sgr() {
        let cases = [
            (MouseButton::Left, "0"),
            (MouseButton::Middle, "1"),
            (MouseButton::Right, "2"),
        ];
        for (btn, code) in cases {
            let bytes = encode_mouse_event(
                ev(MouseEventKind::Down(btn), 0, 0),
                MouseMode::Normal,
                MouseEncoding::Sgr,
            )
            .unwrap();
            let expected = format!("\x1b[<{code};1;1M");
            assert_eq!(bytes, expected.as_bytes(), "button {btn:?}");
        }
    }
}
