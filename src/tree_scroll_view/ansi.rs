/// Number of visible columns in `s` (ANSI escape sequences excluded).
///
/// Uses a lightweight state machine to skip CSI/OSC sequences without
/// allocating. Each Unicode scalar value counts as 1 column (no wide-char
/// handling — consistent with the rest of the codebase).
pub fn visual_width(s: &str) -> usize {
    let mut state = AscState::Normal;
    let mut count = 0usize;
    for ch in s.chars() {
        state = match state {
            AscState::Normal => {
                if ch == '\x1b' {
                    AscState::Esc
                } else {
                    count += 1;
                    AscState::Normal
                }
            }
            AscState::Esc => match ch {
                '[' => AscState::Csi,
                ']' => AscState::Osc,
                _ => AscState::Normal, // single-char escape — consume silently
            },
            AscState::Csi => {
                // Final byte is in 0x40-0x7e ('@'..='~')
                if ('@'..='~').contains(&ch) {
                    AscState::Normal
                } else {
                    AscState::Csi
                }
            }
            AscState::Osc => {
                if ch == '\x07' {
                    AscState::Normal // BEL terminates OSC
                } else if ch == '\x1b' {
                    AscState::OscEsc // potential ST (ESC \)
                } else {
                    AscState::Osc
                }
            }
            AscState::OscEsc => AscState::Normal, // ESC + any char ends OSC
        };
    }
    count
}

/// Clip `s` to at most `max` visible columns.
///
/// Returns `(clipped_slice, was_truncated)`. The slice is byte-safe (never
/// splits a UTF-8 sequence or cuts inside an ANSI escape) and can be passed
/// directly to `ansi_to_tui::IntoText::into_text()` — the parser handles
/// incomplete escape sequences at the clip boundary gracefully.
pub fn clip_to_visual_width(s: &str, max: usize) -> (&str, bool) {
    let mut state = AscState::Normal;
    let mut visible = 0usize;
    for (byte_pos, ch) in s.char_indices() {
        state = match state {
            AscState::Normal => {
                if ch == '\x1b' {
                    AscState::Esc
                } else {
                    if visible == max {
                        return (&s[..byte_pos], true);
                    }
                    visible += 1;
                    AscState::Normal
                }
            }
            AscState::Esc => match ch {
                '[' => AscState::Csi,
                ']' => AscState::Osc,
                _ => AscState::Normal,
            },
            AscState::Csi => {
                if ('@'..='~').contains(&ch) {
                    AscState::Normal
                } else {
                    AscState::Csi
                }
            }
            AscState::Osc => {
                if ch == '\x07' {
                    AscState::Normal
                } else if ch == '\x1b' {
                    AscState::OscEsc
                } else {
                    AscState::Osc
                }
            }
            AscState::OscEsc => AscState::Normal,
        };
    }
    (s, false)
}

#[derive(Clone, Copy)]
enum AscState {
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── visual_width ─────────────────────────────────────────────────────────

    #[test]
    fn plain_ascii() {
        assert_eq!(visual_width("hello"), 5);
    }

    #[test]
    fn empty() {
        assert_eq!(visual_width(""), 0);
    }

    #[test]
    fn escape_only() {
        assert_eq!(visual_width("\x1b[32m\x1b[0m"), 0);
    }

    #[test]
    fn single_colour_escape() {
        assert_eq!(visual_width("\x1b[32mhello\x1b[0m"), 5);
    }

    #[test]
    fn bold_and_reset() {
        assert_eq!(visual_width("\x1b[1mbold\x1b[0m text"), 9);
    }

    #[test]
    fn indexed_256_colour() {
        assert_eq!(visual_width("\x1b[38;5;208morange\x1b[0m"), 6);
    }

    #[test]
    fn rgb_truecolour() {
        assert_eq!(visual_width("\x1b[38;2;255;128;0mcolour\x1b[0m"), 6);
    }

    #[test]
    fn mixed_escape_and_text() {
        assert_eq!(visual_width("ab\x1b[31mcd\x1b[0mef"), 6);
    }

    #[test]
    fn unicode_chars() {
        assert_eq!(visual_width("héllo"), 5);
        assert_eq!(visual_width("\x1b[32mhéllo\x1b[0m"), 5);
    }

    // ── clip_to_visual_width ─────────────────────────────────────────────────

    #[test]
    fn clip_plain_no_truncation() {
        let (s, t) = clip_to_visual_width("hello", 5);
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn clip_plain_exact_boundary() {
        let (s, t) = clip_to_visual_width("hello", 3);
        assert_eq!(s, "hel");
        assert!(t);
    }

    #[test]
    fn clip_before_escape() {
        let (s, t) = clip_to_visual_width("\x1b[32mhello\x1b[0m", 3);
        assert_eq!(s, "\x1b[32mhel");
        assert!(t);
    }

    #[test]
    fn clip_after_escape_fits() {
        let (s, t) = clip_to_visual_width("\x1b[32mhi\x1b[0m", 10);
        assert_eq!(s, "\x1b[32mhi\x1b[0m");
        assert!(!t);
    }

    #[test]
    fn clip_escape_only_no_truncation() {
        let (s, t) = clip_to_visual_width("\x1b[32m\x1b[0m", 5);
        assert_eq!(s, "\x1b[32m\x1b[0m");
        assert!(!t);
    }

    #[test]
    fn clip_zero_max() {
        let (s, t) = clip_to_visual_width("hello", 0);
        assert_eq!(s, "");
        assert!(t);
    }

    #[test]
    fn clip_zero_max_escape_prefix() {
        // escape at start counts as 0 visible; first real char triggers clip
        let (s, t) = clip_to_visual_width("\x1b[32mhello", 0);
        assert_eq!(s, "\x1b[32m");
        assert!(t);
    }

    #[test]
    fn clip_unicode_boundary() {
        let (s, t) = clip_to_visual_width("héllo", 3);
        assert_eq!(s, "hél");
        assert!(t);
    }
}
