use vte::{Params, Perform};

// ── Public enums ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum MouseMode {
    #[default]
    Off,
    Normal,
    ButtonEvent,
    AnyEvent,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum MouseEncoding {
    #[default]
    X10,
    Sgr,
    Urxvt,
}

#[derive(Debug)]
pub enum OscEvent {
    MouseMode(MouseMode),
    MouseEncoding(MouseEncoding),
    BracketedPaste(bool),
    FocusEvents(bool),
    Title(String),
    ClipboardWrite { selection: u8, data: String },
    ClipboardRead(u8),
    ColorQuery(u8),
    CursorShape(u8),
    Cwd(String),
    SyncStart,
    SyncEnd,
    SyncQuery,
}

// ── OscScanner ────────────────────────────────────────────────────────────────

pub struct OscScanner {
    parser: vte::Parser,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
        }
    }

    /// Process a chunk of raw PTY bytes.
    ///
    /// Intercepted sequences are emitted into `out` and stripped from the
    /// return value. Everything else is returned as bytes for vt100::Parser.
    pub fn process(&mut self, input: &[u8], out: &mut Vec<OscEvent>) -> Vec<u8> {
        let mut passthrough = Vec::with_capacity(input.len());
        let mut performer = OscPerformer {
            events: out,
            passthrough: &mut passthrough,
        };
        for &byte in input {
            self.parser.advance(&mut performer, byte);
        }
        passthrough
    }
}

// ── OscPerformer (vte::Perform impl) ─────────────────────────────────────────

struct OscPerformer<'a> {
    events: &'a mut Vec<OscEvent>,
    passthrough: &'a mut Vec<u8>,
}

impl Perform for OscPerformer<'_> {
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.passthrough
            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        self.passthrough.push(byte);
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        if let Some(event) = intercept_csi(params, intermediates, c) {
            self.events.push(event);
        } else {
            re_encode_csi(params, intermediates, c, self.passthrough);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        if let Some(event) = intercept_osc(params) {
            self.events.push(event);
        } else {
            re_encode_osc(params, bell_terminated, self.passthrough);
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        re_encode_esc(intermediates, byte, self.passthrough);
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {}
    fn put(&mut self, byte: u8) {
        self.passthrough.push(byte);
    }
    fn unhook(&mut self) {}
}

// ── Intercept logic ───────────────────────────────────────────────────────────

fn intercept_csi(params: &Params, intermediates: &[u8], c: char) -> Option<OscEvent> {
    let is_private = intermediates.contains(&b'?');

    // DEC private mode set/reset: ESC [ ? N h/l
    if is_private && (c == 'h' || c == 'l') {
        let enabled = c == 'h';
        let mode_num = first_param(params)?;
        return match mode_num {
            1000 => Some(OscEvent::MouseMode(if enabled {
                MouseMode::Normal
            } else {
                MouseMode::Off
            })),
            1002 => Some(OscEvent::MouseMode(if enabled {
                MouseMode::ButtonEvent
            } else {
                MouseMode::Off
            })),
            1003 => Some(OscEvent::MouseMode(if enabled {
                MouseMode::AnyEvent
            } else {
                MouseMode::Off
            })),
            1004 => Some(OscEvent::FocusEvents(enabled)),
            1006 => Some(OscEvent::MouseEncoding(if enabled {
                MouseEncoding::Sgr
            } else {
                MouseEncoding::X10
            })),
            1015 => Some(OscEvent::MouseEncoding(if enabled {
                MouseEncoding::Urxvt
            } else {
                MouseEncoding::X10
            })),
            2004 => Some(OscEvent::BracketedPaste(enabled)),
            2026 => Some(if enabled {
                OscEvent::SyncStart
            } else {
                OscEvent::SyncEnd
            }),
            _ => None,
        };
    }

    // DECRQM query for sync: ESC [ ? 2026 $ p
    if intermediates == [b'?', b'$'] && c == 'p' && first_param(params)? == 2026 {
        return Some(OscEvent::SyncQuery);
    }

    // DECSCUSR cursor shape: ESC [ N SP q
    if intermediates == [b' '] && c == 'q' {
        let n = params
            .iter()
            .next()
            .and_then(|p| p.first().copied())
            .unwrap_or(0);
        return Some(OscEvent::CursorShape(n as u8));
    }

    None
}

fn intercept_osc(params: &[&[u8]]) -> Option<OscEvent> {
    let cmd = params.first()?;

    match *cmd {
        b"0" | b"2" => {
            let title = params
                .get(1)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            Some(OscEvent::Title(title))
        }
        b"7" => {
            let url = params
                .get(1)
                .map(|b| String::from_utf8_lossy(b))
                .unwrap_or_default();
            let path = url.strip_prefix("file://").unwrap_or(&url).to_string();
            Some(OscEvent::Cwd(path))
        }
        b"10" | b"11" => {
            let slot: u8 = if *cmd == b"10" { 10 } else { 11 };
            let data = params.get(1).copied().unwrap_or(b"");
            if data == b"?" {
                Some(OscEvent::ColorQuery(slot))
            } else {
                None
            }
        }
        b"52" => {
            let selection = params
                .get(1)
                .and_then(|s| s.first())
                .copied()
                .unwrap_or(b'c');
            let data = params.get(2).copied().unwrap_or(b"");
            if data == b"?" {
                Some(OscEvent::ClipboardRead(selection))
            } else {
                Some(OscEvent::ClipboardWrite {
                    selection,
                    data: String::from_utf8_lossy(data).into_owned(),
                })
            }
        }
        _ => None,
    }
}

// ── Re-encoding helpers ───────────────────────────────────────────────────────

fn re_encode_csi(params: &Params, intermediates: &[u8], c: char, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[");
    // Private parameter bytes (0x3C–0x3F) precede the numeric params on the wire.
    for &b in intermediates {
        if b >= 0x3C {
            out.push(b);
        }
    }
    // Numeric params separated by ';'; subparams separated by ':'.
    let mut first = true;
    for param in params.iter() {
        if !first {
            out.push(b';');
        }
        first = false;
        for (i, &sub) in param.iter().enumerate() {
            if i > 0 {
                out.push(b':');
            }
            push_u16(sub, out);
        }
    }
    // Regular intermediate bytes (0x20–0x2F) follow the numeric params.
    for &b in intermediates {
        if b < 0x30 {
            out.push(b);
        }
    }
    out.push(c as u8);
}

fn re_encode_osc(params: &[&[u8]], bell_terminated: bool, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b]");
    for (i, param) in params.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        out.extend_from_slice(param);
    }
    if bell_terminated {
        out.push(0x07);
    } else {
        out.extend_from_slice(b"\x1b\\");
    }
}

