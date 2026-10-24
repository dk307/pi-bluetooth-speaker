//! Shared CLI diagnostic and test utilities for hardware inspection binaries.
//!
//! Provides common argument parsers (`--count`, `--interval`), output format
//! selection, UTC wall-clock timestamp formatting, exit-code evaluation, and
//! scoped temporary directories shared across `read_temp` and `read_light`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

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

/// Returns `true` when the human-readable session summary should be printed.
///
/// Suppresses the summary for JSON streams, single-shot invocations (`--once` / `--count 1`),
/// or sessions that performed fewer than 2 reads.
#[must_use]
pub const fn should_print_summary(
    format: OutputFormat,
    max_count: Option<usize>,
    total_reads: usize,
) -> bool {
    matches!(format, OutputFormat::Human) && !matches!(max_count, Some(1)) && total_reads > 1
}

/// Shared numeric telemetry accumulator for CLI diagnostic polling sessions.
#[derive(Debug, Default, Clone, Copy)]
pub struct NumericStats {
    total_reads: usize,
    successful_reads: usize,
    min: Option<f32>,
    max: Option<f32>,
    sum: f64,
}

impl NumericStats {
    /// Records a successful numeric sensor sample.
    pub fn record_success(&mut self, value: f32) {
        self.total_reads += 1;
        self.successful_reads += 1;
        self.sum += f64::from(value);
        self.min = Some(self.min.map_or(value, |cur| cur.min(value)));
        self.max = Some(self.max.map_or(value, |cur| cur.max(value)));
    }

    /// Records a failed sensor read attempt.
    pub fn record_error(&mut self) {
        self.total_reads += 1;
    }

    /// Returns the total number of attempted reads.
    #[must_use]
    pub const fn total_reads(&self) -> usize {
        self.total_reads
    }

    /// Returns the number of successful reads.
    #[must_use]
    pub const fn successful_reads(&self) -> usize {
        self.successful_reads
    }

    /// Returns the total number of failed reads (`total_reads - successful_reads`).
    #[must_use]
    pub const fn errors(&self) -> usize {
        self.total_reads - self.successful_reads
    }

    /// Returns the minimum observed sample value, if any read succeeded.
    #[must_use]
    pub const fn min(&self) -> Option<f32> {
        self.min
    }

    /// Returns the maximum observed sample value, if any read succeeded.
    #[must_use]
    pub const fn max(&self) -> Option<f32> {
        self.max
    }

    /// Returns the arithmetic mean of all successful samples, if any read succeeded.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "read counts are small; f64 precision is exact for integer counts"
    )]
    pub fn average(&self) -> Option<f64> {
        (self.successful_reads > 0).then(|| self.sum / self.successful_reads as f64)
    }

    /// Returns the percentage `(error_count / total_reads) * 100.0`, or `0.0` if `total_reads == 0`.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "read counts are small; f32 precision is exact for percentages"
    )]
    pub fn error_rate_percent(&self, error_count: usize) -> f32 {
        if self.total_reads == 0 {
            0.0
        } else {
            (error_count as f32 / self.total_reads as f32) * 100.0
        }
    }
}

/// RAII temporary directory that automatically removes itself and its contents on drop.
#[derive(Debug)]
pub struct ScopedTempDir {
    path: PathBuf,
}

impl ScopedTempDir {
    /// Creates a new uniquely-named temporary directory under [`env::temp_dir()`].
    ///
    /// # Panics
    /// Panics if the temporary directory cannot be created on the host filesystem.
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        let id = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("{prefix}_{pid}_{id}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create temporary test directory");
        Self { path }
    }

    /// Returns the filesystem path to the temporary directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
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
        assert!(should_print_summary(OutputFormat::Human, None, 2));
        assert!(should_print_summary(OutputFormat::Human, Some(2), 2));
        assert!(!should_print_summary(OutputFormat::Human, Some(1), 2));
        assert!(!should_print_summary(OutputFormat::Human, None, 1));
        assert!(!should_print_summary(OutputFormat::Json, None, 5));
        assert_ne!(program_name("fallback"), "");
        let _ = env_args().count();

        let dir_path = {
            let temp = ScopedTempDir::new("diag_scoped_test");
            assert!(temp.path().is_dir());
            temp.path().to_path_buf()
        };
        assert!(!dir_path.exists());

        let mut stats = NumericStats::default();
        assert_eq!(stats.total_reads(), 0);
        assert_eq!(stats.error_rate_percent(0), 0.0);
        assert!(stats.average().is_none());
        stats.record_success(10.0);
        stats.record_success(30.0);
        stats.record_error();
        assert_eq!(stats.total_reads(), 3);
        assert_eq!(stats.successful_reads(), 2);
        assert_eq!(stats.errors(), 1);
        assert_eq!(stats.min(), Some(10.0));
        assert_eq!(stats.max(), Some(30.0));
        assert!((stats.average().unwrap() - 20.0).abs() < 1e-6);
        assert!((stats.error_rate_percent(1) - (100.0 / 3.0)).abs() < 1e-4);
    }
}
