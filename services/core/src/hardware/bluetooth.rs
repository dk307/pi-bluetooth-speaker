//! Bluetooth and `BlueALSA` hardware subsystem abstraction.
//!
//! Provides strongly-typed, mockable interfaces for the Broadcom BCM43438 Bluetooth
//! controller and the `BlueALSA` audio streaming pipeline on the Raspberry Pi 3 Model B.
//! Supports adapter status inspection, remote device discovery, connection state,
//! and real-time `BlueALSA` PCM audio stream telemetry.
//!
//! Conforms to Microsoft Pragmatic Rust Guidelines:
//! - Mockable external I/O via inherent [`BluetoothDevice::new_mocked`]
//! - Strongly typed invariants via [`BtAddress`] and [`ServiceState`]
//! - Canonical error reporting via [`BtError`]
//! - Non-blocking async queries via [`crate::runtime::run_blocking`]

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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

    /// Soft user-facing parser validating Bluetooth address format without backtrace capture.
    ///
    /// Accepts 6 hexadecimal octets delimited by colons (`:`) or hyphens (`-`).
    /// Rejects sign characters (`+`, `-`) as prefix or infix to comply with repository rules.
    ///
    /// # Errors
    /// Returns a descriptive user-facing error message if `s` is signed, has invalid octet count,
    /// or contains non-hexadecimal characters.
    pub fn parse_str(s: &str) -> Result<Self, String> {
        let clean = s.trim();
        if clean.starts_with('+') {
            return Err(format!(
                "invalid Bluetooth address '{s}': sign characters are not allowed"
            ));
        }

        let delimiter = if clean.contains(':') {
            ':'
        } else if clean.contains('-') {
            '-'
        } else {
            return Err(format!(
                "invalid Bluetooth address '{s}': expected 6 colon- or hyphen-separated hex octets"
            ));
        };

        let parts: Vec<&str> = clean.split(delimiter).collect();
        if parts.len() != BT_ADDRESS_OCTETS {
            return Err(format!(
                "invalid Bluetooth address '{s}': expected 6 hex octets, found {}",
                parts.len()
            ));
        }

        let mut octets = [0u8; BT_ADDRESS_OCTETS];
        for (i, part) in parts.iter().enumerate() {
            if part.starts_with(['+', '-']) {
                return Err(format!(
                    "invalid Bluetooth address '{s}': sign characters are not allowed"
                ));
            }
            if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "invalid Bluetooth address octet '{part}' in '{s}': expected 2 hex digits"
                ));
            }
            octets[i] = u8::from_str_radix(part, 16)
                .map_err(|e| format!("invalid hex octet '{part}' in '{s}': {e}"))?;
        }

        Ok(Self(octets))
    }

    /// Programmatic constructor parsing a Bluetooth address string into a [`BtAddress`].
    ///
    /// # Errors
    /// Returns [`BtError`] if string validation fails.
    pub fn parse(s: &str) -> Result<Self, BtError> {
        Self::parse_str(s).map_err(|err| BtError::invalid_address(s, err))
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

/// Operational state of a systemd daemon service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServiceState {
    /// Service unit is running and healthy (`active`).
    Active,
    /// Service unit is stopped or inactive (`inactive`).
    Inactive,
    /// Service unit exited with an error (`failed`).
    Failed,
    /// Service unit is not installed on the host (`not-found`).
    NotFound,
}

impl ServiceState {
    /// Returns `true` if the service is currently running.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns a human-readable display string for this service state.
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
        write!(f, "{}", self.description())
    }
}

/// Status snapshot of Bluetooth-related systemd daemons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothServices {
    /// `BlueZ` daemon (`bluetooth.service`).
    pub bluetoothd: ServiceState,
    /// `BlueALSA` audio proxy daemon (`bluealsa.service`).
    pub bluealsa: ServiceState,
    /// `BlueALSA` ALSA playback forwarder (`bluealsa-aplay.service`).
    pub bluealsa_aplay: ServiceState,
    /// Bluetooth pairing agent (`bt-agent.service`).
    pub bt_agent: ServiceState,
}

impl BluetoothServices {
    /// Returns `true` if primary Bluetooth and `BlueALSA` services are running.
    #[must_use]
    pub fn is_operational(&self) -> bool {
        self.bluetoothd.is_active()
            && (self.bluealsa.is_active() || self.bluealsa_aplay.is_active())
    }

    /// Returns `true` if all Bluetooth appliance daemons are active.
    #[must_use]
    pub fn all_active(&self) -> bool {
        self.bluetoothd.is_active()
            && self.bluealsa.is_active()
            && self.bluealsa_aplay.is_active()
            && self.bt_agent.is_active()
    }
}

/// Local Bluetooth adapter (controller) properties.
#[expect(
    clippy::struct_excessive_bools,
    reason = "Bluetooth adapter capabilities map directly to BlueZ boolean flags"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtAdapter {
    /// Controller interface identifier (e.g. `hci0`).
    pub id: Box<str>,
    /// Controller hardware MAC address.
    pub address: BtAddress,
    /// Broadcast controller name.
    pub name: Box<str>,
    /// Local alias name.
    pub alias: Box<str>,
    /// Whether the radio transmitter is powered on.
    pub powered: bool,
    /// Whether the adapter is discoverable to nearby devices.
    pub discoverable: bool,
    /// Whether the adapter accepts pairing requests.
    pub pairable: bool,
    /// Whether background device discovery (scanning) is active.
    pub discovering: bool,
    /// Bluetooth device class bitfield (e.g. `0x0024_0404`).
    pub class: Option<u32>,
    /// Device modalias identifier (vendor/product ID string).
    pub modalias: Option<Box<str>>,
}

