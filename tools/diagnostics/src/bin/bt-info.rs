//! Bluetooth and `BlueALSA` hardware monitor and diagnostic binary entrypoint.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_diagnostics::cli::bt::main()
}
