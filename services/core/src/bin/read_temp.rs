//! DS18B20 1-Wire temperature diagnostic utility.
//!
//! Provides CLI diagnostics for the 1-Wire temperature sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! discovers attached `28-*` DS18B20 digital thermometers, and reports
//! live chassis temperature telemetry, `SoC` core thermal metrics, and sensor hardware
//! diagnostic registers.

use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use speaker_core::hardware::{SensorDiagnostics, SensorId, SocThermal, TempSensor, Temperature};

/// Formats UTC wall-clock time as `HH:MM:SS`.
fn format_timestamp() -> String {
    let total_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
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

/// Flags controlling optional diagnostic telemetry fields.
#[derive(Debug, Clone, Copy, Default)]
struct DiagnosticFlags {
    verbose: bool,
    include_soc: bool,
    show_raw: bool,
}

/// Command-line configuration parameters.
#[derive(Debug, Clone)]
struct CliOptions {
    max_count: Option<usize>,
    interval: Duration,
    format: OutputFormat,
    flags: DiagnosticFlags,
    target_sensor: Option<SensorId>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            max_count: None,
            interval: Duration::from_secs(1),
            format: OutputFormat::Human,
            flags: DiagnosticFlags::default(),
            target_sensor: None,
        }
    }
}

fn print_usage(bin_name: &str) {
    println!("DS18B20 1-Wire Temperature Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit");
    println!("  -n, --count <COUNT>      Read COUNT times and exit");
    println!("  -i, --interval <SECS>    Polling interval in seconds (default: 1.0)");
    println!("  -v, --verbose            Show detailed sensor registers & diagnostics");
    println!("  -s, --soc                Include BCM2837 SoC core temperature & delta");
    println!("  -r, --raw                Display raw w1_slave scratchpad payload");
    println!("  -d, --device <ID>        Target specific sensor (e.g. 28-01201b7d5396)");
    println!("  -j, --json               Output readings as JSON lines");
    println!("  -h, --help               Show this help message and exit");
    println!();
    println!("Examples:");
    println!("  {bin_name} --once --verbose");
    println!("  {bin_name} --soc -i 2");
    println!("  {bin_name} --count 5 --json");
}

fn parse_cli_args() -> Result<Option<CliOptions>, String> {
    let mut args = env::args().skip(1);
    let mut opts = CliOptions::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-1" | "--once" => opts.max_count = Some(1),
            "-v" | "--verbose" => opts.flags.verbose = true,
            "-s" | "--soc" => opts.flags.include_soc = true,
            "-r" | "--raw" => opts.flags.show_raw = true,
            "-j" | "--json" => opts.format = OutputFormat::Json,
            "-n" | "--count" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --count".into());
                };
                let count = val
                    .parse::<usize>()
                    .map_err(|e| format!("invalid count '{val}': {e}"))?;
                opts.max_count = Some(count);
            }
            "-i" | "--interval" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --interval".into());
                };
                let secs = val
                    .parse::<f32>()
                    .map_err(|e| format!("invalid interval '{val}': {e}"))?;
                if secs <= 0.0 {
                    return Err("interval must be greater than 0".into());
                }
                opts.interval = Duration::from_secs_f32(secs);
            }
            "-d" | "--device" => {
                let Some(val) = args.next() else {
                    return Err("missing value for --device".into());
                };
                let sensor_id =
                    SensorId::parse(&val).map_err(|e| format!("invalid sensor ID '{val}': {e}"))?;
                opts.target_sensor = Some(sensor_id);
            }
            unknown => return Err(format!("unknown option '{unknown}', use --help for usage")),
        }
    }

    Ok(Some(opts))
}

/// Running statistical summary.
#[derive(Debug, Default)]
struct Statistics {
    total_reads: usize,
    crc_errors: usize,
    min_temp: Option<Temperature>,
    max_temp: Option<Temperature>,
    temp_sum_celsius: f64,
}

impl Statistics {
    fn record_success(&mut self, temp: Temperature) {
        self.total_reads += 1;
        self.temp_sum_celsius += f64::from(temp.celsius());
        self.min_temp = Some(self.min_temp.map_or(temp, |cur| cur.min(temp)));
        self.max_temp = Some(self.max_temp.map_or(temp, |cur| cur.max(temp)));
    }

    fn record_crc_error(&mut self) {
        self.total_reads += 1;
        self.crc_errors += 1;
    }

    fn average_celsius(&self) -> Option<f64> {
        let valid_reads = self.total_reads - self.crc_errors;
        (valid_reads > 0).then(|| self.temp_sum_celsius / valid_reads as f64)
    }
}