/// Remote paired or connected Bluetooth device descriptor.
#[expect(
    clippy::struct_excessive_bools,
    reason = "remote device state maps directly to BlueZ Device1 boolean flags"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtDevice {
    /// Device hardware MAC address.
    pub address: BtAddress,
    /// Broadcast device name.
    pub name: Box<str>,
    /// Local device alias.
    pub alias: Box<str>,
    /// Whether the device is actively connected.
    pub connected: bool,
    /// Whether the device is paired.
    pub paired: bool,
    /// Whether the device is marked trusted for auto-reconnection.
    pub trusted: bool,
    /// Whether the device is blocked.
    pub blocked: bool,
    /// Received signal strength indication (RSSI) in dBm, if available.
    pub rssi: Option<i16>,
    /// Remote battery percentage (`0..=100`), if reported via HFP or GATT.
    pub battery_percentage: Option<u8>,
    /// Device icon theme name (e.g. `phone`, `audio-card`).
    pub icon: Option<Box<str>>,
    /// Advertised service UUID strings.
    pub uuids: Box<[Box<str>]>,
}

impl BtDevice {
    /// Returns the friendly display name (preferring alias over broadcast name).
    #[must_use]
    pub fn display_name(&self) -> &str {
        if !self.alias.is_empty() {
            &self.alias
        } else if !self.name.is_empty() {
            &self.name
        } else {
            "Unknown Device"
        }
    }

    /// Returns `true` if this device advertises A2DP audio sink or source profiles.
    #[must_use]
    pub fn is_audio_capable(&self) -> bool {
        self.uuids.iter().any(|u| {
            let u_lower = u.to_ascii_lowercase();
            u_lower.contains("0000110b")
                || u_lower.contains("0000110a")
                || u_lower.contains("0000110e")
        })
    }
}

/// Active `BlueALSA` audio PCM transport stream descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueAlsaPcm {
    /// `BlueALSA` D-Bus PCM object path.
    pub pcm_path: Box<str>,
    /// Remote device MAC address associated with this PCM stream.
    pub device_address: BtAddress,
    /// Resolved human-readable name of the audio source device.
    pub resolved_name: Box<str>,
    /// Audio transport mode (e.g. `A2DP-sink`, `A2DP-source`, `SCO`).
    pub transport: Box<str>,
    /// Whether audio data is actively flowing through the transport pipeline.
    pub running: bool,
    /// Currently negotiated audio codec (e.g. `AAC`, `SBC`, `LDAC`, `LC3plus`).
    pub selected_codec: Box<str>,
    /// List of codecs supported and allowed for this stream.
    pub available_codecs: Box<[Box<str>]>,
    /// Negotiated sampling rate in Hertz (e.g. `44100`, `48000`, `96000`).
    pub sampling_rate: u32,
    /// Number of audio channels (e.g. 1 for mono, 2 for stereo).
    pub channels: u8,
    /// Sample format (e.g. `S16_LE`, `S24_LE`, `S32_LE`).
    pub format: Box<str>,
    /// Hardware/software volume level string (e.g. `L: 100 R: 100` or percentage).
    pub volume: Option<Box<str>>,
    /// Whether audio output is muted.
    pub muted: bool,
    /// Estimated audio pipeline latency or delay description.
    pub delay: Option<Box<str>>,
}

impl BlueAlsaPcm {
    /// Returns a human-readable description of current audio streaming state.
    #[must_use]
    pub const fn stream_state_description(&self) -> &'static str {
        if self.running {
            "STREAMING (Active Audio Flow)"
        } else {
            "IDLE / PAUSED (Stream Established)"
        }
    }
}

/// Physical Broadcom BCM43438 Bluetooth controller metadata for Raspberry Pi 3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothHardwareInfo {
    /// Integrated wireless/Bluetooth `SoC` model.
    pub chip: Box<str>,
    /// Internal host controller transport bus.
    pub bus: Box<str>,
    /// Operational UART communication baud rate.
    pub baud_rate: u32,
    /// Firmware patchfile path on Linux filesystem.
    pub firmware: Box<str>,
    /// Linux kernel driver module name.
    pub driver: Box<str>,
    /// Default kernel HCI device interface.
    pub default_controller: Box<str>,
}

impl BluetoothHardwareInfo {
    /// Returns the standard hardware specification for the Raspberry Pi 3 appliance.
    #[must_use]
    pub fn rpi3() -> Self {
        Self {
            chip: "Broadcom BCM43438 (Bluetooth 4.1 + BLE)".into(),
            bus: "UART (PL011 / /dev/ttyAMA0)".into(),
            baud_rate: BCM43438_UART_BAUD_RATE,
            firmware: BCM43438_FIRMWARE_PATH.into(),
            driver: "hci_uart (bcm43xx)".into(),
            default_controller: DEFAULT_BT_ADAPTER.into(),
        }
    }
}

/// Holistic snapshot of Bluetooth services, controller, devices, and audio streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothStatus {
    /// Systemd service status for Bluetooth daemons.
    pub services: BluetoothServices,
    /// Local adapter controller properties (if detected).
    pub adapter: Option<BtAdapter>,
    /// Known remote devices (connected or paired).
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
    CommandFailed {
        command: Box<str>,
        details: Box<str>,
    },
    ToolNotFound {
        tool: Box<str>,
    },
    AdapterNotFound {
        adapter: Box<str>,
    },
    InvalidAddress {
        raw: Box<str>,
        reason: Box<str>,
    },
    Io(io::Error),
    Cancelled {
        message: Box<str>,
    },
    MockFault {
        message: Box<str>,
    },
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

    pub(crate) fn command_failed(
        command: impl Into<Box<str>>,
        details: impl Into<Box<str>>,
    ) -> Self {
        Self::new(BtErrorKind::CommandFailed {
            command: command.into(),
            details: details.into(),
        })
    }

    pub(crate) fn tool_not_found(tool: impl Into<Box<str>>) -> Self {
        Self::new(BtErrorKind::ToolNotFound { tool: tool.into() })
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

    pub(crate) fn io(err: io::Error) -> Self {
        Self::new(BtErrorKind::Io(err))
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

    /// Returns `true` if this error was caused by a command execution failure.
    #[must_use]
    pub const fn is_command_failed(&self) -> bool {
        matches!(self.kind, BtErrorKind::CommandFailed { .. })
    }

    /// Returns `true` if an external system tool was missing.
    #[must_use]
    pub const fn is_tool_not_found(&self) -> bool {
        matches!(self.kind, BtErrorKind::ToolNotFound { .. })
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

    /// Returns `true` if this error represents an underlying I/O error.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        matches!(self.kind, BtErrorKind::Io(_))
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
            BtErrorKind::CommandFailed { command, details } => {
                write!(f, "command '{command}' failed: {details}")
            }
            BtErrorKind::ToolNotFound { tool } => {
                write!(f, "required tool '{tool}' not found on system")
            }
            BtErrorKind::AdapterNotFound { adapter } => {
                write!(f, "Bluetooth adapter '{adapter}' not detected")
            }
            BtErrorKind::InvalidAddress { raw, reason } => {
                write!(f, "invalid Bluetooth address '{raw}': {reason}")
            }
            BtErrorKind::Io(err) => write!(f, "I/O error: {err}"),
            BtErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
            BtErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
        }
    }
}

