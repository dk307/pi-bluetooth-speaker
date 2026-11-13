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
    AmpDevice, AmpError, AmpErrorFlags, AmpI2cAddress, AmpSettings, AmpStatus, AudioInputMode,
    I2sFormat, MA12070P_DEFAULT_I2C_BUS, PowerModeCtrl, PowerModeProfile,
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
    /// Master volume raw attenuation (0..=144; dB = raw − 144).
    pub volume_raw: Option<u16>,
    /// Serial mute state (register 0x36 bit 7).
    pub mute: Option<bool>,
    /// Manual power mode `PM_man`.
    pub pm: Option<PowerModeCtrl>,
    /// Power mode profile (0-5).
    pub profile: Option<PowerModeProfile>,
    /// I2S format select.
    pub i2s_format: Option<I2sFormat>,
    /// Audio input mode.
    pub audio_mode: Option<AudioInputMode>,
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
            || self.audio_mode.is_some()
    }
}

/// Valid I2S format selects (register 0x35 bits 2:0, datasheet Table 8-8).
const VALID_I2S_FORMATS: [u8; 6] = [0, 1, 4, 5, 6, 7];

/// Parses an I2S format CLI value; accepts numeric (0,1,4-7) or human-readable names.
fn parse_i2s_format(value: &str) -> Result<I2sFormat, String> {
    let trimmed = value.trim().to_ascii_lowercase();
    // Try human-readable names first
    match trimmed.as_str() {
        "standard" | "i2s" | "std" => return Ok(I2sFormat::StandardI2s),
        "left-justified" | "left" | "lj" => return Ok(I2sFormat::LeftJustified),
        "rj16" | "right-justified-16" | "rj-16" => return Ok(I2sFormat::Rj16),
        "rj18" | "right-justified-18" | "rj-18" => return Ok(I2sFormat::Rj18),
        "rj20" | "right-justified-20" | "rj-20" => return Ok(I2sFormat::Rj20),
        "rj24" | "right-justified-24" | "rj-24" => return Ok(I2sFormat::Rj24),
        _ => {}
    }
    // Fall back to numeric
    let parsed = parse_u8_in_range(&trimmed, "I2S format", 7)?;
    if !VALID_I2S_FORMATS.contains(&parsed) {
        return Err(format!(
            "Invalid I2S format value '{value}': 2 and 3 are reserved (valid: 0, 1, 4, 5, 6, 7)"
        ));
    }
    Ok(I2sFormat::from_field(parsed))
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

/// Parses a manual power mode CLI value; accepts numeric (0-3) or human-readable names.
fn parse_power_mode_ctrl(value: &str) -> Result<PowerModeCtrl, String> {
    let trimmed = value.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "pm0" | "p0" | "0" => Ok(PowerModeCtrl::Pm0),
        "pm1" | "p1" | "1" => Ok(PowerModeCtrl::Pm1),
        "pm2" | "p2" | "2" => Ok(PowerModeCtrl::Pm2),
        "pm3" | "p3" | "3" => Ok(PowerModeCtrl::Pm3),
        _ => {
            let parsed = parse_u8_in_range(&trimmed, "power mode", 3)?;
            Ok(PowerModeCtrl::from_field(parsed))
        }
    }
}

/// Parses a power mode profile CLI value; accepts numeric (0-5) or human-readable names.
fn parse_power_mode_profile(value: &str) -> Result<PowerModeProfile, String> {
    let trimmed = value.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "pmp0" | "p0" | "0" => Ok(PowerModeProfile::Pmp0),
        "pmp1" | "p1" | "1" => Ok(PowerModeProfile::Pmp1),
        "pmp2" | "p2" | "2" => Ok(PowerModeProfile::Pmp2),
        "pmp3" | "p3" | "3" => Ok(PowerModeProfile::Pmp3),
        "pmp4" | "p4" | "4" => Ok(PowerModeProfile::Pmp4),
        "pmp5" | "p5" | "custom" | "5" => Ok(PowerModeProfile::Pmp5Custom),
        _ => {
            let parsed = parse_u8_in_range(&trimmed, "power mode profile", 5)?;
            Ok(PowerModeProfile::from_field(parsed))
        }
    }
}

/// Parses an audio input mode CLI value; accepts numeric (0-5) or human-readable names.
fn parse_audio_input_mode(value: &str) -> Result<AudioInputMode, String> {
    let trimmed = value.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "digital-20db" | "d20" | "0" => Ok(AudioInputMode::Digital20dB),
        "digital-26db" | "d26" | "1" => Ok(AudioInputMode::Digital26dB),
        "digital-32db" | "d32" | "2" => Ok(AudioInputMode::Digital32dB),
        "digital-0db" | "digital-bypass" | "d0" | "3" => Ok(AudioInputMode::Digital0dB),
        "analog-20db" | "a20" | "4" => Ok(AudioInputMode::Analog20dB),
        "analog-26db" | "a26" | "5" => Ok(AudioInputMode::Analog26dB),
        _ => {
            let parsed = parse_u8_in_range(&trimmed, "audio input mode", 5)?;
            Ok(AudioInputMode::from_field(parsed))
        }
    }
}

