//! Async MPD / `rmpd` client and mockable peripheral abstraction.
//!
//! Provides strongly typed, non-blocking interfaces for controlling and
//! observing an MPD-compatible daemon (`rmpd` on `127.0.0.1:6600`) across
//! workspace services (`audio-manager` for source handover and volume sync;
//! `screen-manager` for Now Playing metadata and transport controls).
//!
//! Conforms to Microsoft Pragmatic Rust Guidelines:
//! - Strongly typed domain models ([`MpdHost`], [`MpdPort`], [`MpdPlaybackState`], [`MpdStatus`])
//! - Inherent mocking via [`MpdDevice::new_mocked`] and [`MpdDeviceMock`]
//! - Cheaply cloneable (`Arc`-backed) `Send + Sync` [`MpdDevice`] handle
//! - Canonical error struct [`MpdError`] with query predicates and single-line `summary()`

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mpd_client::Client;
use mpd_client::client::{ConnectionEvent, Subsystem};
use mpd_client::commands::{
    AlbumArt, AlbumArtEmbedded, CurrentSong, Next, Play, Previous, Seek, SeekMode, SetPause,
    SetRandom, SetRepeat, SetVolume, Status, Stop,
};
use mpd_client::responses::PlayState;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};

/// Default MPD TCP host address.
pub const DEFAULT_MPD_HOST: &str = "127.0.0.1";

/// Default MPD TCP port number.
pub const DEFAULT_MPD_PORT: u16 = 6600;

/// Minimum valid TCP port number.
pub const MIN_MPD_PORT: u16 = 1;

/// Maximum valid TCP port number.
pub const MAX_MPD_PORT: u16 = 65535;

/// Default bounded timeout for MPD TCP connect and command round-trips.
pub const DEFAULT_MPD_TIMEOUT: Duration = Duration::from_secs(2);

const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Strong newtype wrapping a validated MPD host address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MpdHost(Box<str>);

impl<'de> Deserialize<'de> for MpdHost {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_str(&raw).map_err(serde::de::Error::custom)
    }
}

impl Default for MpdHost {
    fn default() -> Self {
        Self(DEFAULT_MPD_HOST.into())
    }
}

impl Display for MpdHost {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl MpdHost {
    /// Validates and constructs an [`MpdHost`] from a string slice.
    ///
    /// # Errors
    /// Returns [`MpdError`] if the host is empty, contains whitespace, or is not a valid IP/hostname.
    pub fn new(host: &str) -> Result<Self, MpdError> {
        Self::parse_str(host).map_err(MpdError::invalid_state)
    }

    /// Parses and validates a host string for CLI and config inputs.
    ///
    /// # Errors
    /// Returns a human-readable validation error message when invalid.
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
        if trimmed.parse::<IpAddr>().is_ok() {
            return Ok(Self(trimmed.into()));
        }
        if trimmed.len() <= 253
            && trimmed.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        {
            return Ok(Self(trimmed.into()));
        }
        Err(format!("invalid host format '{trimmed}'"))
    }

    /// Returns the host string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` if the host refers to the local loopback interface.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        let bare = self.0.trim_start_matches('[').trim_end_matches(']');
        bare.eq_ignore_ascii_case("localhost")
            || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }
}

impl TryFrom<&str> for MpdHost {
    type Error = MpdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Strong newtype wrapping a validated non-zero MPD TCP port (`1..=65535`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MpdPort(u16);

impl<'de> Deserialize<'de> for MpdPort {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u16::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl Default for MpdPort {
    fn default() -> Self {
        Self(DEFAULT_MPD_PORT)
    }
}

impl Display for MpdPort {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl MpdPort {
    /// Validates and constructs an [`MpdPort`].
    ///
    /// # Errors
    /// Returns [`MpdError`] if `port == 0`.
    pub fn new(port: u16) -> Result<Self, MpdError> {
        if port < MIN_MPD_PORT {
            return Err(MpdError::invalid_state(format!(
                "MPD TCP port must be between {MIN_MPD_PORT} and {MAX_MPD_PORT}, got {port}"
            )));
        }
        Ok(Self(port))
    }

    /// Parses a decimal TCP port string for CLI and config inputs.
    ///
    /// # Errors
    /// Returns a descriptive string error when parsing or range validation fails.
    pub fn parse_str(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid TCP port '{trimmed}'"));
        }
        let port: u16 = match trimmed.parse() {
            Ok(p) => p,
            Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => {
                return Err(format!(
                    "TCP port must be between {MIN_MPD_PORT} and {MAX_MPD_PORT}"
                ));
            }
            Err(e) => return Err(format!("invalid TCP port '{trimmed}': {e}")),
        };
        if port < MIN_MPD_PORT {
            return Err(format!(
                "TCP port must be between {MIN_MPD_PORT} and {MAX_MPD_PORT}"
            ));
        }
        Ok(Self(port))
    }

    /// Returns the raw `u16` port value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for MpdPort {
    type Error = MpdError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Playback state reported by MPD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum MpdPlaybackState {
    /// Playback is stopped.
    #[default]
    Stopped,
    /// Playback is paused.
    Paused,
    /// Playback is active.
    Playing,
}

impl MpdPlaybackState {
    /// Returns `true` when MPD is actively playing.
    #[must_use]
    pub const fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }

    /// Returns `true` when a track is loaded (`Playing` or `Paused`).
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Playing | Self::Paused)
    }
}

impl Display for MpdPlaybackState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Paused => "paused",
            Self::Playing => "playing",
        })
    }
}

