//! Shared CLI diagnostic utilities for standalone hardware inspection binaries.
//!
//! Provides common argument parsers (`--count`, `--interval`), output format
//! selection, UTC wall-clock timestamp formatting, and exit-code evaluation
//! shared across `read_temp`, `read_light`, and future appliance diagnostic tools.

use std::env;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Output formatting mode for CLI diagnostic utilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Human-readable console output with headers and summaries.
    #[default]
    Human,
    /// Newline-delimited JSON (`NDJSON`) telemetry stream.
    Json,
}

/// Formats the current UTC wall-clock time as `HH:MM:SS`.
#[must_use]
pub fn format_timestamp() -> String {
    let total_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let hours = (total_secs / 3600) % 24;
    let minutes = (total_secs / 60) % 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Returns command-line arguments (excluding argv\[0\]) converted lossily to UTF-8.
pub fn env_args() -> impl Iterator<Item = String> {
    env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
}

/// Returns the invoked program name from `argv[0]`, or `default_name` if unavailable.
#[must_use]
pub fn program_name(default_name: &str) -> String {
    env::args_os()
        .next()
        .map_or_else(|| default_name.into(), |a| a.to_string_lossy().into_owned())
}

/// Parses and validates a `--count <COUNT>` argument value (must be `>= 1`).
///
/// # Errors
/// Returns a descriptive user-facing error string if `val` is missing, non-numeric, or `0`.
pub fn parse_count_arg(val: Option<String>) -> Result<usize, String> {
    let Some(val) = val else {
        return Err("missing value for --count".into());
    };
    let count = val
        .parse::<usize>()
        .map_err(|e| format!("invalid count '{val}': {e}"))?;
    if count == 0 {
        return Err("--count must be at least 1".into());
    }
    Ok(count)
}

/// Parses and validates an `--interval <SECS>` argument value (must be finite and `> 0.0`).
///
/// # Errors
/// Returns a descriptive user-facing error string if `val` is missing, non-numeric, NaN, infinite, or `<= 0`.
pub fn parse_interval_arg(val: Option<String>) -> Result<Duration, String> {
    let Some(val) = val else {
        return Err("missing value for --interval".into());
    };
    let secs = val
        .parse::<f32>()
        .map_err(|e| format!("invalid interval '{val}': {e}"))?;
    Duration::try_from_secs_f32(secs)
        .ok()
        .filter(|d| !d.is_zero())
        .ok_or_else(|| format!("interval must be a finite value greater than 0, got '{val}'"))
}

/// Computes the process [`ExitCode`] from total and successful read counts.
///
/// Returns [`ExitCode::FAILURE`] if at least one read was attempted and all reads failed;
/// otherwise returns [`ExitCode::SUCCESS`].
#[must_use]
pub const fn exit_code_for_reads(total_reads: usize, successful_reads: usize) -> ExitCode {
    if total_reads > 0 && successful_reads == 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_timestamp_returns_valid_hh_mm_ss() {
        let ts = format_timestamp();
        assert_eq!(ts.len(), 8);
        assert_eq!(&ts[2..3], ":");
        assert_eq!(&ts[5..6], ":");
    }

    #[test]
    fn parse_count_and_interval_validate_bounds() {
        assert_eq!(parse_count_arg(Some("5".into())).unwrap(), 5);
        assert!(parse_count_arg(None).unwrap_err().contains("missing value"));
        assert!(
            parse_count_arg(Some("0".into()))
                .unwrap_err()
                .contains("at least 1")
        );
        assert!(
            parse_count_arg(Some("abc".into()))
                .unwrap_err()
                .contains("invalid count")
        );

        assert_eq!(
            parse_interval_arg(Some("0.5".into())).unwrap(),
            Duration::from_millis(500)
        );
        assert!(
            parse_interval_arg(None)
                .unwrap_err()
                .contains("missing value")
        );
        for bad in ["0", "-1.0", "nan", "inf", "1e400", "abc"] {
            assert!(
                parse_interval_arg(Some(bad.into())).is_err(),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn exit_code_and_arg_helpers() {
        assert_eq!(exit_code_for_reads(0, 0), ExitCode::SUCCESS);
        assert_eq!(exit_code_for_reads(3, 1), ExitCode::SUCCESS);
        assert_eq!(exit_code_for_reads(3, 0), ExitCode::FAILURE);
        assert_ne!(program_name("fallback"), "");
        let _ = env_args().count();
    }
}
