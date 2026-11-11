//! MA12070P audio amplifier hardware monitor and diagnostic utility.
//!
//! Provides CLI diagnostics for the Infineon MERUS MA12070P multi-level Class-D
//! amplifier on the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! decodes power mode profiles, audio format, and internal charge pump states,
//! monitors real-time and historical latched error flags, and supports clearing error latches.

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use speaker_common::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts, run_poll_loop,
    should_print_summary,
};
use speaker_hardware::{
    AmpDevice, AmpError, AmpErrorFlags, AmpI2cAddress, AmpSettings, AmpStatus,
    MA12070P_DEFAULT_I2C_BUS,
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
    /// Pending Phase 1.5 setting changes to apply before reading.
    pub set: SetValues,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            address: AmpI2cAddress::default(),
            clear: false,
            set: SetValues::default(),
        }
    }
}

/// Phase 1.5 setting changes requested on the command line.
///
/// Each `Some` value is written to the amplifier once, in field order, before
/// the diagnostic read loop begins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetValues {
    /// Master volume raw attenuation (0..=144, raw = -dB).
    pub volume_raw: Option<u16>,
    /// Serial mute state (register 0x36 bit 7).
    pub mute: Option<bool>,
    /// Manual power mode `PM_man` (0-3).
    pub pm: Option<u8>,
    /// Power mode profile (0-4).
    pub profile: Option<u8>,
    /// I2S format select (0-4).
    pub i2s_format: Option<u8>,
    /// Audio input gain select (0-3).
    pub gain: Option<u8>,
}

impl SetValues {
    /// Returns `true` if any setting change was requested.
    #[must_use]
    pub const fn any(self) -> bool {
        self.volume_raw.is_some()
            || self.mute.is_some()
            || self.pm.is_some()
            || self.profile.is_some()
            || self.i2s_format.is_some()
            || self.gain.is_some()
    }
}

/// Parses an unsigned integer CLI value and enforces an inclusive upper bound.
///
/// Returns a clean, user-facing error message (no backtrace) on invalid input.
fn parse_u8_in_range(value: &str, name: &str, max: u8) -> Result<u8, String> {
    let trimmed = value.trim();
    let parsed: u8 = match trimmed.parse() {
        Ok(v) => v,
        Err(_) => {
            return Err(format!(
                "Invalid {name} value '{value}': expected an integer 0-{max}"
            ));
        }
    };
    if parsed > max {
        return Err(format!("Invalid {name} value '{value}': expected 0-{max}"));
    }
    Ok(parsed)
}

/// Parses a master volume raw attenuation value (0-144, raw = -dB).
fn parse_volume_raw(value: &str) -> Result<u16, String> {
    let trimmed = value.trim();
    let parsed: u16 = match trimmed.parse() {
        Ok(v) => v,
        Err(_) => {
            return Err(format!(
                "Invalid volume value '{value}': expected an integer 0-144"
            ));
        }
    };
    if parsed > 144 {
        return Err(format!("Invalid volume value '{value}': expected 0-144"));
    }
    Ok(parsed)
}

