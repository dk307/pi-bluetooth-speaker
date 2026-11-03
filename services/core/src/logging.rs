//! Global [`tracing`] subscriber setup for the appliance daemon.
//!
//! Logging follows the Linux systemd daemon convention: the service writes
//! structured `tracing` events to stderr and journald captures them via
//! `StandardError=journal`, so the daemon never manages log files, rotation,
//! or retention itself.
//!
//! Verbosity is controlled by the `RUST_LOG` environment variable using
//! [`EnvFilter`] directive syntax (for example
//! `RUST_LOG=info,speaker_core::hardware::bluetooth=debug`). When `RUST_LOG`
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
/// use speaker_core::logging::daemon_log_filter;
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
/// use speaker_core::logging::init_daemon_logging;
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

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LOG_FILTER, daemon_log_filter, init_daemon_logging};
    use std::env;
    use std::sync::Mutex;

    /// Serializes environment-variable mutation across parallel tests;
    /// `RUST_LOG` is process-global state that the filter reads eagerly.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_rust_log(value: Option<&str>) {
        match value {
            // SAFETY: single-threaded mutation of process-wide env state,
            // serialized by ENV_LOCK so no concurrent reader can observe a
            // torn value.
            Some(v) => unsafe { env::set_var("RUST_LOG", v) },
            // SAFETY: same serialized mutation contract as `set_var` above.
            None => unsafe { env::remove_var("RUST_LOG") },
        }
    }

    #[test]
    fn unset_rust_log_falls_back_to_info() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_rust_log(None);
        assert_eq!(daemon_log_filter().to_string(), DEFAULT_LOG_FILTER);
        set_rust_log(None);
    }

    #[test]
    fn valid_rust_log_directive_is_honored() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_rust_log(Some("speaker_core::hardware=debug"));
        let filter = daemon_log_filter().to_string();
        assert!(
            filter.contains("speaker_core::hardware=debug"),
            "filter must honor the RUST_LOG directive, got: {filter}"
        );
        set_rust_log(None);
    }

    #[test]
    fn invalid_rust_log_falls_back_to_info() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_rust_log(Some("not-a-valid=level=bogus"));
        assert_eq!(daemon_log_filter().to_string(), DEFAULT_LOG_FILTER);
        set_rust_log(None);
    }

    #[test]
    fn init_daemon_logging_is_reentrant_safe() {
        init_daemon_logging();
        // A second call must be a silent no-op (Once-guarded), never panic.
        init_daemon_logging();
    }
}
