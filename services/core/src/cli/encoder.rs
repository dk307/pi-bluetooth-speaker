//! EC12 rotary encoder diagnostic utility.
//!
//! Provides CLI diagnostics for the quadrature encoder subsystem on the Raspberry
//! Pi 3 appliance. Displays hardware GPIO pin mappings, verifies the RGB indicator
//! channels, and continuously reports live rotation, switch, and LED state. Rotation
//! decoding is interrupt-driven (once per detent) and switch edges are debounced,
//! so polls only observe already-accumulated counters.

use std::process::ExitCode;
use std::time::Duration;

use crate::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, parse_args_with_common, resolve_cli_opts, run_poll_loop,
    should_print_summary,
};
use crate::hardware::{EncoderDevice, EncoderError, EncoderStatus, RgbColor, RotationDirection};

/// Default polling interval between encoder state samples.
///
/// Rotation and switch counters are maintained by dedicated interrupt threads, so
/// polls never miss transitions; the 10 ms interval only governs how quickly new
/// cumulative counts are observed and reported.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Dwell time each color is held during the `--verify-leds` channel sweep.
///
/// Without a dwell the entire eight-color sweep completes in microseconds and a
/// human observer cannot visually confirm any channel; the pause makes each
/// color observable before the sweep advances to the next one.
pub const LED_SWEEP_DWELL: Duration = Duration::from_millis(300);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliOptions {
    /// Common CLI options (timing, formatting, count).
    pub common: CommonCliOptions,
    /// Verify the RGB indicator channels with the baseline color sweep and exit.
    pub verify_leds: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            verify_leds: false,
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
    println!("EC12 Rotary Encoder Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Sample once and exit (default)");
    println!("  -R, --repeat             Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>      Sample COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 0.01)");
    println!("  -l, --verify-leds        Run the RGB channel color sweep and exit");
    println!(
        "  -v, --verbose            Display extended hardware diagnostics (human format only)"
    );
    println!("  -j, --json               Stream events in newline-delimited JSON");
    println!("  -V, --version            Print semantic version and exit");
    println!("  -h, --help               Display this help text");
}

/// Parses command-line arguments from an iterator of strings.
///
/// # Errors
///
/// Returns an error string if an argument or flag value is invalid.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    parse_args_with_common(
        CliOptions::default(),
        args,
        |o| &mut o.common,
        |opts, arg, _args| match arg {
            "-l" | "--verify-leds" => {
                opts.verify_leds = true;
                Ok(true)
            }
            _ => Ok(false),
        },
    )
}

/// Parses CLI arguments from environment `std::env::args()`.
///
/// # Errors
///
/// Returns an error string if an argument or flag value is invalid.
pub fn parse_cli_args() -> Result<CliParse<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

/// Prints the hardware metadata banner to stdout.
pub fn print_header(device: &EncoderDevice) {
    let hw = device.hardware_info();
    println!("==================================================");
    println!("       EC12 ROTARY ENCODER MONITOR                ");
    println!("==================================================");
    println!("Hardware GPIO Info:");
    println!("  • Encoder     : TOP-UP EC12 Quadrature + Push Switch");
    println!(
        "  • Phase A     : {} ({})",
        hw.phase_a.bcm_pin, hw.phase_a.physical_pin
    );
    println!(
        "  • Phase B     : {} ({})",
        hw.phase_b.bcm_pin, hw.phase_b.physical_pin
    );
    println!(
        "  • Push Switch : {} ({})",
        hw.push_switch.bcm_pin, hw.push_switch.physical_pin
    );
    println!(
        "  • LED R/G/B   : {} ({}) / {} ({}) / {} ({})",
        hw.led_red.bcm_pin,
        hw.led_red.physical_pin,
        hw.led_green.bcm_pin,
        hw.led_green.physical_pin,
        hw.led_blue.bcm_pin,
        hw.led_blue.physical_pin
    );
    println!("--------------------------------------------------");
}

