//! Application UI Shell controller and view manager.
//!
//! Orchestrates the 800x480 appliance user interface:
//! - Top status bar ([`TopStatusBar`])
//! - Central active viewport ([`MainViewport`])
//! - Bottom navigation dock ([`NavDock`])
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`).

use std::cell::RefCell;
use std::fmt::{self, Debug, Formatter};

use slint::ComponentHandle;

use super::framebuffer::MarqueeRowSpec;
use super::generated::{MainWindow, MarqueeCompose, Theme};
use super::marquee::{MarqueeStripCache, MarqueeTypography, StripRequest};
use super::theme::ThemePreset;

/// Shared state and interactive controller for [`MainWindow`].
pub struct AppShell {
    window: MainWindow,
    strips: RefCell<MarqueeStripCache>,
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
            strips: RefCell::new(MarqueeStripCache::default()),
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
    /// Sets the marquee scroll rate in fps (30 or 60) on the shared composer global.
    pub fn set_marquee_fps(&self, fps: u32) {
        self.window
            .global::<MarqueeCompose>()
            .set_marquee_fps(i32::try_from(fps).unwrap_or(i32::MAX));
    }

    /// Toggles Rust-side marquee composition (strip images render statically in Slint).
    pub fn set_marquee_compose_in_rust(&self, compose: bool) {
        self.window
            .global::<MarqueeCompose>()
            .set_compose_in_rust(compose);
    }

    /// Suspends or resumes marquee strip rendering for the one-frame background capture
    /// (`visible = false` keeps the strip layout geometry but renders nothing).
    pub fn set_marquee_strips_visible(&self, visible: bool) {
        self.window
            .global::<MarqueeCompose>()
            .set_capture_pending(!visible);
    }

    /// Marquee viewport rectangles `(x, y, width, height)` in framebuffer pixels, read
    /// from the shared composer global (clamped to the fixed 800x480 panel).
    #[must_use]
    pub fn marquee_geometry(&self) -> Vec<(u32, u32, u32, u32)> {
        let g = self.window.global::<MarqueeCompose>();
        let px = |v: f32, max: f32| u32::try_from(v.round().clamp(0.0, max) as i64).unwrap_or(0);
        let rect = |origin: slint::LogicalPosition, w: f32, h: f32| {
            (
                px(origin.x, 800.0),
                px(origin.y, 480.0),
                px(w, 800.0),
                px(h, 480.0),
            )
        };
        vec![rect(g.get_title_origin(), g.get_title_w(), g.get_title_h())]
    }

    /// Builds [`MarqueeRowSpec`]s for the Rust-side composer from the shared global and
    /// the cached strip pixels; rows are omitted when the text does not overflow.
    ///
    /// # Panics
    ///
    /// Panics if the strip-cache mutex was poisoned by a prior panic in this thread.
    #[must_use]
    pub fn marquee_row_specs(&self) -> Vec<MarqueeRowSpec> {
        let g = self.window.global::<MarqueeCompose>();
        if !g.get_compose_in_rust() {
            return Vec::new();
        }
        let px = |v: f32, max: f32| u32::try_from(v.round().clamp(0.0, max) as i64).unwrap_or(0);
        let row = |origin: slint::LogicalPosition,
                   w: f32,
                   h: f32,
                   scroll: i32,
                   cycle: i32,
                   overflow: bool,
                   strip: Option<&super::marquee::MarqueeStrip>| {
            let strip = strip.filter(|s| !s.is_empty())?;
            // Non-overflow rows are composed too (Slint strips are hidden in compose
            // mode): a single unscrolled tile with a cycle wider than the viewport so
            // the tiling loop never draws a second copy inside the row.
            let (scroll_px, cycle_px) = if overflow {
                (
                    u32::try_from(scroll).unwrap_or(0),
                    u32::try_from(cycle).unwrap_or(0),
                )
            } else {
                (0, px(w, 800.0) + 1)
            };
            Some(MarqueeRowSpec {
                rect: (
                    px(origin.x, 800.0),
                    px(origin.y, 480.0),
                    px(w, 800.0),
                    px(h, 480.0),
                ),
                scroll_px,
                cycle_px,
                strip: strip.pixels_arc(),
                strip_w: strip.width(),
                strip_id: strip.strip_id(),
            })
        };
        vec![row(
            g.get_title_origin(),
            g.get_title_w(),
            g.get_title_h(),
            g.get_title_scroll_px(),
            g.get_title_cycle_px(),
            g.get_title_overflow(),
            self.strips.borrow().title(),
        )]
        .into_iter()
        .flatten()
        .collect()
    }

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
        self.refresh_marquee_strips();
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
        let ratio = speaker_bluetooth::playback_progress_ratio(position_ms, duration_ms);
        self.window.set_progress_ratio(ratio);
        self.window
            .set_position_text(speaker_bluetooth::format_duration_ms(position_ms).into());
        self.window
            .set_duration_text(speaker_bluetooth::format_duration_ms(duration_ms).into());
    }

    /// Applies a full [`BtPlaybackState`] snapshot to the window properties.
    ///
    /// Called by the bt-monitor worker (and available to integration tests to
    /// simulate the worker tick) after fresh state is fetched on a background
    /// thread; the transport control callbacks intentionally do not fetch or
    /// apply state themselves to avoid blocking D-Bus/OBEX round-trips on the
    /// render thread.
    pub fn refresh_playback_display(&self, playback: &speaker_bluetooth::BtPlaybackState) {
        Self::apply_playback_to_window(&self.window, playback);
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

    /// Applies audio stream metrics from an active [`speaker_bluetooth::BlueAlsaPcm`].
    pub fn apply_pcm_stream(&self, pcm: &speaker_bluetooth::BlueAlsaPcm) {
        // Guarded setters: repeated identical property sets still notify Slint bindings
        // and force repeated partial invalidations on every 250 ms worker push.
        if !pcm.selected_codec.is_empty()
            && self.window.get_stream_codec().as_str() != pcm.selected_codec.as_ref()
        {
            self.set_stream_codec(pcm.selected_codec.as_ref());
        }
        let rate_khz = if pcm.sampling_rate > 0 {
            format!("{} kHz", pcm.sampling_rate / 1000)
        } else {
            "48 kHz".to_string()
        };
        if self.window.get_stream_sample_rate().as_str() != rate_khz.as_str() {
            self.set_stream_sample_rate(rate_khz);
        }

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
        if self.window.get_stream_bit_depth().as_str() != bit_depth {
            self.set_stream_bit_depth(bit_depth);
        }
        if self.window.get_is_muted() != pcm.muted {
            self.window.set_is_muted(pcm.muted);
        }
        if let Some(pct) = pcm.volume_percent {
            let volume = f32::from(pct);
            if (self.window.get_volume_value() - volume).abs() > f32::EPSILON {
                self.set_volume(volume);
            }
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
    ///
    /// Cover art path tracking (dedup) lives on the `bt-monitor` worker; the shell
    /// only mirrors the presence flag and the decoded image pushed by the worker.
    pub fn set_has_cover_art(&self, has_art: bool) {
        self.window.set_has_cover_art(has_art);
    }

    /// Updates the cover art image.
    pub fn set_cover_image(&self, image: slint::Image) {
        self.window.set_cover_image(image);
        self.window.set_has_cover_art(true);
    }

    /// Applies state from a [`speaker_bluetooth::BtPlaybackState`] point-in-time snapshot.
    pub fn apply_bluetooth_playback(&self, playback: &speaker_bluetooth::BtPlaybackState) {
        Self::apply_playback_to_window(&self.window, playback);
        self.refresh_marquee_strips();
    }

    fn apply_playback_to_window(w: &MainWindow, playback: &speaker_bluetooth::BtPlaybackState) {
        // Only notify Slint property bindings when the value actually changed: the
        // Bluetooth worker re-applies state every 250 ms position tick, and
        // unconditional `set_track_*` calls invalidate `MarqueeText` layout bindings
        // and force text re-shaping four times per second during active scrolling.
        if w.get_track_title().as_str() != playback.title.as_ref() {
            w.set_track_title(playback.title.as_ref().into());
        }
        if w.get_track_artist().as_str() != playback.artist.as_ref() {
            w.set_track_artist(playback.artist.as_ref().into());
        }
        if w.get_track_album().as_str() != playback.album.as_ref() {
            w.set_track_album(playback.album.as_ref().into());
        }
        #[expect(
            clippy::cast_possible_wrap,
            reason = "duration_ms up to i32::MAX fits cleanly in slint int"
        )]
        if w.get_duration_ms() != playback.duration_ms as i32 {
            w.set_duration_ms(playback.duration_ms as i32);
        }
        let playing = playback.is_playing();
        if w.get_is_playing() != playing {
            w.set_is_playing(playing);
        }
        let ratio =
            speaker_bluetooth::playback_progress_ratio(playback.position_ms, playback.duration_ms);
        w.set_progress_ratio(ratio);
        w.set_position_text(speaker_bluetooth::format_duration_ms(playback.position_ms).into());
        let duration_text = speaker_bluetooth::format_duration_ms(playback.duration_ms);
        if w.get_duration_text().as_str() != duration_text.as_str() {
            w.set_duration_text(duration_text.into());
        }
        if !playback.device_name.is_empty()
            && w.get_track_or_device().as_str() != playback.device_name.as_ref()
        {
            w.set_track_or_device(playback.device_name.as_ref().into());
        }

        // Cover art is loaded and upscaled by the `bt-monitor` worker (off the render
        // thread) and pushed through the shared worker update channel; playback
        // snapshots never clear it. A snapshot carrying no cached path means a
        // handle-less track or a fetch pending on the cover-fetch worker — blanking
        // here would drop working art on every 250 ms position tick. Only an
        // explicit device-disconnect delta clears the displayed cover.
    }

    /// Applies a theme preset to the UI shell.
    pub fn apply_theme(&self, preset: ThemePreset) {
        preset.apply(&self.window);
        self.refresh_marquee_strips();
    }

    /// Re-rasterizes the marquee strips when their text, color, or typography changed.
    ///
    /// Cached strips make this a no-op while metadata, theme, and typography are stable,
    /// so repeated playback-state ticks perform no offscreen rasterization.
    fn refresh_marquee_strips(&self) {
        let w = &self.window;
        let preset = self.current_theme();
        let palette = preset.palette();
        let rgb = |color: slint::Color| (color.red(), color.green(), color.blue());

        let title_request = StripRequest {
            text: w.get_track_title().as_str().into(),
            color: rgb(palette.text_primary),
            font_size_px: MarqueeTypography::TITLE_FONT_SIZE_PX,
            font_weight: MarqueeTypography::TITLE_FONT_WEIGHT,
            font_family: preset.font_family().into(),
        };

        let title_strip = {
            let mut cache = self.strips.borrow_mut();
            cache.rasterize_title(&title_request)
        };
        // Only touch the image properties when a strip actually re-rendered: setting an
        // identical property value still fires Slint change notifications and forces
        // (expensive) partial/full invalidations four times per second.
        if let Some(strip) = title_strip {
            w.set_title_marquee_strip(strip.image().clone());
        }
    }

    /// Cycles to the next available theme preset in cyclic order.
    #[must_use]
    pub fn cycle_theme(&self) -> ThemePreset {
        let next = self.current_theme().next();
        self.apply_theme(next);
        next
    }

    /// Wires the UI shell transport callbacks to the `bt-monitor` worker command queue.
    ///
    /// Every callback applies an optimistic local UI update and pushes a
    /// [`speaker_bluetooth::TransportCommand`] onto the queue; the `bt-monitor` worker
    /// executes the blocking D-Bus round-trips off the render thread and publishes
    /// authoritative state back through the shared worker update channel.
    pub fn wire_bluetooth_transport(&self, commands: &speaker_bluetooth::TransportCommandQueue) {
        let commands_toggle = commands.clone();
        let win = self.window.as_weak();
        self.window.on_toggle_play_pause(move || {
            if let Some(w) = win.upgrade() {
                // Optimistic local toggle: the worker's next authoritative publish
                // (within one monitor tick) reconciles the real player state.
                commands_toggle.push(speaker_bluetooth::TransportCommand::TogglePlayPause);
                w.set_is_playing(!w.get_is_playing());
            }
        });

        let commands_next = commands.clone();
        self.window.on_next_track(move || {
            commands_next.push(speaker_bluetooth::TransportCommand::NextTrack);
        });

        let commands_prev = commands.clone();
        self.window.on_previous_track(move || {
            commands_prev.push(speaker_bluetooth::TransportCommand::PreviousTrack);
        });

        let commands_seek = commands.clone();
        let win = self.window.as_weak();
        self.window.on_seek(move |ratio| {
            if let Some(w) = win.upgrade() {
                // Optimistic scrub: move the progress bar immediately, then let the
                // worker translate the absolute position into a D-Bus `Seek` call.
                let pos = Self::apply_seek_ratio(&w, ratio);
                commands_seek.push(speaker_bluetooth::TransportCommand::Seek(pos));
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
        w.set_position_text(speaker_bluetooth::format_duration_ms(pos).into());
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
