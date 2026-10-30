//! `CamillaDSP` pipeline monitor, WebSocket client, and diagnostic abstraction.
//!
//! Provides strongly-typed, non-blocking interfaces for inspecting the `CamillaDSP`
//! audio processing pipeline on the Raspberry Pi 3 appliance. Connects to the native
//! `CamillaDSP` WebSocket server (default: `127.0.0.1:1234`), reads real-time processing
//! states, CPU load, rate adjust ratios, buffer fill levels, and dBFS capture/playback
//! signal levels.
//!
//! Conforms to Microsoft Pragmatic Rust Guidelines:
//! - Strongly typed domain models ([`DspHost`], [`DspPort`], [`ProcessingState`], [`StopReason`])
//! - Inherent mocking via [`DspDevice::new_mocked`] and [`DspDeviceMock`]
//! - Thread-safe, cheaply cloneable [`DspDevice`] handle (`Arc`-backed per `M-SERVICES-CLONE`)
//! - Canonical error struct [`DspError`] with query predicates and single-line telemetry summaries

use std::backtrace::Backtrace;
use std::fmt::{self, Display, Formatter};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::bluetooth::ServiceState;
use crate::runtime::FromJoinError;

/// Default `CamillaDSP` WebSocket host address.
pub const DEFAULT_DSP_HOST: &str = "127.0.0.1";

/// Default `CamillaDSP` WebSocket TCP port number.
pub const DEFAULT_DSP_PORT: u16 = 1234;

/// Default `CamillaDSP` ALSA chunk size / buffer capacity in samples.
pub const DEFAULT_DSP_BUFFER_CAPACITY: u32 = 1024;

/// Minimum valid TCP port number.
pub const MIN_DSP_PORT: u16 = 1;

/// Maximum valid TCP port number.
pub const MAX_DSP_PORT: u16 = 65535;

/// Strong newtype wrapping a validated `CamillaDSP` host address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DspHost(Box<str>);

impl Default for DspHost {
    fn default() -> Self {
        Self(DEFAULT_DSP_HOST.into())
    }
}

impl Display for DspHost {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl DspHost {
    /// Validates and constructs a [`DspHost`] from a string slice.
    ///
    /// # Errors
    /// Returns [`DspError`] if the host is empty, contains whitespace, or contains invalid characters.
    pub fn new(host: &str) -> Result<Self, DspError> {
        Self::parse_str(host).map_err(DspError::invalid_state)
    }

    /// Parses and validates a host string for CLI and user-facing inputs.
    ///
    /// # Errors
    /// Returns a descriptive error message string if validation fails.
    pub fn parse_str(host: &str) -> Result<Self, String> {
        let trimmed = host.trim();
        if trimmed.is_empty() {
            return Err("host cannot be empty".to_string());
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "host '{trimmed}' contains invalid characters or whitespace"
            ));
        }
        // Accept valid IP addresses or valid DNS hostnames
        if trimmed.parse::<IpAddr>().is_ok() {
            return Ok(Self(trimmed.into()));
        }
        // Validate DNS hostname / localhost (RFC 1123: no leading/trailing dot/dash, <=253 chars, alphanumeric)
        if !trimmed.starts_with('.')
            && !trimmed.starts_with('-')
            && !trimmed.ends_with('.')
            && !trimmed.ends_with('-')
            && trimmed.len() <= 253
            && trimmed.chars().any(|c| c.is_ascii_alphanumeric())
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            return Ok(Self(trimmed.into()));
        }
        Err(format!("invalid host format '{trimmed}'"))
    }

    /// Returns the host as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` if the host refers to the local loopback interface (`127.0.0.1`, `localhost`, `::1`).
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        let s = self.as_str().trim_start_matches('[').trim_end_matches(']');
        if s.eq_ignore_ascii_case("localhost") {
            return true;
        }
        s.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }
}

/// Strong newtype wrapping a validated `CamillaDSP` TCP port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DspPort(u16);

impl Default for DspPort {
    fn default() -> Self {
        Self(DEFAULT_DSP_PORT)
    }
}

impl Display for DspPort {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl DspPort {
    /// Constructs a [`DspPort`] from a primitive `u16`.
    ///
    /// # Errors
    /// Returns [`DspError`] if the port is less than [`MIN_DSP_PORT`].
    pub fn new(port: u16) -> Result<Self, DspError> {
        if port < MIN_DSP_PORT {
            return Err(DspError::invalid_state("port cannot be zero"));
        }
        Ok(Self(port))
    }

    /// Parses and validates a port string from CLI inputs, rejecting signed prefixes.
    ///
    /// # Errors
    /// Returns a descriptive error message string if the string is empty, signed, or out of range.
    pub fn parse_str(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("port cannot be empty".to_string());
        }
        if trimmed.starts_with('+') || trimmed.starts_with('-') {
            return Err(format!(
                "invalid port '{trimmed}': signed prefixes ('+', '-') are not permitted"
            ));
        }
        let val: u16 = trimmed
            .parse()
            .map_err(|e| format!("invalid port '{trimmed}': must be an unsigned integer {MIN_DSP_PORT}..={MAX_DSP_PORT}: {e}"))?;
        if val < MIN_DSP_PORT {
            return Err(format!(
                "invalid port '{val}': port must be between {MIN_DSP_PORT} and {MAX_DSP_PORT}"
            ));
        }
        Ok(Self(val))
    }

