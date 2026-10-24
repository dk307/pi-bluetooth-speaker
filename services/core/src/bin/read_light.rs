//! BH1750 ambient light sensor diagnostic utility.
//!
//! Provides CLI diagnostics for the I2C ambient light sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! configures measurement resolution, and continuously reports live optical
//! illuminance (lux and foot-candles) alongside hardware registers.

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use speaker_core::diag::{
    CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads, foot_candles,
    format_timestamp, next_arg_value, resolve_cli_opts, round_2dp, should_print_summary,
};
use speaker_core::hardware::{
    BH1750_DEFAULT_I2C_BUS, I2cAddress, Illuminance, LightError, LightSensor, MeasurementMode,
};

/// Default polling interval between light measurements.
///
/// Set to 300 ms to exceed the worst-case continuous High-Res conversion time (180 ms)
/// with ample headroom for Linux scheduling, ensuring a fresh conversion is available on each poll.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy)]
struct CliOptions {
    common: CommonCliOptions,
    address: I2cAddress,
    mode: MeasurementMode,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        }
    }
}

fn print_usage(bin_name: &str) {
    println!("BH1750 Ambient Light Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit");
    println!("  -n, --count <COUNT>      Read COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 0.3)");
    println!(
        "  -m, --mode <MODE>        Measurement mode: high-res (h1), high-res2 (h2), low-res (low) (default: high-res)"
    );
    println!("  -a, --address <HEX>      I2C address in hex: 0x23 (default) or 0x5C");
    println!(
        "  -v, --verbose            Display extended hardware registers and info (human format only)"
    );
    println!("  -j, --json               Stream readings in newline-delimited JSON");
    println!("  -h, --help               Display this help text");
}

const CONTINUOUS_MODES: [MeasurementMode; 3] = [
    MeasurementMode::ContinuousHighRes,
    MeasurementMode::ContinuousHighRes2,
    MeasurementMode::ContinuousLowRes,
];

const MODE_SHORT_ALIASES: &[(&str, MeasurementMode)] = &[
    ("h-res", MeasurementMode::ContinuousHighRes),
    ("h1", MeasurementMode::ContinuousHighRes),
    ("h-res2", MeasurementMode::ContinuousHighRes2),
    ("h2", MeasurementMode::ContinuousHighRes2),
    ("l-res", MeasurementMode::ContinuousLowRes),
    ("low", MeasurementMode::ContinuousLowRes),
];

fn parse_mode(s: &str) -> Result<MeasurementMode, String> {
    let lower = s.to_ascii_lowercase();
    CONTINUOUS_MODES
        .into_iter()
        .find(|m| m.slug() == lower || m.slug().replace('-', "") == lower)
        .or_else(|| {
            MODE_SHORT_ALIASES
                .iter()
                .find_map(|(alias, mode)| (*alias == lower).then_some(*mode))
        })
        .ok_or_else(|| format!("invalid mode '{lower}'. Supported: high-res, high-res2, low-res"))
}

fn parse_cli_args_from<I>(args: I) -> Result<Option<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut opts = CliOptions::default();

    while let Some(arg) = args.next() {
        match opts.common.parse_common_flag(&arg, &mut args)? {
            None => return Ok(None),
            Some(true) => continue,
            Some(false) => {}
        }
        match arg.as_str() {
            "-m" | "--mode" => opts.mode = parse_mode(&next_arg_value(&mut args, "--mode")?)?,
            "-a" | "--address" => {
                opts.address = I2cAddress::parse_hex(&next_arg_value(&mut args, "--address")?)?;
            }
            unknown => return Err(format!("unknown option '{unknown}', use --help for usage")),
        }
    }

    Ok(Some(opts))
}

fn parse_cli_args() -> Result<Option<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

