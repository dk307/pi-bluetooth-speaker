//! BH1750 ambient light sensor diagnostic utility.
//!
//! Provides CLI diagnostics for the I2C ambient light sensing subsystem on
//! the Raspberry Pi 3 appliance. Displays hardware GPIO pin mappings,
//! and continuously reports live optical illuminance (lux and foot-candles)
//! alongside hardware diagnostics.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_diagnostics::cli::light::main()
}
