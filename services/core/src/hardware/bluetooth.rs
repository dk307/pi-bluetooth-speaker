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
        self.hci_version.is_some()
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
fn parse_hciconfig_version(output: &str) -> Option<BluetoothControllerInfo> {
    let mut info = BluetoothControllerInfo::unknown();
    for line in output.lines() {
        let line = line.trim();
        if let Some((_, rest)) = line.split_once("Bus:") {
            info.bus_type = Some(rest.trim().to_string().into_boxed_str());
        }
        if let Some((_, rest)) = line.split_once("BD Address:") {
            let rest = rest.trim();
            let addr = rest.split_whitespace().next().unwrap_or(rest);
            info.bd_address = Some(addr.to_string().into_boxed_str());
        }
        if let Some((_, rest)) = line.split_once("HCI Version:") {
            let rest = rest.trim();
            let version = rest.split_whitespace().next()?;
            info.hci_version = Some(version.to_string().into_boxed_str());
        }
        if let Some((_, rest)) = line.split_once("Revision:") {
            info.hci_revision = rest
                .split_whitespace()
                .next()
                .map(String::from)
                .map(Into::into);
        }
        if let Some((_, rest)) = line.split_once("LMP Version:") {
            let version = rest.split_whitespace().next()?;
            info.lmp_version = Some(version.to_string().into_boxed_str());
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
        (Some(rev), Some(sub)) => {
            let rev = u32::from_str_radix(rev.trim_start_matches("0x"), 16).ok()?;
            let sub = u32::from_str_radix(sub.trim_start_matches("0x"), 16).ok()?;
            Some(format!("{:#x}", (rev << 16) | sub).into_boxed_str())
        }
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
}

#[derive(Debug)]
pub(crate) enum BtErrorKind {
    Dbus { details: Box<str> },
    AdapterNotFound { adapter: Box<str> },
    InvalidAddress { raw: Box<str>, reason: Box<str> },
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
fn format_volume_bytes(
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

pub(crate) fn is_no_such_unit_error(err: &zbus::Error) -> bool {
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

pub(crate) fn is_service_absent_error(err: &zbus::Error) -> bool {
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

pub(crate) fn is_service_absent_fdo_error(err: &zbus::fdo::Error) -> bool {
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
#[derive(Clone)]
pub(crate) struct DbusClient {
    conn: Connection,
}

impl DbusClient {
    pub(crate) fn new(conn: Connection) -> Self {
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

    pub(crate) fn parse_systemd_state(active: &str, load: &str) -> ServiceState {
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

    pub(crate) fn query_bluez(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<(Option<BtAdapter>, Box<[BtDevice]>), BtError> {
        let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination("org.bluez")
            .and_then(|b| b.path("/"))
            .and_then(zbus::blocking::proxy::Builder::build)?;

        let objects = proxy.get_managed_objects()?;

        Self::parse_bluez_objects(&objects, target_adapter)
    }

    pub(crate) fn parse_bluez_objects(
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

        Ok(Self::parse_bluealsa_objects(&objects, devices)
            .into_iter()
            .map(|mut pcm| {
                pcm.available_codecs = Self::query_pcm_codecs(&self.conn, &pcm.pcm_path);
                pcm
            })
            .collect::<Vec<_>>()
            .into_boxed_slice())
    }

    /// Queries available codecs for a PCM via the `GetCodecs()` D-Bus method.
    ///
    /// Returns an empty list on any error (e.g. unsupported method on older daemons),
    /// mirroring `bluealsactl` which prints `[ Unknown ]` when the list is empty.
    fn query_pcm_codecs(conn: &Connection, pcm_path: &str) -> Box<[Box<str>]> {
        let Ok(proxy) =
            zbus::blocking::Proxy::new(conn, "org.bluealsa", pcm_path, "org.bluealsa.PCM1")
        else {
            return Box::default();
        };

        let Ok(reply) = proxy.call_method("GetCodecs", &()) else {
            return Box::default();
        };

        let Ok(codecs) = reply
            .body()
            .deserialize::<HashMap<String, HashMap<String, OwnedValue>>>()
        else {
            return Box::default();
        };

        let mut names: Vec<Box<str>> = codecs.into_keys().map(Into::into).collect();
        names.sort();
        names.into_boxed_slice()
    }

    pub(crate) fn parse_bluealsa_objects(
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

    pub(crate) fn extract_mac_from_pcm_path(path: &str) -> Option<String> {
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
                let (adapter, devices) = dbus.query_bluez(target_adapter)?;
                let pcms = dbus.query_bluealsa(&devices)?;
                let mut hardware = BluetoothHardwareInfo::rpi3();
                hardware.controller = BluetoothControllerInfo::probe(&hardware.default_controller);
                Ok(BluetoothStatus {
                    services,
                    adapter,
                    devices,
                    pcms,
                    hardware,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_validation_and_parsing() {
        let valid = "AA:BB:CC:DD:EE:FF";
        let parsed = BtAddress::parse(valid).unwrap();
        assert_eq!(parsed.to_string(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(parsed.as_bytes(), &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

        let hyphens = "aa-bb-cc-dd-ee-ff";
        let parsed_h = BtAddress::parse_str(hyphens).unwrap();
        assert_eq!(parsed_h.to_string(), "AA:BB:CC:DD:EE:FF");

        BtAddress::parse_str("+AA:BB:CC:DD:EE:FF").unwrap_err();
        BtAddress::parse_str("AA:+BB:CC:DD:EE:FF").unwrap_err();
        BtAddress::parse_str("-AA:BB:CC:DD:EE:FF").unwrap_err();
        BtAddress::parse_str("AA:BB:CC:DD:EE").unwrap_err();
        BtAddress::parse_str("AA:BB:CC:DD:EE:FF:00").unwrap_err();
        BtAddress::parse_str("AA:GG:CC:DD:EE:FF").unwrap_err();
        BtAddress::parse_str("no delimiters").unwrap_err();

        BtAddress::try_from(valid).unwrap();
        valid.parse::<BtAddress>().unwrap();
    }

    #[test]
    fn service_state_predicates_and_display() {
        assert!(ServiceState::Active.is_active());
        assert!(!ServiceState::Inactive.is_active());
        assert!(!ServiceState::Failed.is_active());
        assert!(!ServiceState::NotFound.is_active());

        assert_eq!(ServiceState::Active.description(), "Running");
        assert_eq!(ServiceState::Inactive.to_string(), "INACTIVE / STOPPED");
        assert_eq!(ServiceState::Failed.to_string(), "FAILED");
        assert_eq!(
            ServiceState::NotFound.to_string(),
            "NOT INSTALLED / NOT FOUND"
        );
    }

    #[test]
    fn bluetooth_services_predicates() {
        let all_ok = BluetoothServices {
            bluetoothd: ServiceState::Active,
            bluealsa: ServiceState::Active,
            bluealsa_aplay: ServiceState::Active,
            bt_agent: ServiceState::Active,
        };
        assert!(all_ok.is_operational());
        assert!(all_ok.all_active());

        let aplay_only = BluetoothServices {
            bluetoothd: ServiceState::Active,
            bluealsa: ServiceState::Inactive,
            bluealsa_aplay: ServiceState::Active,
            bt_agent: ServiceState::Active,
        };
        assert!(aplay_only.is_operational());
        assert!(!aplay_only.all_active());

        let bt_down = BluetoothServices {
            bluetoothd: ServiceState::Inactive,
            bluealsa: ServiceState::Active,
            bluealsa_aplay: ServiceState::Active,
            bt_agent: ServiceState::Active,
        };
        assert!(!bt_down.is_operational());
    }

    #[test]
    fn bt_device_helpers() {
        let addr = BtAddress::new([1, 2, 3, 4, 5, 6]);
        let dev1 = BtDevice {
            address: addr,
            name: "Original".into(),
            alias: "Custom Alias".into(),
            connected: true,
            paired: true,
            trusted: true,
            blocked: false,
            rssi: Some(-50),
            battery_percentage: Some(90),
            icon: Some("phone".into()),
            uuids: Box::default(),
        };
        assert_eq!(dev1.display_name(), "Custom Alias");

        let dev2 = BtDevice {
            address: addr,
            name: "Fallback Name".into(),
            alias: "Unknown".into(),
            connected: false,
            paired: false,
            trusted: false,
            blocked: false,
            rssi: None,
            battery_percentage: None,
            icon: None,
            uuids: Box::default(),
        };
        assert_eq!(dev2.display_name(), "Fallback Name");

        let dev3 = BtDevice {
            address: addr,
            name: "".into(),
            alias: "".into(),
            connected: false,
            paired: false,
            trusted: false,
            blocked: false,
            rssi: None,
            battery_percentage: None,
            icon: None,
            uuids: Box::default(),
        };
        assert_eq!(dev3.display_name(), "Unknown Device");
    }

    #[test]
    fn bluealsa_pcm_stream_state_description() {
        let pcm = BlueAlsaPcm {
            pcm_path: "/org/bluealsa/hci0/dev_00_11_22_33_44_55/a2dpsnk/source".into(),
            device_address: BtAddress::new([0, 0x11, 0x22, 0x33, 0x44, 0x55]),
            resolved_name: "Pixel 8".into(),
            transport: "A2DP-sink".into(),
            running: true,
            selected_codec: "AAC".into(),
            available_codecs: vec!["SBC".into(), "AAC".into()].into_boxed_slice(),
            sampling_rate: 44_100,
            channels: 2,
            format: "S16_LE".into(),
            volume: Some("100%".into()),
            muted: false,
            delay: None,
        };
        assert_eq!(pcm.stream_state_description(), "STREAMING (Active)");

        let mut paused = pcm.clone();
        paused.running = false;
        assert_eq!(
            paused.stream_state_description(),
            "STANDBY / PAUSED (Connected)"
        );
    }

    #[test]
    fn hardware_info_rpi3_constants() {
        let hw = BluetoothHardwareInfo::rpi3();
        assert_eq!(&*hw.chip, "Broadcom BCM43438 (Bluetooth 4.1 + BLE)");
        assert_eq!(&*hw.bus, "UART (PL011 / /dev/ttyAMA0)");
        assert_eq!(hw.baud_rate, 921_600);
        assert_eq!(&*hw.firmware, "/lib/firmware/brcm/BCM43430A1.hcd");
        assert_eq!(&*hw.driver, "hci_uart (bcm)");
        assert_eq!(&*hw.default_controller, "hci0");
        assert!(!hw.controller.is_known());
        assert_eq!(hw.controller, BluetoothControllerInfo::unknown());
    }

    /// Real `hciconfig hci0 version` output from the Pi (TP-Link UB500, RTL8761B).
    const HCICONFIG_RTL8761B_OUTPUT: &str = "\
hci0:	Type: Primary  Bus: USB
	BD Address: 20:E1:5D:B2:16:BF  ACL MTU: 1021:6  SCO MTU: 255:12
	HCI Version: 5.4 (0xd)  Revision: 0xddd5
	LMP Version: 5.4 (0xd)  Subversion: 0xc761
	Manufacturer: Realtek Semiconductor Corporation (93)
";

    #[test]
    fn parse_hciconfig_version_rtl8761b() {
        let info = BluetoothControllerInfo::from_hciconfig_output(HCICONFIG_RTL8761B_OUTPUT)
            .expect("real hciconfig output must parse");
        assert_eq!(info.bus_type.as_deref(), Some("USB"));
        assert_eq!(info.bd_address.as_deref(), Some("20:E1:5D:B2:16:BF"));
        assert_eq!(info.hci_version.as_deref(), Some("5.4"));
        assert_eq!(info.hci_revision.as_deref(), Some("0xddd5"));
        assert_eq!(info.lmp_version.as_deref(), Some("5.4"));
        assert_eq!(info.lmp_subversion.as_deref(), Some("0xc761"));
        assert_eq!(
            info.manufacturer.as_deref(),
            Some("Realtek Semiconductor Corporation")
        );
        assert_eq!(info.firmware_version.as_deref(), Some("0xddd5c761"));
        assert!(info.is_known());
    }

    #[test]
    fn parse_hciconfig_version_bcm_uart() {
        let output = "\
hci0:	Type: Primary  Bus: UART
	BD Address: B8:27:EB:12:34:56  ACL MTU: 1021:8  SCO MTU: 255:12
	HCI Version: 4.1 (0x7)  Revision: 0x1000
	LMP Version: 4.1 (0x7)  Subversion: 0x2207
	Manufacturer: Broadcom Corporation (15)
";
        let info =
            BluetoothControllerInfo::from_hciconfig_output(output).expect("BCM output must parse");
        assert_eq!(info.bus_type.as_deref(), Some("UART"));
        assert_eq!(info.bd_address.as_deref(), Some("B8:27:EB:12:34:56"));
        assert_eq!(info.hci_version.as_deref(), Some("4.1"));
        assert_eq!(info.hci_revision.as_deref(), Some("0x1000"));
        assert_eq!(info.lmp_version.as_deref(), Some("4.1"));
        assert_eq!(info.lmp_subversion.as_deref(), Some("0x2207"));
        assert_eq!(info.manufacturer.as_deref(), Some("Broadcom Corporation"));
        assert_eq!(info.firmware_version.as_deref(), Some("0x10002207"));
    }

    #[test]
    fn parse_hciconfig_version_tolerates_missing_fields() {
        let output = "hci0:\tHCI Version: 5.4 (0xd)  Revision: 0xddd5\n";
        let info = BluetoothControllerInfo::from_hciconfig_output(output)
            .expect("version-only output still parses");
        assert_eq!(info.bus_type, None);
        assert_eq!(info.bd_address, None);
        assert_eq!(info.hci_version.as_deref(), Some("5.4"));
        assert_eq!(info.hci_revision.as_deref(), Some("0xddd5"));
        assert_eq!(info.firmware_version, None);
    }

    #[test]
    fn parse_hciconfig_version_empty_is_none() {
        assert!(BluetoothControllerInfo::from_hciconfig_output("").is_none());
        assert!(BluetoothControllerInfo::from_hciconfig_output("garbage text\n").is_none());
    }

    #[test]
    fn bt_error_predicates_and_conversions() {
        let dbus_err = BtError::dbus("connection refused");
        assert!(dbus_err.is_dbus());
        assert!(!dbus_err.is_adapter_not_found());
        assert!(!dbus_err.is_invalid_address());
        assert!(!dbus_err.is_cancelled());
        assert!(!dbus_err.is_mock_fault());
        assert_eq!(
            dbus_err.summary(),
            "D-Bus communication error: connection refused"
        );

        let adapter_err = BtError::adapter_not_found("hci1");
        assert!(adapter_err.is_adapter_not_found());
        assert_eq!(
            adapter_err.summary(),
            "Bluetooth adapter 'hci1' not detected"
        );

        let inv_err = BtError::invalid_address("abc", "too short");
        assert!(inv_err.is_invalid_address());
        assert_eq!(
            inv_err.summary(),
            "invalid Bluetooth address 'abc': too short"
        );

        let cancel_err = BtError::cancelled("aborted");
        assert!(cancel_err.is_cancelled());
        assert_eq!(cancel_err.summary(), "task cancelled: aborted");

        let mock_err = BtError::mock_fault("simulated error");
        assert!(mock_err.is_mock_fault());
        assert_eq!(mock_err.summary(), "injected mock fault: simulated error");

        let from_join = BtError::from_cancelled("timeout");
        assert!(from_join.is_cancelled());

        let _bt = dbus_err.backtrace();
    }

    #[test]
    fn mock_device_operations_and_fault_injection() {
        let (device, mock) = BluetoothDevice::new_mocked();

        // Initial happy path
        let status = device.status().unwrap();
        assert!(status.is_operational());
        assert_eq!(status.connected_count(), 1);
        assert_eq!(status.connected_devices().len(), 1);
        assert_eq!(status.paired_devices().len(), 1);
        assert!(status.is_streaming());
        assert!(status.active_pcm().is_some());

        // Test clear devices and add device
        mock.clear_devices();
        assert_eq!(device.status().unwrap().connected_count(), 0);

        let new_dev = BtDevice {
            address: BtAddress::new([1, 2, 3, 4, 5, 6]),
            name: "Headphones".into(),
            alias: "WH-1000XM4".into(),
            connected: true,
            paired: true,
            trusted: true,
            blocked: false,
            rssi: Some(-65),
            battery_percentage: Some(60),
            icon: Some("audio-headphones".into()),
            uuids: Box::default(),
        };
        mock.add_device(new_dev.clone());
        mock.add_device(new_dev); // updates existing
        assert_eq!(device.status().unwrap().connected_count(), 1);

        // Test PCMs
        mock.clear_pcms();
        assert!(!device.status().unwrap().is_streaming());
        assert!(device.status().unwrap().active_pcm().is_none());

        let new_pcm = BlueAlsaPcm {
            pcm_path: "/org/bluealsa/hci0/dev_01_02_03_04_05_06/a2dpsnk/source".into(),
            device_address: BtAddress::new([1, 2, 3, 4, 5, 6]),
            resolved_name: "WH-1000XM4".into(),
            transport: "A2DP-sink".into(),
            running: true,
            selected_codec: "LDAC".into(),
            available_codecs: vec!["SBC".into(), "LDAC".into()].into_boxed_slice(),
            sampling_rate: 96_000,
            channels: 2,
            format: "S32_LE".into(),
            volume: Some("80".into()),
            muted: false,
            delay: None,
        };
        mock.add_pcm(new_pcm);
        assert!(device.status().unwrap().is_streaming());

        // Test adapter controls on mock
        device.set_powered(false).unwrap();
        device.set_powered_for_adapter(Some("hci0"), true).unwrap();
        device
            .set_powered_for_adapter(Some("hci99"), true)
            .unwrap_err();

        device.set_discoverable(true).unwrap();
        device
            .set_discoverable_for_adapter(Some("hci0"), false)
            .unwrap();
        device
            .set_discoverable_for_adapter(Some("hci99"), false)
            .unwrap_err();

        device.set_pairable(false).unwrap();
        device.set_pairable_for_adapter(Some("hci0"), true).unwrap();
        device
            .set_pairable_for_adapter(Some("hci99"), true)
            .unwrap_err();

        // Target adapter missing on mock when adapter is cleared
        mock.set_adapter(None);
        let err_p = device.set_powered(true).unwrap_err();
        assert!(err_p.is_adapter_not_found());
        let err_d = device.set_discoverable(true).unwrap_err();
        assert!(err_d.is_adapter_not_found());
        let err_pair = device.set_pairable(true).unwrap_err();
        assert!(err_pair.is_adapter_not_found());

        // Fault injection
        mock.set_fault(Some("D-Bus daemon stopped".into()));
        let err = device.status().unwrap_err();
        assert!(err.is_mock_fault());
        assert!(err.to_string().contains("D-Bus daemon stopped"));
    }

    #[tokio::test]
    async fn async_methods_read_from_mock_device() {
        let (device, _) = BluetoothDevice::new_mocked();
        let status = device.status_async().await.unwrap();
        assert!(status.is_operational());
    }

    #[tokio::test]
    async fn status_for_adapter_queries() {
        let (device, mock) = BluetoothDevice::new_mocked();
        let status = device.status_for_adapter(Some("hci0")).unwrap();
        assert_eq!(status.adapter.as_ref().unwrap().id.as_ref(), "hci0");

        let err = device.status_for_adapter(Some("hci1")).unwrap_err();
        assert!(err.is_adapter_not_found());
        assert!(err.to_string().contains("hci1"));

        let async_status = device
            .status_for_adapter_async(Some("hci0".into()))
            .await
            .unwrap();
        assert_eq!(async_status.adapter.as_ref().unwrap().id.as_ref(), "hci0");

        let async_err = device
            .status_for_adapter_async(Some("hci1".into()))
            .await
            .unwrap_err();
        assert!(async_err.is_adapter_not_found());

        assert!(device.adapter_info_for(Some("hci0")).unwrap().is_some());
        device.adapter_info_for(Some("hci1")).unwrap_err();
        mock.read_status().unwrap();

        mock.set_adapter(None);
        let none_err = device.status_for_adapter(Some("hci0")).unwrap_err();
        assert!(none_err.is_adapter_not_found());
    }

    #[test]
    fn bluetooth_device_convenience_methods_and_debug() {
        let (device, mock) = BluetoothDevice::new_mocked();
        assert!(format!("{device:?}").contains("BluetoothDevice::Mock"));

        let zerr = BtError::from(zbus::Error::Address("test address".into()));
        assert!(zerr.is_dbus());
        assert_eq!(
            zerr.summary(),
            "D-Bus communication error: address error: test address"
        );

        device.services_status().unwrap();
        device.adapter_info().unwrap();
        device.connected_devices().unwrap();
        device.audio_pcms().unwrap();

        let new_services = BluetoothServices {
            bluetoothd: ServiceState::Active,
            bluealsa: ServiceState::Active,
            bluealsa_aplay: ServiceState::Inactive,
            bt_agent: ServiceState::Inactive,
        };
        mock.set_services(new_services.clone());
        assert_eq!(device.services_status().unwrap(), new_services);
    }

    #[test]
    fn parse_systemd_state_all_combinations() {
        assert_eq!(
            DbusClient::parse_systemd_state("active", "loaded"),
            ServiceState::Active
        );
        assert_eq!(
            DbusClient::parse_systemd_state("inactive", "not-found"),
            ServiceState::NotFound
        );
        assert_eq!(
            DbusClient::parse_systemd_state("not-found", "loaded"),
            ServiceState::NotFound
        );
        assert_eq!(
            DbusClient::parse_systemd_state("failed", "loaded"),
            ServiceState::Failed
        );
        assert_eq!(
            DbusClient::parse_systemd_state("inactive", "loaded"),
            ServiceState::Inactive
        );
        assert_eq!(
            DbusClient::parse_systemd_state("deactivating", "loaded"),
            ServiceState::Inactive
        );
    }

    fn to_val<T: Into<Value<'static>>>(v: T) -> OwnedValue {
        OwnedValue::try_from(v.into()).unwrap()
    }

    fn iface(name: &'static str) -> OwnedInterfaceName {
        OwnedInterfaceName::try_from(name).unwrap()
    }

    #[test]
    fn parse_bluez_objects_adapter_and_devices() {
        let mut objects: ManagedObjects = HashMap::new();

        // 1. Adapter object
        let adapter_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
        let mut adapter_ifaces = HashMap::new();
        let mut adapter_props = HashMap::new();
        adapter_props.insert("Address".to_string(), to_val("DC:A6:32:11:22:33"));
        adapter_props.insert("Name".to_string(), to_val("RaspberryPi"));
        adapter_props.insert("Alias".to_string(), to_val("Speaker-Pi"));
        adapter_props.insert("Powered".to_string(), to_val(true));
        adapter_props.insert("Discoverable".to_string(), to_val(false));
        adapter_props.insert("Pairable".to_string(), to_val(true));
        adapter_props.insert("Discovering".to_string(), to_val(false));
        adapter_props.insert("Class".to_string(), to_val(0x0024_0404_u32));
        adapter_props.insert("Modalias".to_string(), to_val("usb:v1D6Bp0246"));
        adapter_ifaces.insert(iface("org.bluez.Adapter1"), adapter_props);
        objects.insert(adapter_path, adapter_ifaces);

        // 2. Device object with Battery1
        let dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap();
        let mut dev_ifaces = HashMap::new();
        let mut dev_props = HashMap::new();
        dev_props.insert("Address".to_string(), to_val("AA:BB:CC:DD:EE:FF"));
        dev_props.insert("Name".to_string(), to_val("Pixel 8"));
        dev_props.insert("Alias".to_string(), to_val("Deepak's Phone"));
        dev_props.insert("Connected".to_string(), to_val(true));
        dev_props.insert("Paired".to_string(), to_val(true));
        dev_props.insert("Trusted".to_string(), to_val(true));
        dev_props.insert("Blocked".to_string(), to_val(false));
        dev_props.insert("RSSI".to_string(), to_val(-48i16));
        dev_props.insert("Icon".to_string(), to_val("phone"));
        dev_props.insert(
            "UUIDs".to_string(),
            to_val(vec!["0000110b-0000-1000-8000-00805f9b34fb"]),
        );
        dev_ifaces.insert(iface("org.bluez.Device1"), dev_props);

        let mut bat_props = HashMap::new();
        bat_props.insert("Percentage".to_string(), to_val(88u8));
        dev_ifaces.insert(iface("org.bluez.Battery1"), bat_props);
        objects.insert(dev_path, dev_ifaces);

        // 3. Invalid adapter without Address and with invalid Address
        let bad_adapter_path = OwnedObjectPath::try_from("/org/bluez/bad").unwrap();
        let mut bad_adapter_props = HashMap::new();
        bad_adapter_props.insert("Name".to_string(), to_val("BadAdapter"));
        let mut bad_adapter_ifaces = HashMap::new();
        bad_adapter_ifaces.insert(iface("org.bluez.Adapter1"), bad_adapter_props);
        objects.insert(bad_adapter_path, bad_adapter_ifaces);

        let bad_addr_adapter_path = OwnedObjectPath::try_from("/org/bluez/bad2").unwrap();
        let mut bad_addr_props = HashMap::new();
        bad_addr_props.insert("Address".to_string(), to_val("not-an-address"));
        let mut bad_addr_ifaces = HashMap::new();
        bad_addr_ifaces.insert(iface("org.bluez.Adapter1"), bad_addr_props);
        objects.insert(bad_addr_adapter_path, bad_addr_ifaces);

        // 4. Invalid device without Address and with invalid Address
        let bad_dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/bad_dev").unwrap();
        let mut bad_dev_props = HashMap::new();
        bad_dev_props.insert("Name".to_string(), to_val("NoAddressDev"));
        let mut bad_dev_ifaces = HashMap::new();
        bad_dev_ifaces.insert(iface("org.bluez.Device1"), bad_dev_props);
        objects.insert(bad_dev_path, bad_dev_ifaces);

        let bad_addr_dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/bad_dev2").unwrap();
        let mut bad_addr_dev_props = HashMap::new();
        bad_addr_dev_props.insert("Address".to_string(), to_val("bad-mac"));
        let mut bad_addr_dev_ifaces = HashMap::new();
        bad_addr_dev_ifaces.insert(iface("org.bluez.Device1"), bad_addr_dev_props);
        objects.insert(bad_addr_dev_path, bad_addr_dev_ifaces);

        // Parse with default adapter
        let (adapter, devices) = DbusClient::parse_bluez_objects(&objects, None).unwrap();
        let adapter = adapter.unwrap();
        assert_eq!(adapter.id.as_ref(), "hci0");
        assert_eq!(adapter.name.as_ref(), "RaspberryPi");
        assert_eq!(adapter.alias.as_ref(), "Speaker-Pi");
        assert!(adapter.powered);
        assert_eq!(adapter.class, Some(0x0024_0404));

        assert_eq!(devices.len(), 1);
        let dev = &devices[0];
        assert_eq!(dev.display_name(), "Deepak's Phone");
        assert!(dev.connected);
        assert_eq!(dev.battery_percentage, Some(88));
        assert_eq!(dev.rssi, Some(-48));
        assert_eq!(dev.uuids.len(), 1);

        // Parse with target adapter
        let (targeted, _) = DbusClient::parse_bluez_objects(&objects, Some("hci0")).unwrap();
        assert_eq!(targeted.unwrap().id.as_ref(), "hci0");

        // Parse with non-existent target adapter
        let not_found = DbusClient::parse_bluez_objects(&objects, Some("hci99")).unwrap_err();
        assert!(not_found.is_adapter_not_found());
    }

    #[test]
    fn parse_bluez_objects_filters_devices_by_target_adapter() {
        let mut objects: ManagedObjects = HashMap::new();

        let adapter0_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
        let mut adapter0_ifaces = HashMap::new();
        let mut adapter0_props = HashMap::new();
        adapter0_props.insert("Address".to_string(), to_val("DC:A6:32:00:00:00"));
        adapter0_ifaces.insert(iface("org.bluez.Adapter1"), adapter0_props);
        objects.insert(adapter0_path, adapter0_ifaces);

        let dev0_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_00_00_00_00_00").unwrap();
        let mut dev0_ifaces = HashMap::new();
        let mut dev0_props = HashMap::new();
        dev0_props.insert("Address".to_string(), to_val("AA:00:00:00:00:00"));
        dev0_ifaces.insert(iface("org.bluez.Device1"), dev0_props);
        objects.insert(dev0_path, dev0_ifaces);

        let adapter1_path = OwnedObjectPath::try_from("/org/bluez/hci1").unwrap();
        let mut adapter1_ifaces = HashMap::new();
        let mut adapter1_props = HashMap::new();
        adapter1_props.insert("Address".to_string(), to_val("DC:A6:32:11:11:11"));
        adapter1_ifaces.insert(iface("org.bluez.Adapter1"), adapter1_props);
        objects.insert(adapter1_path, adapter1_ifaces);

        let dev1_path = OwnedObjectPath::try_from("/org/bluez/hci1/dev_BB_11_11_11_11_11").unwrap();
        let mut dev1_ifaces = HashMap::new();
        let mut dev1_props = HashMap::new();
        dev1_props.insert("Address".to_string(), to_val("BB:11:11:11:11:11"));
        dev1_ifaces.insert(iface("org.bluez.Device1"), dev1_props);
        objects.insert(dev1_path, dev1_ifaces);

        let (a0, d0) = DbusClient::parse_bluez_objects(&objects, Some("hci0")).unwrap();
        assert_eq!(a0.unwrap().id.as_ref(), "hci0");
        assert_eq!(d0.len(), 1);
        assert_eq!(d0[0].address, BtAddress::new([0xAA, 0, 0, 0, 0, 0]));

        let (a1, d1) = DbusClient::parse_bluez_objects(&objects, Some("hci1")).unwrap();
        assert_eq!(a1.unwrap().id.as_ref(), "hci1");
        assert_eq!(d1.len(), 1);
        assert_eq!(
            d1[0].address,
            BtAddress::new([0xBB, 0x11, 0x11, 0x11, 0x11, 0x11])
        );
    }

    #[test]
    fn parse_bluealsa_objects_streams() {
        let mut objects: ManagedObjects = HashMap::new();

        let pcm_path =
            OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source")
                .unwrap();
        let mut pcm_ifaces = HashMap::new();
        let mut pcm_props = HashMap::new();
        pcm_props.insert(
            "Device".to_string(),
            to_val("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
        );
        pcm_props.insert("Transport".to_string(), to_val("A2DP-sink"));
        pcm_props.insert("Running".to_string(), to_val(true));
        pcm_props.insert("Codec".to_string(), to_val("AAC"));
        pcm_props.insert("Sampling".to_string(), to_val(48000u32));
        pcm_props.insert("Channels".to_string(), to_val(2u8));
        pcm_props.insert("Format".to_string(), to_val(0x8210u16));
        pcm_props.insert("Volume".to_string(), to_val(vec![0x7Fu8, 0x7Fu8]));
        pcm_props.insert("Delay".to_string(), to_val(1500u16));

        pcm_ifaces.insert(iface("org.bluealsa.PCM1"), pcm_props);
        objects.insert(pcm_path, pcm_ifaces);

        // Stream 2 with string codecs, Rate, u16 channels, u16 volume, Muted, string delay
        let pcm_path2 =
            OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/sink")
                .unwrap();
        let mut pcm_ifaces2 = HashMap::new();
        let mut pcm_props2 = HashMap::new();
        pcm_props2.insert(
            "Device".to_string(),
            to_val("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
        );
        pcm_props2.insert("Transport".to_string(), to_val("A2DP-sink"));
        pcm_props2.insert("Running".to_string(), to_val(false));
        pcm_props2.insert("Codec".to_string(), to_val("SBC"));
        pcm_props2.insert("Rate".to_string(), to_val(44100u32));
        pcm_props2.insert("Format".to_string(), to_val(0x8210u16));
        pcm_props2.insert("Channels".to_string(), to_val(2u16));
        pcm_props2.insert(
            "ChannelMap".to_string(),
            to_val(vec!["L".to_string(), "R".to_string()]),
        );
        pcm_props2.insert("Volume".to_string(), to_val(vec![0xFFu8, 0x7Fu8]));
        pcm_props2.insert("Delay".to_string(), to_val(1200u16));
        pcm_ifaces2.insert(iface("org.bluealsa.PCM1"), pcm_props2);
        objects.insert(pcm_path2, pcm_ifaces2);

        // Stream 3 with [ Unknown ] codecs and no Device (skipped)
        let pcm_path3 =
            OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_unknown/sco/source").unwrap();
        let mut pcm_ifaces3 = HashMap::new();
        let mut pcm_props3 = HashMap::new();
        pcm_props3.insert("Codecs".to_string(), to_val("[ Unknown ]"));
        pcm_ifaces3.insert(iface("org.bluealsa.PCM1"), pcm_props3);
        objects.insert(pcm_path3, pcm_ifaces3);

        let devices = vec![BtDevice {
            address: BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
            name: "Phone".into(),
            alias: "Pixel Phone".into(),
            connected: true,
            paired: true,
            trusted: true,
            blocked: false,
            rssi: None,
            battery_percentage: None,
            icon: None,
            uuids: Box::default(),
        }];

        let pcms = DbusClient::parse_bluealsa_objects(&objects, &devices);
        assert_eq!(pcms.len(), 2);

        // Sorted by pcm_path: .../sink comes before .../source
        let sink_pcm = &pcms[0];
        assert_eq!(sink_pcm.selected_codec.as_ref(), "SBC");
        assert!(sink_pcm.available_codecs.is_empty());
        assert_eq!(sink_pcm.sampling_rate, 44100);
        assert_eq!(sink_pcm.channels, 2);
        assert!(!sink_pcm.running);
        assert!(sink_pcm.muted);
        assert_eq!(sink_pcm.volume.as_deref(), Some("L: 127 R: 127"));
        assert_eq!(sink_pcm.delay.as_deref(), Some("120.0 ms"));
        assert_eq!(sink_pcm.format.as_ref(), "S16_2LE");

        let source_pcm = &pcms[1];
        assert_eq!(source_pcm.resolved_name.as_ref(), "Pixel Phone");
        assert_eq!(source_pcm.selected_codec.as_ref(), "AAC");
        assert!(source_pcm.available_codecs.is_empty());
        assert_eq!(source_pcm.sampling_rate, 48000);
        assert_eq!(source_pcm.channels, 2);
        assert!(source_pcm.running);
        assert!(!source_pcm.muted);
        assert_eq!(source_pcm.volume.as_deref(), Some("CH0: 127 CH1: 127"));
        assert_eq!(source_pcm.delay.as_deref(), Some("150.0 ms"));
    }

    #[test]
    fn decode_pcm_format_identifiers() {
        // Signed/unsigned prefix, width, endian; no byte suffix when bytes*8 == width.
        assert_eq!(decode_pcm_format(0x8210).as_ref(), "S16_2LE");
        assert_eq!(decode_pcm_format(0x0108).as_ref(), "U8LE");
        assert_eq!(decode_pcm_format(0x8420).as_ref(), "S32_4LE");
        // Physical bytes larger than width: underscore byte-count suffix.
        assert_eq!(decode_pcm_format(0x8318).as_ref(), "S24_3LE");
        assert_eq!(decode_pcm_format(0x8418).as_ref(), "S24_4LE");
        // Doc examples: unsigned 16-bit 2 bytes big-endian.
        assert_eq!(decode_pcm_format(0x4210).as_ref(), "U16_2BE");
        // Invalid: zero id, zero width, zero bytes, width larger than bytes*8.
        assert_eq!(decode_pcm_format(0).as_ref(), "Invalid");
        assert_eq!(decode_pcm_format(0x8100).as_ref(), "Invalid");
        assert_eq!(decode_pcm_format(0x8000).as_ref(), "Invalid");
        assert_eq!(decode_pcm_format(0x8020).as_ref(), "Invalid");
    }

    #[test]
    fn format_volume_bytes_channels() {
        // Empty input: no volume, not muted.
        assert_eq!(
            format_volume_bytes(&[], None),
            (Option::<Box<str>>::None, false)
        );
        // Stereo, unmuted, generic channel names.
        assert_eq!(
            format_volume_bytes(&[0x7F, 0x40], None),
            (Some("CH0: 127 CH1: 64".into()) as Option<Box<str>>, false)
        );
        // Any muted high bit → muted true; levels masked to 7 bits.
        assert_eq!(
            format_volume_bytes(&[0xFF, 0x7F], None),
            (Some("CH0: 127 CH1: 127".into()) as Option<Box<str>>, true)
        );
        // ChannelMap names override generic indices.
        let names: Box<[Box<str>]> = vec!["L".into(), "R".into()].into_boxed_slice();
        assert_eq!(
            format_volume_bytes(&[0x80, 0x20], Some(&names)),
            (Some("L: 0 R: 32".into()) as Option<Box<str>>, true)
        );
        // ChannelMap shorter than volume bytes: falls back to CH{i}.
        let one_name: Box<[Box<str>]> = vec!["L".into()].into_boxed_slice();
        assert_eq!(
            format_volume_bytes(&[0x10, 0x20], Some(&one_name)),
            (Some("L: 16 CH1: 32".into()) as Option<Box<str>>, false)
        );
    }

    #[test]
    fn extract_mac_from_pcm_path_validation() {
        assert_eq!(
            DbusClient::extract_mac_from_pcm_path(
                "/org/bluealsa/hci0/dev_11_22_33_44_55_66/a2dpsnk/source"
            )
            .as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(
            DbusClient::extract_mac_from_pcm_path(
                "/org/bluealsa/hci0/dev_GG_GG_GG_GG_GG_GG/a2dpsnk/source"
            ),
            None
        );
        assert_eq!(
            DbusClient::extract_mac_from_pcm_path("/org/bluealsa/hci0/dev_11_22_33/a2dpsnk/source"),
            None
        );
        assert_eq!(DbusClient::extract_mac_from_pcm_path("/invalid/path"), None);
    }

    #[test]
    fn unit_not_found_error_matching() {
        use zbus::fdo;
        use zbus::names::OwnedErrorName;

        let dummy_msg = zbus::message::Message::method_call("/org/test", "Test")
            .unwrap()
            .build(&())
            .unwrap();

        let err_no_such = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.systemd1.NoSuchUnit").unwrap(),
            Some("Unit not loaded".into()),
            dummy_msg.clone(),
        );
        assert!(is_no_such_unit_error(&err_no_such));

        let err_unknown_unit = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.systemd1.UnknownUnit").unwrap(),
            None,
            dummy_msg.clone(),
        );
        assert!(is_no_such_unit_error(&err_unknown_unit));

        let err_not_found = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.DBus.Error.NotFound").unwrap(),
            None,
            dummy_msg.clone(),
        );
        assert!(!is_no_such_unit_error(&err_not_found));

        let err_denied = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.DBus.Error.AccessDenied").unwrap(),
            None,
            dummy_msg.clone(),
        );
        assert!(!is_no_such_unit_error(&err_denied));

        let err_fdo_unknown =
            zbus::Error::FDO(Box::new(fdo::Error::UnknownObject("test".to_string())));
        assert!(is_no_such_unit_error(&err_fdo_unknown));

        let err_fdo_failed = zbus::Error::FDO(Box::new(fdo::Error::Failed("failed".to_string())));
        assert!(!is_no_such_unit_error(&err_fdo_failed));

        let err_string_fallback =
            zbus::Error::Failure("org.freedesktop.systemd1.NoSuchUnit".to_string());
        assert!(is_no_such_unit_error(&err_string_fallback));

        let err_io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        )));
        assert!(!is_no_such_unit_error(&err_io));
    }

    #[test]
    fn service_absent_error_matching() {
        use zbus::fdo;
        use zbus::names::OwnedErrorName;

        let dummy_msg = zbus::message::Message::method_call("/org/test", "Test")
            .unwrap()
            .build(&())
            .unwrap();

        let err_svc_unknown = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.DBus.Error.ServiceUnknown").unwrap(),
            None,
            dummy_msg.clone(),
        );
        assert!(is_service_absent_error(&err_svc_unknown));

        let err_no_owner = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.DBus.Error.NameHasNoOwner").unwrap(),
            None,
            dummy_msg.clone(),
        );
        assert!(is_service_absent_error(&err_no_owner));

        let err_fdo_iface =
            zbus::Error::FDO(Box::new(fdo::Error::UnknownInterface("test".to_string())));
        assert!(is_service_absent_error(&err_fdo_iface));

        let err_fdo_obj = zbus::Error::FDO(Box::new(fdo::Error::UnknownObject("test".to_string())));
        assert!(is_service_absent_error(&err_fdo_obj));

        assert!(is_service_absent_fdo_error(&fdo::Error::ServiceUnknown(
            "not found".into()
        )));
        assert!(is_service_absent_fdo_error(&fdo::Error::NameHasNoOwner(
            "no owner".into()
        )));
        assert!(is_service_absent_fdo_error(&fdo::Error::UnknownInterface(
            "unknown".into()
        )));
        assert!(is_service_absent_fdo_error(&fdo::Error::UnknownObject(
            "unknown".into()
        )));
        assert!(!is_service_absent_fdo_error(&fdo::Error::AccessDenied(
            "denied".into()
        )));

        let err_denied = zbus::Error::MethodError(
            OwnedErrorName::try_from("org.freedesktop.DBus.Error.AccessDenied").unwrap(),
            None,
            dummy_msg,
        );
        assert!(!is_service_absent_error(&err_denied));

        let err_string_fallback =
            zbus::Error::Failure("The name org.bluealsa was not provided (ServiceUnknown)".into());
        assert!(is_service_absent_error(&err_string_fallback));

        let err_io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )));
        assert!(!is_service_absent_error(&err_io));
    }
}