    /// Returns the port as a `u16` integer.
    #[must_use]
    pub const fn as_u16(&self) -> u16 {
        self.0
    }
}

/// Operational state of the `CamillaDSP` audio processing engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessingState {
    /// Processing is running normally.
    Running,
    /// Processing is paused because the input signal is silent.
    Paused,
    /// The program is inactive and waiting for a new configuration.
    Inactive,
    /// The program is starting up processing with a new configuration.
    Starting,
    /// Processing is stalled because the capture device is not providing data.
    Stalled,
    /// Unknown or forward-compatible future engine state.
    Unknown(Box<str>),
}

impl ProcessingState {
    /// Parses a state string returned from the `CamillaDSP` WebSocket API.
    #[must_use]
    pub fn parse_state(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "inactive" => Self::Inactive,
            "starting" => Self::Starting,
            "stalled" => Self::Stalled,
            _ => Self::Unknown(s.trim().into()),
        }
    }

    /// Returns `true` if processing is actively running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Returns `true` if processing is paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        matches!(self, Self::Paused)
    }

    /// Returns `true` if processing is stalled.
    #[must_use]
    pub const fn is_stalled(&self) -> bool {
        matches!(self, Self::Stalled)
    }

    /// Returns a human-readable description of the state.
    #[must_use]
    pub const fn description(&self) -> &'static str {
        match self {
            Self::Running => "Audio processing active and running",
            Self::Paused => "Processing paused (silence detected)",
            Self::Inactive => "Inactive (waiting for configuration)",
            Self::Starting => "Starting up pipeline",
            Self::Stalled => "Stalled (capture device starved)",
            Self::Unknown(_) => "Unknown state",
        }
    }

    /// Returns the canonical state string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::Inactive => "Inactive",
            Self::Starting => "Starting",
            Self::Stalled => "Stalled",
            Self::Unknown(s) => s.as_ref(),
        }
    }
}

impl Display for ProcessingState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reason why `CamillaDSP` stopped audio processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// Processing has not stopped.
    None,
    /// Capture device reached the end of the stream.
    Done,
    /// Capture device encountered a hardware or I/O error.
    CaptureError,
    /// Playback device encountered a hardware or I/O error.
    PlaybackError,
    /// Capture format or sample rate changed.
    CaptureFormatChange,
    /// Playback format or sample rate changed.
    PlaybackFormatChange,
    /// Unknown or vendor-specific stop reason.
    Unknown(Box<str>),
}

impl StopReason {
    /// Parses a stop reason string returned from `CamillaDSP`.
    #[must_use]
    pub fn parse_reason(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Self::None,
            "done" => Self::Done,
            "captureerror" => Self::CaptureError,
            "playbackerror" => Self::PlaybackError,
            "captureformatchange" => Self::CaptureFormatChange,
            "playbackformatchange" => Self::PlaybackFormatChange,
            _ => Self::Unknown(s.trim().into()),
        }
    }

    /// Returns `true` if processing is not stopped.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Returns `true` if processing stopped due to an error.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::CaptureError | Self::PlaybackError)
    }

    /// Returns a human-readable description of the stop reason.
    #[must_use]
    pub const fn description(&self) -> &'static str {
        match self {
            Self::None => "No stop recorded",
            Self::Done => "Stream completed",
            Self::CaptureError => "Capture device hardware error",
            Self::PlaybackError => "Playback device hardware error",
            Self::CaptureFormatChange => "Capture audio format changed",
            Self::PlaybackFormatChange => "Playback audio format changed",
            Self::Unknown(_) => "Unknown stop reason",
        }
    }

    /// Returns the canonical stop reason string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "None",
            Self::Done => "Done",
            Self::CaptureError => "CaptureError",
            Self::PlaybackError => "PlaybackError",
            Self::CaptureFormatChange => "CaptureFormatChange",
            Self::PlaybackFormatChange => "PlaybackFormatChange",
            Self::Unknown(s) => s.as_ref(),
        }
    }
}

impl Display for StopReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Multi-channel audio signal levels in dBFS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DspChannelLevels {
    /// Peak signal levels in dBFS per channel.
    pub peaks: Box<[f32]>,
    /// RMS signal levels in dBFS per channel.
    pub rms: Box<[f32]>,
}

impl DspChannelLevels {
    /// Constructs [`DspChannelLevels`] from peak and RMS vectors.
    #[must_use]
    pub fn new(peaks: Vec<f32>, rms: Vec<f32>) -> Self {
        Self {
            peaks: peaks.into_boxed_slice(),
            rms: rms.into_boxed_slice(),
        }
    }

    /// Returns the number of channels.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.peaks.len()
    }

    /// Returns peak dBFS for the given channel index, or `-100.0` if out of bounds.
    #[must_use]
    pub fn peak_or_floor(&self, channel: usize) -> f32 {
        self.peaks.get(channel).copied().unwrap_or(-100.0)
    }

    /// Returns RMS dBFS for the given channel index, or `-100.0` if out of bounds.
    #[must_use]
    pub fn rms_or_floor(&self, channel: usize) -> f32 {
        self.rms.get(channel).copied().unwrap_or(-100.0)
    }
}

