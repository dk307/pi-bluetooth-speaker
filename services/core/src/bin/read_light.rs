//! BH1750 ambient light sensor diagnostic utility.
//!
//! Provides CLI diagnostics for the I2C ambient light sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! configures measurement resolution, and continuously reports live optical
//! illuminance (lux and foot-candles) alongside hardware registers.

use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use speaker_core::hardware::{
    BH1750_ALT_I2C_ADDR, BH1750_DEFAULT_I2C_ADDR, BH1750_DEFAULT_I2C_BUS, I2cAddress, Illuminance,
    LUX_TO_FOOT_CANDLES, LightSensor, MeasurementMode,
};

/// Default polling interval between light measurements.
///
/// Set to 300 ms to exceed the worst-case continuous High-Res conversion time (180 ms)
/// with ample headroom for Linux scheduling, ensuring a fresh conversion is available on each poll.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Formats UTC wall-clock time as `HH:MM:SS`.
fn format_timestamp() -> String {
    let total_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let hours = (total_secs / 3600) % 24;
    let minutes = (total_secs / 60) % 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Output formatting mode for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OutputFormat {
    #[default]
    Human,
    Json,
}

/// Diagnostic display options.
#[derive(Debug, Clone, Copy, Default)]
struct DiagnosticFlags {
    verbose: bool,
}

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy)]
struct CliOptions {
    max_count: Option<usize>,
    interval: Duration,
    format: OutputFormat,
    flags: DiagnosticFlags,
    address: I2cAddress,
    mode: MeasurementMode,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            max_count: None,
            interval: DEFAULT_POLL_INTERVAL,
            format: OutputFormat::Human,
            flags: DiagnosticFlags::default(),
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

fn parse_mode(s: &str) -> Result<MeasurementMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "high-res" | "highres" | "h-res" | "h1" => Ok(MeasurementMode::ContinuousHighRes),
        "high-res2" | "highres2" | "h-res2" | "h2" => Ok(MeasurementMode::ContinuousHighRes2),
        "low-res" | "lowres" | "l-res" | "low" => Ok(MeasurementMode::ContinuousLowRes),
        other => Err(format!(
            "invalid mode '{other}'. Supported: high-res, high-res2, low-res"
        )),
    }
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
            "-v" | "--verbose" => opts.flags.verbose = true,
            "-j" | "--json" => opts.format = OutputFormat::Json,
            "-n" | "--count" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --count".into());
                };
                let count = val
                    .parse::<usize>()
                    .map_err(|e| format!("invalid count '{val}': {e}"))?;
                if count == 0 {
                    return Err("--count must be at least 1".into());
                }
                opts.max_count = Some(count);
            }
            "-i" | "--interval" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --interval".into());
                };
                let secs = val
                    .parse::<f32>()
                    .map_err(|e| format!("invalid interval '{val}': {e}"))?;
                opts.interval = Duration::try_from_secs_f32(secs)
                    .ok()
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| {
                        format!("interval must be a finite value greater than 0, got '{val}'")
                    })?;
            }
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
    parse_cli_args_from(
        env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
    )
}

/// Running statistical summary for ambient light telemetry.
#[derive(Debug, Default)]
struct Statistics {
    total_reads: usize,
    successful_reads: usize,
    errors: usize,
    min_lux: Option<f32>,
    max_lux: Option<f32>,
    lux_sum: f64,
}

impl Statistics {
    fn record_success(&mut self, illum: Illuminance) {
        self.total_reads += 1;
        self.successful_reads += 1;
        let lux = illum.lux();
        self.lux_sum += f64::from(lux);
        self.min_lux = Some(self.min_lux.map_or(lux, |cur| cur.min(lux)));
        self.max_lux = Some(self.max_lux.map_or(lux, |cur| cur.max(lux)));
    }

    fn record_error(&mut self) {
        self.total_reads += 1;
        self.errors += 1;
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "read counts are small; f64 precision is exact for integer counts"
    )]
    fn average_lux(&self) -> Option<f64> {
        (self.successful_reads > 0).then(|| self.lux_sum / self.successful_reads as f64)
    }
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

#[expect(
    clippy::cast_precision_loss,
    reason = "read counts are small; f32 precision is exact for percentages"
)]
fn print_session_summary(stats: &Statistics) {
    println!("\n--- Light Diagnostic Session Summary ---");
    println!("Total reads : {}", stats.total_reads);
    println!("Successful  : {}", stats.successful_reads);
    if stats.total_reads > 0 {
        let total = stats.total_reads as f32;
        println!(
            "Read errors : {} ({:.2}% error rate)",
            stats.errors,
            (stats.errors as f32 / total) * 100.0
        );
    }
    if let (Some(min), Some(max), Some(avg)) = (stats.min_lux, stats.max_lux, stats.average_lux()) {
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
        "mode": mode.as_str(),
        "illuminance": {
            "lux": lux_val,
            "millilux": illum.millilux(),
            "foot_candles": fc_val,
        }
    });
    println!("{obj}");
}

