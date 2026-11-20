//! Shared tracing capture for hermetic autobright integration tests.
//!
//! Several `run_tick` / `run_with` branches only evaluate their structured
//! log arguments when an INFO subscriber is installed. Installing one real
//! subscriber (once per test binary) lets those telemetry paths be asserted
//! instead of silently compiling them away at runtime.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

/// Thread-safe in-memory sink shared by the test subscriber and assertions.
#[derive(Debug, Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// Installs the process-wide INFO subscriber and returns its shared buffer.
    ///
    /// # Panics
    /// Panics if a global tracing subscriber was already installed by another
    /// crate; autobright's integration tests never do so.
    #[must_use]
    pub fn install() -> &'static Self {
        static CAPTURE: OnceLock<LogCapture> = OnceLock::new();
        CAPTURE.get_or_init(|| {
            let capture = Self::default();
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_writer(capture.clone())
                .try_init()
                .expect("install one test tracing subscriber");
            capture
        })
    }

    /// Returns `true` when any captured log line contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        let bytes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).contains(needle)
    }

    /// Returns the full captured log text for diagnostic assertion messages.
    #[must_use]
    pub fn snapshot(&self) -> String {
        let bytes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
