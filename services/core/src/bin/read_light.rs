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
    NumericStats, OutputFormat, env_args, exit_code_for_reads, format_timestamp, parse_count_arg,
    parse_interval_arg, program_name, should_print_summary,
};
use speaker_core::hardware::{
    BH1750_ALT_I2C_ADDR, BH1750_DEFAULT_I2C_ADDR, BH1750_DEFAULT_I2C_BUS, I2cAddress, Illuminance,
    LUX_TO_FOOT_CANDLES, LightSensor, MeasurementMode,
};

/// Default polling interval between light measurements.
///
/// Set to 300 ms to exceed the worst-case continuous High-Res conversion time (180 ms)
/// with ample headroom for Linux scheduling, ensuring a fresh conversion is available on each poll.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy)]
struct CliOptions {
    max_count: Option<usize>,
    interval: Duration,
    format: OutputFormat,
    verbose: bool,
    address: I2cAddress,
    mode: MeasurementMode,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            max_count: None,
            interval: DEFAULT_POLL_INTERVAL,
            format: OutputFormat::Human,
            verbose: false,
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
        "  -m, --mode <MODE>        Measurement mode: high-res, high-res2, low-res (default: high-res)"
    );
    println!("  -a, --address <HEX>      I2C address in hex: 0x23 (default) or 0x5C");
    println!("  -v, --verbose            Display extended hardware registers and info");
    println!("  -j, --json               Stream readings in newline-delimited JSON");
    println!("  -h, --help               Display this help text");
}

const MODE_ALIASES: &[(&str, MeasurementMode)] = &[
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
];

fn parse_mode(s: &str) -> Result<MeasurementMode, String> {
    let lower = s.to_ascii_lowercase();
    MODE_ALIASES
        .iter()
        .find_map(|(alias, mode)| (*alias == lower).then_some(*mode))
        .ok_or_else(|| format!("invalid mode '{lower}'. Supported: high-res, high-res2, low-res"))
}

fn parse_hex_address(s: &str) -> Result<I2cAddress, String> {
    let clean = s.trim();
    if clean.starts_with(['+', '-']) {
        return Err(format!(
            "invalid hex address '{s}': sign characters are not allowed"
        ));
    }
    let clean = clean
        .strip_prefix("0x")
        .or_else(|| clean.strip_prefix("0X"))
        .unwrap_or(clean);
    let addr =
        u8::from_str_radix(clean, 16).map_err(|e| format!("invalid hex address '{s}': {e}"))?;
    I2cAddress::new(addr).map_err(|_err| {
        format!(
            "invalid BH1750 I2C address '{s}' (expected 0x{BH1750_DEFAULT_I2C_ADDR:02X} or 0x{BH1750_ALT_I2C_ADDR:02X})"
        )
    })
}

fn parse_cli_args_from<I>(args: I) -> Result<Option<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut opts = CliOptions::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-1" | "--once" => opts.max_count = Some(1),
            "-v" | "--verbose" => opts.verbose = true,
            "-j" | "--json" => opts.format = OutputFormat::Json,
            "-n" | "--count" => opts.max_count = Some(parse_count_arg(args.next())?),
            "-i" | "--interval" => opts.interval = parse_interval_arg(args.next())?,
            "-m" | "--mode" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --mode".into());
                };
                opts.mode = parse_mode(&val)?;
            }
            "-a" | "--address" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --address".into());
                };
                opts.address = parse_hex_address(&val)?;
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
    if let (Some(min), Some(max), Some(avg)) = (stats.min(), stats.max(), stats.average()) {
        println!(
            "Min Light   : {min:7.2} lx ({:6.2} fc)",
            min * LUX_TO_FOOT_CANDLES
        );
        println!(
            "Max Light   : {max:7.2} lx ({:6.2} fc)",
            max * LUX_TO_FOOT_CANDLES
        );
        println!(
            "Avg Light   : {avg:7.2} lx ({:6.2} fc)",
            avg * f64::from(LUX_TO_FOOT_CANDLES)
        );
    }
}

fn output_json_reading(
    timestamp: &str,
    sensor: &LightSensor,
    illum: Illuminance,
    mode: MeasurementMode,
) {
    let lux_val = (f64::from(illum.lux()) * 100.0).round() / 100.0;
    let fc_val = (f64::from(illum.foot_candles()) * 100.0).round() / 100.0;

    let obj = serde_json::json!({
        "timestamp": timestamp,
        "i2c_bus": sensor.i2c_bus(),
        "i2c_address": sensor.i2c_address().to_string(),
        "mode": mode.slug(),
        "illuminance": {
            "lux": lux_val,
            "millilux": illum.millilux(),
            "foot_candles": fc_val,
        }
    });
    println!("{obj}");
}

