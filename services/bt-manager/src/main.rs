//! Bluetooth manager appliance service binary target.
//!
//! The executable binary target is named `bt-manager` in `Cargo.toml`
//! (`[[bin]] name = "bt-manager"`), matching the `DietPi` systemd unit
//! `/etc/systemd/system/bt-manager.service`.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    bt_manager::daemon::main()
}
