//! MA12070P audio amplifier hardware monitor and diagnostic binary entrypoint.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_diagnostics::cli::amp::main()
}
