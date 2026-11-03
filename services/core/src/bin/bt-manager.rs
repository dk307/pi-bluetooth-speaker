//! Bluetooth manager appliance service binary target.
//!
//! The executable binary target is named `bt-manager` in `Cargo.toml`
//! (`[[bin]] name = "bt-manager"`), matching the `DietPi` systemd unit
//! `/etc/systemd/system/bt-manager.service`. All application logic lives in
//! [`speaker_core::cli::bt_manager`] so it stays fully unit-testable.

use std::process::ExitCode;

fn main() -> ExitCode {
    speaker_core::cli::bt_manager::main()
}
