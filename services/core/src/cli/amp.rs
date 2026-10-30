//! MA12070P audio amplifier hardware monitor and diagnostic utility.
//!
//! Provides CLI diagnostics for the Infineon MERUS MA12070P multi-level Class-D
//! amplifier on the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! decodes power mode profiles, audio format, and internal charge pump states,
//! monitors real-time and historical latched error flags, and supports clearing error latches.

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use crate::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts, run_poll_loop,
    should_print_summary,
};
use crate::hardware::{
    AmpDevice, AmpError, AmpErrorFlags, AmpI2cAddress, AmpStatus, MA12070P_DEFAULT_I2C_BUS,
};

/// Default polling interval between amplifier status checks (1.0 second).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliOptions {
    /// Common CLI options (timing, formatting, count).
    pub common: CommonCliOptions,
    /// I2C address for the amplifier.
    pub address: AmpI2cAddress,
    /// Whether to clear latched error flags.
    pub clear: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            address: AmpI2cAddress::default(),
            clear: false,
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
    println!("MA12070P Amplifier Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit (default)");
    println!("  -R, --repeat             Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>      Read COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 1.0)");
    println!("  -a, --address <HEX>      I2C address in hex: 0x20 (default) or 0x21..=0x23");
    println!("  -c, --clear              Clear historical latched error logs");
    println!(
        "  -v, --verbose            Display extended hardware diagnostics (human format only)"
    );
    println!("  -j, --json               Stream readings in newline-delimited JSON");
    println!("  -V, --version            Print semantic version and exit");
    println!("  -h, --help               Display this help text");
}

/// Parses command-line arguments from an iterator of strings.
///
/// # Errors
///
/// Returns an error string if an argument or flag value is invalid or missing.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    parse_args_with_common(
        CliOptions::default(),
        args,
        |o| &mut o.common,
        |opts, arg, args| match arg {
            "-a" | "--address" => {
                opts.address = AmpI2cAddress::parse_hex(&next_arg_value(args, "--address")?)?;
                Ok(true)
            }
            "-c" | "--clear" => {
                opts.clear = true;
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

/// Diagnostic session statistics for amplifier status readings.
#[derive(Debug, Default)]
pub struct AmpStats {
    /// Numeric sample counters.
    pub samples: NumericStats,
    /// Number of reads where active live faults were present.
    pub live_fault_reads: usize,
    /// Number of reads where historical latched faults were present.
    pub latched_fault_reads: usize,
}

impl AmpStats {
    /// Records a successful status read.
    pub fn record_success(&mut self, status: &AmpStatus) {
        self.samples.record_success_no_sample();
        if !status.live_errors.is_clear() {
            self.live_fault_reads += 1;
        }
        if !status.latched_errors.is_clear() {
            self.latched_fault_reads += 1;
        }
    }

    /// Records a failed read iteration.
    pub fn record_error(&mut self) {
        self.samples.record_error();
    }
}

/// Prints the hardware metadata banner to stdout.
pub fn print_header(amp: &AmpDevice) {
    let hw = amp.hardware_info();
    println!("=========================================================");
    println!("            MA12070P AMPLIFIER STATUS                    ");
    println!("=========================================================");
    println!("Hardware I2C Info:");
    println!("  • Device       : Infineon MERUS MA12070P Class-D Audio Amplifier");
    println!("  • I2C Bus      : I2C-{} ({})", hw.bus, hw.device_path);
    println!("  • I2C Address  : {}", hw.address);
    println!(
        "  • SDA Line     : {} ({})",
        hw.sda_pin.bcm_pin, hw.sda_pin.physical_pin
    );
    println!(
        "  • SCL Line     : {} ({})",
        hw.scl_pin.bcm_pin, hw.scl_pin.physical_pin
    );
    println!(
        "  • Power / GND  : 3.3V ({}) | GND ({})",
        hw.vcc_pin, hw.gnd_pin
    );
    println!("---------------------------------------------------------");
}

/// Prints a summary of reading statistics to stdout.
pub fn print_session_summary(stats: &AmpStats) {
    let s = &stats.samples;
    println!("\n--- Amplifier Diagnostic Session Summary ---");
    println!("Total reads : {}", s.total_reads());
    println!("Successful  : {}", s.successful_reads());
    if s.total_reads() > 0 {
        let errors = s.errors();
        println!(
            "Read errors : {errors} ({:.2}% error rate)",
            s.error_rate_percent()
        );
    }
    println!("Live faults detected    : {}", stats.live_fault_reads);
    println!("Latched faults detected : {}", stats.latched_fault_reads);
}

/// Converts fault flags to a JSON value.
#[must_use]
pub fn faults_json(flags: AmpErrorFlags) -> serde_json::Value {
    let faults: Vec<_> = flags
        .faults()
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name(),
                "description": f.description(),
                "bit": f.bit(),
            })
        })
        .collect();

    serde_json::json!({
        "raw": flags.as_u8(),
        "is_ok": flags.is_clear(),
        "faults": faults,
    })
}

