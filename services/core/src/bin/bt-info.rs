//! Bluetooth and `BlueALSA` hardware monitor and diagnostic binary entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::bt::main()
}
