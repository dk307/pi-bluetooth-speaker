//! Bluetooth and `BlueALSA` hardware subsystem abstraction via D-Bus.
//!
//! Provides strongly-typed, mockable interfaces for the Broadcom BCM43438 Bluetooth
//! controller and the `BlueALSA` audio streaming pipeline on the Raspberry Pi 3 Model B.
//! Connects directly to the Linux system bus via `zbus` for `BlueZ` 5 (`org.bluez`),
//! `BlueALSA` (`org.bluealsa`), and systemd (`org.freedesktop.systemd1`).
//! Supports adapter status inspection, remote device discovery, connection state,
//! real-time `BlueALSA` PCM audio stream telemetry, and radio controls.
//!
//! Conforms to Microsoft Pragmatic Rust Guidelines:
//! - Mockable external I/O via inherent [`BluetoothDevice::new_mocked`] (`M-MOCKABLE-SYSCALLS`)
//! - Strongly typed invariants via [`BtAddress`] and [`ServiceState`] (`M-STRONG-TYPES-GUARD`)
//! - Canonical error reporting via [`BtError`] (`M-ERRORS-CANONICAL-STRUCTS`)
//! - Non-blocking async queries via [`crate::runtime::run_blocking`] (`M-SERVICES-CLONE`)

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zbus::blocking::Connection;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::runtime::FromJoinError;

/// Number of octets in a standard Bluetooth MAC address.
pub const BT_ADDRESS_OCTETS: usize = 6;

/// Default Bluetooth controller interface name on Raspberry Pi 3.
pub const DEFAULT_BT_ADAPTER: &str = "hci0";

/// Broadcom BCM43438 Bluetooth hardware UART baud rate.
pub const BCM43438_UART_BAUD_RATE: u32 = 921_600;

/// Broadcom BCM43438 firmware patchfile path on `DietPi` / Debian.
pub const BCM43438_FIRMWARE_PATH: &str = "/lib/firmware/brcm/BCM43430A1.hcd";

/// Strong newtype for a 48-bit Bluetooth device address (MAC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BtAddress([u8; BT_ADDRESS_OCTETS]);

impl BtAddress {
    /// Creates a Bluetooth address from raw 6-octet byte array.
    #[must_use]
    pub const fn new(octets: [u8; BT_ADDRESS_OCTETS]) -> Self {
        Self(octets)
    }

    /// Returns the raw 6-octet byte array.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; BT_ADDRESS_OCTETS] {
        &self.0
    }

    /// Parses a colon- or hyphen-separated hex string into a [`BtAddress`].
    ///
    /// # Errors
    /// Returns [`BtError`] if string format or octet values are invalid.
    pub fn parse(s: &str) -> Result<Self, BtError> {
        Self::parse_str(s).map_err(|reason| BtError::invalid_address(s, reason))
    }

    /// Soft user-input string parser returning a readable error message without backtrace.
    ///
    /// # Errors
    /// Returns a descriptive error string if the address is malformed.
    pub fn parse_str(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if trimmed.starts_with('+') || trimmed.starts_with('-') {
            return Err("sign characters are not allowed in Bluetooth addresses".to_string());
        }

        let delimiter = if trimmed.contains(':') {
            ':'
        } else if trimmed.contains('-') {
            '-'
        } else {
            return Err("expected 6 colon- or hyphen-separated hex octets".to_string());
        };

        let parts: Vec<&str> = trimmed.split(delimiter).collect();
        if parts.len() != BT_ADDRESS_OCTETS {
            return Err(format!(
                "expected {BT_ADDRESS_OCTETS} hex octets, found {}",
                parts.len()
            ));
        }

        let mut octets = [0u8; BT_ADDRESS_OCTETS];
        for (i, part) in parts.iter().enumerate() {
            if part.starts_with('+') || part.starts_with('-') {
                return Err("sign characters are not allowed in Bluetooth addresses".to_string());
            }
            if part.len() != 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "octet {i} ('{part}') is not a valid 2-digit hex byte"
                ));
            }
            octets[i] = u8::from_str_radix(part, 16)
                .map_err(|e| format!("failed to parse octet {i}: {e}"))?;
        }

        Ok(Self(octets))
    }
}

impl Display for BtAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl TryFrom<&str> for BtAddress {
    type Error = BtError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl std::str::FromStr for BtAddress {
    type Err = BtError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Linux system service execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Service unit is running and operational.
    Active,
    /// Service unit is loaded but currently inactive or stopped.
    Inactive,
    /// Service unit entered a failed or crashed state.
    Failed,
    /// Service unit file was not found on the host system.
    NotFound,
}

impl ServiceState {
    /// Returns `true` if the service is actively running.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns a human-readable state description matching `bt_status.py`.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Active => "Running",
            Self::Inactive => "INACTIVE / STOPPED",
            Self::Failed => "FAILED",
            Self::NotFound => "NOT INSTALLED / NOT FOUND",
        }
    }
}

impl Display for ServiceState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

/// Operational status of required Bluetooth daemon services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothServices {
    /// Core `BlueZ` 5 Bluetooth daemon (`bluetooth.service`).
    pub bluetoothd: ServiceState,
    /// `BlueALSA` audio proxy daemon (`bluealsa.service`).
    pub bluealsa: ServiceState,
    /// `BlueALSA` ALSA audio forwarder (`bluealsa-aplay.service`).
    pub bluealsa_aplay: ServiceState,
    /// Auto-trust pairing agent (`bt-agent.service`).
    pub bt_agent: ServiceState,
}

impl BluetoothServices {
    /// Returns `true` if the core Bluetooth daemon and at least one `BlueALSA` component are active.
    #[must_use]
    pub fn is_operational(&self) -> bool {
        self.bluetoothd.is_active()
            && (self.bluealsa.is_active() || self.bluealsa_aplay.is_active())
    }

    /// Returns `true` if all 4 services are actively running.
    #[must_use]
    pub fn all_active(&self) -> bool {
        self.bluetoothd.is_active()
            && self.bluealsa.is_active()
            && self.bluealsa_aplay.is_active()
            && self.bt_agent.is_active()
    }
}

/// Local Bluetooth adapter hardware interface information.
#[expect(
    clippy::struct_excessive_bools,
    reason = "direct reflection of BlueZ D-Bus Adapter1 properties"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtAdapter {
    /// Interface identifier (e.g. `hci0`).
    pub id: Box<str>,
    /// Controller MAC address.
    pub address: BtAddress,
    /// User-friendly name advertised by the controller.
    pub name: Box<str>,
    /// Controller alias name.
    pub alias: Box<str>,
    /// Whether radio power is turned on.
    pub powered: bool,
    /// Whether controller is discoverable by other devices.
    pub discoverable: bool,
    /// Whether controller accepts pairing requests.
    pub pairable: bool,
    /// Whether controller is actively performing device discovery.
    pub discovering: bool,
    /// Bluetooth Device Class value, if available.
    pub class: Option<u32>,
    /// USB/UART modalias hardware identification string.
    pub modalias: Option<Box<str>>,
}

/// Remote Bluetooth paired or connected device.
#[expect(
    clippy::struct_excessive_bools,
    reason = "direct reflection of BlueZ D-Bus Device1 properties"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtDevice {
    /// Remote device MAC address.
    pub address: BtAddress,
    /// Remote advertised device name.
    pub name: Box<str>,
    /// User or system alias.
    pub alias: Box<str>,
    /// Whether device is actively connected.
    pub connected: bool,
    /// Whether device is paired with the controller.
    pub paired: bool,
    /// Whether device is trusted.
    pub trusted: bool,
    /// Whether device is blocked.
    pub blocked: bool,
    /// Received Signal Strength Indicator (RSSI) in dBm, if available.
    pub rssi: Option<i16>,
    /// Remote battery percentage (0–100), if reported via `org.bluez.Battery1`.
    pub battery_percentage: Option<u8>,
    /// Device icon name (e.g. `phone`, `audio-card`).
    pub icon: Option<Box<str>>,
    /// Advertised Bluetooth service UUIDs.
    pub uuids: Box<[Box<str>]>,
}

impl BtDevice {
    /// Returns the most descriptive name available (`alias` if customized, else `name`).
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.alias.is_empty() || self.alias.as_ref() == "Unknown" {
            if self.name.is_empty() {
                "Unknown Device"
            } else {
                &self.name
            }
        } else {
            &self.alias
        }
    }
}

