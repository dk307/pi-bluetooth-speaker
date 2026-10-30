//! DS18B20 1-Wire temperature diagnostic CLI application logic.
//!
//! Provides CLI diagnostics for the 1-Wire temperature sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! discovers attached `28-*` DS18B20 digital thermometers, and reports
//! live chassis temperature telemetry, `SoC` core thermal metrics, and sensor hardware
//! diagnostic registers.

use std::process::ExitCode;
use std::time::Duration;

use crate::diag::{
    CliParse, CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads,
    format_timestamp, next_arg_value, parse_args_with_common, print_min_max_avg, resolve_cli_opts,
    round_2dp, run_poll_loop, should_print_summary,
};
use crate::hardware::{SensorId, SensorReading, SocThermal, TempError, TempSensor, Temperature};

/// Command-line configuration parameters for `temp-read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    /// Shared common CLI options (timing, formatting, count).
    pub common: CommonCliOptions,
    /// Whether to include BCM2837 `SoC` core temperature and thermal delta.
    pub include_soc: bool,
    /// Whether to display raw `w1_slave` scratchpad payloads.
    pub show_raw: bool,
    /// Optional specific sensor identifier to target.
    pub target_sensor: Option<SensorId>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            common: CommonCliOptions::with_default_interval(Duration::from_secs(1)),
            include_soc: false,
            show_raw: false,
            target_sensor: None,
        }
    }
}

/// Prints CLI usage and flag documentation to stdout.
pub fn print_usage(bin_name: &str) {
    println!("DS18B20 1-Wire Temperature Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit (default)");
    println!("  -R, --repeat             Poll repeatedly until interrupted (Ctrl+C)");
    println!("  -n, --count <COUNT>      Read at most COUNT device samples and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 1.0)");
    println!(
        "  -v, --verbose            Show detailed sensor registers & diagnostics (human format only)"
    );
    println!("  -s, --soc                Include BCM2837 SoC core temperature & delta");
    println!(
        "  -r, --raw                Display raw w1_slave scratchpad payload (human format only)"
    );
    println!("  -d, --device <ID>        Target specific sensor (e.g. 28-01201b7d5396)");
    println!("  -j, --json               Output readings as JSON lines");
    println!("  -V, --version            Print semantic version and exit");
    println!("  -h, --help               Show this help message and exit");
    println!();
    println!("Examples:");
    println!("  {bin_name} --verbose");
    println!("  {bin_name} --soc -i 2");
    println!("  {bin_name} --count 5 --json");
    println!("  {bin_name} --repeat --interval 5");
}

/// Parses CLI arguments from an arbitrary iterator into [`CliParse<CliOptions>`].
///
/// # Errors
///
/// Returns a descriptive error string if argument parsing or sensor ID validation fails.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    parse_args_with_common(
        CliOptions::default(),
        args,
        |o| &mut o.common,
        |opts, arg, args| match arg {
            "-s" | "--soc" => {
                opts.include_soc = true;
                Ok(true)
            }
            "-r" | "--raw" => {
                opts.show_raw = true;
                Ok(true)
            }
            "-d" | "--device" => {
                let val = next_arg_value(args, "--device")?;
                opts.target_sensor = Some(SensorId::parse_str(&val)?);
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

/// Running statistical summary for temperature monitoring sessions.
#[derive(Debug, Default)]
pub struct Statistics {
    /// Cumulative numeric sample statistics (min, max, average, total reads).
    pub samples: NumericStats,
    /// Number of encountered 1-Wire CRC check failures.
    pub crc_errors: usize,
    /// Number of other encountered errors (I/O, parsing, missing device).
    pub other_errors: usize,
}

impl Statistics {
    /// Records a successful temperature reading.
    pub fn record_success(&mut self, temp: Temperature) {
        self.samples.record_success(temp.celsius());
    }

    /// Records a temperature sensor read error, classifying CRC failures.
    pub fn record_error(&mut self, err: &TempError) {
        self.samples.record_error();
        if err.is_crc_mismatch() {
            self.crc_errors += 1;
        } else {
            self.other_errors += 1;
        }
    }
}

/// Prints standard hardware wiring and 1-Wire GPIO information to stdout.
pub fn print_header(sensor: &TempSensor) {
    let gpio = sensor.gpio_info();
    println!("==================================================");
    println!("      DS18B20 1-WIRE TEMPERATURE MONITOR          ");
    println!("==================================================");
    println!("Hardware GPIO Info:");
    println!("  • Signal       : {}", gpio.signal_name);
    println!(
        "  • GPIO Pin     : {} ({})",
        gpio.bcm_pin, gpio.physical_pin
    );
    println!("  • Bus Protocol : {}", gpio.bus_protocol);
    println!("  • DT Overlay   : {}", gpio.electrical_config);
    println!(
        "  • Sysfs Path   : {}/28-*/w1_slave",
        sensor.sysfs_base_path().display()
    );
    println!("--------------------------------------------------");
}

/// Prints extended register and sensor configuration diagnostics to stdout.
pub fn print_diagnostic_registers(sensor: &TempSensor, devices: &[SensorId]) {
    println!("\nExtended Hardware Registers:");
    for dev in devices {
        match sensor.read_diagnostics(dev) {
            Ok(diag) => println!("{diag}"),
            Err(err) => eprintln!("  [!] Failed to read diagnostics for {dev}: {err}"),
        }
    }
}

/// Prints summary statistics for the completed temperature monitoring session to stdout.
pub fn print_session_summary(stats: &Statistics) {
    let s = &stats.samples;
    println!("\n--- Diagnostic Session Summary ---");
    println!("Total reads : {}", s.total_reads());
    println!("Successful  : {}", s.successful_reads());
    println!(
        "CRC errors  : {} ({:.2}% error rate)",
        stats.crc_errors,
        s.fraction_of_reads_percent(stats.crc_errors)
    );
    if stats.other_errors > 0 {
        println!(
            "Other errors: {} ({:.2}% error rate)",
            stats.other_errors,
            s.fraction_of_reads_percent(stats.other_errors)
        );
    }
    print_min_max_avg(s, "Chassis", "°C", None);
}

/// Formats and outputs a single successful temperature reading as JSON.
pub fn output_json_reading(
    timestamp: &str,
    dev: &SensorId,
    temp: Temperature,
    soc: Option<SocThermal>,
) {
    let obj = serde_json::json!({
        "timestamp": timestamp,
        "sensor_id": dev.as_str(),
        "temperature": {
            "celsius": round_2dp(temp.celsius()),
            "millidegrees": temp.millidegrees_celsius(),
        },
        "soc_temperature": soc.map(|s| {
            serde_json::json!({
                "celsius": round_2dp(s.temperature().celsius()),
            })
        }),
        "delta_celsius": soc.map(|s| round_2dp(s.delta_from_chassis(temp))),
    });
    println!("{obj}");
}

/// Formats and outputs a single temperature read error as JSON.
pub fn output_json_error(timestamp: &str, sensor_id: Option<&SensorId>, err_msg: &str) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "sensor_id": sensor_id.map(SensorId::as_str),
            "error": err_msg,
        })
    );
}

