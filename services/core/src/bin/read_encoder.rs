//! EC12 rotary encoder diagnostic utility.
//!
//! Provides CLI diagnostics for the quadrature encoder subsystem on the Raspberry
//! Pi 3 appliance. Displays hardware GPIO pin mappings, verifies the RGB indicator
//! channels, and continuously reports live rotation, switch, and LED state.

use std::process::ExitCode;
use std::time::Duration;

use speaker_core::diag::{
    CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads, format_timestamp,
    parse_args_with_common, resolve_cli_opts, run_poll_loop, should_print_summary,
};
use speaker_core::hardware::{
    EncoderDevice, EncoderError, EncoderStatus, RgbColor, RotationDirection,
};

/// Default polling interval between encoder state samples.
///
/// Set to 10 ms to closely match the baseline's continuous polling cadence (~2 ms
/// inter-transition sleeps). The baseline decode table only counts transitions
/// originating from rest states 00/11, so a fast flick can skip intermediate states
/// between polls and drop detents; use shorter intervals or interrupt-driven decoding
/// for high-rate rotation capture.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy)]
struct CliOptions {
    common: CommonCliOptions,
    /// Verify the RGB indicator channels with the baseline color sweep and exit.
    verify_leds: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            verify_leds: false,
        }
    }
}

fn print_usage(bin_name: &str) {
    println!("EC12 Rotary Encoder Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Sample once and exit");
    println!("  -n, --count <COUNT>      Sample COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 0.1)");
    println!("  -l, --verify-leds        Run the RGB channel color sweep and exit");
    println!(
        "  -v, --verbose            Display extended hardware diagnostics (human format only)"
    );
    println!("  -j, --json               Stream events in newline-delimited JSON");
    println!("  -h, --help               Display this help text");
}

fn parse_cli_args_from<I>(args: I) -> Result<Option<CliOptions>, String>
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

fn parse_cli_args() -> Result<Option<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

fn print_header(device: &EncoderDevice) {
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

fn print_diagnostic_status(device: &EncoderDevice) {
    println!("\nExtended Hardware Diagnostics:");
    match device.read_status() {
        Ok(status) => println!("{status}"),
        Err(err) => eprintln!("  [!] Failed to read encoder status: {err}"),
    }
    println!("--------------------------------------------------");
}

/// Session statistics tracking poll outcomes and cumulative encoder events.
#[derive(Debug, Default)]
struct EncoderStats {
    samples: NumericStats,
    cw_events: u64,
    ccw_events: u64,
    press_events: u64,
    release_events: u64,
    last_cw: u64,
    last_ccw: u64,
    last_press: u64,
    last_release: u64,
}

impl EncoderStats {
    fn record_success(&mut self, status: &EncoderStatus) {
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

    fn record_error(&mut self) {
        self.samples.record_error();
    }
}

fn print_session_summary(stats: &EncoderStats) {
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
    println!("CW detents   : {}", stats.cw_events);
    println!("CCW detents  : {}", stats.ccw_events);
    println!("Switch press : {}", stats.press_events);
    println!("Switch release: {}", stats.release_events);
}

fn output_json_status(timestamp: &str, status: &EncoderStatus) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "phase_a": status.phase_a,
        "phase_b": status.phase_b,
        "switch_pressed": status.switch_pressed,
        "cw_count": status.cw_count,
        "ccw_count": status.ccw_count,
        "press_count": status.press_count,
        "release_count": status.release_count,
        "led_color": status.led_color.to_string(),
    });
    println!("{obj}");
}

fn output_json_error(timestamp: &str, err: &EncoderError) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "error": err.summary(),
        })
    );
}

#[expect(
    clippy::similar_names,
    reason = "status/stats pair reads naturally for the incremental event diff helper"
)]
fn describe_rotation(status: &EncoderStatus, stats: &EncoderStats) -> Option<RotationDirection> {
    if status.cw_count > stats.last_cw {
        Some(RotationDirection::Clockwise)
    } else if status.ccw_count > stats.last_ccw {
        Some(RotationDirection::CounterClockwise)
    } else {
        None
    }
}