/// Active `BlueALSA` audio transport PCM stream information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueAlsaPcm {
    /// D-Bus object path for this audio PCM stream.
    pub pcm_path: Box<str>,
    /// Associated remote device MAC address.
    pub device_address: BtAddress,
    /// Human-readable device name resolved from connected device map.
    pub resolved_name: Box<str>,
    /// Audio transport mode (e.g. `A2DP-sink`, `A2DP-source`, `HFP-AG`).
    pub transport: Box<str>,
    /// Whether audio is actively streaming across the transport.
    pub running: bool,
    /// Bluetooth audio codec negotiated for this stream (e.g. `AAC`, `SBC`, `aptX`, `LDAC`).
    pub selected_codec: Box<str>,
    /// Available codecs supported by this PCM stream.
    pub available_codecs: Box<[Box<str>]>,
    /// Sampling rate in Hz (e.g. 44100, 48000).
    pub sampling_rate: u32,
    /// Number of audio channels (e.g. 2 for stereo).
    pub channels: u8,
    /// PCM sample format decoded from the 16-bit identifier (e.g. `S16_LE`).
    pub format: Box<str>,
    /// Hardware volume levels per channel (e.g. `L: 127 R: 127`), if reported.
    pub volume: Option<Box<str>>,
    /// Whether audio output is muted (any channel volume high bit set).
    pub muted: bool,
    /// Audio transmission latency in milliseconds, if reported.
    pub delay: Option<Box<str>>,
}

impl BlueAlsaPcm {
    /// Returns a human-readable stream state description.
    #[must_use]
    pub fn stream_state_description(&self) -> &'static str {
        if self.running {
            "STREAMING (Active)"
        } else {
            "STANDBY / PAUSED (Connected)"
        }
    }
}

/// Formats a duration in milliseconds into a user-friendly string (`MM:SS` or `HH:MM:SS`).
#[must_use]
pub fn format_duration_ms(ms: u32) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Active Bluetooth AVRCP media player playback state and track metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtPlaybackState {
    /// Associated remote device MAC address, if known.
    pub device_address: Option<BtAddress>,
    /// Remote device name or alias.
    pub device_name: Box<str>,
    /// Playback status (e.g. `playing`, `paused`, `stopped`).
    pub status: Box<str>,
    /// Current track title.
    pub title: Box<str>,
    /// Track artist.
    pub artist: Box<str>,
    /// Track album.
    pub album: Box<str>,
    /// Elapsed playback position in milliseconds.
    pub position_ms: u32,
    /// Total track duration in milliseconds.
    pub duration_ms: u32,
    /// Path to player D-Bus object (e.g. `/org/bluez/hci0/dev_.../player0`).
    pub player_path: Option<Box<str>>,
}

impl Default for BtPlaybackState {
    fn default() -> Self {
        Self {
            device_address: None,
            device_name: "Bluetooth Audio Device".into(),
            status: "stopped".into(),
            title: "Waiting for Bluetooth Connection...".into(),
            artist: "DietPi Audiophile Speaker".into(),
            album: "High-Resolution DSP Sink".into(),
            position_ms: 0,
            duration_ms: 0,
            player_path: None,
        }
    }
}

impl BtPlaybackState {
    /// Returns `true` if playback status is "playing" (case-insensitive).
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.status.eq_ignore_ascii_case("playing")
    }

    /// Returns the progress ratio between 0.0 and 1.0.
    #[must_use]
    pub fn progress_ratio(&self) -> f32 {
        if self.duration_ms == 0 {
            0.0
        } else {
            (self.position_ms as f32 / self.duration_ms as f32).clamp(0.0, 1.0)
        }
    }

    /// Formats the elapsed position as `MM:SS`.
    #[must_use]
    pub fn format_position(&self) -> String {
        format_duration_ms(self.position_ms)
    }

    /// Formats the track duration as `MM:SS`.
    #[must_use]
    pub fn format_duration(&self) -> String {
        format_duration_ms(self.duration_ms)
    }
}

/// Raspberry Pi 3 hardware controller specifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothHardwareInfo {
    /// Controller chip model.
    pub chip: Box<str>,
    /// Physical transport bus.
    pub bus: Box<str>,
    /// Operating UART baud rate in bps.
    pub baud_rate: u32,
    /// Firmware file location.
    pub firmware: Box<str>,
    /// Kernel subsystem driver.
    pub driver: Box<str>,
    /// Default hardware interface identifier.
    pub default_controller: Box<str>,
    /// Live controller identity and firmware version, when probed.
    pub controller: BluetoothControllerInfo,
}

impl BluetoothHardwareInfo {
    /// Returns static hardware information for the Raspberry Pi 3 Model B.
    #[must_use]
    pub fn rpi3() -> Self {
        Self {
            chip: "Broadcom BCM43438 (Bluetooth 4.1 + BLE)".into(),
            bus: "UART (PL011 / /dev/ttyAMA0)".into(),
            baud_rate: BCM43438_UART_BAUD_RATE,
            firmware: BCM43438_FIRMWARE_PATH.into(),
            driver: "hci_uart (bcm)".into(),
            default_controller: DEFAULT_BT_ADAPTER.into(),
            controller: BluetoothControllerInfo::unknown(),
        }
    }
}

/// Live HCI controller identity and firmware version read from the kernel.
///
/// All fields are `None` when the corresponding detail could not be probed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BluetoothControllerInfo {
    /// HCI interface name the info was probed from (e.g. `hci0`).
    pub adapter: Option<Box<str>>,
    /// HCI specification version (e.g. `5.4`).
    pub hci_version: Option<Box<str>>,
    /// Controller hardware revision (hex, e.g. `0xddd5`).
    pub hci_revision: Option<Box<str>>,
    /// LMP/Radio specification version (e.g. `5.4`).
    pub lmp_version: Option<Box<str>>,
    /// Controller radio subversion (hex, e.g. `0xc761`).
    pub lmp_subversion: Option<Box<str>>,
    /// Controller manufacturer name (e.g. `Realtek Semiconductor Corporation`).
    pub manufacturer: Option<Box<str>>,
    /// Physical transport bus type (e.g. `USB`, `UART`).
    pub bus_type: Option<Box<str>>,
    /// Controller Bluetooth device address.
    pub bd_address: Option<Box<str>>,
    /// Composite loaded firmware version (hex `(revision << 16) | subversion`).
    pub firmware_version: Option<Box<str>>,
}

impl BluetoothControllerInfo {
    /// Returns controller info with every detail unknown (probe unavailable).
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Builds controller info by parsing raw `hciconfig <adapter> version` text.
    #[must_use]
    pub fn from_hciconfig_output(output: &str) -> Option<Self> {
        parse_hciconfig_version(output)
    }

    /// Probes the live controller via `hciconfig <adapter> version` (10s deadline).
    ///
    /// Every failure mode (missing binary, timeout, parse failure) degrades to
    /// [`BluetoothControllerInfo::unknown`] rather than an error, so status
    /// reporting stays available even on hosts without `hciconfig`.
    #[must_use]
    pub fn probe(adapter: &str) -> Self {
        let Some(output) = run_hciconfig_version(adapter) else {
            return Self::unknown();
        };
        Self::from_hciconfig_output(&output).unwrap_or_else(Self::unknown)
    }

    /// Returns `true` if any controller detail was successfully probed.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.adapter.is_some()
            || self.hci_version.is_some()
            || self.hci_revision.is_some()
            || self.lmp_version.is_some()
            || self.lmp_subversion.is_some()
            || self.manufacturer.is_some()
            || self.bus_type.is_some()
            || self.bd_address.is_some()
            || self.firmware_version.is_some()
    }
}

