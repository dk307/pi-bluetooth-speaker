//! MA12070P audio amplifier hardware monitor and diagnostic binary entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::amp::main()
}
