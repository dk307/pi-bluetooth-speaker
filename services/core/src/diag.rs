//! Shared CLI diagnostic and test utilities for hardware inspection binaries.
//!
//! Provides common argument parsers (`--count`, `--interval`), output format
//! selection, UTC wall-clock timestamp formatting, exit-code evaluation, and
//! scoped temporary directories shared across `read_temp` and `read_light`.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Foot-candles per lux conversion multiplier (1 lx ≈ 0.092903 fc).
pub const LUX_TO_FOOT_CANDLES: f32 = 0.092_903;

/// Output formatting mode for CLI diagnostic utilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Human-readable console output with headers and summaries.
    #[default]
    Human,
    /// Newline-delimited JSON (`NDJSON`) telemetry stream.
    Json,
}

/// Common CLI polling and formatting options shared across `read_*` diagnostic utilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonCliOptions {
    /// Maximum number of polling iterations (`None` for continuous polling).
    pub max_count: Option<usize>,
    /// Delay between polling iterations.
    pub interval: Duration,
    /// Output formatting mode (human console vs NDJSON stream).
    pub format: OutputFormat,
    /// Whether extended hardware register diagnostics are enabled.
    pub verbose: bool,
}

impl Default for CommonCliOptions {
    fn default() -> Self {
        Self::with_default_interval(Duration::from_secs(1))
    }
}

impl CommonCliOptions {
    /// Creates common CLI options with the specified default polling interval.
    #[must_use]
    pub const fn with_default_interval(interval: Duration) -> Self {
        Self {
            max_count: None,
            interval,
            format: OutputFormat::Human,
            verbose: false,
        }
    }

    /// Attempts to parse a shared CLI flag (`-h`/`--help`, `-1`/`--once`, `-v`/`--verbose`,
    /// `-j`/`--json`, `-n`/`--count`, `-i`/`--interval`).
    ///
    /// # Errors
    /// Returns a descriptive user-facing error string if a recognized flag is missing its
    /// value or fails validation. Returns `Ok(None)` when `-h` or `--help` is requested,
    /// `Ok(Some(true))` when `arg` was consumed, or `Ok(Some(false))` when `arg` is a
    /// binary-specific flag.
    pub fn parse_common_flag<I: Iterator<Item = String> + ?Sized>(
        &mut self,
        arg: &str,
        args: &mut I,
    ) -> Result<Option<bool>, String> {
        match arg {
            "-h" | "--help" => return Ok(None),
            "-1" | "--once" => self.max_count = Some(1),
            "-v" | "--verbose" => self.verbose = true,
            "-j" | "--json" => self.format = OutputFormat::Json,
            "-n" | "--count" => self.max_count = Some(parse_count_arg(args.next())?),
            "-i" | "--interval" => self.interval = parse_interval_arg(args.next())?,
            _ => return Ok(Some(false)),
        }
        Ok(Some(true))
    }
}

/// Parses command-line arguments using shared common flags (`-h`, `-1`, `-v`, `-j`, `-n`, `-i`)
/// and a domain-specific custom flag parser callback.
///
/// # Errors
/// Returns `Err(msg)` if any common or custom flag validation fails, or if an unknown option is provided.
/// Returns `Ok(None)` if `-h` or `--help` was requested.
pub fn parse_args_with_common<O, I, F>(
    mut opts: O,
    args: I,
    mut get_common: impl FnMut(&mut O) -> &mut CommonCliOptions,
    mut parse_custom: F,
) -> Result<Option<O>, String>
where
    I: IntoIterator<Item = String>,
    F: FnMut(&mut O, &str, &mut I::IntoIter) -> Result<bool, String>,
{
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match get_common(&mut opts).parse_common_flag(&arg, &mut iter)? {
            None => return Ok(None),
            Some(true) => continue,
            Some(false) => {}
        }
        if !parse_custom(&mut opts, &arg, &mut iter)? {
            return Err(format!("unknown option '{arg}', use --help for usage"));
        }
    }
    Ok(Some(opts))
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

/// Converts degrees Celsius to degrees Fahrenheit (`°C * 9 / 5 + 32`).
#[must_use]
pub const fn fahrenheit(celsius: f32) -> f32 {
    (celsius * 9.0 / 5.0) + 32.0
}

