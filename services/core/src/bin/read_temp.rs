//! DS18B20 1-Wire temperature diagnostic utility.
//!
//! Provides CLI diagnostics for the 1-Wire temperature sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! discovers attached `28-*` DS18B20 digital thermometers, and reports
//! live chassis temperature telemetry, `SoC` core thermal metrics, and sensor hardware
//! diagnostic registers.

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use speaker_core::diag::{
    CommonCliOptions, NumericStats, OutputFormat, env_args, exit_code_for_reads, fahrenheit,
    format_timestamp, next_arg_value, parse_args_with_common, resolve_cli_opts,
    should_print_summary,
};
use speaker_core::hardware::{
    SensorDiagnostics, SensorId, SocThermal, TempError, TempSensor, Temperature,
};

/// Command-line configuration parameters.
#[derive(Debug, Clone)]
struct CliOptions {
    common: CommonCliOptions,
    include_soc: bool,
    show_raw: bool,
    target_sensor: Option<SensorId>,
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

fn print_usage(bin_name: &str) {
    println!("DS18B20 1-Wire Temperature Diagnostic Utility");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -1, --once               Read once and exit");
    println!("  -n, --count <COUNT>      Read COUNT times and exit");
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

fn parse_cli_args() -> Result<Option<CliOptions>, String> {
    parse_cli_args_from(env_args())
}

/// Running statistical summary.
#[derive(Debug, Default)]
struct Statistics {
    samples: NumericStats,
    crc_errors: usize,
    other_errors: usize,
}

impl Statistics {
    fn record_success(&mut self, temp: Temperature) {
        self.samples.record_success(temp.celsius());
    }

    fn record_error(&mut self, err: &TempError) {
        self.samples.record_error();
        if err.is_crc_mismatch() {
            self.crc_errors += 1;
        } else {
            self.other_errors += 1;
        }
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
    if let (Some(min_c), Some(max_c), Some(avg)) = (s.min(), s.max(), s.average()) {
        println!("Chassis Min : {min_c:.2}°C ({:.2}°F)", fahrenheit(min_c));
        println!("Chassis Max : {max_c:.2}°C ({:.2}°F)", fahrenheit(max_c));
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

fn output_json_error(timestamp: &str, sensor_id: Option<&SensorId>, err_msg: &str) {
    println!(
        "{}",
        serde_json::json!({
            "timestamp": timestamp,
            "sensor_id": sensor_id.map(SensorId::as_str),
            "error": err_msg,
        })
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
                match opts.common.format {
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

                        if opts.show_raw {
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

fn select_target_devices(
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

fn run_app(sensor: &TempSensor, opts: &CliOptions) -> ExitCode {
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

    loop {
        let loop_start = Instant::now();
        let timestamp = format_timestamp();
        let soc = if opts.include_soc {
            SocThermal::read().ok()
        } else {
            None
        };

        handle_sensor_iteration(sensor, &target_devices, opts, &timestamp, soc, &mut stats);

        if let Some(limit) = opts.common.max_count {
            if stats.samples.total_reads() >= limit {
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
    match resolve_cli_opts(parse_cli_args(), "read_temp", print_usage) {
        Ok(opts) => run_app(&TempSensor::new(), &opts),
        Err(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use speaker_core::diag::ScopedTempDir;

    #[test]
    fn print_usage_runs_cleanly() {
        print_usage("read_temp");
    }

    #[test]
    fn statistics_computations_and_summary() {
        let mut stats = Statistics::default();
        assert!(stats.samples.average().is_none());

        stats.record_success(Temperature::from_celsius(20.0).unwrap());
        stats.record_success(Temperature::from_celsius(30.0).unwrap());

        let crc_err =
            speaker_core::hardware::parse_w1_slave("00 : crc=00 NO\n00 t=1000\n").unwrap_err();
        stats.record_error(&crc_err);

        let io_err = TempError::from(std::io::Error::other("bus down"));
        stats.record_error(&io_err);

        assert_eq!(stats.samples.total_reads(), 4);
        assert_eq!(stats.samples.successful_reads(), 2);
        assert_eq!(stats.crc_errors, 1);
        assert_eq!(stats.other_errors, 1);
        assert_eq!(stats.samples.min(), Some(20.0));
        assert_eq!(stats.samples.max(), Some(30.0));
        assert!((stats.samples.average().unwrap() - 25.0).abs() < 1e-4);

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

        let temp_dir = ScopedTempDir::new("read_temp_bin_test");
        let soc_file = temp_dir.path().join("temp");
        std::fs::write(&soc_file, "42000\n").unwrap();
        let soc = SocThermal::read_from_path(&soc_file).ok();

        let opts_human = CliOptions {
            common: CommonCliOptions {
                max_count: Some(1),
                interval: Duration::from_millis(10),
                format: OutputFormat::Human,
                verbose: true,
            },
            include_soc: true,
            show_raw: true,
            ..CliOptions::default()
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
        assert_eq!(stats.samples.successful_reads(), 1);

        // JSON format with SoC
        let opts_json = CliOptions {
            common: CommonCliOptions {
                format: OutputFormat::Json,
                ..opts_human.common
            },
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
        assert_eq!(stats.samples.successful_reads(), 2);

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
    }

    #[test]
    fn run_app_iterations_and_failure_modes() {
        let (sensor, mock) = TempSensor::new_mocked();
        let id = SensorId::parse("28-000000000001").unwrap();
        mock.set_temperature(id.clone(), Temperature::from_celsius(24.5).unwrap());

        // Test run_app in human mode with iterations
        let opts_run = CliOptions {
            common: CommonCliOptions {
                max_count: Some(2),
                interval: Duration::from_millis(1),
                format: OutputFormat::Human,
                verbose: true,
            },
            ..CliOptions::default()
        };
        assert_eq!(run_app(&sensor, &opts_run), ExitCode::SUCCESS);

        // Test run_app in JSON mode
        let opts_run_json = CliOptions {
            common: CommonCliOptions {
                max_count: Some(1),
                interval: Duration::from_millis(1),
                format: OutputFormat::Json,
                verbose: false,
            },
            target_sensor: Some(id),
            ..CliOptions::default()
        };
        assert_eq!(run_app(&sensor, &opts_run_json), ExitCode::SUCCESS);

        // Test run_app error when sensor not found
        let bad_sensor_id = SensorId::parse("28-000000000999").unwrap();
        let opts_fail = CliOptions {
            common: CommonCliOptions {
                max_count: Some(1),
                ..opts_run.common
            },
            target_sensor: Some(bad_sensor_id.clone()),
            ..CliOptions::default()
        };
        assert_eq!(run_app(&sensor, &opts_fail), ExitCode::FAILURE);

        let opts_fail_json = CliOptions {
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

        assert_eq!(opts.common.max_count, Some(3));
        assert!(opts.common.verbose);
        assert!(opts.include_soc);
        assert!(opts.show_raw);
        assert_eq!(opts.common.format, OutputFormat::Json);
        assert_eq!(
            opts.target_sensor,
            Some(SensorId::parse("28-000000000001").unwrap())
        );

        // Errors
        for (bad_args, expected_msg) in [
            (&["--count"][..], "missing value for --count"),
            (&["--count", "abc"][..], "invalid count"),
            (&["--interval"][..], "missing value for --interval"),
            (
                &["--interval", "0"][..],
                "interval must be a finite value greater than 0",
            ),
            (&["--device"][..], "missing value for --device"),
            (&["--device", "bad-id"][..], "invalid DS18B20 sensor ID"),
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
