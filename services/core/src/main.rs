//! Central speaker daemon and hardware orchestration service for Raspberry Pi 3.
//!
//! # Binary Target Naming
//!
//! The executable binary target is named `core` in `Cargo.toml` (`[[bin]] name = "core"`)
//! matching the `DietPi` systemd unit `/etc/systemd/system/core.service`. To prevent shadowing
//! the standard library's `core` crate in external prelude and dependency graphs, the library
//! crate is named `speaker_core`.

use speaker_core::hardware::TempSensor;

/// Entrypoint for the `core` binary.
///
/// Contract: currently prints the hardware diagnostic startup banner and exits cleanly (`0`).
/// Subprocess test `tests/cli_tests.rs::core_main_binary_executes` relies on this finite exit;
/// if a long-running event loop is added here, provide a `--check` / `--once` flag for CI probes.
fn main() {
    let temp_sensor = TempSensor::new();

    println!("============================================================");
    println!("  Speaker Core Service is running on Raspberry Pi 3!");
    println!("  Target: ARM Cortex-A53 (aarch64)");
    println!("  Thermal Sensor: {}", temp_sensor.gpio_info());
    println!("============================================================");
}
