//! Integration and CLI binary tests for core executables and diagnostics.
//!
//! Verifies CLI flag parsing, help text generation, invalid argument rejection,
//! and error message formatting across standalone binaries without depending on
//! physical Raspberry Pi hardware peripherals or live sysfs paths.
//!
//! Note that application execution, JSON serialization, and hardware fault injection
//! are unit-tested via `run_app` entrypoints using mock backends in `src/bin/read_temp.rs`
//! and `src/bin/read_light.rs`.

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

/// Panics with `msg` when running inside a CI environment (`CI` env var set).
fn fail_if_ci(msg: &str) {
    assert!(std::env::var("CI").is_err(), "{msg}");
}

/// Spawns `bin` with `args` and waits up to `timeout`, returning `None` if the binary cannot be spawned.
fn try_run_cli(
    bin: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<(ExitStatus, String, String)> {
    let Ok(child) = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        fail_if_ci(&format!("failed to spawn {bin} in CI environment"));
        return None;
    };
    let output = wait_with_deadline(child, timeout, bin);
    Some((
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Probes whether a compiled binary is executable in the current environment via `--help`.
///
/// Returns `Some((status, stdout, stderr))` when `--help` succeeds, allowing callers to
/// inspect the help output directly without spawning a duplicate `--help` subprocess.
/// In CI environments, any execution failure or timeout panics loudly to prevent
/// silent skips from masking test regressions or binfmt misconfigurations.
fn probe_help(bin_path: &str) -> Option<(ExitStatus, String, String)> {
    let (status, stdout, stderr) = try_run_cli(bin_path, &["--help"], CLI_PROBE_TIMEOUT)?;
    if status.success() {
        return Some((status, stdout, stderr));
    }
    fail_if_ci(&format!(
        "binary '{bin_path}' exited {status:?} in CI ({stderr}); must never skip silently"
    ));
    None
}

/// Returns `true` if the compiled binary can be executed in the current environment.
fn can_exec_binary(bin_path: &str) -> bool {
    probe_help(bin_path).is_some()
}

/// Executes a binary with arguments under `CLI_PROBE_TIMEOUT` and captures status, stdout, and stderr.
fn run_cli(bin: &str, args: &[&str]) -> (ExitStatus, String, String) {
    try_run_cli(bin, args, CLI_PROBE_TIMEOUT)
        .unwrap_or_else(|| panic!("failed to spawn binary '{bin}' with args {args:?}"))
}

#[test]
fn core_main_binary_executes() {
    // Note: core binary currently executes an initial diagnostic banner and exits.
    // The banner prints static GPIO/hardware pin metadata via `TempSensor::gpio_info()`
    // which does not perform live sysfs I/O, ensuring deterministic execution on host.
    let bin_path = env!("CARGO_BIN_EXE_core");
    if !can_exec_binary(bin_path) {
        return;
    }

    let (status, stdout, _) =
        try_run_cli(bin_path, &[], CORE_MAIN_TIMEOUT).expect("failed to spawn core main binary");

    assert!(status.success());
    assert!(stdout.contains("Speaker Core Service is running on Raspberry Pi 3!"));
    assert!(stdout.contains("ARM Cortex-A53 (aarch64)"));
    assert!(stdout.contains("Thermal Sensor:"));
}

#[test]
fn read_temp_cli_help_displays() {
    let bin_path = env!("CARGO_BIN_EXE_read_temp");
    let Some((status, stdout, _)) = probe_help(bin_path) else {
        return;
    };
    assert!(status.success());
    assert!(stdout.contains("DS18B20 1-Wire Temperature Diagnostic Utility"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--count"));
    assert!(stdout.contains("--interval"));

    let (short_status, _, _) = run_cli(bin_path, &["-h"]);
    assert!(short_status.success());
}

/// Shared invalid CLI flag cases common to all `read_*` diagnostic utilities.
const COMMON_INVALID_ARGS: &[(&[&str], &str)] = &[
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
];

fn assert_cli_rejects(bin_path: &str, cases: &[(&[&str], &str)]) {
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
fn read_temp_cli_argument_validation() {
    let bin_path = env!("CARGO_BIN_EXE_read_temp");
    if !can_exec_binary(bin_path) {
        return;
    }

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
fn read_light_cli_help_displays() {
    let bin_path = env!("CARGO_BIN_EXE_read_light");
    let Some((status, stdout, _)) = probe_help(bin_path) else {
        return;
    };
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

    assert_cli_rejects(bin_path, COMMON_INVALID_ARGS);
    assert_cli_rejects(
        bin_path,
        &[
            (&["--mode"], "missing value for --mode"),
            (&["--mode", "ultra-mode"], "invalid mode 'ultra-mode'"),
            (&["--address"], "missing value for --address"),
            (&["--address", "+0x23"], "sign characters are not allowed"),
            (&["--address", "-0x23"], "sign characters are not allowed"),
            (&["--address", "not-hex"], "invalid hex address"),
            (&["--address", "0xzz"], "invalid hex address"),
            (&["--address", "0x00"], "invalid BH1750 I2C address"),
            (&["--address", "0x42"], "invalid BH1750 I2C address"),
            (&["--address", "0x77"], "invalid BH1750 I2C address"),
            (&["--address", "0x80"], "invalid BH1750 I2C address"),
        ],
    );
}