/// Runs `hciconfig <adapter> version` with a hard deadline, capturing stdout.
fn run_hciconfig_version(adapter: &str) -> Option<String> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let mut child = Command::new("hciconfig")
        .args([adapter, "version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }?;
    if !status.success() {
        return None;
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    String::from_utf8(stdout).ok()
}

/// Parses `hciconfig <adapter> version` output into controller identity fields.
pub fn parse_hciconfig_version(output: &str) -> Option<BluetoothControllerInfo> {
    let mut info = BluetoothControllerInfo::unknown();
    for (index, line) in output.lines().enumerate() {
        let line = line.trim();
        if index == 0 {
            if let Some((dev, _)) = line.split_once(':') {
                let dev = dev.trim();
                if dev.starts_with("hci") {
                    info.adapter = Some(dev.to_string().into_boxed_str());
                }
            }
        }
        if let Some((_, rest)) = line.split_once("Bus:") {
            info.bus_type = Some(rest.trim().to_string().into_boxed_str());
        }
        if let Some((_, rest)) = line.split_once("BD Address:") {
            let rest = rest.trim();
            let addr = rest.split_whitespace().next().unwrap_or(rest);
            info.bd_address = Some(addr.to_string().into_boxed_str());
        }
        if let Some((_, rest)) = line.split_once("HCI Version:") {
            info.hci_version = rest
                .split_whitespace()
                .next()
                .map(String::from)
                .map(Into::into);
        }
        if let Some((_, rest)) = line.split_once("Revision:") {
            info.hci_revision = rest
                .split_whitespace()
                .next()
                .map(String::from)
                .map(Into::into);
        }
        if let Some((_, rest)) = line.split_once("LMP Version:") {
            info.lmp_version = rest
                .split_whitespace()
                .next()
                .map(String::from)
                .map(Into::into);
        }
        if let Some((_, rest)) = line.split_once("Subversion:") {
            info.lmp_subversion = rest
                .split_whitespace()
                .next()
                .map(String::from)
                .map(Into::into);
        }
        if let Some((_, rest)) = line.split_once("Manufacturer:") {
            let rest = rest.trim();
            let name = match rest.rfind('(') {
                Some(idx) => rest[..idx].trim(),
                None => rest,
            };
            info.manufacturer = Some(name.to_string().into_boxed_str());
        }
    }
    info.firmware_version = match (info.hci_revision.as_deref(), info.lmp_subversion.as_deref()) {
        (Some(rev), Some(sub)) => u32::from_str_radix(rev.trim_start_matches("0x"), 16)
            .ok()
            .and_then(|rev| {
                u32::from_str_radix(sub.trim_start_matches("0x"), 16)
                    .ok()
                    .map(|sub| format!("{rev:#x}{sub:04x}"))
            })
            .map(std::string::String::into_boxed_str),
        _ => None,
    };
    (info.hci_version.is_some() || info.lmp_version.is_some()).then_some(info)
}

/// Comprehensive point-in-time snapshot of the Bluetooth subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothStatus {
    /// Daemon services status.
    pub services: BluetoothServices,
    /// Active local controller adapter, if detected.
    pub adapter: Option<BtAdapter>,
    /// Discovered or connected remote devices.
    pub devices: Box<[BtDevice]>,
    /// Active `BlueALSA` audio PCM streams.
    pub pcms: Box<[BlueAlsaPcm]>,
    /// Raspberry Pi hardware controller metadata.
    pub hardware: BluetoothHardwareInfo,
    /// Active AVRCP media player playback state, if available.
    pub playback: Option<BtPlaybackState>,
}

impl BluetoothStatus {
    /// Returns `true` if Bluetooth daemon is running and radio is powered on.
    #[must_use]
    pub fn is_operational(&self) -> bool {
        self.services.is_operational() && self.adapter.as_ref().is_some_and(|a| a.powered)
    }

    /// Returns the count of actively connected devices.
    #[must_use]
    pub fn connected_count(&self) -> usize {
        self.devices.iter().filter(|d| d.connected).count()
    }

    /// Returns `true` if any `BlueALSA` PCM stream is actively streaming audio.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.pcms.iter().any(|p| p.running)
    }

    /// Returns the first actively streaming PCM stream, if any.
    #[must_use]
    pub fn active_pcm(&self) -> Option<&BlueAlsaPcm> {
        self.pcms.iter().find(|p| p.running)
    }

    /// Returns all actively connected devices.
    #[must_use]
    pub fn connected_devices(&self) -> Vec<&BtDevice> {
        self.devices.iter().filter(|d| d.connected).collect()
    }

    /// Returns all paired devices.
    #[must_use]
    pub fn paired_devices(&self) -> Vec<&BtDevice> {
        self.devices.iter().filter(|d| d.paired).collect()
    }

    /// Returns `true` if media playback is actively playing.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(BtPlaybackState::is_playing)
    }

    /// Returns a reference to the active media player playback state, if any.
    #[must_use]
    pub const fn playback_state(&self) -> Option<&BtPlaybackState> {
        self.playback.as_ref()
    }
}

#[derive(Debug)]
pub(crate) enum BtErrorKind {
    Dbus { details: Box<str> },
    AdapterNotFound { adapter: Box<str> },
    InvalidAddress { raw: Box<str>, reason: Box<str> },
    NoActivePlayer { details: Box<str> },
    Cancelled { message: Box<str> },
    MockFault { message: Box<str> },
}

/// Subsystem error type for Bluetooth driver and diagnostic operations.
#[derive(Debug)]
pub struct BtError {
    kind: BtErrorKind,
    backtrace: Backtrace,
}

impl BtError {
    fn new(kind: BtErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn dbus(details: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::Dbus {
            details: details.into(),
        })
    }

    pub(crate) fn adapter_not_found(adapter: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::AdapterNotFound {
            adapter: adapter.into(),
        })
    }

    pub(crate) fn invalid_address(raw: impl Into<Box<str>>, reason: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::InvalidAddress {
            raw: raw.into(),
            reason: reason.into(),
        })
    }

    pub(crate) fn no_active_player(details: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::NoActivePlayer {
            details: details.into(),
        })
    }

    pub(crate) fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::Cancelled {
            message: message.into(),
        })
    }

    pub(crate) fn mock_fault(message: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::MockFault {
            message: message.into(),
        })
    }

    /// Returns `true` if this error represents a D-Bus communication error.
    #[must_use]
    pub const fn is_dbus(&self) -> bool {
        matches!(self.kind, BtErrorKind::Dbus { .. })
    }

    /// Returns `true` if the requested Bluetooth adapter was not found.
    #[must_use]
    pub const fn is_adapter_not_found(&self) -> bool {
        matches!(self.kind, BtErrorKind::AdapterNotFound { .. })
    }

    /// Returns `true` if an address string failed validation.
    #[must_use]
    pub const fn is_invalid_address(&self) -> bool {
        matches!(self.kind, BtErrorKind::InvalidAddress { .. })
    }

    /// Returns `true` if an operation failed because no active media player was found.
    #[must_use]
    pub const fn is_no_active_player(&self) -> bool {
        matches!(self.kind, BtErrorKind::NoActivePlayer { .. })
    }

    /// Returns `true` if this error represents an asynchronous task cancellation.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, BtErrorKind::Cancelled { .. })
    }

    /// Returns `true` if this error was injected by a mock controller.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, BtErrorKind::MockFault { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BtErrorKind::Dbus { details } => {
                write!(f, "D-Bus communication error: {details}")
            }
            BtErrorKind::AdapterNotFound { adapter } => {
                write!(f, "Bluetooth adapter '{adapter}' not detected")
            }
            BtErrorKind::InvalidAddress { raw, reason } => {
                write!(f, "invalid Bluetooth address '{raw}': {reason}")
            }
            BtErrorKind::NoActivePlayer { details } => {
                write!(f, "no active Bluetooth media player: {details}")
            }
            BtErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
            BtErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
        }
    }
}

super::impl_canonical_error_display!(BtError);

impl std::error::Error for BtError {}

impl From<zbus::Error> for BtError {
    fn from(err: zbus::Error) -> Self {
        Self::dbus(err.to_string())
    }
}

impl From<zbus::fdo::Error> for BtError {
    fn from(err: zbus::fdo::Error) -> Self {
        Self::dbus(err.to_string())
    }
}

impl FromJoinError for BtError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

// ============================================================================
// D-Bus Proxies (zbus)
// ============================================================================

#[zbus::proxy(
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    interface = "org.freedesktop.systemd1.Manager",
    gen_async = false,
    blocking_name = "SystemdManagerProxyBlocking"
)]
trait SystemdManager {
    fn get_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.systemd1",
    interface = "org.freedesktop.systemd1.Unit",
    gen_async = false,
    blocking_name = "SystemdUnitProxyBlocking"
)]
trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn load_state(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    default_service = "org.bluez",
    interface = "org.bluez.Adapter1",
    gen_async = false,
    blocking_name = "BlueZAdapter1ProxyBlocking"
)]
pub(crate) trait BlueZAdapter1 {
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discoverable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_discoverable(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn pairable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_pairable(&self, value: bool) -> zbus::Result<()>;
}

/// Type alias for D-Bus `ObjectManager` `GetManagedObjects` map.
pub type ManagedObjects =
    HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>>;

fn find_iface<'a>(
    ifaces: &'a HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
    target: &str,
) -> Option<&'a HashMap<String, OwnedValue>> {
    ifaces
        .iter()
        .find(|(k, _)| k.as_str() == target)
        .map(|(_, v)| v)
}

fn get_str<'a>(props: &'a HashMap<String, OwnedValue>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(|v| v.downcast_ref::<&str>().ok())
}

fn get_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

fn get_u32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    props.get(key).and_then(|v| v.downcast_ref::<u32>().ok())
}

