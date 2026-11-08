//! Bluetooth controller, D-Bus client, and device connection logic.
//!
//! Core newtypes (`BtAddress`), controller hardware info, systemd/BlueZ D-Bus
//! client, the in-memory mock controller, and the user-facing `BluetoothDevice`
//! facade with its transport command plumbing.

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zbus::blocking::Connection;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::cover_service::CoverSnapshot;
use speaker_common::runtime::FromJoinError;

use crate::cover_art::{COVER_ART_CACHE_DIR, cover_art_cached_path_in};
use crate::transport::{
    BlueAlsaPcm, BtPlaybackState, apply_mute_to_volume_bytes, decode_pcm_format,
    encode_volume_bytes, format_volume_bytes,
};

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

pub use speaker_common::systemd::is_no_such_unit_error;
use speaker_common::systemd::{ServiceState, SystemdManagerProxyBlocking, check_systemd_unit};

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

/// Raspberry Pi Bluetooth controller chip and transport bus metadata.
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
/// Hard deadline for external probe commands (production). Shrunk under
/// `cfg(test)` so the timeout path is testable in real time.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn probe_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_millis(150)
    } else {
        PROBE_TIMEOUT
    }
}

fn run_probe_command(program: &str, args: &[&str]) -> Result<String, std::io::Error> {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + probe_timeout();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "external process timed out",
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e);
            }
        }
    };
    let status = status?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{program} exited with a non-success status"
        )));
    }
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout was taken"))?
        .read_to_end(&mut stdout)?;
    String::from_utf8(stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.utf8_error()))
}

fn run_hciconfig_version(adapter: &str) -> Option<String> {
    match run_probe_command("hciconfig", &[adapter, "version"]) {
        Ok(output) => Some(output),
        Err(e) => {
            tracing::debug!(error = %e, "hciconfig {adapter} version probe failed");
            None
        }
    }
}

/// Parses `hciconfig <adapter> version` output into controller identity fields.
pub fn parse_hciconfig_version(output: &str) -> Option<BluetoothControllerInfo> {
    let mut info = BluetoothControllerInfo::unknown();
    for (index, line) in output.lines().enumerate() {
        let line = line.trim();
        if index == 0
            && let Some((dev, _)) = line.split_once(':')
        {
            let dev = dev.trim();
            if dev.starts_with("hci") {
                info.adapter = Some(dev.to_string().into_boxed_str());
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

/// Lightweight adapter, device, and audio-stream snapshot for live UI refresh.
///
/// Unlike [`BluetoothStatus`] it skips the `systemd` unit queries and the
/// `hciconfig` subprocess probe, so it is cheap enough for the 250 ms monitor tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSnapshot {
    /// Active local controller adapter, if detected.
    pub adapter: Option<BtAdapter>,
    /// Known remote devices.
    pub devices: Box<[BtDevice]>,
    /// Active `BlueALSA` audio PCM streams.
    pub pcms: Box<[BlueAlsaPcm]>,
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
///
/// The optional `source` field preserves the typed underlying error (e.g. the
/// original `zbus::Error`) so the cause chain survives `?` conversions, per
/// the M-ERRORS-STD-SOURCE guideline.
#[derive(Debug)]
pub struct BtError {
    kind: BtErrorKind,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    backtrace: Backtrace,
}

impl BtError {
    fn new(kind: BtErrorKind) -> Self {
        Self {
            kind,
            source: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Attaches an underlying error as this error's [`std::error::Error::source`].
    fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Builds a D-Bus error with a human-readable `details` context *and* the
    /// typed underlying error preserved as the source.
    pub(crate) fn dbus_with_source(
        details: impl Into<Box<str>>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::new(BtErrorKind::Dbus {
            details: details.into(),
        })
        .with_source(source)
    }

    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn dbus(details: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::Dbus {
            details: details.into(),
        })
    }

    #[must_use]
    pub fn adapter_not_found(adapter: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::AdapterNotFound {
            adapter: adapter.into(),
        })
    }

    #[must_use]
    pub fn invalid_address(raw: impl Into<Box<str>>, reason: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::InvalidAddress {
            raw: raw.into(),
            reason: reason.into(),
        })
    }

    #[must_use]
    pub fn no_active_player(details: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::NoActivePlayer {
            details: details.into(),
        })
    }

    #[must_use]
    pub fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::Cancelled {
            message: message.into(),
        })
    }

    #[must_use]
    pub fn mock_fault(message: impl Into<Box<str>>) -> Self {
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

speaker_common::impl_canonical_error_display!(BtError);

impl std::error::Error for BtError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|boxed| {
            let erased: &(dyn std::error::Error + 'static) = &**boxed;
            erased
        })
    }
}

impl From<zbus::Error> for BtError {
    fn from(err: zbus::Error) -> Self {
        Self::dbus_with_source(err.to_string(), err)
    }
}