impl From<PlayState> for MpdPlaybackState {
    fn from(state: PlayState) -> Self {
        match state {
            PlayState::Stopped => Self::Stopped,
            PlayState::Paused => Self::Paused,
            PlayState::Playing => Self::Playing,
        }
    }
}

/// Metadata summary for the currently loaded MPD track.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MpdTrack {
    /// Track file path or stream URI.
    pub file: Box<str>,
    /// Track title tag, if present.
    pub title: Option<Box<str>>,
    /// Primary artist tag, if present.
    pub artist: Option<Box<str>>,
    /// Album tag, if present.
    pub album: Option<Box<str>>,
    /// Radio station or stream `Name` tag, if present.
    #[serde(default)]
    pub station_name: Option<Box<str>>,
    /// Track duration, if known.
    pub duration: Option<Duration>,
}

impl MpdTrack {
    /// Returns `true` when `file` is an HTTP or HTTPS stream URI.
    #[must_use]
    pub fn is_stream_url(&self) -> bool {
        let trimmed = self.file.trim_start();
        trimmed
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
            || trimmed
                .get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    }

    /// Resolves `(artist, title)`, splitting ICY `"Artist - Title"` strings when
    /// `artist` is missing on an HTTP/HTTPS stream.
    #[must_use]
    pub fn resolved_artist_title(&self) -> (Option<&str>, Option<&str>) {
        let artist = self.artist.as_deref().filter(|s| !s.trim().is_empty());
        let title = self.title.as_deref().filter(|s| !s.trim().is_empty());
        if artist.is_some() {
            return (artist, title);
        }
        if self.is_stream_url()
            && let Some(raw_title) = title
            && let Some((left, right)) = raw_title.split_once(" - ")
        {
            let left_trimmed = left.trim();
            let right_trimmed = right.trim();
            if !left_trimmed.is_empty() && !right_trimmed.is_empty() {
                return (Some(left_trimmed), Some(right_trimmed));
            }
        }
        (None, title)
    }

    /// Infers a short codec/container badge label from the track URI or file extension.
    #[must_use]
    pub fn codec_label(&self) -> &'static str {
        self.codec_label_with_mime(None)
    }

    /// Infers a short codec/container badge label from an optional HTTP `Content-Type`
    /// header, falling back to the track URI or file extension.
    #[must_use]
    pub fn codec_label_with_mime(&self, content_type: Option<&str>) -> &'static str {
        if let Some(mime) = content_type.and_then(codec_from_mime) {
            return mime;
        }
        let path_part = self.file.split(['?', '#']).next().unwrap_or(&self.file);
        let ext = path_part
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "flac" => "FLAC",
            "mp3" => "MP3",
            "opus" => "OPUS",
            "ogg" | "oga" => "OGG",
            "aac" | "aacp" | "m4a" | "mp4" => "AAC",
            "alac" => "ALAC",
            "wav" => "WAV",
            "aiff" | "aif" => "AIFF",
            "dsf" | "dff" => "DSD",
            "wv" => "WV",
            "m3u8" => "HLS",
            _ => {
                if self.is_stream_url() {
                    codec_from_stream_path(path_part).unwrap_or("PCM")
                } else {
                    "PCM"
                }
            }
        }
    }
}

/// Maps an HTTP `Content-Type` header value to a short codec badge label.
#[must_use]
pub fn codec_from_mime(content_type: &str) -> Option<&'static str> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "audio/mpeg" | "audio/mp3" => Some("MP3"),
        "audio/aac" | "audio/aacp" | "audio/mp4" | "audio/x-aac" => Some("AAC"),
        "audio/flac" | "audio/x-flac" => Some("FLAC"),
        "audio/opus" => Some("OPUS"),
        "application/ogg" | "audio/ogg" | "audio/vorbis" => Some("OGG"),
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => Some("HLS"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("WAV"),
        _ => None,
    }
}

fn codec_from_stream_path(path_part: &str) -> Option<&'static str> {
    let lower = path_part.to_ascii_lowercase();
    let tail = lower.rsplit('/').next().unwrap_or(&lower);
    if tail.contains("flac") {
        Some("FLAC")
    } else if tail.contains("opus") {
        Some("OPUS")
    } else if tail.contains("aacp") || tail.contains("aac") {
        Some("AAC")
    } else if tail.contains("mp3") {
        Some("MP3")
    } else if tail.contains("ogg") {
        Some("OGG")
    } else if tail.contains("hls") {
        Some("HLS")
    } else {
        None
    }
}

/// Snapshot of MPD daemon playback and queue status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MpdStatus {
    /// Connected MPD host.
    pub host: MpdHost,
    /// Connected MPD port.
    pub port: MpdPort,
    /// Negotiated MPD protocol version string.
    pub protocol_version: Box<str>,
    /// Current playback state.
    pub state: MpdPlaybackState,
    /// Mixer volume percentage (`0..=100`), when reported.
    pub volume: Option<u8>,
    /// Elapsed playback position in the current track, if any.
    pub elapsed: Option<Duration>,
    /// Total duration of the current track, if known.
    pub duration: Option<Duration>,
    /// Metadata for the current track, if one is loaded.
    pub current_track: Option<MpdTrack>,
    /// Whether repeat mode is enabled.
    pub repeat: bool,
    /// Whether random (shuffle) mode is enabled.
    pub random: bool,
    /// Instantaneous stream bitrate in kbps, if reported.
    pub bitrate_kbps: Option<u32>,
    /// Decoded PCM sample rate in Hz (`audio` line), if reported.
    pub sample_rate_hz: Option<u32>,
    /// Decoded PCM bit depth (`audio` line), if reported.
    pub bit_depth: Option<u8>,
    /// Decoded PCM channel count (`audio` line), if reported.
    pub channels: Option<u8>,
}