/// Parses a master volume raw attenuation value (0-144; dB = raw − 144).
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
    println!(
        "  --set-volume <RAW>       Master volume attenuation 0-144 (dB = raw - 144; 144 = 0 dB)"
    );
    println!("  --set-mute <on|off>      Serial mute (register 0x36 bit 7)");
    println!("  --set-pm <NAME|0-3>      Manual power mode (pm0/pm1/pm2/pm3 or 0-3)");
    println!("  --set-profile <NAME|0-5> Power mode profile (pmp0-pmp5/custom or 0-5)");
    println!(
        "  --set-i2s <NAME|0,1,4-7> I2S format (standard/i2s, left-justified/lj, rj16, rj18, rj20, rj24)"
    );
    println!(
        "  --set-audio-mode <NAME|0-5> Audio input mode (digital-20db/digital-26db/digital-32db/digital-0db/analog-20db/analog-26db or 0-5)"
    );
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
                opts.set.pm = Some(parse_power_mode_ctrl(&next_arg_value(args, "--set-pm")?)?);
                Ok(true)
            }
            "--set-profile" => {
                opts.set.profile = Some(parse_power_mode_profile(&next_arg_value(
                    args,
                    "--set-profile",
                )?)?);
                Ok(true)
            }
            "--set-i2s" => {
                opts.set.i2s_format = Some(parse_i2s_format(&next_arg_value(args, "--set-i2s")?)?);
                Ok(true)
            }
            "--set-audio-mode" => {
                opts.set.audio_mode = Some(parse_audio_input_mode(&next_arg_value(
                    args,
                    "--set-audio-mode",
                )?)?);
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
        "pm_mon": format!("{:?}", status.pm_mon),
        "pm_mon_description": status.pm_mon.description(),
        "pm2_config": {
            "usespread": status.pm2_config.usespread,
            "dtsteps": status.pm2_config.dtsteps,
            "baltype": status.pm2_config.baltype,
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
            // dB = raw − 144 (user attenuation scale; raw 144 = 0 dB).
            "master_volume_db": i64::from(settings.master_volume_raw) - 144,
            "master_volume_frac": settings.master_volume_frac,
            "serial_muted": settings.serial_muted,
            "pm_ctrl_pm": settings.pm_ctrl_pm,
            "pm_profile": settings.pm_profile,
            "i2s_format": settings.i2s_format,
            "audio_in_mode": settings.audio_in_mode,
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
                            format!("{:?}", status.pm_mon),
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
/// Reads back and reports the current settings.
/// Returns `Err` when the settings readback fails (already reported), so
/// automation can distinguish a completed readback from a failed one.
fn show_settings(amp: &AmpDevice, format: OutputFormat) -> Result<(), AmpError> {
    match amp.read_settings() {
        Ok(settings) => match format {
            OutputFormat::Json => output_json_settings(&format_timestamp(), amp, &settings),
            OutputFormat::Human => {
                println!("Configuration Settings:");
                println!("{settings}");
                println!("---------------------------------------------------------");
            }
        },
        Err(err) => {
            match format {
                OutputFormat::Json => {
                    output_json_error(&format_timestamp(), amp.i2c_bus(), amp.i2c_address(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("Failed to read configuration settings: {err}");
                }
            }
            return Err(err);
        }
    }
    Ok(())
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
/// A deferred write operation: its report label and the write itself. The
/// closure form keeps the write unexecuted until `handle_set` reaches it in
/// the safety-ordered sequence (no eager bus transactions).
type PendingWrite<'a> = (&'static str, Box<dyn FnOnce() -> Result<(), AmpError> + 'a>);

fn handle_set(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
    let set = opts.set;

    // Safety ordering: a requested MUTE is applied FIRST (the amp must
    // never sit loud while later writes run); a requested UNMUTE is
    // applied LAST, only after every other write succeeded — a failed
    // volume write must never unmask the output. Ops are collected as
    // DEFERRED closures in that order and executed one at a time, so a
    // failure stops before any later transaction touches the bus.
    let mut writes: Vec<PendingWrite<'_>> = Vec::new();
    if set.mute == Some(true) {
        writes.push(("serial mute", Box::new(|| amp.write_serial_mute(true))));
    }
    if let Some(raw) = set.volume_raw {
        writes.push((
            "master volume",
            Box::new(move || amp.write_master_volume(raw)),
        ));
    }
    if let Some(pm) = set.pm {
        writes.push((
            "power mode",
            Box::new(move || amp.write_power_mode_ctrl(pm)),
        ));
    }
    if let Some(profile) = set.profile {
        writes.push((
            "power mode profile",
            Box::new(move || amp.write_power_mode_profile(profile)),
        ));
    }
    if let Some(format) = set.i2s_format {
        writes.push(("I2S format", Box::new(move || amp.write_i2s_format(format))));
    }
    if let Some(mode) = set.audio_mode {
        writes.push((
            "audio input mode",
            Box::new(move || amp.write_audio_in_mode(mode)),
        ));
    }
    // Unmute is pushed AFTER every other requested write: a failure in any
    // of them must leave the amp muted (a failed set never unmasks output).
    if set.mute == Some(false) {
        writes.push(("serial mute", Box::new(|| amp.write_serial_mute(false))));
    }

    for (name, op) in writes {
        if !report_write(amp, opts, name, op()) {
            return ExitCode::FAILURE;
        }
    }

    if show_settings(amp, opts.common.format).is_err() {
        return ExitCode::FAILURE;
    }
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
    // A failed readback here cannot abort the monitoring session (reads
    // follow their own error-rate accounting); keep the previous
    // behavior for the poll path.
    let _ = show_settings(amp, opts.common.format);

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