impl From<zbus::fdo::Error> for BtError {
    fn from(err: zbus::fdo::Error) -> Self {
        Self::dbus_with_source(err.to_string(), err)
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
    if let Some(val) = props.get(key)
        && let Value::Array(arr) = &**val
    {
        return arr
            .iter()
            .filter_map(|v| v.downcast_ref::<&str>().ok().map(Into::into))
            .collect();
    }
    Box::default()
}

/// Extraction rules shared by all track-property readers so that the
/// dictionary key-matching and nested-`Value` unwrapping scaffolding has a
/// single implementation (see `get_track_prop`).
trait TrackPropExtract: Sized {
    /// Extracts the value from a plain dict entry.
    fn extract(value: &Value<'_>) -> Option<Self>;

    /// Extracts the value from an entry that wraps an inner `Value` variant
    /// (produced when `BlueZ` nests `v`-typed values inside `a{sv}` maps).
    fn extract_nested(value: &Value<'_>) -> Option<Self> {
        if let Value::Value(inner) = value {
            Self::extract(inner)
        } else {
            None
        }
    }
}

impl TrackPropExtract for Box<str> {
    fn extract(value: &Value<'_>) -> Option<Self> {
        if let Ok(s) = value.downcast_ref::<&str>() {
            return Some(s.into());
        }
        if let Value::Str(s) = value {
            return Some(s.as_str().into());
        }
        None
    }
}

impl TrackPropExtract for u32 {
    fn extract(value: &Value<'_>) -> Option<Self> {
        if let Ok(u) = value.downcast_ref::<u32>() {
            return Some(u);
        }
        if let Value::U32(u) = value {
            return Some(*u);
        }
        None
    }
}

/// Looks up `key` inside a `a{sv}` dict and extracts its typed value,
/// transparently unwrapping nested `Value` variants.
fn get_track_prop<T: TrackPropExtract>(val: &OwnedValue, key: &str) -> Option<T> {
    if let Value::Dict(dict) = &**val {
        for (k, v) in dict.iter() {
            let matches_key = if let Ok(s) = k.downcast_ref::<&str>() {
                s == key
            } else if let Value::Str(s) = k {
                s.as_str() == key
            } else {
                false
            };
            if !matches_key {
                continue;
            }
            if let Some(out) = T::extract(v) {
                return Some(out);
            }
            if let Some(out) = T::extract_nested(v) {
                return Some(out);
            }
        }
    }
    None
}

fn get_track_prop_str(val: &OwnedValue, key: &str) -> Option<Box<str>> {
    get_track_prop::<Box<str>>(val, key)
}

fn get_track_prop_u32(val: &OwnedValue, key: &str) -> Option<u32> {
    get_track_prop::<u32>(val, key)
}

/// Inspects managed objects for any `org.bluez.MediaItem1` under `player_path`
/// that advertises an `ImgHandle` in its `Metadata` dictionary (e.g. from `/NowPlaying`).
fn find_item_image_handle(
    objects: &HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>>,
    player_path: &str,
) -> Option<Box<str>> {
    for (obj_path, ifaces) in objects {
        if obj_path.as_str().starts_with(player_path)
            && let Some(item_props) = find_iface(ifaces, "org.bluez.MediaItem1")
            && let Some(metadata) = item_props.get("Metadata")
            && let Some(handle) = get_track_prop_str(metadata, "ImgHandle")
            && !handle.is_empty()
        {
            return Some(handle);
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

/// Detects `UnknownMethod`/`UnknownObject` D-Bus error replies.
#[must_use]
pub fn is_unknown_method_error(err: &zbus::Error) -> bool {
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

/// Shared change-notification flag used by the D-Bus backend and the mock
/// to wake property monitors when the subsystem state may have changed
/// (event-driven instead of fixed-interval polling).
#[derive(Debug, Default)]
pub(crate) struct ChangeFlag {
    ready: Mutex<bool>,
    cv: Condvar,
}

impl ChangeFlag {
    /// Marks a change as pending and wakes all waiting monitors.
    pub(crate) fn notify(&self) {
        if let Ok(mut ready) = self.ready.lock() {
            *ready = true;
            self.cv.notify_all();
        }
    }

    /// Waits up to `timeout` for a change notification, consuming it if present.
    ///
    /// Returns `true` when a change was signaled, `false` on keep-alive timeout.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let mut ready = self.ready.lock().unwrap_or_else(|p| {
            tracing::warn!("ChangeFlag mutex poisoned; recovering inner state");
            p.into_inner()
        });
        // Consume the flag under the same lock acquisition that observed it: a
        // separate later clear would erase any notify() landing between the wait
        // returning and the clear, delaying the wake by a full keep-alive period.
        // Clearing before waiting is safe because notify() also signals the
        // condvar, so a change arriving mid-wait still wakes this waiter.
        let changed = *ready;
        *ready = false;
        if !changed {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Ok((guard, res)) = self.cv.wait_timeout(ready, remaining) else {
                    break;
                };
                ready = guard;
                if *ready {
                    *ready = false;
                    return true;
                }
                if res.timed_out() {
                    break;
                }
            }
        }
        changed
    }
}

/// Backoff between D-Bus signal-listener reconnection attempts after a setup
/// failure or iterator exhaustion (transient bus restart).
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Spawns a background thread that watches for `org.freedesktop.DBus.Properties`
/// change signals from `org.bluez` and `org.bluealsa` on the system bus and wakes
/// the shared [`ChangeFlag`] whenever one arrives.
///
/// If the system bus cannot be reached, the thread retries with backoff and
/// callers fall back to the keep-alive timeout polling path meanwhile.
fn spawn_properties_changed_listener(
    conn_slot: Arc<Mutex<Option<Connection>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Arc<ChangeFlag> {
    let flag = Arc::new(ChangeFlag::default());
    let listener_flag = Arc::clone(&flag);
    let Ok(join) = std::thread::Builder::new()
        .name("bt-dbus-signals".into())
        .spawn(move || {
            // A single unfiltered PropertiesChanged rule avoids the deadlock of
            // per-sender blocking iterators (each `next()` would block forever on
            // its own sender). Extra wakeups from unrelated services are cheap:
            // they just trigger one extra state re-poll on the monitor thread.
            //
            // The listener shares the client's connection instead of opening a
            // second system-bus connection so that private-bus integration tests
            // and injected connections observe the signals under test. On rule
            // setup failure or iterator exhaustion (bus lost) the thread sleeps
            // a backoff interval and rebuilds the rule/iterator, honoring the
            // stop flag between attempts (the keep-alive polling fallback covers
            // the gap while a blocked `next()` cannot observe the flag).
            loop {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let Ok(rule) = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .interface("org.freedesktop.DBus.Properties")
                    .map(zbus::match_rule::Builder::build)
                else {
                    std::thread::sleep(RECONNECT_BACKOFF);
                    continue;
                };
                // Clone the connection out of the slot so the slot lock is
                // released while the thread parks inside the iterator.
                // Otherwise `DbusClient::drop` (which must lock the slot to
                // take and close the connection) would deadlock against the
                // parked listener thread.
                let Some(conn) = conn_slot.lock().unwrap().as_ref().cloned() else {
                    // Slot emptied by drop: the connection has been closed.
                    return;
                };
                let Ok(iter) = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)
                else {
                    std::thread::sleep(RECONNECT_BACKOFF);
                    continue;
                };
                for msg in iter.flatten() {
                    // Only wake the monitor for Bluetooth-related objects; an
                    // unfiltered rule would re-query BlueZ/BlueALSA for every
                    // unrelated PropertiesChanged on a busy system bus.
                    if msg.header().path().is_some_and(|p| {
                        let p = p.as_str();
                        p.starts_with("/org/bluez") || p.starts_with("/org/bluealsa")
                    }) {
                        listener_flag.notify();
                    }
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                }
                // Iterator exhausted (bus lost): back off and rebuild.
                std::thread::sleep(RECONNECT_BACKOFF);
            }
        })
    else {
        // Thread spawn failed: rely on the keep-alive fallback only.
        return flag;
    };
    drop(join);
    flag
}

/// D-Bus client executing direct queries against systemd, `BlueZ`, and `BlueALSA`.
///
/// Deliberately not `Clone`: the `Drop` impl signals the per-client listener
/// thread to stop, which must only happen when the last owner is dropped.
#[derive(Debug)]
pub struct DbusClient {
    conn_slot: Arc<Mutex<Option<Connection>>>,
    device_caps_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    change_flag: Arc<ChangeFlag>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

/// Snapshot of `BlueZ` adapter, connected/paired devices, and media player state.
pub type BluezSnapshot = (Option<BtAdapter>, Box<[BtDevice]>, Option<BtPlaybackState>);

impl Drop for DbusClient {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let taken = self.conn_slot.lock().unwrap().take();
        if let Some(conn) = taken
            && let Err(e) = conn.close()
        {
            tracing::warn!("failed to close D-Bus connection on drop: {e}");
        }
    }
}

impl DbusClient {
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        let conn_slot = Arc::new(Mutex::new(Some(conn)));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            conn_slot: Arc::clone(&conn_slot),
            device_caps_dir: std::path::PathBuf::from(COVER_ART_CACHE_DIR),
            cache_dir: std::path::PathBuf::from(COVER_ART_CACHE_DIR),
            change_flag: spawn_properties_changed_listener(conn_slot, Arc::clone(&stop)),
            stop,
        }
    }