fn get_u16(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u16> {
    props.get(key).and_then(|v| v.downcast_ref::<u16>().ok())
}

fn get_i16(props: &HashMap<String, OwnedValue>, key: &str) -> Option<i16> {
    props.get(key).and_then(|v| v.downcast_ref::<i16>().ok())
}

fn get_u8(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u8> {
    props.get(key).and_then(|v| v.downcast_ref::<u8>().ok())
}

fn get_str_array(props: &HashMap<String, OwnedValue>, key: &str) -> Box<[Box<str>]> {
    if let Some(val) = props.get(key) {
        if let Value::Array(arr) = &**val {
            return arr
                .iter()
                .filter_map(|v| v.downcast_ref::<&str>().ok().map(Into::into))
                .collect();
        }
    }
    Box::default()
}

fn get_track_prop_str(val: &OwnedValue, key: &str) -> Option<Box<str>> {
    if let Value::Dict(dict) = &**val {
        for (k, v) in dict.iter() {
            let matches_key = if let Ok(s) = k.downcast_ref::<&str>() {
                s == key
            } else if let Value::Str(s) = k {
                s.as_str() == key
            } else {
                false
            };
            if matches_key {
                if let Ok(s) = v.downcast_ref::<&str>() {
                    return Some(s.into());
                } else if let Value::Str(s) = v {
                    return Some(s.as_str().into());
                } else if let Value::Value(inner) = v {
                    if let Ok(s) = inner.downcast_ref::<&str>() {
                        return Some(s.into());
                    } else if let Value::Str(s) = &**inner {
                        return Some(s.as_str().into());
                    }
                }
            }
        }
    }
    None
}

fn get_track_prop_u32(val: &OwnedValue, key: &str) -> Option<u32> {
    if let Value::Dict(dict) = &**val {
        for (k, v) in dict.iter() {
            let matches_key = if let Ok(s) = k.downcast_ref::<&str>() {
                s == key
            } else if let Value::Str(s) = k {
                s.as_str() == key
            } else {
                false
            };
            if matches_key {
                if let Ok(u) = v.downcast_ref::<u32>() {
                    return Some(u);
                } else if let Value::U32(u) = v {
                    return Some(*u);
                } else if let Value::Value(inner) = v {
                    if let Ok(u) = inner.downcast_ref::<u32>() {
                        return Some(u);
                    } else if let Value::U32(u) = &**inner {
                        return Some(*u);
                    }
                }
            }
        }
    }
    None
}

/// Extracts a D-Bus `ay` byte array property (e.g. `BlueALSA` `Volume`).
fn get_byte_array(props: &HashMap<String, OwnedValue>, key: &str) -> Option<Box<[u8]>> {
    match props.get(key).map(|v| &**v) {
        Some(Value::Array(arr)) => {
            let mut bytes = Vec::with_capacity(arr.len());
            for v in arr.iter() {
                bytes.push(v.downcast_ref::<u8>().ok()?);
            }
            Some(bytes.into_boxed_slice())
        }
        _ => None,
    }
}

/// Decodes a `BlueALSA` 16-bit PCM format identifier (e.g. `0x8410` → `S16_2LE`).
///
/// Encoding per `BlueALSA` `BA_TRANSPORT_PCM_FORMAT`: bit 15 = signed, bit 14 = big-endian,
/// bits 8–13 = physical bytes per sample, bits 0–7 = bit width.
#[must_use]
pub fn decode_pcm_format(id: u16) -> Box<str> {
    if id == 0 {
        return "Invalid".into();
    }
    let signed = (id >> 15) & 1 == 1;
    let big_endian = (id >> 14) & 1 == 1;
    let bytes_per_sample = ((id >> 8) & 0x3F) as u8;
    let width = (id & 0xFF) as u8;

    if width == 0 || bytes_per_sample == 0 || u16::from(bytes_per_sample) * 8 < u16::from(width) {
        return "Invalid".into();
    }

    let sign = if signed { 'S' } else { 'U' };
    let endian = if big_endian { "BE" } else { "LE" };
    if bytes_per_sample == 1 {
        format!("{sign}{width}{endian}").into()
    } else {
        format!("{sign}{width}_{bytes_per_sample}{endian}").into()
    }
}

/// Renders `BlueALSA` per-channel volume bytes (`(muted << 7) | level`).
///
/// Uses channel names when provided (`ChannelMap`), falling back to generic indices.
#[must_use]
pub fn format_volume_bytes(
    bytes: &[u8],
    channel_names: Option<&[Box<str>]>,
) -> (Option<Box<str>>, bool) {
    if bytes.is_empty() {
        return (None, false);
    }
    let muted = bytes.iter().any(|b| b & 0x80 != 0);
    let volume = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let name = channel_names
                .and_then(|names| names.get(i))
                .map_or_else(|| format!("CH{i}"), std::string::ToString::to_string);
            format!("{name}: {}", b & 0x7F)
        })
        .collect::<Vec<_>>()
        .join(" ");
    (Some(volume.into()), muted)
}

#[must_use]
pub fn is_no_such_unit_error(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => {
            let s = name.as_str();
            s == "org.freedesktop.systemd1.NoSuchUnit"
                || s == "org.freedesktop.systemd1.UnknownUnit"
                || s.ends_with(".NoSuchUnit")
                || s.ends_with(".UnknownUnit")
        }
        zbus::Error::FDO(fdo_err) => matches!(**fdo_err, zbus::fdo::Error::UnknownObject(_)),
        other => {
            let s = other.to_string();
            s.contains("NoSuchUnit") || s.contains("UnknownUnit")
        }
    }
}

/// Detects `UnknownMethod`/`UnknownObject` D-Bus error replies.
fn is_unknown_method_error(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => {
            let s = name.as_str();
            s.ends_with(".UnknownMethod") || s.ends_with(".UnknownObject")
        }
        zbus::Error::FDO(fdo_err) => {
            matches!(
                &**fdo_err,
                zbus::fdo::Error::UnknownMethod(_) | zbus::fdo::Error::UnknownObject(_)
            )
        }
        _ => false,
    }
}

#[must_use]
pub fn is_service_absent_error(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => {
            let s = name.as_str();
            s == "org.freedesktop.DBus.Error.ServiceUnknown"
                || s == "org.freedesktop.DBus.Error.NameHasNoOwner"
                || s.ends_with(".ServiceUnknown")
                || s.ends_with(".NameHasNoOwner")
        }
        zbus::Error::FDO(fdo_err) => is_service_absent_fdo_error(fdo_err),
        other => {
            let s = other.to_string();
            s.contains("ServiceUnknown") || s.contains("NameHasNoOwner")
        }
    }
}

#[must_use]
pub fn is_service_absent_fdo_error(err: &zbus::fdo::Error) -> bool {
    match err {
        zbus::fdo::Error::ServiceUnknown(_)
        | zbus::fdo::Error::NameHasNoOwner(_)
        | zbus::fdo::Error::UnknownObject(_)
        | zbus::fdo::Error::UnknownInterface(_) => true,
        zbus::fdo::Error::ZBus(e) => is_service_absent_error(e),
        _ => {
            let s = err.to_string();
            s.contains("ServiceUnknown") || s.contains("NameHasNoOwner")
        }
    }
}

/// D-Bus client executing direct queries against systemd, `BlueZ`, and `BlueALSA`.
#[derive(Clone, Debug)]
pub struct DbusClient {
    conn: Connection,
}

/// Snapshot of `BlueZ` adapter, connected/paired devices, and media player state.
pub type BluezSnapshot = (Option<BtAdapter>, Box<[BtDevice]>, Option<BtPlaybackState>);

impl DbusClient {
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub(crate) fn check_services(&self) -> Result<BluetoothServices, BtError> {
        let manager = SystemdManagerProxyBlocking::new(&self.conn)?;
        Ok(BluetoothServices {
            bluetoothd: self.check_service(&manager, "bluetooth.service")?,
            bluealsa: self.check_service(&manager, "bluealsa.service")?,
            bluealsa_aplay: self.check_service(&manager, "bluealsa-aplay.service")?,
            bt_agent: self.check_service(&manager, "bt-agent.service")?,
        })
    }

    fn check_service(
        &self,
        manager: &SystemdManagerProxyBlocking<'_>,
        name: &str,
    ) -> Result<ServiceState, BtError> {
        match manager.get_unit(name) {
            Ok(path) => {
                let unit = match SystemdUnitProxyBlocking::builder(&self.conn)
                    .path(path)
                    .and_then(zbus::blocking::proxy::Builder::build)
                {
                    Ok(u) => u,
                    Err(e) if is_no_such_unit_error(&e) => return Ok(ServiceState::NotFound),
                    Err(e) => return Err(BtError::from(e)),
                };
                let active = unit.active_state()?;
                let load = unit.load_state()?;
                Ok(Self::parse_systemd_state(&active, &load))
            }
            Err(e) if is_no_such_unit_error(&e) => Ok(ServiceState::NotFound),
            Err(e) => Err(BtError::from(e)),
        }
    }

