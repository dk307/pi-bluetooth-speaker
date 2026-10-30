//! Integration and CLI binary tests for core executables and diagnostics.
//!
//! Verifies CLI flag parsing, help text generation, invalid argument rejection,
//! and error message formatting across standalone binaries without depending on
//! physical Raspberry Pi hardware peripherals or live sysfs paths.
//!
//! Note that application execution, JSON serialization, and hardware fault injection
//! are unit-tested via `run_app` entrypoints using mock backends in `src/bin/temp-read.rs`
//! and `src/bin/light-read.rs`.

use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Maximum wait time for fast CLI probes (`--help` and argument validation).
///
/// Generous 10s bound accommodates emulated CI runners while guarding against infinite loops.
const CLI_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum wait time for the `core` binary diagnostic startup banner.
///
/// Generous 30s bound prevents false-positive timeouts under heavy CI load.
const CORE_MAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling interval while waiting for a spawned child process to exit.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Waits for a spawned child process up to `timeout`, killing the process and panicking if it hangs.
///
/// Invariant: all probed CLI binaries emit bounded output (< 4 KiB, well within the 64 KiB OS pipe
/// buffer), so polling `try_wait()` before `wait_with_output()` cannot deadlock on pipe writes.
fn wait_with_deadline(mut child: Child, timeout: Duration, context: &str) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .unwrap_or_else(|e| panic!("failed to collect output for '{context}': {e}"));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(WAIT_POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess '{context}' timed out after {timeout:?}");
            }
            Err(e) => panic!("wait failed for '{context}': {e}"),
        }
    }
}

/// Spawns `bin` with `args` and waits up to `timeout`, panicking if spawning fails or times out.
fn run_cli_with_timeout(
    bin: &str,
    args: &[&str],
    timeout: Duration,
) -> (ExitStatus, String, String) {
    let child = Command::new(bin)
        .env("SPEAKER_CORE_HEADLESS", "1")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn '{bin}' with args {args:?}: {e}"));
    let output = wait_with_deadline(child, timeout, bin);
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Executes a binary with arguments under `CLI_PROBE_TIMEOUT` and captures status, stdout, and stderr.
fn run_cli(bin: &str, args: &[&str]) -> (ExitStatus, String, String) {
    run_cli_with_timeout(bin, args, CLI_PROBE_TIMEOUT)
}

#[test]
fn core_main_binary_executes() {
    // Note: core binary currently executes an initial diagnostic banner and exits.
    // The banner prints static GPIO/hardware pin metadata via `TempSensor::gpio_info()`
    // which does not perform live sysfs I/O, ensuring deterministic execution on host.
    let bin_path = env!("CARGO_BIN_EXE_core");
    let (status, stdout, _) = run_cli_with_timeout(bin_path, &[], CORE_MAIN_TIMEOUT);

    assert!(status.success());
    assert!(stdout.contains("Speaker Core Service is running on Raspberry Pi 3!"));
    assert!(stdout.contains("ARM Cortex-A53 (aarch64)"));
    assert!(stdout.contains("Thermal Sensor:"));

    // Test help flags
    for flag in &["-h", "--help"] {
        let (status, stdout, _) = run_cli(bin_path, &[flag]);
        assert!(status.success());
        assert!(stdout.contains("DietPi Bluetooth Speaker & DSP Core Appliance Service"));
        assert!(stdout.contains("--preview"));
    }

    // Test version flags
    for flag in &["-V", "--version"] {
        let (status, stdout, _) = run_cli(bin_path, &[flag]);
        assert!(status.success());
        assert!(stdout.contains(&format!("core {}", env!("CARGO_PKG_VERSION"))));
    }

    // Test zero duration preview
    let (status, stdout, _) = run_cli(bin_path, &["-p", "0"]);
    assert!(status.success());
    assert!(stdout.contains("Speaker Core Service"));

    // Test error cases (clean, backtrace-free user-facing rejections)
    for (args, expected) in [
        (
            &["--preview", "invalid"][..],
            "invalid preview seconds 'invalid'",
        ),
        (&["--preview", "+5"][..], "sign prefixes are not accepted"),
        (&["--preview", "-5"][..], "sign prefixes are not accepted"),
        (&["--preview"][..], "missing value for --preview"),
        (&["--unknown-flag"][..], "unknown option '--unknown-flag'"),
    ] {
        let (status, _, stderr) = run_cli(bin_path, args);
        assert!(!status.success());
        assert!(stderr.contains(expected), "stderr: {stderr}");
        assert!(!stderr.contains("Backtrace") && !stderr.contains("panicked"));
    }
}

const HELP_CASES: &[(&str, &str, &str)] = &[
    (
        env!("CARGO_BIN_EXE_temp-read"),
        "DS18B20 1-Wire Temperature Diagnostic Utility",
        "--repeat",
    ),
    (
        env!("CARGO_BIN_EXE_light-read"),
        "BH1750 Ambient Light Diagnostic Utility",
        "--repeat",
    ),
    (
        env!("CARGO_BIN_EXE_amp-info"),
        "MA12070P Amplifier Diagnostic Utility",
        "--repeat",
    ),
    (
        env!("CARGO_BIN_EXE_bt-info"),
        "Bluetooth & BlueALSA Diagnostic Utility",
        "--repeat",
    ),
    (
        env!("CARGO_BIN_EXE_encoder-read"),
        "EC12 Rotary Encoder Diagnostic Utility",
        "--repeat",
    ),
    (
        env!("CARGO_BIN_EXE_dsp-info"),
        "CamillaDSP Pipeline Diagnostic Utility",
        "--repeat",
    ),
];

/// Shared `--version` semantic version probe cases for all diagnostic utilities.
const VERSION_CASES: &[(&str, &str)] = &[
    (env!("CARGO_BIN_EXE_temp-read"), "temp-read"),
    (env!("CARGO_BIN_EXE_light-read"), "light-read"),
    (env!("CARGO_BIN_EXE_amp-info"), "amp-info"),
    (env!("CARGO_BIN_EXE_bt-info"), "bt-info"),
    (env!("CARGO_BIN_EXE_encoder-read"), "encoder-read"),
    (env!("CARGO_BIN_EXE_dsp-info"), "dsp-info"),
];

#[test]
fn diagnostic_utilities_version_display() {
    for &(bin_path, expected_name) in VERSION_CASES {
        for flag in ["--version", "-V"] {
            let (status, stdout, _) = run_cli(bin_path, &[flag]);
            assert!(status.success(), "expected success for '{bin_path} {flag}'");
            let expected = format!("{expected_name} {}", env!("CARGO_PKG_VERSION"));
            assert!(
                stdout.trim().ends_with(&expected),
                "expected stdout to end with '{expected}', got: '{stdout}'"
            );
        }
    }
}

#[test]
fn diagnostic_utilities_once_is_the_default() {
    // With no arguments each utility must perform a single bounded read and exit
    // successfully instead of entering continuous polling mode.
    for &(bin_path, expected_name) in VERSION_CASES {
        let (_, stdout, _) = run_cli(bin_path, &[]);
        assert!(
            !stdout.contains("Press Ctrl+C to stop"),
            "'{expected_name}' entered repeat mode by default"
        );
    }
}

#[test]
fn diagnostic_utilities_help_display() {
    for &(bin_path, expected_title, expected_flag) in HELP_CASES {
        for flag in ["--help", "-h"] {
            let (status, stdout, _) = run_cli(bin_path, &[flag]);
            assert!(status.success(), "expected success for '{bin_path} {flag}'");
            assert!(stdout.contains(expected_title));
            assert!(stdout.contains("Usage:"));
            assert!(stdout.contains(expected_flag));
        }
    }
}

/// Shared invalid CLI flag cases common to all diagnostic utilities.
const COMMON_INVALID_ARGS: &[(&[&str], &str)] = &[
    (&["--invalid-flag"], "unknown option '--invalid-flag'"),
    (&["--count"], "missing value for --count"),
    (&["--count", "not-a-number"], "invalid count 'not-a-number'"),
    (&["--count", "-1"], "invalid count '-1'"),
    (&["--count", "0"], "--count must be at least 1"),
    (
        &["--count", "9999999999999999999999999999999999999999"],
        "invalid count",
    ),
    (&["--interval"], "missing value for --interval"),
    (
        &["--interval", "0"],
        "interval must be a finite value greater than 0",
    ),
    (&["--interval", "invalid"], "invalid interval 'invalid'"),
    (
        &["--interval", "-1.0"],
        "interval must be a finite value greater than 0",
    ),
    (
        &["--interval", "1e-400"],
        "interval must be a finite value greater than 0",
    ),
    (
        &["--interval", "1e400"],
        "interval must be a finite value greater than 0",
    ),
    (
        &["--interval", "nan"],
        "interval must be a finite value greater than 0",
    ),
];

fn assert_cli_rejects(bin_path: &str, cases: &[(&[&str], &str)]) {
    for (args, expected_err) in cases {
        let (status, _, stderr) = run_cli(bin_path, args);
        assert!(!status.success(), "expected failure for args: {args:?}");
        assert!(
            stderr.contains(expected_err),
            "expected stderr to contain '{expected_err}' for args {args:?}, got: '{stderr}'"
        );
        assert!(
            !stderr.contains("Backtrace") && !stderr.contains("panicked"),
            "expected backtrace-free user error, got: '{stderr}'"
        );
    }
}

#[test]
fn temp_read_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_temp-read");
    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
    assert_cli_rejects(
        bin_path,
        &[
            (&["--device"], "missing value for --device"),
            (
                &["--device", "invalid-device-id"],
                "invalid DS18B20 sensor ID",
            ),
        ],
    );
}

