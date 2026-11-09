//! Global [`tracing`] subscriber setup for the appliance daemon.
//!
//! Logging follows the Linux systemd daemon convention: the service writes
//! structured `tracing` events to stderr and journald captures them via
//! `StandardError=journal`, so the daemon never manages log files, rotation,
//! or retention itself.
//!
//! Verbosity is controlled by the `RUST_LOG` environment variable using
//! [`EnvFilter`] directive syntax (for example
//! `RUST_LOG=info,speaker_bluetooth=debug`). When `RUST_LOG`
//! is unset or cannot be parsed, the filter falls back to
//! [`DEFAULT_LOG_FILTER`] so a malformed value can never silence the daemon
//! entirely.
//!
//! # Examples
//! ```
//! // One-off terminal debugging session:
//! //   RUST_LOG=debug ./core-daemon
//! //
//! // Persistent override under systemd:
//! //   systemctl edit core   # [Service] Environment=RUST_LOG=debug
//! ```

use std::io::IsTerminal;
use std::sync::Once;
use tracing_subscriber::EnvFilter;

/// Default verbosity applied when `RUST_LOG` is unset or cannot be parsed.
pub const DEFAULT_LOG_FILTER: &str = "info";

static LOGGING_INIT: Once = Once::new();

/// Builds the daemon log filter from the `RUST_LOG` environment variable.
///
/// Falls back to [`DEFAULT_LOG_FILTER`] when `RUST_LOG` is unset or contains
/// invalid directives, guaranteeing the daemon always has a usable verbosity
/// level.
///
/// # Examples
/// ```
/// use speaker_common::logging::daemon_log_filter;
/// use tracing_subscriber::EnvFilter;
///
/// // Unset environment produces the info-level fallback.
/// unsafe { std::env::remove_var("RUST_LOG"); }
/// let filter = daemon_log_filter();
/// assert!(filter.to_string().contains("info"));
/// ```
///
/// # Panics
/// Panics only if the fallback filter string itself is invalid, which the
/// compile-time [`DEFAULT_LOG_FILTER`] constant makes impossible.
#[must_use]
pub fn daemon_log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER))
}

/// Installs the global `tracing` subscriber for the daemon process.
///
/// Events are written to stderr in journald-friendly single-line format, with
/// ANSI colors enabled only when stderr is an interactive terminal so journal
/// storage never accumulates escape codes. Level control comes from
/// [`daemon_log_filter`] (`RUST_LOG`, defaulting to `info`).
///
/// Installation is guarded by a [`Once`], so repeated calls (for example
/// across unit-test harnesses or CLI re-entry) are safe and only the first
/// call configures logging.
///
/// # Examples
/// ```
/// use speaker_common::logging::init_daemon_logging;
///
/// init_daemon_logging();
/// tracing::info!("daemon logging is live");
/// ```
pub fn init_daemon_logging() {
    LOGGING_INIT.call_once(|| {
        let ansi = std::io::stderr().is_terminal();
        tracing_subscriber::fmt()
            .with_env_filter(daemon_log_filter())
            .with_ansi(ansi)
            .with_writer(std::io::stderr)
            .init();
    });
}
