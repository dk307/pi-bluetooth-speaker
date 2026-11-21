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

use camilladsp_schema::config::{CaptureDevice, Configuration, PlaybackDevice};
use camilladsp_schema::protocol::{
    ProcessingState as SchemaProcessingState, StopReason as SchemaStopReason, WsCommand, WsReply,
    WsResult,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use speaker_common::runtime::FromJoinError;
use speaker_common::systemd::{ServiceState, probe_systemd_service};

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

/// Runtime configuration summary read from `CamillaDSP` (`GetConfigJson`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DspConfigInfo {
    /// Configured sample rate in Hz.
    pub samplerate: Option<u32>,
    /// Configured chunk size in samples.
    pub chunksize: Option<u32>,
    /// Configured capture device identifier (e.g. `hw:Loopback,1,0`).
    pub capture_device: Option<Box<str>>,
    /// Configured capture sample format (e.g. `S32_LE`).
    pub capture_format: Option<Box<str>>,
    /// Configured playback device identifier (e.g. `hw:sndrpihifiberry,0`).
    pub playback_device: Option<Box<str>>,
    /// Configured playback sample format (e.g. `S32_LE`).
    pub playback_format: Option<Box<str>>,
    /// Names of configured mixers.
    pub mixers: Vec<Box<str>>,
    /// Names of configured filters.
    pub filters: Vec<Box<str>>,
    /// Names of configured processors.
    pub processors: Vec<Box<str>>,
    /// Number of pipeline stages configured.
    pub pipeline_stages: usize,
}

impl DspConfigInfo {
    /// Returns `true` when no mixers, filters, processors, or pipeline stages are configured.
    #[must_use]
    pub fn is_passthrough(&self) -> bool {
        self.pipeline_stages == 0
            && self.mixers.is_empty()
            && self.filters.is_empty()
            && self.processors.is_empty()
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
    /// Live configuration summary read from `CamillaDSP` (`GetConfigJson`).
    pub config: Option<DspConfigInfo>,
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

/// Static service metadata for `CamillaDSP` on Raspberry Pi 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspHardwareInfo {
    /// Systemd service unit name.
    pub service_unit: &'static str,
    /// Default local WebSocket host.
    pub default_host: &'static str,
    /// Default local WebSocket port.
    pub default_port: u16,
    /// Service log output path.
    pub log_path: &'static str,
}

impl DspHardwareInfo {
    /// Returns the standard service profile for the Raspberry Pi 3 Bluetooth Speaker appliance.
    #[must_use]
    pub const fn rpi3() -> Self {
        Self {
            service_unit: "camilladsp.service",
            default_host: DEFAULT_DSP_HOST,
            default_port: DEFAULT_DSP_PORT,
            log_path: "/var/log/camilladsp/camilladsp.log",
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

speaker_common::impl_canonical_error_display!(DspError);
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
    config: Option<DspConfigInfo>,
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
    last_set_config: Option<String>,
    last_set_config_json: Option<String>,
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
                config_title: Some("CamillaDSP".into()),
                config_path: Some("/etc/camilladsp/crossover.yml".into()),
                config: Some(DspConfigInfo {
                    samplerate: Some(48000),
                    chunksize: Some(DEFAULT_DSP_BUFFER_CAPACITY),
                    capture_device: Some("hw:Loopback,1,0".into()),
                    capture_format: Some("S32_LE".into()),
                    playback_device: Some("hw:sndrpihifiberry,0".into()),
                    playback_format: Some("S32_LE".into()),
                    mixers: Vec::new(),
                    filters: Vec::new(),
                    processors: Vec::new(),
                    pipeline_stages: 0,
                }),
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
                last_set_config: None,
                last_set_config_json: None,
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

    /// Sets the simulated parsed configuration summary.
    pub fn set_config(&self, config: Option<DspConfigInfo>) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(chunk) = config.as_ref().and_then(|c| c.chunksize) {
                g.buffer_capacity = chunk;
            }
            g.config = config;
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

    /// Mock state-only read (sibling of [`Self::read_status`]).
    fn read_state(&self, _host: &DspHost, _port: DspPort) -> Result<ProcessingState, DspError> {
        let g = self.inner.lock().expect("dsp mock mutex poisoned");
        if let Some(ref fault) = g.injected_fault {
            return Err(DspError::protocol_error(fault.clone()));
        }
        Ok(g.state.clone())
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
            config: g.config.clone(),
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

    /// Returns the most recent YAML configuration uploaded via `set_config` on this mock.
    #[must_use]
    pub fn last_set_config(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.last_set_config.clone())
    }

    /// Returns the most recent JSON configuration uploaded via `set_config_json` on this mock.
    #[must_use]
    pub fn last_set_config_json(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.last_set_config_json.clone())
    }

    fn reset_clipped(&self) -> Result<(), DspError> {
        let mut g = self.inner.lock().expect("dsp mock mutex poisoned");
        if let Some(ref fault) = g.injected_fault {
            return Err(DspError::protocol_error(fault.clone()));
        }
        g.clipped_samples = 0;
        Ok(())
    }

    fn set_config_yaml(&self, config_yaml: &str) -> Result<(), DspError> {
        let mut g = self.inner.lock().expect("dsp mock mutex poisoned");
        if let Some(ref fault) = g.injected_fault {
            return Err(DspError::protocol_error(fault.clone()));
        }
        g.last_set_config = Some(config_yaml.to_owned());
        Ok(())
    }

    fn set_config_json(&self, config_json: &str) -> Result<(), DspError> {
        let mut g = self.inner.lock().expect("dsp mock mutex poisoned");
        if let Some(ref fault) = g.injected_fault {
            return Err(DspError::protocol_error(fault.clone()));
        }
        g.last_set_config_json = Some(config_json.to_owned());
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

    /// Reads only the `CamillaDSP` engine state (`GetState`).
    ///
    /// Lightweight sibling of [`Self::read_status`]: a single WS connection
    /// and a single command, so ancillary-telemetry failures (rate adjust,
    /// buffer level, …) can never block the DSP stream-lifecycle watcher's
    /// start/stop edges.
    ///
    /// # Errors
    /// Returns [`DspError`] if communication fails or the server returns an error.
    pub fn read_state(&self) -> Result<ProcessingState, DspError> {
        match &*self.backend {
            Backend::Mock { host, port, mock } => mock.read_state(host, *port),
            Backend::Hardware { host, port } => {
                let host_clone = host.clone();
                let port_val = *port;
                let handle = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| DspError::invalid_state(e.to_string()))?;
                    rt.block_on(Self::query_state(host_clone, port_val))
                });
                handle
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            }
        }
    }

