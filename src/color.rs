/// A 24-bit RGB color (8 bits per channel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Fallback background color used when the host terminal doesn't respond to OSC 11.
pub(crate) const DEFAULT_DARK_BG: RgbColor = RgbColor {
    r: 0x1c,
    g: 0x1e,
    b: 0x1f,
};

/// Query the host terminal's background color.
///
/// Checks `$COLORFGBG` first (fast, no I/O). Falls back to sending an OSC 11
/// query to the host terminal and reading the response with a 100 ms timeout.
/// Returns `None` if both methods fail; callers should default to dark.
///
/// Must be called before the crossterm event loop starts — once EventTask is
/// running, it competes for stdin.
pub fn query_host_bg_color() -> Option<RgbColor> {
    use std::io::Write;

    // Fast path: $COLORFGBG is set by most terminal emulators.
    if let Some(color) = parse_colorfgbg() {
        return Some(color);
    }

    // OSC 11 query: enable raw mode, send query, read response.
    crossterm::terminal::enable_raw_mode().ok()?;
    let _ = std::io::stdout().write_all(b"\x1b]11;?\x07");
    let _ = std::io::stdout().flush();
    let response = read_stdin_timeout_ms(100);
    let _ = crossterm::terminal::disable_raw_mode();

    parse_osc_color_response(&response)
}

/// Parse an OSC 10/11 color string like `rgb:RRRR/GGGG/BBBB`.
///
/// The hex components may be 1-4 digits. 4-digit values are scaled to 8-bit
/// by taking the high byte (`v >> 8`); shorter values are used as-is.
pub fn parse_osc_color(s: &str) -> Option<RgbColor> {
    let s = s.strip_prefix("rgb:")?;
    let mut parts = s.splitn(3, '/');
    let rs = parts.next()?;
    let gs = parts.next()?;
    let bs = parts.next()?;
    let rv = u16::from_str_radix(rs, 16).ok()?;
    let gv = u16::from_str_radix(gs, 16).ok()?;
    let bv = u16::from_str_radix(bs, 16).ok()?;
    let scale =
        |v: u16, digits: usize| -> u8 { if digits <= 2 { v as u8 } else { (v >> 8) as u8 } };
    Some(RgbColor {
        r: scale(rv, rs.len()),
        g: scale(gv, gs.len()),
        b: scale(bv, bs.len()),
    })
}

/// Parse a raw OSC 10/11 terminal response buffer (including ESC ] ... BEL/ST).
pub fn parse_osc_color_response(bytes: &[u8]) -> Option<RgbColor> {
    let s = std::str::from_utf8(bytes).ok()?;
    // Strip leading ESC ] and trailing BEL (0x07) or ST (\x1b\)
    let s = s.trim_start_matches('\x1b').trim_start_matches(']');
    let s = s
        .trim_end_matches('\x07')
        .trim_end_matches('\\')
        .trim_end_matches('\x1b');
    // Find the "rgb:" substring (after "10;" or "11;")
    let idx = s.find("rgb:")?;
    parse_osc_color(&s[idx..])
}

