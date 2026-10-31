//! Application UI Shell controller and view manager.
//!
//! Orchestrates the 800x480 appliance user interface:
//! - Top status bar ([`TopStatusBar`])
//! - Central active viewport ([`MainViewport`])
//! - Bottom navigation dock ([`NavDock`])
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`).

use std::fmt::{self, Debug, Formatter};
use std::sync::{Arc, Mutex};

use slint::ComponentHandle;

use super::generated::{MainWindow, Theme};
use super::theme::ThemePreset;

/// Shared state and interactive controller for [`MainWindow`].
pub struct AppShell {
    window: MainWindow,
    last_cover_path: Arc<Mutex<Option<Box<str>>>>,
}

impl Debug for AppShell {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppShell")
            .field("active_theme", &self.current_theme())
            .field("active_tab", &self.current_tab())
            .finish_non_exhaustive()
    }
}

impl AppShell {
    /// Initializes an [`AppShell`] controller wrapping a [`MainWindow`] component.
    #[must_use]
    pub fn new(window: MainWindow) -> Self {
        let shell = Self {
            window,
            last_cover_path: Arc::new(Mutex::new(None)),
        };

        shell.setup_callbacks();
        shell.apply_theme(ThemePreset::default());
        shell
    }

    /// Returns a reference to the underlying Slint [`MainWindow`] handle.
    #[must_use]
    pub const fn window(&self) -> &MainWindow {
        &self.window
    }

    /// Returns the currently active theme preset.
    #[must_use]
    pub fn current_theme(&self) -> ThemePreset {
        ThemePreset::from_index(self.window.global::<Theme>().get_active_preset())
    }

    /// Returns the currently active navigation dock tab index (0 to 5).
    #[must_use]
    pub fn current_tab(&self) -> i32 {
        self.window.get_active_tab()
    }

    /// Returns whether the optical CD player service and navigation entry are enabled.
    #[must_use]
    pub fn is_cd_enabled(&self) -> bool {
        self.window.get_cd_enabled()
    }

    /// Sets whether the optical CD player service and navigation entry are enabled.
    ///
    /// If disabled while the shell is currently on the CD Player tab (tab 3), the active tab
    /// is automatically reset to the Now Playing player tab (tab 0) to prevent state desync.
    pub fn set_cd_enabled(&self, enabled: bool) {
        self.window.set_cd_enabled(enabled);
        if !enabled && self.current_tab() == 3 {
            self.set_tab(0);
        }
    }

    /// Switches the active navigation dock tab.
    ///
    /// If `tab` is 3 (CD Player) but CD Player is not enabled, falls back to tab 0.
    pub fn set_tab(&self, tab: i32) {
        let mut tab = tab.clamp(0, 5);
        if tab == 3 && !self.is_cd_enabled() {
            tab = 0;
        }
        self.window.set_active_tab(tab);
    }

    /// Updates the active audio source name shown in the top status bar.
    pub fn set_source(&self, source: impl Into<slint::SharedString>) {
        self.window.set_active_source(source.into());
    }

    /// Updates the track or connected device name displayed in the top status bar.
    pub fn set_track_or_device(&self, info: impl Into<slint::SharedString>) {
        self.window.set_track_or_device(info.into());
    }

    /// Updates the active audio codec badge in the top status bar.
    pub fn set_stream_codec(&self, codec: impl Into<slint::SharedString>) {
        self.window.set_stream_codec(codec.into());
    }

    /// Updates the Now Playing title, artist, and album.
    pub fn set_track_metadata(
        &self,
        title: impl Into<slint::SharedString>,
        artist: impl Into<slint::SharedString>,
        album: impl Into<slint::SharedString>,
    ) {
        self.window.set_track_title(title.into());
        self.window.set_track_artist(artist.into());
        self.window.set_track_album(album.into());
    }

    /// Returns the current volume percentage (0.0–100.0).
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.window.get_volume_value()
    }

    /// Returns whether audio stream specifications (sample rate / bit depth) are currently visible.
    #[must_use]
    pub fn show_stream_specs(&self) -> bool {
        self.window.get_show_stream_specs()
    }

    /// Updates the volume percentage (clamped to 0.0–100.0).
    pub fn set_volume(&self, volume: f32) {
        let clamped = if volume.is_finite() {
            volume.clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.window.set_volume_value(clamped);
    }

    /// Returns whether media is actively playing.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.window.get_is_playing()
    }

    /// Sets whether media is playing.
    pub fn set_playing(&self, is_playing: bool) {
        self.window.set_is_playing(is_playing);
    }

    /// Returns whether audio output is currently soft-muted.
    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.window.get_is_muted()
    }

    /// Sets whether audio output is currently soft-muted.
    pub fn set_muted(&self, muted: bool) {
        self.window.set_is_muted(muted);
    }

    /// Updates playback state, elapsed position, and total duration.
    pub fn set_playback_state(&self, is_playing: bool, position_ms: u32, duration_ms: u32) {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "duration_ms up to i32::MAX fits cleanly in slint int"
        )]
        self.window.set_duration_ms(duration_ms as i32);
        self.window.set_is_playing(is_playing);
        let ratio = if duration_ms == 0 {
            0.0
        } else {
            (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
        };
        self.window.set_progress_ratio(ratio);
        self.window
            .set_position_text(crate::hardware::format_duration_ms(position_ms).into());
        self.window
            .set_duration_text(crate::hardware::format_duration_ms(duration_ms).into());
    }

    /// Updates the audio stream inspector pill details.
    pub fn set_stream_details(&self, details: impl Into<slint::SharedString>) {
        self.window.set_stream_details(details.into());
    }

    /// Updates the sampling rate badge on the top status bar.
    pub fn set_stream_sample_rate(&self, rate: impl Into<slint::SharedString>) {
        self.window.set_stream_sample_rate(rate.into());
    }

    /// Updates the bit depth badge on the top status bar.
    pub fn set_stream_bit_depth(&self, depth: impl Into<slint::SharedString>) {
        self.window.set_stream_bit_depth(depth.into());
    }

    /// Applies audio stream metrics from an active [`crate::hardware::BlueAlsaPcm`].
    pub fn apply_pcm_stream(&self, pcm: &crate::hardware::BlueAlsaPcm) {
        if !pcm.selected_codec.is_empty() {
            self.set_stream_codec(pcm.selected_codec.as_ref());
        }
        let rate_khz = if pcm.sampling_rate > 0 {
            format!("{} kHz", pcm.sampling_rate / 1000)
        } else {
            "48 kHz".to_string()
        };
        self.set_stream_sample_rate(rate_khz);

        let bit_depth = match pcm.format.as_ref() {
            "S16_LE" | "S16_BE" => "16-bit",
            "S24_LE" | "S24_BE" | "S24_3LE" => "24-bit",
            "S32_LE" | "S32_BE" => "32-bit",
            other => {
                if other.contains("16") {
                    "16-bit"
                } else if other.contains("24") {
                    "24-bit"
                } else if other.contains("32") {
                    "32-bit"
                } else {
                    "16-bit"
                }
            }
        };
        self.set_stream_bit_depth(bit_depth);
        self.window.set_is_muted(pcm.muted);
        if let Some(pct) = pcm.volume_percent {
            self.set_volume(f32::from(pct));
        }
    }

    /// Updates the mini peak meter visualization ratios (0.0 to 1.0).
    pub fn set_peak_levels(&self, left: f32, right: f32) {
        let clamp_ratio = |val: f32| {
            if val.is_nan() {
                0.0
            } else {
                val.clamp(0.0, 1.0)
            }
        };
        self.window.set_left_peak_ratio(clamp_ratio(left));
        self.window.set_right_peak_ratio(clamp_ratio(right));
    }

    /// Sets whether cover art is present.
    pub fn set_has_cover_art(&self, has_art: bool) {
        if !has_art {
            if let Ok(mut last) = self.last_cover_path.lock() {
                *last = None;
            }
        }
        self.window.set_has_cover_art(has_art);
    }

    /// Updates the cover art image.
    pub fn set_cover_image(&self, image: slint::Image) {
        self.window.set_cover_image(image);
        self.window.set_has_cover_art(true);
    }

    /// Applies state from a [`crate::hardware::BtPlaybackState`] point-in-time snapshot.
    pub fn apply_bluetooth_playback(&self, playback: &crate::hardware::BtPlaybackState) {
        Self::apply_playback_to_window(&self.window, &self.last_cover_path, playback);
    }

    fn apply_playback_to_window(
        w: &MainWindow,
        cover_cache: &Mutex<Option<Box<str>>>,
        playback: &crate::hardware::BtPlaybackState,
    ) {
        w.set_track_title(playback.title.as_ref().into());
        w.set_track_artist(playback.artist.as_ref().into());
        w.set_track_album(playback.album.as_ref().into());
        #[expect(
            clippy::cast_possible_wrap,
            reason = "duration_ms up to i32::MAX fits cleanly in slint int"
        )]
        w.set_duration_ms(playback.duration_ms as i32);
        w.set_is_playing(playback.is_playing());
        let ratio = if playback.duration_ms == 0 {
            0.0
        } else {
            (playback.position_ms as f32 / playback.duration_ms as f32).clamp(0.0, 1.0)
        };
        w.set_progress_ratio(ratio);
        w.set_position_text(crate::hardware::format_duration_ms(playback.position_ms).into());
        w.set_duration_text(crate::hardware::format_duration_ms(playback.duration_ms).into());
        if !playback.device_name.is_empty() {
            w.set_track_or_device(playback.device_name.as_ref().into());
        }

        if let Ok(mut last) = cover_cache.lock() {
            if let Some(ref path) = playback.cover_art_path {
                if last.as_deref() != Some(path.as_ref()) {
                    if let Ok(img) =
                        slint::Image::load_from_path(std::path::Path::new(path.as_ref()))
                    {
                        w.set_cover_image(img);
                        w.set_has_cover_art(true);
                    } else {
                        w.set_has_cover_art(false);
                    }
                    *last = Some(path.clone());
                }
            } else if last.is_some() {
                w.set_has_cover_art(false);
                *last = None;
            }
        }
    }

    /// Applies a theme preset to the UI shell.
    pub fn apply_theme(&self, preset: ThemePreset) {
        preset.apply(&self.window);
    }

    /// Cycles to the next available theme preset in cyclic order.
    #[must_use]
    pub fn cycle_theme(&self) -> ThemePreset {
        let next = self.current_theme().next();
        self.apply_theme(next);
        next
    }

    /// Wires the UI shell transport callbacks to an interactive [`crate::hardware::BluetoothDevice`].
    pub fn wire_bluetooth_transport(&self, bt: crate::hardware::BluetoothDevice) {
        let bt_toggle = bt.clone();
        let win = self.window.as_weak();
        self.window.on_toggle_play_pause(move || {
            if let Some(w) = win.upgrade() {
                if let Ok(next_state) = bt_toggle.player_toggle() {
                    let actual = bt_toggle
                        .playback_state()
                        .ok()
                        .flatten()
                        .map_or(next_state, |p| p.is_playing());
                    w.set_is_playing(actual);
                }
            }
        });

        let bt_next = bt.clone();
        let bt_sync_next = bt.clone();
        let win_next = self.window.as_weak();
        let cover_next = Arc::clone(&self.last_cover_path);
        self.window.on_next_track(move || {
            if bt_next.player_next().is_ok() {
                if let Some(w) = win_next.upgrade() {
                    if let Ok(Some(ref p)) = bt_sync_next.playback_state() {
                        Self::apply_playback_to_window(&w, &cover_next, p);
                    }
                }
            }
        });

        let bt_prev = bt.clone();
        let bt_sync_prev = bt.clone();
        let win_prev = self.window.as_weak();
        let cover_prev = Arc::clone(&self.last_cover_path);
        self.window.on_previous_track(move || {
            if bt_prev.player_previous().is_ok() {
                if let Some(w) = win_prev.upgrade() {
                    if let Ok(Some(ref p)) = bt_sync_prev.playback_state() {
                        Self::apply_playback_to_window(&w, &cover_prev, p);
                    }
                }
            }
        });

        let bt_seek = bt;
        let win = self.window.as_weak();
        self.window.on_seek(move |ratio| {
            if let Some(w) = win.upgrade() {
                let pos = Self::apply_seek_ratio(&w, ratio);
                let _ = bt_seek.player_seek(pos);
            }
        });
    }

    fn apply_seek_ratio(w: &MainWindow, ratio: f32) -> u32 {
        let clamped = if ratio.is_finite() {
            ratio.clamp(0.0, 1.0)
        } else {
            0.0
        };
        w.set_progress_ratio(clamped);
        let dur = u32::try_from(w.get_duration_ms()).unwrap_or(0);
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "clamped is non-negative <= 1.0 and dur is positive u32"
        )]
        let pos = (clamped * dur as f32).round() as u32;
        w.set_position_text(crate::hardware::format_duration_ms(pos).into());
        tracing::debug!("Seek to ratio: {clamped} ({pos}ms)");
        pos
    }

    fn setup_callbacks(&self) {
        let win = self.window.as_weak();
        self.window.on_tab_selected(move |idx| {
            if let Some(w) = win.upgrade() {
                let mut clamped = idx.clamp(0, 5);
                if clamped == 3 && !w.get_cd_enabled() {
                    clamped = 0;
                }
                w.set_active_tab(clamped);
            }
        });

        let win = self.window.as_weak();
        self.window.on_select_theme(move |idx| {
            if let Some(w) = win.upgrade() {
                let preset = ThemePreset::from_index(idx);
                preset.apply(&w);
            }
        });

        let win = self.window.as_weak();
        self.window.on_cycle_theme(move || {
            if let Some(w) = win.upgrade() {
                let current = ThemePreset::from_index(w.global::<Theme>().get_active_preset());
                let next = current.next();
                next.apply(&w);
            }
        });

        self.window.on_toggle_play_pause(|| {
            tracing::debug!("Play/Pause requested (no audio transport wired)");
        });

        let win = self.window.as_weak();
        self.window.on_toggle_mute(move || {
            if let Some(w) = win.upgrade() {
                let current = w.get_is_muted();
                w.set_is_muted(!current);
                tracing::debug!("Mute toggled to {}", !current);
            }
        });

        let win = self.window.as_weak();
        self.window.on_seek(move |ratio| {
            if let Some(w) = win.upgrade() {
                Self::apply_seek_ratio(&w, ratio);
            }
        });

        self.window.on_next_track(|| {
            tracing::info!("Next track requested");
        });

        self.window.on_previous_track(|| {
            tracing::info!("Previous track requested");
        });

        self.window.on_stream_pill_clicked(|| {
            tracing::info!("Stream inspector pill clicked");
        });

        self.window.on_source_clicked(|| {
            tracing::info!("Source pill clicked");
        });

        self.window.on_night_mode_clicked(|| {
            tracing::info!("Night mode clicked");
        });

        self.window.on_volume_changed(|val| {
            tracing::debug!("Volume changed to {val}%");
        });
    }
}