/// Outputs status information in newline-delimited JSON format.
pub fn output_json_status(timestamp: &str, amp: &AmpDevice, status: &AmpStatus) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "i2c_bus": amp.i2c_bus(),
        "i2c_address": amp.i2c_address().to_string(),
        "power_mode": format!("{:?}", status.power_mode),
        "power_mode_description": status.power_mode.description(),
        "audio_in": {
            "format": format!("{:?}", status.audio_in.format),
            "format_description": status.audio_in.format.description(),
            "ratio": format!("{:?}", status.audio_in.ratio),
            "ratio_description": status.audio_in.ratio.description(),
        },
        "dcu_monitor": {
            "ch0_active": status.dcu_monitor.ch0.is_active(),
            "ch1_active": status.dcu_monitor.ch1.is_active(),
        },
        "live_errors": faults_json(status.live_errors),
        "latched_errors": faults_json(status.latched_errors),
    });
    println!("{obj}");
}

/// Outputs an error message in JSON format.
pub fn output_json_error(timestamp: &str, bus: u8, address: AmpI2cAddress, err: &AmpError) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "i2c_bus": bus,
            "i2c_address": address.to_string(),
            "error": err.summary(),
        })
    );
}

/// Outputs clear operation result in JSON format.
pub fn output_json_clear(timestamp: &str, bus: u8, address: AmpI2cAddress) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "i2c_bus": bus,
            "i2c_address": address.to_string(),
            "cleared": true,
        })
    );
}

/// Handles clearing latched error registers.
#[must_use]
pub fn handle_clear(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
    match amp.clear_latched_errors() {
        Ok(()) => {
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_clear(&format_timestamp(), amp.i2c_bus(), amp.i2c_address());
                }
                OutputFormat::Human => {
                    println!("Success: Cleared historical latched error logs.");
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(&format_timestamp(), amp.i2c_bus(), amp.i2c_address(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("Failed to clear error logs: {err}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// Executes a single status read iteration.
pub fn handle_amp_iteration(
    amp: &AmpDevice,
    opts: &CliOptions,
    timestamp: &str,
    session_stats: &mut AmpStats,
) {
    match amp.read_status() {
        Ok(status) => {
            session_stats.record_success(&status);
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_status(timestamp, amp, &status);
                }
                OutputFormat::Human => {
                    if opts.common.max_count == Some(1) {
                        println!("{status}");
                        if !status.latched_errors.is_clear() {
                            let bin = env::args().next().unwrap_or_else(|| "amp-info".into());
                            println!("\nTip: Clear this historical log with: {bin} --clear");
                        }
                        println!("=========================================================");
                    } else {
                        let live_str = status.live_errors.names_joined();
                        let latched_str = status.latched_errors.names_joined();
                        println!(
                            "[{timestamp}] Status: {:5} | Mode: {:4} | DCU: Ch0 {:7}, Ch1 {:7} | Live: {live_str} | Latched: {latched_str}",
                            if status.live_errors.is_clear() {
                                "OK"
                            } else {
                                "FAULT"
                            },
                            format!("{:?}", status.power_mode),
                            format!("{:?}", status.dcu_monitor.ch0),
                            format!("{:?}", status.dcu_monitor.ch1),
                        );
                        if opts.common.verbose {
                            println!("{status}");
                            println!("---------------------------------------------------------");
                        }
                    }
                }
            }
        }
        Err(err) => {
            session_stats.record_error();
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(timestamp, amp.i2c_bus(), amp.i2c_address(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("[{timestamp}] Read error: {err}");
                }
            }
        }
    }
}

/// Runs the amplifier diagnostic polling or clearing workflow.
#[must_use]
pub fn run_app(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
    if opts.clear {
        return handle_clear(amp, opts);
    }

    if opts.common.format == OutputFormat::Human {
        print_header(amp);
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        }
    }

    let mut stats = AmpStats::default();

    run_poll_loop(opts.common.interval, |timestamp| {
        handle_amp_iteration(amp, opts, timestamp, &mut stats);
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
    let opts = match resolve_cli_opts(parse_cli_args(), "amp-info", print_usage) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let amp = match AmpDevice::with_bus_and_address(MA12070P_DEFAULT_I2C_BUS, opts.address) {
        Ok(a) => a,
        Err(err) => {
            if opts.common.format == OutputFormat::Json {
                output_json_error(
                    &format_timestamp(),
                    MA12070P_DEFAULT_I2C_BUS,
                    opts.address,
                    &err,
                );
            } else {
                eprintln!("Failed to initialize MA12070P amplifier: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    run_app(&amp, &opts)
}