fn print_header(sensor: &LightSensor) {
    let hw = sensor.hardware_info();
    println!("==================================================");
    println!("     BH1750 AMBIENT LIGHT SENSOR MONITOR          ");
    println!("==================================================");
    println!("Hardware I2C Info:");
    println!("  • Sensor       : ROHM BH1750FVI Digital Ambient Light");
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
    println!("  • Power / GND  : {} | {}", hw.vcc_pin, hw.gnd_pin);
    println!("--------------------------------------------------");
}

fn print_diagnostic_registers(sensor: &LightSensor) {
    println!("\nExtended Hardware Diagnostics:");
    match sensor.read_diagnostics() {
        Ok(diag) => println!("{diag}"),
        Err(err) => eprintln!("  [!] Failed to read diagnostics: {err}"),
    }
    println!("--------------------------------------------------");
}

fn print_session_summary(stats: &NumericStats) {
    println!("\n--- Light Diagnostic Session Summary ---");
    println!("Total reads : {}", stats.total_reads());
    println!("Successful  : {}", stats.successful_reads());
    if stats.total_reads() > 0 {
        let errors = stats.errors();
        println!(
            "Read errors : {errors} ({:.2}% error rate)",
            stats.error_rate_percent(errors)
        );
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "average of f32 lux samples fits in f32"
    )]
    if let (Some(min), Some(max), Some(avg)) = (stats.min(), stats.max(), stats.average()) {
        println!("Min Light   : {min:7.2} lx ({:6.2} fc)", foot_candles(min));
        println!("Max Light   : {max:7.2} lx ({:6.2} fc)", foot_candles(max));
        println!(
            "Avg Light   : {avg:7.2} lx ({:6.2} fc)",
            foot_candles(avg as f32)
        );
    }
}

fn output_json_reading(
    timestamp: &str,
    sensor: &LightSensor,
    illum: Illuminance,
    mode: MeasurementMode,
) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "i2c_bus": sensor.i2c_bus(),
        "i2c_address": sensor.i2c_address().to_string(),
        "mode": mode.slug(),
        "illuminance": {
            "lux": round_2dp(illum.lux()),
            "millilux": illum.millilux(),
            "foot_candles": round_2dp(illum.foot_candles()),
        }
    });
    println!("{obj}");
}

fn output_json_error(
    timestamp: &str,
    bus: u8,
    address: I2cAddress,
    mode: MeasurementMode,
    err: &LightError,
) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "i2c_bus": bus,
            "i2c_address": address.to_string(),
            "mode": mode.slug(),
            "error": err.summary(),
        })
    );
}

fn handle_light_iteration(
    sensor: &LightSensor,
    opts: &CliOptions,
    timestamp: &str,
    stats: &mut NumericStats,
) {
    match sensor.read_lux() {
        Ok(illum) => {
            stats.record_success(illum.lux());
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_reading(timestamp, sensor, illum, opts.mode);
                }
                OutputFormat::Human => {
                    println!(
                        "[{timestamp}] Illuminance: {:7.2} lx  | {:6.2} fc",
                        illum.lux(),
                        illum.foot_candles()
                    );
                }
            }
        }
        Err(err) => {
            stats.record_error();
            match opts.common.format {
                OutputFormat::Json => {
                    output_json_error(
                        timestamp,
                        sensor.i2c_bus(),
                        sensor.i2c_address(),
                        opts.mode,
                        &err,
                    );
                }
                OutputFormat::Human => {
                    eprintln!("[{timestamp}] Read error: {err}");
                }
            }
        }
    }
}

