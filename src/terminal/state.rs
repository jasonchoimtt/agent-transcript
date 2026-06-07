use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SUPPRESS_WINDOW: Duration = Duration::from_millis(30);

use color_eyre::eyre::eyre;
use crossterm::event::Event as CrosstermEvent;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;
use tracing::debug;

use crate::color::{DEFAULT_DARK_BG, RgbColor};
use crate::event::{AppEvent, Event};
use crate::terminal::crop::{CollapsedCrop, CropDetector};
use crate::terminal::osc::{MouseEncoding, MouseMode, OscEvent, OscScanner};

const INITIAL_ROWS: u16 = 20;
const SCROLLBACK_LEN: usize = 10_000;
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);

/// How long a sync lock is allowed to remain set before the watchdog force-clears it.
const SYNC_WATCHDOG: Duration = Duration::from_secs(1);

// portable-pty uses anyhow::Error which doesn't impl std::error::Error,
// so we convert by formatting the error message.
macro_rules! pty {
    ($e:expr) => {
        $e.map_err(|e| eyre!("{e}"))?
    };
}

pub struct TerminalState {
    pub parser: vt100::Parser,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    writer: Box<dyn std::io::Write + Send>,
    pub rows: u16,
    cols: u16,
    pending_resize: Option<(u16, u16)>,
    last_resize_at: Instant,

    // OscScanner and tracked child-process terminal modes.
    scanner: OscScanner,
    pub mouse_mode: MouseMode,
    pub mouse_encoding: MouseEncoding,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub terminal_title: Option<String>,
    pub terminal_cwd: Option<String>,
    pub cursor_shape: u8,
    pub sync_locked: bool,
    pub sync_lock_deadline: Option<Instant>,

    crop_detector: Box<dyn CropDetector>,
    /// Most-recently computed crop region for the collapsed terminal view.
    pub collapsed_crop: Option<CollapsedCrop>,
    pub collapsed_crop_is_alt_screen: bool,
    /// Minimum height to report for a detected crop (0 = no minimum).
    pub crop_min_height: u16,

    /// Cursor row recorded at the last non-suppressed render.
    settled_cursor_row: Option<u16>,
    /// When `Some`, renders are suppressed until this deadline or until cursor returns.
    suppress_render_deadline: Option<Instant>,
}

impl TerminalState {
    pub fn new(sender: mpsc::UnboundedSender<Event>, terminal_id: u64) -> color_eyre::Result<Self> {
        use crate::terminal::crop::NullCropDetector;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::new_with_cmd(
            CommandBuilder::new(shell),
            None,
            Box::new(NullCropDetector),
            sender,
            terminal_id,
        )
    }