/// Prints extended diagnostic status to stdout.
pub fn print_diagnostic_status(device: &EncoderDevice) {
    println!("\nExtended Hardware Diagnostics:");
    match device.read_status() {
        Ok(status) => println!("{status}"),
        Err(err) => eprintln!("  [!] Failed to read encoder status: {err}"),
    }
    println!("--------------------------------------------------");
}

/// Session statistics tracking poll outcomes and cumulative encoder events.
#[derive(Debug, Default)]
pub struct EncoderStats {
    /// Sample counter statistics.
    pub samples: NumericStats,
    /// Total CW transitions.
    pub cw_events: u64,
    /// Total CCW transitions.
    pub ccw_events: u64,
    /// Total switch press events.
    pub press_events: u64,
    /// Total switch release events.
    pub release_events: u64,
    /// Last observed CW count.
    pub last_cw: u64,
    /// Last observed CCW count.
    pub last_ccw: u64,
    /// Last observed press count.
    pub last_press: u64,
    /// Last observed release count.
    pub last_release: u64,
}

impl EncoderStats {
    /// Records a successful poll iteration.
    pub fn record_success(&mut self, status: &EncoderStatus) {
        self.samples.record_success_no_sample();
        self.cw_events += status.cw_count.saturating_sub(self.last_cw);
        self.ccw_events += status.ccw_count.saturating_sub(self.last_ccw);
        self.press_events += status.press_count.saturating_sub(self.last_press);
        self.release_events += status.release_count.saturating_sub(self.last_release);
        self.last_cw = status.cw_count;
        self.last_ccw = status.ccw_count;
        self.last_press = status.press_count;
        self.last_release = status.release_count;
    }

    /// Records a failed poll iteration.
    pub fn record_error(&mut self) {
        self.samples.record_error();
    }
}

/// Prints session summary statistics to stdout.
pub fn print_session_summary(stats: &EncoderStats) {
    let s = &stats.samples;
    println!("\n--- Encoder Diagnostic Session Summary ---");
    println!("Total polls  : {}", s.total_reads());
    println!("Successful   : {}", s.successful_reads());
    if s.total_reads() > 0 {
        let errors = s.errors();
        println!(
            "Poll errors  : {errors} ({:.2}% error rate)",
            s.error_rate_percent()
        );
    }
    println!("CW transitions  : {}", stats.cw_events);
    println!("CCW transitions : {}", stats.ccw_events);
    println!("Switch press : {}", stats.press_events);
    println!("Switch release: {}", stats.release_events);
}

/// Formats and outputs encoder status as JSON.
pub fn output_json_status(timestamp: &str, status: &EncoderStatus) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "switch_pressed": status.switch_pressed,
        "cw_count": status.cw_count,
        "ccw_count": status.ccw_count,
        "press_count": status.press_count,
        "release_count": status.release_count,
        "led_color": status.led_color.to_string(),
    });
    println!("{obj}");
}

/// Formats and outputs an error as JSON.
pub fn output_json_error(timestamp: &str, err: &EncoderError) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "error": err.summary(),
        })
    );
}

/// Returns the direction of rotation if a counter advanced since the last poll.
#[expect(
    clippy::similar_names,
    reason = "status/stats pair reads naturally for the incremental event diff helper"
)]
#[must_use]
pub fn describe_rotation(
    status: &EncoderStatus,
    stats: &EncoderStats,
) -> Option<RotationDirection> {
    if status.cw_count > stats.last_cw {
        Some(RotationDirection::Clockwise)
    } else if status.ccw_count > stats.last_ccw {
        Some(RotationDirection::CounterClockwise)
    } else {
        None
    }
}