/// MPD subsystem identifier emitted on state changes (`idle`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MpdSubsystem {
    /// Playback state or current song changed.
    Player,
    /// Mixer volume changed.
    Mixer,
    /// Play queue changed.
    Queue,
    /// Playback options (repeat/random/consume/single) changed.
    Options,
    /// Audio outputs changed.
    Output,
    /// Any other MPD subsystem name.
    Other(Box<str>),
}

impl From<Subsystem> for MpdSubsystem {
    fn from(sub: Subsystem) -> Self {
        match sub {
            Subsystem::Player => Self::Player,
            Subsystem::Mixer => Self::Mixer,
            Subsystem::Queue => Self::Queue,
            Subsystem::Options => Self::Options,
            Subsystem::Output => Self::Output,
            other => Self::Other(other.as_str().into()),
        }
    }
}

/// Push event emitted by an MPD `idle` subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpdEvent {
    /// One MPD subsystem reported a change.
    SubsystemChanged(MpdSubsystem),
    /// The underlying MPD event stream closed with a reason.
    Disconnected(Box<str>),
}

/// Receiver stream for MPD push events (`MpdDevice::subscribe_events`).
#[derive(Debug)]
pub struct MpdEventStream {
    rx: mpsc::Receiver<MpdEvent>,
}

impl MpdEventStream {
    /// Receives the next [`MpdEvent`], or `None` when the background stream stops.
    pub async fn recv(&mut self) -> Option<MpdEvent> {
        self.rx.recv().await
    }

    /// Attempts to receive a queued [`MpdEvent`] without awaiting.
    ///
    /// # Errors
    /// Returns [`mpsc::error::TryRecvError`] when empty or disconnected.
    pub fn try_recv(&mut self) -> Result<MpdEvent, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

/// Command recorded by [`MpdDeviceMock`] for test verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpdMockCommand {
    /// `pause 1`
    Pause,
    /// `pause 0`
    Resume,
    /// `play`
    Play,
    /// `stop`
    Stop,
    /// `next`
    Next,
    /// `previous`
    Previous,
    /// `setvol <percent>`
    SetVolume(u8),
    /// `seekcur <seconds>`
    Seek(Duration),
    /// `repeat <0|1>`
    SetRepeat(bool),
    /// `random <0|1>`
    SetRandom(bool),
}

/// Internal error cause for MPD client operations.
#[derive(Debug)]
pub(crate) enum MpdErrorKind {
    ConnectionFailed {
        host: Box<str>,
        port: u16,
        source: std::io::Error,
    },
    ProtocolError {
        message: Box<str>,
    },
    Timeout {
        message: Box<str>,
    },
    CommandFailed {
        command: Box<str>,
        message: Box<str>,
    },
    InvalidState {
        message: Box<str>,
    },
}

/// Canonical error struct for MPD peripheral failures.
#[derive(Debug)]
pub struct MpdError {
    pub(crate) kind: MpdErrorKind,
    pub(crate) backtrace: Backtrace,
}

