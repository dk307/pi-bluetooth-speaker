//! Central speaker daemon and hardware orchestration service for Raspberry Pi 3.
//!
//! # Binary Target Naming
//!
//! The executable binary target is named `core` in `Cargo.toml` (`[[bin]] name = "core"`)
//! matching the `DietPi` systemd unit `/etc/systemd/system/core.service`. To prevent shadowing
//! the standard library's `core` crate in external prelude and dependency graphs, the library
//! crate is named `speaker_core`.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::core_daemon::main()
}