    /// Clones the current D-Bus connection out of the slot.
    ///
    /// The `Drop` impl drains the slot and closes the connection to release
    /// the parked listener thread; reaching this method afterwards is a
    /// use-after-drop bug, so panicking is the correct behavior.
    fn conn(&self) -> Connection {
        self.conn_slot
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .expect("DbusClient connection accessed after drop")
    }

    /// Waits up to `timeout` for a D-Bus property-change notification.
    ///
    /// Returns `true` when a change was signaled, `false` on timeout (keep-alive).
    pub(crate) fn wait_for_change(&self, timeout: Duration) -> bool {
        self.change_flag.wait(timeout)
    }

    pub(crate) fn check_services(&self) -> Result<BluetoothServices, BtError> {
        let manager = SystemdManagerProxyBlocking::new(&self.conn())?;
        // Check for bt-manager.service (native auto-trust agent + cover art provider),
        // falling back to legacy bt-agent.service if present.
        let bt_agent = match self.check_service(&manager, "bt-manager.service")? {
            s if s.is_active() => s,
            s => match self.check_service(&manager, "bt-agent.service")? {
                legacy if legacy.is_active() => legacy,
                _ => s,
            },
        };
        Ok(BluetoothServices {
            bluetoothd: self.check_service(&manager, "bluetooth.service")?,
            bluealsa: self.check_service(&manager, "bluealsa.service")?,
            bluealsa_aplay: self.check_service(&manager, "bluealsa-aplay.service")?,
            bt_agent,
        })
    }

    fn check_service(
        &self,
        manager: &SystemdManagerProxyBlocking<'_>,
        name: &str,
    ) -> Result<ServiceState, BtError> {
        check_systemd_unit(&self.conn(), manager, name).map_err(BtError::from)
    }

    #[must_use]
    pub fn parse_systemd_state(active: &str, load: &str) -> ServiceState {
        speaker_common::systemd::parse_systemd_state(active, load)
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
        objects: &HashMap<
            OwnedObjectPath,
            HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
        >,
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
            let artist = get_track_prop_str(track_val, "Artist").unwrap_or_default();
            let album = get_track_prop_str(track_val, "Album").unwrap_or_default();
            let duration_ms = get_track_prop_u32(track_val, "Duration").unwrap_or(0);
            (title, artist, album, duration_ms)
        } else {
            // Disconnected / no track published yet: keep the "Waiting for
            // Bluetooth Connection..." placeholder title and leave the artist
            // and album lines empty so the UI does not display stale branding.
            (
                super::transport::BT_NO_CONNECTION_TITLE.into(),
                "".into(),
                "".into(),
                0,
            )
        };

