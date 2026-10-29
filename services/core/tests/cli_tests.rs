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
        assert!(stdout.contains("core 2.0.0"));
    }

    // Test zero duration preview
    let (status, stdout, _) = run_cli(bin_path, &["-p", "0"]);
    assert!(status.success());
    assert!(stdout.contains("Speaker Core Service"));

    // Test error cases
    let (status, _, stderr) = run_cli(bin_path, &["--preview", "invalid"]);
    assert!(!status.success());
    assert!(stderr.contains("valid positive integer"));

    let (status, _, stderr) = run_cli(bin_path, &["--preview"]);
    assert!(!status.success());
    assert!(stderr.contains("requires a seconds argument"));

    let (status, _, stderr) = run_cli(bin_path, &["--unknown-flag"]);
    assert!(!status.success());
    assert!(stderr.contains("unrecognized argument"));
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