/// Snapshot of the `CamillaDSP` audio pipeline status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DspStatus {
    /// Systemd `camilladsp.service` daemon state.
    pub service: ServiceState,
    /// Connected WebSocket host address.
    pub host: DspHost,
    /// Connected WebSocket port number.
    pub port: DspPort,
    /// `CamillaDSP` software version (e.g. `4.1.3`).
    pub version: Option<Box<str>>,
    /// Active configuration title.
    pub config_title: Option<Box<str>>,
    /// Active configuration file path.
    pub config_path: Option<Box<str>>,
    /// Measured capture sample rate in Hz.
    pub capture_rate: Option<u32>,
    /// Current audio processing state.
    pub state: ProcessingState,
    /// CPU processing load capacity utilization as a percentage (0.0..=100.0%).
    pub processing_load: f32,
    /// Asynchronous resampler CPU load as a percentage, if active.
    pub resampler_load: Option<f32>,
    /// Clock adjustment ratio applied to the resampler (1.0 = nominal).
    pub rate_adjust: f64,
    /// Playback buffer level in samples.
    pub buffer_level: u32,
    /// Total buffer capacity in samples (typically 1024).
    pub buffer_capacity: u32,
    /// Master digital volume in dB.
    pub volume_db: Option<f32>,
    /// Master mute state.
    pub muted: Option<bool>,
    /// Counter of digital clipped samples since configuration was loaded.
    pub clipped_samples: u64,
    /// Reason why processing stopped, if applicable.
    pub stop_reason: StopReason,
    /// Input audio capture signal levels per channel.
    pub capture: DspChannelLevels,
    /// Output audio playback signal levels per channel.
    pub playback: DspChannelLevels,
}

impl DspStatus {
    /// Returns the buffer fill level as a percentage (0.0..=100.0%).
    #[must_use]
    pub fn buffer_fill_percent(&self) -> f32 {
        if self.buffer_capacity == 0 {
            0.0
        } else {
            (self.buffer_level as f32 / self.buffer_capacity as f32) * 100.0
        }
    }

    /// Returns the resampler rate adjustment in parts-per-million (ppm).
    #[must_use]
    pub fn rate_adjust_ppm(&self) -> f64 {
        (self.rate_adjust - 1.0) * 1_000_000.0
    }

    /// Returns `true` if processing is running and no clipped samples have occurred.
    #[must_use]
    pub fn is_operational(&self) -> bool {
        self.state.is_running() && self.clipped_samples == 0
    }

    /// Returns `true` if any clipped samples have been detected.
    #[must_use]
    pub const fn has_clipping(&self) -> bool {
        self.clipped_samples > 0
    }
}

/// Static hardware and pipeline configuration info for `CamillaDSP` on Raspberry Pi 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspHardwareInfo {
    /// Systemd service unit name.
    pub service_unit: &'static str,
    /// Default local WebSocket host.
    pub default_host: &'static str,
    /// Default local WebSocket port.
    pub default_port: u16,
    /// Primary crossover engine configuration path.
    pub config_path: &'static str,
    /// Service log output path.
    pub log_path: &'static str,
    /// ALSA capture device identifier.
    pub capture_device: &'static str,
    /// ALSA playback hardware device identifier.
    pub playback_device: &'static str,
    /// Subwoofer infrasonic high-pass filter frequency in Hz.
    pub infrasonic_hpf_hz: u32,
    /// Subwoofer to coaxial satellite crossover frequency in Hz.
    pub crossover_freq_hz: u32,
    /// Coaxial tweeter padding shelf frequency in Hz.
    pub tweeter_shelf_hz: u32,
    /// Standard ALSA chunk size in samples.
    pub chunk_size: u32,
}

impl DspHardwareInfo {
    /// Returns the standard hardware profile for the Raspberry Pi 3 Bluetooth Speaker appliance.
    #[must_use]
    pub const fn rpi3() -> Self {
        Self {
            service_unit: "camilladsp.service",
            default_host: DEFAULT_DSP_HOST,
            default_port: DEFAULT_DSP_PORT,
            config_path: "/etc/camilladsp/crossover.yml",
            log_path: "/var/log/camilladsp/camilladsp.log",
            capture_device: "hw:Loopback,1,0",
            playback_device: "hw:sndrpimerusamp,0",
            infrasonic_hpf_hz: 45,
            crossover_freq_hz: 180,
            tweeter_shelf_hz: 3000,
            chunk_size: DEFAULT_DSP_BUFFER_CAPACITY,
        }
    }
}

/// Internal error cause for `CamillaDSP` operations.
#[derive(Debug)]
pub(crate) enum DspErrorKind {
    /// Failed to establish a TCP or WebSocket connection to the server.
    ConnectionFailed {
        host: Box<str>,
        port: u16,
        message: Box<str>,
    },
    /// Invalid protocol message or unexpected response received from server.
    ProtocolError { message: Box<str> },
    /// Operation timed out waiting for server response.
    Timeout { message: Box<str> },
    /// `CamillaDSP` responded with an error result for a command.
    CommandFailed {
        command: Box<str>,
        message: Box<str>,
    },
    /// Invalid domain state or parameter.
    InvalidState { message: Box<str> },
    /// Asynchronous task was cancelled.
    Cancelled { message: Box<str> },
}

/// Canonical error struct for `CamillaDSP` subsystem failures.
#[derive(Debug)]
pub struct DspError {
    pub(crate) kind: DspErrorKind,
    pub(crate) backtrace: Backtrace,
}