fn handle_light_iteration(
    sensor: &LightSensor,
    opts: &CliOptions,
    timestamp: &str,
    stats: &mut Statistics,
) {
    match sensor.read_lux() {
        Ok(illum) => {
            stats.record_success(illum);
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
                    println!(
                        "{}",
                        serde_json::json!({
                            "timestamp": timestamp,
                            "i2c_bus": sensor.i2c_bus(),
                            "i2c_address": sensor.i2c_address().to_string(),
                            "mode": opts.mode.to_string(),
                            "error": err.to_string(),
                        })
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
            println!("{}", serde_json::json!({ "error": err.to_string() }));
        } else {
            eprintln!("Failed to set BH1750 mode to {}: {err}", opts.mode);
        }
        return ExitCode::FAILURE;
    }

    if opts.format == OutputFormat::Human {
        print_header(sensor);
        if opts.flags.verbose {
            print_diagnostic_registers(sensor);
        }
        if opts.max_count.is_none() {
            println!("Press Ctrl+C to stop monitoring.\n");
        } else {
            println!();
        }
    }

    let mut stats = Statistics::default();
    let mut iterations = 0;

    loop {
        let loop_start = Instant::now();
        let timestamp = format_timestamp();

        handle_light_iteration(sensor, opts, &timestamp, &mut stats);

        iterations += 1;
        if let Some(max) = opts.max_count {
            if iterations >= max {
                break;
            }
        }

        let elapsed = loop_start.elapsed();
        if let Some(remaining) = opts.interval.checked_sub(elapsed) {
            thread::sleep(remaining);
        }
    }

    if opts.format == OutputFormat::Human && opts.max_count != Some(1) {
        print_session_summary(&stats);
    }

    if stats.total_reads > 0 && stats.successful_reads == 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn main() -> ExitCode {
    let opts = match parse_cli_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            let bin_name = env::args_os()
                .next()
                .map_or_else(|| "read_light".into(), |a| a.to_string_lossy().into_owned());
            print_usage(&bin_name);
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
                println!("{}", serde_json::json!({ "error": err.to_string() }));
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
    fn format_timestamp_returns_valid_time() {
        let ts = format_timestamp();
        assert_eq!(ts.len(), 8);
        assert_eq!(&ts[2..3], ":");
        assert_eq!(&ts[5..6], ":");
    }

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
        let mut stats = Statistics::default();
        assert!(stats.average_lux().is_none());

        stats.record_success(Illuminance::from_lux(100.0).unwrap());
        stats.record_success(Illuminance::from_lux(200.0).unwrap());
        stats.record_error();

        assert_eq!(stats.total_reads, 3);
        assert_eq!(stats.successful_reads, 2);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.min_lux, Some(100.0));
        assert_eq!(stats.max_lux, Some(200.0));
        assert!((stats.average_lux().unwrap() - 150.0).abs() < 1e-4);

        print_session_summary(&stats);

        let empty_stats = Statistics::default();
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
            flags: DiagnosticFlags { verbose: true },
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        };
        let mut stats = Statistics::default();
        handle_light_iteration(&sensor, &opts_human, "12:00:00", &mut stats);
        assert_eq!(stats.successful_reads, 1);

        let opts_json = CliOptions {
            format: OutputFormat::Json,
            ..opts_human
        };
        handle_light_iteration(&sensor, &opts_json, "12:00:01", &mut stats);
        assert_eq!(stats.successful_reads, 2);

        // Injected error iteration
        mock.set_fault(Some("hardware disconnected".into()));
        handle_light_iteration(&sensor, &opts_human, "12:00:02", &mut stats);
        handle_light_iteration(&sensor, &opts_json, "12:00:03", &mut stats);
        assert_eq!(stats.errors, 2);

        // Test run_app in human mode
        mock.set_fault(None);
        let opts_run = CliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            format: OutputFormat::Human,
            flags: DiagnosticFlags { verbose: true },
            address: I2cAddress::default(),
            mode: MeasurementMode::ContinuousHighRes,
        };
        assert_eq!(run_app(&sensor, &opts_run), ExitCode::SUCCESS);

        // Test run_app in JSON mode
        let opts_run_json = CliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(1),
            format: OutputFormat::Json,
            flags: DiagnosticFlags::default(),
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
            flags: DiagnosticFlags::default(),
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
        assert!(opts.flags.verbose);
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