/// Converts illuminance in lux (`f32`) to foot-candles (`lx * LUX_TO_FOOT_CANDLES`).
#[must_use]
pub const fn foot_candles(lux: f32) -> f32 {
    lux * LUX_TO_FOOT_CANDLES
}

/// Rounds a `f32` measurement to 2 decimal places as `f64` for JSON telemetry serialization.
#[must_use]
pub fn round_2dp(value: f32) -> f64 {
    (f64::from(value) * 100.0).round() / 100.0
}

/// Evaluates a CLI option parser result, printing usage on `Ok(None)` or an error on `Err`.
///
/// # Errors
/// Returns `Err(ExitCode::SUCCESS)` when `--help` was requested (`Ok(None)`), or
/// `Err(ExitCode::FAILURE)` when argument validation failed (`Err(msg)`).
pub fn resolve_cli_opts<T>(
    parsed: Result<Option<T>, String>,
    default_bin_name: &str,
    print_usage: fn(&str),
) -> Result<T, ExitCode> {
    match parsed {
        Ok(Some(opts)) => Ok(opts),
        Ok(None) => {
            print_usage(&program_name(default_bin_name));
            Err(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("Error: {err}\nRun with --help for usage information.");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Extracts the next argument value for `flag`, or returns a `"missing value for {flag}"` error.
///
/// # Errors
/// Returns a descriptive user-facing error string if `args` yields `None`.
pub fn next_arg_value<I: Iterator<Item = String> + ?Sized>(
    args: &mut I,
    flag: &str,
) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

/// Parses and validates a `--count <COUNT>` argument value (must be `>= 1`).
///
/// # Errors
/// Returns a descriptive user-facing error string if `val` is missing, non-numeric, or `0`.
pub fn parse_count_arg(val: Option<String>) -> Result<usize, String> {
    let val = val.ok_or_else(|| "missing value for --count".to_string())?;
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
    let val = val.ok_or_else(|| "missing value for --interval".to_string())?;
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
    ///
    /// # Panics
    /// Panics if `value` is not finite (`NaN` or infinite), preventing silent
    /// corruption of running averages and bounds.
    pub fn record_success(&mut self, value: f32) {
        assert!(
            value.is_finite(),
            "numeric sample must be finite, got {value}"
        );
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

    /// Returns the arithmetic mean of all successful samples as `f32`, if any read succeeded.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "sensor sample domain is f32; average fits in f32"
    )]
    pub fn average_f32(&self) -> Option<f32> {
        self.average().map(|avg| avg as f32)
    }

    /// Returns the session error percentage `(errors() / total_reads) * 100.0`, or `0.0` if `total_reads == 0`.
    #[must_use]
    pub fn error_rate_percent(&self) -> f32 {
        self.fraction_of_reads_percent(self.errors())
    }

    /// Returns the fraction `(count / total_reads) * 100.0`, or `0.0` if `total_reads == 0`.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "read counts are small; f32 precision is exact for percentages"
    )]
    pub fn fraction_of_reads_percent(&self, count: usize) -> f32 {
        if self.total_reads == 0 {
            0.0
        } else {
            (count as f32 / self.total_reads as f32) * 100.0
        }
    }
}

/// Secondary unit converter function and unit label (e.g. `(foot_candles, "fc")`).
pub type SecondaryUnit = (fn(f32) -> f32, &'static str);

/// Builds formatted min, max, and average summary lines for a numeric sample set.
///
/// Returns `None` if no successful samples have been recorded. Pads label and stat keys
/// to 11 characters to align vertically with standard session summary headers, and formats
/// values with fixed column widths (`{val:7.2}`) for consistent table layout.
#[must_use]
pub fn format_min_max_avg(
    stats: &NumericStats,
    label: &str,
    unit: &str,
    to_secondary: Option<SecondaryUnit>,
) -> Option<[String; 3]> {
    let (min, max, avg) = (stats.min()?, stats.max()?, stats.average_f32()?);
    let entries = [("Min", min), ("Max", max), ("Avg", avg)];
    Some(entries.map(|(stat_name, val)| {
        let key = format!("{label} {stat_name}");
        match to_secondary {
            Some((conv, sec_unit)) => format!(
                "{key:<11} : {val:7.2} {unit} ({:6.2} {sec_unit})",
                conv(val)
            ),
            None => format!("{key:<11} : {val:7.2} {unit}"),
        }
    }))
}

/// Prints min, max, and average bounds for a numeric sample set.
///
/// If a secondary conversion function and unit are provided, prints secondary values in parentheses.
pub fn print_min_max_avg(
    stats: &NumericStats,
    label: &str,
    unit: &str,
    to_secondary: Option<SecondaryUnit>,
) {
    if let Some(lines) = format_min_max_avg(stats, label, unit, to_secondary) {
        for line in lines {
            println!("{line}");
        }
    }
}

/// Executes a periodic polling loop with interval regulation and cumulative termination.
///
/// In each iteration, `step` is called with the formatted timestamp string and returns `true`
/// if polling should terminate (e.g. cumulative count reached). If `step` returns `true`, the loop
/// terminates immediately without sleeping; otherwise, the thread sleeps for the remainder of
/// `interval` before executing the next iteration.
pub fn run_poll_loop(interval: Duration, mut step: impl FnMut(&str) -> bool) {
    loop {
        let loop_start = Instant::now();
        let timestamp = format_timestamp();
        if step(&timestamp) {
            break;
        }

        let elapsed = loop_start.elapsed();
        if let Some(remaining) = interval.checked_sub(elapsed) {
            std::thread::sleep(remaining);
        }
    }
}

/// RAII temporary directory that automatically removes itself and its contents on drop.
///
/// Exported unconditionally alongside [`TempSensor::new_mocked`](crate::hardware::TempSensor::new_mocked)
/// and [`LightSensor::new_mocked`](crate::hardware::LightSensor::new_mocked) per the repository
/// `M-TEST-UTIL` convention so unit tests, binary tests, and integration tests share a single
/// hermetic filesystem guard without requiring feature flags.
#[derive(Debug)]
pub struct ScopedTempDir {
    path: PathBuf,
}

impl ScopedTempDir {
    /// Attempts to create a new uniquely-named temporary directory under [`env::temp_dir()`].
    ///
    /// # Errors
    /// Returns [`io::Error`] if the temporary directory cannot be created on the host filesystem.
    pub fn try_new(prefix: &str) -> io::Result<Self> {
        let id = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("{prefix}_{pid}_{id}"));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    /// Creates a new uniquely-named temporary directory under [`env::temp_dir()`].
    ///
    /// # Panics
    /// Panics if the temporary directory cannot be created on the host filesystem.
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        Self::try_new(prefix).expect("failed to create temporary test directory")
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
        fn noop_usage(_: &str) {}

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
            let temp = ScopedTempDir::try_new("diag_scoped_test").unwrap();
            assert!(temp.path().is_dir());
            temp.path().to_path_buf()
        };
        assert!(!dir_path.exists());

        let mut stats = NumericStats::default();
        assert_eq!(stats.total_reads(), 0);
        assert_eq!(stats.error_rate_percent(), 0.0);
        assert_eq!(stats.fraction_of_reads_percent(0), 0.0);
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
        assert!((stats.average_f32().unwrap() - 20.0).abs() < 1e-6);
        assert!((stats.error_rate_percent() - (100.0 / 3.0)).abs() < 1e-4);
        assert!((stats.fraction_of_reads_percent(1) - (100.0 / 3.0)).abs() < 1e-4);

        assert!((fahrenheit(0.0) - 32.0).abs() < 1e-5);
        assert!((fahrenheit(100.0) - 212.0).abs() < 1e-4);
        assert!((round_2dp(12.3456) - 12.35).abs() < 1e-6);

        let mut args = ["val".to_string()].into_iter();
        assert_eq!(next_arg_value(&mut args, "--flag").unwrap(), "val");
        assert!(
            next_arg_value(&mut args, "--flag")
                .unwrap_err()
                .contains("missing value for --flag")
        );

        assert_eq!(
            resolve_cli_opts(Ok(Some(42)), "bin", noop_usage).unwrap(),
            42
        );
        assert_eq!(
            resolve_cli_opts::<i32>(Ok(None), "bin", noop_usage).unwrap_err(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            resolve_cli_opts::<i32>(Err("bad arg".into()), "bin", noop_usage).unwrap_err(),
            ExitCode::FAILURE
        );

        let mut common = CommonCliOptions::with_default_interval(Duration::from_secs(1));
        let mut empty = std::iter::empty::<String>();
        assert_eq!(
            common.parse_common_flag("--help", &mut empty).unwrap(),
            None
        );
        assert_eq!(
            common.parse_common_flag("--once", &mut empty).unwrap(),
            Some(true)
        );
        assert_eq!(common.max_count, Some(1));
        assert_eq!(
            common.parse_common_flag("--verbose", &mut empty).unwrap(),
            Some(true)
        );
        assert!(common.verbose);
        assert_eq!(
            common.parse_common_flag("--json", &mut empty).unwrap(),
            Some(true)
        );
        assert_eq!(common.format, OutputFormat::Json);
        assert_eq!(
            common.parse_common_flag("--custom", &mut empty).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn parse_args_with_common_helper_tests() {
        #[derive(Default, PartialEq, Debug)]
        struct TestOpts {
            common: CommonCliOptions,
            flag: bool,
        }

        let parsed = parse_args_with_common(
            TestOpts::default(),
            ["--once".into(), "--flag".into()],
            |o| &mut o.common,
            |opts, arg, _args| match arg {
                "--flag" => {
                    opts.flag = true;
                    Ok(true)
                }
                _ => Ok(false),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.common.max_count, Some(1));
        assert!(parsed.flag);

        let err = parse_args_with_common(
            TestOpts::default(),
            ["--unknown".into()],
            |o| &mut o.common,
            |_opts, _arg, _args| Ok(false),
        )
        .unwrap_err();
        assert!(err.contains("unknown option '--unknown'"));

        let help = parse_args_with_common(
            TestOpts::default(),
            ["--help".into()],
            |o| &mut o.common,
            |_opts, _arg, _args| Ok(false),
        )
        .unwrap();
        assert!(help.is_none());
    }

    #[test]
    #[should_panic(expected = "numeric sample must be finite")]
    fn numeric_stats_record_success_panics_on_nan() {
        let mut stats = NumericStats::default();
        stats.record_success(f32::NAN);
    }

    #[test]
    #[should_panic(expected = "numeric sample must be finite")]
    fn numeric_stats_record_success_panics_on_infinity() {
        let mut stats = NumericStats::default();
        stats.record_success(f32::INFINITY);
    }

    #[test]
    fn run_poll_loop_terminates_on_is_done() {
        let mut count = 0;
        run_poll_loop(Duration::from_millis(1), |_timestamp| {
            count += 1;
            count >= 3
        });
        assert_eq!(count, 3);
    }

    #[test]
    fn format_min_max_avg_formatting_and_bounds() {
        let mut stats = NumericStats::default();
        stats.record_success(10.0);
        stats.record_success(30.0);
        let formatted_light =
            format_min_max_avg(&stats, "Light", "lx", Some((foot_candles, "fc"))).unwrap();
        assert_eq!(formatted_light[0], "Light Min   :   10.00 lx (  0.93 fc)");
        assert_eq!(formatted_light[1], "Light Max   :   30.00 lx (  2.79 fc)");
        assert_eq!(formatted_light[2], "Light Avg   :   20.00 lx (  1.86 fc)");

        let formatted_temp =
            format_min_max_avg(&stats, "Chassis", "°C", Some((fahrenheit, "°F"))).unwrap();
        assert_eq!(formatted_temp[0], "Chassis Min :   10.00 °C ( 50.00 °F)");
        assert_eq!(formatted_temp[1], "Chassis Max :   30.00 °C ( 86.00 °F)");
        assert_eq!(formatted_temp[2], "Chassis Avg :   20.00 °C ( 68.00 °F)");

        let formatted_plain = format_min_max_avg(&stats, "Chassis", "°C", None).unwrap();
        assert_eq!(formatted_plain[0], "Chassis Min :   10.00 °C");
        assert_eq!(formatted_plain[1], "Chassis Max :   30.00 °C");
        assert_eq!(formatted_plain[2], "Chassis Avg :   20.00 °C");

        let empty_stats = NumericStats::default();
        assert_eq!(format_min_max_avg(&empty_stats, "Test", "lx", None), None);

        // Exercise print_min_max_avg to cover stdout rendering for both Some and None paths
        print_min_max_avg(&stats, "Light", "lx", Some((foot_candles, "fc")));
        print_min_max_avg(&empty_stats, "Empty", "lx", None);
    }
}
