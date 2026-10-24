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

use speaker_core::hardware::{
    SensorDiagnostics, SensorId, SocThermal, TempError, TempSensor, Temperature,
};

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

fn parse_cli_args() -> Result<Option<CliOptions>, String> {
    parse_cli_args_from(env::args().skip(1))
}

/// Running statistical summary.
#[derive(Debug, Default)]
struct Statistics {
    total_reads: usize,
    successful_reads: usize,
    crc_errors: usize,
    other_errors: usize,
    min_temp: Option<Temperature>,
    max_temp: Option<Temperature>,
    temp_sum_celsius: f64,
}

impl Statistics {
    fn record_success(&mut self, temp: Temperature) {
        self.total_reads += 1;
        self.successful_reads += 1;
        self.temp_sum_celsius += f64::from(temp.celsius());
        self.min_temp = Some(self.min_temp.map_or(temp, |cur| cur.min(temp)));
        self.max_temp = Some(self.max_temp.map_or(temp, |cur| cur.max(temp)));
    }

    fn record_error(&mut self, err: &TempError) {
        self.total_reads += 1;
        if err.is_crc_mismatch() {
            self.crc_errors += 1;
        } else {
            self.other_errors += 1;
        }
    }

    fn average_celsius(&self) -> Option<f64> {
        (self.successful_reads > 0).then(|| self.temp_sum_celsius / self.successful_reads as f64)
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
    println!("Successful  : {}", stats.successful_reads);
    if stats.total_reads > 0 {
        let total = stats.total_reads as f32;
        println!(
            "CRC errors  : {} ({:.2}% error rate)",
            stats.crc_errors,
            (stats.crc_errors as f32 / total) * 100.0
        );
        if stats.other_errors > 0 {
            println!(
                "Other errors: {} ({:.2}% error rate)",
                stats.other_errors,
                (stats.other_errors as f32 / total) * 100.0
            );
        }
    } else {
        println!("CRC errors  : 0 (0.00% error rate)");
    }
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
                stats.record_error(&err);
                match opts.format {
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "timestamp": timestamp,
                                "sensor_id": dev.as_str(),
                                "error": err.to_string(),
                            })
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

fn run_app(sensor: &TempSensor, opts: &CliOptions) -> ExitCode {
    if opts.format == OutputFormat::Human {
        print_header(sensor);
    }

    let target_devices = match select_target_devices(sensor, opts.target_sensor.as_ref()) {
        Ok(devs) => devs,
        Err(err) => {
            if opts.format == OutputFormat::Json {
                println!("{}", serde_json::json!({ "error": err }));
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
            print_diagnostic_registers(sensor, &target_devices);
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

        handle_sensor_iteration(sensor, &target_devices, opts, &timestamp, soc, &mut stats);

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
    fn print_usage_runs_cleanly() {
        print_usage("read_temp");
    }

    #[test]
    fn statistics_computations_and_summary() {
        let mut stats = Statistics::default();
        assert!(stats.average_celsius().is_none());

        stats.record_success(Temperature::from_celsius(20.0).unwrap());
        stats.record_success(Temperature::from_celsius(30.0).unwrap());

        let crc_err =
            speaker_core::hardware::parse_w1_slave("00 : crc=00 NO\n00 t=1000\n").unwrap_err();
        stats.record_error(&crc_err);

        let io_err = TempError::from(std::io::Error::other("bus down"));
        stats.record_error(&io_err);

        assert_eq!(stats.total_reads, 4);
        assert_eq!(stats.successful_reads, 2);
        assert_eq!(stats.crc_errors, 1);
        assert_eq!(stats.other_errors, 1);
        assert_eq!(stats.min_temp.unwrap().celsius(), 20.0);
        assert_eq!(stats.max_temp.unwrap().celsius(), 30.0);
        assert!((stats.average_celsius().unwrap() - 25.0).abs() < 1e-4);

        print_session_summary(&stats);

        let empty_stats = Statistics::default();
        print_session_summary(&empty_stats);
    }

    #[test]
    fn header_diagnostics_and_iterations_with_mock() {
        let (sensor, mock) = TempSensor::new_mocked();
        let id = SensorId::parse("28-000000000001").unwrap();
        mock.set_temperature(id.clone(), Temperature::from_celsius(24.5).unwrap());

        print_header(&sensor);
        print_diagnostic_registers(&sensor, std::slice::from_ref(&id));

        // Error path for diagnostic registers
        let bad_id = SensorId::parse("28-000000000099").unwrap();
        print_diagnostic_registers(&sensor, &[bad_id]);

        let temp_dir =
            std::env::temp_dir().join(format!("read_temp_bin_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let soc_file = temp_dir.join("temp");
        std::fs::write(&soc_file, "42000\n").unwrap();
        let soc = SocThermal::read_from_path(&soc_file).ok();

        let opts_human = CliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(10),
            format: OutputFormat::Human,
            flags: DiagnosticFlags {
                verbose: true,
                include_soc: true,
                show_raw: true,
            },
            target_sensor: None,
        };

        let mut stats = Statistics::default();
        handle_sensor_iteration(
            &sensor,
            std::slice::from_ref(&id),
            &opts_human,
            "12:00:00",
            soc,
            &mut stats,
        );
        assert_eq!(stats.successful_reads, 1);

        // JSON format with SoC
        let opts_json = CliOptions {
            format: OutputFormat::Json,
            ..opts_human.clone()
        };
        handle_sensor_iteration(
            &sensor,
            std::slice::from_ref(&id),
            &opts_json,
            "12:00:01",
            soc,
            &mut stats,
        );
        assert_eq!(stats.successful_reads, 2);

        // Injected error iteration
        mock.set_io_error("device removed");
        handle_sensor_iteration(
            &sensor,
            std::slice::from_ref(&id),
            &opts_human,
            "12:00:02",
            None,
            &mut stats,
        );
        handle_sensor_iteration(
            &sensor,
            std::slice::from_ref(&id),
            &opts_json,
            "12:00:03",
            None,
            &mut stats,
        );
        assert_eq!(stats.other_errors, 2);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn run_app_iterations_and_failure_modes() {
        let (sensor, mock) = TempSensor::new_mocked();
        let id = SensorId::parse("28-000000000001").unwrap();
        mock.set_temperature(id.clone(), Temperature::from_celsius(24.5).unwrap());

        // Test run_app in human mode with iterations
        let opts_run = CliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            format: OutputFormat::Human,
            flags: DiagnosticFlags {
                verbose: true,
                include_soc: false,
                show_raw: false,
            },
            target_sensor: None,
        };
        assert_eq!(run_app(&sensor, &opts_run), ExitCode::SUCCESS);

        // Test run_app in JSON mode
        let opts_run_json = CliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(1),
            format: OutputFormat::Json,
            flags: DiagnosticFlags::default(),
            target_sensor: Some(id),
        };
        assert_eq!(run_app(&sensor, &opts_run_json), ExitCode::SUCCESS);

        // Test run_app error when sensor not found
        let bad_sensor_id = SensorId::parse("28-000000000999").unwrap();
        let opts_fail = CliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(1),
            format: OutputFormat::Human,
            flags: DiagnosticFlags::default(),
            target_sensor: Some(bad_sensor_id.clone()),
        };
        assert_eq!(run_app(&sensor, &opts_fail), ExitCode::FAILURE);

        let opts_fail_json = CliOptions {
            format: OutputFormat::Json,
            target_sensor: Some(bad_sensor_id),
            ..opts_run_json
        };
        assert_eq!(run_app(&sensor, &opts_fail_json), ExitCode::FAILURE);
    }

    #[test]
    fn parse_cli_args_flags_and_validation() {
        print_usage("read_temp_test");

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
            "--soc".into(),
            "--raw".into(),
            "--json".into(),
            "--count".into(),
            "3".into(),
            "--interval".into(),
            "2.5".into(),
            "--device".into(),
            "28-000000000001".into(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(opts.max_count, Some(3));
        assert!(opts.flags.verbose);
        assert!(opts.flags.include_soc);
        assert!(opts.flags.show_raw);
        assert_eq!(opts.format, OutputFormat::Json);
        assert_eq!(
            opts.target_sensor,
            Some(SensorId::parse("28-000000000001").unwrap())
        );

        // Errors
        let _ = parse_cli_args_from(["--count".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--count".into(), "abc".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--interval".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--interval".into(), "0".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--device".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--device".into(), "bad-id".into()]).unwrap_err();
        let _ = parse_cli_args_from(["--unknown".into()]).unwrap_err();
    }
}
