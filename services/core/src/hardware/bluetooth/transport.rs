//! Bluetooth playback state, PCM metadata, and transport command plumbing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::connection::BtAddress;

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
    /// Average volume level as a percentage (0..=100), if reported.
    pub volume_percent: Option<u8>,
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

/// Computes the playback progress ratio clamped to `0.0..=1.0`.
///
/// A zero or unknown duration maps to `0.0`. Shared between
/// [`BtPlaybackState::progress_ratio`] and the UI shell so the ratio logic has
/// a single source of truth.
#[must_use]
pub fn playback_progress_ratio(position_ms: u32, duration_ms: u32) -> f32 {
    if duration_ms == 0 {
        0.0
    } else {
        (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
    }
}

/// Computes the `MediaPlayer1.Seek` relative offset for an absolute target.
///
/// `BlueZ` `Seek` takes a *relative* offset in microseconds; convert the
/// absolute millisecond target using the freshly observed position.
#[must_use]
pub fn seek_offset_us(position_ms: u32, current_ms: u32) -> i64 {
    (i64::from(position_ms) - i64::from(current_ms)) * 1_000
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
    /// OBEX BIP L2CAP PSM port (AVRCP 1.6 Cover Art), if advertised by remote player.
    pub obex_port: Option<u16>,
    /// Track image handle (e.g. "1000001") for AVRCP Cover Art over OBEX BIP.
    pub image_handle: Option<Box<str>>,
    /// Local cache path to downloaded cover art image (e.g. `/var/cache/speaker-covers/...jpg`).
    pub cover_art_path: Option<Box<str>>,
}

/// Placeholder title shown whenever no Bluetooth media player is connected.
/// Single source of truth shared by the transport default state and the D-Bus
/// property fallbacks in the connection module.
pub const BT_NO_CONNECTION_TITLE: &str = "Waiting for Bluetooth Connection...";

impl Default for BtPlaybackState {
    fn default() -> Self {
        Self {
            device_address: None,
            device_name: "Bluetooth Audio Device".into(),
            status: "stopped".into(),
            title: BT_NO_CONNECTION_TITLE.into(),
            artist: "".into(),
            album: "".into(),
            position_ms: 0,
            duration_ms: 0,
            player_path: None,
            obex_port: None,
            image_handle: None,
            cover_art_path: None,
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
        playback_progress_ratio(self.position_ms, self.duration_ms)
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

/// Decodes a `BlueALSA` 16-bit PCM format identifier into a human-readable name.
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

/// Renders `BlueALSA` per-channel volume bytes (`(muted << 7) | level`) and calculates average volume percent.
///
/// Uses channel names when provided (`ChannelMap`), falling back to generic indices.
/// Returns `(formatted_channel_string, is_muted, volume_percent)`.
#[must_use]
pub fn format_volume_bytes(
    bytes: &[u8],
    channel_names: Option<&[Box<str>]>,
) -> (Option<Box<str>>, bool, Option<u8>) {
    if bytes.is_empty() {
        return (None, false, None);
    }
    let muted = bytes.iter().any(|b| b & 0x80 != 0);
    let mut total_level: u32 = 0;
    let volume = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let level = b & 0x7F;
            total_level += u32::from(level);
            let name = channel_names
                .and_then(|names| names.get(i))
                .map_or_else(|| format!("CH{i}"), std::string::ToString::to_string);
            format!("{name}: {level}")
        })
        .collect::<Vec<_>>()
        .join(" ");

    let avg_level = f64::from(total_level) / (bytes.len() as f64);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "percentage clamped 0.0..=100.0 fits in u8"
    )]
    let percent = ((avg_level / 127.0) * 100.0).round().clamp(0.0, 100.0) as u8;

    (Some(volume.into()), muted, Some(percent))
}

/// User-initiated transport command dispatched from the render thread to the `bt-monitor` worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCommand {
    /// Toggle between Play and Pause on the active media player.
    TogglePlayPause,
    /// Advance to the next track.
    NextTrack,
    /// Return to the previous track.
    PreviousTrack,
    /// Seek to an absolute playback position in milliseconds.
    Seek(u32),
}

/// Thread-safe command queue bridging Slint render-thread callbacks and the
/// `bt-monitor` worker thread so transport D-Bus calls never block the render thread.
///
/// The render thread only `push`es (lock-free cheap mutex); the worker drains the
/// queue once per monitor tick and executes the blocking D-Bus calls off-thread.
#[derive(Debug, Clone, Default)]
pub struct TransportCommandQueue {
    queue: Arc<Mutex<VecDeque<TransportCommand>>>,
}

impl TransportCommandQueue {
    /// Creates an empty transport command queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Enqueues a transport command for execution on the `bt-monitor` worker.
    ///
    /// # Errors
    /// Never: a poisoned mutex is recovered with `into_inner` so the enqueue
    /// still lands. When the 32-entry backlog is already full the new command
    /// is dropped and logged — seek/navigation commands have no authoritative
    /// reconciliation, so overflow is surfaced at warn level, not debug.
    pub fn push(&self, command: TransportCommand) {
        let mut lock = match self.queue.lock() {
            Ok(lock) => lock,
            Err(poisoned) => {
                tracing::warn!("TransportCommandQueue mutex poisoned in push; recovering");
                poisoned.into_inner()
            }
        };
        // Bounded to 32 commands: rapid-fire clicks can never accumulate an
        // unbounded backlog. Play/pause drops reconcile on the next publish,
        // but seek/navigation drops have no such reconciliation, so overflow
        // is surfaced at warn level for operators.
        if lock.len() < 32 {
            lock.push_back(command);
        } else {
            tracing::warn!("transport command queue full; dropping {command:?}");
        }
    }

    /// Returns `true` while no command is pending execution.
    ///
    /// # Errors
    /// Never: a poisoned mutex is recovered with `into_inner` so the queue state
    /// stays queryable (command queue poisoning cannot happen without a prior
    /// panic while the lock was held).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self.queue.lock() {
            Ok(lock) => lock.is_empty(),
            Err(poisoned) => {
                tracing::warn!("TransportCommandQueue mutex poisoned in is_empty; recovering");
                poisoned.into_inner().is_empty()
            }
        }
    }

    /// Drains all pending commands in FIFO order.
    ///
    /// # Errors
    /// Never: a poisoned mutex is recovered with `into_inner` so pending commands
    /// are never silently dropped.
    #[must_use]
    pub fn drain(&self) -> Vec<TransportCommand> {
        match self.queue.lock() {
            Ok(mut lock) => lock.drain(..).collect(),
            Err(poisoned) => {
                tracing::warn!("TransportCommandQueue mutex poisoned in drain; recovering");
                let mut lock = poisoned.into_inner();
                lock.drain(..).collect()
            }
        }
    }
}