fn print_header(sensor: &TempSensor) {
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

fn print_diagnostic_registers(sensor: &TempSensor, devices: &[SensorId]) {
    println!("\nExtended Hardware Registers:");
    for dev in devices {
        match sensor.read_diagnostics(dev) {
            Ok(diag) => println!("{diag}"),
            Err(err) => eprintln!("  [!] Failed to read diagnostics for {dev}: {err}"),
        }
    }
}

fn print_session_summary(stats: &Statistics) {
    println!("\n--- Diagnostic Session Summary ---");
    println!("Total reads : {}", stats.total_reads);
    println!(
        "CRC errors  : {} ({:.2}% error rate)",
        stats.crc_errors,
        (stats.crc_errors as f32 / stats.total_reads as f32) * 100.0
    );
    if let (Some(min), Some(max), Some(avg)) =
        (stats.min_temp, stats.max_temp, stats.average_celsius())
    {
        println!(
            "Chassis Min : {:.2}°C ({:.2}°F)",
            min.celsius(),
            min.fahrenheit()
        );
        println!(
            "Chassis Max : {:.2}°C ({:.2}°F)",
            max.celsius(),
            max.fahrenheit()
        );
        println!("Chassis Avg : {avg:.2}°C");
    }
}

fn output_json_reading(
    timestamp: &str,
    dev: &SensorId,
    temp: Temperature,
    soc: Option<SocThermal>,
) {
    let soc_json = soc.map_or("null".to_string(), |s| {
        format!(
            r#"{{"celsius":{:.2},"fahrenheit":{:.2}}}"#,
            s.temperature().celsius(),
            s.temperature().fahrenheit()
        )
    });
    let delta_json = soc.map_or("null".to_string(), |s| {
        format!("{:.2}", s.delta_from_chassis(temp))
    });

    println!(
        r#"{{"timestamp":"{timestamp}","sensor_id":"{dev}","temperature":{{"celsius":{:.2},"fahrenheit":{:.2},"millidegrees":{}}},"soc_temperature":{soc_json},"delta_celsius":{delta_json}}}"#,
        temp.celsius(),
        temp.fahrenheit(),
        temp.millidegrees_celsius()
    );
}

fn handle_sensor_iteration(
    sensor: &TempSensor,
    devices: &[SensorId],
    opts: &CliOptions,
    timestamp: &str,
    soc: Option<SocThermal>,
    stats: &mut Statistics,
) {
    for dev in devices {
        match sensor.read_sensor(dev) {
            Ok(reading) => {
                stats.record_success(reading.temperature);
                match opts.format {
                    OutputFormat::Json => {
                        output_json_reading(timestamp, dev, reading.temperature, soc);
                    }
                    OutputFormat::Human => {
                        let soc_info = soc.map_or(String::new(), |s| {
                            format!(
                                " | SoC: {:5.2}°C ({:5.2}°F) | ΔT: {:+5.2}°C",
                                s.temperature().celsius(),
                                s.temperature().fahrenheit(),
                                s.delta_from_chassis(reading.temperature)
                            )
                        });
                        println!("[{timestamp}] {reading}{soc_info}");

                        if opts.flags.show_raw {
                            if let Ok(SensorDiagnostics { raw_payload, .. }) =
                                sensor.read_diagnostics(dev)
                            {
                                println!(
                                    "    [Raw Payload]: {}",
                                    raw_payload.trim_end().replace('\n', " | ")
                                );
                            }
                        }
                    }
                }
            }
            Err(err) => {
                stats.record_crc_error();
                match opts.format {
                    OutputFormat::Json => {
                        println!(
                            r#"{{"timestamp":"{timestamp}","sensor_id":"{dev}","error":"{err}"}}"#
                        );
                    }
                    OutputFormat::Human => {
                        println!("[{timestamp}] [{dev}] Read error: {err}");
                    }
                }
            }
        }
    }
}

fn select_target_devices(
    sensor: &TempSensor,
    target: Option<&SensorId>,
) -> Result<Vec<SensorId>, String> {
    let devices = sensor.discover_sensors().map_err(|e| e.to_string())?;
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

fn main() -> ExitCode {
    let opts = match parse_cli_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            let bin_name = env::args().next().unwrap_or_else(|| "read_temp".into());
            print_usage(&bin_name);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("Error: {err}\nRun with --help for usage information.");
            return ExitCode::FAILURE;
        }
    };

    let sensor = TempSensor::new();
    if opts.format == OutputFormat::Human {
        print_header(&sensor);
    }

    let target_devices = match select_target_devices(&sensor, opts.target_sensor.as_ref()) {
        Ok(devs) => devs,
        Err(err) => {
            if opts.format == OutputFormat::Json {
                println!(r#"{{"error":"{err}"}}"#);
            } else {
                eprintln!("Error: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    if opts.format == OutputFormat::Human {
        println!("Discovered {} sensor(s):", target_devices.len());
        for dev in &target_devices {
            println!("  • {dev}");
        }
        if opts.flags.verbose {
            print_diagnostic_registers(&sensor, &target_devices);
        }
        if opts.max_count.is_none() {
            println!("\nPress Ctrl+C to stop.\n");
        } else {
            println!();
        }
    }

    let mut stats = Statistics::default();
    let mut iteration = 0usize;

    loop {
        let loop_start = Instant::now();
        let timestamp = format_timestamp();
        let soc = if opts.flags.include_soc {
            SocThermal::read().ok()
        } else {
            None
        };

        handle_sensor_iteration(&sensor, &target_devices, &opts, &timestamp, soc, &mut stats);

        iteration += 1;
        if let Some(limit) = opts.max_count {
            if iteration >= limit {
                break;
            }
        }

        let elapsed = loop_start.elapsed();
        if let Some(remaining) = opts.interval.checked_sub(elapsed) {
            thread::sleep(remaining);
        }
    }

    if opts.format == OutputFormat::Human && stats.total_reads > 1 {
        print_session_summary(&stats);
    }

    ExitCode::SUCCESS
}