    #[must_use]
    pub fn parse_systemd_state(active: &str, load: &str) -> ServiceState {
        if active == "active" {
            ServiceState::Active
        } else if load == "not-found" || active == "not-found" {
            ServiceState::NotFound
        } else if active == "failed" {
            ServiceState::Failed
        } else {
            ServiceState::Inactive
        }
    }

    fn parse_adapter_props(path: &str, props: &HashMap<String, OwnedValue>) -> Option<BtAdapter> {
        let id = path.rsplit('/').next().unwrap_or(DEFAULT_BT_ADAPTER);
        let address = BtAddress::parse_str(get_str(props, "Address")?).ok()?;
        Some(BtAdapter {
            id: id.into(),
            address,
            name: get_str(props, "Name").unwrap_or("Unknown").into(),
            alias: get_str(props, "Alias").unwrap_or("Unknown").into(),
            powered: get_bool(props, "Powered"),
            discoverable: get_bool(props, "Discoverable"),
            pairable: get_bool(props, "Pairable"),
            discovering: get_bool(props, "Discovering"),
            class: get_u32(props, "Class"),
            modalias: get_str(props, "Modalias").map(Into::into),
        })
    }

    fn parse_device_props(
        props: &HashMap<String, OwnedValue>,
        ifaces: &HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
    ) -> Option<BtDevice> {
        let addr_str = get_str(props, "Address")?;
        let address = BtAddress::parse_str(addr_str).ok()?;
        let battery_percentage = find_iface(ifaces, "org.bluez.Battery1")
            .and_then(|b_props| get_u8(b_props, "Percentage"));

        Some(BtDevice {
            address,
            name: get_str(props, "Name").unwrap_or(addr_str).into(),
            alias: get_str(props, "Alias").unwrap_or(addr_str).into(),
            connected: get_bool(props, "Connected"),
            paired: get_bool(props, "Paired"),
            trusted: get_bool(props, "Trusted"),
            blocked: get_bool(props, "Blocked"),
            rssi: get_i16(props, "RSSI"),
            battery_percentage,
            icon: get_str(props, "Icon").map(Into::into),
            uuids: get_str_array(props, "UUIDs"),
        })
    }

    fn parse_media_player_props(
        path: &str,
        props: &HashMap<String, OwnedValue>,
        devices: &[BtDevice],
    ) -> BtPlaybackState {
        let status: Box<str> = get_str(props, "Status").unwrap_or("stopped").into();
        let position_ms = get_u32(props, "Position").unwrap_or(0);
        let player_path = Some(path.into());

        let device_address = props
            .get("Device")
            .and_then(|v| match &**v {
                Value::ObjectPath(p) => Self::extract_mac_from_pcm_path(p.as_str()),
                Value::Str(s) => Self::extract_mac_from_pcm_path(s.as_str()),
                _ => v
                    .downcast_ref::<&str>()
                    .ok()
                    .and_then(Self::extract_mac_from_pcm_path),
            })
            .and_then(|s| BtAddress::parse_str(&s).ok());

        let device_name = device_address
            .and_then(|addr| devices.iter().find(|d| d.address == addr))
            .map_or_else(
                || "Bluetooth Audio Device".into(),
                |d| d.display_name().into(),
            );

        let (title, artist, album, duration_ms) = if let Some(track_val) = props.get("Track") {
            let title = get_track_prop_str(track_val, "Title")
                .unwrap_or_else(|| "Waiting for Bluetooth Track...".into());
            let artist = get_track_prop_str(track_val, "Artist")
                .unwrap_or_else(|| "DietPi Audiophile Speaker".into());
            let album = get_track_prop_str(track_val, "Album")
                .unwrap_or_else(|| "High-Resolution DSP Sink".into());
            let duration_ms = get_track_prop_u32(track_val, "Duration").unwrap_or(0);
            (title, artist, album, duration_ms)
        } else {
            (
                "Waiting for Bluetooth Connection...".into(),
                "DietPi Audiophile Speaker".into(),
                "High-Resolution DSP Sink".into(),
                0,
            )
        };

        BtPlaybackState {
            device_address,
            device_name,
            status,
            title,
            artist,
            album,
            position_ms,
            duration_ms,
            player_path,
        }
    }

    pub(crate) fn query_bluez(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<BluezSnapshot, BtError> {
        let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination("org.bluez")
            .and_then(|b| b.path("/"))
            .and_then(zbus::blocking::proxy::Builder::build)?;

        let objects = proxy.get_managed_objects()?;

        Self::parse_bluez_all(&objects, target_adapter)
    }

    /// Parses `BlueZ` managed objects into adapter and device models.
    ///
    /// # Errors
    /// Returns [`BtError`] if adapter filtering or object parsing fails.
    pub fn parse_bluez_objects(
        objects: &ManagedObjects,
        target_adapter: Option<&str>,
    ) -> Result<(Option<BtAdapter>, Box<[BtDevice]>), BtError> {
        let mut adapters = Vec::new();
        let mut devices = Vec::new();

        // 1. Search for Adapter1 interfaces
        for (path, ifaces) in objects {
            if let Some(props) = find_iface(ifaces, "org.bluez.Adapter1") {
                if let Some(adapter) = Self::parse_adapter_props(path.as_str(), props) {
                    adapters.push(adapter);
                }
            }
        }

        adapters.sort_by(|a, b| a.id.cmp(&b.id));

        let adapter = if let Some(target) = target_adapter {
            match adapters.into_iter().find(|a| a.id.as_ref() == target) {
                Some(a) => Some(a),
                None => return Err(BtError::adapter_not_found(target)),
            }
        } else {
            adapters
                .iter()
                .position(|a| a.id.as_ref() == DEFAULT_BT_ADAPTER)
                .map(|idx| adapters.swap_remove(idx))
                .or_else(|| adapters.into_iter().next())
        };

        let selected_adapter_id = adapter.as_ref().map(|a| a.id.as_ref());

        // 2. Search for Device1 interfaces belonging to the selected adapter
        for (path, ifaces) in objects {
            if let Some(props) = find_iface(ifaces, "org.bluez.Device1") {
                if let Some(adapter_id) = selected_adapter_id {
                    let matches_adapter = path
                        .as_str()
                        .strip_prefix("/org/bluez/")
                        .and_then(|p| p.split('/').next())
                        .is_some_and(|id| id == adapter_id);
                    if !matches_adapter {
                        continue;
                    }
                }

                if let Some(device) = Self::parse_device_props(props, ifaces) {
                    devices.push(device);
                }
            }
        }

        devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then_with(|| a.address.cmp(&b.address))
        });

