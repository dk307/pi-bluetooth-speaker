//! Integration and CLI binary tests for core executables and diagnostics.
//!
//! Verifies CLI flag parsing, help text generation, invalid argument rejection,
//! and error message formatting across standalone binaries without depending on
//! physical Raspberry Pi hardware peripherals or live sysfs paths.
//!
//! Note that application execution, JSON serialization, and hardware fault injection
//! are unit-tested via `run_app` entrypoints using mock backends in `src/bin/read_temp.rs`
//! and `src/bin/read_light.rs`.

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn core_main_binary_executes() {
    // Note: core binary currently executes an initial diagnostic banner and exits.
    // When converted to a long-running daemon, this will invoke `--self-test` or `--version`.
    let bin_path = env!("CARGO_BIN_EXE_core");

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

    let output = Command::new(bin_path)
        .arg("--help")
        .output()
        .expect("failed to run read_temp");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("DS18B20 1-Wire Temperature Diagnostic Utility"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--count"));
    assert!(stdout.contains("--interval"));

    let short_help = Command::new(bin_path)
        .arg("-h")
        .output()
        .expect("failed to run read_temp");
    assert!(short_help.status.success());
}

#[test]
fn read_temp_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_read_temp");

    // Invalid flag
    let err_arg = Command::new(bin_path)
        .arg("--invalid-flag")
        .output()
        .expect("failed to run read_temp");
    assert!(!err_arg.status.success());

    // Missing count parameter
    let missing_count = Command::new(bin_path)
        .arg("--count")
        .output()
        .expect("failed to run read_temp");
    assert!(!missing_count.status.success());

    // Non-integer count
    let invalid_count = Command::new(bin_path)
        .args(["--count", "not-a-number"])
        .output()
        .expect("failed to run read_temp");
    assert!(!invalid_count.status.success());

    // Zero count rejected
    let zero_count = Command::new(bin_path)
        .args(["--count", "0"])
        .output()
        .expect("failed to run read_temp");
    assert!(!zero_count.status.success());
    let stderr_count = String::from_utf8_lossy(&zero_count.stderr);
    assert!(stderr_count.contains("--count must be at least 1"));

    // Missing interval parameter
    let missing_interval = Command::new(bin_path)
        .arg("--interval")
        .output()
        .expect("failed to run read_temp");
    assert!(!missing_interval.status.success());

    // Zero interval rejected
    let zero_interval = Command::new(bin_path)
        .args(["--interval", "0"])
        .output()
        .expect("failed to run read_temp");
    assert!(!zero_interval.status.success());

    // Non-float interval
    let invalid_interval = Command::new(bin_path)
        .args(["--interval", "invalid"])
        .output()
        .expect("failed to run read_temp");
    assert!(!invalid_interval.status.success());

    // Missing device parameter
    let missing_device = Command::new(bin_path)
        .arg("--device")
        .output()
        .expect("failed to run read_temp");
    assert!(!missing_device.status.success());

    // Invalid device ID format
    let invalid_device = Command::new(bin_path)
        .args(["--device", "invalid-device-id"])
        .output()
        .expect("failed to run read_temp");
    assert!(!invalid_device.status.success());
    let stderr_dev = String::from_utf8_lossy(&invalid_device.stderr);
    assert!(stderr_dev.contains("invalid DS18B20 sensor ID"));
}

#[test]
fn read_light_cli_help_displays() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");

    let output = Command::new(bin_path)
        .arg("--help")
        .output()
        .expect("failed to run read_light");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("BH1750 Ambient Light Diagnostic Utility"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--mode"));
    assert!(stdout.contains("--address"));

    let short_help = Command::new(bin_path)
        .arg("-h")
        .output()
        .expect("failed to run read_light");
    assert!(short_help.status.success());
}

#[test]
fn read_light_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");

    // Invalid flag
    let err_arg = Command::new(bin_path)
        .arg("--invalid-flag")
        .output()
        .expect("failed to run read_light");
    assert!(!err_arg.status.success());

    // Missing count parameter
    let missing_count = Command::new(bin_path)
        .arg("--count")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_count.status.success());

    // Non-integer count
    let invalid_count = Command::new(bin_path)
        .args(["--count", "not-a-number"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_count.status.success());

    // Zero count rejected
    let zero_count = Command::new(bin_path)
        .args(["--count", "0"])
        .output()
        .expect("failed to run read_light");
    assert!(!zero_count.status.success());
    let stderr_count = String::from_utf8_lossy(&zero_count.stderr);
    assert!(stderr_count.contains("--count must be at least 1"));

    // Missing interval parameter
    let missing_interval = Command::new(bin_path)
        .arg("--interval")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_interval.status.success());

    // Zero interval rejected
    let zero_interval = Command::new(bin_path)
        .args(["--interval", "0"])
        .output()
        .expect("failed to run read_light");
    assert!(!zero_interval.status.success());

    // Non-float interval
    let invalid_interval = Command::new(bin_path)
        .args(["--interval", "not-float"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_interval.status.success());
}

#[test]
fn read_light_cli_mode_and_address_validation() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");

    // Mode parsing & errors
    let missing_mode = Command::new(bin_path)
        .arg("--mode")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_mode.status.success());

    let invalid_mode = Command::new(bin_path)
        .args(["--mode", "ultra-mode"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_mode.status.success());
    let stderr_mode = String::from_utf8_lossy(&invalid_mode.stderr);
    assert!(stderr_mode.contains("invalid mode 'ultra-mode'"));

    // Address parsing & errors
    let missing_addr = Command::new(bin_path)
        .arg("--address")
        .output()
        .expect("failed to run read_light");
    assert!(!missing_addr.status.success());

    let invalid_hex = Command::new(bin_path)
        .args(["--address", "not-hex"])
        .output()
        .expect("failed to run read_light");
    assert!(!invalid_hex.status.success());
    let stderr_hex = String::from_utf8_lossy(&invalid_hex.stderr);
    assert!(stderr_hex.contains("invalid hex address"));

    // Non-BH1750 address rejected
    let unsupported_addr = Command::new(bin_path)
        .args(["--address", "0x77"])
        .output()
        .expect("failed to run read_light");
    assert!(!unsupported_addr.status.success());
    let stderr_addr = String::from_utf8_lossy(&unsupported_addr.stderr);
    assert!(stderr_addr.contains("invalid BH1750 I2C address"));
}