super::impl_canonical_error_display!(BtError);

impl std::error::Error for BtError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            BtErrorKind::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for BtError {
    fn from(err: io::Error) -> Self {
        Self::io(err)
    }
}

impl FromJoinError for BtError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

/// Abstract runner for executing system inspection commands.
pub trait CommandRunner: Send + Sync {
    /// Executes `program` with `args`, returning [`CommandOutput`] or an I/O error.
    ///
    /// # Errors
    /// Returns [`io::Error`] if spawning or collecting output fails.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}

/// Output captured from a system command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Standard output text stream.
    pub stdout: String,
    /// Standard error text stream.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
}

/// Default timeout for individual Bluetooth subprocess queries (5 seconds).
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(crate) struct OsCommandRunner {
    timeout: Duration,
}

impl Default for OsCommandRunner {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }
}

impl OsCommandRunner {
    #[cfg(test)]
    pub(crate) const fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl CommandRunner for OsCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let mut child = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait()? {
                Some(_) => {
                    let output = child.wait_with_output()?;
                    return Ok(CommandOutput {
                        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        exit_code: output.status.code().unwrap_or(-1),
                    });
                }
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("command '{program}' timed out after {:?}", self.timeout),
                    ));
                }
            }
        }
    }
}

/// Hardware backend inspector executing system CLI queries (`bluetoothctl`, `systemctl`, `bluealsa-cli`).
#[derive(Clone)]
pub(crate) struct SystemInspector {
    runner: Arc<dyn CommandRunner>,
}

