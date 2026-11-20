//! Backlight sysfs diagnostic CLI application logic.
//!
//! Reports the raw and maximum backlight values, the derived brightness
//! percentage, and the optional kernel `bl_power` state for the configured
//! backlight device, in either human-readable or NDJSON format.

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use speaker_common::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts, round_2dp,
    run_poll_loop, should_print_summary,
};
use speaker_hardware::{BACKLIGHT_SYSFS_BASE, Backlight, BacklightError, DEFAULT_BACKLIGHT_DEVICE};

/// Default polling interval for the essentially static backlight telemetry.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Command-line configuration parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    /// Shared common CLI options (timing, formatting, count).
    pub common: CommonCliOptions,
    /// Kernel backlight device directory name under the sysfs root.
    pub device: Box<str>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            device: DEFAULT_BACKLIGHT_DEVICE.into(),
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
    println!("Backlight Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit (default)");
    println!("  -R, --repeat             Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>      Read COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 0.5)");
    println!("  -d, --device <NAME>      Backlight device name (default: rpi_backlight)");
    println!("  -v, --verbose            Display extended diagnostics (human format only)");
    println!("  -j, --json               Stream readings in newline-delimited JSON");
    println!("  -V, --version            Print semantic version and exit");
    println!("  -h, --help               Display this help text");
}

/// Parses CLI arguments from an arbitrary iterator into [`CliParse<CliOptions>`].
///
/// # Errors
///
/// Returns a descriptive error string if an argument is missing or the device
/// name is empty or contains a path separator.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    parse_args_with_common(
        CliOptions::default(),
        args,
        |o| &mut o.common,
        |opts, arg, args| match arg {
            "-d" | "--device" => {
                let value = next_arg_value(args, "--device")?;
                if value.is_empty() || value.contains('/') || value.contains('\\') {
                    return Err(format!("invalid backlight device name '{value}'"));
                }
                opts.device = value.into_boxed_str();
                Ok(true)
            }
            _ => Ok(false),
        },
    )
}

/// Parses command-line arguments from [`std::env::args`].
///
/// # Errors
///
/// Returns a descriptive error string if argument parsing fails.
pub fn parse_cli_args() -> Result<CliParse<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

/// Reads the optional kernel `bl_power` state for a device.
#[must_use]
pub fn read_bl_power(device: &str) -> Option<u8> {
    let path = Path::new(BACKLIGHT_SYSFS_BASE)
        .join(device)
        .join("bl_power");
    fs::read_to_string(path).ok()?.trim().parse::<u8>().ok()
}

/// Prints the device banner for a human-readable session.
pub fn print_header(backlight: &Backlight, device: &str) {
    println!("==================================================");
    println!("        LINUX BACKLIGHT MONITOR                   ");
    println!("==================================================");
    println!("  • Device       : {device}");
    println!("  • Sysfs Base   : {BACKLIGHT_SYSFS_BASE}");
    println!("  • Max Raw      : {}", backlight.max_brightness());
    if let Some(power) = read_bl_power(device) {
        println!("  • bl_power     : {power}");
    }
    println!("--------------------------------------------------");
}

/// Formats one successful reading as a newline-delimited JSON record.
pub fn output_json_reading(timestamp: &str, device: &str, raw: u32, max: u32, power: Option<u8>) {
    let percent = if max == 0 {
        0.0
    } else {
        (f64::from(raw) * 100.0) / f64::from(max)
    };
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "device": device,
        "raw": raw,
        "max": max,
        "percent": round_2dp(percent as f32),
        "bl_power": power,
    });
    println!("{obj}");
}

/// Formats one backlight failure as a single-line NDJSON error record.
pub fn output_json_error(timestamp: &str, err: &BacklightError) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "error": err.summary(),
    });
    println!("{obj}");
}

/// Processes a single backlight read according to the selected output format.
pub fn handle_backlight_iteration(
    backlight: &Backlight,
    opts: &CliOptions,
    timestamp: &str,
    stats: &mut NumericStats,
) {
    match backlight.read() {
        Ok(raw) => {
            stats.record_success(raw as f32);
            let max = backlight.max_brightness();
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_reading(
                        timestamp,
                        &opts.device,
                        raw,
                        max,
                        read_bl_power(&opts.device),
                    );
                }
                OutputFormat::Human => {
                    let percent = if max == 0 {
                        0.0
                    } else {
                        (f64::from(raw) * 100.0) / f64::from(max)
                    };
                    println!("[{timestamp}] raw={raw}/{max} ({percent:.2}%)");
                    if opts.common.verbose
                        && let Some(power) = read_bl_power(&opts.device)
                    {
                        println!("           bl_power={power}");
                    }
                }
            }
        }
        Err(err) => {
            stats.record_error();
            match opts.common.format {
                OutputFormat::Json => output_json_error(timestamp, &err),
                OutputFormat::Human => eprintln!("[{timestamp}] Read error: {err}"),
            }
        }
    }
}

/// Prints summary statistics for the completed monitoring session.
pub fn print_session_summary(stats: &NumericStats) {
    println!("\n--- Backlight Diagnostic Session Summary ---");
    println!("Total reads : {}", stats.total_reads());
    println!("Successful  : {}", stats.successful_reads());
    if stats.total_reads() > 0 {
        let errors = stats.errors();
        println!(
            "Read errors : {errors} ({:.2}% error rate)",
            stats.error_rate_percent()
        );
    }
}

/// Runs the complete diagnostic monitoring loop with the provided backlight and options.
#[must_use]
pub fn run_app(backlight: &Backlight, opts: &CliOptions) -> ExitCode {
    if opts.common.format == OutputFormat::Human {
        print_header(backlight, &opts.device);
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        } else {
            println!();
        }
    }

    let mut stats = NumericStats::default();
    run_poll_loop(opts.common.interval, |timestamp| {
        handle_backlight_iteration(backlight, opts, timestamp, &mut stats);
        opts.common
            .max_count
            .is_some_and(|max| stats.total_reads() >= max)
    });

    if should_print_summary(
        opts.common.format,
        opts.common.max_count,
        stats.total_reads(),
    ) {
        print_session_summary(&stats);
    }

    exit_code_for_reads(stats.total_reads(), stats.successful_reads())
}

/// Emits a backlight initialization failure in the selected output format.
fn report_init_error(err: &BacklightError, opts: &CliOptions) {
    if opts.common.format == OutputFormat::Json {
        output_json_error(&format_timestamp(), err);
    } else {
        eprintln!("Failed to open backlight device '{}': {err}", opts.device);
    }
}

/// Standalone CLI binary entrypoint for `backlight-info`.
#[must_use]
pub fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "backlight-info", print_usage) {
        Ok(opts) => opts,
        Err(code) => return code,
    };

    let backlight = match Backlight::with_device(&opts.device) {
        Ok(backlight) => backlight,
        Err(err) => {
            report_init_error(&err, &opts);
            return ExitCode::FAILURE;
        }
    };

    run_app(&backlight, &opts)
}