impl DspError {
    pub(crate) fn connection_failed(
        host: impl Into<Box<str>>,
        port: u16,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            kind: DspErrorKind::ConnectionFailed {
                host: host.into(),
                port,
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn protocol_error(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: DspErrorKind::ProtocolError {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn timeout(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: DspErrorKind::Timeout {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn command_failed(
        command: impl Into<Box<str>>,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            kind: DspErrorKind::CommandFailed {
                command: command.into(),
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn invalid_state(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: DspErrorKind::InvalidState {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns `true` if the failure was caused by a connection refusal or drop.
    #[must_use]
    pub const fn is_connection_failed(&self) -> bool {
        matches!(self.kind, DspErrorKind::ConnectionFailed { .. })
    }

    /// Returns `true` if the failure was a protocol format or parsing error.
    #[must_use]
    pub const fn is_protocol_error(&self) -> bool {
        matches!(self.kind, DspErrorKind::ProtocolError { .. })
    }

    /// Returns `true` if the operation timed out.
    #[must_use]
    pub const fn is_timeout(&self) -> bool {
        matches!(self.kind, DspErrorKind::Timeout { .. })
    }

    /// Returns `true` if `CamillaDSP` returned an explicit command error.
    #[must_use]
    pub const fn is_command_failed(&self) -> bool {
        matches!(self.kind, DspErrorKind::CommandFailed { .. })
    }

    /// Returns `true` if an internal state or configuration parameter was invalid.
    #[must_use]
    pub const fn is_invalid_state(&self) -> bool {
        matches!(self.kind, DspErrorKind::InvalidState { .. })
    }

    /// Returns `true` if the underlying asynchronous task was cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, DspErrorKind::Cancelled { .. })
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            DspErrorKind::ConnectionFailed {
                host,
                port,
                message,
            } => {
                write!(
                    f,
                    "Failed to connect to CamillaDSP at ws://{host}:{port}: {message}"
                )
            }
            DspErrorKind::ProtocolError { message } => {
                write!(f, "CamillaDSP WebSocket protocol error: {message}")
            }
            DspErrorKind::Timeout { message } => {
                write!(f, "CamillaDSP request timed out: {message}")
            }
            DspErrorKind::CommandFailed { command, message } => {
                write!(f, "CamillaDSP command '{command}' failed: {message}")
            }
            DspErrorKind::InvalidState { message } => {
                write!(f, "Invalid DSP parameter or state: {message}")
            }
            DspErrorKind::Cancelled { message } => {
                write!(f, "DSP task cancelled: {message}")
            }
        }
    }
}

impl_canonical_error_display!(DspError);
impl std::error::Error for DspError {}

impl FromJoinError for DspError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: DspErrorKind::Cancelled {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }
}

/// Simulated `CamillaDSP` state controller for deterministic testing.
#[derive(Debug, Clone)]
pub struct DspDeviceMock {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Debug)]
struct MockState {
    service: ServiceState,
    version: Option<Box<str>>,
    config_title: Option<Box<str>>,
    config_path: Option<Box<str>>,
    capture_rate: Option<u32>,
    state: ProcessingState,
    processing_load: f32,
    resampler_load: Option<f32>,
    rate_adjust: f64,
    buffer_level: u32,
    buffer_capacity: u32,
    volume_db: Option<f32>,
    muted: Option<bool>,
    clipped_samples: u64,
    stop_reason: StopReason,
    capture_peaks: Vec<f32>,
    capture_rms: Vec<f32>,
    playback_peaks: Vec<f32>,
    playback_rms: Vec<f32>,
    injected_fault: Option<String>,
}

impl Default for DspDeviceMock {
    fn default() -> Self {
        Self::new()
    }
}

impl DspDeviceMock {
    /// Creates a mock DSP controller with nominal defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockState {
                service: ServiceState::Active,
                version: Some("4.1.3".into()),
                config_title: Some("CamillaDSP Crossover Engine".into()),
                config_path: Some("/etc/camilladsp/crossover.yml".into()),
                capture_rate: Some(48000),
                state: ProcessingState::Running,
                processing_load: 3.45,
                resampler_load: Some(0.12),
                rate_adjust: 1.000_012,
                buffer_level: 512,
                buffer_capacity: DEFAULT_DSP_BUFFER_CAPACITY,
                volume_db: Some(0.0),
                muted: Some(false),
                clipped_samples: 0,
                stop_reason: StopReason::None,
                capture_peaks: vec![-15.2, -14.8],
                capture_rms: vec![-20.1, -19.8],
                playback_peaks: vec![-18.5, -18.2],
                playback_rms: vec![-23.4, -23.1],
                injected_fault: None,
            })),
        }
    }

    /// Sets the simulated systemd service state.
    pub fn set_service_state(&self, state: ServiceState) {
        if let Ok(mut g) = self.inner.lock() {
            g.service = state;
        }
    }

    /// Sets the simulated processing state.
    pub fn set_state(&self, state: ProcessingState) {
        if let Ok(mut g) = self.inner.lock() {
            g.state = state;
        }
    }

    /// Sets the simulated CPU processing load percentage.
    pub fn set_processing_load(&self, load: f32) {
        if let Ok(mut g) = self.inner.lock() {
            g.processing_load = load;
        }
    }

    /// Sets the simulated buffer fill level in samples.
    pub fn set_buffer_level(&self, samples: u32) {
        if let Ok(mut g) = self.inner.lock() {
            g.buffer_level = samples;
        }
    }

    /// Sets the simulated resampler CPU load percentage.
    pub fn set_resampler_load(&self, load: Option<f32>) {
        if let Ok(mut g) = self.inner.lock() {
            g.resampler_load = load;
        }
    }

    /// Sets the simulated stop reason.
    pub fn set_stop_reason(&self, reason: StopReason) {
        if let Ok(mut g) = self.inner.lock() {
            g.stop_reason = reason;
        }
    }

    /// Sets the simulated DSP software version.
    pub fn set_version(&self, version: Option<&str>) {
        if let Ok(mut g) = self.inner.lock() {
            g.version = version.map(Into::into);
        }
    }

    /// Sets the simulated active configuration title.
    pub fn set_config_title(&self, title: Option<&str>) {
        if let Ok(mut g) = self.inner.lock() {
            g.config_title = title.map(Into::into);
        }
    }

    /// Sets the simulated measured capture rate in Hz.
    pub fn set_capture_rate(&self, rate: Option<u32>) {
        if let Ok(mut g) = self.inner.lock() {
            g.capture_rate = rate;
        }
    }

    /// Sets the simulated rate adjust ratio.
    pub fn set_rate_adjust(&self, ratio: f64) {
        if let Ok(mut g) = self.inner.lock() {
            g.rate_adjust = ratio;
        }
    }

    /// Sets the simulated clipped samples counter.
    pub fn set_clipped_samples(&self, count: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.clipped_samples = count;
        }
    }