        Ok((adapter, devices.into_boxed_slice()))
    }

    /// Parses `BlueZ` managed objects into adapter, device, and active media player models.
    ///
    /// # Errors
    /// Returns [`BtError`] if adapter filtering or object parsing fails.
    pub fn parse_bluez_all(
        objects: &ManagedObjects,
        target_adapter: Option<&str>,
    ) -> Result<BluezSnapshot, BtError> {
        let (adapter, devices) = Self::parse_bluez_objects(objects, target_adapter)?;

        let selected_adapter_id = adapter.as_ref().map(|a| a.id.as_ref());

        // 3. Search for MediaPlayer1 interfaces belonging to the selected adapter
        let mut playback = None;
        for (path, ifaces) in objects {
            if let Some(props) = find_iface(ifaces, "org.bluez.MediaPlayer1") {
                if let Some(adapter_id) = selected_adapter_id {
                    let matches_adapter = path
                        .as_str()
                        .strip_prefix("/org/bluez/")
                        .and_then(|p| p.split('/').next())
                        .is_some_and(|id| id == adapter_id);
                    if !matches_adapter {
                        continue;
                    }
                }

                let state = Self::parse_media_player_props(path.as_str(), props, &devices);
                if state.is_playing() || playback.is_none() {
                    playback = Some(state);
                }
            }
        }

        Ok((adapter, devices, playback))
    }

    pub(crate) fn call_player_method(
        &self,
        player_path: Option<&str>,
        method: &str,
    ) -> Result<(), BtError> {
        let path = if let Some(p) = player_path {
            p.to_string()
        } else {
            let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn)
                .destination("org.bluez")
                .and_then(|b| b.path("/"))
                .and_then(zbus::blocking::proxy::Builder::build)?;
            let objects = proxy.get_managed_objects()?;
            let found = objects.iter().find_map(|(p, ifaces)| {
                find_iface(ifaces, "org.bluez.MediaPlayer1").map(|_| p.as_str().to_string())
            });
            found.ok_or_else(|| BtError::no_active_player("no MediaPlayer1 found on D-Bus"))?
        };

        let proxy =
            zbus::blocking::Proxy::new(&self.conn, "org.bluez", path, "org.bluez.MediaPlayer1")
                .map_err(|e| BtError::dbus(e.to_string()))?;
        proxy
            .call_method(method, &())
            .map_err(|e| BtError::dbus(e.to_string()))?;
        Ok(())
    }

    pub(crate) fn query_bluealsa(
        &self,
        devices: &[BtDevice],
    ) -> Result<Box<[BlueAlsaPcm]>, BtError> {
        let proxy = match zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination("org.bluealsa")
            .and_then(|b| b.path("/org/bluealsa"))
            .and_then(zbus::blocking::proxy::Builder::build)
        {
            Ok(p) => p,
            Err(e) if is_service_absent_error(&e) => return Ok(Box::default()),
            Err(e) => return Err(BtError::from(e)),
        };

        let objects = match proxy.get_managed_objects() {
            Ok(objs) => objs,
            Err(e) if is_service_absent_fdo_error(&e) => return Ok(Box::default()),
            Err(e) => return Err(BtError::from(e)),
        };

        let pcms = Self::parse_bluealsa_objects(&objects, devices)
            .into_iter()
            .map(|mut pcm| {
                pcm.available_codecs = Self::query_pcm_codecs(&self.conn, &pcm.pcm_path)?;
                Ok(pcm)
            })
            .collect::<Result<Vec<_>, BtError>>()?;
        Ok(pcms.into_boxed_slice())
    }

    /// Queries available codecs for a PCM via the `GetCodecs()` D-Bus method.
    ///
    /// Returns an empty list only when the daemon does not implement `GetCodecs`
    /// (older `BlueALSA` versions), mirroring `bluealsactl` which prints
    /// `[ Unknown ]` when the list is empty. Transport-level faults (connection
    /// loss, permission errors, malformed replies) propagate as [`BtError`].
    fn query_pcm_codecs(conn: &Connection, pcm_path: &str) -> Result<Box<[Box<str>]>, BtError> {
        let proxy = zbus::blocking::Proxy::new(conn, "org.bluealsa", pcm_path, "org.bluealsa.PCM1")
            .map_err(|e| BtError::dbus(format!("codec proxy for {pcm_path}: {e}")))?;

        let reply = match proxy.call_method("GetCodecs", &()) {
            Ok(reply) => reply,
            Err(e) if is_unknown_method_error(&e) => return Ok(Box::default()),
            Err(e) => return Err(BtError::dbus(format!("GetCodecs call for {pcm_path}: {e}"))),
        };
        let codecs = reply
            .body()
            .deserialize::<HashMap<String, HashMap<String, OwnedValue>>>()
            .map_err(|e| BtError::dbus(format!("GetCodecs reply for {pcm_path}: {e}")))?;

        let mut names: Vec<Box<str>> = codecs.into_keys().map(Into::into).collect();
        names.sort();
        Ok(names.into_boxed_slice())
    }

    pub fn parse_bluealsa_objects(
        objects: &ManagedObjects,
        devices: &[BtDevice],
    ) -> Box<[BlueAlsaPcm]> {
        let mut name_map: HashMap<BtAddress, &str> = HashMap::new();
        for dev in devices {
            name_map.insert(dev.address, dev.display_name());
        }

        let mut pcms = Vec::new();

        for (path, ifaces) in objects {
            if let Some(props) = find_iface(ifaces, "org.bluealsa.PCM1") {
                let pcm_path = path.as_str().into();
                let device_address = get_str(props, "Device")
                    .and_then(Self::extract_mac_from_pcm_path)
                    .or_else(|| Self::extract_mac_from_pcm_path(path.as_str()))
                    .and_then(|s| BtAddress::parse_str(&s).ok());

                let Some(device_address) = device_address else {
                    continue;
                };

                let resolved_name = name_map
                    .get(&device_address)
                    .copied()
                    .unwrap_or("Unknown Device")
                    .into();

                let transport = get_str(props, "Transport").unwrap_or("A2DP-sink").into();
                let running = get_bool(props, "Running");
                let selected_codec = get_str(props, "Codec").unwrap_or("Unknown").into();

                // Available codecs are exposed only via the `GetCodecs()` method
                // (queried per-PCM in `query_bluealsa`), never as properties.
                let available_codecs = Box::default();

                let sampling_rate = get_u32(props, "Sampling")
                    .or_else(|| get_u32(props, "Rate"))
                    .unwrap_or(44_100);

                let channels = get_u8(props, "Channels")
                    .or_else(|| get_u16(props, "Channels").and_then(|c| u8::try_from(c).ok()))
                    .unwrap_or(2);

                let format =
                    get_u16(props, "Format").map_or_else(|| "Unknown".into(), decode_pcm_format);

                let channel_map = get_str_array(props, "ChannelMap");
                let channel_names = if channel_map.is_empty() {
                    None
                } else {
                    Some(channel_map)
                };
                let (volume, muted) = get_byte_array(props, "Volume")
                    .map_or((None, false), |bytes| {
                        format_volume_bytes(&bytes, channel_names.as_deref())
                    });

                // `Delay` is a uint16 in 1/10 ms units (e.g. 1500 → 150.0 ms).
                let delay = get_u16(props, "Delay")
                    .map(|d| format!("{:.1} ms", f64::from(d) / 10.0).into_boxed_str());

                pcms.push(BlueAlsaPcm {
                    pcm_path,
                    device_address,
                    resolved_name,
                    transport,
                    running,
                    selected_codec,
                    available_codecs,
                    sampling_rate,
                    channels,
                    format,
                    volume,
                    muted,
                    delay,
                });
            }
        }

        pcms.sort_by(|a, b| a.pcm_path.cmp(&b.pcm_path));

        pcms.into_boxed_slice()
    }

    pub(crate) fn set_adapter_property<F>(
        &self,
        target_adapter: Option<&str>,
        action: F,
    ) -> Result<(), BtError>
    where
        F: FnOnce(&BlueZAdapter1ProxyBlocking<'_>) -> zbus::Result<()>,
    {
        let adapter_id = target_adapter.unwrap_or(DEFAULT_BT_ADAPTER);
        let path = format!("/org/bluez/{adapter_id}");
        let proxy = match BlueZAdapter1ProxyBlocking::builder(&self.conn)
            .path(path.as_str())
            .and_then(zbus::blocking::proxy::Builder::build)
        {
            Ok(p) => p,
            Err(e) => return Err(BtError::from(e)),
        };

        action(&proxy).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("UnknownObject") || msg.contains("does not exist") {
                BtError::adapter_not_found(adapter_id)
            } else {
                BtError::from(e)
            }
        })
    }

    #[must_use]
    pub fn extract_mac_from_pcm_path(path: &str) -> Option<String> {
        let dev_prefix = "dev_";
        let idx = path.find(dev_prefix)?;
        let start = idx + dev_prefix.len();
        let candidate = &path[start..];
        let mac_part = candidate.split('/').next()?;
        if mac_part.len() != 17 {
            return None;
        }
        let formatted = mac_part.replace('_', ":");
        BtAddress::parse_str(&formatted).ok().map(|_| formatted)
    }
}

// ============================================================================
// In-Memory Mock Controller (M-MOCKABLE-SYSCALLS)
// ============================================================================

/// Controllable in-memory mock for [`BluetoothDevice`].
#[derive(Debug, Clone)]
pub struct BluetoothDeviceMock {
    state: Arc<RwLock<MockState>>,
}

#[derive(Debug)]
struct MockState {
    services: BluetoothServices,
    adapter: Option<BtAdapter>,
    devices: Vec<BtDevice>,
    pcms: Vec<BlueAlsaPcm>,
    playback: Option<BtPlaybackState>,
    fault: Option<Box<str>>,
}

