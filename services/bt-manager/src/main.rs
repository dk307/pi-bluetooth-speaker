//! Bluetooth manager appliance service binary target.
//!
//! The executable binary target is named `bt-manager` in `Cargo.toml`
//! (`[[bin]] name = "bt-manager"`), matching the `DietPi` systemd unit
//! `/etc/systemd/system/bt-manager.service`.

use std::process::ExitCode;

speaker_common::glibc_compat!();

fn main() -> ExitCode {
    // Use current thread runtime to avoid "Cannot start a runtime from within a runtime"
    // when subsystems like cover-art and auto-trust have already started their own runtimes.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime")
        .block_on(bt_manager::daemon::main())
}
