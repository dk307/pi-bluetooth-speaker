//! Source-agnostic Now Playing models, capabilities, and transport command queues.
//!
//! Decouples [`crate::ui::AppShell`] and the Slint UI from specific audio
//! backends (Bluetooth AVRCP, MPD/`rmpd`, or future sources) while preserving
//! capability-driven controls such as MPD position seeking, repeat, and shuffle.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use speaker_bluetooth::BtPlaybackState;
use speaker_mpd::{MpdPlaybackState, MpdStatus, MpdTrack};

/// Canonical source IDs matching `audio-manager`'s `SourceRegistry`.
pub const SOURCE_ID_BLUETOOTH: &str = "bluetooth";
/// Canonical source ID for `rmpd` / Music Player Daemon.
pub const SOURCE_ID_MPD: &str = "rmpd";

/// Sentinel used across `NowPlayingSnapshot` when position or duration is unknown.
pub const UNKNOWN_TIMELINE_MS: u32 = u32::MAX;

/// Identifies the active audio source feeding the Now Playing view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MediaSourceKind {
    /// Bluetooth A2DP + AVRCP (`bt-manager` / `BlueZ`).
    #[default]
    Bluetooth,
    /// Local/network MPD (`rmpd` via `speaker-mpd`).
    Mpd,
    /// Future or custom source registered in `audio-manager`.
    Other(Box<str>),
}

impl MediaSourceKind {
    /// Maps a `SourceId` string from `org.speaker.AudioSources1` into [`MediaSourceKind`].
    #[must_use]
    pub fn from_source_id(id: &str) -> Self {
        match id.trim() {
            "" | SOURCE_ID_BLUETOOTH => Self::Bluetooth,
            SOURCE_ID_MPD => Self::Mpd,
            other => Self::Other(Box::from(other)),
        }
    }

    /// Returns the canonical `audio-manager` source ID string.
    #[must_use]
    pub fn id_str(&self) -> &str {
        match self {
            Self::Bluetooth => SOURCE_ID_BLUETOOTH,
            Self::Mpd => SOURCE_ID_MPD,
            Self::Other(id) => id.as_ref(),
        }
    }

    /// Returns the human-readable display name for UI headers and switchers.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::Bluetooth => "Bluetooth",
            Self::Mpd => "Music Player",
            Self::Other(id) => id.as_ref(),
        }
    }

    /// Returns the idle title shown when no track is loaded for this source.
    #[must_use]
    pub const fn idle_title(&self) -> &'static str {
        match self {
            Self::Bluetooth => "Waiting for Bluetooth Connection...",
            Self::Mpd => "Music Player Idle",
            Self::Other(_) => "Waiting for Audio Source...",
        }
    }
}

/// Capability flags advertised by the active audio source for Now Playing controls.
#[expect(
    clippy::struct_excessive_bools,
    reason = "capability flags mirror discrete UI control enablement"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransportCapabilities {
    /// Whether Play/Pause is supported.
    pub can_play_pause: bool,
    /// Whether Next Track is supported.
    pub can_next: bool,
    /// Whether Previous Track is supported.
    pub can_previous: bool,
    /// Whether interactive position seeking is supported.
    pub can_seek: bool,
    /// Whether Repeat mode toggle is supported.
    pub can_repeat: bool,
    /// Whether Random/Shuffle mode toggle is supported.
    pub can_random: bool,
}

impl TransportCapabilities {
    /// Capabilities for Bluetooth AVRCP playback.
    #[must_use]
    pub const fn bluetooth(has_track_or_playing: bool) -> Self {
        Self {
            can_play_pause: has_track_or_playing,
            can_next: has_track_or_playing,
            can_previous: has_track_or_playing,
            can_seek: false,
            can_repeat: false,
            can_random: false,
        }
    }

    /// Capabilities for MPD (`rmpd`) playback.
    #[must_use]
    pub const fn mpd(has_track: bool, seekable: bool) -> Self {
        Self {
            can_play_pause: has_track,
            can_next: has_track,
            can_previous: has_track,
            can_seek: seekable,
            can_repeat: true,
            can_random: true,
        }
    }
}

/// Playback mode flags (for sources that support Repeat / Random, such as MPD).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaybackModeState {
    /// Repeat mode enabled.
    pub repeat: bool,
    /// Random / shuffle mode enabled.
    pub random: bool,
}

/// Stream format & codec details displayed in the top-bar badge and Codec HUD.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamMetadata {
    /// Short badge label (`"FLAC"`, `"MP3"`, `"AAC"`, `"aptX-HD"`, ...).
    pub codec: Box<str>,
    /// Sample rate label (`"44.1 kHz"`, `"48 kHz"`, `"—"`).
    pub sample_rate: Box<str>,
    /// Bit depth label (`"16-bit"`, `"24-bit"`, `"—"`).
    pub bit_depth: Box<str>,
    /// Summary format line (`"44.1 kHz • 16-bit • Stereo"`).
    pub format: Box<str>,
    /// Latency or bitrate detail (`"320 kbps"` or `"—"`).
    pub detail: Box<str>,
    /// Track file path or stream URL (`""` for Bluetooth).
    pub url: Box<str>,
}

