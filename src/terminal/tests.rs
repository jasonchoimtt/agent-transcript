use std::time::Duration;

use portable_pty::CommandBuilder;
use tokio::sync::mpsc;

use crate::event::{AppEvent, Event};
use crate::terminal::crop::NullCropDetector;
use crate::terminal::state::TerminalState;

fn make_terminal() -> (TerminalState, mpsc::UnboundedReceiver<Event>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let state = TerminalState::new_with_cmd(
        CommandBuilder::new("/bin/sh"),
        None,
        Box::new(NullCropDetector),
        tx,
        0,
    )
    .expect("failed to spawn PTY");
    (state, rx)
}

// ── resize debounce ───────────────────────────────────────────────────────────

#[test]
fn resize_debounce_vt100_immediate() {
    let (mut term, _rx) = make_terminal();
    term.resize_cols(100);
    let (_, cols) = term.parser.screen().size();
    assert_eq!(cols, 100, "vt100 width should update immediately");
}

#[test]
fn resize_debounce_sigwinch_deferred() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args([
        "-c",
        "trap 'printf WINCH' WINCH; while true; do sleep 0.01; done",
    ]);
    let mut term = TerminalState::new_with_cmd(cmd, None, Box::new(NullCropDetector), tx, 0)
        .expect("failed to spawn PTY");

    // Let the shell start up then drain any initial output.
    std::thread::sleep(Duration::from_millis(100));
    while rx.try_recv().is_ok() {}

    // Rapid sequence of resizes — none should trigger SIGWINCH before debounce.
    for w in [90u16, 95, 100, 105, 110] {
        term.resize_cols(w);
    }

    term.flush_pending_resize();
    std::thread::sleep(Duration::from_millis(50));
    let early = drain_output(&mut rx);
    assert!(
        !early.contains("WINCH"),
        "SIGWINCH should not fire before debounce elapses; got: {early:?}"
    );

    // After the 250 ms window, exactly one SIGWINCH should be sent.
    std::thread::sleep(Duration::from_millis(250));
    term.flush_pending_resize();
    std::thread::sleep(Duration::from_millis(100));

    let output = drain_output(&mut rx);
    let count = output.matches("WINCH").count();
    assert_eq!(
        count, 1,
        "exactly one SIGWINCH expected after debounce; got {count} in {output:?}"
    );
}

// ── process_output batch consistency ─────────────────────────────────────────

#[test]
fn process_output_batch_consistency() {
    // An SGR-colored "hello" split across 3-byte fragments should produce
    // the same screen as a single delivery — validating the drain-before-draw invariant.
    let ansi = b"\x1b[32mhello\x1b[0m";

    let (mut one_shot, _rx1) = make_terminal();
    one_shot.process_output(ansi);

    let (mut fragmented, _rx2) = make_terminal();
    for chunk in ansi.chunks(3) {
        fragmented.process_output(chunk);
    }

    let read_row = |term: &mut TerminalState| -> String {
        (0..5)
            .filter_map(|c| term.parser.screen().cell(0, c))
            .map(|cell| cell.contents().to_string())
            .collect()
    };

    let row_one_shot = read_row(&mut one_shot);
    let row_fragmented = read_row(&mut fragmented);

    assert_eq!(row_one_shot, "hello");
    assert_eq!(
        row_one_shot, row_fragmented,
        "fragmented delivery must match one-shot"
    );
}

// ── process_output mode tracking (Phase 2 omissions) ─────────────────────────

#[test]
fn process_output_bracketed_paste_enable() {
    let (mut term, _rx) = make_terminal();
    term.process_output(b"\x1b[?2004h");
    assert!(
        term.bracketed_paste,
        "bracketed_paste should be true after ?2004h"
    );
}

#[test]
fn process_output_bracketed_paste_disable() {
    let (mut term, _rx) = make_terminal();
    term.process_output(b"\x1b[?2004h");
    term.process_output(b"\x1b[?2004l");
    assert!(
        !term.bracketed_paste,
        "bracketed_paste should be false after ?2004l"
    );
}

#[test]
fn process_output_focus_events_enable() {
    let (mut term, _rx) = make_terminal();
    term.process_output(b"\x1b[?1004h");
    assert!(
        term.focus_events,
        "focus_events should be true after ?1004h"
    );
}

#[test]
fn process_output_sync_start() {
    let (mut term, _rx) = make_terminal();
    term.process_output(b"\x1b[?2026h");
    assert!(term.sync_locked, "sync_locked should be true after ?2026h");
}

#[test]
fn process_output_sync_end() {
    let (mut term, _rx) = make_terminal();
    term.process_output(b"\x1b[?2026h");
    term.process_output(b"\x1b[?2026l");
    assert!(
        !term.sync_locked,
        "sync_locked should be false after ?2026l"
    );
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn drain_output(rx: &mut mpsc::UnboundedReceiver<Event>) -> String {
    let mut out = String::new();
    while let Ok(event) = rx.try_recv() {
        if let Event::App(AppEvent::TerminalOutput(bytes)) = event {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    out
}