impl MpdError {
    pub(crate) fn connection_failed(
        host: impl Into<Box<str>>,
        port: u16,
        source: std::io::Error,
    ) -> Self {
        Self {
            kind: MpdErrorKind::ConnectionFailed {
                host: host.into(),
                port,
                source,
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn protocol_error(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: MpdErrorKind::ProtocolError {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn timeout(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: MpdErrorKind::Timeout {
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
            kind: MpdErrorKind::CommandFailed {
                command: command.into(),
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn invalid_state(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: MpdErrorKind::InvalidState {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns `true` when establishing or maintaining the TCP connection failed.
    #[must_use]
    pub const fn is_connection_failed(&self) -> bool {
        matches!(self.kind, MpdErrorKind::ConnectionFailed { .. })
    }

    /// Returns `true` when MPD protocol framing or response decoding failed.
    #[must_use]
    pub const fn is_protocol_error(&self) -> bool {
        matches!(self.kind, MpdErrorKind::ProtocolError { .. })
    }

    /// Returns `true` when the operation exceeded the configured timeout.
    #[must_use]
    pub const fn is_timeout(&self) -> bool {
        matches!(self.kind, MpdErrorKind::Timeout { .. })
    }

    /// Returns `true` when MPD returned an explicit command error (`ACK`).
    #[must_use]
    pub const fn is_command_failed(&self) -> bool {
        matches!(self.kind, MpdErrorKind::CommandFailed { .. })
    }

    /// Returns `true` when a caller-provided argument or state invariant failed.
    #[must_use]
    pub const fn is_invalid_state(&self) -> bool {
        matches!(self.kind, MpdErrorKind::InvalidState { .. })
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            MpdErrorKind::ConnectionFailed { host, port, source } => {
                write!(f, "MPD connection to {host}:{port} failed: {source}")
            }
            MpdErrorKind::ProtocolError { message } => write!(f, "MPD protocol error: {message}"),
            MpdErrorKind::Timeout { message } => write!(f, "MPD timeout: {message}"),
            MpdErrorKind::CommandFailed { command, message } => {
                write!(f, "MPD command '{command}' failed: {message}")
            }
            MpdErrorKind::InvalidState { message } => write!(f, "invalid MPD state: {message}"),
        }
    }
}

speaker_common::impl_canonical_error_display!(MpdError);

impl std::error::Error for MpdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            MpdErrorKind::ConnectionFailed { source, .. } => Some(source),
            MpdErrorKind::ProtocolError { .. }
            | MpdErrorKind::Timeout { .. }
            | MpdErrorKind::CommandFailed { .. }
            | MpdErrorKind::InvalidState { .. } => None,
        }
    }
}

#[derive(Debug)]
struct MockState {
    host: MpdHost,
    port: MpdPort,
    protocol_version: Box<str>,
    state: MpdPlaybackState,
    volume: Option<u8>,
    elapsed: Option<Duration>,
    duration: Option<Duration>,
    current_track: Option<MpdTrack>,
    repeat: bool,
    random: bool,
    audio_format: Option<(u32, u8, u8)>,
    album_art: HashMap<Box<str>, Vec<u8>>,
    commands: Vec<MpdMockCommand>,
    injected_fault: Option<Box<str>>,
    events: broadcast::Sender<MpdEvent>,
}

/// Companion controller for [`MpdDevice::new_mocked`].
#[derive(Debug, Clone)]
pub struct MpdDeviceMock {
    inner: Arc<Mutex<MockState>>,
}

impl MpdDeviceMock {
    /// Creates a mock controller initialized to idle stopped defaults.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(MockState {
                host: MpdHost::default(),
                port: MpdPort::default(),
                protocol_version: "0.24.0".into(),
                state: MpdPlaybackState::Stopped,
                volume: None,
                elapsed: None,
                duration: None,
                current_track: None,
                repeat: false,
                random: false,
                audio_format: None,
                album_art: HashMap::new(),
                commands: Vec::new(),
                injected_fault: None,
                events: tx,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sets the mock playback state.
    pub fn set_state(&self, state: MpdPlaybackState) {
        self.lock().state = state;
    }

    /// Sets the mock mixer volume percentage (`None` simulates `mixer_type = "none"`).
    pub fn set_volume(&self, volume: Option<u8>) {
        self.lock().volume = volume.map(|v| v.min(100));
    }

    /// Sets the mock track metadata and timing.
    pub fn set_track(
        &self,
        track: Option<MpdTrack>,
        elapsed: Option<Duration>,
        duration: Option<Duration>,
    ) {
        let mut g = self.lock();
        g.current_track = track;
        g.elapsed = elapsed;
        g.duration = duration;
    }

    /// Sets the mock repeat flag.
    pub fn set_repeat(&self, enabled: bool) {
        self.lock().repeat = enabled;
    }

    /// Sets the mock random (shuffle) flag.
    pub fn set_random(&self, enabled: bool) {
        self.lock().random = enabled;
    }

    /// Sets the mock decoded PCM audio format `(sample_rate_hz, bit_depth, channels)`.
    pub fn set_audio_format(&self, format: Option<(u32, u8, u8)>) {
        self.lock().audio_format = format;
    }

    /// Sets or clears mock album art bytes for `file`.
    pub fn set_album_art(&self, file: &str, bytes: Option<Vec<u8>>) {
        let mut g = self.lock();
        if let Some(data) = bytes {
            g.album_art.insert(Box::from(file), data);
        } else {
            g.album_art.remove(file);
        }
    }

    /// Injects a fault message returned by subsequent [`MpdDevice`] operations.
    pub fn inject_fault(&self, message: impl Into<Box<str>>) {
        self.lock().injected_fault = Some(message.into());
    }

    /// Clears any previously injected fault.
    pub fn clear_fault(&self) {
        self.lock().injected_fault = None;
    }

    /// Returns every command recorded on the mock device.
    #[must_use]
    pub fn commands(&self) -> Vec<MpdMockCommand> {
        self.lock().commands.clone()
    }

    /// Clears the recorded command history.
    pub fn clear_commands(&self) {
        self.lock().commands.clear();
    }

    /// Broadcasts a synthetic subsystem change event to active [`MpdEventStream`] subscribers.
    pub fn emit_event(&self, event: MpdEvent) {
        let _ = self.lock().events.send(event);
    }
}

impl Default for MpdDeviceMock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
enum Backend {
    Tcp {
        host: MpdHost,
        port: MpdPort,
        timeout: Duration,
        client: tokio::sync::Mutex<Option<Client>>,
    },
    Mock(Arc<Mutex<MockState>>),
}

/// Async MPD (`rmpd`) client handle with inherent mock support.
///
/// Cheaply cloneable (`Arc`-backed) and `Send + Sync`.
#[derive(Debug, Clone)]
pub struct MpdDevice {
    inner: Arc<Backend>,
}

impl MpdDevice {
    /// Creates an MPD client targeting the default appliance endpoint (`127.0.0.1:6600`).
    #[must_use]
    #[expect(
        clippy::new_without_default,
        reason = "external I/O client must be constructed explicitly via new/with_endpoint/new_mocked"
    )]
    pub fn new() -> Self {
        Self::with_endpoint(MpdHost::default(), MpdPort::default())
    }

    /// Creates an MPD client targeting a validated host and TCP port.
    #[must_use]
    pub fn with_endpoint(host: MpdHost, port: MpdPort) -> Self {
        Self {
            inner: Arc::new(Backend::Tcp {
                host,
                port,
                timeout: DEFAULT_MPD_TIMEOUT,
                client: tokio::sync::Mutex::new(None),
            }),
        }
    }

    /// Returns a clone of this handle with a custom command timeout (for TCP backends).
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        match &*self.inner {
            Backend::Tcp { host, port, .. } => Self {
                inner: Arc::new(Backend::Tcp {
                    host: host.clone(),
                    port: *port,
                    timeout,
                    client: tokio::sync::Mutex::new(None),
                }),
            },
            Backend::Mock(mock) => Self {
                inner: Arc::new(Backend::Mock(Arc::clone(mock))),
            },
        }
    }

    /// Creates a deterministic mock MPD device paired with its controller (`M-MOCKABLE-SYSCALLS`).
    #[must_use]
    pub fn new_mocked() -> (Self, MpdDeviceMock) {
        let mock = MpdDeviceMock::new();
        let dev = Self {
            inner: Arc::new(Backend::Mock(Arc::clone(&mock.inner))),
        };
        (dev, mock)
    }

    /// Returns `true` if this handle is backed by an in-memory mock.
    #[must_use]
    pub fn is_mocked(&self) -> bool {
        matches!(&*self.inner, Backend::Mock(_))
    }

    /// Returns the configured MPD host.
    #[must_use]
    pub fn host(&self) -> MpdHost {
        match &*self.inner {
            Backend::Tcp { host, .. } => host.clone(),
            Backend::Mock(mock) => mock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .host
                .clone(),
        }
    }

    /// Returns the configured MPD TCP port.
    #[must_use]
    pub fn port(&self) -> MpdPort {
        match &*self.inner {
            Backend::Tcp { port, .. } => *port,
            Backend::Mock(mock) => {
                mock.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .port
            }
        }
    }

    /// Pauses playback (`pause 1`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn pause(&self) -> Result<(), MpdError> {
        self.exec_simple(
            "pause",
            true,
            MpdMockCommand::Pause,
            |g| g.state = MpdPlaybackState::Paused,
            |client| Box::pin(async move { client.command(SetPause(true)).await }),
        )
        .await
    }

    /// Resumes paused playback (`pause 0`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn resume(&self) -> Result<(), MpdError> {
        self.exec_simple(
            "resume",
            true,
            MpdMockCommand::Resume,
            |g| g.state = MpdPlaybackState::Playing,
            |client| Box::pin(async move { client.command(SetPause(false)).await }),
        )
        .await
    }

    /// Starts or resumes playback of the current queue item (`play`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn play(&self) -> Result<(), MpdError> {
        self.exec_simple(
            "play",
            true,
            MpdMockCommand::Play,
            |g| g.state = MpdPlaybackState::Playing,
            |client| Box::pin(async move { client.command(Play::current()).await }),
        )
        .await
    }

