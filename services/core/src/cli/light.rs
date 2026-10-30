//! BH1750 ambient light sensor diagnostic CLI application logic.
//!
//! Provides CLI diagnostics for the I2C ambient light sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! and continuously reports live optical illuminance (lux and foot-candles)
//! alongside hardware diagnostics.

use std::process::ExitCode;
use std::time::Duration;

use crate::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    foot_candles, format_timestamp, next_arg_value, parse_args_with_common, print_min_max_avg,
    resolve_cli_opts, round_2dp, run_poll_loop, should_print_summary,
};
use crate::hardware::{BH1750_DEFAULT_I2C_BUS, I2cAddress, Illuminance, LightError, LightSensor};

/// Default polling interval between light measurements.
///
/// Set to 300 ms to exceed the continuous High-Res conversion time (~120 ms, max 180 ms)
/// with ample headroom for Linux scheduling, ensuring a fresh conversion is available on each poll.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Command-line configuration parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliOptions {
    /// Shared common CLI options (timing, formatting, count).
    pub common: CommonCliOptions,
    /// Target BH1750 I2C bus address.
    pub address: I2cAddress,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(DEFAULT_POLL_INTERVAL),
            address: I2cAddress::default(),
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
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

/// Parses CLI arguments from an arbitrary iterator into [`CliParse<CliOptions>`].
///
/// # Errors
///
/// Returns a descriptive error string if argument parsing or address validation fails.
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
                opts.address = I2cAddress::parse_hex(&next_arg_value(args, "--address")?)?;
                Ok(true)
            }
            _ => Ok(false),
        },
    )
}

/// Parses command-line arguments from [`std::env::args`].
///
/// # Errors
///
/// Returns a descriptive error string if argument parsing fails.
pub fn parse_cli_args() -> Result<CliParse<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

/// Prints standard hardware wiring and bus information banner to stdout.
pub fn print_header(sensor: &LightSensor) {
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

/// Prints extended register and sensor configuration diagnostics to stdout.
pub fn print_diagnostic_registers(sensor: &LightSensor) {
    println!("\nExtended Hardware Diagnostics:");
    match sensor.read_diagnostics() {
        Ok(diag) => println!("{diag}"),
        Err(err) => eprintln!("  [!] Failed to read diagnostics: {err}"),
    }
    println!("--------------------------------------------------");
}

/// Prints summary statistics for the completed monitoring session to stdout.
pub fn print_session_summary(stats: &NumericStats) {
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

/// Formats and outputs a single successful reading as newline-delimited JSON.
pub fn output_json_reading(timestamp: &str, sensor: &LightSensor, illum: Illuminance) {
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

/// Formats and outputs a single hardware read error as newline-delimited JSON.
pub fn output_json_error(timestamp: &str, bus: u8, address: I2cAddress, err: &LightError) {
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

/// Executes a single sampling iteration, updating cumulative statistics and formatting output.
pub fn handle_light_iteration(
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

/// Runs the complete diagnostic monitoring loop with the provided sensor and options.
#[must_use]
pub fn run_app(sensor: &LightSensor, opts: &CliOptions) -> ExitCode {
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

/// Standalone CLI binary entrypoint for `light-read`.
#[must_use]
pub fn main() -> ExitCode {
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