    pub fn new_with_cmd(
        mut cmd: CommandBuilder,
        cwd: Option<PathBuf>,
        crop_detector: Box<dyn CropDetector>,
        sender: mpsc::UnboundedSender<Event>,
        terminal_id: u64,
    ) -> color_eyre::Result<Self> {
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let cols: u16 = 80;

        let pty_system = native_pty_system();
        let pair = pty!(pty_system.openpty(PtySize {
            rows: INITIAL_ROWS,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }));

        let child = Arc::new(Mutex::new(pty!(pair.slave.spawn_command(cmd))));
        drop(pair.slave);

        let reader = pty!(pair.master.try_clone_reader());
        let writer = pty!(pair.master.take_writer());

        let child_for_thread = Arc::clone(&child);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        // Use blocking wait() — try_wait() can return None if the
                        // process hasn't been fully reaped at PTY-EOF time.
                        let code = child_for_thread
                            .lock()
                            .ok()
                            .and_then(|mut c| c.wait().ok())
                            .map(|s| s.exit_code() as i32);
                        let _ =
                            sender.send(Event::App(AppEvent::TerminalExited { terminal_id, code }));
                        break;
                    }
                    Ok(n) => {
                        if sender
                            .send(Event::App(AppEvent::TerminalOutput(buf[..n].to_vec())))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            parser: vt100::Parser::new(INITIAL_ROWS, cols, SCROLLBACK_LEN),
            master: pair.master,
            child,
            writer,
            rows: INITIAL_ROWS,
            cols,
            pending_resize: None,
            last_resize_at: Instant::now(),
            scanner: OscScanner::new(),
            mouse_mode: MouseMode::default(),
            mouse_encoding: MouseEncoding::default(),
            bracketed_paste: false,
            focus_events: false,
            terminal_title: None,
            terminal_cwd: None,
            cursor_shape: 0,
            sync_locked: false,
            sync_lock_deadline: None,
            crop_detector,
            collapsed_crop: None,
            collapsed_crop_is_alt_screen: false,
            crop_min_height: 0,
            settled_cursor_row: None,
            suppress_render_deadline: None,
        })
    }

    pub fn handle_crossterm_event(&mut self, event: CrosstermEvent) {
        match event {
            crossterm::event::Event::Paste(s) => {
                if self.bracketed_paste {
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(s.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    self.write_input(&bytes);
                } else {
                    self.write_input(s.as_bytes());
                }
            }
            crossterm::event::Event::FocusGained => {
                if self.focus_events {
                    self.write_input(b"\x1b[I");
                }
            }
            crossterm::event::Event::FocusLost => {
                if self.focus_events {
                    self.write_input(b"\x1b[O");
                }
            }
            _ => {}
        }
    }

    /// Dispatch OSC events that require I/O or terminal-state changes.
    ///
    /// Uses `active` to gate cursor-shape forwarding.
    pub fn handle_osc_events(
        &mut self,
        events: Vec<OscEvent>,
        host_bg: Option<RgbColor>,
        active: bool,
    ) {
        for event in events {
            match event {
                OscEvent::SyncStart => {
                    self.sync_lock_deadline = Some(Instant::now() + SYNC_WATCHDOG);
                }
                OscEvent::SyncEnd => {}
                OscEvent::SyncQuery => {
                    self.write_input(b"\x1b[?2026;2$y");
                }
                OscEvent::Title(ref title) => {
                    let seq = format!("\x1b]0;{title}\x07");
                    let _ = std::io::stdout().write_all(seq.as_bytes());
                    let _ = std::io::stdout().flush();
                }
                OscEvent::ClipboardWrite {
                    selection,
                    ref data,
                } => {
                    let sel = selection as char;
                    let seq = format!("\x1b]52;{sel};{data}\x07");
                    let _ = std::io::stdout().write_all(seq.as_bytes());
                    let _ = std::io::stdout().flush();
                }
                OscEvent::ClipboardRead(_) => {}
                OscEvent::CursorShape(_) => {
                    if active {
                        self.apply_cursor_shape();
                    }
                }
                OscEvent::ColorQuery(slot) => {
                    let bg = host_bg.unwrap_or(DEFAULT_DARK_BG);
                    let r16 = (bg.r as u32) * 257;
                    let g16 = (bg.g as u32) * 257;
                    let b16 = (bg.b as u32) * 257;
                    let response = format!("\x1b]{slot};rgb:{r16:04x}/{g16:04x}/{b16:04x}\x07");
                    self.write_input(response.as_bytes());
                }
                OscEvent::MouseMode(_)
                | OscEvent::MouseEncoding(_)
                | OscEvent::BracketedPaste(_)
                | OscEvent::FocusEvents(_)
                | OscEvent::Cwd(_) => {}
            }
        }
    }

    /// Apply the live terminal's stored cursor shape to the host terminal.
    pub fn apply_cursor_shape(&self) {
        let n = self.cursor_shape;
        let seq = format!("\x1b[{n} q");
        let _ = std::io::stdout().write_all(seq.as_bytes());
        let _ = std::io::stdout().flush();
    }

    pub fn on_tick(&mut self) {
        self.flush_pending_resize();
        if let Some(deadline) = self.sync_lock_deadline
            && Instant::now() >= deadline
        {
            self.sync_locked = false;
            self.sync_lock_deadline = None;
        }
    }

    /// Process raw PTY output. Intercepted sequences update internal state and
    /// are stripped from what the vt100 parser sees. Returns the full list of
    /// `OscEvent`s for the caller (`App`) to act on.
    pub fn process_output(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        let vt_bytes = self.scanner.process(bytes, &mut events);
        self.parser.process(&vt_bytes);

        // Apply pure state updates immediately so callers can read them.
        for event in &events {
            match event {
                OscEvent::MouseMode(m) => self.mouse_mode = *m,
                OscEvent::MouseEncoding(e) => self.mouse_encoding = *e,
                OscEvent::BracketedPaste(b) => self.bracketed_paste = *b,
                OscEvent::FocusEvents(b) => self.focus_events = *b,
                OscEvent::Title(t) => self.terminal_title = Some(t.clone()),
                OscEvent::Cwd(p) => self.terminal_cwd = Some(p.clone()),
                OscEvent::CursorShape(n) => self.cursor_shape = *n,
                OscEvent::SyncStart => {
                    tracing::debug!("sync start");
                    self.sync_locked = true;
                }
                OscEvent::SyncEnd => {
                    tracing::debug!("sync end");
                    self.sync_locked = false;
                    self.sync_lock_deadline = None;
                }
                _ => {}
            }
        }

        self.update_cursor_suppression();
        events
    }

    fn update_cursor_suppression(&mut self) {
        let cursor_row = self.parser.screen().cursor_position().0;
        if let Some(settled) = self.settled_cursor_row {
            if cursor_row < settled {
                self.suppress_render_deadline = Some(Instant::now() + SUPPRESS_WINDOW);
            } else if self.suppress_render_deadline.is_some() {
                self.suppress_render_deadline = None;
            }
        }
    }

    /// Returns true if renders should be skipped while the cursor is mid-update.
    pub fn render_suppressed(&self) -> bool {
        self.suppress_render_deadline
            .is_some_and(|d| Instant::now() < d)
    }

    /// Called by `App` after each non-skipped render to anchor the settled cursor row.
    pub fn notify_rendered(&mut self) {
        self.settled_cursor_row = Some(self.parser.screen().cursor_position().0);
    }

    /// Resize the PTY to the given dimensions.
    ///
    /// The vt100 parser is updated immediately (cheap — no reflow). The PTY
    /// master resize (which sends SIGWINCH to the child) is debounced: it only
    /// fires after the size has been stable for 250 ms, preventing a cascade
    /// of full scrollback redraws while the user is dragging the window edge.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        // set_size keeps the existing scrollback; replacing the parser would discard it.
        self.parser.screen_mut().set_size(rows, cols);
        self.pending_resize = Some((rows, cols));
        self.last_resize_at = Instant::now();
    }

    /// Resize only columns; rows stay unchanged.
    pub fn resize_cols(&mut self, cols: u16) {
        self.resize(self.rows, cols);
    }

    /// Resize only rows; columns stay unchanged.
    pub fn resize_rows(&mut self, rows: u16) {
        self.resize(rows, self.cols);
    }

    /// Send a deferred PTY master resize if the debounce window has elapsed.
    /// Called on every Tick so no additional timer task is needed.
    pub fn flush_pending_resize(&mut self) {
        if self.pending_resize.is_some() && self.last_resize_at.elapsed() >= RESIZE_DEBOUNCE {
            let (rows, cols) = self.pending_resize.take().unwrap();
            let _ = self.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Returns how many scrollback rows are currently stored.
    pub fn scrollback_available(&mut self) -> u16 {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let n = screen.scrollback() as u16;
        screen.set_scrollback(0);
        n
    }

    pub fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Send SIGKILL to the child process.
    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    /// Run the crop detector against the current live screen and store the result.
    /// Called after each batch of PTY output in the main event loop.
    /// If detection returns None, the previous value is kept (sticky fallback).
    pub fn recompute_crop(&mut self) {
        let screen = self.parser.screen();

        // Clear crop if entering or leaving alt screen
        if screen.alternate_screen() != self.collapsed_crop_is_alt_screen {
            self.collapsed_crop = None;
            self.collapsed_crop_is_alt_screen = screen.alternate_screen();
        }

        if let Some(mut crop) = self.crop_detector.detect(screen) {
            if self.crop_min_height > 0 && crop.height < self.crop_min_height {
                let (rows, _) = screen.size();
                let available = rows.saturating_sub(crop.start_row);
                crop.height = self.crop_min_height.min(available);
            }
            match self.collapsed_crop {
                None => debug!("initial crop: {:?}", crop),
                Some(prev_crop) => {
                    if prev_crop != crop {
                        debug!("crop changed: {:?}", crop)
                    }
                }
            }
            self.collapsed_crop = Some(crop);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use portable_pty::CommandBuilder;
    use tokio::sync::mpsc;

    use super::*;
    use crate::terminal::crop::NullCropDetector;

    fn make_ts() -> TerminalState {
        let sender = mpsc::unbounded_channel().0;
        TerminalState::new_with_cmd(
            CommandBuilder::new("sh"),
            None,
            Box::new(NullCropDetector),
            sender,
            0,
        )
        .expect("sh must be available")
    }

    // Move cursor to 0-based (row, col) using ANSI CUP (1-based in the sequence).
    fn cup(row: u16, col: u16) -> Vec<u8> {
        format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
    }

    #[test]
    fn no_suppression_before_first_notify_rendered() {
        let mut ts = make_ts();
        // Without notify_rendered(), settled_cursor_row is None — no suppression.
        ts.process_output(&cup(3, 0));
        assert!(!ts.render_suppressed());
    }

    #[test]
    fn suppression_triggers_when_cursor_moves_above_settled() {
        let mut ts = make_ts();
        ts.process_output(&cup(5, 0));
        ts.notify_rendered(); // settled = 5
        ts.process_output(&cup(2, 0)); // cursor moves above settled
        assert!(ts.render_suppressed());
    }

    #[test]
    fn suppression_lifts_when_cursor_returns_to_settled() {
        let mut ts = make_ts();
        ts.process_output(&cup(5, 0));
        ts.notify_rendered(); // settled = 5
        ts.process_output(&cup(2, 0)); // depart
        assert!(ts.render_suppressed());
        ts.process_output(&cup(5, 0)); // return
        assert!(!ts.render_suppressed());
    }

    #[test]
    fn suppression_lifts_when_cursor_returns_below_settled() {
        let mut ts = make_ts();
        ts.process_output(&cup(5, 0));
        ts.notify_rendered();
        ts.process_output(&cup(1, 0));
        assert!(ts.render_suppressed());
        ts.process_output(&cup(7, 0)); // below settled — also clears suppression
        assert!(!ts.render_suppressed());
    }

    #[test]
    fn suppression_expires_after_timeout() {
        let mut ts = make_ts();
        ts.process_output(&cup(5, 0));
        ts.notify_rendered();
        ts.process_output(&cup(2, 0)); // trigger suppression
        assert!(ts.render_suppressed());
        thread::sleep(Duration::from_millis(35));
        assert!(!ts.render_suppressed()); // deadline passed
    }

    #[test]
    fn no_suppression_when_cursor_stays_at_or_below_settled() {
        let mut ts = make_ts();
        ts.process_output(&cup(3, 0));
        ts.notify_rendered(); // settled = 3
        ts.process_output(&cup(3, 0)); // same row
        assert!(!ts.render_suppressed());
        ts.process_output(&cup(5, 0)); // below settled
        assert!(!ts.render_suppressed());
    }
}