/// Source-agnostic Now Playing snapshot rendered by [`crate::ui::AppShell`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlayingSnapshot {
    /// Active audio source kind.
    pub source: MediaSourceKind,
    /// Top-bar quick label (connected BT device name, `"Music Player"`, etc.).
    pub source_label: Box<str>,
    /// Track title (or idle prompt when no track is active).
    pub title: Box<str>,
    /// Primary artist name (empty when none).
    pub artist: Box<str>,
    /// Album name (empty when none).
    pub album: Box<str>,
    /// Whether audio is currently playing.
    pub is_playing: bool,
    /// Current playback position in milliseconds, or [`UNKNOWN_TIMELINE_MS`].
    pub position_ms: u32,
    /// Total track duration in milliseconds, or [`UNKNOWN_TIMELINE_MS`].
    pub duration_ms: u32,
    /// Transport & scrubber capabilities for this source/track.
    pub capabilities: TransportCapabilities,
    /// Repeat/shuffle flags.
    pub modes: PlaybackModeState,
    /// Optional stream/codec metadata (populated for MPD tracks/streams).
    pub stream_info: Option<StreamMetadata>,
}

impl Default for NowPlayingSnapshot {
    fn default() -> Self {
        Self {
            source: MediaSourceKind::Bluetooth,
            source_label: Box::from("Waiting for Connection..."),
            title: Box::from(MediaSourceKind::Bluetooth.idle_title()),
            artist: Box::from(""),
            album: Box::from(""),
            is_playing: false,
            position_ms: 0,
            duration_ms: 0,
            capabilities: TransportCapabilities::default(),
            modes: PlaybackModeState::default(),
            stream_info: None,
        }
    }
}

impl NowPlayingSnapshot {
    /// Converts a Bluetooth [`BtPlaybackState`] into a [`NowPlayingSnapshot`].
    #[must_use]
    pub fn from_bluetooth(pb: &BtPlaybackState) -> Self {
        let is_playing = pb.is_playing();
        let has_track = pb.duration_ms > 0
            || is_playing
            || (!pb.title.is_empty()
                && pb.title.as_ref() != MediaSourceKind::Bluetooth.idle_title());
        let source_label = if pb.device_name.is_empty() {
            Box::from("Waiting for Connection...")
        } else {
            pb.device_name.clone()
        };
        let title = if pb.title.is_empty() {
            Box::from(MediaSourceKind::Bluetooth.idle_title())
        } else {
            pb.title.clone()
        };
        Self {
            source: MediaSourceKind::Bluetooth,
            source_label,
            title,
            artist: pb.artist.clone(),
            album: pb.album.clone(),
            is_playing,
            position_ms: pb.position_ms,
            duration_ms: pb.duration_ms,
            capabilities: TransportCapabilities::bluetooth(has_track),
            modes: PlaybackModeState::default(),
            stream_info: None,
        }
    }

    /// Converts an MPD [`MpdStatus`] into a [`NowPlayingSnapshot`].
    #[must_use]
    pub fn from_mpd(status: &MpdStatus) -> Self {
        Self::from_mpd_with_codec(status, None)
    }

    /// Converts an MPD [`MpdStatus`] into a [`NowPlayingSnapshot`], overriding the
    /// inferred codec label when an HTTP stream probe has determined `probed_codec`.
    #[must_use]
    pub fn from_mpd_with_codec(status: &MpdStatus, probed_codec: Option<&str>) -> Self {
        let is_playing = matches!(status.state, MpdPlaybackState::Playing);
        let track = status.current_track.as_ref();
        let has_track = track.is_some();

        let (resolved_artist, resolved_title) =
            track.map_or((None, None), MpdTrack::resolved_artist_title);

        let title: Box<str> = match track {
            Some(t) => resolved_title
                .or_else(|| t.file.rsplit('/').next().filter(|s| !s.is_empty()))
                .unwrap_or("Unknown Track")
                .into(),
            None => MediaSourceKind::Mpd.idle_title().into(),
        };
        let artist: Box<str> = resolved_artist.unwrap_or("").into();
        let album: Box<str> = track.and_then(|t| t.album.as_deref()).unwrap_or("").into();
        let source_label: Box<str> = track
            .and_then(|t| t.station_name.as_deref())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("")
            .into();

        let duration_ms = status
            .duration
            .or_else(|| track.and_then(|t| t.duration))
            .map_or(0, duration_to_ms_saturating);
        let position_ms = status.elapsed.map_or_else(
            || if has_track { 0 } else { UNKNOWN_TIMELINE_MS },
            duration_to_ms_saturating,
        );
        let seekable = has_track && duration_ms > 0 && duration_ms != UNKNOWN_TIMELINE_MS;

        let stream_info = track.map(|t| {
            let codec = probed_codec
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| t.codec_label());
            let sample_rate: Box<str> = status
                .sample_rate_hz
                .map_or_else(|| Box::from("—"), format_rate_khz);
            let bit_depth: Box<str> = status
                .bit_depth
                .map_or_else(|| Box::from("—"), |b| Box::from(format!("{b}-bit")));
            let channels_label: Box<str> = match status.channels {
                Some(1) => Box::from("Mono"),
                Some(2) => Box::from("Stereo"),
                Some(n) => Box::from(format!("{n} ch")),
                None => Box::from("—"),
            };
            let format = Box::from(format!("{sample_rate} • {bit_depth} • {channels_label}"));
            let detail = status
                .bitrate_kbps
                .map_or_else(|| Box::from("—"), |kbps| Box::from(format!("{kbps} kbps")));
            StreamMetadata {
                codec: Box::from(codec),
                sample_rate,
                bit_depth,
                format,
                detail,
                url: t.file.clone(),
            }
        });