#[test]
fn light_read_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_light-read");
    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
    assert_cli_rejects(
        bin_path,
        &[
            (&["--address"], "missing value for --address"),
            (&["--address", "+0x23"], "sign characters are not allowed"),
            (&["--address", "-0x23"], "sign characters are not allowed"),
            (&["--address", "0x+23"], "sign characters are not allowed"),
            (&["--address", "0x-23"], "sign characters are not allowed"),
            (&["--address", "0X+5C"], "sign characters are not allowed"),
            (&["--address", "not-hex"], "invalid hex address"),
            (&["--address", "0xzz"], "invalid hex address"),
            (&["--address", "0x00"], "invalid BH1750 I2C address"),
            (&["--address", "0x42"], "invalid BH1750 I2C address"),
            (&["--address", "0x77"], "invalid BH1750 I2C address"),
            (&["--address", "0x80"], "invalid BH1750 I2C address"),
        ],
    );
}

#[test]
fn amp_info_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_amp-info");
    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
    assert_cli_rejects(
        bin_path,
        &[
            (&["--address"], "missing value for --address"),
            (&["--address", "+0x20"], "sign characters are not allowed"),
            (&["--address", "-0x20"], "sign characters are not allowed"),
            (&["--address", "0x+20"], "sign characters are not allowed"),
            (&["--address", "0x-20"], "sign characters are not allowed"),
            (&["--address", "0X+21"], "sign characters are not allowed"),
            (&["--address", "not-hex"], "invalid hex address"),
            (&["--address", "0xzz"], "invalid hex address"),
            (&["--address", "0x00"], "invalid MA12070P I2C address"),
            (&["--address", "0x19"], "invalid MA12070P I2C address"),
            (&["--address", "0x24"], "invalid MA12070P I2C address"),
            (&["--address", "0x5C"], "invalid MA12070P I2C address"),
        ],
    );
}

#[test]
fn bt_info_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_bt-info");
    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
    assert_cli_rejects(
        bin_path,
        &[
            (&["--adapter"], "missing value for --adapter"),
            (&["--device"], "missing value for --device"),
            (
                &["--device", "+AA:BB:CC:DD:EE:FF"],
                "sign characters are not allowed",
            ),
            (
                &["--device", "-AA:BB:CC:DD:EE:FF"],
                "sign characters are not allowed",
            ),
            (
                &["--device", "AA:+BB:CC:DD:EE:FF"],
                "sign characters are not allowed",
            ),
            (&["--device", "not-a-mac"], "expected 6 hex octets"),
            (
                &["--device", "invalid"],
                "expected 6 colon- or hyphen-separated hex octets",
            ),
            (&["--device", "AA:BB:CC:DD:EE"], "expected 6 hex octets"),
            (&["--power"], "missing value for --power"),
            (&["--power", "invalid"], "expected 'on' or 'off'"),
            (&["--discoverable"], "missing value for --discoverable"),
            (&["--discoverable", "invalid"], "expected 'on' or 'off'"),
            (&["--pairable"], "missing value for --pairable"),
            (&["--pairable", "invalid"], "expected 'on' or 'off'"),
        ],
    );
}