fn re_encode_esc(intermediates: &[u8], byte: u8, out: &mut Vec<u8>) {
    out.push(0x1b);
    out.extend_from_slice(intermediates);
    out.push(byte);
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn first_param(params: &Params) -> Option<u16> {
    params.iter().next()?.first().copied()
}

fn push_u16(n: u16, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let start = out.len();
    let mut v = n;
    while v > 0 {
        out.push(b'0' + (v % 10) as u8);
        v /= 10;
    }
    out[start..].reverse();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(input: &[u8]) -> (Vec<OscEvent>, Vec<u8>) {
        let mut scanner = OscScanner::new();
        let mut events = Vec::new();
        let passthrough = scanner.process(input, &mut events);
        (events, passthrough)
    }

    // ── Mouse mode ────────────────────────────────────────────────────────────

    #[test]
    fn mouse_normal_enable() {
        let (ev, pt) = scan(b"\x1b[?1000h");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::MouseMode(MouseMode::Normal)]));
    }

    #[test]
    fn mouse_button_event_enable() {
        let (ev, pt) = scan(b"\x1b[?1002h");
        assert!(pt.is_empty());
        assert!(matches!(
            ev[..],
            [OscEvent::MouseMode(MouseMode::ButtonEvent)]
        ));
    }

    #[test]
    fn mouse_any_event_enable() {
        let (ev, pt) = scan(b"\x1b[?1003h");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::MouseMode(MouseMode::AnyEvent)]));
    }

    #[test]
    fn mouse_normal_disable() {
        let (ev, pt) = scan(b"\x1b[?1000l");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::MouseMode(MouseMode::Off)]));
    }

    // ── Mouse encoding ────────────────────────────────────────────────────────

    #[test]
    fn mouse_sgr_enable() {
        let (ev, pt) = scan(b"\x1b[?1006h");
        assert!(pt.is_empty());
        assert!(matches!(
            ev[..],
            [OscEvent::MouseEncoding(MouseEncoding::Sgr)]
        ));
    }

    #[test]
    fn mouse_sgr_disable() {
        let (ev, pt) = scan(b"\x1b[?1006l");
        assert!(pt.is_empty());
        assert!(matches!(
            ev[..],
            [OscEvent::MouseEncoding(MouseEncoding::X10)]
        ));
    }

    #[test]
    fn mouse_urxvt_enable() {
        let (ev, pt) = scan(b"\x1b[?1015h");
        assert!(pt.is_empty());
        assert!(matches!(
            ev[..],
            [OscEvent::MouseEncoding(MouseEncoding::Urxvt)]
        ));
    }

    // ── Bracketed paste ───────────────────────────────────────────────────────

    #[test]
    fn bracketed_paste_enable() {
        let (ev, pt) = scan(b"\x1b[?2004h");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::BracketedPaste(true)]));
    }

    #[test]
    fn bracketed_paste_disable() {
        let (ev, pt) = scan(b"\x1b[?2004l");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::BracketedPaste(false)]));
    }

    // ── Focus events ──────────────────────────────────────────────────────────

    #[test]
    fn focus_events_enable() {
        let (ev, pt) = scan(b"\x1b[?1004h");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::FocusEvents(true)]));
    }

    #[test]
    fn focus_events_disable() {
        let (ev, pt) = scan(b"\x1b[?1004l");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::FocusEvents(false)]));
    }

    // ── Sync (mode 2026) ──────────────────────────────────────────────────────

    #[test]
    fn sync_start() {
        let (ev, pt) = scan(b"\x1b[?2026h");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::SyncStart]));
    }

    #[test]
    fn sync_end() {
        let (ev, pt) = scan(b"\x1b[?2026l");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::SyncEnd]));
    }

    #[test]
    fn sync_query() {
        let (ev, pt) = scan(b"\x1b[?2026$p");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::SyncQuery]));
    }

    // ── OSC title ─────────────────────────────────────────────────────────────

    #[test]
    fn osc_title_osc0() {
        let (ev, _pt) = scan(b"\x1b]0;hello\x07");
        match &ev[..] {
            [OscEvent::Title(t)] => assert_eq!(t, "hello"),
            _ => panic!("unexpected events: {ev:?}"),
        }
    }

    #[test]
    fn osc_title_osc2() {
        let (ev, _pt) = scan(b"\x1b]2;world\x07");
        match &ev[..] {
            [OscEvent::Title(t)] => assert_eq!(t, "world"),
            _ => panic!("unexpected events: {ev:?}"),
        }
    }

    // ── OSC clipboard ─────────────────────────────────────────────────────────

    #[test]
    fn clipboard_write() {
        let (ev, _pt) = scan(b"\x1b]52;c;aGVsbG8=\x07");
        match &ev[..] {
            [OscEvent::ClipboardWrite { selection, data }] => {
                assert_eq!(*selection, b'c');
                assert_eq!(data, "aGVsbG8=");
            }
            _ => panic!("unexpected events: {ev:?}"),
        }
    }

    #[test]
    fn clipboard_read() {
        let (ev, _pt) = scan(b"\x1b]52;c;?\x07");
        assert!(matches!(ev[..], [OscEvent::ClipboardRead(b'c')]));
    }

    // ── OSC color query ───────────────────────────────────────────────────────

    #[test]
    fn color_query_bg() {
        let (ev, _pt) = scan(b"\x1b]11;?\x07");
        assert!(matches!(ev[..], [OscEvent::ColorQuery(11)]));
    }

    #[test]
    fn color_query_fg() {
        let (ev, _pt) = scan(b"\x1b]10;?\x07");
        assert!(matches!(ev[..], [OscEvent::ColorQuery(10)]));
    }

    // ── DECSCUSR cursor shape ─────────────────────────────────────────────────

    #[test]
    fn cursor_shape() {
        let (ev, pt) = scan(b"\x1b[5 q");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::CursorShape(5)]));
    }

    #[test]
    fn cursor_shape_default() {
        let (ev, pt) = scan(b"\x1b[0 q");
        assert!(pt.is_empty());
        assert!(matches!(ev[..], [OscEvent::CursorShape(0)]));
    }

    // ── OSC CWD ───────────────────────────────────────────────────────────────

    #[test]
    fn cwd_osc7() {
        let (ev, _pt) = scan(b"\x1b]7;file:///home/user\x07");
        match &ev[..] {
            [OscEvent::Cwd(p)] => assert_eq!(p, "/home/user"),
            _ => panic!("unexpected events: {ev:?}"),
        }
    }

    // ── Passthrough ───────────────────────────────────────────────────────────

    #[test]
    fn plain_text_passthrough() {
        let (ev, pt) = scan(b"hello");
        assert!(ev.is_empty());
        assert_eq!(pt, b"hello");
    }

    #[test]
    fn sgr_color_passthrough() {
        let input = b"\x1b[32mhello\x1b[0m";
        let (ev, pt) = scan(input);
        assert!(ev.is_empty());
        assert_eq!(pt, input);
    }

    #[test]
    fn interleaved_intercepted_and_passthrough() {
        // "foo" + OSC title + "baz": passthrough should be "foobaz"
        let input = b"foo\x1b]0;bar\x07baz";
        let (ev, pt) = scan(input);
        assert!(matches!(ev[..], [OscEvent::Title(_)]));
        assert_eq!(pt, b"foobaz");
    }

    #[test]
    fn unknown_dec_private_mode_passes_through() {
        // ?9999h is unknown — should pass through to vt100 unchanged
        let (ev, pt) = scan(b"\x1b[?9999h");
        assert!(ev.is_empty());
        assert_eq!(pt, b"\x1b[?9999h");
    }

    #[test]
    fn split_across_buffer_boundary() {
        // Sequence split: first half, then second half
        let mut scanner = OscScanner::new();
        let mut events = Vec::new();
        let pt1 = scanner.process(b"\x1b[?200", &mut events);
        let pt2 = scanner.process(b"4h", &mut events);
        assert!(pt1.is_empty());
        assert!(pt2.is_empty());
        assert!(matches!(events[..], [OscEvent::BracketedPaste(true)]));
    }

    // ── DECCKM passthrough (vt100 tracks it) ─────────────────────────────────

    #[test]
    fn decckm_passes_through() {
        let (ev, pt) = scan(b"\x1b[?1h");
        assert!(ev.is_empty());
        assert_eq!(pt, b"\x1b[?1h");
    }
}