    /// Sets the simulated master volume and mute state.
    pub fn set_volume(&self, volume_db: Option<f32>, muted: Option<bool>) {
        if let Ok(mut g) = self.inner.lock() {
            g.volume_db = volume_db;
            g.muted = muted;
        }
    }

    /// Sets the simulated multi-channel signal levels in dBFS.
    pub fn set_levels(
        &self,
        capture_peaks: Vec<f32>,
        capture_rms: Vec<f32>,
        playback_peaks: Vec<f32>,
        playback_rms: Vec<f32>,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            g.capture_peaks = capture_peaks;
            g.capture_rms = capture_rms;
            g.playback_peaks = playback_peaks;
            g.playback_rms = playback_rms;
        }
    }

    /// Injects a fault message that causes the mock device to return an error.
    pub fn inject_fault(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.inner.lock() {
            g.injected_fault = Some(msg.into());
        }
    }

    /// Clears any previously injected fault.
    pub fn clear_fault(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.injected_fault = None;
        }
    }

    fn read_status(&self, host: &DspHost, port: DspPort) -> Result<DspStatus, DspError> {
        let g = self.inner.lock().expect("dsp mock mutex poisoned");
        if let Some(ref fault) = g.injected_fault {
            return Err(DspError::protocol_error(fault.clone()));
        }
        Ok(DspStatus {
            service: g.service,
            host: host.clone(),
            port,
            version: g.version.clone(),
            config_title: g.config_title.clone(),
            config_path: g.config_path.clone(),
            capture_rate: g.capture_rate,
            state: g.state.clone(),
            processing_load: g.processing_load,
            resampler_load: g.resampler_load,
            rate_adjust: g.rate_adjust,
            buffer_level: g.buffer_level,
            buffer_capacity: g.buffer_capacity,
            volume_db: g.volume_db,
            muted: g.muted,
            clipped_samples: g.clipped_samples,
            stop_reason: g.stop_reason.clone(),
            capture: DspChannelLevels::new(g.capture_peaks.clone(), g.capture_rms.clone()),
            playback: DspChannelLevels::new(g.playback_peaks.clone(), g.playback_rms.clone()),
        })
    }

    fn reset_clipped(&self) -> Result<(), DspError> {
        let mut g = self.inner.lock().expect("dsp mock mutex poisoned");
        if let Some(ref fault) = g.injected_fault {
            return Err(DspError::protocol_error(fault.clone()));
        }
        g.clipped_samples = 0;
        Ok(())
    }
}

enum Backend {
    Hardware {
        host: DspHost,
        port: DspPort,
    },
    Mock {
        host: DspHost,
        port: DspPort,
        mock: DspDeviceMock,
    },
}

/// Thread-safe `CamillaDSP` client and pipeline status monitor.
///
/// Thread-safe and cheaply cloneable (`Arc`-backed handle per `M-SERVICES-CLONE`).
#[derive(Clone)]
pub struct DspDevice {
    backend: Arc<Backend>,
}

impl Default for DspDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DspDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &*self.backend {
            Backend::Mock { host, port, .. } => f
                .debug_struct("DspDevice::Mock")
                .field("host", host)
                .field("port", port)
                .finish(),
            Backend::Hardware { host, port } => f
                .debug_struct("DspDevice::Hardware")
                .field("host", host)
                .field("port", port)
                .finish(),
        }
    }
}

impl DspDevice {
    /// Creates a new `CamillaDSP` device targeting default local host and port (`127.0.0.1:1234`).
    #[must_use]
    pub fn new() -> Self {
        Self::with_host_and_port(DspHost::default(), DspPort::default())
    }

    /// Creates a device targeting a specific host and port.
    #[must_use]
    pub fn with_host_and_port(host: DspHost, port: DspPort) -> Self {
        Self {
            backend: Arc::new(Backend::Hardware { host, port }),
        }
    }