#[test]
fn encoder_read_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_encoder-read");
    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
}

#[test]
fn dsp_info_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_dsp-info");
    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
    assert_cli_rejects(
        bin_path,
        &[
            (&["--port"], "missing value for --port"),
            (&["-p"], "missing value for --port"),
            (&["--port", "0"], "port must be between 1 and 65535"),
            (
                &["--port", "70000"],
                "must be an unsigned integer 1..=65535",
            ),
            (
                &["--port", "+1234"],
                "signed prefixes ('+', '-') are not permitted",
            ),
            (
                &["--port", "-1234"],
                "signed prefixes ('+', '-') are not permitted",
            ),
            (&["--port", "abc"], "must be an unsigned integer 1..=65535"),
            (&["--host"], "missing value for --host"),
            (&["-H"], "missing value for --host"),
            (&["--host", ""], "host cannot be empty"),
            (&["--host", "bad host name"], "contains invalid characters"),
        ],
    );
}

#[test]
fn light_read_cli_argument_parsing_and_stats() {
    use speaker_core::cli::light::{
        CliOptions, DEFAULT_POLL_INTERVAL, parse_cli_args_from, print_session_summary,
    };
    use speaker_core::diag::{CliParse, NumericStats, OutputFormat, foot_candles};

    fn unwrap_options(parsed: Result<CliParse<CliOptions>, String>) -> CliOptions {
        match parsed.unwrap() {
            CliParse::Options(o) => o,
            other => panic!("expected CliParse::Options, got {other:?}"),
        }
    }

    // Diagnostics and stats
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

    let lines =
        speaker_core::diag::format_min_max_avg(&stats, "Light", "lx", Some((foot_candles, "fc")))
            .unwrap();
    assert_eq!(lines[0], "Light Min   :  100.00 lx (  9.29 fc)");
    assert_eq!(lines[1], "Light Max   :  200.00 lx ( 18.58 fc)");
    assert_eq!(lines[2], "Light Avg   :  150.00 lx ( 13.94 fc)");

    print_session_summary(&stats);
    let empty_stats = NumericStats::default();
    print_session_summary(&empty_stats);

    // Flag parsing
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

    let defaults = unwrap_options(parse_cli_args_from([]));
    assert_eq!(defaults.common.max_count, Some(1));
    assert_eq!(defaults.common.interval, DEFAULT_POLL_INTERVAL);

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
fn light_read_cli_mock_execution_and_fault_handling() {
    use std::process::ExitCode;

    use speaker_core::cli::light::{
        CliOptions, handle_light_iteration, print_diagnostic_registers, print_header, run_app,
    };
    use speaker_core::diag::{CommonCliOptions, NumericStats, OutputFormat};
    use speaker_core::hardware::{I2cAddress, LightSensor};

    let (sensor, mock) = LightSensor::new_mocked();
    print_header(&sensor);
    print_diagnostic_registers(&sensor);

    let run_opts = CliOptions {
        common: CommonCliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            verbose: true,
            format: OutputFormat::Human,
        },
        address: I2cAddress::default(),
    };
    assert_eq!(run_app(&sensor, &run_opts), ExitCode::SUCCESS);

    let json_opts = CliOptions {
        common: CommonCliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            verbose: false,
            format: OutputFormat::Json,
        },
        address: I2cAddress::default(),
    };
    assert_eq!(run_app(&sensor, &json_opts), ExitCode::SUCCESS);

    // Mock fault handling
    mock.set_fault(Some("I2C simulated bus lockup".into()));
    let mut err_stats = NumericStats::default();
    handle_light_iteration(&sensor, &run_opts, "12:00:00", &mut err_stats);
    handle_light_iteration(&sensor, &json_opts, "12:00:00", &mut err_stats);
    assert_eq!(err_stats.errors(), 2);
}

#[test]
fn temp_read_cli_argument_parsing_and_stats() {
    use speaker_core::cli::temp::{
        CliOptions, Statistics, parse_cli_args_from, print_session_summary, print_usage,
    };
    use speaker_core::diag::{CliParse, OutputFormat};
    use speaker_core::hardware::{SensorId, TempError, Temperature, parse_w1_slave};

    fn unwrap_opts(parsed: Result<CliParse<CliOptions>, String>) -> CliOptions {
        match parsed.unwrap() {
            CliParse::Options(o) => o,
            other => panic!("expected CliParse::Options, got {other:?}"),
        }
    }

    print_usage("temp-read-test");

    let mut stats = Statistics::default();
    assert!(stats.samples.average().is_none());
    stats.record_success(Temperature::from_celsius(20.0).unwrap());
    stats.record_success(Temperature::from_celsius(30.0).unwrap());

    let crc_err = parse_w1_slave("00 : crc=00 NO\n00 t=1000\n").unwrap_err();
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

    let lines =
        speaker_core::diag::format_min_max_avg(&stats.samples, "Chassis", "°C", None).unwrap();
    assert_eq!(lines[0], "Chassis Min :   20.00 °C");
    assert_eq!(lines[1], "Chassis Max :   30.00 °C");
    assert_eq!(lines[2], "Chassis Avg :   25.00 °C");

    print_session_summary(&stats);
    let empty_stats = Statistics::default();
    print_session_summary(&empty_stats);

    assert!(matches!(
        parse_cli_args_from(["--help".into()]).unwrap(),
        CliParse::Help
    ));
    assert!(matches!(
        parse_cli_args_from(["--version".into()]).unwrap(),
        CliParse::Version
    ));

    let defaults = unwrap_opts(parse_cli_args_from([]));
    assert_eq!(defaults.common.max_count, Some(1));

    let repeat = unwrap_opts(parse_cli_args_from(["--repeat".into()]));
    assert_eq!(repeat.common.max_count, None);

    let opts = unwrap_opts(parse_cli_args_from([
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
    ]));
    assert_eq!(opts.common.max_count, Some(3));
    assert!(opts.common.verbose);
    assert!(opts.include_soc);
    assert!(opts.show_raw);
    assert_eq!(opts.common.format, OutputFormat::Json);
    assert_eq!(
        opts.target_sensor,
        Some(SensorId::parse("28-000000000001").unwrap())
    );

    let _ = parse_cli_args_from(["--count".into(), "0".into()]).unwrap_err();
    let _ = parse_cli_args_from(["--unknown".into()]).unwrap_err();
}