fn handle_encoder_iteration(
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

fn run_led_sweep(device: &EncoderDevice, format: OutputFormat) -> Result<(), EncoderError> {
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
    }
    Ok(())
}

fn run_app(device: &EncoderDevice, opts: &CliOptions) -> ExitCode {
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

fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "read_encoder", print_usage) {
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

    if opts.verify_leds {
        if opts.common.format == OutputFormat::Human {
            println!("RGB indicator channel sweep (common anode, active-LOW):\n");
        }
        return match run_led_sweep(&device, opts.common.format) {
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

    run_app(&device, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_computations() {
        let mut stats = EncoderStats::default();

        let baseline = EncoderStatus {
            phase_a: true,
            phase_b: true,
            switch_pressed: false,
            cw_count: 0,
            ccw_count: 0,
            press_count: 0,
            release_count: 0,
            led_color: RgbColor::OFF,
        };
        stats.record_success(&baseline);
        assert_eq!(stats.samples.total_reads(), 1);
        assert_eq!(stats.samples.successful_reads(), 1);
        assert_eq!(stats.cw_events, 0);

        let rotated = EncoderStatus {
            cw_count: 2,
            ccw_count: 1,
            press_count: 1,
            release_count: 1,
            switch_pressed: false,
            ..baseline
        };
        stats.record_success(&rotated);
        assert_eq!(stats.cw_events, 2);
        assert_eq!(stats.ccw_events, 1);
        assert_eq!(stats.press_events, 1);
        assert_eq!(stats.release_events, 1);

        stats.record_error();
        assert_eq!(stats.samples.errors(), 1);
        assert_eq!(stats.samples.total_reads(), 3);

        print_session_summary(&stats);
        print_session_summary(&EncoderStats::default());
    }

    #[test]
    fn parse_cli_args_flags_and_validation() {
        assert!(parse_cli_args_from(["--help".into()]).unwrap().is_none());
        assert!(parse_cli_args_from(["-h".into()]).unwrap().is_none());

        let opts = parse_cli_args_from(["--once".into()]).unwrap().unwrap();
        assert_eq!(opts.common.max_count, Some(1));
        assert!(!opts.verify_leds);

        let opts = parse_cli_args_from([
            "--count".into(),
            "5".into(),
            "--interval".into(),
            "0.5".into(),
            "--verify-leds".into(),
            "--verbose".into(),
            "--json".into(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(opts.common.max_count, Some(5));
        assert_eq!(opts.common.interval, Duration::from_millis(500));
        assert!(opts.verify_leds);
        assert!(opts.common.verbose);
        assert_eq!(opts.common.format, OutputFormat::Json);

        let _ = parse_cli_args_from(["--unknown-flag".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--count".into(), "not-a-number".into()]).unwrap_err();
    }

    #[test]
    fn header_diagnostics_and_iterations_with_mock() {
        let (device, mock) = EncoderDevice::new_mocked();
        print_header(&device);
        print_diagnostic_status(&device);

        let opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                verbose: true,
                format: OutputFormat::Human,
            },
            verify_leds: false,
        };

        let code = run_app(&device, &opts);
        assert_eq!(code, ExitCode::SUCCESS);

        let json_opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                verbose: false,
                format: OutputFormat::Json,
            },
            verify_leds: false,
        };

        let code = run_app(&device, &json_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Test error handling branch
        mock.set_fault(Some("GPIO simulated bus fault".into()));
        let mut err_stats = EncoderStats::default();
        handle_encoder_iteration(&device, &opts, "12:00:00", &mut err_stats);
        handle_encoder_iteration(&device, &json_opts, "12:00:00", &mut err_stats);
        assert_eq!(err_stats.samples.errors(), 2);
    }

    #[test]
    fn led_sweep_runs_on_mock() {
        let (device, mock) = EncoderDevice::new_mocked();
        run_led_sweep(&device, OutputFormat::Human).unwrap();
        assert_eq!(mock.led_color(), RgbColor::OFF);

        let (device, _mock) = EncoderDevice::new_mocked();
        run_led_sweep(&device, OutputFormat::Json).unwrap();
    }
}