    /// Stops playback (`stop`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn stop(&self) -> Result<(), MpdError> {
        self.exec_simple(
            "stop",
            true,
            MpdMockCommand::Stop,
            |g| g.state = MpdPlaybackState::Stopped,
            |client| Box::pin(async move { client.command(Stop).await }),
        )
        .await
    }

    /// Advances to the next track (`next`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn next(&self) -> Result<(), MpdError> {
        self.exec_simple(
            "next",
            false,
            MpdMockCommand::Next,
            |_| {},
            |client| Box::pin(async move { client.command(Next).await }),
        )
        .await
    }

    /// Returns to the previous track (`previous`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn previous(&self) -> Result<(), MpdError> {
        self.exec_simple(
            "previous",
            false,
            MpdMockCommand::Previous,
            |_| {},
            |client| Box::pin(async move { client.command(Previous).await }),
        )
        .await
    }

    /// Seeks within the currently playing track (`seekcur <seconds>`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn seek(&self, position: Duration) -> Result<(), MpdError> {
        self.exec_simple(
            "seekcur",
            true,
            MpdMockCommand::Seek(position),
            |g| {
                let clamped = g.duration.map_or(position, |dur| position.min(dur));
                g.elapsed = Some(clamped);
            },
            |client| {
                Box::pin(async move { client.command(Seek(SeekMode::Absolute(position))).await })
            },
        )
        .await
    }

    /// Enables or disables MPD repeat mode (`repeat <0|1>`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn set_repeat(&self, enabled: bool) -> Result<(), MpdError> {
        self.exec_simple(
            "repeat",
            true,
            MpdMockCommand::SetRepeat(enabled),
            |g| g.repeat = enabled,
            |client| Box::pin(async move { client.command(SetRepeat(enabled)).await }),
        )
        .await
    }

    /// Enables or disables MPD random (shuffle) mode (`random <0|1>`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting fails, the command times out, or MPD rejects it.
    pub async fn set_random(&self, enabled: bool) -> Result<(), MpdError> {
        self.exec_simple(
            "random",
            true,
            MpdMockCommand::SetRandom(enabled),
            |g| g.random = enabled,
            |client| Box::pin(async move { client.command(SetRandom(enabled)).await }),
        )
        .await
    }

    /// Reads album art bytes for `file`, trying `albumart` first and falling back to `readpicture`.
    ///
    /// Returns `Ok(None)` when neither external cover art nor embedded track art is available.
    ///
    /// # Errors
    /// Returns [`MpdError`] on TCP/timeout/protocol failure or if the image exceeds 16 MiB.
    pub async fn read_album_art(&self, file: &str) -> Result<Option<Vec<u8>>, MpdError> {
        const MAX_ALBUM_ART_BYTES: usize = 16 * 1024 * 1024;
        if file.is_empty() {
            return Ok(None);
        }
        match &*self.inner {
            Backend::Mock(mock) => {
                let g = mock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(ref fault) = g.injected_fault {
                    return Err(MpdError::protocol_error(fault.clone()));
                }
                Ok(g.album_art.get(file).cloned())
            }
            Backend::Tcp {
                host,
                port,
                timeout,
                client: client_slot,
            } => {
                let client = cached_client(host, *port, *timeout, client_slot).await?;
                if let Some(bytes) =
                    fetch_art_chunks(&client, *timeout, MAX_ALBUM_ART_BYTES, file, false).await?
                {
                    return Ok(Some(bytes));
                }
                fetch_art_chunks(&client, *timeout, MAX_ALBUM_ART_BYTES, file, true).await
            }
        }
    }

    /// Sets the MPD mixer volume (`0..=100`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if `percent > 100`, connection fails, or MPD has no mixer (`mixer_type = "none"`).
    pub async fn set_volume(&self, percent: u8) -> Result<(), MpdError> {
        if percent > 100 {
            return Err(MpdError::invalid_state(format!(
                "MPD volume percent {percent} out of range 0..=100"
            )));
        }
        self.exec_simple(
            "setvol",
            true,
            MpdMockCommand::SetVolume(percent),
            |g| g.volume = Some(percent),
            |client| Box::pin(async move { client.command(SetVolume(percent)).await }),
        )
        .await
    }

    /// Queries only the MPD mixer volume (`status` without `currentsong`) for
    /// low-overhead volume sync loops.
    ///
    /// # Errors
    /// Returns [`MpdError`] on TCP/timeout/protocol failure.
    pub async fn read_volume(&self) -> Result<Option<u8>, MpdError> {
        match &*self.inner {
            Backend::Mock(mock) => {
                let g = mock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(ref fault) = g.injected_fault {
                    return Err(MpdError::protocol_error(fault.clone()));
                }
                Ok(g.volume)
            }
            Backend::Tcp {
                host,
                port,
                timeout,
                client: client_slot,
            } => {
                let client = cached_client(host, *port, *timeout, client_slot).await?;
                let res =
                    match await_timeout(*timeout, "MPD status", client.command(RawStatus)).await {
                        Ok(res) => res,
                        Err(e) => {
                            *client_slot.lock().await = None;
                            return Err(e);
                        }
                    };
                let (_status, volume, _audio) = match res {
                    Ok(ok) => ok,
                    Err(mpd_client::client::CommandError::ConnectionClosed) => {
                        *client_slot.lock().await = None;
                        let fresh = cached_client(host, *port, *timeout, client_slot).await?;
                        match await_timeout(*timeout, "MPD status", fresh.command(RawStatus)).await
                        {
                            Ok(res) => res.map_err(|e| map_command_error("status", e))?,
                            Err(e) => {
                                *client_slot.lock().await = None;
                                return Err(e);
                            }
                        }
                    }
                    Err(e) => return Err(map_command_error("status", e)),
                };
                Ok(volume)
            }
        }
    }

    /// Queries the current playback status and loaded track metadata.
    ///
    /// # Errors
    /// Returns [`MpdError`] on TCP/timeout/protocol failure.
    pub async fn read_status(&self) -> Result<MpdStatus, MpdError> {
        match &*self.inner {
            Backend::Mock(mock) => {
                let g = mock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(ref fault) = g.injected_fault {
                    return Err(MpdError::protocol_error(fault.clone()));
                }
                Ok(MpdStatus {
                    host: g.host.clone(),
                    port: g.port,
                    protocol_version: g.protocol_version.clone(),
                    state: g.state,
                    volume: g.volume,
                    elapsed: g.elapsed,
                    duration: g.duration,
                    current_track: g.current_track.clone(),
                    repeat: g.repeat,
                    random: g.random,
                    bitrate_kbps: None,
                    sample_rate_hz: g.audio_format.map(|a| a.0),
                    bit_depth: g.audio_format.map(|a| a.1),
                    channels: g.audio_format.map(|a| a.2),
                })
            }
            Backend::Tcp {
                host,
                port,
                timeout,
                client: client_slot,
            } => {
                let mut client = cached_client(host, *port, *timeout, client_slot).await?;
                let res = match await_timeout(
                    *timeout,
                    "MPD status",
                    client.command_list((RawStatus, CurrentSong)),
                )
                .await
                {
                    Ok(res) => res,
                    Err(e) => {
                        *client_slot.lock().await = None;
                        return Err(e);
                    }
                };
                let ((status, volume, audio), song) = match res {
                    Ok(ok) => ok,
                    Err(mpd_client::client::CommandError::ConnectionClosed) => {
                        *client_slot.lock().await = None;
                        client = cached_client(host, *port, *timeout, client_slot).await?;
                        match await_timeout(
                            *timeout,
                            "MPD status",
                            client.command_list((RawStatus, CurrentSong)),
                        )
                        .await
                        {
                            Ok(res) => res.map_err(|e| map_command_error("status", e))?,
                            Err(e) => {
                                *client_slot.lock().await = None;
                                return Err(e);
                            }
                        }
                    }
                    Err(e) => return Err(map_command_error("status", e)),
                };
                let protocol_version: Box<str> = client.protocol_version().into();

                let current_track = song.map(map_song_to_track);

                Ok(MpdStatus {
                    host: host.clone(),
                    port: *port,
                    protocol_version,
                    state: status.state.into(),
                    volume,
                    elapsed: status.elapsed,
                    duration: status.duration,
                    current_track,
                    repeat: status.repeat,
                    random: status.random,
                    bitrate_kbps: status.bitrate.and_then(|b| u32::try_from(b).ok()),
                    sample_rate_hz: audio.0,
                    bit_depth: audio.1,
                    channels: audio.2,
                })
            }
        }
    }

    /// Subscribes to MPD subsystem push events (`idle` loop handled by `mpd_client`).
    ///
    /// # Errors
    /// Returns [`MpdError`] if connecting to MPD fails or times out.
    pub async fn subscribe_events(&self) -> Result<MpdEventStream, MpdError> {
        match &*self.inner {
            Backend::Mock(mock) => {
                let mut sub = {
                    let g = mock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(ref fault) = g.injected_fault {
                        return Err(MpdError::protocol_error(fault.clone()));
                    }
                    g.events.subscribe()
                };
                let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            () = tx.closed() => break,
                            evt = sub.recv() => match evt {
                                Ok(event) => {
                                    if tx.send(event).await.is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                                Err(broadcast::error::RecvError::Lagged(_)) => {}
                            }
                        }
                    }
                });
                Ok(MpdEventStream { rx })
            }
            Backend::Tcp {
                host,
                port,
                timeout,
                ..
            } => {
                let (client, mut events) = connect_client(host, *port, *timeout).await?;
                let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
                tokio::spawn(async move {
                    // Hold `client` alive for the lifetime of the subscription so the
                    // underlying TCP connection stays open for `ConnectionEvents`.
                    let _keep_client_alive = client;
                    let mut sent_disconnect = false;
                    loop {
                        tokio::select! {
                            () = tx.closed() => return,
                            maybe_evt = events.next() => {
                                let Some(evt) = maybe_evt else {
                                    break;
                                };
                                let mapped = match evt {
                                    ConnectionEvent::SubsystemChange(sub) => {
                                        MpdEvent::SubsystemChanged(sub.into())
                                    }
                                    ConnectionEvent::ConnectionClosed(err) => {
                                        sent_disconnect = true;
                                        MpdEvent::Disconnected(err.to_string().into())
                                    }
                                };
                                if tx.send(mapped).await.is_err() || sent_disconnect {
                                    return;
                                }
                            }
                        }
                    }
                    if !sent_disconnect {
                        let _ = tx
                            .send(MpdEvent::Disconnected("connection closed".into()))
                            .await;
                    }
                });
                Ok(MpdEventStream { rx })
            }
        }
    }

    async fn exec_simple<'a, F, Fut>(
        &'a self,
        cmd_name: &'static str,
        retry_on_close: bool,
        mock_cmd: MpdMockCommand,
        apply_mock: impl FnOnce(&mut MockState),
        run_tcp: F,
    ) -> Result<(), MpdError>
    where
        F: Fn(Client) -> Fut,
        Fut: std::future::Future<Output = Result<(), mpd_client::client::CommandError>> + 'a,
    {
        match &*self.inner {
            Backend::Mock(mock) => {
                let mut g = mock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(ref fault) = g.injected_fault {
                    return Err(MpdError::protocol_error(fault.clone()));
                }
                g.commands.push(mock_cmd);
                apply_mock(&mut g);
                Ok(())
            }
            Backend::Tcp {
                host,
                port,
                timeout,
                client: client_slot,
            } => {
                let client = cached_client(host, *port, *timeout, client_slot).await?;
                let res = match await_timeout(*timeout, cmd_name, run_tcp(client)).await {
                    Ok(res) => res,
                    Err(e) => {
                        *client_slot.lock().await = None;
                        return Err(e);
                    }
                };
                match res {
                    Ok(()) => Ok(()),
                    Err(mpd_client::client::CommandError::ConnectionClosed) => {
                        *client_slot.lock().await = None;
                        if !retry_on_close {
                            return Err(map_command_error(
                                cmd_name,
                                mpd_client::client::CommandError::ConnectionClosed,
                            ));
                        }
                        let fresh = cached_client(host, *port, *timeout, client_slot).await?;
                        match await_timeout(*timeout, cmd_name, run_tcp(fresh)).await {
                            Ok(res) => res.map_err(|e| map_command_error(cmd_name, e)),
                            Err(e) => {
                                *client_slot.lock().await = None;
                                Err(e)
                            }
                        }
                    }
                    Err(e) => Err(map_command_error(cmd_name, e)),
                }
            }
        }
    }
}

