//! Single-test binary: mutates `DBUS_SYSTEM_BUS_ADDRESS` without racing peers.
//!
//! `std::env::set_var` is unsound when another thread reads the same variable,
//! so this scenario lives in its own test executable with exactly one test.

use std::process::ExitCode;

use autobright::{AutobrightOptions, daemon};

#[path = "common/log_capture.rs"]
mod log_capture;

use log_capture::LogCapture;

/// Points zbus at a path that cannot exist, forcing `Connection::system()` to
/// fail if the daemon ever reaches the D-Bus bootstrap stage.
fn set_invalid_bus_address() {
    // SAFETY: this executable contains exactly one test, so no sibling test can
    // read or write DBUS_SYSTEM_BUS_ADDRESS while this mutation is in flight.
    unsafe {
        std::env::set_var(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/tmp/opencode/definitely-missing-autobright-bus.sock",
        );
    }
}

#[tokio::test]
async fn main_reports_a_concrete_bootstrap_failure_with_an_invalid_bus_address() {
    let logs = LogCapture::install();
    set_invalid_bus_address();

    let code = daemon::main(AutobrightOptions {
        tick_ms: Some(10),
        duration_secs: Some(1),
        ..AutobrightOptions::default()
    })
    .await;

    assert_eq!(code, ExitCode::FAILURE);
    let possible_branches = [
        "failed to resolve autobright configuration",
        "failed to open BH1750 ambient light sensor",
        "failed to open backlight sysfs device",
        "failed to connect to the system D-Bus",
    ];
    let observed = possible_branches
        .iter()
        .find(|message| logs.contains(message))
        .copied();
    assert!(
        observed.is_some(),
        "main must log a concrete bootstrap failure: {}",
        logs.snapshot()
    );
    eprintln!(
        "autobright::main bootstrap failure branch on this host: {}",
        observed.expect("checked above")
    );
}