/// Executes a single sampling iteration across all target sensors.
pub fn handle_sensor_iteration(
    sensor: &TempSensor,
    devices: &[SensorId],
    opts: &CliOptions,
    timestamp: &str,
    soc: Option<SocThermal>,
    stats: &mut Statistics,
) {
    for dev in devices {
        let read_result = if opts.show_raw && opts.common.format == OutputFormat::Human {
            sensor.read_diagnostics(dev).map(|diag| {
                (
                    SensorReading {
                        sensor_id: diag.sensor_id,
                        temperature: diag.temperature,
                    },
                    Some(diag.raw_payload),
                )
            })
        } else {
            sensor.read_sensor(dev).map(|reading| (reading, None))
        };

        match read_result {
            Ok((reading, raw_payload)) => {
                stats.record_success(reading.temperature);
                match opts.common.format {
                    OutputFormat::Json => {
                        output_json_reading(timestamp, dev, reading.temperature, soc);
                    }
                    OutputFormat::Human => {
                        let soc_info = soc.map_or(String::new(), |s| {
                            format!(
                                " | SoC: {:5.2}°C | ΔT: {:+5.2}°C",
                                s.temperature().celsius(),
                                s.delta_from_chassis(reading.temperature)
                            )
                        });
                        println!("[{timestamp}] {reading}{soc_info}");

                        if let Some(raw) = raw_payload {
                            println!("    [Raw Payload]: {}", raw.trim_end().replace('\n', " | "));
                        }
                    }
                }
            }
            Err(err) => {
                stats.record_error(&err);
                match opts.common.format {
                    OutputFormat::Json => {
                        output_json_error(timestamp, Some(dev), &err.summary());
                    }
                    OutputFormat::Human => {
                        eprintln!("[{timestamp}] [{dev}] Read error: {err}");
                    }
                }
            }
        }
    }
}

/// Filters discovered sensors according to CLI configuration.
///
/// # Errors
///
/// Returns an error string if sensor discovery fails or the targeted sensor is not present.
pub fn select_target_devices(
    sensor: &TempSensor,
    target: Option<&SensorId>,
) -> Result<Vec<SensorId>, String> {
    let devices = sensor.discover_sensors().map_err(|e| e.summary())?;
    if let Some(target) = target {
        if devices.contains(target) {
            Ok(vec![target.clone()])
        } else {
            Err(format!(
                "target sensor '{target}' not discovered on 1-Wire bus"
            ))
        }
    } else {
        Ok(devices)
    }
}

/// Runs the complete temperature diagnostic monitoring session.
#[must_use]
pub fn run_app(sensor: &TempSensor, opts: &CliOptions) -> ExitCode {
    if opts.common.format == OutputFormat::Human {
        print_header(sensor);
    }

    let target_devices = match select_target_devices(sensor, opts.target_sensor.as_ref()) {
        Ok(devs) => devs,
        Err(err) => {
            if opts.common.format == OutputFormat::Json {
                output_json_error(&format_timestamp(), opts.target_sensor.as_ref(), &err);
            } else {
                eprintln!("Error: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    if opts.common.format == OutputFormat::Human {
        println!("Discovered {} sensor(s):", target_devices.len());
        for dev in &target_devices {
            println!("  • {dev}");
        }
        if opts.common.verbose {
            print_diagnostic_registers(sensor, &target_devices);
        }
        if opts.common.max_count.is_none() {
            println!("\nPress Ctrl+C to stop.\n");
        } else {
            println!();
        }
    }

    let mut stats = Statistics::default();

    run_poll_loop(opts.common.interval, |timestamp| {
        let soc = if opts.include_soc {
            SocThermal::read().ok()
        } else {
            None
        };
        handle_sensor_iteration(sensor, &target_devices, opts, timestamp, soc, &mut stats);
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

/// Standalone CLI binary entrypoint for `temp-read`.
#[must_use]
pub fn main() -> ExitCode {
    match resolve_cli_opts(parse_cli_args(), "temp-read", print_usage) {
        Ok(opts) => run_app(&TempSensor::new(), &opts),
        Err(code) => code,
    }
}
