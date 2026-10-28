//! BH1750 ambient light sensor diagnostic utility.
//!
//! Provides CLI diagnostics for the I2C ambient light sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! and continuously reports live optical illuminance (lux and foot-candles)
//! alongside hardware diagnostics.

use std::process::ExitCode;
use std::time::Duration;

use speaker_core::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    foot_candles, format_timestamp, next_arg_value, parse_args_with_common, print_min_max_avg,
    resolve_cli_opts, round_2dp, run_poll_loop, should_print_summary,
};
use speaker_core::hardware::{
    BH1750_DEFAULT_I2C_BUS, I2cAddress, Illuminance, LightError, LightSensor,
};

/// Default polling interval between light measurements.
///
/// Set to 300 ms to exceed the continuous High-Res conversion time (~120 ms, max 180 ms)
/// with ample headroom for Linux scheduling, ensuring a fresh conversion is available on each poll.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy)]
struct CliOptions {
    common: CommonCliOptions,
    address: I2cAddress,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            address: I2cAddress::default(),
        }
    }
}

fn print_usage(bin_name: &str) {
    println!("BH1750 Ambient Light Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit (default)");
    println!("  -R, --repeat             Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>      Read COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 0.3)");
    println!("  -a, --address <HEX>      I2C address in hex: 0x23 (default) or 0x5C");
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
                opts.address = I2cAddress::parse_hex(&next_arg_value(args, "--address")?)?;
                Ok(true)
            }
            _ => Ok(false),
        },
    )
}

fn parse_cli_args() -> Result<CliParse<CliOptions>, String> {
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
    println!(
        "  • Power / GND  : 3.3V ({}) | GND ({})",
        hw.vcc_pin, hw.gnd_pin
    );
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
            stats.error_rate_percent()
        );
    }
    print_min_max_avg(stats, "Light", "lx", Some((foot_candles, "fc")));
}

fn output_json_reading(timestamp: &str, sensor: &LightSensor, illum: Illuminance) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "i2c_bus": sensor.i2c_bus(),
        "i2c_address": sensor.i2c_address().to_string(),
        "illuminance": {
            "lux": round_2dp(illum.lux()),
            "millilux": illum.millilux(),
            "foot_candles": round_2dp(illum.foot_candles()),
        }
    });
    println!("{obj}");
}

fn output_json_error(timestamp: &str, bus: u8, address: I2cAddress, err: &LightError) {
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
                    output_json_reading(timestamp, sensor, illum);
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
                    output_json_error(timestamp, sensor.i2c_bus(), sensor.i2c_address(), &err);
                }
                OutputFormat::Human => {
                    eprintln!("[{timestamp}] Read error: {err}");
                }
            }
        }
    }
}

fn run_app(sensor: &LightSensor, opts: &CliOptions) -> ExitCode {
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

    run_poll_loop(opts.common.interval, |timestamp| {
        handle_light_iteration(sensor, opts, timestamp, &mut stats);
        opts.common
            .max_count
            .is_some_and(|m| stats.total_reads() >= m)
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

fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args(), "light-read", print_usage) {
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

        let lines = speaker_core::diag::format_min_max_avg(
            &stats,
            "Light",
            "lx",
            Some((foot_candles, "fc")),
        )
        .unwrap();
        assert_eq!(lines[0], "Light Min   :  100.00 lx (  9.29 fc)");
        assert_eq!(lines[1], "Light Max   :  200.00 lx ( 18.58 fc)");
        assert_eq!(lines[2], "Light Avg   :  150.00 lx ( 13.94 fc)");

        print_session_summary(&stats);

        let empty_stats = NumericStats::default();
        print_session_summary(&empty_stats);
    }

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

        // Defaults: once mode, 300 ms interval
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
        assert_eq!(opts.address.as_u8(), 0x23);

        let opts = unwrap_options(parse_cli_args_from([
            "--count".into(),
            "5".into(),
            "--interval".into(),
            "0.5".into(),
            "--address".into(),
            "0x5C".into(),
            "--verbose".into(),
            "--json".into(),
        ]));

        assert_eq!(opts.common.max_count, Some(5));
        assert_eq!(opts.common.interval, Duration::from_millis(500));
        assert_eq!(opts.address.as_u8(), 0x5C);
        assert!(opts.common.verbose);
        assert_eq!(opts.common.format, OutputFormat::Json);

        let _ = parse_cli_args_from(["--unknown-flag".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--address".into(), "0x99".into()]).unwrap_err();
    }

    #[test]
    fn header_diagnostics_and_iterations_with_mock() {
        let (sensor, mock) = LightSensor::new_mocked();
        print_header(&sensor);
        print_diagnostic_registers(&sensor);

        let opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                verbose: true,
                format: OutputFormat::Human,
            },
            address: I2cAddress::default(),
        };

        let code = run_app(&sensor, &opts);
        assert_eq!(code, ExitCode::SUCCESS);

        let json_opts = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                verbose: false,
                format: OutputFormat::Json,
            },
            address: I2cAddress::default(),
        };

        let code = run_app(&sensor, &json_opts);
        assert_eq!(code, ExitCode::SUCCESS);

        // Test error handling branch
        mock.set_fault(Some("I2C simulated bus lockup".into()));
        let mut err_stats = NumericStats::default();
        handle_light_iteration(&sensor, &opts, "12:00:00", &mut err_stats);
        handle_light_iteration(&sensor, &json_opts, "12:00:00", &mut err_stats);
        assert_eq!(err_stats.errors(), 2);
    }
}