struct RawStatus;

impl mpd_client::commands::Command for RawStatus {
    type Response = (
        mpd_client::responses::Status,
        Option<u8>,
        (Option<u32>, Option<u8>, Option<u8>),
    );

    fn command(&self) -> mpd_client::protocol::command::Command {
        Status.command()
    }

    fn response(
        self,
        frame: mpd_client::protocol::response::Frame,
    ) -> Result<Self::Response, mpd_client::responses::TypedResponseError> {
        let vol_present = frame.find("volume").is_some();
        let audio = frame
            .find("audio")
            .map_or((None, None, None), parse_mpd_audio_format);
        let status = Status.response(frame)?;
        let volume = if vol_present {
            Some(status.volume).filter(|&v| v <= 100)
        } else {
            None
        };
        Ok((status, volume, audio))
    }
}

fn parse_mpd_audio_format(raw: &str) -> (Option<u32>, Option<u8>, Option<u8>) {
    let mut parts = raw.split(':');
    let rate = parts.next().and_then(|s| s.parse::<u32>().ok());
    let bits = parts.next().and_then(|s| {
        if s == "f" {
            Some(32)
        } else {
            s.parse::<u8>().ok()
        }
    });
    let ch = parts.next().and_then(|s| s.parse::<u8>().ok());
    (rate, bits, ch)
}

