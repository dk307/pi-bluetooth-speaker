//! Touchscreen Appliance Console & Screen Manager service for Raspberry Pi 3.
//!
//! The executable binary target is named `screen-manager` in `Cargo.toml`
//! (`[[bin]] name = "screen-manager"`), matching the `DietPi` systemd unit
//! `/etc/systemd/system/screen-manager.service`.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    screen_manager::daemon::main()
}
