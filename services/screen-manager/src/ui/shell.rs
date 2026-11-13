//! Application UI Shell controller and view manager.
//!
//! Orchestrates the 800x480 appliance user interface:
//! - Top status bar ([`TopStatusBar`])
//! - Expanded main screen viewport ([`MainViewport`]) with Settings hub navigation
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`).

use std::cell::{Cell, RefCell};
use std::fmt::{self, Debug, Formatter};
use std::rc::Rc;

use slint::ComponentHandle;

use crate::config::{
    CoreConfig, DEFAULT_VOLUME_HUD_FADE_MS, DEFAULT_VOLUME_HUD_TIMEOUT_MS,
    DEFAULT_VOLUME_STEP_PERCENT, MAX_VOLUME_STEP_PERCENT, MIN_VOLUME_STEP_PERCENT,
};

use super::framebuffer::MarqueeRowSpec;
use super::generated::{AppTheme, MainWindow, MarqueeCompose};
use super::marquee::{MarqueeStripCache, MarqueeTypography, StripRequest};
use super::theme::ThemePreset;

/// Tracks cumulative [`speaker_hardware::EncoderStatus`] counters and translates
/// quadrature detents and push-switch presses into UI volume and mute actions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncoderVolumeTracker {
    clockwise: u64,
    counter_clockwise: u64,
    presses: u64,
}

impl EncoderVolumeTracker {
    /// Creates a new tracker initialized at zero counts.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            clockwise: 0,
            counter_clockwise: 0,
            presses: 0,
        }
    }

    /// Creates a tracker pre-seeded from an initial [`speaker_hardware::EncoderStatus`] snapshot.
    #[must_use]
    pub const fn seeded(status: &speaker_hardware::EncoderStatus) -> Self {
        Self {
            clockwise: status.cw_count,
            counter_clockwise: status.ccw_count,
            presses: status.press_count,
        }
    }

    /// Applies any delta between the previous counters and `status` to `shell`.
    ///
    /// Returns `true` if any rotary detent or switch press occurred.
    pub fn apply_status(
        &mut self,
        status: &speaker_hardware::EncoderStatus,
        shell: &AppShell,
    ) -> bool {
        let up_steps = status.cw_count.saturating_sub(self.clockwise);
        let down_steps = status.ccw_count.saturating_sub(self.counter_clockwise);
        let press_delta = status.press_count.saturating_sub(self.presses);
        self.clockwise = status.cw_count;
        self.counter_clockwise = status.ccw_count;
        self.presses = status.press_count;

        let up_detents = i32::try_from(up_steps).unwrap_or(i32::MAX);
        let down_detents = i32::try_from(down_steps).unwrap_or(i32::MAX);
        let net_detents = up_detents.saturating_sub(down_detents);

        if net_detents != 0 {
            let _ = shell.apply_volume_delta(net_detents);
        } else if up_steps > 0 {
            shell.show_volume_hud();
        }

        if press_delta > 0 {
            if press_delta % 2 == 1 {
                let _ = shell.toggle_mute_and_show_hud();
            } else {
                shell.show_volume_hud();
            }
        }

        up_steps > 0 || down_steps > 0 || press_delta > 0
    }

    /// Polls `encoder` and applies any counter deltas to `shell`.
    ///
    /// # Errors
    /// Propagates [`speaker_hardware::EncoderError`] if reading the encoder status fails.
    pub fn poll_and_apply(
        &mut self,
        encoder: &speaker_hardware::EncoderDevice,
        shell: &AppShell,
    ) -> Result<bool, speaker_hardware::EncoderError> {
        let status = encoder.read_status()?;
        Ok(self.apply_status(&status, shell))
    }
}

/// Placeholder shown for details that are unknown or not applicable.
const NO_DETAIL: &str = "—";

/// Point-in-time Bluetooth connection details shown on the details screen.
///
/// Built off the render thread from a [`speaker_bluetooth::LinkSnapshot`] and applied
/// by [`AppShell::apply_bt_link_details`]; `None` fields clear the matching text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BtLinkDetails {
    /// Connected remote device, if any.
    pub device: Option<speaker_bluetooth::BtDevice>,
    /// Local controller adapter, if detected.
    pub adapter: Option<speaker_bluetooth::BtAdapter>,
    /// Audio stream negotiated with the connected device, if any.
    pub pcm: Option<speaker_bluetooth::BlueAlsaPcm>,
}

impl BtLinkDetails {
    /// Selects the connected device (preferring `preferred`) and its audio stream.
    #[must_use]
    pub fn select(
        snapshot: &speaker_bluetooth::LinkSnapshot,
        preferred: Option<speaker_bluetooth::BtAddress>,
    ) -> Self {
        let device = preferred
            .and_then(|addr| {
                snapshot
                    .devices
                    .iter()
                    .find(|d| d.connected && d.address == addr)
            })
            .or_else(|| snapshot.devices.iter().find(|d| d.connected))
            .cloned();
        let pcm = device.as_ref().and_then(|d| {
            snapshot
                .pcms
                .iter()
                .find(|p| p.device_address == d.address)
                .cloned()
        });
        Self {
            device,
            adapter: snapshot.adapter.clone(),
            pcm,
        }
    }

    fn address_text(&self) -> String {
        self.device
            .as_ref()
            .map_or_else(|| NO_DETAIL.into(), |d| d.address.to_string())
    }

    fn link_text(&self) -> String {
        let Some(device) = &self.device else {
            return "Waiting for connection".into();
        };
        let mut parts = vec![if self.pcm.as_ref().is_some_and(|p| p.running) {
            "Streaming"
        } else {
            "Connected"
        }];
        if device.paired {
            parts.push("Paired");
        }
        if device.trusted {
            parts.push("Trusted");
        }
        parts.join(" • ")
    }

    fn rssi_text(&self) -> String {
        self.device
            .as_ref()
            .and_then(|d| d.rssi)
            .map_or_else(String::new, |rssi| format!("{rssi} dBm"))
    }

    fn battery_text(&self) -> String {
        self.device
            .as_ref()
            .and_then(|d| d.battery_percentage)
            .map_or_else(String::new, |pct| format!("{pct}%"))
    }

    fn stream_format_text(&self) -> String {
        let Some(pcm) = &self.pcm else {
            return NO_DETAIL.into();
        };
        let rate = if pcm.sampling_rate % 1000 == 0 {
            format!("{} kHz", pcm.sampling_rate / 1000)
        } else {
            format!("{:.1} kHz", f64::from(pcm.sampling_rate) / 1000.0)
        };
        let channels = match pcm.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} ch"),
        };
        format!("{rate} • {} • {channels}", pcm_bit_depth(&pcm.format))
    }

    fn delay_text(&self) -> String {
        self.pcm
            .as_ref()
            .and_then(|p| p.delay.as_deref())
            .unwrap_or(NO_DETAIL)
            .into()
    }

    fn adapter_name_text(&self) -> String {
        self.adapter.as_ref().map_or_else(
            || NO_DETAIL.into(),
            |a| {
                if a.alias.is_empty() {
                    a.name.to_string()
                } else {
                    a.alias.to_string()
                }
            },
        )
    }

    fn adapter_address_text(&self) -> String {
        self.adapter
            .as_ref()
            .map_or_else(|| NO_DETAIL.into(), |a| a.address.to_string())
    }
}

/// Maps a `BlueALSA` PCM sample-format identifier to a bit-depth label.
fn pcm_bit_depth(format: &str) -> &'static str {
    match format {
        "S16_LE" | "S16_BE" => "16-bit",
        "S24_LE" | "S24_BE" | "S24_3LE" => "24-bit",
        "S32_LE" | "S32_BE" => "32-bit",
        other if other.contains("24") => "24-bit",
        other if other.contains("32") => "32-bit",
        _ => "16-bit",
    }
}

/// Shared state and interactive controller for [`MainWindow`].
pub struct AppShell {
    window: MainWindow,
    strips: Rc<RefCell<MarqueeStripCache>>,
    volume_step_percent: Cell<u8>,
    transport_commands: Rc<RefCell<Option<speaker_bluetooth::TransportCommandQueue>>>,
    amp_commands: Rc<RefCell<Option<crate::volume::AmpVolumeCommandQueue>>>,
    last_toggle: Rc<Cell<std::time::Instant>>,
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
            strips: Rc::new(RefCell::new(MarqueeStripCache::default())),
            volume_step_percent: Cell::new(DEFAULT_VOLUME_STEP_PERCENT),
            transport_commands: Rc::new(RefCell::new(None)),
            amp_commands: Rc::new(RefCell::new(None)),
            last_toggle: Rc::new(Cell::new(
                std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .unwrap_or_else(std::time::Instant::now),
            )),
        };

        shell.set_volume_hud_timeout_ms(DEFAULT_VOLUME_HUD_TIMEOUT_MS);
        shell.set_volume_hud_fade_ms(DEFAULT_VOLUME_HUD_FADE_MS);
        shell.setup_callbacks();
        shell.apply_theme(ThemePreset::default());
        shell
    }

    /// Applies runtime parameters from [`CoreConfig`] (theme, marquee fps, volume step,
    /// and Volume HUD timeout/fade durations) to this shell.
    pub fn apply_config(&self, config: &CoreConfig) {
        self.set_marquee_fps(config.marquee_fps.value());
        self.set_volume_step_percent(config.volume_step_percent.value());
        self.set_volume_hud_timeout_ms(config.volume_hud_timeout_ms.value());
        self.set_volume_hud_fade_ms(config.volume_hud_fade_ms.value());
        if let Some(theme) = config.theme {
            self.apply_theme(theme);
        }
    }

    /// Returns the configured volume step percentage per rotary encoder detent (`1..=25`).
    #[must_use]
    pub fn volume_step_percent(&self) -> u8 {
        self.volume_step_percent.get()
    }

    /// Sets the volume step percentage per rotary encoder detent (clamped to `1..=25`).
    pub fn set_volume_step_percent(&self, step: u8) {
        self.volume_step_percent
            .set(step.clamp(MIN_VOLUME_STEP_PERCENT, MAX_VOLUME_STEP_PERCENT));
    }

    /// Returns the Volume HUD auto-dismiss timeout in milliseconds.
    #[must_use]
    pub fn volume_hud_timeout_ms(&self) -> u32 {
        u32::try_from(self.window.get_volume_hud_timeout_ms())
            .unwrap_or(DEFAULT_VOLUME_HUD_TIMEOUT_MS)
    }

    /// Sets the Volume HUD auto-dismiss timeout in milliseconds.
    pub fn set_volume_hud_timeout_ms(&self, timeout_ms: u32) {
        self.window
            .set_volume_hud_timeout_ms(i32::try_from(timeout_ms).unwrap_or(i32::MAX));
    }

    /// Returns the Volume HUD fade-out duration in milliseconds.
    #[must_use]
    pub fn volume_hud_fade_ms(&self) -> u32 {
        u32::try_from(self.window.get_volume_hud_fade_ms()).unwrap_or(DEFAULT_VOLUME_HUD_FADE_MS)
    }

    /// Sets the Volume HUD fade-out duration in milliseconds.
    pub fn set_volume_hud_fade_ms(&self, fade_ms: u32) {
        self.window
            .set_volume_hud_fade_ms(i32::try_from(fade_ms).unwrap_or(i32::MAX));
    }

    /// Returns whether the Volume HUD overlay is currently in its active hold state.
    #[must_use]
    pub fn is_volume_hud_visible(&self) -> bool {
        self.window.get_volume_hud_visible()
    }

    /// Returns the current opacity (`0.0..=1.0`) of the Volume HUD overlay.
    #[must_use]
    pub fn volume_hud_opacity(&self) -> f32 {
        self.window.get_volume_hud_opacity()
    }

    /// Returns `true` while the Volume HUD is visible or actively fading out (`opacity > 0.01`).
    #[must_use]
    pub fn is_volume_hud_active(&self) -> bool {
        self.is_volume_hud_visible() || self.volume_hud_opacity() > 0.01
    }

    /// Opens the Volume HUD overlay and resets its auto-dismiss countdown timer.
    pub fn show_volume_hud(&self) {
        self.window.invoke_show_volume_hud();
    }

    /// Immediately begins dismissing (fading out) the Volume HUD overlay.
    pub fn dismiss_volume_hud(&self) {
        self.window.invoke_dismiss_volume_hud();
    }

    /// Adjusts volume by `detents * volume_step_percent`, unmutes, opens/refreshes the
    /// Volume HUD, and fires the `volume_changed` callback.
    ///
    /// Returns the new clamped volume percentage (`0..=100`).
    pub fn apply_volume_delta(&self, detents: i32) -> u8 {
        let vol = self.volume();
        let current = if vol.is_finite() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "clamped to 0.0..=100.0 fits in i32"
            )]
            {
                vol.clamp(0.0, 100.0).round() as i32
            }
        } else {
            0
        };
        let delta = detents.saturating_mul(i32::from(self.volume_step_percent()));
        let target = u8::try_from(current.saturating_add(delta).clamp(0, 100)).unwrap_or(0);
        // The `volume_changed` callback owns the optimistic UI update, the
        // mute-clear (queued as an explicit `SetMute(false)` amp command when
        // the slider is moved while muted), and the Volume HUD refresh.
        self.window.invoke_volume_changed(f32::from(target));
        target
    }

    /// Toggles the mute state, opens/refreshes the Volume HUD, and returns the new mute state.
    pub fn toggle_mute_and_show_hud(&self) -> bool {
        self.window.invoke_toggle_mute();
        self.show_volume_hud();
        self.is_muted()
    }

    /// Returns a reference to the underlying Slint [`MainWindow`] handle.
    #[must_use]
    pub const fn window(&self) -> &MainWindow {
        &self.window
    }

    /// Returns the currently active theme preset.
    #[must_use]
    pub fn current_theme(&self) -> ThemePreset {
        ThemePreset::from_index(self.window.global::<AppTheme>().get_active_preset())
    }

    /// Returns the currently active screen tab index (0 to 5).
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

    /// Returns whether the raw marquee overlay should be suppressed
    /// (e.g. when not on the Now Playing tab or when the Volume HUD is active).
    #[must_use]
    pub fn should_suppress_marquee(&self) -> bool {
        self.current_tab() != 0 || self.is_volume_hud_active()
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
        // Suppress Rust-side marquee composition whenever the Now Playing tab is not
        // visible or while the Volume HUD overlay covers the middle viewport.
        if !g.get_compose_in_rust() || self.should_suppress_marquee() {
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

    /// Switches the active screen tab.
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

        let bit_depth = pcm_bit_depth(pcm.format.as_ref());
        if self.window.get_stream_bit_depth().as_str() != bit_depth {
            self.set_stream_bit_depth(bit_depth);
        }
        // PCM volume/mute are stream metadata owned by the phone (BlueALSA);
        // the on-screen level and mute indicator exclusively reflect the amp
        // via `apply_amp_volume`, so they are intentionally not applied here.
    }

    /// Applies the authoritative amp level/mute snapshot published by the
    /// `amp-volume` worker to the volume slider and mute indicator.
    pub fn apply_amp_volume(&self, state: &crate::volume::AmpVolumeState) {
        let volume = f32::from(state.level);
        if (self.window.get_volume_value() - volume).abs() >= 0.5 {
            self.set_volume(volume);
        }
        if self.window.get_is_muted() != state.muted {
            self.window.set_is_muted(state.muted);
        }
    }

    /// Wires the volume slider / mute toggle / encoder callbacks to the
    /// dedicated `amp-volume` worker command queue.
    ///
    /// Screen-initiated volume changes drive the amplifier directly
    /// (`audio-manager` D-Bus), not the Bluetooth PCM transport.
    pub fn wire_amp_volume(&self, commands: &crate::volume::AmpVolumeCommandQueue) {
        *self.amp_commands.borrow_mut() = Some(commands.clone());
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

    /// Applies Bluetooth connection details to the details screen (change-guarded).
    pub fn apply_bt_link_details(&self, details: &BtLinkDetails) {
        let w = &self.window;
        let connected = details.device.is_some();
        if w.get_bt_connected() != connected {
            w.set_bt_connected(connected);
        }
        macro_rules! guarded {
            ($get:ident, $set:ident, $text:expr) => {
                let text = $text;
                if w.$get().as_str() != text.as_str() {
                    w.$set(text.into());
                }
            };
        }
        guarded!(
            get_bt_device_address,
            set_bt_device_address,
            details.address_text()
        );
        guarded!(get_bt_link_text, set_bt_link_text, details.link_text());
        guarded!(get_bt_rssi_text, set_bt_rssi_text, details.rssi_text());
        guarded!(
            get_bt_battery_text,
            set_bt_battery_text,
            details.battery_text()
        );
        guarded!(
            get_bt_stream_format,
            set_bt_stream_format,
            details.stream_format_text()
        );
        guarded!(get_bt_delay_text, set_bt_delay_text, details.delay_text());
        guarded!(
            get_bt_adapter_name,
            set_bt_adapter_name,
            details.adapter_name_text()
        );
        guarded!(
            get_bt_adapter_address,
            set_bt_adapter_address,
            details.adapter_address_text()
        );
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
        Self::refresh_marquee_strips_for(&self.window, &self.strips);
    }

    /// Free-function form of [`AppShell::refresh_marquee_strips`] callable from
    /// Slint callbacks that only hold a weak window handle and the shared strip cache.
    fn refresh_marquee_strips_for(w: &MainWindow, strips: &RefCell<MarqueeStripCache>) {
        let preset = ThemePreset::from_index(w.global::<AppTheme>().get_active_preset());
        // Use Material scheme's onBackground (equivalent to old text_primary)
        let schemes = preset.material_schemes();
        let text_primary = schemes.dark.onBackground;
        let rgb = |color: slint::Color| (color.red(), color.green(), color.blue());

        let title_request = StripRequest {
            text: w.get_track_title().as_str().into(),
            color: rgb(text_primary),
            font_size_px: MarqueeTypography::TITLE_FONT_SIZE_PX,
            font_weight: MarqueeTypography::TITLE_FONT_WEIGHT,
            font_family: preset.font_family().into(),
        };

        let title_strip = {
            let mut cache = strips.borrow_mut();
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
        *self.transport_commands.borrow_mut() = Some(commands.clone());
        self.last_toggle.set(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now),
        );
    }

    fn setup_callbacks(&self) {
        self.setup_navigation_callbacks();
        self.setup_transport_callbacks();
    }

    fn setup_navigation_callbacks(&self) {
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
        let strips = Rc::clone(&self.strips);
        self.window.on_select_theme(move |idx| {
            if let Some(w) = win.upgrade() {
                let preset = ThemePreset::from_index(idx);
                preset.apply(&w);
                Self::refresh_marquee_strips_for(&w, &strips);
            }
        });

        let win = self.window.as_weak();
        let strips = Rc::clone(&self.strips);
        self.window.on_cycle_theme(move || {
            if let Some(w) = win.upgrade() {
                let current = ThemePreset::from_index(w.global::<AppTheme>().get_active_preset());
                let next = current.next();
                next.apply(&w);
                Self::refresh_marquee_strips_for(&w, &strips);
            }
        });

        self.window.on_volume_clicked(|| {
            tracing::debug!("Top bar volume indicator clicked; showing Volume HUD");
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
    }

    fn setup_transport_callbacks(&self) {
        let last_toggle = Rc::clone(&self.last_toggle);
        let transport_play = Rc::clone(&self.transport_commands);
        let win = self.window.as_weak();
        self.window.on_toggle_play_pause(move || {
            let now = std::time::Instant::now();
            if now.duration_since(last_toggle.get()) < std::time::Duration::from_millis(300) {
                return;
            }
            last_toggle.set(now);
            if let Some(w) = win.upgrade() {
                if let Some(cmds) = transport_play.borrow().as_ref() {
                    cmds.push(speaker_bluetooth::TransportCommand::TogglePlayPause);
                    w.set_is_playing(!w.get_is_playing());
                } else {
                    tracing::debug!("Play/Pause requested (no audio transport wired)");
                }
            }
        });

        let transport_mute = Rc::clone(&self.amp_commands);
        let win = self.window.as_weak();
        self.window.on_toggle_mute(move || {
            if let Some(w) = win.upgrade() {
                let next_muted = !w.get_is_muted();
                w.set_is_muted(next_muted);
                w.invoke_show_volume_hud();
                if let Some(cmds) = transport_mute.borrow().as_ref() {
                    cmds.push(crate::volume::AmpVolumeCommand::SetMute(next_muted));
                }
                tracing::debug!("Amp mute toggled to {next_muted}");
            }
        });

        let transport_next = Rc::clone(&self.transport_commands);
        self.window.on_next_track(move || {
            if let Some(cmds) = transport_next.borrow().as_ref() {
                cmds.push(speaker_bluetooth::TransportCommand::NextTrack);
            } else {
                tracing::info!("Next track requested");
            }
        });

        let transport_prev = Rc::clone(&self.transport_commands);
        self.window.on_previous_track(move || {
            if let Some(cmds) = transport_prev.borrow().as_ref() {
                cmds.push(speaker_bluetooth::TransportCommand::PreviousTrack);
            } else {
                tracing::info!("Previous track requested");
            }
        });

        let transport_vol = Rc::clone(&self.amp_commands);
        let win = self.window.as_weak();
        self.window.on_volume_changed(move |val| {
            if let Some(w) = win.upgrade() {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped 0.0..=100.0 fits in u8"
                )]
                let clamped = if val.is_finite() {
                    val.round().clamp(0.0, 100.0) as u8
                } else {
                    0
                };
                let was_muted = w.get_is_muted();
                w.set_volume_value(f32::from(clamped));
                if was_muted {
                    w.set_is_muted(false);
                }
                w.invoke_show_volume_hud();
                if let Some(cmds) = transport_vol.borrow().as_ref() {
                    // A level change while muted audibly unmutes: queue both.
                    // The level write MUST be queued (and applied) before the
                    // unmute so the amp never plays its stored previous level.
                    cmds.push(crate::volume::AmpVolumeCommand::SetLevel(clamped));
                    if was_muted {
                        cmds.push(crate::volume::AmpVolumeCommand::SetMute(false));
                    }
                }
                tracing::debug!("Amp volume changed to {clamped}%");
            }
        });
    }
}