        let obex_port = get_u16(props, "ObexPort").filter(|&p| p > 0);
        let image_handle = props
            .get("Track")
            .and_then(|track_val| get_track_prop_str(track_val, "ImgHandle"))
            .filter(|h| !h.is_empty())
            .or_else(|| find_item_image_handle(objects, path));

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
            obex_port,
            image_handle,
            cover_art_path: None,
        }
    }

    pub(crate) fn query_bluez(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<BluezSnapshot, BtError> {
        let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn())
            .destination("org.bluez")
            .and_then(|b| b.path("/"))
            .and_then(zbus::blocking::proxy::Builder::build)?;

        let objects = proxy.get_managed_objects()?;

        let (adapter, devices, playback) = Self::parse_bluez_all(&objects, target_adapter)?;
        Ok((adapter, devices, playback))
    }

    fn resolve_cover_art(&self, playback: &mut Option<BtPlaybackState>) {
        resolve_cover_art_with(&self.conn(), &self.cache_dir, playback);
    }

    /// Parses adapter and device entries out of a `GetManagedObjects` snapshot.
    ///
    /// # Errors
    /// Returns [`BtError`] when a device entry cannot be parsed.
    pub fn parse_bluez_objects(
        objects: &ManagedObjects,
        target_adapter: Option<&str>,
    ) -> Result<(Option<BtAdapter>, Box<[BtDevice]>), BtError> {
        let mut adapters = Vec::new();
        let mut devices = Vec::new();

        // 1. Search for Adapter1 interfaces
        for (path, ifaces) in objects {
            if let Some(props) = find_iface(ifaces, "org.bluez.Adapter1")
                && let Some(adapter) = Self::parse_adapter_props(path.as_str(), props)
            {
                adapters.push(adapter);
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

        // 3. Search for MediaPlayer1 interfaces belonging to the selected adapter.
        // All candidates are collected and then selected deterministically:
        // prefer a player that is actively playing; among equal-status candidates,
        // break ties by lexicographic object path so the `HashMap` iteration order
        // of `ManagedObjects` cannot influence the selection.
        let mut candidates: Vec<BtPlaybackState> = Vec::new();
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

                candidates.push(Self::parse_media_player_props(
                    path.as_str(),
                    props,
                    &devices,
                    objects,
                ));
            }
        }

        candidates.sort_by(|a, b| {
            b.is_playing()
                .cmp(&a.is_playing())
                .then_with(|| a.player_path.cmp(&b.player_path))
        });
        let playback = candidates.into_iter().next();

        Ok((adapter, devices, playback))
    }

    /// Extracts the active `MediaPlayer1` object path from a parsed playback state.
    ///
    /// # Errors
    /// Returns [`BtError`] if `playback` is `None` or has no object path.
    pub fn extract_player_path(playback: Option<BtPlaybackState>) -> Result<Box<str>, BtError> {
        playback
            .and_then(|p| p.player_path)
            .ok_or_else(|| BtError::no_active_player("no active MediaPlayer1 found on D-Bus"))
    }

    /// Resolves the target D-Bus object path, method name (`"Play"` / `"Pause"`),
    /// and resulting playing state for a toggle operation.
    ///
    /// # Errors
    /// Returns [`BtError`] if no active player or player path is present.
    pub fn resolve_toggle_target(
        playback: Option<BtPlaybackState>,
    ) -> Result<(Box<str>, &'static str, bool), BtError> {
        let playback = playback
            .ok_or_else(|| BtError::no_active_player("no active MediaPlayer1 found on D-Bus"))?;
        let next_playing = !playback.is_playing();
        let method = if playback.is_playing() {
            "Pause"
        } else {
            "Play"
        };
        let path = playback
            .player_path
            .ok_or_else(|| BtError::no_active_player("active MediaPlayer1 has no object path"))?;
        Ok((path, method, next_playing))
    }

    /// Resolves the target D-Bus object path, current position (ms), and relative
    /// microsecond offset for a `MediaPlayer1.Seek(i64)` operation.
    ///
    /// # Errors
    /// Returns [`BtError`] if no active player or player path is present.
    pub fn resolve_seek_target(
        playback: Option<BtPlaybackState>,
        position_ms: u32,
    ) -> Result<(Box<str>, u32, i64), BtError> {
        let playback = playback
            .ok_or_else(|| BtError::no_active_player("no active MediaPlayer1 found on D-Bus"))?;
        let current_ms = playback.position_ms;
        let path = playback
            .player_path
            .ok_or_else(|| BtError::no_active_player("active MediaPlayer1 has no object path"))?;
        let offset_us = super::transport::seek_offset_us(position_ms, current_ms);
        Ok((path, current_ms, offset_us))
    }

    fn call_player_on_path<B>(&self, path: &str, method: &str, body: &B) -> Result<(), BtError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let proxy =
            zbus::blocking::Proxy::new(&self.conn(), "org.bluez", path, "org.bluez.MediaPlayer1")?;
        proxy.call_method(method, body)?;
        Ok(())
    }

    pub(crate) fn call_player_method(&self, method: &str) -> Result<(), BtError> {
        let path = Self::extract_player_path(self.playback_state()?)?;
        self.call_player_on_path(&path, method, &())
    }

    /// Seeks to an absolute position by translating the target into the relative
    /// `Seek(i64 offset)` call exposed by `BlueZ`'s `MediaPlayer1` (offset in
    /// microseconds; a positive offset moves forward, a negative offset moves
    /// backward).
    ///
    /// The current position is read from a fresh managed-objects snapshot via
    /// [`DbusClient::playback_state`] so that seeking never triggers an OBEX
    /// cover-art fetch.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no active player exists.
    pub(crate) fn player_seek(&self, position_ms: u32) -> Result<(), BtError> {
        let (path, current_ms, offset_us) =
            Self::resolve_seek_target(self.playback_state()?, position_ms)?;
        self.call_player_on_path(&path, "Seek", &(offset_us,))?;
        tracing::debug!(
            position_ms,
            current_ms,
            offset_us,
            "seek issued via BlueZ MediaPlayer1.Seek"
        );
        Ok(())
    }

    /// Returns the playback state without resolving cover art.
    ///
    /// Cover art is resolved asynchronously at the [`BluetoothDevice`] layer
    /// (a detached worker fetches into the disk cache and a later poll picks
    /// the file up), so a slow OBEX transfer never blocks track metadata.
    pub(crate) fn playback_state(&self) -> Result<Option<BtPlaybackState>, BtError> {
        let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn())
            .destination("org.bluez")
            .and_then(|b| b.path("/"))
            .and_then(zbus::blocking::proxy::Builder::build)?;
        let objects = proxy.get_managed_objects()?;
        let (_, _, playback) = Self::parse_bluez_all(&objects, None)?;
        Ok(playback)
    }

    pub(crate) fn player_toggle(&self) -> Result<bool, BtError> {
        let (path, method, next_playing) = Self::resolve_toggle_target(self.playback_state()?)?;
        self.call_player_on_path(&path, method, &())?;
        Ok(next_playing)
    }

    /// Maps the result of `org.bluealsa` `GetManagedObjects` into a [`ManagedObjects`]
    /// snapshot, treating service absence as an empty object map.
    ///
    /// # Errors
    /// Returns [`BtError`] on transport or permission errors unrelated to service absence.
    pub fn map_bluealsa_objects_result(
        res: Result<ManagedObjects, zbus::fdo::Error>,
    ) -> Result<ManagedObjects, BtError> {
        match res {
            Ok(objs) => Ok(objs),
            Err(e) if is_service_absent_fdo_error(&e) => Ok(ManagedObjects::new()),
            Err(e) => Err(BtError::from(e)),
        }
    }

    fn fetch_bluealsa_objects(&self) -> Result<ManagedObjects, BtError> {
        let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn())
            .destination("org.bluealsa")
            .and_then(|b| b.path("/org/bluealsa"))
            .and_then(zbus::blocking::proxy::Builder::build);
        match proxy {
            Ok(p) => Self::map_bluealsa_objects_result(p.get_managed_objects()),
            Err(e) if is_service_absent_error(&e) => Ok(ManagedObjects::new()),
            Err(e) => Err(BtError::from(e)),
        }
    }

    pub(crate) fn query_bluealsa(
        &self,
        devices: &[BtDevice],
    ) -> Result<Box<[BlueAlsaPcm]>, BtError> {
        let objects = self.fetch_bluealsa_objects()?;
        let mut pcms = Self::parse_bluealsa_objects(&objects, devices);
        for pcm in &mut pcms {
            pcm.available_codecs = Self::query_pcm_codecs(&self.conn(), &pcm.pcm_path)?;
        }
        Ok(pcms)
    }

    fn select_active_pcm(pcms: &[BlueAlsaPcm]) -> Result<&BlueAlsaPcm, BtError> {
        crate::transport::select_active_pcm(pcms).ok_or_else(|| {
            BtError::no_active_player("no active BlueALSA PCM stream found on D-Bus")
        })
    }

    /// Selects the target `BlueALSA` PCM stream (preferring an actively `running`
    /// stream, otherwise the first available stream) and encodes its new `Volume`
    /// (`ay`) byte array for `percent` (`0..=100`, unmuted).
    ///
    /// # Errors
    /// Returns [`BtError`] if no `BlueALSA` PCM stream is present.
    pub fn resolve_pcm_volume_target(
        objects: &ManagedObjects,
        percent: u8,
    ) -> Result<(Box<str>, Vec<u8>), BtError> {
        let pcms = Self::parse_bluealsa_objects(objects, &[]);
        let pcm = Self::select_active_pcm(&pcms)?;
        let bytes = encode_volume_bytes(pcm.channels, percent, false);
        Ok((pcm.pcm_path.clone(), bytes))
    }

    /// Selects the target `BlueALSA` PCM stream and computes its updated `Volume`
    /// (`ay`) byte array with the mute bit (`0x80`) set or cleared while preserving
    /// per-channel 7-bit levels.
    ///
    /// # Errors
    /// Returns [`BtError`] if no `BlueALSA` PCM stream is present.
    pub fn resolve_pcm_mute_target(
        objects: &ManagedObjects,
        muted: bool,
    ) -> Result<(Box<str>, Vec<u8>), BtError> {
        let pcms = Self::parse_bluealsa_objects(objects, &[]);
        let pcm = Self::select_active_pcm(&pcms)?;
        let existing = objects
            .iter()
            .find(|(path, _)| path.as_str() == pcm.pcm_path.as_ref())
            .and_then(|(_, ifaces)| find_iface(ifaces, "org.bluealsa.PCM1"))
            .and_then(|props| get_byte_array(props, "Volume"))
            .unwrap_or_default();
        let bytes = apply_mute_to_volume_bytes(&existing, pcm.channels, muted);
        Ok((pcm.pcm_path.clone(), bytes))
    }

    fn set_pcm_volume_bytes(&self, pcm_path: &str, bytes: &[u8]) -> Result<(), BtError> {
        let proxy = zbus::blocking::Proxy::new(
            &self.conn(),
            "org.bluealsa",
            pcm_path,
            "org.bluealsa.PCM1",
        )?;
        proxy.set_property("Volume", bytes)?;
        Ok(())
    }

    pub(crate) fn set_volume(&self, percent: u8) -> Result<(), BtError> {
        let objects = self.fetch_bluealsa_objects()?;
        let (pcm_path, bytes) = Self::resolve_pcm_volume_target(&objects, percent)?;
        self.set_pcm_volume_bytes(&pcm_path, &bytes)
    }

    pub(crate) fn set_mute(&self, muted: bool) -> Result<(), BtError> {
        let objects = self.fetch_bluealsa_objects()?;
        let (pcm_path, bytes) = Self::resolve_pcm_mute_target(&objects, muted)?;
        self.set_pcm_volume_bytes(&pcm_path, &bytes)
    }

    /// Maps a `GetCodecs` D-Bus method error, treating `UnknownMethod` / `UnknownObject`
    /// (older `BlueALSA` versions) as an empty codec list.
    ///
    /// # Errors
    /// Returns [`BtError`] for non-`UnknownMethod` D-Bus transport or permission faults.
    pub fn map_get_codecs_error(
        pcm_path: &str,
        err: zbus::Error,
    ) -> Result<Box<[Box<str>]>, BtError> {
        if is_unknown_method_error(&err) {
            Ok(Box::default())
        } else {
            Err(BtError::dbus_with_source(
                format!("GetCodecs call for {pcm_path}"),
                err,
            ))
        }
    }

    /// Extracts and sorts codec names from a deserialized `GetCodecs` D-Bus reply map.
    ///
    /// # Errors
    /// Returns [`BtError`] if reply body deserialization failed.
    pub fn decode_pcm_codecs_map(
        pcm_path: &str,
        codecs: Result<HashMap<String, HashMap<String, OwnedValue>>, zbus::Error>,
    ) -> Result<Box<[Box<str>]>, BtError> {
        let codecs = codecs
            .map_err(|e| BtError::dbus_with_source(format!("GetCodecs reply for {pcm_path}"), e))?;
        let mut names: Vec<Box<str>> = codecs.into_keys().map(Into::into).collect();
        names.sort();
        Ok(names.into_boxed_slice())
    }

    /// Queries available codecs for a PCM via the `GetCodecs()` D-Bus method.
    ///
    /// Returns an empty list only when the daemon does not implement `GetCodecs`
    /// (older `BlueALSA` versions), mirroring `bluealsactl` which prints
    /// `[ Unknown ]` when the list is empty. Transport-level faults (connection
    /// loss, permission errors, malformed replies) propagate as [`BtError`].
    fn query_pcm_codecs(conn: &Connection, pcm_path: &str) -> Result<Box<[Box<str>]>, BtError> {
        let proxy = zbus::blocking::Proxy::new(conn, "org.bluealsa", pcm_path, "org.bluealsa.PCM1")
            .map_err(|e| BtError::dbus_with_source(format!("codec proxy for {pcm_path}"), e))?;
        match proxy.call_method("GetCodecs", &()) {
            Ok(reply) => Self::decode_pcm_codecs_map(pcm_path, reply.body().deserialize()),
            Err(e) => Self::map_get_codecs_error(pcm_path, e),
        }
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
                let (volume, muted, volume_percent) = get_byte_array(props, "Volume")
                    .map_or((None, false, None), |bytes| {
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
                    volume_percent,
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
        let proxy = match BlueZAdapter1ProxyBlocking::builder(&self.conn())
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
    change_flag: Arc<ChangeFlag>,
}

#[derive(Debug)]
struct MockState {
    services: BluetoothServices,
    adapter: Option<BtAdapter>,
    devices: Vec<BtDevice>,
    pcms: Vec<BlueAlsaPcm>,
    playback: Option<BtPlaybackState>,
    fault: Option<Box<str>>,
    cache_dir: std::path::PathBuf,
}

impl BluetoothDeviceMock {
    fn new() -> Self {
        let change_flag = Arc::new(ChangeFlag::default());
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
                volume_percent: Some(79),
                muted: false,
                delay: Some("200.0 ms".into()),
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
                obex_port: Some(4097),
                image_handle: Some("1000001".into()),
                cover_art_path: None,
            }),
            fault: None,
            cache_dir: std::path::PathBuf::from(COVER_ART_CACHE_DIR),
        };

        Self {
            state: Arc::new(RwLock::new(state)),
            change_flag,
        }
    }

    /// Sets the simulated Bluetooth system services state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_services(&self, services: BluetoothServices) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.services = services;

        self.change_flag.notify();
    }

    /// Sets the simulated local adapter properties.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_adapter(&self, adapter: Option<BtAdapter>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.adapter = adapter;

        self.change_flag.notify();
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

        self.change_flag.notify();
    }

    /// Clears all simulated remote devices.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn clear_devices(&self) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.devices.clear();

        self.change_flag.notify();
    }

    /// Adds a simulated `BlueALSA` audio PCM stream.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn add_pcm(&self, pcm: BlueAlsaPcm) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.pcms.push(pcm);

        self.change_flag.notify();
    }

    /// Clears all simulated `BlueALSA` audio PCM streams.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn clear_pcms(&self) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.pcms.clear();

        self.change_flag.notify();
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.fault = fault;

        self.change_flag.notify();
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

        self.change_flag.notify();
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

        self.change_flag.notify();
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

        self.change_flag.notify();
    }

    /// Sets the simulated media player playback state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_playback(&self, playback: Option<BtPlaybackState>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.playback = playback;

        self.change_flag.notify();
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

        self.change_flag.notify();
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

        self.change_flag.notify();
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

        self.change_flag.notify();
    }

    /// Sets the simulated album cover art path directly.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_player_cover_art(&self, cover_art_path: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut p) = state.playback {
            p.cover_art_path = cover_art_path;
        }

        self.change_flag.notify();
    }

    /// Sets the simulated image handle and OBEX BIP PSM port.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_player_image_handle(&self, handle: Option<Box<str>>, port: Option<u16>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref mut p) = state.playback {
            p.image_handle = handle;
            p.obex_port = port;
        }

        self.change_flag.notify();
    }

    /// Overrides the cover art cache directory used for simulated cache lookups.
    ///
    /// Mirrors [`BluetoothDevice::with_cache_dir`] so hermetic tests can point the
    /// mock at a temporary directory instead of the production cache directory
    /// (which requires root privileges to create).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_cover_art_cache_dir(&self, dir: &std::path::Path) {
        let mut state = self.state.write().expect("mock lock poisoned");
        dir.clone_into(&mut state.cache_dir);

        self.change_flag.notify();
    }

    /// Blocks until a mock property changes or `timeout` elapses.
    ///
    /// Returns `true` when a change was signaled, `false` on keep-alive timeout.
    #[must_use]
    pub fn wait_for_property_change(&self, timeout: Duration) -> bool {
        self.change_flag.wait(timeout)
    }

    fn player_play(&self) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            p.status = "playing".into();
            self.change_flag.notify();
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
            self.change_flag.notify();
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn playback_state(&self) -> Result<Option<BtPlaybackState>, BtError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        let mut pb = state.playback.clone();
        if let Some(ref mut p) = pb
            && p.cover_art_path.is_none()
            && let (Some(addr), Some(handle)) = (p.device_address, &p.image_handle)
        {
            p.cover_art_path = cover_art_cached_path_in(&state.cache_dir, addr, handle);
        }
        Ok(pb)
    }

    fn player_toggle(&self) -> Result<bool, BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        if let Some(ref mut p) = state.playback {
            let next_playing = !p.is_playing();
            if next_playing {
                p.status = "playing".into();
            } else {
                p.status = "paused".into();
            }
            self.change_flag.notify();
            Ok(next_playing)
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
            self.change_flag.notify();
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
            self.change_flag.notify();
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
            self.change_flag.notify();
            Ok(())
        } else {
            Err(BtError::no_active_player("no active mock player"))
        }
    }

    fn apply_mock_pcm_volume(pcm: &mut BlueAlsaPcm, percent: u8, muted: bool) {
        let clamped = percent.min(100);
        let bytes = encode_volume_bytes(pcm.channels, clamped, muted);
        let channel_names = [Box::<str>::from("L"), Box::<str>::from("R")];
        let (vol_str, is_muted, vol_pct) = format_volume_bytes(&bytes, Some(&channel_names));
        pcm.volume = vol_str;
        pcm.muted = is_muted;
        pcm.volume_percent = vol_pct;
    }

    fn active_mock_pcm_mut(pcms: &mut [BlueAlsaPcm]) -> Option<&mut BlueAlsaPcm> {
        let idx = pcms.iter().position(|p| p.running).unwrap_or(0);
        pcms.get_mut(idx)
    }

    /// Sets the simulated volume percentage (`0..=100`) and mute state on the active mock PCM stream.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_pcm_volume_state(&self, percent: u8, muted: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(pcm) = Self::active_mock_pcm_mut(&mut state.pcms) {
            Self::apply_mock_pcm_volume(pcm, percent, muted);
        }
        self.change_flag.notify();
    }

    fn set_volume(&self, percent: u8) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        let pcm = Self::active_mock_pcm_mut(&mut state.pcms)
            .ok_or_else(|| BtError::no_active_player("no active mock PCM"))?;
        Self::apply_mock_pcm_volume(pcm, percent, false);
        self.change_flag.notify();
        Ok(())
    }

    fn set_mute(&self, muted: bool) -> Result<(), BtError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        let pcm = Self::active_mock_pcm_mut(&mut state.pcms)
            .ok_or_else(|| BtError::no_active_player("no active mock PCM"))?;
        let pct = pcm.volume_percent.unwrap_or(0);
        Self::apply_mock_pcm_volume(pcm, pct, muted);
        self.change_flag.notify();
        Ok(())
    }

    fn link_snapshot(&self) -> Result<LinkSnapshot, BtError> {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ref fault) = state.fault {
            return Err(BtError::mock_fault(fault.clone()));
        }
        Ok(LinkSnapshot {
            adapter: state.adapter.clone(),
            devices: state.devices.clone().into_boxed_slice(),
            pcms: state.pcms.clone().into_boxed_slice(),
        })
    }

    fn get_adapter_mut<'a>(
        state: &'a mut MockState,
        target_adapter: Option<&str>,
    ) -> Result<&'a mut BtAdapter, BtError> {
        let adapter = state.adapter.as_mut().ok_or_else(|| {
            BtError::adapter_not_found(target_adapter.unwrap_or(DEFAULT_BT_ADAPTER))
        })?;
        if let Some(target) = target_adapter
            && adapter.id.as_ref() != target
        {
            return Err(BtError::adapter_not_found(target));
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
        let mut playback = state.playback.clone();
        if let Some(ref mut p) = playback
            && p.cover_art_path.is_none()
            && let (Some(addr), Some(handle)) = (p.device_address, &p.image_handle)
        {
            p.cover_art_path = cover_art_cached_path_in(&state.cache_dir, addr, handle);
        }
        Ok(BluetoothStatus {
            services: state.services.clone(),
            adapter: state.adapter.clone(),
            devices: state.devices.clone().into_boxed_slice(),
            pcms: state.pcms.clone().into_boxed_slice(),
            hardware: BluetoothHardwareInfo::rpi3(),
            playback,
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

    /// Blocks until a Bluetooth property changes or `timeout` elapses.
    ///
    /// Returns `true` when a change was signaled, `false` on keep-alive timeout.
    #[must_use]
    pub fn wait_for_property_change(&self, timeout: Duration) -> bool {
        match &*self.backend {
            Backend::Dbus(dbus) => dbus.wait_for_change(timeout),
            Backend::Mock(mock) => mock.wait_for_property_change(timeout),
        }
    }

    /// Overrides the directory holding persistent per-device BIP resolution caps.
    ///
    /// Used by hermetic tests to avoid polluting the production cache directory.
    #[must_use]
    pub fn with_device_caps_dir(mut self, dir: &std::path::Path) -> Self {
        if let Some(Backend::Dbus(dbus)) = Arc::get_mut(&mut self.backend) {
            dir.clone_into(&mut dbus.device_caps_dir);
        }
        self
    }

    /// Overrides the directory holding downloaded cover art images.
    ///
    /// Used by hermetic tests to avoid touching the production cache directory
    /// (which requires root privileges to create).
    #[must_use]
    pub fn with_cache_dir(mut self, dir: &std::path::Path) -> Self {
        if let Some(Backend::Dbus(dbus)) = Arc::get_mut(&mut self.backend) {
            dir.clone_into(&mut dbus.cache_dir);
        }
        self
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
                let (adapter, devices, mut playback) = dbus.query_bluez(target_adapter)?;
                // bt-info and other status consumers read cover_art_path from
                // this snapshot, so resolve it exactly like playback_state().
                dbus.resolve_cover_art(&mut playback);
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
        speaker_common::runtime::run_blocking("bluetooth status read", move || {
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
        Ok(self.link_snapshot()?.pcms)
    }

    /// Returns the adapter, remote devices, and audio streams without service or hardware probes.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or a mock fault is injected.
    pub fn link_snapshot(&self) -> Result<LinkSnapshot, BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.link_snapshot(),
            Backend::Dbus(dbus) => {
                let (adapter, devices, _) = dbus.query_bluez(None)?;
                let pcms = dbus.query_bluealsa(&devices)?;
                Ok(LinkSnapshot {
                    adapter,
                    devices,
                    pcms,
                })
            }
        }
    }

    /// Returns the active media player playback state, if available.
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn playback_state(&self) -> Result<Option<BtPlaybackState>, BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.playback_state(),
            Backend::Dbus(dbus) => {
                let mut playback = dbus.playback_state()?;
                dbus.resolve_cover_art(&mut playback);
                Ok(playback)
            }
        }
    }

    /// Blocking playback query with synchronous cover resolution.
    ///
    /// Used by diagnostic queries and integration tests that exercise the
    /// OBEX pipeline directly; the monitor loop uses [`Self::playback_state`].
    ///
    /// # Errors
    /// Returns [`BtError`] if querying fails.
    pub fn playback_state_with_cover(&self) -> Result<Option<BtPlaybackState>, BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.playback_state(),
            Backend::Dbus(dbus) => {
                let mut playback = dbus.playback_state()?;
                dbus.resolve_cover_art(&mut playback);
                Ok(playback)
            }
        }
    }

    /// Returns the active media player playback state asynchronously.
    ///
    /// # Errors
    /// Returns [`BtError`] if reading fails or task is cancelled.
    pub async fn playback_state_async(&self) -> Result<Option<BtPlaybackState>, BtError> {
        let dev = self.clone();
        speaker_common::runtime::run_blocking("bluetooth playback read", move || {
            dev.playback_state()
        })
        .await
    }

    /// Sends a Play command to the active Bluetooth media player.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_play(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_play(),
            Backend::Dbus(dbus) => dbus.call_player_method("Play"),
        }
    }

    /// Sends a Pause command to the active Bluetooth media player.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_pause(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_pause(),
            Backend::Dbus(dbus) => dbus.call_player_method("Pause"),
        }
    }

    /// Toggles between Play and Pause on the active Bluetooth media player.
    ///
    /// Returns `true` if the player transitioned to playing, or `false` if paused.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_toggle(&self) -> Result<bool, BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_toggle(),
            Backend::Dbus(dbus) => dbus.player_toggle(),
        }
    }

    /// Sends a Next command to advance to the next track.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_next(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_next(),
            Backend::Dbus(dbus) => dbus.call_player_method("Next"),
        }
    }

    /// Sends a Previous command to restart or return to the previous track.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails or no player is available.
    pub fn player_previous(&self) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_previous(),
            Backend::Dbus(dbus) => dbus.call_player_method("Previous"),
        }
    }

    /// Requests seeking to a specific playback position in milliseconds.
    ///
    /// # Note
    /// `BlueZ`'s `MediaPlayer1` only exposes a *relative* `Seek(i64 offset)` method
    /// (offset in microseconds; positive moves forward, negative moves backward),
    /// so the absolute target is translated using the currently reported position
    /// read from a fresh managed-objects snapshot. In mock / preview mode, this
    /// simulates timeline scrubbing.
    ///
    /// # Errors
    /// Returns [`BtError`] if the D-Bus seek fails, no active player exists, or
    /// (in mock mode) an injected fault fires.
    pub fn player_seek(&self, position_ms: u32) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.player_seek(position_ms),
            Backend::Dbus(dbus) => dbus.player_seek(position_ms),
        }
    }

    /// Sets the output volume percentage (`0..=100`) and unmutes the active `BlueALSA` PCM stream.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails, no active PCM stream exists,
    /// or (in mock mode) an injected fault fires.
    pub fn set_volume(&self, percent: u8) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.set_volume(percent),
            Backend::Dbus(dbus) => dbus.set_volume(percent),
        }
    }

    /// Sets or clears the mute state on the active `BlueALSA` PCM stream while preserving volume level.
    ///
    /// # Errors
    /// Returns [`BtError`] if D-Bus communication fails, no active PCM stream exists,
    /// or (in mock mode) an injected fault fires.
    pub fn set_mute(&self, muted: bool) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.set_mute(muted),
            Backend::Dbus(dbus) => dbus.set_mute(muted),
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
    use super::{BlueAlsaPcm, BtDevice, BtError};
    pub use super::{
        DbusClient, ManagedObjects, is_no_such_unit_error, is_service_absent_error,
        is_service_absent_fdo_error, is_unknown_method_error, parse_hciconfig_version,
    };
    pub use crate::transport::format_volume_bytes;
    use speaker_common::runtime::FromJoinError;

    /// Creates a `BtError::dbus` error for testing.
    /// Calls [`DbusClient::query_bluealsa`] (integration-test seam for the
    /// `pub(crate)` method).
    ///
    /// # Errors
    /// Returns the underlying [`BtError`] from the D-Bus query.
    pub fn query_bluealsa(
        client: &DbusClient,
        devices: &[BtDevice],
    ) -> Result<Box<[BlueAlsaPcm]>, BtError> {
        client.query_bluealsa(devices)
    }

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