    /// Creates a simulated device and companion mock handle for testing.
    #[must_use]
    pub fn new_mocked() -> (Self, DspDeviceMock) {
        let mock = DspDeviceMock::new();
        let device = Self {
            backend: Arc::new(Backend::Mock {
                host: DspHost::default(),
                port: DspPort::default(),
                mock: mock.clone(),
            }),
        };
        (device, mock)
    }

    /// Returns the configured host.
    #[must_use]
    pub fn host(&self) -> &DspHost {
        match &*self.backend {
            Backend::Hardware { host, .. } | Backend::Mock { host, .. } => host,
        }
    }

    /// Returns the configured port.
    #[must_use]
    pub fn port(&self) -> DspPort {
        match &*self.backend {
            Backend::Hardware { port, .. } | Backend::Mock { port, .. } => *port,
        }
    }

    /// Returns hardware and pipeline metadata for the appliance.
    #[must_use]
    pub const fn hardware_info(&self) -> DspHardwareInfo {
        DspHardwareInfo::rpi3()
    }

    /// Reads `CamillaDSP` pipeline telemetry synchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if communication fails or the server returns an error.
    pub fn read_status(&self) -> Result<DspStatus, DspError> {
        match &*self.backend {
            Backend::Mock { host, port, mock } => mock.read_status(host, *port),
            Backend::Hardware { host, port } => {
                let host_clone = host.clone();
                let port_val = *port;
                let handle = std::thread::spawn(move || {
                    let service = if host_clone.is_loopback() {
                        probe_systemd_service(DspHardwareInfo::rpi3().service_unit)
                    } else {
                        ServiceState::NotFound
                    };
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| DspError::invalid_state(e.to_string()))?;
                    rt.block_on(Self::query_hardware_status(host_clone, port_val, service))
                });
                handle
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            }
        }
    }

    /// Reads `CamillaDSP` pipeline telemetry asynchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if communication fails or the server returns an error.
    pub async fn read_status_async(&self) -> Result<DspStatus, DspError> {
        let dev = self.clone();
        crate::runtime::run_blocking("dsp status", move || dev.read_status()).await
    }

    /// Resets the clipped samples counter to zero synchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if the reset command fails or the socket cannot be contacted.
    pub fn reset_clipped_samples(&self) -> Result<(), DspError> {
        match &*self.backend {
            Backend::Mock { mock, .. } => mock.reset_clipped(),
            Backend::Hardware { host, port } => {
                let host_clone = host.clone();
                let port_val = *port;
                let handle = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| DspError::invalid_state(e.to_string()))?;
                    rt.block_on(Self::reset_hardware_clipped(host_clone, port_val))
                });
                handle
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            }
        }
    }

    /// Resets the clipped samples counter to zero asynchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if the reset command fails.
    pub async fn reset_clipped_samples_async(&self) -> Result<(), DspError> {
        let dev = self.clone();
        crate::runtime::run_blocking("dsp reset clipped", move || dev.reset_clipped_samples()).await
    }

    async fn connect_ws(
        host: &DspHost,
        port: DspPort,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, DspError> {
        let host_str = if host.as_str().contains(':') && !host.as_str().starts_with('[') {
            format!("[{}]", host.as_str())
        } else {
            host.as_str().to_string()
        };
        let url = format!("ws://{host_str}:{}", port.as_u16());
        let connect_fut = tokio_tungstenite::connect_async(&url);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(3), connect_fut)
            .await
            .map_err(|_timeout_err| {
                DspError::timeout(format!("connection to {url} timed out after 3s"))
            })?
            .map_err(|e| {
                DspError::connection_failed(host.as_str(), port.as_u16(), e.to_string())
            })?;
        Ok(ws)
    }

    async fn send_command<S>(ws: &mut S, cmd: &str) -> Result<serde_json::Value, DspError>
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
            + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        let payload =
            serde_json::to_string(cmd).map_err(|e| DspError::protocol_error(e.to_string()))?;
        ws.send(Message::Text(payload.into()))
            .await
            .map_err(|e| DspError::protocol_error(format!("failed to send '{cmd}': {e}")))?;

        let recv_fut = async {
            while let Some(msg) = ws.next().await {
                match msg {
                    Ok(Message::Text(text)) => return Ok(text),
                    Ok(Message::Close(_)) => {
                        return Err(DspError::protocol_error("connection closed by server"));
                    }
                    Ok(_) => {} // Ignore ping/pong/binary
                    Err(e) => return Err(DspError::protocol_error(e.to_string())),
                }
            }
            Err(DspError::protocol_error("stream ended without response"))
        };

        let text = tokio::time::timeout(Duration::from_secs(2), recv_fut)
            .await
            .map_err(|_timeout_err| DspError::timeout(format!("command '{cmd}' timed out")))??;

        let val: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            DspError::protocol_error(format!("invalid JSON in response to '{cmd}': {e}"))
        })?;

        // Response format: { "<cmd>": { "result": "Ok", "value": ... } }
        if let Some(cmd_obj) = val.get(cmd) {
            let res = cmd_obj
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("Error");
            if res == "Ok" {
                Ok(cmd_obj
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null))
            } else {
                let err_msg = cmd_obj
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                Err(DspError::command_failed(cmd, err_msg))
            }
        } else {
            Err(DspError::protocol_error(format!(
                "missing '{cmd}' key in server response: {val}"
            )))
        }
    }

    async fn reset_hardware_clipped(host: DspHost, port: DspPort) -> Result<(), DspError> {
        let mut ws = Self::connect_ws(&host, port).await?;
        Self::send_command(&mut ws, "ResetClippedSamples").await?;
        let _ = ws.close(None).await;
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Sequentially queries all CamillaDSP WebSocket pipeline telemetry fields"
    )]
    async fn query_hardware_status(
        host: DspHost,
        port: DspPort,
        service: ServiceState,
    ) -> Result<DspStatus, DspError> {
        let mut ws = Self::connect_ws(&host, port).await?;

        // 1. Version
        let version = Self::send_command(&mut ws, "GetVersion")
            .await
            .ok()
            .and_then(|v| v.as_str().map(Into::into));

        // 2. Config Title
        let config_title = Self::send_command(&mut ws, "GetConfigTitle")
            .await
            .ok()
            .and_then(|v| v.as_str().map(Into::into));

        // 3. Config File Path
        let config_path = Self::send_command(&mut ws, "GetConfigFilePath")
            .await
            .ok()
            .and_then(|v| v.as_str().map(Into::into));

        // 4. State (mandatory core telemetry)
        let state_val = Self::send_command(&mut ws, "GetState").await?;
        let state_str = state_val
            .as_str()
            .ok_or_else(|| DspError::protocol_error("invalid GetState string response"))?;
        let state = ProcessingState::parse_state(state_str);

        // 5. Processing Load (mandatory core telemetry)
        let load_val = Self::send_command(&mut ws, "GetProcessingLoad").await?;
        let processing_load = load_val
            .as_f64()
            .map(|f| f as f32)
            .ok_or_else(|| DspError::protocol_error("invalid GetProcessingLoad float response"))?;

        // 6. Resampler Load (optional v4 feature)
        let resampler_load = Self::send_command(&mut ws, "GetResamplerLoad")
            .await
            .ok()
            .and_then(|v| v.as_f64().map(|f| f as f32));

        // 7. Rate Adjust (mandatory core telemetry)
        let rate_val = Self::send_command(&mut ws, "GetRateAdjust").await?;
        let rate_adjust = rate_val
            .as_f64()
            .ok_or_else(|| DspError::protocol_error("invalid GetRateAdjust float response"))?;

        // 8. Buffer Level (mandatory core telemetry)
        let buf_val = Self::send_command(&mut ws, "GetBufferLevel").await?;
        let buffer_level = buf_val
            .as_u64()
            .map(|u| u as u32)
            .ok_or_else(|| DspError::protocol_error("invalid GetBufferLevel integer response"))?;

        // 9. Clipped Samples (mandatory core telemetry)
        let clip_val = Self::send_command(&mut ws, "GetClippedSamples").await?;
        let clipped_samples = clip_val.as_u64().ok_or_else(|| {
            DspError::protocol_error("invalid GetClippedSamples integer response")
        })?;

        // 10. Capture Rate (optional, present when capture stream active)
        let capture_rate = Self::send_command(&mut ws, "GetCaptureRate")
            .await
            .ok()
            .and_then(|v| v.as_u64().map(|u| u as u32));

        // 11. Stop Reason (mandatory core telemetry)
        let stop_val = Self::send_command(&mut ws, "GetStopReason").await?;
        let stop_str = stop_val
            .as_str()
            .ok_or_else(|| DspError::protocol_error("invalid GetStopReason string response"))?;
        let stop_reason = StopReason::parse_reason(stop_str);

        // 12. Volume & Mute
        let volume_db = Self::send_command(&mut ws, "GetVolume")
            .await
            .ok()
            .and_then(|v| v.as_f64().map(|f| f as f32));

        let muted = Self::send_command(&mut ws, "GetMute")
            .await
            .ok()
            .and_then(|v| v.as_bool());

        // 13. Levels: Try GetSignalLevels first, fallback to individual queries
        let (capture_peaks, capture_rms, playback_peaks, playback_rms) =
            if let Ok(levels_val) = Self::send_command(&mut ws, "GetSignalLevels").await {
                parse_levels_object(&levels_val)
            } else {
                let cap_p = Self::send_command(&mut ws, "GetCaptureSignalPeak")
                    .await
                    .ok()
                    .map(|v| parse_f32_list(&v))
                    .unwrap_or_default();
                let cap_r = Self::send_command(&mut ws, "GetCaptureSignalRms")
                    .await
                    .ok()
                    .map(|v| parse_f32_list(&v))
                    .unwrap_or_default();
                let play_p = Self::send_command(&mut ws, "GetPlaybackSignalPeak")
                    .await
                    .ok()
                    .map(|v| parse_f32_list(&v))
                    .unwrap_or_default();
                let play_r = Self::send_command(&mut ws, "GetPlaybackSignalRms")
                    .await
                    .ok()
                    .map(|v| parse_f32_list(&v))
                    .unwrap_or_default();
                (cap_p, cap_r, play_p, play_r)
            };

        let _ = ws.close(None).await;

        Ok(DspStatus {
            service,
            host,
            port,
            version,
            config_title,
            config_path,
            capture_rate,
            state,
            processing_load,
            resampler_load,
            rate_adjust,
            buffer_level,
            buffer_capacity: DEFAULT_DSP_BUFFER_CAPACITY,
            volume_db,
            muted,
            clipped_samples,
            stop_reason,
            capture: DspChannelLevels::new(capture_peaks, capture_rms),
            playback: DspChannelLevels::new(playback_peaks, playback_rms),
        })
    }
}

