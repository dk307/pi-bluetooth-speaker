//! Application UI Shell controller and view manager.
//!
//! Orchestrates the 800x480 appliance user interface:
//! - Top status bar ([`TopStatusBar`])
//! - Central active viewport ([`MainViewport`])
//! - Bottom navigation dock ([`NavDock`])
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`).

use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use slint::ComponentHandle;

use super::generated::{MainWindow, Theme};
use super::theme::ThemePreset;

/// Shared state and interactive controller for [`MainWindow`].
pub struct AppShell {
    window: MainWindow,
    duration_ms: Arc<AtomicU32>,
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
            duration_ms: Arc::new(AtomicU32::new(0)),
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
        self.duration_ms.store(duration_ms, Ordering::Relaxed);
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
        self.window.set_has_cover_art(has_art);
    }

    /// Updates the cover art image.
    pub fn set_cover_image(&self, image: slint::Image) {
        self.window.set_cover_image(image);
        self.window.set_has_cover_art(true);
    }

    /// Applies state from a [`crate::hardware::BtPlaybackState`] point-in-time snapshot.
    pub fn apply_bluetooth_playback(&self, playback: &crate::hardware::BtPlaybackState) {
        self.set_track_metadata(
            playback.title.as_ref(),
            playback.artist.as_ref(),
            playback.album.as_ref(),
        );
        self.set_playback_state(
            playback.is_playing(),
            playback.position_ms,
            playback.duration_ms,
        );
        if !playback.device_name.is_empty() {
            self.set_track_or_device(playback.device_name.as_ref());
        }
    }

    /// Configures the shell with demo playback for visual preview and verification.
    pub fn apply_demo_playback(&self) {
        self.set_tab(0);
        self.set_source("Bluetooth");
        self.set_track_or_device("Pixel 10 Pro XL");
        self.set_stream_codec("aptX-HD 48kHz");
        self.set_track_metadata("Money for Nothing", "Dire Straits", "Brothers in Arms");
        self.set_stream_details("aptX-HD 576k | 48kHz/16-bit ➔ DSP 32-bit | S: 0.999974");
        self.set_playback_state(true, 105_000, 230_000);
        self.set_volume(78.0);
        self.set_peak_levels(0.72, 0.68);
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
                let current = w.get_is_playing();
                let _ = bt_toggle.player_toggle();
                w.set_is_playing(!current);
            }
        });

        let bt_next = bt.clone();
        self.window.on_next_track(move || {
            let _ = bt_next.player_next();
        });

        let bt_prev = bt.clone();
        self.window.on_previous_track(move || {
            let _ = bt_prev.player_previous();
        });

        let bt_seek = bt;
        let dur_atomic = Arc::clone(&self.duration_ms);
        let win = self.window.as_weak();
        self.window.on_seek(move |ratio| {
            if let Some(w) = win.upgrade() {
                let clamped = if ratio.is_finite() {
                    ratio.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                w.set_progress_ratio(clamped);
                let dur = dur_atomic.load(Ordering::Relaxed);
                #[expect(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "clamped is non-negative <= 1.0 and dur is positive u32"
                )]
                let pos = (clamped * dur as f32).round() as u32;
                w.set_position_text(crate::hardware::format_duration_ms(pos).into());
                let _ = bt_seek.player_seek(pos);
            }
        });
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

        let win = self.window.as_weak();
        self.window.on_toggle_play_pause(move || {
            if let Some(w) = win.upgrade() {
                let current = w.get_is_playing();
                w.set_is_playing(!current);
                tracing::debug!("Play/Pause toggled to {}", !current);
            }
        });

        let win = self.window.as_weak();
        self.window.on_toggle_mute(move || {
            if let Some(w) = win.upgrade() {
                let current = w.get_is_muted();
                w.set_is_muted(!current);
                tracing::debug!("Mute toggled to {}", !current);
            }
        });

        let dur_atomic = Arc::clone(&self.duration_ms);
        let win = self.window.as_weak();
        self.window.on_seek(move |ratio| {
            if let Some(w) = win.upgrade() {
                let clamped = if ratio.is_finite() {
                    ratio.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                w.set_progress_ratio(clamped);
                let dur = dur_atomic.load(Ordering::Relaxed);
                #[expect(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "clamped is non-negative <= 1.0 and dur is positive u32"
                )]
                let pos = (clamped * dur as f32).round() as u32;
                w.set_position_text(crate::hardware::format_duration_ms(pos).into());
                tracing::debug!("Seek to ratio: {clamped}");
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