impl SystemInspector {
    fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    fn check_service(&self, name: &str) -> ServiceState {
        match self.runner.run("systemctl", &["is-active", name]) {
            Ok(out) => {
                if out.exit_code == 0 && out.stdout == "active" {
                    ServiceState::Active
                } else if out.exit_code == 4
                    || out.stderr.contains("not-found")
                    || out.stdout.contains("could not be found")
                    || out.stdout.contains("not-found")
                {
                    ServiceState::NotFound
                } else if out.stdout == "failed" {
                    ServiceState::Failed
                } else {
                    ServiceState::Inactive
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => ServiceState::NotFound,
            Err(_) => ServiceState::Inactive,
        }
    }

    fn check_services(&self) -> BluetoothServices {
        BluetoothServices {
            bluetoothd: self.check_service("bluetooth"),
            bluealsa: self.check_service("bluealsa"),
            bluealsa_aplay: self.check_service("bluealsa-aplay"),
            bt_agent: self.check_service("bt-agent"),
        }
    }

    fn parse_adapter(&self, target_adapter: Option<&str>) -> Result<Option<BtAdapter>, BtError> {
        let args = if let Some(a) = target_adapter {
            vec!["show", a]
        } else {
            vec!["show"]
        };

        let out = match self.runner.run("bluetoothctl", &args) {
            Ok(o) => o,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(BtError::tool_not_found("bluetoothctl"));
            }
            Err(e) => return Err(BtError::io(e)),
        };

        if out.exit_code != 0 || out.stdout.is_empty() {
            if let Some(target) = target_adapter {
                return Err(BtError::adapter_not_found(target));
            }
            return Ok(None);
        }

        let parsed = Self::parse_adapter_output(&out.stdout, target_adapter);
        if parsed.is_none() {
            if let Some(target) = target_adapter {
                return Err(BtError::adapter_not_found(target));
            }
        }
        Ok(parsed)
    }

    pub(crate) fn parse_adapter_output(
        stdout: &str,
        target_adapter: Option<&str>,
    ) -> Option<BtAdapter> {
        let mut addr = None;
        let mut name = String::from("Unknown");
        let mut alias = String::from("Unknown");
        let mut powered = false;
        let mut discoverable = false;
        let mut pairable = false;
        let mut discovering = false;
        let mut class = None;
        let mut modalias = None;

        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Controller") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(parsed_addr) = BtAddress::parse_str(parts[1]) {
                        addr = Some(parsed_addr);
                    }
                }
            } else if let Some(val) = trimmed.strip_prefix("Name:") {
                name = val.trim().to_string();
            } else if let Some(val) = trimmed.strip_prefix("Alias:") {
                alias = val.trim().to_string();
            } else if let Some(val) = trimmed.strip_prefix("Powered:") {
                powered = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Discoverable:") {
                discoverable = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Pairable:") {
                pairable = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Discovering:") {
                discovering = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Class:") {
                let hex_str = val.trim().trim_start_matches("0x").trim_start_matches("0X");
                class = u32::from_str_radix(hex_str, 16).ok();
            } else if let Some(val) = trimmed.strip_prefix("Modalias:") {
                modalias = Some(val.trim().into());
            }
        }

        let address = addr?;

        let id = target_adapter.unwrap_or(DEFAULT_BT_ADAPTER).into();

        Some(BtAdapter {
            id,
            address,
            name: name.into_boxed_str(),
            alias: alias.into_boxed_str(),
            powered,
            discoverable,
            pairable,
            discovering,
            class,
            modalias,
        })
    }

    fn get_devices(&self) -> Result<Box<[BtDevice]>, BtError> {
        let connected_out = match self.runner.run("bluetoothctl", &["devices", "Connected"]) {
            Ok(o) => o,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(BtError::tool_not_found("bluetoothctl"));
            }
            Err(e) => return Err(BtError::io(e)),
        };

        let all_out = self.runner.run("bluetoothctl", &["devices"]).ok();

        let mut device_map: HashMap<BtAddress, (String, bool)> = HashMap::new();

        if all_out.as_ref().is_some_and(|o| o.exit_code == 0) {
            for line in all_out.as_ref().unwrap().stdout.lines() {
                if let Some((addr, name)) = Self::parse_device_line(line) {
                    device_map.insert(addr, (name, false));
                }
            }
        }

        if connected_out.exit_code == 0 {
            for line in connected_out.stdout.lines() {
                if let Some((addr, name)) = Self::parse_device_line(line) {
                    device_map.insert(addr, (name, true));
                }
            }
        }

        let mut devices = Vec::with_capacity(device_map.len());
        for (addr, (default_name, connected)) in device_map {
            let addr_str = addr.to_string();
            let info = self.runner.run("bluetoothctl", &["info", &addr_str]).ok();
            let dev = if let Some(info_out) = info.filter(|o| o.exit_code == 0) {
                Self::parse_device_info(&info_out.stdout, addr, &default_name, connected)
            } else {
                BtDevice {
                    address: addr,
                    name: default_name.clone().into_boxed_str(),
                    alias: default_name.into_boxed_str(),
                    connected,
                    paired: false,
                    trusted: false,
                    blocked: false,
                    rssi: None,
                    battery_percentage: None,
                    icon: None,
                    uuids: Box::default(),
                }
            };
            devices.push(dev);
        }

        devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then_with(|| a.address.cmp(&b.address))
        });
        Ok(devices.into_boxed_slice())
    }

    pub(crate) fn parse_device_line(line: &str) -> Option<(BtAddress, String)> {
        let trimmed = line.trim();
        let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
        if parts.len() >= 3 && parts[0] == "Device" {
            if let Ok(addr) = BtAddress::parse_str(parts[1]) {
                return Some((addr, parts[2].trim().to_string()));
            }
        }
        None
    }

    pub(crate) fn parse_device_info(
        stdout: &str,
        addr: BtAddress,
        fallback_name: &str,
        default_connected: bool,
    ) -> BtDevice {
        let mut name = fallback_name.to_string();
        let mut alias = fallback_name.to_string();
        let mut connected = default_connected;
        let mut paired = false;
        let mut trusted = false;
        let mut blocked = false;
        let mut rssi = None;
        let mut battery = None;
        let mut icon = None;
        let mut uuids = Vec::new();

        for line in stdout.lines() {
            let trimmed = line.trim();
            if let Some(val) = trimmed.strip_prefix("Name:") {
                name = val.trim().to_string();
            } else if let Some(val) = trimmed.strip_prefix("Alias:") {
                alias = val.trim().to_string();
            } else if let Some(val) = trimmed.strip_prefix("Connected:") {
                connected = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Paired:") {
                paired = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Trusted:") {
                trusted = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("Blocked:") {
                blocked = val.trim().eq_ignore_ascii_case("yes");
            } else if let Some(val) = trimmed.strip_prefix("RSSI:") {
                let clean_rssi = val.split_whitespace().next().unwrap_or("");
                rssi = clean_rssi.parse::<i16>().ok();
            } else if let Some(val) = trimmed.strip_prefix("Battery Percentage:") {
                let clean_bat = val.trim().trim_end_matches('%').trim();
                battery = clean_bat.parse::<u8>().ok();
            } else if let Some(val) = trimmed.strip_prefix("Icon:") {
                icon = Some(val.trim().into());
            } else if let Some(val) = trimmed.strip_prefix("UUID:") {
                uuids.push(val.trim().into());
            }
        }

        BtDevice {
            address: addr,
            name: name.into_boxed_str(),
            alias: alias.into_boxed_str(),
            connected,
            paired,
            trusted,
            blocked,
            rssi,
            battery_percentage: battery,
            icon,
            uuids: uuids.into_boxed_slice(),
        }
    }

    fn get_bluealsa_pcms(&self, devices: &[BtDevice]) -> Box<[BlueAlsaPcm]> {
        let (cli_tool, list_out) = match self.runner.run("bluealsa-cli", &["list-pcms"]) {
            Ok(o) if o.exit_code == 0 => ("bluealsa-cli", o),
            _ => match self.runner.run("bluealsactl", &["list-pcms"]) {
                Ok(o) if o.exit_code == 0 => ("bluealsactl", o),
                _ => return Box::default(),
            },
        };

        let mut name_map: HashMap<BtAddress, &str> = HashMap::new();
        for dev in devices {
            name_map.insert(dev.address, dev.display_name());
        }

        let pcm_paths: Vec<&str> = list_out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("/org/bluealsa/"))
            .collect();

        let mut pcms = Vec::with_capacity(pcm_paths.len());
        for path in pcm_paths {
            if let Ok(info_out) = self.runner.run(cli_tool, &["info", path]) {
                if info_out.exit_code == 0 {
                    if let Some(pcm) = Self::parse_pcm_info(&info_out.stdout, path, &name_map) {
                        pcms.push(pcm);
                    }
                }
            }
        }

        pcms.into_boxed_slice()
    }

    pub(crate) fn parse_pcm_info(
        stdout: &str,
        path: &str,
        name_map: &HashMap<BtAddress, &str>,
    ) -> Option<BlueAlsaPcm> {
        let mut props: HashMap<String, String> = HashMap::new();
        for line in stdout.lines() {
            if let Some((k, v)) = line.split_once(':') {
                props.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let mac_str = Self::extract_mac_from_pcm_path(path).or_else(|| {
            props
                .get("device")
                .and_then(|d| Self::extract_mac_from_pcm_path(d))
        })?;

        let device_address = BtAddress::parse_str(&mac_str).ok()?;
        let resolved_name = name_map
            .get(&device_address)
            .copied()
            .unwrap_or("Unknown Device")
            .into();

        let transport = props
            .get("transport")
            .cloned()
            .unwrap_or_else(|| "A2DP".into());
        let running = props
            .get("running")
            .is_some_and(|r| r.eq_ignore_ascii_case("true"));
        let selected_codec = props
            .get("selected codec")
            .or_else(|| props.get("codec"))
            .cloned()
            .unwrap_or_else(|| "Unknown".into());

        let available_codecs = props
            .get("available codecs")
            .filter(|s| s.trim() != "[ Unknown ]")
            .map(|s| {
                s.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|c| !c.is_empty())
                    .map(Into::into)
                    .collect::<Vec<Box<str>>>()
                    .into_boxed_slice()
            })
            .unwrap_or_default();

        let sampling_rate = props
            .get("sampling")
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(44_100);

        let channels = props
            .get("channels")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(2);

        let format = props
            .get("format")
            .cloned()
            .unwrap_or_else(|| "S16_LE".into());
        let volume = props.get("volume").cloned().map(Into::into);
        let muted = props
            .get("muted")
            .is_some_and(|m| m.eq_ignore_ascii_case("true"));
        let delay = props.get("delay").cloned().map(Into::into);

        Some(BlueAlsaPcm {
            pcm_path: path.into(),
            device_address,
            resolved_name,
            transport: transport.into_boxed_str(),
            running,
            selected_codec: selected_codec.into_boxed_str(),
            available_codecs,
            sampling_rate,
            channels,
            format: format.into_boxed_str(),
            volume,
            muted,
            delay,
        })
    }

    pub(crate) fn extract_mac_from_pcm_path(path: &str) -> Option<String> {
        let dev_prefix = "dev_";
        let idx = path.find(dev_prefix)?;
        let start = idx + dev_prefix.len();
        let candidate = &path[start..];
        let mac_part = candidate.split('/').next()?;
        (mac_part.len() == 17).then(|| mac_part.replace('_', ":"))
    }
}

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
    System(SystemInspector),
    Mock(BluetoothDeviceMock),
}

