//! EC12 rotary encoder diagnostic binary entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::encoder::main()
}