    /// Reads only the engine state asynchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if communication fails or the server returns an error.
    pub async fn read_state_async(&self) -> Result<ProcessingState, DspError> {
        let dev = self.clone();
        speaker_common::runtime::run_blocking("dsp state", move || dev.read_state()).await
    }

    async fn query_state(host: DspHost, port: DspPort) -> Result<ProcessingState, DspError> {
        let mut ws = Self::connect_ws(&host, port).await?;
        let reply = Self::send_ws_command(&mut ws, &WsCommand::GetState).await?;
        // Close with a proper WebSocket closing handshake: this poll runs at
        // 2-4 Hz for the lifecycle watcher, and dropping the stream without a
        // close frame makes the CamillaDSP socket server log a
        // "Lost connection: Connection reset without closing handshake"
        // warning per poll, flooding `/var/log/camilladsp/camilladsp.log`.
        // Bound the handshake wait so an unresponsive server can never hang
        // the lifecycle watcher's poll loop.
        let _ = tokio::time::timeout(Duration::from_millis(500), ws.close(None)).await;
        match reply {
            WsReply::GetState { result, value } => {
                check_ws_result("GetState", result)?;
                Ok(value.into())
            }
            other => Err(DspError::protocol_error(format!(
                "unexpected reply '{}' for GetState",
                other.name()
            ))),
        }
    }

    /// Reads `CamillaDSP` pipeline telemetry asynchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if communication fails or the server returns an error.
    pub async fn read_status_async(&self) -> Result<DspStatus, DspError> {
        let dev = self.clone();
        speaker_common::runtime::run_blocking("dsp status", move || dev.read_status()).await
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
        speaker_common::runtime::run_blocking("dsp reset clipped", move || {
            dev.reset_clipped_samples()
        })
        .await
    }