fn run_app(sensor: &LightSensor, opts: &CliOptions) -> ExitCode {
    if let Err(err) = sensor.set_mode(opts.mode) {
        if opts.common.format == OutputFormat::Json {
            output_json_error(
                &format_timestamp(),
                sensor.i2c_bus(),
                sensor.i2c_address(),
                opts.mode,
                &err,
            );
        } else {
            eprintln!("Failed to set BH1750 mode to {}: {err}", opts.mode);
        }
        return ExitCode::FAILURE;
    }

    if opts.common.format == OutputFormat::Human {
        print_header(sensor);
        if opts.common.verbose {
            print_diagnostic_registers(sensor);
        }
        if opts.common.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        } else {
            println!();
        }
    }

    let mut stats = NumericStats::default();

    loop {
        let loop_start = Instant::now();
        let timestamp = format_timestamp();

        handle_light_iteration(sensor, opts, &timestamp, &mut stats);

        if let Some(max) = opts.common.max_count {
            if stats.total_reads() >= max {
                break;
            }
        }

        let elapsed = loop_start.elapsed();
        if let Some(remaining) = opts.common.interval.checked_sub(elapsed) {
            thread::sleep(remaining);
        }
    }

    if should_print_summary(
        opts.common.format,
        opts.common.max_count,
        stats.total_reads(),
    ) {
        print_session_summary(&stats);
    }

    exit_code_for_reads(stats.total_reads(), stats.successful_reads())
}

fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "read_light", print_usage) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let sensor = match LightSensor::with_bus_and_address(BH1750_DEFAULT_I2C_BUS, opts.address) {
        Ok(s) => s,
        Err(err) => {
            if opts.common.format == OutputFormat::Json {
                output_json_error(
                    &format_timestamp(),
                    BH1750_DEFAULT_I2C_BUS,
                    opts.address,
                    opts.mode,
                    &err,
                );
            } else {
                eprintln!("Failed to initialize BH1750 light sensor: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    run_app(&sensor, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_variants_and_errors() {
        for (input, expected) in [
            ("high-res", MeasurementMode::ContinuousHighRes),
            ("highres", MeasurementMode::ContinuousHighRes),
            ("h-res", MeasurementMode::ContinuousHighRes),
            ("h1", MeasurementMode::ContinuousHighRes),
            ("high-res2", MeasurementMode::ContinuousHighRes2),
            ("highres2", MeasurementMode::ContinuousHighRes2),
            ("h-res2", MeasurementMode::ContinuousHighRes2),
            ("h2", MeasurementMode::ContinuousHighRes2),
            ("low-res", MeasurementMode::ContinuousLowRes),
            ("lowres", MeasurementMode::ContinuousLowRes),
            ("l-res", MeasurementMode::ContinuousLowRes),
            ("low", MeasurementMode::ContinuousLowRes),
        ] {
            assert_eq!(parse_mode(input).unwrap(), expected);
        }
        let err = parse_mode("invalid-mode").unwrap_err();
        assert!(err.contains("invalid mode 'invalid-mode'"));
    }

    #[test]
    fn statistics_computations() {
        let mut stats = NumericStats::default();
        assert!(stats.average().is_none());

        stats.record_success(100.0);
        stats.record_success(200.0);
        stats.record_error();

        assert_eq!(stats.total_reads(), 3);
        assert_eq!(stats.successful_reads(), 2);
        assert_eq!(stats.errors(), 1);
        assert_eq!(stats.min(), Some(100.0));
        assert_eq!(stats.max(), Some(200.0));
        assert!((stats.average().unwrap() - 150.0).abs() < 1e-4);

        print_session_summary(&stats);

        let empty_stats = NumericStats::default();
        print_session_summary(&empty_stats);
    }

    #[test]
    fn header_diagnostics_and_iterations_with_mock() {
        let (sensor, mock) = LightSensor::new_mocked();
        print_header(&sensor);
        print_diagnostic_registers(&sensor);

        // Error path for diagnostic registers
        mock.set_fault(Some("diagnostics bus timeout".into()));
        print_diagnostic_registers(&sensor);
        mock.set_fault(None);

        let opts_human = CliOptions {
            common: CommonCliOptions {
                max_count: Some(1),
                interval: Duration::from_millis(1),
                format: OutputFormat::Human,
                verbose: true,
            },
            ..CliOptions::default()
        };
        let mut stats = NumericStats::default();
        handle_light_iteration(&sensor, &opts_human, "12:00:00", &mut stats);
        assert_eq!(stats.successful_reads(), 1);

        let opts_json = CliOptions {
            common: CommonCliOptions {
                format: OutputFormat::Json,
                verbose: false,
                ..opts_human.common
            },
            ..opts_human
        };
        handle_light_iteration(&sensor, &opts_json, "12:00:01", &mut stats);
        assert_eq!(stats.successful_reads(), 2);

        // Injected error iteration
        mock.set_fault(Some("hardware disconnected".into()));
        handle_light_iteration(&sensor, &opts_human, "12:00:02", &mut stats);
        handle_light_iteration(&sensor, &opts_json, "12:00:03", &mut stats);
        assert_eq!(stats.errors(), 2);

        // Test run_app in human mode
        mock.set_fault(None);
        let opts_run = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                ..opts_human.common
            },
            ..opts_human
        };
        assert_eq!(run_app(&sensor, &opts_run), ExitCode::SUCCESS);

        // Test run_app in JSON mode
        assert_eq!(run_app(&sensor, &opts_json), ExitCode::SUCCESS);

        // Test run_app mode failure in human and JSON modes
        mock.set_fault(Some("mode error".into()));
        assert_eq!(run_app(&sensor, &opts_human), ExitCode::FAILURE);
        assert_eq!(run_app(&sensor, &opts_json), ExitCode::FAILURE);
    }

    #[test]
    fn parse_cli_args_flags_and_validation() {
        print_usage("read_light_test");

        // Help
        assert!(parse_cli_args_from(["--help".into()]).unwrap().is_none());
        assert!(parse_cli_args_from(["-h".into()]).unwrap().is_none());

        // Zero count
        let err_zero = parse_cli_args_from(["--count".into(), "0".into()]).unwrap_err();
        assert!(err_zero.contains("--count must be at least 1"));

        // Valid flags
        let opts = parse_cli_args_from([
            "--once".into(),
            "--verbose".into(),
            "--json".into(),
            "--count".into(),
            "5".into(),
            "--interval".into(),
            "0.5".into(),
            "--mode".into(),
            "low-res".into(),
            "--address".into(),
            "0x5C".into(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(opts.common.max_count, Some(5));
        assert!(opts.common.verbose);
        assert_eq!(opts.common.format, OutputFormat::Json);
        assert_eq!(opts.mode, MeasurementMode::ContinuousLowRes);
        assert_eq!(opts.address, I2cAddress::alternate_address());

        // Errors (including signed hex addresses before and after 0x prefix)
        for (bad_args, expected_msg) in [
            (&["--count"][..], "missing value for --count"),
            (&["--count", "abc"][..], "invalid count"),
            (&["--interval"][..], "missing value for --interval"),
            (
                &["--interval", "0"][..],
                "interval must be a finite value greater than 0",
            ),
            (&["--interval", "bad"][..], "invalid interval"),
            (&["--mode"][..], "missing value for --mode"),
            (&["--address"][..], "missing value for --address"),
            (
                &["--address", "+0x23"][..],
                "sign characters are not allowed",
            ),
            (
                &["--address", "-0x23"][..],
                "sign characters are not allowed",
            ),
            (
                &["--address", "0x+23"][..],
                "sign characters are not allowed",
            ),
            (
                &["--address", "0x-23"][..],
                "sign characters are not allowed",
            ),
            (
                &["--address", "0X+5C"][..],
                "sign characters are not allowed",
            ),
            (&["--address", "not-hex"][..], "invalid hex address"),
            (&["--address", "0x77"][..], "invalid BH1750 I2C address"),
            (&["--unknown"][..], "unknown option '--unknown'"),
        ] {
            let err = parse_cli_args_from(bad_args.iter().map(|s| (*s).to_string())).unwrap_err();
            assert!(
                err.contains(expected_msg),
                "expected '{expected_msg}' in '{err}'"
            );
        }
    }
}
