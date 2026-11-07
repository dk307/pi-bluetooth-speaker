//! Integration tests for the daemon logging subscriber setup.

use speaker_common::logging::{DEFAULT_LOG_FILTER, daemon_log_filter, init_daemon_logging};
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
    set_rust_log(Some("speaker_common::hardware=debug"));
    let filter = daemon_log_filter().to_string();
    assert!(
        filter.contains("speaker_common::hardware=debug"),
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