    /// Uploads and immediately applies a YAML configuration string on `CamillaDSP`.
    ///
    /// # Errors
    /// Returns [`DspError`] if the WebSocket request fails or `CamillaDSP` rejects the config.
    pub fn set_config(&self, config_yaml: &str) -> Result<(), DspError> {
        match &*self.backend {
            Backend::Mock { mock, .. } => mock.set_config_yaml(config_yaml),
            Backend::Hardware { host, port } => {
                let host_clone = host.clone();
                let port_val = *port;
                let yaml_owned = config_yaml.to_owned();
                let handle = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| DspError::invalid_state(e.to_string()))?;
                    rt.block_on(Self::send_hardware_unit_cmd(
                        host_clone,
                        port_val,
                        WsCommand::SetConfig { value: yaml_owned },
                    ))
                });
                handle
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            }
        }
    }

    /// Uploads and immediately applies a YAML configuration string asynchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if the WebSocket request fails or `CamillaDSP` rejects the config.
    pub async fn set_config_async(&self, config_yaml: &str) -> Result<(), DspError> {
        let dev = self.clone();
        let yaml_owned = config_yaml.to_owned();
        speaker_common::runtime::run_blocking("dsp set config", move || dev.set_config(&yaml_owned))
            .await
    }

    /// Uploads and immediately applies a JSON configuration string on `CamillaDSP`.
    ///
    /// # Errors
    /// Returns [`DspError`] if the WebSocket request fails or `CamillaDSP` rejects the config.
    pub fn set_config_json(&self, config_json: &str) -> Result<(), DspError> {
        match &*self.backend {
            Backend::Mock { mock, .. } => mock.set_config_json(config_json),
            Backend::Hardware { host, port } => {
                let host_clone = host.clone();
                let port_val = *port;
                let json_owned = config_json.to_owned();
                let handle = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| DspError::invalid_state(e.to_string()))?;
                    rt.block_on(Self::send_hardware_unit_cmd(
                        host_clone,
                        port_val,
                        WsCommand::SetConfigJson { value: json_owned },
                    ))
                });
                handle
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            }
        }
    }

    /// Uploads and immediately applies a JSON configuration string asynchronously.
    ///
    /// # Errors
    /// Returns [`DspError`] if the WebSocket request fails or `CamillaDSP` rejects the config.
    pub async fn set_config_json_async(&self, config_json: &str) -> Result<(), DspError> {
        let dev = self.clone();
        let json_owned = config_json.to_owned();
        speaker_common::runtime::run_blocking("dsp set config json", move || {
            dev.set_config_json(&json_owned)
        })
        .await
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

    async fn send_ws_command<S>(ws: &mut S, cmd: &WsCommand) -> Result<WsReply, DspError>
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
            + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        let cmd_name = cmd.name();
        let payload =
            serde_json::to_string(cmd).map_err(|e| DspError::protocol_error(e.to_string()))?;
        ws.send(Message::Text(payload.into()))
            .await
            .map_err(|e| DspError::protocol_error(format!("failed to send '{cmd_name}': {e}")))?;

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
            .map_err(|_timeout_err| {
                DspError::timeout(format!("command '{cmd_name}' timed out"))
            })??;

        let reply: WsReply = serde_json::from_str(&text).map_err(|e| {
            DspError::protocol_error(format!("invalid JSON in response to '{cmd_name}': {e}"))
        })?;

        if let WsReply::Invalid { error } = reply {
            return Err(DspError::command_failed(cmd_name, error));
        }

        Ok(reply)
    }

    async fn optional_ws_query<S>(ws: &mut S, cmd: &WsCommand) -> Option<WsReply>
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
            + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        match Self::send_ws_command(ws, cmd).await {
            Ok(reply) => Some(reply),
            Err(e) => {
                tracing::debug!(
                    command = cmd.name(),
                    error = %e,
                    "optional DSP telemetry query failed; skipping field"
                );
                None
            }
        }
    }

    async fn reset_hardware_clipped(host: DspHost, port: DspPort) -> Result<(), DspError> {
        Self::send_hardware_unit_cmd(host, port, WsCommand::ResetClippedSamples).await
    }

    async fn send_hardware_unit_cmd(
        host: DspHost,
        port: DspPort,
        cmd: WsCommand,
    ) -> Result<(), DspError> {
        let cmd_name = cmd.name();
        let mut ws = Self::connect_ws(&host, port).await?;
        let reply = Self::send_ws_command(&mut ws, &cmd).await?;
        let _ = ws.close(None).await;
        match reply {
            WsReply::ResetClippedSamples { result }
            | WsReply::SetConfig { result }
            | WsReply::SetConfigJson { result } => check_ws_result(cmd_name, result),
            other => Err(DspError::protocol_error(format!(
                "unexpected reply '{}' for '{cmd_name}'",
                other.name()
            ))),
        }
    }

    async fn query_optional_metadata<S>(ws: &mut S) -> OptionalTelemetry
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
            + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        let version = match Self::optional_ws_query(ws, &WsCommand::GetVersion).await {
            Some(WsReply::GetVersion {
                result: WsResult::Ok,
                value,
            }) => Some(value.into_boxed_str()),
            _ => None,
        };
        let config_title = match Self::optional_ws_query(ws, &WsCommand::GetConfigTitle).await {
            Some(WsReply::GetConfigTitle {
                result: WsResult::Ok,
                value,
            }) => Some(value.into_boxed_str()),
            _ => None,
        };
        let config_path = match Self::optional_ws_query(ws, &WsCommand::GetConfigFilePath).await {
            Some(WsReply::GetConfigFilePath {
                result: WsResult::Ok,
                value: Some(p),
            }) => Some(p.into_boxed_str()),
            _ => None,
        };
        let config = match Self::optional_ws_query(ws, &WsCommand::GetConfigJson).await {
            Some(WsReply::GetConfigJson {
                result: WsResult::Ok,
                value,
            }) => parse_config_json_str(&value),
            _ => None,
        };
        let resampler_load = match Self::optional_ws_query(ws, &WsCommand::GetResamplerLoad).await {
            Some(WsReply::GetResamplerLoad {
                result: WsResult::Ok,
                value,
            }) => Some(value),
            _ => None,
        };
        let capture_rate = match Self::optional_ws_query(ws, &WsCommand::GetCaptureRate).await {
            Some(WsReply::GetCaptureRate {
                result: WsResult::Ok,
                value,
            }) => u32::try_from(value).ok(),
            _ => None,
        };
        let volume_db = match Self::optional_ws_query(ws, &WsCommand::GetVolume).await {
            Some(WsReply::GetVolume {
                result: WsResult::Ok,
                value,
            }) => Some(value),
            _ => None,
        };
        let muted = match Self::optional_ws_query(ws, &WsCommand::GetMute).await {
            Some(WsReply::GetMute {
                result: WsResult::Ok,
                value,
            }) => Some(value),
            _ => None,
        };
        OptionalTelemetry {
            version,
            config_title,
            config_path,
            config,
            resampler_load,
            capture_rate,
            volume_db,
            muted,
        }
    }

    async fn query_hardware_status(
        host: DspHost,
        port: DspPort,
        service: ServiceState,
    ) -> Result<DspStatus, DspError> {
        let mut ws = Self::connect_ws(&host, port).await?;

        let opt = Self::query_optional_metadata(&mut ws).await;

        let state = match Self::send_ws_command(&mut ws, &WsCommand::GetState).await? {
            WsReply::GetState { result, value } => {
                check_ws_result("GetState", result)?;
                ProcessingState::from(value)
            }
            other => return Err(unexpected_reply("GetState", &other)),
        };
        let processing_load =
            match Self::send_ws_command(&mut ws, &WsCommand::GetProcessingLoad).await? {
                WsReply::GetProcessingLoad { result, value } => {
                    check_ws_result("GetProcessingLoad", result)?;
                    value
                }
                other => return Err(unexpected_reply("GetProcessingLoad", &other)),
            };
        let rate_adjust = match Self::send_ws_command(&mut ws, &WsCommand::GetRateAdjust).await? {
            WsReply::GetRateAdjust { result, value } => {
                check_ws_result("GetRateAdjust", result)?;
                f64::from(value)
            }
            other => return Err(unexpected_reply("GetRateAdjust", &other)),
        };
        let buffer_level = match Self::send_ws_command(&mut ws, &WsCommand::GetBufferLevel).await? {
            WsReply::GetBufferLevel { result, value } => {
                check_ws_result("GetBufferLevel", result)?;
                u32::try_from(value).map_err(|e| {
                    DspError::protocol_error(format!("GetBufferLevel exceeded u32 range: {e}"))
                })?
            }
            other => return Err(unexpected_reply("GetBufferLevel", &other)),
        };
        let clipped_samples =
            match Self::send_ws_command(&mut ws, &WsCommand::GetClippedSamples).await? {
                WsReply::GetClippedSamples { result, value } => {
                    check_ws_result("GetClippedSamples", result)?;
                    value as u64
                }
                other => return Err(unexpected_reply("GetClippedSamples", &other)),
            };
        let stop_reason = match Self::send_ws_command(&mut ws, &WsCommand::GetStopReason).await? {
            WsReply::GetStopReason { result, value } => {
                check_ws_result("GetStopReason", result)?;
                StopReason::from(value)
            }
            other => return Err(unexpected_reply("GetStopReason", &other)),
        };

        let (capture_peaks, capture_rms, playback_peaks, playback_rms) =
            Self::query_signal_levels(&mut ws).await;
        let _ = ws.close(None).await;

        let buffer_capacity = opt
            .config
            .as_ref()
            .and_then(|c| c.chunksize)
            .unwrap_or(DEFAULT_DSP_BUFFER_CAPACITY);

        Ok(DspStatus {
            service,
            host,
            port,
            version: opt.version,
            config_title: opt.config_title,
            config_path: opt.config_path,
            config: opt.config,
            capture_rate: opt.capture_rate,
            state,
            processing_load,
            resampler_load: opt.resampler_load,
            rate_adjust,
            buffer_level,
            buffer_capacity,
            volume_db: opt.volume_db,
            muted: opt.muted,
            clipped_samples,
            stop_reason,
            capture: DspChannelLevels::new(capture_peaks, capture_rms),
            playback: DspChannelLevels::new(playback_peaks, playback_rms),
        })
    }

    async fn query_signal_levels<S>(ws: &mut S) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>)
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
            + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        if let Ok(WsReply::GetSignalLevels {
            result: WsResult::Ok,
            value: levels,
        }) = Self::send_ws_command(ws, &WsCommand::GetSignalLevels).await
        {
            return (
                levels.capture_peak,
                levels.capture_rms,
                levels.playback_peak,
                levels.playback_rms,
            );
        }
        let cap_p = match Self::optional_ws_query(ws, &WsCommand::GetCaptureSignalPeak).await {
            Some(WsReply::GetCaptureSignalPeak {
                result: WsResult::Ok,
                value,
            }) => value,
            _ => Vec::new(),
        };
        let cap_r = match Self::optional_ws_query(ws, &WsCommand::GetCaptureSignalRms).await {
            Some(WsReply::GetCaptureSignalRms {
                result: WsResult::Ok,
                value,
            }) => value,
            _ => Vec::new(),
        };
        let play_p = match Self::optional_ws_query(ws, &WsCommand::GetPlaybackSignalPeak).await {
            Some(WsReply::GetPlaybackSignalPeak {
                result: WsResult::Ok,
                value,
            }) => value,
            _ => Vec::new(),
        };
        let play_r = match Self::optional_ws_query(ws, &WsCommand::GetPlaybackSignalRms).await {
            Some(WsReply::GetPlaybackSignalRms {
                result: WsResult::Ok,
                value,
            }) => value,
            _ => Vec::new(),
        };
        (cap_p, cap_r, play_p, play_r)
    }
}