/// Resolves `cover_art_path` for the active playback snapshot.
///
/// Fast path: the deterministic disk cache for `(MAC, handle)`. Slow path:
/// the `org.speaker.CoverArt` D-Bus service published by the `bt-manager`
/// provider — the provider owns the single BIP session to the phone and core
/// must never open its own OBEX connections (the AOSP responder serves one
/// client at a time). Exposed `pub` for the private-bus integration tests.
pub fn resolve_cover_art_with(
    conn: &zbus::blocking::Connection,
    cache_dir: &std::path::Path,
    playback: &mut Option<BtPlaybackState>,
) {
    let Some(pb) = playback else { return };
    let Some(addr) = pb.device_address else {
        return;
    };
    let Some(ref handle) = pb.image_handle else {
        tracing::debug!(
            title = pb.title.as_ref(),
            "cover art unavailable: no ImgHandle in Track"
        );
        return;
    };
    // Consult the provider's published snapshot FIRST: its path is the
    // provider-blessed file for the currently published (device, handle).
    // A bare disk-cache hit is NOT sufficient on its own — BIP handles are
    // only unique within one OBEX session, so after a phone-side BIP
    // re-registration a new track's handle can collide with a file from the
    // previous session; the provider re-fetches in that case and the
    // snapshot mismatch below keeps the consumer from serving the stale
    // bytes in the interim (self-correcting on the next poll once the
    // provider republishes).
    match crate::cover_service::query_snapshot_state(conn) {
        CoverSnapshot::Published(snapshot) => {
            if snapshot.device.as_ref() == addr.to_string()
                && snapshot.handle.as_ref() == handle.as_ref()
                && std::path::Path::new(snapshot.path.as_ref()).is_file()
            {
                pb.cover_art_path = Some(snapshot.path);
            } else {
                tracing::debug!(
                    published_handle = snapshot.handle.as_ref(),
                    expected_handle = handle.as_ref(),
                    "cover-art service has not published this track's image yet"
                );
            }
        }
        // The provider actively cleared (the track lost its artwork): never
        // substitute the previous track's cache file.
        CoverSnapshot::Cleared => {
            tracing::debug!("cover-art service reported a cleared state");
        }
        // The truth is unknown (transport fault, malformed reply): keep the
        // last known state instead of serving a possibly stale cache file.
        CoverSnapshot::Fault(fault) => {
            tracing::warn!(%fault, "cover-art snapshot query failed; keeping last known state");
        }
        // Provider absent (bt-manager not running): the disk cache is the
        // only information available; serve it best-effort.
        CoverSnapshot::Absent => {
            if let Some(path) = cover_art_cached_path_in(cache_dir, addr, handle) {
                pb.cover_art_path = Some(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `MethodError` the way zbus surfaces remote method failures.
    fn method_error(name: &str) -> zbus::Error {
        let message = zbus::message::Message::method_call("/org/example", "Call")
            .expect("valid method call")
            .build(&())
            .expect("buildable message");
        zbus::Error::MethodError(name.try_into().expect("valid error name"), None, message)
    }

    #[test]
    fn is_unknown_method_error_matches_error_names() {
        assert!(is_unknown_method_error(&method_error(
            "org.freedesktop.DBus.Error.UnknownMethod"
        )));
        assert!(is_unknown_method_error(&method_error(
            "org.freedesktop.DBus.Error.UnknownObject"
        )));
    }

    #[test]
    fn is_unknown_method_error_rejects_other_names() {
        assert!(!is_unknown_method_error(&method_error(
            "org.freedesktop.DBus.Error.AccessDenied"
        )));
        assert!(!is_unknown_method_error(&method_error(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        )));
    }

    #[test]
    fn is_unknown_method_error_matches_fdo_typed_variants() {
        assert!(is_unknown_method_error(&zbus::Error::FDO(Box::new(
            zbus::fdo::Error::UnknownMethod("method".into())
        ))));
        assert!(is_unknown_method_error(&zbus::Error::FDO(Box::new(
            zbus::fdo::Error::UnknownObject("object".into())
        ))));
        assert!(!is_unknown_method_error(&zbus::Error::FDO(Box::new(
            zbus::fdo::Error::AccessDenied("denied".into())
        ))));
    }

    #[test]
    fn run_probe_command_captures_stdout() {
        let out = run_probe_command("sh", &["-c", "printf hello"]).expect("probe must succeed");
        assert_eq!(out, "hello");
    }

    #[test]
    fn run_probe_command_rejects_non_success_status() {
        let error = run_probe_command("sh", &["-c", "exit 3"])
            .expect_err("non-zero exit must surface as an error");
        assert!(error.to_string().contains("non-success status"));
    }

    #[test]
    fn run_probe_command_rejects_missing_binary() {
        let error = run_probe_command("/nonexistent/speaker-probe", &[])
            .expect_err("missing binary must surface as an error");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn run_probe_command_kills_on_timeout() {
        let error = run_probe_command("sh", &["-c", "sleep 5"])
            .expect_err("an unresponsive probe must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn is_unknown_method_error_rejects_transport_faults() {
        assert!(!is_unknown_method_error(&zbus::Error::InputOutput(
            std::sync::Arc::new(std::io::Error::other("broken pipe"))
        )));
    }
}
