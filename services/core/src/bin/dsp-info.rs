//! `CamillaDSP` audio processing engine diagnostic and status binary entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::dsp::main()
}