/// Handles a single encoder poll iteration.
pub fn handle_encoder_iteration(
    device: &EncoderDevice,
    opts: &CliOptions,
    timestamp: &str,
    poll_stats: &mut EncoderStats,
) {
    match device.read_status() {
        Ok(status) => {
            let rotation = describe_rotation(&status, poll_stats);
            poll_stats.record_success(&status);
            match opts.common.format {
                OutputFormat::Json => output_json_status(timestamp, &status),
                OutputFormat::Human => {
                    let mut parts = Vec::new();
                    if let Some(direction) = rotation {
                        parts.push(format!("Rotate: {direction}"));
                    }
                    if status.switch_pressed {
                        parts.push("Switch: pressed".to_owned());
                    }
                    let events = if parts.is_empty() {
                        String::new()
                    } else {
                        format!("  |  {}", parts.join("  "))
                    };
                    println!(
                        "[{timestamp}] CW {:4}  CCW {:4}  Press {:3}  Rel {:3}  LED {:8}{events}",
                        status.cw_count,
                        status.ccw_count,
                        status.press_count,
                        status.release_count,
                        status.led_color,
                    );
                }
            }
        }
        Err(err) => {
            poll_stats.record_error();
            match opts.common.format {
                OutputFormat::Json => output_json_error(timestamp, &err),
                OutputFormat::Human => {
                    eprintln!("[{timestamp}] Poll error: {err}");
                }
            }
        }
    }
}

/// Runs the RGB LED indicator verification channel sweep.
///
/// # Errors
///
/// Returns an error if setting an LED channel fails.
pub fn run_led_sweep(
    device: &EncoderDevice,
    format: OutputFormat,
    dwell: Duration,
) -> Result<(), EncoderError> {
    for color in [
        RgbColor::RED,
        RgbColor::GREEN,
        RgbColor::BLUE,
        RgbColor::YELLOW,
        RgbColor::CYAN,
        RgbColor::MAGENTA,
        RgbColor::WHITE,
        RgbColor::OFF,
    ] {
        device.set_leds(color)?;
        match format {
            OutputFormat::Json => println!(
                "{}",
                serde_json::json!({
                    "timestamp": format_timestamp(),
                    "event": "led_sweep",
                    "color": color.to_string(),
                })
            ),
            OutputFormat::Human => println!("LED: {color}"),
        }
        std::thread::sleep(dwell);
    }
    Ok(())
}

/// Runs the main encoder diagnostic application loop.
#[must_use]
pub fn run_app(device: &EncoderDevice, opts: &CliOptions) -> ExitCode {
    if opts.verify_leds {
        if opts.common.format == OutputFormat::Human {
            println!("RGB indicator channel sweep (common anode, active-LOW):\n");
        }
        return match run_led_sweep(device, opts.common.format, LED_SWEEP_DWELL) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                if opts.common.format == OutputFormat::Json {
                    output_json_error(&format_timestamp(), &err);
                } else {
                    eprintln!("LED sweep failed: {err}");
                }
                ExitCode::FAILURE
            }
        };
    }

    if opts.common.format == OutputFormat::Human {
        print_header(device);
        if opts.common.verbose {
            print_diagnostic_status(device);
        }
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        } else {
            println!();
        }
    }

    let mut stats = EncoderStats::default();

    run_poll_loop(opts.common.interval, |timestamp| {
        handle_encoder_iteration(device, opts, timestamp, &mut stats);
        opts.common
            .max_count
            .is_some_and(|m| stats.samples.total_reads() >= m)
    });

    if should_print_summary(
        opts.common.format,
        opts.common.max_count,
        stats.samples.total_reads(),
    ) {
        print_session_summary(&stats);
    }

    exit_code_for_reads(
        stats.samples.total_reads(),
        stats.samples.successful_reads(),
    )
}

/// Standalone CLI binary entrypoint.
pub fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "encoder-read", print_usage) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let device = match EncoderDevice::new() {
        Ok(d) => d,
        Err(err) => {
            if opts.common.format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({
                        "timestamp": format_timestamp(),
                        "error": err.summary(),
                    })
                );
            } else {
                eprintln!("Failed to initialize EC12 encoder device: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    run_app(&device, &opts)
}