/// Parses an on/off mute value.
fn parse_mute(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!(
            "Invalid mute value '{value}': expected 'on' or 'off'"
        )),
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
    println!();
    println!("Setting changes (applied once before reading, then exit):");
    println!("  --set-volume <RAW>       Master volume attenuation 0-144 (raw = -dB; 144 = 0 dB)");
    println!("  --set-mute <on|off>      Serial mute (register 0x36 bit 7)");
    println!("  --set-pm <0-3>           Manual power mode PM_man (0-3)");
    println!("  --set-profile <0-4>      Power mode profile (0-4)");
    println!("  --set-i2s <0-4>          I2S format select (0-4)");
    println!("  --set-gain <0-3>         Audio input gain select (0-3)");
    println!();
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
            "--set-volume" => {
                opts.set.volume_raw =
                    Some(parse_volume_raw(&next_arg_value(args, "--set-volume")?)?);
                Ok(true)
            }
            "--set-mute" => {
                opts.set.mute = Some(parse_mute(&next_arg_value(args, "--set-mute")?)?);
                Ok(true)
            }
            "--set-pm" => {
                opts.set.pm = Some(parse_u8_in_range(
                    &next_arg_value(args, "--set-pm")?,
                    "power mode",
                    3,
                )?);
                Ok(true)
            }
            "--set-profile" => {
                opts.set.profile = Some(parse_u8_in_range(
                    &next_arg_value(args, "--set-profile")?,
                    "power mode profile",
                    4,
                )?);
                Ok(true)
            }
            "--set-i2s" => {
                opts.set.i2s_format = Some(parse_u8_in_range(
                    &next_arg_value(args, "--set-i2s")?,
                    "I2S format",
                    4,
                )?);
                Ok(true)
            }
            "--set-gain" => {
                opts.set.gain = Some(parse_u8_in_range(
                    &next_arg_value(args, "--set-gain")?,
                    "audio input gain",
                    3,
                )?);
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

/// Outputs Phase 1.5 settings in JSON format.
pub fn output_json_settings(timestamp: &str, amp: &AmpDevice, settings: &AmpSettings) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "i2c_bus": amp.i2c_bus(),
        "i2c_address": amp.i2c_address().to_string(),
        "settings": {
            "master_volume_raw": settings.master_volume_raw,
            "master_volume_db": -i64::from(settings.master_volume_raw),
            "master_volume_frac": settings.master_volume_frac,
            "serial_muted": settings.serial_muted,
            "pm_ctrl_pm": settings.pm_ctrl_pm,
            "pm_profile": settings.pm_profile,
            "i2s_format": settings.i2s_format,
            "audio_in_gain": settings.audio_in_gain,
        },
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

/// Reads the Phase 1.5 configuration settings and prints them in the active output format.
fn show_settings(amp: &AmpDevice, format: OutputFormat) {
    match amp.read_settings() {
        Ok(settings) => match format {
            OutputFormat::Json => output_json_settings(&format_timestamp(), amp, &settings),
            OutputFormat::Human => {
                println!("Configuration Settings:");
                println!("{settings}");
                println!("---------------------------------------------------------");
            }
        },
        Err(err) => match format {
            OutputFormat::Json => {
                output_json_error(&format_timestamp(), amp.i2c_bus(), amp.i2c_address(), &err);
            }
            OutputFormat::Human => {
                eprintln!("Failed to read configuration settings: {err}");
            }
        },
    }
}

/// Reports a single setting write in the active output format.
/// Returns `true` on success, `false` on failure (error already reported).
fn report_write(
    amp: &AmpDevice,
    opts: &CliOptions,
    name: &str,
    result: Result<(), AmpError>,
) -> bool {
    match result {
        Ok(()) => {
            match opts.common.format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "timestamp": format_timestamp(),
                            "i2c_bus": amp.i2c_bus(),
                            "i2c_address": amp.i2c_address().to_string(),
                            "set": name,
                            "ok": true,
                        })
                    );
                }
                OutputFormat::Human => {
                    println!("Set {name}: OK");
                }
            }
            true
        }
        Err(err) => {
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(&format_timestamp(), amp.i2c_bus(), amp.i2c_address(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("Failed to set {name}: {err}");
                }
            }
            false
        }
    }
}

/// Applies requested Phase 1.5 setting changes, then reads back and reports the settings.
///
/// Stops at the first failed write to avoid cascading transactions on a misbehaving bus.
fn handle_set(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
    let set = opts.set;

    if let Some(raw) = set.volume_raw
        && !report_write(amp, opts, "master volume", amp.write_master_volume(raw))
    {
        return ExitCode::FAILURE;
    }
    if let Some(muted) = set.mute
        && !report_write(amp, opts, "serial mute", amp.write_serial_mute(muted))
    {
        return ExitCode::FAILURE;
    }
    if let Some(pm) = set.pm
        && !report_write(amp, opts, "power mode", amp.write_power_mode_ctrl(pm))
    {
        return ExitCode::FAILURE;
    }
    if let Some(profile) = set.profile
        && !report_write(
            amp,
            opts,
            "power mode profile",
            amp.write_power_mode_profile(profile),
        )
    {
        return ExitCode::FAILURE;
    }
    if let Some(format) = set.i2s_format
        && !report_write(amp, opts, "I2S format", amp.write_i2s_format(format))
    {
        return ExitCode::FAILURE;
    }
    if let Some(gain) = set.gain
        && !report_write(amp, opts, "audio input gain", amp.write_audio_in_mode(gain))
    {
        return ExitCode::FAILURE;
    }

    show_settings(amp, opts.common.format);
    ExitCode::SUCCESS
}

/// Runs the amplifier diagnostic polling or clearing workflow.
#[must_use]
pub fn run_app(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
    if opts.clear {
        return handle_clear(amp, opts);
    }

    if opts.set.any() {
        return handle_set(amp, opts);
    }

    if opts.common.format == OutputFormat::Human {
        print_header(amp);
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        }
    }
    show_settings(amp, opts.common.format);

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
