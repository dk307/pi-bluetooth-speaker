//! Active Bluetooth audio source: the single connected device allowed to play.
//!
//! Several phones/laptops may stay connected (A2DP + AVRCP) at once, but only
//! one stream may reach the DSP. The hard audio gate is
//! `bluealsa-aplay --single-audio`: the first PCM that starts streaming owns
//! the ALSA output until its `Running` property drops, every other stream is
//! read and discarded. `bluealsa-aplay` never publishes which worker won, so
//! `bt-manager` mirrors the same first-come rule and publishes the result on
//! the system bus for every other consumer (volume sync, cover art, screen UI,
//! transport controls):
//!
//! - bus name [`ACTIVE_SOURCE_DBUS_NAME`] (`org.speaker.Bluetooth`)
//! - object path [`ACTIVE_SOURCE_OBJECT_PATH`] (`/org/speaker/bluetooth`)
//! - interface [`ACTIVE_SOURCE_INTERFACE`] (`org.speaker.Bluetooth1`)
//! - property [`ACTIVE_DEVICE_PROPERTY`] (`s`): canonical MAC of the active
//!   device, or the empty string when nothing streams.

use crate::connection::{BtAddress, DbusClient};

/// Well-known bus name owned by `bt-manager` for the active-source service.
pub const ACTIVE_SOURCE_DBUS_NAME: &str = "org.speaker.Bluetooth";

/// Object path serving the active-source interface.
pub const ACTIVE_SOURCE_OBJECT_PATH: &str = "/org/speaker/bluetooth";

/// D-Bus interface exposing the active-source state.
pub const ACTIVE_SOURCE_INTERFACE: &str = "org.speaker.Bluetooth1";

/// Property carrying the active device MAC (empty string when none).
pub const ACTIVE_DEVICE_PROPERTY: &str = "ActiveDevice";

/// Where a [`crate::BluetoothDevice`] handle learns the active source from.
///
/// A sum type rather than `Option<Option<BtAddress>>`: "not the arbiter" and
/// "the arbiter says nothing streams" are different states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ActiveSourceMode {
    /// Consumer process: read [`ACTIVE_DEVICE_PROPERTY`] from `bt-manager`.
    #[default]
    Remote,
    /// Arbiter process (`bt-manager`): the value is maintained locally.
    Local(Option<BtAddress>),
}

/// Parses a published [`ACTIVE_DEVICE_PROPERTY`] value.
///
/// The empty string means "no active device"; a malformed value is treated
/// the same way so consumers fall back to their own heuristic instead of
/// pinning a bogus device.
#[must_use]
pub fn parse_active_device(raw: &str) -> Option<BtAddress> {
    if raw.is_empty() {
        return None;
    }
    BtAddress::parse_str(raw).ok()
}

/// Formats an active device for the [`ACTIVE_DEVICE_PROPERTY`] wire value.
#[must_use]
pub fn format_active_device(device: Option<BtAddress>) -> String {
    device.map_or_else(String::new, |d| d.to_string())
}

/// Extracts the remote device address from a `BlueALSA` PCM or `BlueZ`
/// object path (`.../dev_AA_BB_CC_DD_EE_FF/...`).
#[must_use]
pub fn device_address_from_path(path: &str) -> Option<BtAddress> {
    DbusClient::extract_mac_from_pcm_path(path).and_then(|mac| BtAddress::parse_str(&mac).ok())
}