/// Raspberry Pi Bluetooth controller and `BlueALSA` audio subsystem client.
///
/// Thread-safe, cheaply cloneable (`Arc`-backed handle per `M-SERVICES-CLONE`),
/// providing non-blocking asynchronous inspection and control methods.
#[derive(Clone)]
pub struct BluetoothDevice {
    backend: Arc<Backend>,
}

impl fmt::Debug for BluetoothDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &*self.backend {
            Backend::Mock(mock) => f.debug_tuple("BluetoothDevice::Mock").field(mock).finish(),
            Backend::System(_) => f.debug_tuple("BluetoothDevice::System").finish(),
        }
    }
}

impl Default for BluetoothDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl BluetoothDevice {
    /// Connects to the host Linux Bluetooth subsystem using standard OS commands.
    #[must_use]
    pub fn new() -> Self {
        let inspector = SystemInspector::new(Arc::new(OsCommandRunner::default()));
        Self {
            backend: Arc::new(Backend::System(inspector)),
        }
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

    /// Creates a device instance backed by a custom command runner.
    ///
    /// Intended for testing system command parsing and error handling deterministically.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        let inspector = SystemInspector::new(runner);
        Self {
            backend: Arc::new(Backend::System(inspector)),
        }
    }

    /// Inspects the status for a specific adapter (or default if `None`) synchronously.
    ///
    /// # Errors
    /// Returns [`BtError`] if system commands fail, the adapter is not found, or a mock fault is injected.
    pub fn status_for_adapter(
        &self,
        target_adapter: Option<&str>,
    ) -> Result<BluetoothStatus, BtError> {
        match &*self.backend {
            Backend::Mock(mock) => mock.read_status_for_adapter(target_adapter),
            Backend::System(inspector) => {
                let services = inspector.check_services();
                let adapter = inspector.parse_adapter(target_adapter)?;
                let devices = inspector.get_devices()?;
                let pcms = inspector.get_bluealsa_pcms(&devices);
                Ok(BluetoothStatus {
                    services,
                    adapter,
                    devices,
                    pcms,
                    hardware: BluetoothHardwareInfo::rpi3(),
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
    /// Returns [`BtError`] if system commands fail or a mock fault is injected.
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

    /// Powers on or off the local Bluetooth radio.
    ///
    /// # Errors
    /// Returns [`BtError`] if the power command fails.
    pub fn set_powered(&self, powered: bool) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => {
                mock.set_powered(powered);
                Ok(())
            }
            Backend::System(inspector) => {
                let val = if powered { "on" } else { "off" };
                let out = inspector
                    .runner
                    .run("bluetoothctl", &["power", val])
                    .map_err(BtError::io)?;
                if out.exit_code != 0 {
                    return Err(BtError::command_failed("bluetoothctl power", out.stderr));
                }
                Ok(())
            }
        }
    }

    /// Enables or disables discoverable mode on the local Bluetooth radio.
    ///
    /// # Errors
    /// Returns [`BtError`] if the discoverable command fails.
    pub fn set_discoverable(&self, discoverable: bool) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => {
                mock.set_discoverable(discoverable);
                Ok(())
            }
            Backend::System(inspector) => {
                let val = if discoverable { "on" } else { "off" };
                let out = inspector
                    .runner
                    .run("bluetoothctl", &["discoverable", val])
                    .map_err(BtError::io)?;
                if out.exit_code != 0 {
                    return Err(BtError::command_failed(
                        "bluetoothctl discoverable",
                        out.stderr,
                    ));
                }
                Ok(())
            }
        }
    }

    /// Enables or disables pairable mode on the local Bluetooth radio.
    ///
    /// # Errors
    /// Returns [`BtError`] if the pairable command fails.
    pub fn set_pairable(&self, pairable: bool) -> Result<(), BtError> {
        match &*self.backend {
            Backend::Mock(mock) => {
                mock.set_pairable(pairable);
                Ok(())
            }
            Backend::System(inspector) => {
                let val = if pairable { "on" } else { "off" };
                let out = inspector
                    .runner
                    .run("bluetoothctl", &["pairable", val])
                    .map_err(BtError::io)?;
                if out.exit_code != 0 {
                    return Err(BtError::command_failed("bluetoothctl pairable", out.stderr));
                }
                Ok(())
            }
        }
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BluetoothDevice>();
    assert_send_sync::<BluetoothDeviceMock>();
    assert_send_sync::<BluetoothStatus>();
    assert_send_sync::<BtAddress>();
    assert_send_sync::<BtError>();
};

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRunner {
        responses: HashMap<(String, String), CommandOutput>,
    }

    impl TestRunner {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
            }
        }

