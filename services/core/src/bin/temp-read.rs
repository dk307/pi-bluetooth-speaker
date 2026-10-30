//! DS18B20 1-Wire temperature diagnostic utility.
//!
//! Provides CLI diagnostics for the 1-Wire temperature sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! discovers attached `28-*` DS18B20 digital thermometers, and reports
//! live chassis temperature telemetry, `SoC` core thermal metrics, and sensor hardware
//! diagnostic registers.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::temp::main()
}