/// Compute relative luminance per WCAG 2.0 (0.0 = black, 1.0 = white).
pub fn relative_luminance(c: RgbColor) -> f64 {
    fn linearize(v: u8) -> f64 {
        let s = v as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linearize(c.r) + 0.7152 * linearize(c.g) + 0.0722 * linearize(c.b)
}

/// Parse `$COLORFGBG` ("fg_index;bg_index") and return an approximate RgbColor
/// for the background. Indices 0-8 map to a near-black dark background;
/// indices 9-15 map to a near-white light background.
pub fn parse_colorfgbg() -> Option<RgbColor> {
    let val = std::env::var("COLORFGBG").ok()?;
    colorfgbg_str_to_rgb(&val)
}

fn colorfgbg_str_to_rgb(s: &str) -> Option<RgbColor> {
    let mut parts = s.split(';');
    let _fg = parts.next()?;
    let bg: u8 = parts.next()?.trim().parse().ok()?;
    if bg <= 8 {
        Some(DEFAULT_DARK_BG)
    } else {
        Some(RgbColor {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        })
    }
}

/// Read bytes from stdin until an OSC response terminator (BEL or ST) is seen
/// or `timeout_ms` milliseconds elapse. Uses `libc::poll` for a non-blocking
/// wait so no background thread is left behind after the timeout.
fn read_stdin_timeout_ms(timeout_ms: u64) -> Vec<u8> {
    use std::os::unix::io::AsRawFd;

    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms);

    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            break;
        }
        let remaining_ms = (timeout - elapsed).as_millis() as i32;

        // SAFETY: poll is async-signal-safe and we pass a valid pollfd.
        let ready = unsafe {
            let mut pfd = libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut pfd as *mut libc::pollfd, 1, remaining_ms)
        };
        if ready <= 0 {
            break;
        }

        // SAFETY: fd is valid, byte is a single writable byte.
        let mut byte = 0u8;
        let n = unsafe { libc::read(stdin_fd, &mut byte as *mut u8 as *mut libc::c_void, 1) };
        if n != 1 {
            break;
        }

        buf.push(byte);
        let len = buf.len();
        // BEL-terminated or ST-terminated OSC response
        if byte == 0x07 || (len >= 2 && buf[len - 2] == 0x1b && byte == b'\\') {
            break;
        }
        if len > 256 {
            break;
        }
    }
    buf
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_osc_color ───────────────────────────────────────────────────────

    #[test]
    fn parse_osc_color_white() {
        let c = parse_osc_color("rgb:ffff/ffff/ffff").unwrap();
        assert_eq!(
            c,
            RgbColor {
                r: 255,
                g: 255,
                b: 255
            }
        );
    }

    #[test]
    fn parse_osc_color_black() {
        let c = parse_osc_color("rgb:0000/0000/0000").unwrap();
        assert_eq!(c, RgbColor { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn parse_osc_color_dark_grey() {
        let c = parse_osc_color("rgb:1c1c/1e1e/1f1f").unwrap();
        assert_eq!(
            c,
            RgbColor {
                r: 0x1c,
                g: 0x1e,
                b: 0x1f
            }
        );
    }

    #[test]
    fn parse_osc_color_malformed_returns_none() {
        assert!(parse_osc_color("rgb:gg/00/00").is_none());
        assert!(parse_osc_color("").is_none());
        assert!(parse_osc_color("rgb:ff/ff").is_none());
        assert!(parse_osc_color("notrgb:ff/ff/ff").is_none());
    }

    // ── relative_luminance ────────────────────────────────────────────────────

    #[test]
    fn luminance_white_is_one() {
        let l = relative_luminance(RgbColor {
            r: 255,
            g: 255,
            b: 255,
        });
        assert!((l - 1.0).abs() < 1e-6, "expected ~1.0, got {l}");
    }

    #[test]
    fn luminance_black_is_zero() {
        let l = relative_luminance(RgbColor { r: 0, g: 0, b: 0 });
        assert!(l.abs() < 1e-10, "expected ~0.0, got {l}");
    }

    #[test]
    fn luminance_dark_bg_is_low() {
        let l = relative_luminance(RgbColor {
            r: 28,
            g: 30,
            b: 31,
        });
        assert!(l < 0.02, "expected < 0.02 for dark bg, got {l}");
    }

    // ── parse_colorfgbg ───────────────────────────────────────────────────────

    #[test]
    fn colorfgbg_dark() {
        // "15;0": fg=white(15), bg=black(0) → dark theme
        let c = colorfgbg_str_to_rgb("15;0").unwrap();
        assert!(
            relative_luminance(c) < 0.5,
            "bg=0 should yield a dark color"
        );
    }

    #[test]
    fn colorfgbg_light() {
        // "0;15": fg=black(0), bg=white(15) → light theme
        let c = colorfgbg_str_to_rgb("0;15").unwrap();
        assert!(
            relative_luminance(c) > 0.5,
            "bg=15 should yield a light color"
        );
    }

    // ── parse_osc_color_response ──────────────────────────────────────────────

    #[test]
    fn parse_response_bel_terminated() {
        let resp = b"\x1b]11;rgb:1c1c/1e1e/1f1f\x07";
        let c = parse_osc_color_response(resp).unwrap();
        assert_eq!(
            c,
            RgbColor {
                r: 0x1c,
                g: 0x1e,
                b: 0x1f
            }
        );
    }

    #[test]
    fn parse_response_st_terminated() {
        let resp = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        let c = parse_osc_color_response(resp).unwrap();
        assert_eq!(
            c,
            RgbColor {
                r: 255,
                g: 255,
                b: 255
            }
        );
    }
}
