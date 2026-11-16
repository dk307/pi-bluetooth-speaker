//! Bluetooth devices screen: pure row/update transformations shared by the
//! shell controller and the bt-monitor worker.
//!
//! All functions here are pure so the screen's presentation rules (row fields,
//! category slugs, status labels, passkey formatting) are host-testable without
//! a D-Bus connection or a rendered window.

use speaker_bluetooth::{BtAddress, BtDevice, PairingStatus};

use super::generated::BtDeviceRow;

/// Incremental state pushed from the bt-monitor worker to the Bluetooth devices
/// screen.
///
/// `rows`, `discovered`, and `scanning` are full-state snapshots (rebuilt from
/// the link snapshot each tick); `pairing`, `toast`, and `adapter_alias` are
/// one-shot deltas emitted only when they change, so a consumer can treat
/// `None` as "nothing to update".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BtDevicesUpdate {
    /// Paired or connected devices for the main list.
    pub rows: Vec<BtDeviceRow>,
    /// Discovered, unpaired devices for the Add Device list.
    pub discovered: Vec<BtDeviceRow>,
    /// Whether `BlueZ` discovery is running (adapter's authoritative flag).
    pub scanning: bool,
    /// One-shot pairing state transition, if any this publish.
    pub pairing: Option<PairingStatus>,
    /// One-shot toast text, if any this publish.
    pub toast: Option<Box<str>>,
    /// Adapter alias for the header subtitle, when it changed.
    pub adapter_alias: Option<Box<str>>,
}

/// Shell-side dedup gate: drops consecutive identical publishes so the Slint
/// property setters (and their strip re-rasterization) only run on real change.
#[derive(Debug, Default)]
pub struct BtDevicesController {
    last: Option<BtDevicesUpdate>,
}

impl BtDevicesController {
    /// Returns `true` when `update` differs from the last accepted one and
    /// records it as the new baseline. Consecutive identical updates are
    /// dropped — except one-shot events (pairing transitions, toasts), which
    /// are always delivered: a repeated refusal toast or pairing terminal is a
    /// fresh user-visible event, not redundant state.
    pub fn accepts(&mut self, update: &BtDevicesUpdate) -> bool {
        let one_shot = update.pairing.is_some() || update.toast.is_some();
        if !one_shot && self.last.as_ref() == Some(update) {
            return false;
        }
        self.last = Some(update.clone());
        true
    }
}

/// Maps a `BlueZ` `Device1.Icon`/`Class` hint to an on-disk asset slug.
///
/// Unknown or missing hints fall back to the generic Bluetooth glyph.
#[must_use]
pub fn icon_slug(icon: Option<&str>) -> &'static str {
    match icon.unwrap_or_default() {
        "phone" | "telephone" => "phone",
        "computer" | "laptop" | "desktop" => "laptop",
        "tablet" => "tablet",
        "audio-card" | "headset" | "headphones" | "speaker" => "headset",
        _ => "bluetooth",
    }
}

/// Human category label paired with the icon in card subtitles.
fn category_label(icon: Option<&str>) -> &'static str {
    match icon_slug(icon) {
        "phone" => "Phone",
        "laptop" => "Laptop",
        "tablet" => "Tablet",
        "headset" => "Audio",
        _ => "Device",
    }
}

/// Card subtitle: category label plus hardware address, e.g. `"Phone · AA:BB:CC:DD:EE:FF"`.
#[must_use]
pub fn subtitle_text(device: &BtDevice) -> String {
    format!(
        "{} · {}",
        category_label(device.icon.as_deref()),
        device.address
    )
}

/// Card status label: the active/connected device reads `Active`, other
/// paired devices `Paired`, discovery rows `Available`.
#[must_use]
pub fn status_text(device: &BtDevice, active: bool) -> &'static str {
    if active || device.connected {
        "Active"
    } else if device.paired {
        "Paired"
    } else {
        "Available"
    }
}

/// Six-digit zero-padded pairing passkey (`482910`), per the spec layout.
#[must_use]
pub fn pairing_code_text(passkey: u32) -> String {
    format!("{passkey:06}")
}

/// Builds a screen row for `device`, marking it active when it is the
/// currently selected output device.
#[must_use]
pub fn device_row(device: &BtDevice, active_address: Option<BtAddress>) -> BtDeviceRow {
    let active = active_address == Some(device.address);
    BtDeviceRow {
        address: device.address.to_string().into(),
        name: device.display_name().into(),
        subtitle: subtitle_text(device).into(),
        status: status_text(device, active).into(),
        icon: icon_slug(device.icon.as_deref()).into(),
        connected: device.connected,
        paired: device.paired,
    }
}