fn output_json_error(
    timestamp: &str,
    bus: u8,
    address: I2cAddress,
    mode: MeasurementMode,
    err: &dyn std::fmt::Display,
) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "i2c_bus": bus,
            "i2c_address": address.to_string(),
            "mode": mode.slug(),
            "error": err.to_string(),
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
            match opts.format {
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
            match opts.format {
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
        if opts.format == OutputFormat::Json {
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

    if opts.format == OutputFormat::Human {
        print_header(sensor);
        if opts.verbose {
            print_diagnostic_registers(sensor);
        }
        if opts.max_count.is_none() {
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

        if let Some(max) = opts.max_count {
            if stats.total_reads() >= max {
                break;
            }
        }

        let elapsed = loop_start.elapsed();
        if let Some(remaining) = opts.interval.checked_sub(elapsed) {
            thread::sleep(remaining);
        }
    }

    if should_print_summary(opts.format, opts.max_count, stats.total_reads()) {
        print_session_summary(&stats);
    }

    exit_code_for_reads(stats.total_reads(), stats.successful_reads())
}

fn main() -> ExitCode {
    let opts = match parse_cli_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            print_usage(&program_name("read_light"));
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("Error: {err}\nRun with --help for usage information.");
            return ExitCode::FAILURE;
        }
    };

    let sensor = match LightSensor::with_bus_and_address(BH1750_DEFAULT_I2C_BUS, opts.address) {
        Ok(s) => s,
        Err(err) => {
            if opts.format == OutputFormat::Json {
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
        assert_eq!(
            parse_mode("high-res").unwrap(),
            MeasurementMode::ContinuousHighRes
        );
        assert_eq!(
            parse_mode("highres").unwrap(),
            MeasurementMode::ContinuousHighRes
        );
        assert_eq!(
            parse_mode("h1").unwrap(),
            MeasurementMode::ContinuousHighRes
        );
        assert_eq!(
            parse_mode("high-res2").unwrap(),
            MeasurementMode::ContinuousHighRes2
        );
        assert_eq!(
            parse_mode("h2").unwrap(),
            MeasurementMode::ContinuousHighRes2
        );
        assert_eq!(
            parse_mode("low-res").unwrap(),
            MeasurementMode::ContinuousLowRes
        );
        assert_eq!(
            parse_mode("low").unwrap(),
            MeasurementMode::ContinuousLowRes
        );
        let _ = parse_mode("invalid-mode").unwrap_err();
    }

    #[test]
    fn hex_address_parsing() {
        assert_eq!(parse_hex_address("0x23").unwrap().as_u8(), 0x23);
        assert_eq!(parse_hex_address("0X5C").unwrap().as_u8(), 0x5C);
        assert_eq!(parse_hex_address("23").unwrap().as_u8(), 0x23);
        let err = parse_hex_address("0x77").unwrap_err();
        assert_eq!(
            err,
            "invalid BH1750 I2C address '0x77' (expected 0x23 or 0x5C)"
        );
        let err_not_hex = parse_hex_address("not-hex").unwrap_err();
        assert!(err_not_hex.contains("invalid hex address"));
        let err_plus = parse_hex_address("+0x23").unwrap_err();
        assert!(err_plus.contains("sign characters are not allowed"));
        let err_minus = parse_hex_address("-0x23").unwrap_err();
        assert!(err_minus.contains("sign characters are not allowed"));
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
            max_count: Some(1),
            interval: Duration::from_millis(10),
            format: OutputFormat::Human,
            verbose: true,
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        };
        let mut stats = NumericStats::default();
        handle_light_iteration(&sensor, &opts_human, "12:00:00", &mut stats);
        assert_eq!(stats.successful_reads(), 1);

        let opts_json = CliOptions {
            format: OutputFormat::Json,
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
            max_count: Some(2),
            interval: Duration::from_millis(1),
            format: OutputFormat::Human,
            verbose: true,
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        };
        assert_eq!(run_app(&sensor, &opts_run), ExitCode::SUCCESS);

        // Test run_app in JSON mode
        let opts_run_json = CliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(1),
            format: OutputFormat::Json,
            verbose: false,
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        };
        assert_eq!(run_app(&sensor, &opts_run_json), ExitCode::SUCCESS);

        // Test run_app mode failure
        mock.set_fault(Some("mode error".into()));
        let opts_fail = CliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(1),
            format: OutputFormat::Human,
            verbose: false,
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        };
        assert_eq!(run_app(&sensor, &opts_fail), ExitCode::FAILURE);

        let opts_fail_json = CliOptions {
            format: OutputFormat::Json,
            ..opts_fail
        };
        assert_eq!(run_app(&sensor, &opts_fail_json), ExitCode::FAILURE);
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

        assert_eq!(opts.max_count, Some(5));
        assert!(opts.verbose);
        assert_eq!(opts.format, OutputFormat::Json);
        assert_eq!(opts.mode, MeasurementMode::ContinuousLowRes);
        assert_eq!(opts.address, I2cAddress::alternate_address());

        // Errors
        let _ = parse_cli_args_from(["--count".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--count".into(), "abc".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--interval".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--interval".into(), "0".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--interval".into(), "bad".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--mode".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--address".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--unknown".into()]).unwrap_err();
    }
}
