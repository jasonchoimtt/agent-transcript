use color_eyre::eyre::OptionExt;
use crossterm::event::Event as CrosstermEvent;
use futures::{FutureExt, StreamExt};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::providers::TranscriptEntry;
use crate::reader_op::ReaderOp;

/// The frequency at which tick events are emitted.
const TICK_FPS: f64 = 30.0;

/// Representation of all possible events.
pub enum Event {
    /// An event that is emitted on a regular schedule.
    Tick,
    /// Crossterm events.
    Crossterm(CrosstermEvent),
    /// Application events.
    App(AppEvent),
    /// A reader operation from the active TranscriptReader (tree mutations and reset lifecycle).
    ReaderOp(ReaderOp),
}

/// Application events.
#[derive(Debug)]
pub enum AppEvent {
    /// Raw bytes received from the embedded PTY.
    TerminalOutput(Vec<u8>),
    /// The embedded PTY process has exited.
    /// `terminal_id` identifies which terminal spawned this event; stale events
    /// from a killed terminal are ignored if the ID no longer matches the app's
    /// current terminal.
    TerminalExited { terminal_id: u64, code: Option<i32> },
    /// A session was detected via the Unix socket hook.  Fired once per
    /// `/new` or `/resume` inside a live Claude session, and once on Cursor
    /// session start.
    SessionDetected {
        session_id: String,
        transcript_path: Option<PathBuf>,
        /// First workspace root reported by the hook (Cursor only).
        workspace_path: Option<PathBuf>,
    },
    /// A batch of transcript entries from the background picker refresh task.
    PickerEntries { entries: Vec<TranscriptEntry> },
    /// The background picker refresh task has finished.
    PickerDone,
    /// The transcript reader encountered a terminal error.
    ReaderError(String),
}

/// Terminal event handler.
pub struct EventHandler {
    /// Event sender channel.
    sender: mpsc::UnboundedSender<Event>,
    /// Event receiver channel.
    receiver: mpsc::UnboundedReceiver<Event>,
    /// Events pushed back via `unget` — drained before the channel.
    lookahead: VecDeque<Event>,
}

impl std::fmt::Debug for EventHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHandler")
            .field("lookahead_len", &self.lookahead.len())
            .finish_non_exhaustive()
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHandler {
    /// Constructs a new instance of [`EventHandler`] and spawns a new thread to handle events.
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let actor = EventTask::new(sender.clone());
        tokio::spawn(async { actor.run().await });
        Self {
            sender,
            receiver,
            lookahead: VecDeque::new(),
        }
    }

    /// Receives an event from the sender.
    ///
    /// This function blocks until an event is received.
    ///
    /// # Errors
    ///
    /// This function returns an error if the sender channel is disconnected. This can happen if an
    /// error occurs in the event thread. In practice, this should not happen unless there is a
    /// problem with the underlying terminal.
    pub async fn next(&mut self) -> color_eyre::Result<Event> {
        if let Some(event) = self.lookahead.pop_front() {
            return Ok(event);
        }
        self.receiver
            .recv()
            .await
            .ok_or_eyre("Failed to receive event")
    }

    /// Returns a cloned sender for use by background tasks (e.g. PTY reader).
    pub fn sender(&self) -> mpsc::UnboundedSender<Event> {
        self.sender.clone()
    }

    /// Non-blocking receive — returns the next event if one is immediately available.
    /// Drains the lookahead buffer (populated by `unget`) before the channel.
    pub fn try_recv(&mut self) -> Option<Event> {
        if let Some(event) = self.lookahead.pop_front() {
            return Some(event);
        }
        self.receiver.try_recv().ok()
    }

    /// Push an event back to the front of the queue so the next `try_recv` or
    /// `next` call returns it first.  Use this when a `try_recv` loop receives
    /// an event that doesn't match the batch it is draining.
    pub fn unget(&mut self, event: Event) {
        self.lookahead.push_front(event);
    }
}

/// A thread that handles reading crossterm events and emitting tick events on a regular schedule.
struct EventTask {
    /// Event sender channel.
    sender: mpsc::UnboundedSender<Event>,
}

impl EventTask {
    /// Constructs a new instance of [`EventThread`].
    fn new(sender: mpsc::UnboundedSender<Event>) -> Self {
        Self { sender }
    }

    /// Runs the event thread.
    ///
    /// This function emits tick events at a fixed rate and polls for crossterm events in between.
    async fn run(self) -> color_eyre::Result<()> {
        let tick_rate = Duration::from_secs_f64(1.0 / TICK_FPS);
        let mut reader = crossterm::event::EventStream::new();
        let mut tick = tokio::time::interval(tick_rate);
        loop {
            let tick_delay = tick.tick();
            let crossterm_event = reader.next().fuse();
            tokio::select! {
              _ = self.sender.closed() => {
                break;
              }
              _ = tick_delay => {
                self.send(Event::Tick);
              }
              Some(Ok(evt)) = crossterm_event => {
                self.send(Event::Crossterm(evt));
              }
            };
        }
        Ok(())
    }

    /// Sends an event to the receiver.
    fn send(&self, event: Event) {
        // Ignores the result because shutting down the app drops the receiver, which causes the send
        // operation to fail. This is expected behavior and should not panic.
        let _ = self.sender.send(event);
    }
}