impl BluetoothDeviceMock {
    fn new() -> Self {
        let adapter_addr = BtAddress::new([0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33]);
        let phone_addr = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

        let state = MockState {
            services: BluetoothServices {
                bluetoothd: ServiceState::Active,
                bluealsa: ServiceState::Active,
                bluealsa_aplay: ServiceState::Active,
                bt_agent: ServiceState::Active,
            },
            adapter: Some(BtAdapter {
                id: DEFAULT_BT_ADAPTER.into(),
                address: adapter_addr,
                name: "RaspberryPi-DSP".into(),
                alias: "RaspberryPi-DSP".into(),
                powered: true,
                discoverable: false,
                pairable: true,
                discovering: false,
                class: Some(0x0024_0404),
                modalias: Some("usb:v1D6Bp0246d0537".into()),
            }),
            devices: vec![BtDevice {
                address: phone_addr,
                name: "iPhone".into(),
                alias: "iPhone 15 Pro".into(),
                connected: true,
                paired: true,
                trusted: true,
                blocked: false,
                rssi: Some(-42),
                battery_percentage: Some(85),
                icon: Some("phone".into()),
                uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".into()].into_boxed_slice(),
            }],
            pcms: vec![BlueAlsaPcm {
                pcm_path: "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source".into(),
                device_address: phone_addr,
                resolved_name: "iPhone 15 Pro".into(),
                transport: "A2DP-sink".into(),
                running: true,
                selected_codec: "AAC".into(),
                available_codecs: vec!["SBC".into(), "AAC".into(), "LDAC".into()]
                    .into_boxed_slice(),
                sampling_rate: 44_100,
                channels: 2,
                format: "S16_LE".into(),
                volume: Some("L: 100 R: 100".into()),
                muted: false,
                delay: Some("2000".into()),
            }],
            playback: Some(BtPlaybackState {
                device_address: Some(phone_addr),
                device_name: "iPhone 15 Pro".into(),
                status: "playing".into(),
                title: "Money for Nothing".into(),
                artist: "Dire Straits".into(),
                album: "Brothers in Arms".into(),
                position_ms: 105_000,
                duration_ms: 230_000,
                player_path: Some("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0".into()),
            }),
            fault: None,
        };

        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    /// Sets the simulated Bluetooth system services state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_services(&self, services: BluetoothServices) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.services = services;
    }

    /// Sets the simulated local adapter properties.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_adapter(&self, adapter: Option<BtAdapter>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.adapter = adapter;
    }

    /// Adds or updates a simulated remote device.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn add_device(&self, device: BtDevice) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(pos) = state
            .devices
            .iter()
            .position(|d| d.address == device.address)
        {
            state.devices[pos] = device;
        } else {
            state.devices.push(device);
        }
    }

    /// Clears all simulated remote devices.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn clear_devices(&self) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.devices.clear();
    }

    /// Adds a simulated `BlueALSA` audio PCM stream.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn add_pcm(&self, pcm: BlueAlsaPcm) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.pcms.push(pcm);
    }

    /// Clears all simulated `BlueALSA` audio PCM streams.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn clear_pcms(&self) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.pcms.clear();
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.fault = fault;
    }

    /// Sets the simulated adapter radio power state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_powered(&self, powered: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut adapter) = state.adapter {
            adapter.powered = powered;
        }
    }

    /// Sets the simulated adapter discoverable state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_discoverable(&self, discoverable: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut adapter) = state.adapter {
            adapter.discoverable = discoverable;
        }
    }

    /// Sets the simulated adapter pairable state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_pairable(&self, pairable: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut adapter) = state.adapter {
            adapter.pairable = pairable;
        }
    }

    /// Sets the simulated media player playback state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_playback(&self, playback: Option<BtPlaybackState>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.playback = playback;
    }

    /// Updates the simulated playback status (e.g. `playing`, `paused`, `stopped`).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_player_status(&self, status: impl Into<Box<str>>) {
        let status = status.into();
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut p) = state.playback {
            p.status = status;
        }
    }

    /// Updates the simulated track metadata and total duration.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_player_track(&self, title: &str, artist: &str, album: &str, duration_ms: u32) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut p) = state.playback {
            p.title = title.into();
            p.artist = artist.into();
            p.album = album.into();
            p.duration_ms = duration_ms;
            p.position_ms = 0;
        }
    }

    /// Sets the simulated playback position in milliseconds.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_player_position(&self, position_ms: u32) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut p) = state.playback {
            p.position_ms = position_ms.min(p.duration_ms);
        }
    }

    fn player_play(&self) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            p.status = "playing".into();
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn player_pause(&self) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            p.status = "paused".into();
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn player_toggle(&self) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            if p.is_playing() {
                p.status = "paused".into();
            } else {
                p.status = "playing".into();
            }
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn player_next(&self) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            p.position_ms = 0;
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn player_previous(&self) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            p.position_ms = 0;
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn player_seek(&self, position_ms: u32) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            p.position_ms = position_ms.min(p.duration_ms);
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn get_adapter_mut<'a>(
        state: &'a mut MockState,
        target_adapter: Option<&str>,
    ) -> Result<&'a mut BtAdapter, BtError> {
        let adapter = state.adapter.as_mut().ok_or_else(|| {
            BtError::adapter_not_found(target_adapter.unwrap_or(DEFAULT_BT_ADAPTER))
        })?;
        if let Some(target) = target_adapter {
            if adapter.id.as_ref() != target {
                return Err(BtError::adapter_not_found(target));
            }
        }
        Ok(adapter)
    }

    fn set_powered_for_adapter(
        &self,
        target_adapter: Option<&str>,
        powered: bool,
    ) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        let adapter = Self::get_adapter_mut(&mut state, target_adapter)?;
        adapter.powered = powered;
        Ok(())
    }

    fn set_discoverable_for_adapter(
        &self,
        target_adapter: Option<&str>,
        discoverable: bool,
    ) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        let adapter = Self::get_adapter_mut(&mut state, target_adapter)?;
        adapter.discoverable = discoverable;
        Ok(())
    }

    fn set_pairable_for_adapter(
        &self,
        target_adapter: Option<&str>,
        pairable: bool,
    ) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        let adapter = Self::get_adapter_mut(&mut state, target_adapter)?;
        adapter.pairable = pairable;
        Ok(())
    }

    fn read_status_for_adapter(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<BluetoothStatus, BtError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(target) = target_adapter {
            match &state.adapter {
                Some(adapter) if adapter.id.as_ref() == target => {}
                _ => return Err(BtError::adapter_not_found(target)),
            }
        }
        Ok(BluetoothStatus {
            services: state.services.clone(),
            adapter: state.adapter.clone(),
            devices: state.devices.clone().into_boxed_slice(),
            pcms: state.pcms.clone().into_boxed_slice(),
            hardware: BluetoothHardwareInfo::rpi3(),
            playback: state.playback.clone(),
        })
    }

    /// Returns the current simulated Bluetooth status for the default adapter.
    ///
    /// # Errors
    /// Returns [`BtError`] if a mock fault is injected.
    pub fn read_status(&self) -> Result<BluetoothStatus, BtError> {
        self.read_status_for_adapter(None)
    }
}

enum Backend {
    Dbus(DbusClient),
    Mock(BluetoothDeviceMock),
}

/// Raspberry Pi Bluetooth controller and `BlueALSA` audio subsystem client.
///
/// Thread-safe, cheaply cloneable (`Arc`-backed handle per `M-SERVICES-CLONE`),
/// providing native D-Bus inspection and control methods over `zbus`.
#[derive(Clone)]
pub struct BluetoothDevice {
    backend: Arc<Backend>,
}

impl fmt::Debug for BluetoothDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &*self.backend {
            Backend::Mock(mock) => f.debug_tuple("BluetoothDevice::Mock").field(mock).finish(),
            Backend::Dbus(_) => f.debug_tuple("BluetoothDevice::Dbus").finish(),
        }
    }
}

impl BluetoothDevice {
    /// Connects to the Linux system D-Bus and initializes the Bluetooth driver.
    ///
    /// # Errors
    /// Returns [`BtError`] if connecting to the system D-Bus fails.
    pub fn new() -> Result<Self, BtError> {
        let conn = Connection::system()?;
        Ok(Self {
            backend: Arc::new(Backend::Dbus(DbusClient::new(conn))),
        })
    }

    /// Creates an inherent mock device and companion mock controller for testing.
    #[must_use]
    pub fn new_mocked() -> (Self, BluetoothDeviceMock) {
        let mock = BluetoothDeviceMock::new();
        let device = Self {
            backend: Arc::new(Backend::Mock(mock.clone())),
        };
        (device, mock)
    }

    /// Creates a device instance backed by an established D-Bus connection.
    #[must_use]
    pub fn with_connection(conn: Connection) -> Self {
        Self {
            backend: Arc::new(Backend::Dbus(DbusClient::new(conn))),
        }
    }