#[test]
fn temp_read_cli_mock_execution_and_failure_modes() {
    use std::process::ExitCode;

    use speaker_core::cli::temp::{
        CliOptions, Statistics, handle_sensor_iteration, print_diagnostic_registers, print_header,
        run_app,
    };
    use speaker_core::diag::{CommonCliOptions, OutputFormat, ScopedTempDir};
    use speaker_core::hardware::{SensorId, SocThermal, TempSensor, Temperature};

    let (sensor, mock) = TempSensor::new_mocked();
    let id = SensorId::parse("28-000000000001").unwrap();
    mock.set_temperature(id.clone(), Temperature::from_celsius(24.5).unwrap());

    print_header(&sensor);
    print_diagnostic_registers(&sensor, std::slice::from_ref(&id));
    let bad_id = SensorId::parse("28-000000000099").unwrap();
    print_diagnostic_registers(&sensor, &[bad_id]);

    let temp_dir = ScopedTempDir::new("temp_read_cli_test");
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

    // Clear fault for run_app testing
    mock.clear_sensors();
    mock.clear_io_error();
    mock.set_temperature(id.clone(), Temperature::from_celsius(24.5).unwrap());

    assert_eq!(run_app(&sensor, &opts_human), ExitCode::SUCCESS);
    assert_eq!(run_app(&sensor, &opts_json), ExitCode::SUCCESS);

    let opts_target_ok = CliOptions {
        target_sensor: Some(id.clone()),
        include_soc: false,
        ..opts_human
    };
    assert_eq!(run_app(&sensor, &opts_target_ok), ExitCode::SUCCESS);

    let opts_fail = CliOptions {
        target_sensor: Some(SensorId::parse("28-000000000999").unwrap()),
        ..opts_human
    };
    assert_eq!(run_app(&sensor, &opts_fail), ExitCode::FAILURE);

    let mut opts_fail_json = opts_fail;
    opts_fail_json.common.format = OutputFormat::Json;
    assert_eq!(run_app(&sensor, &opts_fail_json), ExitCode::FAILURE);
}

#[test]
fn amp_info_cli_argument_parsing_and_stats() {
    use std::time::Duration;

    use speaker_core::cli::amp::{
        AmpStats, CliOptions, DEFAULT_POLL_INTERVAL, parse_cli_args_from, print_session_summary,
        print_usage,
    };
    use speaker_core::diag::{CliParse, OutputFormat};
    use speaker_core::hardware::{
        AmpErrorFlags, AmpStatus, AudioInConfig, AudioInputFormat, ClockRatio, DcuChannelStatus,
        DcuMonitor, PowerModeProfile,
    };

    fn unwrap_options(parsed: Result<CliParse<CliOptions>, String>) -> CliOptions {
        match parsed.unwrap() {
            CliParse::Options(o) => o,
            other => panic!("expected CliParse::Options, got {other:?}"),
        }
    }

    print_usage("amp-info-test");

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
fn amp_info_cli_mock_execution_and_fault_handling() {
    use std::process::ExitCode;
    use std::time::Duration;

    use speaker_core::cli::amp::{
        AmpStats, CliOptions, handle_amp_iteration, print_header, run_app,
    };
    use speaker_core::diag::{CommonCliOptions, OutputFormat};
    use speaker_core::hardware::{AmpDevice, AmpErrorFlags, AmpI2cAddress};

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

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "comprehensive DSP CLI flag and argument validation"
)]
fn dsp_info_cli_argument_parsing_and_stats() {
    use std::time::Duration;

    use speaker_core::cli::dsp::{
        CliOptions, DspStats, parse_cli_args_from, print_session_summary, print_usage,
    };
    use speaker_core::diag::{CliParse, OutputFormat};
    use speaker_core::hardware::{
        DEFAULT_DSP_HOST, DEFAULT_DSP_PORT, DspChannelLevels, DspHost, DspPort, DspStatus,
        ProcessingState, ServiceState, StopReason,
    };

    print_usage("dsp-info-test");

    let default_opts = CliOptions::default();
    assert_eq!(default_opts.common.max_count, Some(1));

    let empty: Vec<String> = vec![];
    let parse = parse_cli_args_from(empty).unwrap();
    assert!(matches!(parse, CliParse::Options(_)));
    if let CliParse::Options(opts) = parse {
        assert_eq!(opts.common.max_count, Some(1));
        assert_eq!(opts.common.interval, Duration::from_secs(1));
        assert_eq!(opts.common.format, OutputFormat::Human);
        assert!(!opts.reset_clipped);
        assert_eq!(opts.host.as_str(), DEFAULT_DSP_HOST);
        assert_eq!(opts.port.as_u16(), DEFAULT_DSP_PORT);
    }

    let custom = vec![
        "-R".into(),
        "-i".into(),
        "2.5".into(),
        "-n".into(),
        "5".into(),
        "-H".into(),
        "192.168.2.110".into(),
        "-p".into(),
        "8080".into(),
        "-c".into(),
        "-v".into(),
        "-j".into(),
    ];
    let parse = parse_cli_args_from(custom).unwrap();
    if let CliParse::Options(opts) = parse {
        assert_eq!(opts.common.max_count, Some(5));
        assert_eq!(opts.common.interval, Duration::from_millis(2500));
        assert_eq!(opts.common.format, OutputFormat::Json);
        assert!(opts.common.verbose);
        assert!(opts.reset_clipped);
        assert_eq!(opts.host.as_str(), "192.168.2.110");
        assert_eq!(opts.port.as_u16(), 8080);
    }

    // Help and version flags
    assert!(matches!(
        parse_cli_args_from(vec!["-h".into()]).unwrap(),
        CliParse::Help
    ));
    assert!(matches!(
        parse_cli_args_from(vec!["-V".into()]).unwrap(),
        CliParse::Version
    ));

    // Invalid flags
    parse_cli_args_from(vec!["--unknown".into()]).unwrap_err();
    parse_cli_args_from(vec!["--port".into()]).unwrap_err();
    parse_cli_args_from(vec!["--port".into(), "0".into()]).unwrap_err();
    parse_cli_args_from(vec!["--host".into()]).unwrap_err();
    parse_cli_args_from(vec!["--host".into(), String::new()]).unwrap_err();

    let mut stats = DspStats::default();
    let mut sample_status = DspStatus {
        service: ServiceState::Active,
        host: DspHost::default(),
        port: DspPort::default(),
        version: Some("4.1.3".into()),
        config_title: None,
        config_path: None,
        capture_rate: Some(48000),
        state: ProcessingState::Running,
        processing_load: 10.0,
        resampler_load: None,
        rate_adjust: 1.0,
        buffer_level: 500,
        buffer_capacity: 1024,
        volume_db: None,
        muted: None,
        clipped_samples: 0,
        stop_reason: StopReason::None,
        capture: DspChannelLevels::new(vec![-20.0, -18.0], vec![-24.0, -22.0]),
        playback: DspChannelLevels::new(vec![-16.0, -14.0], vec![-20.0, -18.0]),
    };

    stats.record_success(&sample_status);
    assert_eq!(stats.samples.successful_reads(), 1);
    assert_eq!(stats.load_stats.average_f32().unwrap(), 10.0);
    assert_eq!(stats.buffer_stats.average_f32().unwrap(), 500.0);
    assert_eq!(stats.max_capture_peaks, vec![-20.0, -18.0]);
    assert_eq!(stats.max_playback_peaks, vec![-16.0, -14.0]);

    sample_status.processing_load = 20.0;
    sample_status.buffer_level = 600;
    sample_status.clipped_samples = 3;
    sample_status.capture.peaks = vec![-15.0, -19.0].into_boxed_slice();
    sample_status.playback.peaks = vec![-12.0, -15.0].into_boxed_slice();

    stats.record_success(&sample_status);
    assert_eq!(stats.samples.successful_reads(), 2);
    assert_eq!(stats.load_stats.average_f32().unwrap(), 15.0);
    assert_eq!(stats.buffer_stats.average_f32().unwrap(), 550.0);
    assert_eq!(stats.max_capture_peaks, vec![-15.0, -18.0]);
    assert_eq!(stats.max_playback_peaks, vec![-12.0, -14.0]);
    assert_eq!(stats.clipped_reads, 1);
    assert_eq!(stats.max_clipped_samples, 3);

    stats.record_error();
    assert_eq!(stats.samples.errors(), 1);

    // Channel count change resets peak vectors cleanly
    sample_status.capture.peaks = vec![-5.0, -6.0, -7.0].into_boxed_slice();
    sample_status.playback.peaks = vec![-6.0, -7.0, -8.0].into_boxed_slice();
    stats.record_success(&sample_status);
    assert_eq!(stats.max_capture_peaks, vec![-5.0, -6.0, -7.0]);
    assert_eq!(stats.max_playback_peaks, vec![-6.0, -7.0, -8.0]);

    print_session_summary(&stats);
}