async fn cached_client(
    host: &MpdHost,
    port: MpdPort,
    timeout: Duration,
    slot: &tokio::sync::Mutex<Option<Client>>,
) -> Result<Client, MpdError> {
    let mut guard = slot.lock().await;
    if let Some(client) = guard.as_ref() {
        return Ok(client.clone());
    }
    let (client, mut events) = connect_client(host, port, timeout).await?;
    tokio::spawn(async move { while events.next().await.is_some() {} });
    *guard = Some(client.clone());
    Ok(client)
}

async fn await_timeout<F: std::future::Future>(
    timeout: Duration,
    op: &str,
    fut: F,
) -> Result<F::Output, MpdError> {
    tokio::time::timeout(timeout, fut).await.map_err(|elapsed| {
        MpdError::timeout(format!("{op} timed out after {timeout:?}: {elapsed}"))
    })
}

async fn connect_client(
    host: &MpdHost,
    port: MpdPort,
    timeout: Duration,
) -> Result<(Client, mpd_client::client::ConnectionEvents), MpdError> {
    let addr_label = if host.as_str().contains(':') {
        format!("[{}]:{}", host.as_str(), port.get())
    } else {
        format!("{}:{}", host.as_str(), port.get())
    };
    let stream = await_timeout(
        timeout,
        &format!("MPD TCP connect to {addr_label}"),
        TcpStream::connect((host.as_str(), port.get())),
    )
    .await?
    .map_err(|e| MpdError::connection_failed(host.as_str(), port.get(), e))?;

    await_timeout(
        timeout,
        &format!("MPD greeting handshake with {addr_label}"),
        Client::connect(stream),
    )
    .await?
    .map_err(|e| MpdError::protocol_error(e.to_string()))
}

