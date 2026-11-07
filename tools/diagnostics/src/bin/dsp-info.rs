//! `CamillaDSP` audio processing engine diagnostic and status binary entrypoint.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    speaker_diagnostics::cli::dsp::main()
}
