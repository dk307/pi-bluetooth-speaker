//! EC12 rotary encoder diagnostic binary entrypoint.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_diagnostics::cli::encoder::main()
}