fn parse_f32_list(val: &serde_json::Value) -> Vec<f32> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_levels_object(val: &serde_json::Value) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
    let cap_peak = val
        .get("capture_peak")
        .map(parse_f32_list)
        .unwrap_or_default();
    let cap_rms = val
        .get("capture_rms")
        .map(parse_f32_list)
        .unwrap_or_default();
    let play_peak = val
        .get("playback_peak")
        .map(parse_f32_list)
        .unwrap_or_default();
    let play_rms = val
        .get("playback_rms")
        .map(parse_f32_list)
        .unwrap_or_default();
    (cap_peak, cap_rms, play_peak, play_rms)
}

#[expect(
    clippy::cast_sign_loss,
    reason = "ratio is mathematically non-negative after clamp"
)]
fn meter_bar(ratio: f32, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = ((ratio.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    let empty = width.saturating_sub(filled);

    let mut bar = String::with_capacity(width * 4 + 2);
    bar.push('[');
    for _ in 0..filled {
        bar.push('█');
    }
    for _ in 0..empty {
        bar.push('░');
    }
    bar.push(']');
    bar
}

/// Formats a graphical ASCII/Unicode dBFS level meter bar.
///
/// Full scale (0.0 dBFS) fills all blocks; silence (≤ -60.0 dBFS) shows empty blocks.
#[must_use]
pub fn format_dbfs_meter(dbfs: f32, width: usize) -> String {
    // Dynamic range from -60.0 dBFS to 0.0 dBFS
    let clamped = dbfs.clamp(-60.0, 0.0);
    let ratio = (clamped + 60.0) / 60.0;
    meter_bar(ratio, width)
}

/// Formats a CPU load meter bar (0% to 100%).
#[must_use]
pub fn format_load_meter(load_pct: f32, width: usize) -> String {
    let ratio = load_pct.clamp(0.0, 100.0) / 100.0;
    meter_bar(ratio, width)
}

/// Probes systemd for a service unit state using D-Bus, adhering to M-MOCKABLE-SYSCALLS
/// and distinguishing unit absence (`NoSuchUnit`) from degradation/failure.
fn probe_systemd_service(unit_name: &str) -> ServiceState {
    let Ok(conn) = zbus::blocking::Connection::system() else {
        return ServiceState::Failed;
    };
    let Ok(manager) = SystemdManagerProxyBlocking::new(&conn) else {
        return ServiceState::Failed;
    };
    match manager.get_unit(unit_name) {
        Ok(path) => {
            let unit = match SystemdUnitProxyBlocking::builder(&conn)
                .path(path)
                .and_then(zbus::blocking::proxy::Builder::build)
            {
                Ok(u) => u,
                Err(ref e) if super::bluetooth::is_no_such_unit_error(e) => {
                    return ServiceState::NotFound;
                }
                Err(_) => return ServiceState::Failed,
            };
            let Ok(active) = unit.active_state() else {
                return ServiceState::Failed;
            };
            let Ok(load) = unit.load_state() else {
                return ServiceState::Failed;
            };
            super::bluetooth::DbusClient::parse_systemd_state(&active, &load)
        }
        Err(ref e) if super::bluetooth::is_no_such_unit_error(e) => ServiceState::NotFound,
        Err(_) => ServiceState::Failed,
    }
}

#[zbus::proxy(
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    interface = "org.freedesktop.systemd1.Manager",
    gen_async = false,
    blocking_name = "SystemdManagerProxyBlocking"
)]
trait SystemdManager {
    fn get_unit(&self, name: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
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

#[cfg(feature = "test-util")]
pub mod test_helpers {
    use super::{DspError, parse_f32_list, parse_levels_object};
    use crate::runtime::FromJoinError;

    /// Helper for testing JSON float list parsing.
    #[must_use]
    pub fn helper_parse_f32_list(val: &serde_json::Value) -> Vec<f32> {
        parse_f32_list(val)
    }

    /// Helper for testing JSON channel levels object parsing.
    #[must_use]
    pub fn helper_parse_levels_object(
        val: &serde_json::Value,
    ) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
        parse_levels_object(val)
    }

    /// Creates a `DspError::connection_failed` error for testing.
    #[must_use]
    pub fn make_connection_failed_error(host: &str, port: u16, msg: &str) -> DspError {
        DspError::connection_failed(host, port, msg)
    }

    /// Creates a `DspError::protocol_error` error for testing.
    #[must_use]
    pub fn make_protocol_error(msg: &str) -> DspError {
        DspError::protocol_error(msg)
    }

    /// Creates a `DspError::timeout` error for testing.
    #[must_use]
    pub fn make_timeout_error(msg: &str) -> DspError {
        DspError::timeout(msg)
    }

    /// Creates a `DspError::command_failed` error for testing.
    #[must_use]
    pub fn make_command_failed_error(cmd: &str, msg: &str) -> DspError {
        DspError::command_failed(cmd, msg)
    }

    /// Creates a `DspError::invalid_state` error for testing.
    #[must_use]
    pub fn make_invalid_state_error(msg: &str) -> DspError {
        DspError::invalid_state(msg)
    }

    /// Creates a `DspError::from_cancelled` error for testing.
    #[must_use]
    pub fn make_cancelled_error(msg: &str) -> DspError {
        DspError::from_cancelled(msg)
    }
}
