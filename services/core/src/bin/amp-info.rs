//! MA12070P audio amplifier hardware monitor and diagnostic utility.
//!
//! Provides CLI diagnostics for the Infineon MERUS MA12070P multi-level Class-D
//! amplifier on the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! decodes power mode profiles, audio format, and internal charge pump states,
//! monitors real-time and historical latched error flags, and supports clearing error latches.

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use speaker_core::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts, run_poll_loop,
    should_print_summary,
};
use speaker_core::hardware::{
    AmpDevice, AmpError, AmpErrorFlags, AmpI2cAddress, AmpStatus, MA12070P_DEFAULT_I2C_BUS,
};

/// Default polling interval between amplifier status checks (1.0 second).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy)]
struct CliOptions {
    common: CommonCliOptions,
    address: AmpI2cAddress,
    clear: bool,
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

fn print_usage(bin_name: &str) {
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

fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CliOptions>, String>
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

fn parse_cli_args() -> Result<CliParse<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

/// Diagnostic session statistics for amplifier status readings.
#[derive(Debug, Default)]
struct AmpStats {
    samples: NumericStats,
    live_fault_reads: usize,
    latched_fault_reads: usize,
}

impl AmpStats {
    fn record_success(&mut self, status: &AmpStatus) {
        self.samples.record_success_no_sample();
        if !status.live_errors.is_clear() {
            self.live_fault_reads += 1;
        }
        if !status.latched_errors.is_clear() {
            self.latched_fault_reads += 1;
        }
    }

    fn record_error(&mut self) {
        self.samples.record_error();
    }
}

fn print_header(amp: &AmpDevice) {
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

fn print_session_summary(stats: &AmpStats) {
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

fn faults_json(flags: AmpErrorFlags) -> serde_json::Value {
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

fn output_json_status(timestamp: &str, amp: &AmpDevice, status: &AmpStatus) {
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

fn output_json_error(timestamp: &str, bus: u8, address: AmpI2cAddress, err: &AmpError) {
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

fn output_json_clear(timestamp: &str, bus: u8, address: AmpI2cAddress) {
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

fn handle_clear(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
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

fn handle_amp_iteration(
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

fn run_app(amp: &AmpDevice, opts: &CliOptions) -> ExitCode {
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

fn main() -> ExitCode {
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

#[cfg(test)]
mod tests {
    use super::*;
    use speaker_core::hardware::{
        AudioInConfig, AudioInputFormat, ClockRatio, DcuChannelStatus, DcuMonitor, PowerModeProfile,
    };

    fn unwrap_options(parsed: Result<CliParse<CliOptions>, String>) -> CliOptions {
        match parsed.unwrap() {
            CliParse::Options(o) => o,
            other => panic!("expected CliParse::Options, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_flags_and_validation() {
        assert!(matches!(
            parse_cli_args_from(["--help".into()]).unwrap(),
            CliParse::Help
        ));
        assert!(matches!(
            parse_cli_args_from(["-h".into()]).unwrap(),
            CliParse::Help
        ));
        assert!(matches!(
            parse_cli_args_from(["--version".into()]).unwrap(),
            CliParse::Version
        ));

        // Defaults: once mode, one second interval
        let defaults = unwrap_options(parse_cli_args_from([]));
        assert_eq!(defaults.common.max_count, Some(1));
        assert_eq!(defaults.common.interval, DEFAULT_POLL_INTERVAL);

        // Repeat flag switches back to continuous polling
        let repeat = unwrap_options(parse_cli_args_from(["--repeat".into()]));
        assert_eq!(repeat.common.max_count, None);
        let repeat_short = unwrap_options(parse_cli_args_from(["-R".into()]));
        assert_eq!(repeat_short.common.max_count, None);

        let opts = unwrap_options(parse_cli_args_from(["--once".into()]));
        assert_eq!(opts.common.max_count, Some(1));
        assert_eq!(opts.address.as_u8(), 0x20);
        assert!(!opts.clear);

        let opts = unwrap_options(parse_cli_args_from([
            "--count".into(),
            "3".into(),
            "--interval".into(),
            "0.5".into(),
            "--address".into(),
            "0x21".into(),
            "--clear".into(),
            "--verbose".into(),
            "--json".into(),
        ]));

        assert_eq!(opts.common.max_count, Some(3));
        assert_eq!(opts.common.interval, Duration::from_millis(500));
        assert_eq!(opts.address.as_u8(), 0x21);
        assert!(opts.clear);
        assert!(opts.common.verbose);
        assert_eq!(opts.common.format, OutputFormat::Json);

        let _ = parse_cli_args_from(["--unknown".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--address".into(), "0x99".into()]).unwrap_err();
    }

    #[test]
    fn print_usage_runs_cleanly() {
        print_usage("amp-info");
    }

    #[test]
    fn statistics_computations_and_summary() {
        let mut stats = AmpStats::default();
        assert_eq!(stats.samples.total_reads(), 0);
        assert_eq!(stats.samples.successful_reads(), 0);
        assert_eq!(stats.samples.errors(), 0);
        assert_eq!(stats.samples.error_rate_percent(), 0.0);

        let ok_status = AmpStatus {
            power_mode: PowerModeProfile::Pmp2,
            audio_in: AudioInConfig {
                format: AudioInputFormat::StandardI2s,
                ratio: ClockRatio::AutoDetect,
            },
            dcu_monitor: DcuMonitor {
                ch0: DcuChannelStatus::Active,
                ch1: DcuChannelStatus::Active,
            },
            live_errors: AmpErrorFlags::NONE,
            latched_errors: AmpErrorFlags::NONE,
        };

        stats.record_success(&ok_status);
        assert_eq!(stats.samples.total_reads(), 1);
        assert_eq!(stats.samples.successful_reads(), 1);
        assert_eq!(stats.live_fault_reads, 0);

        let fault_status = AmpStatus {
            live_errors: AmpErrorFlags::from_bits(1 << 0),
            latched_errors: AmpErrorFlags::from_bits(1 << 1),
            ..ok_status
        };
        stats.record_success(&fault_status);
        assert_eq!(stats.live_fault_reads, 1);
        assert_eq!(stats.latched_fault_reads, 1);

        stats.record_error();
        assert_eq!(stats.samples.total_reads(), 3);
        assert_eq!(stats.samples.errors(), 1);
        assert!((stats.samples.error_rate_percent() - 33.333).abs() < 0.1);

        print_session_summary(&stats);

        let empty_stats = AmpStats::default();
        print_session_summary(&empty_stats);
    }

    #[test]
    fn run_app_iterations_and_failure_modes() {
        let (amp, mock) = AmpDevice::new_mocked();
        print_header(&amp);

        // Test single read (Human mode)
        let once_opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(1),
                interval: Duration::from_millis(1),
                verbose: false,
                format: OutputFormat::Human,
            },
            address: AmpI2cAddress::default(),
            clear: false,
        };
        let code = run_app(&amp, &once_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Single read with latched faults triggers tip display
        mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 2));
        let code = run_app(&amp, &once_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Continuous read (multiple iterations) with verbose
        let multi_opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                verbose: true,
                format: OutputFormat::Human,
            },
            address: AmpI2cAddress::default(),
            clear: false,
        };
        let code = run_app(&amp, &multi_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Continuous read (multiple iterations) with verbose and live faults
        mock.set_live_errors(AmpErrorFlags::from_bits(1 << 1));
        let code = run_app(&amp, &multi_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Continuous read (multiple iterations) with no faults
        mock.set_live_errors(AmpErrorFlags::NONE);
        mock.set_latched_errors(AmpErrorFlags::NONE);
        let code = run_app(&amp, &multi_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // JSON streaming mode with faults
        mock.set_live_errors(AmpErrorFlags::from_bits(1 << 0));
        mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 2));
        let json_opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                verbose: false,
                format: OutputFormat::Json,
            },
            address: AmpI2cAddress::default(),
            clear: false,
        };
        let code = run_app(&amp, &json_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Test clear mode (Human & JSON)
        let clear_opts = CliOptions {
            clear: true,
            ..once_opts
        };
        assert_eq!(run_app(&amp, &clear_opts), ExitCode::SUCCESS);

        let clear_json_opts = CliOptions {
            clear: true,
            ..json_opts
        };
        assert_eq!(run_app(&amp, &clear_json_opts), ExitCode::SUCCESS);

        // Injected mock fault during clear
        mock.set_fault(Some("I2C write failure".into()));
        assert_eq!(run_app(&amp, &clear_opts), ExitCode::FAILURE);
        assert_eq!(run_app(&amp, &clear_json_opts), ExitCode::FAILURE);

        // Injected mock fault during read iteration
        let mut err_stats = AmpStats::default();
        handle_amp_iteration(&amp, &once_opts, "12:00:00", &mut err_stats);
        handle_amp_iteration(&amp, &json_opts, "12:00:00", &mut err_stats);
        assert_eq!(err_stats.samples.errors(), 2);
    }
}