        fn with_cmd(mut self, program: &str, args: &[&str], out: CommandOutput) -> Self {
            self.responses
                .insert((program.to_string(), args.join(" ")), out);
            self
        }
    }

    impl CommandRunner for TestRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
            let key = (program.to_string(), args.join(" "));
            if let Some(out) = self.responses.get(&key) {
                Ok(out.clone())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("mock command not registered: {program} {args:?}"),
                ))
            }
        }
    }

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
            uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".into()].into_boxed_slice(),
        };
        assert_eq!(dev1.display_name(), "Custom Alias");
        assert!(dev1.is_audio_capable());

        let dev2 = BtDevice {
            address: addr,
            name: "Original".into(),
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
        assert_eq!(dev2.display_name(), "Original");
        assert!(!dev2.is_audio_capable());
    }

    #[test]
    fn bluealsa_pcm_stream_state_description() {
        let addr = BtAddress::new([1, 2, 3, 4, 5, 6]);
        let mut pcm = BlueAlsaPcm {
            pcm_path: "/org/bluealsa/pcm0".into(),
            device_address: addr,
            resolved_name: "Phone".into(),
            transport: "A2DP-sink".into(),
            running: true,
            selected_codec: "AAC".into(),
            available_codecs: Box::default(),
            sampling_rate: 48_000,
            channels: 2,
            format: "S16_LE".into(),
            volume: None,
            muted: false,
            delay: None,
        };
        assert_eq!(
            pcm.stream_state_description(),
            "STREAMING (Active Audio Flow)"
        );
        pcm.running = false;
        assert_eq!(
            pcm.stream_state_description(),
            "IDLE / PAUSED (Stream Established)"
        );
    }

    #[test]
    fn hardware_info_rpi3_constants() {
        let hw = BluetoothHardwareInfo::rpi3();
        assert_eq!(&*hw.chip, "Broadcom BCM43438 (Bluetooth 4.1 + BLE)");
        assert_eq!(&*hw.bus, "UART (PL011 / /dev/ttyAMA0)");
        assert_eq!(hw.baud_rate, 921_600);
        assert_eq!(&*hw.default_controller, "hci0");
    }

    #[test]
    fn bt_error_predicates_and_conversions() {
        let err_cmd = BtError::command_failed("btctl", "failed to run");
        assert!(err_cmd.is_command_failed());
        assert_eq!(err_cmd.summary(), "command 'btctl' failed: failed to run");
        assert!(err_cmd.to_string().contains("btctl"));

        let err_tool = BtError::tool_not_found("bluealsa-cli");
        assert!(err_tool.is_tool_not_found());

        let err_adapter = BtError::adapter_not_found("hci0");
        assert!(err_adapter.is_adapter_not_found());

        let err_addr = BtError::invalid_address("invalid", "bad chars");
        assert!(err_addr.is_invalid_address());

        let err_io = BtError::io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert!(err_io.is_io());
        assert!(std::error::Error::source(&err_io).is_some());

        let err_canc = BtError::cancelled("shutdown");
        assert!(err_canc.is_cancelled());

        let from_canc = BtError::from_cancelled("aborted");
        assert!(from_canc.is_cancelled());

        let from_io: BtError = io::Error::other("test io").into();
        assert!(from_io.is_io());

        let err_mock = BtError::mock_fault("fault injected");
        assert!(err_mock.is_mock_fault());
        assert!(
            err_mock.backtrace().status() == std::backtrace::BacktraceStatus::Captured
                || err_mock.backtrace().status() == std::backtrace::BacktraceStatus::Disabled
                || err_mock.backtrace().status() == std::backtrace::BacktraceStatus::Unsupported
        );
    }

    #[test]
    fn mock_device_operations_and_fault_injection() {
        let (device, mock) = BluetoothDevice::new_mocked();
        let initial = device.status().unwrap();
        assert!(initial.is_operational());
        assert_eq!(initial.connected_count(), 1);
        assert!(initial.is_streaming());
        assert!(initial.active_pcm().is_some());
        assert_eq!(initial.connected_devices().len(), 1);
        assert_eq!(initial.paired_devices().len(), 1);

        mock.set_powered(false);
        let updated = device.status().unwrap();
        assert!(!updated.is_operational());

        mock.set_discoverable(true);
        mock.set_pairable(false);
        let adapter = device.adapter_info().unwrap().unwrap();
        assert!(adapter.discoverable);
        assert!(!adapter.pairable);

        mock.clear_devices();
        assert_eq!(device.connected_devices().unwrap().len(), 0);

        mock.clear_pcms();
        assert_eq!(device.audio_pcms().unwrap().len(), 0);

        mock.set_fault(Some("D-Bus connection lost".into()));
        let err = device.status().unwrap_err();
        assert!(err.is_mock_fault());
        assert!(err.summary().contains("D-Bus connection lost"));

        mock.set_fault(None);
        device.status().unwrap();

        device.set_powered(true).unwrap();
        device.set_discoverable(false).unwrap();
        device.set_pairable(true).unwrap();
    }

    #[tokio::test]
    async fn async_methods_read_from_mock_device() {
        let (device, mock) = BluetoothDevice::new_mocked();
        let status = device.status_async().await.unwrap();
        assert!(status.is_operational());

        mock.set_fault(Some("async fault".into()));
        device.status_async().await.unwrap_err();
    }

    #[test]
    fn system_inspector_parsers() {
        let show_out = r"
Controller DC:A6:32:11:22:33 (public)
	Name: Test-Speaker
	Alias: Custom-Alias
	Class: 0x00240404
	Powered: yes
	PowerState: on
	Discoverable: yes
	Pairable: yes
	Discovering: no
	Modalias: usb:v1D6Bp0246d0537
";
        let adapter = SystemInspector::parse_adapter_output(show_out, Some("hci0")).unwrap();
        assert_eq!(adapter.id.as_ref(), "hci0");
        assert_eq!(adapter.name.as_ref(), "Test-Speaker");
        assert_eq!(adapter.alias.as_ref(), "Custom-Alias");
        assert!(adapter.powered);
        assert!(adapter.discoverable);
        assert!(adapter.pairable);
        assert!(!adapter.discovering);
        assert_eq!(adapter.class, Some(0x0024_0404));
        assert_eq!(adapter.modalias.as_deref(), Some("usb:v1D6Bp0246d0537"));

        let empty = SystemInspector::parse_adapter_output("", None);
        assert!(empty.is_none());

        let line = "Device AA:BB:CC:DD:EE:FF My Phone";
        let (addr, name) = SystemInspector::parse_device_line(line).unwrap();
        assert_eq!(addr.to_string(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(name, "My Phone");
        assert!(SystemInspector::parse_device_line("Invalid").is_none());

        let dev_info = r"
Device AA:BB:CC:DD:EE:FF (public)
	Name: Remote Phone
	Alias: Favorite Phone
	Connected: yes
	Paired: yes
	Trusted: yes
	Blocked: no
	RSSI: -45 dBm
	Battery Percentage: 95%
	Icon: phone
	UUID: Audio Sink (0000110b-0000-1000-8000-00805f9b34fb)
";
        let dev = SystemInspector::parse_device_info(dev_info, addr, "Default", false);
        assert_eq!(dev.name.as_ref(), "Remote Phone");
        assert_eq!(dev.alias.as_ref(), "Favorite Phone");
        assert!(dev.connected);
        assert!(dev.paired);
        assert!(dev.trusted);
        assert!(!dev.blocked);
        assert_eq!(dev.rssi, Some(-45));
        assert_eq!(dev.battery_percentage, Some(95));
        assert_eq!(dev.icon.as_deref(), Some("phone"));
        assert_eq!(dev.uuids.len(), 1);

        let pcm_path = "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source";
        assert_eq!(
            SystemInspector::extract_mac_from_pcm_path(pcm_path),
            Some("AA:BB:CC:DD:EE:FF".to_string())
        );
        assert!(SystemInspector::extract_mac_from_pcm_path("/invalid/path").is_none());

        let pcm_info = r"
Device: /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF
Sequence: 0
Transport: A2DP-sink
Mode: source
Running: true
Format: S16_LE
Channels: 2
Sampling: 48000
Selected codec: AAC
Available codecs: SBC, AAC, LDAC
Delay: 2000
Volume: L: 100 R: 100
Muted: false
";
        let mut name_map = HashMap::new();
        name_map.insert(addr, "Favorite Phone");
        let pcm = SystemInspector::parse_pcm_info(pcm_info, pcm_path, &name_map).unwrap();
        assert_eq!(pcm.device_address, addr);
        assert_eq!(pcm.resolved_name.as_ref(), "Favorite Phone");
        assert_eq!(pcm.transport.as_ref(), "A2DP-sink");
        assert!(pcm.running);
        assert_eq!(pcm.selected_codec.as_ref(), "AAC");
        assert_eq!(pcm.sampling_rate, 48_000);
        assert_eq!(pcm.channels, 2);
        assert_eq!(pcm.format.as_ref(), "S16_LE");
        assert_eq!(pcm.volume.as_deref(), Some("L: 100 R: 100"));
        assert!(!pcm.muted);
        assert_eq!(pcm.delay.as_deref(), Some("2000"));
    }

    #[expect(
        clippy::too_many_lines,
        reason = "comprehensive command output mocking and verification table"
    )]
    #[test]
    fn system_inspector_with_runner_integration() {
        let runner = TestRunner::new()
            .with_cmd(
                "systemctl",
                &["is-active", "bluetooth"],
                CommandOutput {
                    stdout: "active".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "systemctl",
                &["is-active", "bluealsa"],
                CommandOutput {
                    stdout: "inactive".into(),
                    stderr: String::new(),
                    exit_code: 3,
                },
            )
            .with_cmd(
                "systemctl",
                &["is-active", "bluealsa-aplay"],
                CommandOutput {
                    stdout: "failed".into(),
                    stderr: String::new(),
                    exit_code: 1,
                },
            )
            .with_cmd(
                "systemctl",
                &["is-active", "bt-agent"],
                CommandOutput {
                    stdout: "could not be found".into(),
                    stderr: "Unit bt-agent.service not-found".into(),
                    exit_code: 4,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["show"],
                CommandOutput {
                    stdout: "Controller DC:A6:32:11:22:33 (public)\nName: Pi-BT\nPowered: yes\nPairable: yes\nDiscoverable: no\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["devices", "Connected"],
                CommandOutput {
                    stdout: "Device AA:BB:CC:DD:EE:FF Phone 1\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["devices"],
                CommandOutput {
                    stdout: "Device AA:BB:CC:DD:EE:FF Phone 1\nDevice 11:22:33:44:55:66 Headset\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["info", "AA:BB:CC:DD:EE:FF"],
                CommandOutput {
                    stdout: "Device AA:BB:CC:DD:EE:FF\nName: Phone 1\nConnected: yes\nPaired: yes\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["info", "11:22:33:44:55:66"],
                CommandOutput {
                    stdout: "Device 11:22:33:44:55:66\nName: Headset\nConnected: no\nPaired: yes\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluealsa-cli",
                &["list-pcms"],
                CommandOutput {
                    stdout: "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluealsa-cli",
                &["info", "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source"],
                CommandOutput {
                    stdout: "Transport: A2DP-sink\nRunning: true\nSelected codec: SBC\nSampling: 44100\nChannels: 2\nFormat: S16_LE\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            );

        let device = BluetoothDevice::with_runner(Arc::new(runner));
        let status = device.status().unwrap();
        assert_eq!(status.services.bluetoothd, ServiceState::Active);
        assert_eq!(status.services.bluealsa, ServiceState::Inactive);
        assert_eq!(status.services.bluealsa_aplay, ServiceState::Failed);
        assert_eq!(status.services.bt_agent, ServiceState::NotFound);

        let adapter = status.adapter.as_ref().unwrap();
        assert_eq!(adapter.name.as_ref(), "Pi-BT");
        assert!(adapter.powered);

        assert_eq!(status.devices.len(), 2);
        assert_eq!(status.connected_count(), 1);

        assert_eq!(status.pcms.len(), 1);
        assert!(status.is_streaming());
    }

    #[test]
    fn system_inspector_control_commands() {
        let runner = TestRunner::new()
            .with_cmd(
                "bluetoothctl",
                &["power", "on"],
                CommandOutput {
                    stdout: "Changing power on succeeded".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["discoverable", "off"],
                CommandOutput {
                    stdout: "Changing discoverable off succeeded".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["pairable", "on"],
                CommandOutput {
                    stdout: "Changing pairable on succeeded".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            );

        let device = BluetoothDevice::with_runner(Arc::new(runner));
        device.set_powered(true).unwrap();
        device.set_discoverable(false).unwrap();
        device.set_pairable(true).unwrap();

        let failing_runner = TestRunner::new().with_cmd(
            "bluetoothctl",
            &["power", "off"],
            CommandOutput {
                stdout: String::new(),
                stderr: "Failed to set power off: org.bluez.Error.Failed".into(),
                exit_code: 1,
            },
        );
        let failing_dev = BluetoothDevice::with_runner(Arc::new(failing_runner));
        let err = failing_dev.set_powered(false).unwrap_err();
        assert!(err.is_command_failed());

        let disc_fail_runner = TestRunner::new().with_cmd(
            "bluetoothctl",
            &["discoverable", "on"],
            CommandOutput {
                stdout: String::new(),
                stderr: "Failed: org.bluez.Error.Failed".into(),
                exit_code: 1,
            },
        );
        let disc_dev = BluetoothDevice::with_runner(Arc::new(disc_fail_runner));
        assert!(
            disc_dev
                .set_discoverable(true)
                .unwrap_err()
                .is_command_failed()
        );

        let pair_fail_runner = TestRunner::new().with_cmd(
            "bluetoothctl",
            &["pairable", "off"],
            CommandOutput {
                stdout: String::new(),
                stderr: "Failed: org.bluez.Error.Failed".into(),
                exit_code: 1,
            },
        );
        let pair_dev = BluetoothDevice::with_runner(Arc::new(pair_fail_runner));
        assert!(
            pair_dev
                .set_pairable(false)
                .unwrap_err()
                .is_command_failed()
        );
    }

    #[test]
    fn bluetooth_device_convenience_methods_and_debug() {
        let (device, mock) = BluetoothDevice::new_mocked();
        assert!(format!("{device:?}").contains("BluetoothDevice::Mock"));

        device.services_status().unwrap();
        device.adapter_info().unwrap();
        device.connected_devices().unwrap();
        device.audio_pcms().unwrap();

        let _ = BluetoothDevice::new();

        let sys_dev = BluetoothDevice::with_runner(Arc::new(TestRunner::new()));
        assert!(format!("{sys_dev:?}").contains("BluetoothDevice::System"));

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
    fn system_inspector_fallback_and_error_paths() {
        let runner = TestRunner::new()
            .with_cmd(
                "systemctl",
                &["is-active", "svc-deactivating"],
                CommandOutput {
                    stdout: "deactivating".into(),
                    stderr: String::new(),
                    exit_code: 3,
                },
            )
            .with_cmd(
                "systemctl",
                &["is-active", "svc-failed"],
                CommandOutput {
                    stdout: "failed".into(),
                    stderr: String::new(),
                    exit_code: 1,
                },
            )
            .with_cmd(
                "bluetoothctl",
                &["show", "hci99"],
                CommandOutput {
                    stdout: String::new(),
                    stderr: "No controller available".into(),
                    exit_code: 1,
                },
            );

        let inspector = SystemInspector::new(Arc::new(runner));
        assert_eq!(
            inspector.check_service("svc-deactivating"),
            ServiceState::Inactive
        );
        assert_eq!(inspector.check_service("svc-failed"), ServiceState::Failed);
        assert_eq!(
            inspector.check_service("svc-missing"),
            ServiceState::NotFound
        );

        let err = inspector.parse_adapter(Some("hci99")).unwrap_err();
        assert!(err.is_adapter_not_found());

        let dev_err = inspector.get_devices().unwrap_err();
        assert!(dev_err.is_tool_not_found());

        let pcms = inspector.get_bluealsa_pcms(&[]);
        assert!(pcms.is_empty());
    }

    #[test]
    fn bluealsactl_fallback_resolution() {
        let pcm_path = "/org/bluealsa/hci0/dev_22_33_44_55_66_77/a2dpsnk/source";
        let runner = TestRunner::new()
            .with_cmd(
                "bluealsactl",
                &["list-pcms"],
                CommandOutput {
                    stdout: format!("{pcm_path}\n"),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .with_cmd(
                "bluealsactl",
                &["info", pcm_path],
                CommandOutput {
                    stdout: "Transport: A2DP-sink\nRunning: false\nCodec: AAC\nSampling: 44100\nChannels: 2\nFormat: S16_LE\nAvailable codecs: SBC, AAC\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            );

        let inspector = SystemInspector::new(Arc::new(runner));
        let pcms = inspector.get_bluealsa_pcms(&[]);
        assert_eq!(pcms.len(), 1);
        assert_eq!(pcms[0].selected_codec.as_ref(), "AAC");
        assert_eq!(pcms[0].available_codecs.len(), 2);
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
    fn os_command_runner_execution_and_timeout() {
        let runner = OsCommandRunner::default();
        let out = runner.run("sh", &["-c", "echo hello_os_runner"]).unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "hello_os_runner");

        let fast_timeout_runner = OsCommandRunner::with_timeout(Duration::from_millis(50));
        let err = fast_timeout_runner
            .run("sh", &["-c", "sleep 1"])
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        let default_device = BluetoothDevice::default();
        assert!(format!("{default_device:?}").contains("BluetoothDevice::System"));
    }

    #[test]
    fn parse_pcm_info_codec_variations() {
        let path = "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source";
        let name_map = HashMap::new();

        let space_codecs_out = "Transport: A2DP-sink\nAvailable codecs: SBC AAC\n";
        let pcm = SystemInspector::parse_pcm_info(space_codecs_out, path, &name_map).unwrap();
        assert_eq!(pcm.available_codecs.as_ref(), &["SBC".into(), "AAC".into()]);

        let unknown_codecs_out = "Transport: A2DP-sink\nAvailable codecs: [ Unknown ]\n";
        let pcm_unknown =
            SystemInspector::parse_pcm_info(unknown_codecs_out, path, &name_map).unwrap();
        assert!(pcm_unknown.available_codecs.is_empty());
    }
}