#[test]
fn dsp_info_cli_mock_execution_and_fault_handling() {
    use std::process::ExitCode;
    use std::time::Duration;

    use speaker_core::cli::dsp::{CliOptions, run_app};
    use speaker_core::diag::{CommonCliOptions, OutputFormat};
    use speaker_core::hardware::{DspDevice, StopReason};

    let (dsp, mock) = DspDevice::new_mocked();
    let mut opts = CliOptions {
        common: CommonCliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            ..CommonCliOptions::default()
        },
        ..CliOptions::default()
    };

    let code = run_app(&dsp, &opts);
    assert_eq!(code, ExitCode::SUCCESS);

    let mut detailed_status = dsp.read_status().unwrap();
    detailed_status.clipped_samples = 42;
    detailed_status.volume_db = Some(-12.5);
    detailed_status.muted = Some(true);
    detailed_status.stop_reason = StopReason::Done;
    detailed_status.capture.peaks = Box::new([-1.0, -2.0, -3.0]);
    detailed_status.capture.rms = Box::new([-10.0, -11.0, -12.0]);
    detailed_status.playback.peaks = Box::new([-4.0, -5.0, -6.0]);
    detailed_status.playback.rms = Box::new([-13.0, -14.0, -15.0]);
    speaker_core::cli::dsp::print_human_detailed(&detailed_status);

    detailed_status.muted = Some(false);
    speaker_core::cli::dsp::print_human_detailed(&detailed_status);

    // JSON format
    opts.common.format = OutputFormat::Json;
    let code_json = run_app(&dsp, &opts);
    assert_eq!(code_json, ExitCode::SUCCESS);

    // Reset clipped mode
    let mut reset_opts = CliOptions {
        reset_clipped: true,
        ..CliOptions::default()
    };
    mock.set_clipped_samples(10);
    let code_reset = run_app(&dsp, &reset_opts);
    assert_eq!(code_reset, ExitCode::SUCCESS);

    // Reset clipped in JSON mode
    reset_opts.common.format = OutputFormat::Json;
    let code_reset_json = run_app(&dsp, &reset_opts);
    assert_eq!(code_reset_json, ExitCode::SUCCESS);

    // Fault injected returns error in both formats
    mock.inject_fault("connection failure");
    let code_fault = run_app(&dsp, &opts);
    assert_eq!(code_fault, ExitCode::FAILURE);

    let code_reset_fault = run_app(&dsp, &reset_opts);
    assert_eq!(code_reset_fault, ExitCode::FAILURE);

    reset_opts.common.format = OutputFormat::Human;
    let code_reset_human_fault = run_app(&dsp, &reset_opts);
    assert_eq!(code_reset_human_fault, ExitCode::FAILURE);

    mock.clear_fault();

    // Once mode with clipping tip
    mock.set_clipped_samples(5);
    let mut once_opts = CliOptions {
        common: CommonCliOptions {
            max_count: Some(1),
            interval: Duration::from_millis(1),
            ..CommonCliOptions::default()
        },
        ..CliOptions::default()
    };
    let code_once = run_app(&dsp, &once_opts);
    assert_eq!(code_once, ExitCode::SUCCESS);

    // Multi-read verbose mode
    once_opts.common.max_count = Some(2);
    once_opts.common.verbose = true;
    let code_verbose = run_app(&dsp, &once_opts);
    assert_eq!(code_verbose, ExitCode::SUCCESS);

    // Test with single-channel levels
    mock.set_levels(vec![-10.0], vec![-14.0], vec![-12.0], vec![-16.0]);
    let code_single_ch = run_app(&dsp, &once_opts);
    assert_eq!(code_single_ch, ExitCode::SUCCESS);
}