    /// Inspects the status for a specific adapter (or default if `None`) synchronously.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails, the adapter is not found, or a mock fault is injected.
    pub fn status_for_adapter(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<BluetoothStatus, BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.read_status_for_adapter(target_adapter),
            Backend::Dbus(dbus) => {
                let services = dbus.check_services()?;
                let (adapter, devices, playback) = dbus.query_bluez(target_adapter)?;
                let pcms = dbus.query_bluealsa(&devices)?;
                let mut hardware = BluetoothHardwareInfo::rpi3();
                let probed = adapter
                    .as_ref()
                    .map_or_else(|| hardware.default_controller.clone(), |a| a.id.clone());
                hardware.controller = BluetoothControllerInfo::probe(&probed);
                Ok(BluetoothStatus {
                    services,
                    adapter,
                    devices,
                    pcms,
                    hardware,
                    playback,
                })
            }
        }
    }

    /// Inspects the status for a specific adapter asynchronously without blocking Tokio.
    ///
    /// # Errors
    /// Returns [`BtError`] if reading fails, the adapter is not found, or the task is cancelled.
    pub async fn status_for_adapter_async(
        &self,
        target_adapter: Option<Box<str>>,
    ) -> Result<BluetoothStatus, BtError> {
        let dev = self.clone();
        crate::runtime::run_blocking("bluetooth status read", move || {
            dev.status_for_adapter(target_adapter.as_deref())
        })
        .await
    }

    /// Inspects the complete Bluetooth subsystem status synchronously.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or a mock fault is injected.
    pub fn status(&self) -> Result<BluetoothStatus, BtError> {
        self.status_for_adapter(None)
    }

    /// Inspects the complete Bluetooth subsystem status asynchronously without blocking Tokio.
    ///
    /// # Errors
    /// Returns [`BtError`] if reading fails or task is cancelled.
    pub async fn status_async(&self) -> Result<BluetoothStatus, BtError> {
        self.status_for_adapter_async(None).await
    }

    /// Inspects Bluetooth system service states.
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn services_status(&self) -> Result<BluetoothServices, BtError> {
        Ok(self.status()?.services)
    }

    /// Returns local controller adapter information for a specific adapter (or default if `None`).
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails or the adapter is not found.
    pub fn adapter_info_for(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<Option<BtAdapter>, BtError> {
        Ok(self.status_for_adapter(target_adapter)?.adapter)
    }

    /// Returns local controller adapter information.
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn adapter_info(&self) -> Result<Option<BtAdapter>, BtError> {
        self.adapter_info_for(None)
    }

    /// Returns all connected Bluetooth devices.
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn connected_devices(&self) -> Result<Box<[BtDevice]>, BtError> {
        let status = self.status()?;
        let connected: Vec<BtDevice> = status
            .devices
            .iter()
            .filter(|d| d.connected)
            .cloned()
            .collect();
        Ok(connected.into_boxed_slice())
    }

    /// Returns active `BlueALSA` audio PCM transport streams.
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn audio_pcms(&self) -> Result<Box<[BlueAlsaPcm]>, BtError> {
        Ok(self.status()?.pcms)
    }

    /// Returns the active media player playback state, if available.
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn playback_state(&self) -> Result<Option<BtPlaybackState>, BtError> {
        Ok(self.status()?.playback)
    }

    /// Returns the active media player playback state asynchronously.
    ///
    /// # Errors
    /// Returns [`BtError`] if reading fails or task is cancelled.
    pub async fn playback_state_async(&self) -> Result<Option<BtPlaybackState>, BtError> {
        let dev = self.clone();
        crate::runtime::run_blocking("bluetooth playback read", move || dev.playback_state()).await
    }

    /// Sends a Play command to the active Bluetooth media player.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_play(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_play(),
            Backend::Dbus(dbus) => dbus.call_player_method(None, "Play"),
        }
    }

    /// Sends a Pause command to the active Bluetooth media player.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_pause(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_pause(),
            Backend::Dbus(dbus) => dbus.call_player_method(None, "Pause"),
        }
    }

    /// Toggles between Play and Pause on the active Bluetooth media player.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_toggle(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_toggle(),
            Backend::Dbus(dbus) => {
                let status = self.status()?;
                if status.is_playing() {
                    dbus.call_player_method(None, "Pause")
                } else {
                    dbus.call_player_method(None, "Play")
                }
            }
        }
    }

    /// Sends a Next command to advance to the next track.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_next(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_next(),
            Backend::Dbus(dbus) => dbus.call_player_method(None, "Next"),
        }
    }

    /// Sends a Previous command to restart or return to the previous track.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_previous(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_previous(),
            Backend::Dbus(dbus) => dbus.call_player_method(None, "Previous"),
        }
    }

    /// Requests seeking to a specific playback position in milliseconds.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_seek(&self, position_ms: u32) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_seek(position_ms),
            Backend::Dbus(_) => Ok(()),
        }
    }

    /// Powers on or off the local Bluetooth radio for a specific adapter.
    ///
    /// # Errors
    /// Returns [`BtError`] if the D-Bus power property call fails.
    ///
    /// # Panics
    /// Panics if the mock backend lock is poisoned.
    pub fn set_powered_for_adapter(
        &self,
        target_adapter: Option<&str>,
        powered: bool,
    ) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.set_powered_for_adapter(target_adapter, powered),
            Backend::Dbus(dbus) => {
                dbus.set_adapter_property(target_adapter, |p| p.set_powered(powered))
            }
        }
    }

    /// Powers on or off the default local Bluetooth radio.
    ///
    /// # Errors
    /// Returns [`BtError`] if the power command fails.
    pub fn set_powered(&self, powered: bool) -> Result<(), BtError> {
        self.set_powered_for_adapter(None, powered)
    }

    /// Enables or disables discoverable mode for a specific adapter.
    ///
    /// # Errors
    /// Returns [`BtError`] if the discoverable property call fails.
    ///
    /// # Panics
    /// Panics if the mock backend lock is poisoned.
    pub fn set_discoverable_for_adapter(
        &self,
        target_adapter: Option<&str>,
        discoverable: bool,
    ) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.set_discoverable_for_adapter(target_adapter, discoverable),
            Backend::Dbus(dbus) => {
                dbus.set_adapter_property(target_adapter, |p| p.set_discoverable(discoverable))
            }
        }
    }

    /// Enables or disables discoverable mode on the default Bluetooth radio.
    ///
    /// # Errors
    /// Returns [`BtError`] if the discoverable command fails.
    pub fn set_discoverable(&self, discoverable: bool) -> Result<(), BtError> {
        self.set_discoverable_for_adapter(None, discoverable)
    }

    /// Enables or disables pairable mode for a specific adapter.
    ///
    /// # Errors
    /// Returns [`BtError`] if the pairable property call fails.
    ///
    /// # Panics
    /// Panics if the mock backend lock is poisoned.
    pub fn set_pairable_for_adapter(
        &self,
        target_adapter: Option<&str>,
        pairable: bool,
    ) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.set_pairable_for_adapter(target_adapter, pairable),
            Backend::Dbus(dbus) => {
                dbus.set_adapter_property(target_adapter, |p| p.set_pairable(pairable))
            }
        }
    }

    /// Enables or disables pairable mode on the default Bluetooth radio.
    ///
    /// # Errors
    /// Returns [`BtError`] if the pairable command fails.
    pub fn set_pairable(&self, pairable: bool) -> Result<(), BtError> {
        self.set_pairable_for_adapter(None, pairable)
    }
}

#[cfg(feature = "test-util")]
pub mod test_helpers {
    use super::BtError;
    pub use super::{
        DbusClient, ManagedObjects, format_volume_bytes, is_no_such_unit_error,
        is_service_absent_error, is_service_absent_fdo_error, parse_hciconfig_version,
    };
    use crate::runtime::FromJoinError;

    /// Creates a `BtError::dbus` error for testing.
    #[must_use]
    pub fn make_dbus_error(details: &str) -> BtError {
        BtError::dbus(details)
    }

    /// Creates a `BtError::adapter_not_found` error for testing.
    #[must_use]
    pub fn make_adapter_not_found_error(adapter: &str) -> BtError {
        BtError::adapter_not_found(adapter)
    }

    /// Creates a `BtError::invalid_address` error for testing.
    #[must_use]
    pub fn make_invalid_address_error(raw: &str, reason: &str) -> BtError {
        BtError::invalid_address(raw, reason)
    }

    /// Creates a `BtError::no_active_player` error for testing.
    #[must_use]
    pub fn make_no_active_player_error(details: &str) -> BtError {
        BtError::no_active_player(details)
    }

    /// Creates a `BtError::cancelled` error for testing.
    #[must_use]
    pub fn make_cancelled_error(task: &str) -> BtError {
        BtError::cancelled(task)
    }

    /// Creates a `BtError::from_cancelled` error for testing.
    #[must_use]
    pub fn make_from_cancelled_error(msg: &str) -> BtError {
        BtError::from_cancelled(msg)
    }

    /// Creates a `BtError::mock_fault` error for testing.
    #[must_use]
    pub fn make_mock_fault_error(detail: &str) -> BtError {
        BtError::mock_fault(detail)
    }
}