        Self {
            source: MediaSourceKind::Mpd,
            source_label,
            title,
            artist,
            album,
            is_playing,
            position_ms,
            duration_ms,
            capabilities: TransportCapabilities::mpd(has_track, seekable),
            modes: PlaybackModeState {
                repeat: status.repeat,
                random: status.random,
            },
            stream_info,
        }
    }
}

fn format_rate_khz(hz: u32) -> Box<str> {
    if hz.is_multiple_of(1000) {
        Box::from(format!("{} kHz", hz / 1000))
    } else {
        Box::from(format!("{:.1} kHz", f64::from(hz) / 1000.0))
    }
}

fn duration_to_ms_saturating(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX - 1)
}

/// Unified transport action dispatched from the Now Playing UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAction {
    /// Toggle play/pause on the active source.
    TogglePlayPause,
    /// Skip to next track.
    NextTrack,
    /// Skip to previous track.
    PreviousTrack,
    /// Seek to an absolute position in milliseconds.
    SeekToMs(u32),
    /// Toggle repeat mode.
    SetRepeat(bool),
    /// Toggle random/shuffle mode.
    SetRandom(bool),
}

/// MPD worker command queued from the Slint render thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpdCommand {
    /// Toggle between playing and paused.
    TogglePlayPause,
    /// Advance to next track.
    Next,
    /// Return to previous track.
    Previous,
    /// Seek to an absolute position.
    Seek(Duration),
    /// Set repeat mode.
    SetRepeat(bool),
    /// Set random/shuffle mode.
    SetRandom(bool),
}

/// Thread-safe FIFO queue for [`MpdCommand`]s between the render thread and MPD worker.
#[derive(Debug, Clone, Default)]
pub struct MpdCommandQueue {
    inner: Arc<Mutex<VecDeque<MpdCommand>>>,
}

impl MpdCommandQueue {
    /// Creates an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a command from the UI thread.
    pub fn push(&self, cmd: MpdCommand) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(cmd);
    }

    /// Drains all pending commands in FIFO order on the worker thread.
    #[must_use]
    pub fn drain(&self) -> Vec<MpdCommand> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}

/// One registered source entry from `org.speaker.AudioSources1.Sources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSourceEntry {
    /// Canonical source ID (`"bluetooth"`, `"rmpd"`, ...).
    pub id: Box<str>,
    /// Human-readable name (`"Bluetooth"`, `"Music Player"`, ...).
    pub display_name: Box<str>,
    /// `"streaming"` or `"idle"`.
    pub state: Box<str>,
    /// Current stream sample rate in Hz (`0` when idle).
    pub sample_rate_hz: u32,
    /// Whether this source is currently active.
    pub is_active: bool,
}

/// Snapshot of multi-source routing state from `org.speaker.AudioSources1`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioSourcesState {
    /// Active source ID, or `None` when idle.
    pub active_source: Option<Box<str>>,
    /// Pinned source ID, or `None` when auto-switching.
    pub pinned_source: Option<Box<str>>,
    /// Registered sources in priority/registry order.
    pub sources: Vec<AudioSourceEntry>,
}

/// Command sent from the UI to the `audio-sources` D-Bus worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceCommand {
    /// Request immediate source switch via `AudioSources1.SelectSource`.
    SelectSource(Box<str>),
    /// Pin a source or clear pin (`None`) via `AudioSources1.PinSource`.
    PinSource(Option<Box<str>>),
}

/// Thread-safe FIFO queue for [`SourceCommand`]s.
#[derive(Debug, Clone, Default)]
pub struct SourceCommandQueue {
    inner: Arc<Mutex<VecDeque<SourceCommand>>>,
}

impl SourceCommandQueue {
    /// Creates an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a source command from the UI thread.
    pub fn push(&self, cmd: SourceCommand) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(cmd);
    }

    /// Drains all pending source commands on the worker thread.
    #[must_use]
    pub fn drain(&self) -> Vec<SourceCommand> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}