#[test]
fn encoder_read_cli_argument_parsing_and_stats() {
    use std::time::Duration;

    use speaker_core::cli::encoder::{
        CliOptions, DEFAULT_POLL_INTERVAL, EncoderStats, parse_cli_args_from,
        print_session_summary, print_usage,
    };
    use speaker_core::diag::{CliParse, OutputFormat};
    use speaker_core::hardware::{EncoderStatus, RgbColor};

    fn unwrap_options(parsed: Result<CliParse<CliOptions>, String>) -> CliOptions {
        match parsed.unwrap() {
            CliParse::Options(o) => o,
            other => panic!("expected CliParse::Options, got {other:?}"),
        }
    }

    print_usage("encoder-read-test");

    let mut stats = EncoderStats::default();

    let baseline = EncoderStatus {
        switch_pressed: false,
        cw_count: 0,
        ccw_count: 0,
        press_count: 0,
        release_count: 0,
        led_color: RgbColor::OFF,
    };
    stats.record_success(&baseline);
    assert_eq!(stats.samples.total_reads(), 1);
    assert_eq!(stats.samples.successful_reads(), 1);
    assert_eq!(stats.cw_events, 0);

    let rotated = EncoderStatus {
        cw_count: 2,
        ccw_count: 1,
        press_count: 1,
        release_count: 1,
        switch_pressed: false,
        ..baseline
    };
    stats.record_success(&rotated);
    assert_eq!(stats.cw_events, 2);
    assert_eq!(stats.ccw_events, 1);
    assert_eq!(stats.press_events, 1);
    assert_eq!(stats.release_events, 1);

    stats.record_error();
    assert_eq!(stats.samples.errors(), 1);
    assert_eq!(stats.samples.total_reads(), 3);

    print_session_summary(&stats);
    print_session_summary(&EncoderStats::default());

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

    // Defaults: once mode, 10 ms interval
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
    assert!(!opts.verify_leds);

    let opts = unwrap_options(parse_cli_args_from([
        "--count".into(),
        "5".into(),
        "--interval".into(),
        "0.5".into(),
        "--verify-leds".into(),
        "--verbose".into(),
        "--json".into(),
    ]));

    assert_eq!(opts.common.max_count, Some(5));
    assert_eq!(opts.common.interval, Duration::from_millis(500));
    assert!(opts.verify_leds);
    assert!(opts.common.verbose);
    assert_eq!(opts.common.format, OutputFormat::Json);

    let _ = parse_cli_args_from(["--unknown-flag".into()]).unwrap_err();
    let _ = parse_cli_args_from(["--count".into(), "not-a-number".into()]).unwrap_err();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "comprehensive encoder CLI mock execution, LED sweep, and event handling validation"
)]
fn encoder_read_cli_mock_execution_and_events() {
    use std::process::ExitCode;
    use std::time::Duration;

    use speaker_core::cli::encoder::{
        CliOptions, EncoderStats, handle_encoder_iteration, print_diagnostic_status, print_header,
        run_app, run_led_sweep,
    };
    use speaker_core::diag::{CommonCliOptions, OutputFormat};
    use speaker_core::hardware::{EncoderDevice, RgbColor, RotationDirection};

    let (device, mock) = EncoderDevice::new_mocked();
    print_header(&device);
    print_diagnostic_status(&device);

    let opts = CliOptions {
        common: CommonCliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            verbose: true,
            format: OutputFormat::Human,
        },
        verify_leds: false,
    };

    let code = run_app(&device, &opts);
    assert_eq!(code, ExitCode::SUCCESS);

    let json_opts = CliOptions {
        common: CommonCliOptions {
            max_count: Some(2),
            interval: Duration::from_millis(1),
            verbose: false,
            format: OutputFormat::Json,
        },
        verify_leds: false,
    };

    let code = run_app(&device, &json_opts);
    assert_eq!(code, ExitCode::SUCCESS);

    // Test error handling branch
    mock.set_fault(Some("GPIO simulated bus fault".into()));
    let mut err_stats = EncoderStats::default();
    handle_encoder_iteration(&device, &opts, "12:00:00", &mut err_stats);
    handle_encoder_iteration(&device, &json_opts, "12:00:00", &mut err_stats);
    assert_eq!(err_stats.samples.errors(), 2);

    mock.set_fault(None);

    // LED sweep on mock
    run_led_sweep(&device, OutputFormat::Human, Duration::ZERO).unwrap();
    assert_eq!(mock.led_color(), RgbColor::OFF);
    run_led_sweep(&device, OutputFormat::Json, Duration::ZERO).unwrap();

    // LED sweep fault injection
    mock.set_fault(Some("GPIO simulated bus fault".into()));
    let err = run_led_sweep(&device, OutputFormat::Human, Duration::ZERO).unwrap_err();
    assert!(err.is_mock_fault());
    mock.set_fault(None);

    // Rotation and switch events simulation
    let opts_human = CliOptions {
        common: CommonCliOptions {
            max_count: None,
            interval: Duration::from_millis(1),
            verbose: false,
            format: OutputFormat::Human,
        },
        verify_leds: false,
    };
    let mut stats = EncoderStats::default();

    mock.simulate_rotation(RotationDirection::Clockwise, 2);
    mock.simulate_rotation(RotationDirection::CounterClockwise, 1);
    mock.set_switch_pressed(true);
    handle_encoder_iteration(&device, &opts_human, "12:00:00", &mut stats);
    assert_eq!(stats.cw_events, 2);
    assert_eq!(stats.ccw_events, 1);

    mock.simulate_rotation(RotationDirection::CounterClockwise, 1);
    handle_encoder_iteration(&device, &opts_human, "12:00:01", &mut stats);
    assert_eq!(stats.ccw_events, 2);

    mock.set_switch_pressed(false);
    handle_encoder_iteration(&device, &opts_human, "12:00:02", &mut stats);
    assert_eq!(stats.release_events, 1);

    handle_encoder_iteration(&device, &opts_human, "12:00:03", &mut stats);
    assert_eq!(stats.samples.successful_reads(), 4);

    handle_encoder_iteration(&device, &json_opts, "12:00:04", &mut stats);
    assert_eq!(stats.samples.successful_reads(), 5);

    let mut run_opts = CliOptions::default();
    run_opts.common.max_count = Some(1);
    run_opts.common.format = OutputFormat::Human;
    assert_eq!(
        speaker_core::cli::encoder::run_app(&device, &run_opts),
        std::process::ExitCode::SUCCESS
    );
    run_opts.common.format = OutputFormat::Json;
    assert_eq!(
        speaker_core::cli::encoder::run_app(&device, &run_opts),
        std::process::ExitCode::SUCCESS
    );

    // LED sweep success and fault
    speaker_core::cli::encoder::run_led_sweep(
        &device,
        OutputFormat::Human,
        Duration::from_millis(1),
    )
    .unwrap();
    speaker_core::cli::encoder::run_led_sweep(
        &device,
        OutputFormat::Json,
        Duration::from_millis(1),
    )
    .unwrap();
    mock.set_fault(Some("LED fault".into()));
    assert!(
        speaker_core::cli::encoder::run_led_sweep(
            &device,
            OutputFormat::Human,
            Duration::from_millis(1)
        )
        .is_err()
    );

    let mut verify_opts = CliOptions {
        verify_leds: true,
        ..CliOptions::default()
    };
    verify_opts.common.format = OutputFormat::Human;
    assert_eq!(
        speaker_core::cli::encoder::run_app(&device, &verify_opts),
        std::process::ExitCode::FAILURE
    );
    verify_opts.common.format = OutputFormat::Json;
    assert_eq!(
        speaker_core::cli::encoder::run_app(&device, &verify_opts),
        std::process::ExitCode::FAILURE
    );

    mock.set_fault(None);
    verify_opts.common.format = OutputFormat::Human;
    assert_eq!(
        speaker_core::cli::encoder::run_app(&device, &verify_opts),
        std::process::ExitCode::SUCCESS
    );
}