impl From<SchemaProcessingState> for ProcessingState {
    fn from(value: SchemaProcessingState) -> Self {
        match value {
            SchemaProcessingState::Running => Self::Running,
            SchemaProcessingState::Paused => Self::Paused,
            SchemaProcessingState::Inactive => Self::Inactive,
            SchemaProcessingState::Starting => Self::Starting,
            SchemaProcessingState::Stalled => Self::Stalled,
        }
    }
}

impl From<SchemaStopReason> for StopReason {
    fn from(value: SchemaStopReason) -> Self {
        match value {
            SchemaStopReason::None => Self::None,
            SchemaStopReason::Done => Self::Done,
            SchemaStopReason::CaptureError(_) => Self::CaptureError,
            SchemaStopReason::PlaybackError(_) => Self::PlaybackError,
            SchemaStopReason::CaptureFormatChange(_) => Self::CaptureFormatChange,
            SchemaStopReason::PlaybackFormatChange(_) => Self::PlaybackFormatChange,
            SchemaStopReason::UnknownError(msg) => Self::Unknown(msg.into_boxed_str()),
        }
    }
}

struct OptionalTelemetry {
    version: Option<Box<str>>,
    config_title: Option<Box<str>>,
    config_path: Option<Box<str>>,
    config: Option<DspConfigInfo>,
    resampler_load: Option<f32>,
    capture_rate: Option<u32>,
    volume_db: Option<f32>,
    muted: Option<bool>,
}

