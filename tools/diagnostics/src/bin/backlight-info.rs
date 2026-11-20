//! Backlight sysfs diagnostic utility.
//!
//! Reports the configured kernel backlight device's raw and maximum brightness,
//! derived percentage, and optional blanking power state as human-readable
//! output or an NDJSON telemetry stream.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_diagnostics::cli::backlight::main()
}
