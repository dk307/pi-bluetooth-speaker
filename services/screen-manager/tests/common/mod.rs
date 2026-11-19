//! Shared integration-test helpers for the screen-manager Bluetooth suites.
//!
//! Integration tests compile as separate crates, so helpers used by more than
//! one `tests/*.rs` binary live here and are pulled in via `mod common;`.

use std::sync::Mutex;
use std::time::Duration;

use screen_manager::daemon::{BtMonitorState, BtSharedUpdate};
use screen_manager::ui::BtDevicesUpdate;
use speaker_bluetooth::{BluetoothDevice, DeviceCommandQueue, TransportCommandQueue};

/// Drives one full worker tick and returns the staged screen update.
pub fn tick(
    state: &mut BtMonitorState,
    bt: &BluetoothDevice,
    shared: &Mutex<BtSharedUpdate>,
    transport: &TransportCommandQueue,
    commands: &DeviceCommandQueue,
    pairing_timeout: Duration,
) -> Option<BtDevicesUpdate> {
    assert!(
        state.poll_tick(bt, shared, transport, commands, pairing_timeout),
        "a tick only fails when a transport command interrupts it"
    );
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .devices
        .take()
}