#[test]
fn bt_info_cli_argument_parsing_and_stats() {
    use speaker_core::cli::bt::{
        BtStats, CliOptions, DEFAULT_POLL_INTERVAL, parse_cli_args_from, parse_on_off,
        print_session_summary, print_usage,
    };
    use speaker_core::diag::{CliParse, OutputFormat};
    use speaker_core::hardware::{BluetoothDevice, BtAddress};

    fn unwrap_options(parsed: Result<CliParse<CliOptions>, String>) -> CliOptions {
        match parsed.unwrap() {
            CliParse::Options(o) => o,
            other => panic!("expected CliParse::Options, got {other:?}"),
        }
    }

    print_usage("bt-info-test");

    // parse_on_off variations
    assert!(parse_on_off("on", "--power").unwrap());
    assert!(parse_on_off("true", "--power").unwrap());
    assert!(parse_on_off("1", "--power").unwrap());
    assert!(parse_on_off("yes", "--power").unwrap());
    assert!(!parse_on_off("off", "--power").unwrap());
    assert!(!parse_on_off("false", "--power").unwrap());
    assert!(!parse_on_off("0", "--power").unwrap());
    assert!(!parse_on_off("no", "--power").unwrap());
    parse_on_off("unknown", "--power").unwrap_err();

    // Defaults: once mode, one second interval
    let args: Vec<String> = vec![];
    let opts = unwrap_options(parse_cli_args_from(args));
    assert_eq!(opts.common.interval, DEFAULT_POLL_INTERVAL);
    assert_eq!(opts.common.max_count, Some(1));
    assert_eq!(opts.common.format, OutputFormat::Human);
    assert!(!opts.include_paired);

    // Repeat flag switches back to continuous polling
    let repeat = unwrap_options(parse_cli_args_from(vec!["--repeat".into()]));
    assert_eq!(repeat.common.max_count, None);

    let args = vec![
        "--once".into(),
        "-a".into(),
        "hci0".into(),
        "-d".into(),
        "AA:BB:CC:DD:EE:FF".into(),
        "-p".into(),
        "-v".into(),
        "-j".into(),
        "--power".into(),
        "on".into(),
        "--discoverable".into(),
        "off".into(),
        "--pairable".into(),
        "yes".into(),
    ];
    let opts = unwrap_options(parse_cli_args_from(args));
    assert_eq!(opts.common.max_count, Some(1));
    assert_eq!(opts.adapter.as_deref(), Some("hci0"));
    assert_eq!(
        opts.target_device,
        Some(BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]))
    );
    assert!(opts.include_paired);
    assert!(opts.common.verbose);
    assert_eq!(opts.common.format, OutputFormat::Json);
    assert_eq!(opts.power, Some(true));
    assert_eq!(opts.discoverable, Some(false));
    assert_eq!(opts.pairable, Some(true));

    assert!(matches!(
        parse_cli_args_from(vec!["--help".into()]).unwrap(),
        CliParse::Help
    ));
    assert!(matches!(
        parse_cli_args_from(vec!["--version".into()]).unwrap(),
        CliParse::Version
    ));
    parse_cli_args_from(vec!["--power".into(), "invalid".into()]).unwrap_err();
    parse_cli_args_from(vec!["--device".into(), "bad-mac".into()]).unwrap_err();

    let mut stats = BtStats::default();
    let (device, _) = BluetoothDevice::new_mocked();
    let reading = device.status().unwrap();

    stats.record_success(&reading);
    assert_eq!(stats.samples.total_reads(), 1);
    assert_eq!(stats.samples.successful_reads(), 1);
    assert_eq!(stats.operational_reads, 1);
    assert_eq!(stats.streaming_reads, 1);
    assert_eq!(stats.total_connected_devices, 1);

    stats.record_error();
    assert_eq!(stats.samples.total_reads(), 2);
    assert_eq!(stats.samples.successful_reads(), 1);
    assert_eq!(stats.samples.errors(), 1);

    print_session_summary(&stats);
    print_session_summary(&BtStats::default());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "comprehensive Bluetooth CLI mock execution, controls, and filtering validation"
)]
fn bt_info_cli_mock_execution_and_controls() {
    use std::process::ExitCode;
    use std::time::Duration;

    use speaker_core::cli::bt::{
        BtStats, CliOptions, handle_bt_iteration, output_json_status, print_human_single_status,
        run_app,
    };
    use speaker_core::diag::OutputFormat;
    use speaker_core::hardware::{BluetoothDevice, BtAddress, BtDevice};

    let (device, mock) = BluetoothDevice::new_mocked();
    let mut opts = CliOptions::default();
    opts.common.max_count = Some(1);
    opts.common.verbose = true;
    opts.include_paired = true;

    let code = run_app(&device, &opts);
    assert_eq!(code, ExitCode::SUCCESS);

    opts.common.format = OutputFormat::Json;
    let code_json = run_app(&device, &opts);
    assert_eq!(code_json, ExitCode::SUCCESS);

    // Multi-iterations
    opts.common.format = OutputFormat::Human;
    opts.common.max_count = Some(2);
    opts.common.interval = Duration::from_millis(1);
    let code_multi = run_app(&device, &opts);
    assert_eq!(code_multi, ExitCode::SUCCESS);

    // Controls application
    let mut ctrl_opts = CliOptions::default();
    ctrl_opts.common.max_count = Some(1);
    ctrl_opts.power = Some(true);
    ctrl_opts.discoverable = Some(false);
    ctrl_opts.pairable = Some(true);
    assert_eq!(run_app(&device, &ctrl_opts), ExitCode::SUCCESS);

    // Adapter targeting
    let mut adapt_opts = CliOptions::default();
    adapt_opts.common.max_count = Some(1);
    adapt_opts.adapter = Some("hci0".into());
    assert_eq!(run_app(&device, &adapt_opts), ExitCode::SUCCESS);

    adapt_opts.adapter = Some("hci1".into());
    let mut adapt_stats = BtStats::default();
    handle_bt_iteration(&device, &adapt_opts, "12:00:00", &mut adapt_stats);
    assert_eq!(adapt_stats.samples.errors(), 1);

    // Human and json iteration branches
    let mut multi_opts = CliOptions::default();
    multi_opts.common.max_count = Some(2);
    multi_opts.common.verbose = true;
    multi_opts.common.format = OutputFormat::Human;

    let mut iter_stats = BtStats::default();
    handle_bt_iteration(&device, &multi_opts, "12:00:00", &mut iter_stats);

    mock.set_powered(false);
    handle_bt_iteration(&device, &multi_opts, "12:00:01", &mut iter_stats);

    mock.set_adapter(None);
    handle_bt_iteration(&device, &multi_opts, "12:00:02", &mut iter_stats);

    mock.set_fault(Some("D-Bus failure".into()));
    handle_bt_iteration(&device, &multi_opts, "12:00:03", &mut iter_stats);

    multi_opts.common.format = OutputFormat::Json;
    handle_bt_iteration(&device, &multi_opts, "12:00:04", &mut iter_stats);
    assert_eq!(iter_stats.samples.errors(), 2);

    mock.set_fault(None);

    // Filters and edge cases
    let target_addr = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    let other_addr = BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

    let dev_no_meta = BtDevice {
        address: other_addr,
        name: "Other".into(),
        alias: "".into(),
        connected: false,
        paired: true,
        trusted: false,
        blocked: false,
        rssi: None,
        battery_percentage: None,
        icon: None,
        uuids: Box::default(),
    };
    mock.add_device(dev_no_meta);

    let mut filter_opts = CliOptions::default();
    filter_opts.common.max_count = Some(1);
    filter_opts.include_paired = true;
    filter_opts.common.verbose = true;
    filter_opts.target_device = Some(target_addr);

    let status = device.status().unwrap();
    print_human_single_status(&status, &filter_opts);
    output_json_status("12:00:00", &status, &filter_opts);

    mock.set_adapter(None);
    mock.clear_pcms();
    mock.clear_devices();
    let empty_status = device.status().unwrap();
    filter_opts.target_device = None;
    print_human_single_status(&empty_status, &filter_opts);

    // Bluetooth CLI run_app and controls coverage
    let (run_device, run_mock) = BluetoothDevice::new_mocked();
    let mut run_opts = CliOptions::default();
    run_opts.common.max_count = Some(1);
    run_opts.power = Some(true);
    run_opts.discoverable = Some(true);
    run_opts.pairable = Some(true);
    run_opts.common.format = OutputFormat::Human;
    assert_eq!(
        speaker_core::cli::bt::run_app(&run_device, &run_opts),
        std::process::ExitCode::SUCCESS
    );

    run_opts.common.format = OutputFormat::Json;
    assert_eq!(
        speaker_core::cli::bt::run_app(&run_device, &run_opts),
        std::process::ExitCode::SUCCESS
    );

    // Error in controls
    run_mock.set_fault(Some("simulated D-Bus failure".into()));
    run_opts.common.format = OutputFormat::Human;
    assert_eq!(
        speaker_core::cli::bt::run_app(&run_device, &run_opts),
        std::process::ExitCode::FAILURE
    );

    run_opts.common.format = OutputFormat::Json;
    assert_eq!(
        speaker_core::cli::bt::run_app(&run_device, &run_opts),
        std::process::ExitCode::FAILURE
    );

    // Audio pipeline and hardware info with populated fields
    let populated_hw = speaker_core::hardware::BluetoothHardwareInfo {
        chip: "BCM43438".into(),
        bus: "UART0".into(),
        baud_rate: 921_600,
        firmware: "BCM43430A1.hcd".into(),
        driver: "hci_uart (bcm)".into(),
        default_controller: "hci0".into(),
        controller: speaker_core::hardware::BluetoothControllerInfo {
            adapter: Some("hci0".into()),
            hci_version: Some("4.1".into()),
            hci_revision: Some("0x1234".into()),
            lmp_version: Some("4.1".into()),
            lmp_subversion: Some("0x5678".into()),
            manufacturer: Some("Broadcom".into()),
            bus_type: Some("UART".into()),
            bd_address: Some("B8:27:EB:00:11:22".into()),
            firmware_version: Some("v1.2.3".into()),
        },
    };
    speaker_core::cli::bt::print_human_hardware_info(&populated_hw);

    let pcm = speaker_core::hardware::BlueAlsaPcm {
        pcm_path: "/org/bluealsa/pcm0".into(),
        device_address: target_addr,
        resolved_name: "Test Speaker".into(),
        transport: "A2DP Sink".into(),
        running: true,
        selected_codec: "aptX".into(),
        available_codecs: Box::new(["SBC".into(), "aptX".into()]),
        delay: Some("150 ms".into()),
        volume: Some("80%".into()),
        muted: false,
        sampling_rate: 44100,
        channels: 2,
        format: "S16_2LE".into(),
    };
    run_mock.set_fault(None);
    let mut pcm_status = run_device.status().unwrap();
    pcm_status.pcms = Box::new([pcm]);
    let mut pcm_opts = CliOptions::default();
    speaker_core::cli::bt::print_human_audio_pipeline(&pcm_status, &pcm_opts);
    pcm_opts.target_device = Some(target_addr);
    speaker_core::cli::bt::print_human_audio_pipeline(&pcm_status, &pcm_opts);
}
