use std::io::{self, IsTerminal as _, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::log_buffer::{LogBuffer, LogBufferLayer};

pub const LOG_PATH: &str = "/tmp/agent-transcript.log";

/// Shared handle for toggling debug-level file logging at runtime.
#[derive(Clone, Default)]
pub struct DebugHandle {
    enabled: Arc<AtomicBool>,
    file: Arc<Mutex<Option<std::fs::File>>>,
}

impl DebugHandle {
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Open the log file and start writing. No-op if already enabled.
    pub fn enable(&self) -> io::Result<()> {
        let mut guard = self.file.lock().unwrap();
        if guard.is_none() {
            *guard = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(LOG_PATH)?,
            );
            self.enabled.store(true, Ordering::Release);
        }
        Ok(())
    }
}

pub struct ToggleWriter(Arc<Mutex<Option<std::fs::File>>>);

impl Write for ToggleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock().unwrap().as_mut() {
            Some(f) => f.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.lock().unwrap().as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for DebugHandle {
    type Writer = ToggleWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ToggleWriter(Arc::clone(&self.file))
    }
}

/// Per-event filter backed by an `AtomicBool`. Returns `Interest::sometimes()` so
/// the flag change takes effect without restarting the process.
struct ToggleFilter(Arc<AtomicBool>);

impl<S: tracing::Subscriber> tracing_subscriber::layer::Filter<S> for ToggleFilter {
    fn enabled(
        &self,
        meta: &tracing::Metadata<'_>,
        _: &tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        self.0.load(Ordering::Relaxed)
            && *meta.level() <= tracing::Level::DEBUG
            && !meta.target().starts_with("tui_markdown")
    }

    fn callsite_enabled(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // Never cache: the AtomicBool can flip at any time.
        tracing::subscriber::Interest::sometimes()
    }
}

pub fn init_tracing(
    debug: bool,
    log_buffer: LogBuffer,
    log_to_stderr: bool,
) -> color_eyre::Result<DebugHandle> {
    let handle = DebugHandle::default();

    if log_to_stderr {
        // Parse mode: no TUI, so stderr is available. Write directly there.
        handle
            .enabled
            .store(debug, std::sync::atomic::Ordering::Release);
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(std::io::stderr().is_terminal())
                    .with_filter(ToggleFilter(Arc::clone(&handle.enabled))),
            )
            .with(LogBufferLayer(log_buffer))
            .init();
    } else {
        if debug {
            handle
                .enable()
                .map_err(|e| color_eyre::eyre::eyre!("failed to open log file: {e}"))?;
        }
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(handle.clone())
                    .with_ansi(false)
                    .with_filter(ToggleFilter(Arc::clone(&handle.enabled))),
            )
            .with(LogBufferLayer(log_buffer))
            .init();
    }

    Ok(handle)
}
