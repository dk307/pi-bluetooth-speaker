//! Integration and CLI binary tests for core executables and diagnostics.
//!
//! Verifies CLI flag parsing, help text generation, invalid argument rejection,
//! and error message formatting across standalone binaries without depending on
//! physical Raspberry Pi hardware peripherals or live sysfs paths.
//!
//! Note that application execution, JSON serialization, and hardware fault injection
//! are unit-tested via `run_app` entrypoints using mock backends in `src/bin/read_temp.rs`
//! and `src/bin/read_light.rs`.

use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Probes whether a compiled binary is executable in the current environment.
///
/// In CI environments without cross-compilation emulation, failures panic loudly
/// to prevent silent skips from masking test regressions. `CROSS_COMPILE` may only
/// be set on non-native targets running without kernel `binfmt_misc` emulation.
fn can_exec_binary(bin_path: &str) -> bool {
    match Command::new(bin_path).arg("--help").output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "SKIPPED: target binary '{bin_path}' failed --help probe ({:?}): {stderr}",
                output.status.code()
            );
            assert!(
                std::env::var("CI").is_err() || std::env::var("CROSS_COMPILE").is_ok(),
                "CLI tests unexpectedly failed to execute binary in CI environment: {bin_path} ({stderr})"
            );
            false
        }
        Err(e) => {
            eprintln!(
                "SKIPPED: target binary '{bin_path}' cannot execute on host architecture: {e}"
            );
            assert!(
                std::env::var("CI").is_err() || std::env::var("CROSS_COMPILE").is_ok(),
                "CLI tests unexpectedly failed to execute binary in CI environment: {bin_path} ({e})"
            );
            false
        }
    }
}

/// Helper to execute a binary with arguments and capture status, stdout, and stderr.
fn run_cli(bin: &str, args: &[&str]) -> (ExitStatus, String, String) {
    let output = Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute binary '{bin}' with args {args:?}: {e}"));
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn core_main_binary_executes() {
    // Note: core binary currently executes an initial diagnostic banner and exits.
    // The banner prints static GPIO/hardware pin metadata via `TempSensor::gpio_info()`
    // which does not perform live sysfs I/O, ensuring deterministic execution on host.
    // Standard output is well under the default OS pipe buffer capacity (~64 KB),
    // ensuring `run_cli` / `output()` will not block or deadlock on pipe drainage.
    let bin_path = env!("CARGO_BIN_EXE_core");
    if !can_exec_binary(bin_path) {
        return;
    }

    let mut child = Command::new(bin_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn core main binary");

    let deadline = Instant::now() + Duration::from_secs(5);
    let output = loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                break child
                    .wait_with_output()
                    .expect("failed to collect output after exit");
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("core binary timed out after 5s");
            }
            Err(e) => panic!("failed to wait for core binary: {e}"),
        }
    };

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Speaker Core Service is running on Raspberry Pi 3!"));
    assert!(stdout.contains("ARM Cortex-A53 (aarch64)"));
    assert!(stdout.contains("Thermal Sensor:"));
}

#[test]
fn read_temp_cli_help_displays() {
    let bin_path = env!("CARGO_BIN_EXE_read_temp");
    if !can_exec_binary(bin_path) {
        return;
    }

    let (status, stdout, _) = run_cli(bin_path, &["--help"]);
    assert!(status.success());
    assert!(stdout.contains("DS18B20 1-Wire Temperature Diagnostic Utility"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--count"));
    assert!(stdout.contains("--interval"));

    let (short_status, _, _) = run_cli(bin_path, &["-h"]);
    assert!(short_status.success());
}

#[test]
fn read_temp_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_read_temp");
    if !can_exec_binary(bin_path) {
        return;
    }

    let cases: &[(&[&str], &str)] = &[
        (&["--invalid-flag"], "unknown option '--invalid-flag'"),
        (&["--count"], "missing value for --count"),
        (&["--count", "not-a-number"], "invalid count 'not-a-number'"),
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
            &["--interval", "1e400"],
            "interval must be a finite value greater than 0",
        ),
        (
            &["--interval", "nan"],
            "interval must be a finite value greater than 0",
        ),
        (&["--device"], "missing value for --device"),
        (
            &["--device", "invalid-device-id"],
            "invalid DS18B20 sensor ID",
        ),
    ];

    for (args, expected_err) in cases {
        let (status, _, stderr) = run_cli(bin_path, args);
        assert!(!status.success(), "expected failure for args: {args:?}");
        assert!(
            stderr.contains(expected_err),
            "expected stderr to contain '{expected_err}' for args {args:?}, got: '{stderr}'"
        );
    }
}

#[test]
fn read_light_cli_help_displays() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");
    if !can_exec_binary(bin_path) {
        return;
    }

    let (status, stdout, _) = run_cli(bin_path, &["--help"]);
    assert!(status.success());
    assert!(stdout.contains("BH1750 Ambient Light Diagnostic Utility"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--mode"));
    assert!(stdout.contains("--address"));

    let (short_status, _, _) = run_cli(bin_path, &["-h"]);
    assert!(short_status.success());
}

#[test]
fn read_light_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");
    if !can_exec_binary(bin_path) {
        return;
    }

    let cases: &[(&[&str], &str)] = &[
        (&["--invalid-flag"], "unknown option '--invalid-flag'"),
        (&["--count"], "missing value for --count"),
        (&["--count", "not-a-number"], "invalid count 'not-a-number'"),
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
        (&["--interval", "not-float"], "invalid interval 'not-float'"),
        (
            &["--interval", "-1.0"],
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
        (&["--mode"], "missing value for --mode"),
        (&["--mode", "ultra-mode"], "invalid mode 'ultra-mode'"),
        (&["--address"], "missing value for --address"),
        (&["--address", "not-hex"], "invalid hex address"),
        (&["--address", "0xzz"], "invalid hex address"),
        (&["--address", "0x00"], "invalid BH1750 I2C address"),
        (&["--address", "0x42"], "invalid BH1750 I2C address"),
        (&["--address", "0x77"], "invalid BH1750 I2C address"),
        (&["--address", "0x80"], "invalid BH1750 I2C address"),
    ];

    for (args, expected_err) in cases {
        let (status, _, stderr) = run_cli(bin_path, args);
        assert!(!status.success(), "expected failure for args: {args:?}");
        assert!(
            stderr.contains(expected_err),
            "expected stderr to contain '{expected_err}' for args {args:?}, got: '{stderr}'"
        );
    }
}