fn unexpected_reply(cmd: &str, reply: &WsReply) -> DspError {
    DspError::protocol_error(format!("unexpected reply '{}' for '{cmd}'", reply.name()))
}

fn check_ws_result(cmd: &str, result: WsResult) -> Result<(), DspError> {
    match result {
        WsResult::Ok => Ok(()),
        WsResult::ConfigValidationError { message }
        | WsResult::ConfigReadError { message }
        | WsResult::InvalidValueError { message }
        | WsResult::InvalidRequestError { message }
        | WsResult::DeviceNotFoundError { message }
        | WsResult::DeviceBusyError { message }
        | WsResult::DeviceError { message } => Err(DspError::command_failed(cmd, message)),
        other => Err(DspError::command_failed(cmd, format!("{other:?}"))),
    }
}

fn parse_config_json_str(raw: &str) -> Option<DspConfigInfo> {
    let cfg: Configuration = serde_json::from_str(raw).ok()?;
    let (capture_device, capture_format) = match cfg.devices.capture {
        CaptureDevice::Alsa { device, format, .. } => (
            Some(device.into_boxed_str()),
            format.map(|f| format!("{f:?}").into_boxed_str()),
        ),
        other => (Some(other.type_name().into()), None),
    };
    let (playback_device, playback_format) = match cfg.devices.playback {
        PlaybackDevice::Alsa { device, format, .. } => (
            Some(device.into_boxed_str()),
            format.map(|f| format!("{f:?}").into_boxed_str()),
        ),
        other => (Some(other.type_name().into()), None),
    };
    Some(DspConfigInfo {
        samplerate: u32::try_from(cfg.devices.samplerate.get()).ok(),
        chunksize: u32::try_from(cfg.devices.chunksize.get()).ok(),
        capture_device,
        capture_format,
        playback_device,
        playback_format,
        mixers: cfg
            .mixers
            .map(|m| m.into_keys().map(String::into_boxed_str).collect())
            .unwrap_or_default(),
        filters: cfg
            .filters
            .map(|f| f.into_keys().map(String::into_boxed_str).collect())
            .unwrap_or_default(),
        processors: cfg
            .processors
            .map(|p| p.into_keys().map(String::into_boxed_str).collect())
            .unwrap_or_default(),
        pipeline_stages: cfg.pipeline.as_ref().map_or(0, Vec::len),
    })
}

#[cfg(feature = "test-util")]
fn parse_config_json(val: &serde_json::Value) -> Option<DspConfigInfo> {
    parse_config_json_str(val.as_str()?)
}

#[cfg(feature = "test-util")]
fn parse_f32_list(val: &serde_json::Value) -> Vec<f32> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "test-util")]
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

#[cfg(feature = "test-util")]
pub mod test_helpers {
    use super::{DspConfigInfo, DspError, parse_config_json, parse_f32_list, parse_levels_object};
    use speaker_common::runtime::FromJoinError;

    /// Helper for testing `GetConfigJson` parsing.
    #[must_use]
    pub fn helper_parse_config_json(val: &serde_json::Value) -> Option<DspConfigInfo> {
        parse_config_json(val)
    }

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