async fn fetch_art_chunks(
    client: &Client,
    timeout: Duration,
    max_bytes: usize,
    file: &str,
    embedded: bool,
) -> Result<Option<Vec<u8>>, MpdError> {
    let op = if embedded { "readpicture" } else { "albumart" };
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let resp = if embedded {
            await_timeout(
                timeout,
                op,
                client.command(AlbumArtEmbedded::new(file).offset(offset)),
            )
            .await?
        } else {
            await_timeout(
                timeout,
                op,
                client.command(AlbumArt::new(file).offset(offset)),
            )
            .await?
        };
        let chunk = match resp {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Ok(if out.is_empty() { None } else { Some(out) }),
            Err(mpd_client::client::CommandError::ErrorResponse { .. }) => {
                // MPD returns ACK (e.g. NoExist / unknown command) when no art exists
                return Ok(None);
            }
            Err(e) => return Err(map_command_error(op, e)),
        };
        if chunk.size > max_bytes || out.len().saturating_add(chunk.data.len()) > max_bytes {
            return Err(MpdError::protocol_error(format!(
                "{op} size exceeds {max_bytes} byte limit"
            )));
        }
        if chunk.data.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if out.is_empty() && chunk.size > 0 {
            out.reserve(chunk.size.min(max_bytes));
        }
        out.extend_from_slice(&chunk.data);
        offset = out.len();
        if offset >= chunk.size {
            return Ok(Some(out));
        }
    }
}

fn map_song_to_track(entry: mpd_client::responses::SongInQueue) -> MpdTrack {
    let s = entry.song;
    let station_name = s
        .tags
        .get(&mpd_client::tag::Tag::Name)
        .and_then(|vals| vals.first())
        .map(|n| Box::from(n.as_str()));
    MpdTrack {
        file: s.url.as_str().into(),
        title: s.title().map(Box::from),
        artist: s.artists().first().map(|a| Box::from(a.as_str())),
        album: s.album().map(Box::from),
        station_name,
        duration: s.duration,
    }
}

fn map_command_error(cmd: &str, err: mpd_client::client::CommandError) -> MpdError {
    match err {
        mpd_client::client::CommandError::ErrorResponse { error, .. } => {
            MpdError::command_failed(cmd, format!("{error:?}"))
        }
        mpd_client::client::CommandError::ConnectionClosed => {
            MpdError::protocol_error(format!("MPD connection closed during '{cmd}'"))
        }
        mpd_client::client::CommandError::Protocol(e) => MpdError::protocol_error(e.to_string()),
        mpd_client::client::CommandError::InvalidTypedResponse(e) => {
            MpdError::protocol_error(e.to_string())
        }
    }
}

/// Compile-time proof that [`MpdDevice`] and [`MpdError`] are `Send + Sync`.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MpdDevice>();
    assert_send_sync::<MpdDeviceMock>();
    assert_send_sync::<MpdError>();
};

/// Test helpers exposed unconditionally under the `test-util` feature (`M-TEST-UTIL`).
#[cfg(feature = "test-util")]
pub mod test_helpers {
    pub use super::{MpdDevice, MpdDeviceMock, MpdError, MpdHost, MpdPort, MpdStatus};
}
