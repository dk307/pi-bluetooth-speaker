//! Application UI Shell controller and view manager.
//!
//! Orchestrates the 800x480 appliance user interface:
//! - Top status bar ([`TopStatusBar`])
//! - Expanded main screen viewport ([`MainViewport`]) with Settings hub navigation
//!
//! Follows Microsoft Pragmatic Rust Guidelines (`M-SERVICES-CLONE`, `M-STRONG-TYPES`).

use std::cell::{Cell, RefCell};
use std::fmt::{self, Debug, Formatter};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::config::{
    CodecHudTimeoutMs, CoreConfig, DEFAULT_FONT_SCALE, DEFAULT_VOLUME_STEP_PERCENT, FontScale,
    MAX_CODEC_HUD_TIMEOUT_MS, MAX_FONT_SCALE, MAX_VOLUME_HUD_FADE_MS, MAX_VOLUME_HUD_TIMEOUT_MS,
    MAX_VOLUME_STEP_PERCENT, MIN_CODEC_HUD_TIMEOUT_MS, MIN_FONT_SCALE, MIN_VOLUME_HUD_TIMEOUT_MS,
    MIN_VOLUME_STEP_PERCENT, MarqueeFps, TitleDisplayMode, VolumeHudFadeMs, VolumeHudTimeoutMs,
    VolumeStepPercent,
};
use crate::settings_catalog::{
    SETTINGS_CATALOG, SettingControl, SettingId, SettingTier, apply_step, apply_toggle, row_state,
};

use super::backdrop::OVERLAY_TOP_INSET;
use super::generated::{
    AppTheme, MainWindow, MarqueeCompose, SettingControlKind, SettingRowItem, TouchScale,
};
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

/// Point-in-time Bluetooth connection details selected from a link snapshot.
///
/// Used by the worker to derive the active device for Devices-screen rows and,
/// at startup, to seed the initial PCM stream state.
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

    /// Adapter display name: alias when set, otherwise the adapter model name;
    /// `—` when no adapter was detected.
    #[must_use]
    pub fn adapter_name_text(&self) -> String {
        self.adapter.as_ref().map_or_else(
            || "—".to_string(),
            |a| {
                if a.alias.is_empty() {
                    a.name.to_string()
                } else {
                    a.alias.to_string()
                }
            },
        )
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

#[derive(Clone)]
struct SettingsModels {
    basic: Rc<VecModel<SettingRowItem>>,
    advanced: Rc<VecModel<SettingRowItem>>,
}

/// Seek-row presentation derived from the raw `BlueZ` timeline fields.
///
/// `BlueZ` keeps the "unknown" sentinel (`u32::MAX`) in the wire values, so the
/// presentation layer decides how much of the row is meaningful:
///
/// | `duration_known` | `position_known` | rendering |
/// | --- | --- | --- |
/// | yes | yes | determinate bar with fill and both readouts |
/// | no | yes | thin thumb-less track with a shimmer sweep, position only |
/// | no | no | row hidden, both readouts blank |
/// | yes | no | thin shimmer track with the duration readout, no fill |
///
/// A fill needs a position and the geometry needs a total, so anything short of
/// both is indeterminate rather than a permanently empty determinate bar.
#[derive(Debug)]
struct SeekRow {
    duration_known: bool,
    position_known: bool,
    /// Total length used for bar geometry; `0` when the timeline is unknown.
    timeline_ms: u32,
    /// Empty when the peer never reported an elapsed position: a stale `0:00`
    /// next to a live shimmer would read as "stuck".
    position_text: String,
    /// Empty when the peer never reported a total length; the shimmer carries
    /// the "playing, timeline unknown" signal instead.
    duration_text: String,
}

impl SeekRow {
    fn new(position_ms: u32, duration_ms: u32) -> Self {
        let duration_known = speaker_bluetooth::duration_known_ms(duration_ms);
        let position_known = speaker_bluetooth::position_known_ms(position_ms);
        // Only a determinate timeline feeds the ratio/bar geometry.
        let timeline_ms = if duration_known { duration_ms } else { 0 };
        let position_text = if position_known {
            speaker_bluetooth::format_duration_ms_ui(position_ms, timeline_ms)
        } else {
            String::new()
        };
        let duration_text = if duration_known {
            speaker_bluetooth::format_duration_ms_ui(duration_ms, duration_ms)
        } else {
            String::new()
        };
        Self {
            duration_known,
            position_known,
            timeline_ms,
            position_text,
            duration_text,
        }
    }
}

/// Shared state and interactive controller for [`MainWindow`].
pub struct AppShell {
    window: MainWindow,
    settings_models: SettingsModels,
    config_path: Rc<RefCell<Option<PathBuf>>>,
    persist_tx: Rc<RefCell<Option<std::sync::mpsc::Sender<CoreConfig>>>>,
    last_amp_volume: Cell<Option<crate::volume::AmpVolumeState>>,
    transport_commands: Rc<RefCell<Option<speaker_bluetooth::TransportCommandQueue>>>,
    amp_commands: Rc<RefCell<Option<crate::volume::AmpVolumeCommandQueue>>>,
    device_commands: Rc<RefCell<Option<speaker_bluetooth::DeviceCommandQueue>>>,
    bt_devices: Rc<RefCell<crate::ui::BtDevicesController>>,
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

/// Pushes `command` onto the wired Bluetooth device command queue, logging at
/// `debug` when no queue is wired (e.g. headless test shells).
fn push_device_command(
    queue: &Rc<RefCell<Option<speaker_bluetooth::DeviceCommandQueue>>>,
    command: speaker_bluetooth::DeviceCommand,
) {
    match queue.borrow().as_ref() {
        Some(commands) => commands.push(command),
        None => {
            tracing::debug!(
                ?command,
                "Bluetooth device command dropped (no device queue wired)"
            );
        }
    }
}

impl AppShell {
    /// Initializes an [`AppShell`] controller wrapping a [`MainWindow`] component.
    #[must_use]
    pub fn new(window: MainWindow) -> Self {
        let default_cfg = CoreConfig::default();
        let settings_models = SettingsModels {
            basic: Rc::new(VecModel::from(Self::build_setting_rows(
                &default_cfg,
                SettingTier::Basic,
            ))),
            advanced: Rc::new(VecModel::from(Self::build_setting_rows(
                &default_cfg,
                SettingTier::Advanced,
            ))),
        };
        window.set_basic_settings(ModelRc::from(Rc::clone(&settings_models.basic)));
        window.set_advanced_settings(ModelRc::from(Rc::clone(&settings_models.advanced)));

        let shell = Self {
            window,
            settings_models,
            config_path: Rc::new(RefCell::new(None)),
            persist_tx: Rc::new(RefCell::new(None)),
            last_amp_volume: Cell::new(None),
            transport_commands: Rc::new(RefCell::new(None)),
            amp_commands: Rc::new(RefCell::new(None)),
            device_commands: Rc::new(RefCell::new(None)),
            bt_devices: Rc::new(RefCell::new(crate::ui::BtDevicesController::default())),
            last_toggle: Rc::new(Cell::new(
                std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .unwrap_or_else(std::time::Instant::now),
            )),
        };

        // One-shot seed, not runtime config: the blur pipeline crops at
        // OVERLAY_TOP_INSET, so the overlay geometry in Slint must follow the
        // same constant (the `60px` literal in appwindow.slint is preview-only).
        shell.window.set_overlay_top_inset(OVERLAY_TOP_INSET as f32);
        shell.set_volume_step_percent(VolumeStepPercent::DEFAULT);
        shell.set_volume_hud_timeout_ms(VolumeHudTimeoutMs::DEFAULT);
        shell.set_volume_hud_fade_ms(VolumeHudFadeMs::DEFAULT);
        shell.set_codec_hud_timeout_ms(CodecHudTimeoutMs::DEFAULT);
        shell.sync_settings_model();
        shell.setup_callbacks();
        shell.apply_theme(ThemePreset::default());
        shell
    }

    /// Applies runtime parameters from [`CoreConfig`] (theme, marquee fps, title display mode,
    /// volume step, Volume HUD timeout/fade durations, and Codec HUD timeout) to this shell.
    pub fn apply_config(&self, config: &CoreConfig) {
        Self::apply_config_to_window(&self.window, config);
        Self::sync_settings_models_for(&self.window, &self.settings_models);
        self.set_font_scale(config.font_scale.value());
        if let Some(theme) = config.theme {
            self.apply_theme(theme);
        }
    }

    fn apply_config_to_window(window: &MainWindow, config: &CoreConfig) {
        window
            .global::<MarqueeCompose>()
            .set_marquee_fps(Self::slint_i32(config.marquee_fps.value(), "marquee_fps"));
        window.set_title_split_mode(config.title_display_mode.is_split());
        window.set_volume_step_percent(i32::from(config.volume_step_percent.value()));
        window.set_volume_hud_timeout_ms(Self::slint_i32(
            config.volume_hud_timeout_ms.value(),
            "volume_hud_timeout_ms",
        ));
        window.set_volume_hud_fade_ms(Self::slint_i32(
            config.volume_hud_fade_ms.value(),
            "volume_hud_fade_ms",
        ));
        window.set_codec_hud_timeout_ms(Self::slint_i32(
            config.codec_hud_timeout_ms.value(),
            "codec_hud_timeout_ms",
        ));
    }

    fn build_setting_rows(config: &CoreConfig, tier: SettingTier) -> Vec<SettingRowItem> {
        SETTINGS_CATALOG
            .iter()
            .filter(|desc| desc.tier == tier)
            .map(|desc| {
                let state = row_state(config, desc);
                let (kind, left_label, right_label) = match desc.control {
                    SettingControl::Stepper { .. } => (
                        SettingControlKind::Stepper,
                        slint::SharedString::new(),
                        slint::SharedString::new(),
                    ),
                    SettingControl::TwoOption {
                        left_label,
                        right_label,
                    } => (
                        SettingControlKind::TwoOption,
                        left_label.into(),
                        right_label.into(),
                    ),
                };
                SettingRowItem {
                    id: desc.key().into(),
                    label: desc.label.into(),
                    is_advanced: desc.is_advanced(),
                    kind,
                    value_text: state.value_text.into(),
                    can_decrease: state.can_decrease,
                    can_increase: state.can_increase,
                    left_label,
                    right_label,
                    right_selected: state.right_selected,
                }
            })
            .collect()
    }

    fn sync_settings_model(&self) {
        Self::sync_settings_models_for(&self.window, &self.settings_models);
    }

    fn sync_settings_models_for(window: &MainWindow, models: &SettingsModels) {
        let config = Self::config_from_window(window);
        for (tier, model) in [
            (SettingTier::Basic, &models.basic),
            (SettingTier::Advanced, &models.advanced),
        ] {
            for (idx, row) in Self::build_setting_rows(&config, tier)
                .into_iter()
                .enumerate()
            {
                if idx < model.row_count() {
                    model.set_row_data(idx, row);
                }
            }
        }
    }

    /// Returns the configured volume step percentage per rotary encoder detent.
    #[must_use]
    pub fn volume_step_percent(&self) -> VolumeStepPercent {
        Self::volume_step_percent_from_window(&self.window)
    }

    /// Sets the volume step percentage per rotary encoder detent.
    ///
    /// The [`VolumeStepPercent`] parameter guarantees the `1..=25` invariant by
    /// construction; the validated value is written to the Slint property verbatim.
    pub fn set_volume_step_percent(&self, step: VolumeStepPercent) {
        self.window.set_volume_step_percent(i32::from(step.value()));
        self.sync_settings_model();
    }

    /// Returns the selected marquee scroll rate in fps (`30` or `60`).
    #[must_use]
    pub fn marquee_fps(&self) -> MarqueeFps {
        Self::marquee_fps_from_window(&self.window)
    }

    /// Sets the marquee scroll rate in fps (`30` or `60`) on the shared composer global.
    ///
    /// The [`MarqueeFps`] parameter guarantees the `30`/`60` invariant by
    /// construction; the validated value is written to the Slint property verbatim.
    pub fn set_marquee_fps(&self, fps: MarqueeFps) {
        self.window
            .global::<MarqueeCompose>()
            .set_marquee_fps(Self::slint_i32(fps.value(), "marquee_fps"));
        self.sync_settings_model();
    }

    /// Returns the active track title display mode (`Marquee` or `Split`).
    #[must_use]
    pub fn title_display_mode(&self) -> TitleDisplayMode {
        TitleDisplayMode::from_split(self.window.get_title_split_mode())
    }

    /// Sets the track title display mode (`Marquee` or `Split`).
    pub fn set_title_display_mode(&self, mode: TitleDisplayMode) {
        self.window.set_title_split_mode(mode.is_split());
        self.sync_settings_model();
    }

    /// Returns the Volume HUD auto-dismiss timeout.
    #[must_use]
    pub fn volume_hud_timeout_ms(&self) -> VolumeHudTimeoutMs {
        Self::volume_hud_timeout_from_window(&self.window)
    }

    /// Sets the Volume HUD auto-dismiss timeout.
    ///
    /// The [`VolumeHudTimeoutMs`] parameter guarantees the `250..=30000`
    /// invariant by construction; the validated value is written to the Slint
    /// property verbatim.
    pub fn set_volume_hud_timeout_ms(&self, timeout_ms: VolumeHudTimeoutMs) {
        self.window.set_volume_hud_timeout_ms(Self::slint_i32(
            timeout_ms.value(),
            "volume_hud_timeout_ms",
        ));
        self.sync_settings_model();
    }

    /// Returns the Volume HUD fade-out duration.
    #[must_use]
    pub fn volume_hud_fade_ms(&self) -> VolumeHudFadeMs {
        Self::hud_fade_from_window(&self.window)
    }

    /// Sets the Volume HUD fade-out duration.
    ///
    /// The [`VolumeHudFadeMs`] parameter guarantees the `0..=2000` invariant
    /// by construction; the validated value is written to the Slint property
    /// verbatim.
    pub fn set_volume_hud_fade_ms(&self, fade_ms: VolumeHudFadeMs) {
        self.window
            .set_volume_hud_fade_ms(Self::slint_i32(fade_ms.value(), "volume_hud_fade_ms"));
        self.sync_settings_model();
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

    /// Returns whether the codec information overlay is currently in its active hold state.
    #[must_use]
    pub fn is_codec_hud_visible(&self) -> bool {
        self.window.get_codec_hud_visible()
    }

    /// Returns the current opacity (`0.0..=1.0`) of the codec information overlay.
    #[must_use]
    pub fn codec_hud_opacity(&self) -> f32 {
        self.window.get_codec_hud_opacity()
    }

    /// Returns `true` while the codec overlay is visible or actively fading out.
    #[must_use]
    pub fn is_codec_hud_active(&self) -> bool {
        self.is_codec_hud_visible() || self.codec_hud_opacity() > 0.01
    }

    /// Opens the codec information overlay and resets its auto-dismiss countdown.
    pub fn show_codec_hud(&self) {
        self.window.invoke_show_codec_hud();
    }

    /// Immediately begins dismissing (fading out) the codec information overlay.
    pub fn dismiss_codec_hud(&self) {
        self.window.invoke_dismiss_codec_hud();
    }

    /// Publishes the blurred snapshot that the overlay panels render behind
    /// themselves, replacing the previous screen with a frosted backdrop.
    ///
    /// Publishing a non-empty image also marks the backdrop as available, which
    /// weakens the HUD scrim (the blur provides the separation); publishing an
    /// empty image (the release path) restores full scrim dimming.
    pub fn set_overlay_backdrop(&self, backdrop: slint::Image) {
        let available = backdrop.size().width > 0;
        self.window.set_overlay_backdrop(backdrop);
        self.window.set_overlay_backdrop_available(available);
    }

    /// Returns `true` while the Volume HUD overlay owns the keyboard focus.
    #[must_use]
    pub fn volume_hud_has_focus(&self) -> bool {
        self.window.get_volume_hud_has_focus()
    }

    /// Returns `true` while the codec information overlay owns the keyboard focus.
    #[must_use]
    pub fn codec_hud_has_focus(&self) -> bool {
        self.window.get_codec_hud_has_focus()
    }

    /// Returns the Codec HUD auto-dismiss timeout.
    #[must_use]
    pub fn codec_hud_timeout_ms(&self) -> CodecHudTimeoutMs {
        Self::codec_hud_timeout_from_window(&self.window)
    }

    /// Sets the Codec HUD auto-dismiss timeout.
    ///
    /// The [`CodecHudTimeoutMs`] parameter guarantees the `250..=300000`
    /// invariant by construction; the validated value is written to the Slint
    /// property verbatim.
    pub fn set_codec_hud_timeout_ms(&self, timeout_ms: CodecHudTimeoutMs) {
        self.window
            .set_codec_hud_timeout_ms(Self::slint_i32(timeout_ms.value(), "codec_hud_timeout_ms"));
        self.sync_settings_model();
    }

    /// Binds the JSON path used by the live Screen Settings persistence.
    ///
    /// Spawns the single background persist worker: the render thread sends
    /// config snapshots over an unbounded FIFO channel (non-blocking), and
    /// the worker performs the blocking temp-file + fsync + rename writes.
    /// FIFO ordering guarantees the last UI edit wins on disk.
    pub fn wire_config(&self, path: &Path) {
        let path = path.to_path_buf();
        *self.config_path.borrow_mut() = Some(path.clone());
        let (tx, rx) = std::sync::mpsc::channel::<CoreConfig>();
        let worker_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("screen-settings-persist".into())
            .spawn(move || {
                for config in rx {
                    // M-PANIC-ON-BUG: a worker crash is a bug, not a
                    // recoverable I/O hiccup — re-raise instead of silently
                    // killing the worker and dropping queued snapshots.
                    // A failed save is logged by persist_config and dropped:
                    // the worker must survive to persist the next edit.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = Self::persist_config(&config, &worker_path);
                    }));
                    if let Err(p) = result {
                        std::panic::resume_unwind(p);
                    }
                }
            });
        match spawned {
            Ok(_) => {
                *self.persist_tx.borrow_mut() = Some(tx);
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    path = %path.display(),
                    "failed to spawn config persist worker; live settings not saved"
                );
            }
        }
    }

    /// Builds a [`CoreConfig`] snapshot from the current shell state.
    #[must_use]
    pub fn current_config(&self) -> CoreConfig {
        Self::config_from_window(&self.window)
    }

    /// Saves the current shell state as JSON to the wired config path.
    ///
    /// This is the synchronous path used by boot-time persistence and tests;
    /// UI edits go through the background persist worker instead (see
    /// [`Self::wire_config`]).
    ///
    /// # Errors
    /// Returns [`crate::config::ConfigError`] when no config path is wired or
    /// the atomic JSON write fails.
    pub fn save_current_config(&self) -> Result<(), crate::config::ConfigError> {
        let path = self.config_path.borrow().clone().ok_or_else(|| {
            crate::config::ConfigError::invalid("Screen Settings are not wired to a config path")
        })?;
        Self::persist_config(&self.current_config(), &path)
    }

    /// Snapshot builder shared by live persistence and the public getter.
    fn config_from_window(window: &MainWindow) -> CoreConfig {
        CoreConfig {
            theme: Some(ThemePreset::from_index(
                window.global::<AppTheme>().get_active_preset(),
            )),
            marquee_fps: Self::marquee_fps_from_window(window),
            title_display_mode: TitleDisplayMode::from_split(window.get_title_split_mode()),
            volume_step_percent: Self::volume_step_percent_from_window(window),
            volume_hud_timeout_ms: Self::volume_hud_timeout_from_window(window),
            volume_hud_fade_ms: Self::hud_fade_from_window(window),
            codec_hud_timeout_ms: Self::codec_hud_timeout_from_window(window),
            font_scale: FontScale::try_from(window.global::<TouchScale>().get_clamped_font_scale())
                .unwrap_or_default(),
        }
    }

    /// Saves a config snapshot to `path` and logs the outcome with structured fields.
    ///
    /// Returns the raw save result so synchronous callers
    /// ([`Self::save_current_config`]) propagate write failures; the background
    /// worker logs and drops it (a failed live save must not kill the worker).
    fn persist_config(config: &CoreConfig, path: &Path) -> Result<(), crate::config::ConfigError> {
        let result = config.save(path);
        match &result {
            Ok(()) => tracing::info!(
                path = %path.display(),
                theme = config.theme.map_or_else(|| "default", |t| t.slug()),
                marquee_fps = config.marquee_fps.value(),
                title_display_mode = config.title_display_mode.slug(),
                volume_step_percent = config.volume_step_percent.value(),
                volume_hud_timeout_ms = config.volume_hud_timeout_ms.value(),
                volume_hud_fade_ms = config.volume_hud_fade_ms.value(),
                codec_hud_timeout_ms = config.codec_hud_timeout_ms.value(),
                "Screen settings saved"
            ),
            Err(e) => tracing::error!(
                error = %e,
                path = %path.display(),
                "failed to save screen settings"
            ),
        }
        result
    }

    /// Queues the current window state for background persistence after a UI edit.
    ///
    /// # Render-Thread Safety (`M-NO-DBUS-ON-RENDER-THREAD`)
    ///
    /// `CoreConfig::save` performs temp-file + fsync + rename on the SD card,
    /// which can stall the render thread for tens of milliseconds per click.
    /// The config snapshot is built on the render thread (cheap) and sent over
    /// an unbounded FIFO channel to the single persist worker spawned by
    /// [`Self::wire_config`], keeping HUD animations, marquee, and touch input
    /// responsive. FIFO ordering guarantees the last UI edit wins on disk.
    fn persist_window_state(
        window: &MainWindow,
        persist_tx: &Rc<RefCell<Option<std::sync::mpsc::Sender<CoreConfig>>>>,
    ) {
        let Some(tx) = persist_tx.borrow().as_ref().cloned() else {
            tracing::warn!("Screen settings change not persisted: no config path is wired");
            return;
        };
        let config = Self::config_from_window(window);
        if tx.send(config).is_err() {
            tracing::warn!("Screen settings change not persisted: persist worker is gone");
        }
    }

    /// Returns the composer-global marquee rate, snapping out-of-domain values
    /// to the default rate (the Slint global is untyped beyond `i32`).
    fn marquee_fps_from_window(window: &MainWindow) -> MarqueeFps {
        u32::try_from(window.global::<MarqueeCompose>().get_marquee_fps())
            .ok()
            .and_then(|fps| MarqueeFps::try_from(fps).ok())
            .unwrap_or_default()
    }

    /// Returns the window volume-step property clamped into `1..=25`.
    fn volume_step_percent_from_window(window: &MainWindow) -> VolumeStepPercent {
        let step = u8::try_from(window.get_volume_step_percent())
            .unwrap_or(DEFAULT_VOLUME_STEP_PERCENT)
            .clamp(MIN_VOLUME_STEP_PERCENT, MAX_VOLUME_STEP_PERCENT);
        VolumeStepPercent::new(step)
    }

    /// Returns the window Volume HUD timeout clamped into `250..=30000`.
    fn volume_hud_timeout_from_window(window: &MainWindow) -> VolumeHudTimeoutMs {
        let ms = Self::slint_ms_to_u32(window.get_volume_hud_timeout_ms())
            .clamp(MIN_VOLUME_HUD_TIMEOUT_MS, MAX_VOLUME_HUD_TIMEOUT_MS);
        VolumeHudTimeoutMs::new(ms)
    }

    /// Returns the window Codec HUD timeout clamped into `250..=300000`.
    fn codec_hud_timeout_from_window(window: &MainWindow) -> CodecHudTimeoutMs {
        let ms = Self::slint_ms_to_u32(window.get_codec_hud_timeout_ms())
            .clamp(MIN_CODEC_HUD_TIMEOUT_MS, MAX_CODEC_HUD_TIMEOUT_MS);
        CodecHudTimeoutMs::new(ms)
    }

    /// Converts a Slint `i32` timeout to `u32` (negative values clamp to `0`).
    fn slint_ms_to_u32(value: i32) -> u32 {
        u32::try_from(value.max(0)).unwrap_or_default()
    }

    /// Converts a validated domain `u32` to the Slint `i32` property type.
    ///
    /// `field` names the settings field for diagnostics. Panics if the value
    /// exceeds `i32::MAX` — domain newtypes cap every field far below that
    /// bound, so a failure here is a violated invariant (M-PANIC-ON-BUG),
    /// not a recoverable condition.
    fn slint_i32(value: u32, field: &str) -> i32 {
        i32::try_from(value).unwrap_or_else(|_| panic!("{field} value {value} exceeds i32"))
    }

    /// Reads and clamps the Volume HUD fade duration window property into `0..=2000`.
    fn hud_fade_from_window(window: &MainWindow) -> VolumeHudFadeMs {
        let ms =
            Self::slint_ms_to_u32(window.get_volume_hud_fade_ms()).clamp(0, MAX_VOLUME_HUD_FADE_MS);
        VolumeHudFadeMs::new(ms)
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
        let delta = detents.saturating_mul(i32::from(self.volume_step_percent().value()));
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

    /// Default touch font scale (`1.15`).
    pub const DEFAULT_FONT_SCALE: f32 = DEFAULT_FONT_SCALE;

    /// Minimum allowed touch font scale (`1.0`).
    pub const MIN_FONT_SCALE: f32 = MIN_FONT_SCALE;

    /// Maximum allowed touch font scale (`1.35`).
    pub const MAX_FONT_SCALE: f32 = MAX_FONT_SCALE;

    /// Returns the active clamped touch font scale (`1.0..=1.35`).
    #[must_use]
    pub fn font_scale(&self) -> f32 {
        self.window.global::<TouchScale>().get_clamped_font_scale()
    }

    /// Returns the active damped widget scale (`1.0 + (font_scale - 1.0) * 0.5`).
    #[must_use]
    pub fn widget_scale(&self) -> f32 {
        self.window.global::<TouchScale>().get_widget_scale()
    }

    /// Sets the touch font scale (clamped to `1.0..=1.35`, defaulting to `1.15` on `NaN`).
    pub fn set_font_scale(&self, scale: f32) {
        let clamped = if scale.is_finite() {
            scale.clamp(Self::MIN_FONT_SCALE, Self::MAX_FONT_SCALE)
        } else {
            Self::DEFAULT_FONT_SCALE
        };
        let prev = self.font_scale();
        self.window.global::<TouchScale>().set_font_scale(clamped);
        if prev.to_bits() != self.font_scale().to_bits() {
            Self::refresh_marquee_strip(&self.window);
        }
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
        let title: slint::SharedString = title.into();
        Self::apply_track_title(&self.window, title.as_str());
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
        Self::apply_playback_timing(&self.window, is_playing, position_ms, duration_ms);
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
        // Codec-HUD summary line and codec delay follow the active stream
        // (they used to come from the retired tab-2 details screen).
        let rate = if pcm.sampling_rate.is_multiple_of(1000) {
            format!("{} kHz", pcm.sampling_rate / 1000)
        } else {
            format!("{:.1} kHz", f64::from(pcm.sampling_rate) / 1000.0)
        };
        let channels = match pcm.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} ch"),
        };
        let format_text = format!("{rate} • {bit_depth} • {channels}");
        if self.window.get_bt_stream_format().as_str() != format_text {
            self.window.set_bt_stream_format(format_text.into());
        }
        let delay_text = pcm.delay.as_deref().unwrap_or("—");
        if self.window.get_bt_delay_text().as_str() != delay_text {
            self.window.set_bt_delay_text(delay_text.into());
        }
        // PCM volume/mute are stream metadata owned by the phone (BlueALSA);
        // the on-screen level and mute indicator exclusively reflect the amp
        // via `apply_amp_volume`, so they are intentionally not applied here.
    }

    /// Applies the authoritative amp level/mute snapshot published by the
    /// `amp-volume` worker to the volume slider and mute indicator.
    ///
    /// The initial snapshot synchronizes the UI silently. Later changes open or
    /// refresh the Volume HUD, including changes originating outside the screen
    /// (for example, a phone changing the amplifier volume).
    pub fn apply_amp_volume(&self, state: &crate::volume::AmpVolumeState) {
        let previous = self.last_amp_volume.replace(Some(*state));
        let volume = f32::from(state.level);
        if (self.window.get_volume_value() - volume).abs() >= 0.5 {
            self.set_volume(volume);
        }
        if self.window.get_is_muted() != state.muted {
            self.window.set_is_muted(state.muted);
        }
        if previous.is_some_and(|previous| previous != *state) {
            self.show_volume_hud();
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
    }

    fn refresh_marquee_strip(w: &MainWindow) {
        let title = w.get_track_title();
        let font_scale = w.global::<TouchScale>().get_clamped_font_scale();
        let on_bg = w.global::<super::MaterialPalette>().get_on_background();
        let strip = crate::ui::marquee::rasterize_marquee_strip(
            title.as_str(),
            font_scale,
            (on_bg.red(), on_bg.green(), on_bg.blue()),
        );
        w.global::<MarqueeCompose>().set_title_strip(strip);
    }

    fn apply_track_title(w: &MainWindow, title: &str) {
        // Only notify Slint property bindings when the value actually changed: the
        // Bluetooth worker re-applies state every 250 ms position tick, and
        // unconditional `set_track_*` calls invalidate `MarqueeText` layout bindings
        // and force text re-shaping four times per second during active scrolling.
        if w.get_track_title().as_str() != title {
            let split = speaker_title_breaker::split_song_title_with_config(
                title,
                &speaker_title_breaker::BreakerConfig {
                    min_wrap_chars: 20,
                    max_line1_chars: Some(24),
                },
            );
            w.set_track_title(title.into());
            w.set_track_title_line1(split.line1.into());
            w.set_track_title_line2(split.line2.into());
            Self::refresh_marquee_strip(w);
        }
    }

    fn apply_playback_timing(w: &MainWindow, is_playing: bool, position_ms: u32, duration_ms: u32) {
        /// Minimum progress-bar ratio delta (~1 px on a ~320 px seek bar)
        /// before dirtying Slint's `progress_ratio` binding; sub-pixel 250 ms
        /// increments are visually imperceptible yet trigger full-window
        /// `FemtoVG` redraws.
        const PROGRESS_RATIO_MIN_STEP: f32 = 1.0 / 320.0;

        let row = SeekRow::new(position_ms, duration_ms);
        if w.get_duration_known() != row.duration_known {
            w.set_duration_known(row.duration_known);
        }
        if w.get_position_known() != row.position_known {
            w.set_position_known(row.position_known);
        }

        // Slint's `duration_ms` is an `i32`, while `duration_known_ms` only
        // rejects `0` and the `u32::MAX` sentinel: a peer advertising a
        // multi-week "duration" would wrap negative through an `as` cast. Clamp
        // at the boundary instead of asserting an invariant the caller cannot
        // enforce, so the value handed to the view is always a sane total.
        let duration_ms_i32 = i32::try_from(row.timeline_ms).unwrap_or(i32::MAX);
        if w.get_duration_ms() != duration_ms_i32 {
            w.set_duration_ms(duration_ms_i32);
        }
        if w.get_is_playing() != is_playing {
            w.set_is_playing(is_playing);
        }
        let ratio = speaker_bluetooth::playback_progress_ratio(position_ms, row.timeline_ms);
        let prev_ratio = w.get_progress_ratio();
        let (ratio_bits, prev_bits) = (ratio.to_bits(), prev_ratio.to_bits());
        let at_boundary = (ratio_bits == 0.0_f32.to_bits() && prev_bits != 0.0_f32.to_bits())
            || (ratio_bits == 1.0_f32.to_bits() && prev_bits != 1.0_f32.to_bits());
        if at_boundary || (ratio - prev_ratio).abs() >= PROGRESS_RATIO_MIN_STEP {
            w.set_progress_ratio(ratio);
        }
        if w.get_position_text().as_str() != row.position_text.as_str() {
            w.set_position_text(row.position_text.into());
        }
        if w.get_duration_text().as_str() != row.duration_text.as_str() {
            w.set_duration_text(row.duration_text.into());
        }
    }

    fn apply_playback_to_window(w: &MainWindow, playback: &speaker_bluetooth::BtPlaybackState) {
        Self::apply_track_title(w, playback.title.as_ref());
        if w.get_track_artist().as_str() != playback.artist.as_ref() {
            w.set_track_artist(playback.artist.as_ref().into());
        }
        if w.get_track_album().as_str() != playback.album.as_ref() {
            w.set_track_album(playback.album.as_ref().into());
        }
        Self::apply_playback_timing(
            w,
            playback.is_playing(),
            playback.position_ms,
            playback.duration_ms,
        );
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

    /// Applies a Bluetooth devices screen update pushed by the `bt-monitor`
    /// worker (change-guarded via [`crate::ui::BtDevicesController`]).
    ///
    /// Row/scanning state is full-state (replaced wholesale); alias, pairing
    /// status, and toast text are one-shot deltas applied only when present.
    pub fn apply_bt_devices(&self, update: crate::ui::BtDevicesUpdate) {
        if !self.bt_devices.borrow_mut().accepts(&update) {
            return;
        }
        let w = &self.window;

        if let Some(alias) = &update.adapter_alias {
            w.set_bt_devices_subtitle(format!("Visible as \"{alias}\"").into());
        }
        w.set_bt_scanning(update.scanning);
        w.set_bt_device_rows(slint::ModelRc::new(slint::VecModel::from(update.rows)));
        w.set_bt_discovered_rows(slint::ModelRc::new(slint::VecModel::from(
            update.discovered,
        )));

        // Status-bar connected indicator: any device with an active BlueZ
        // connection among the paired/connected rows (change-guarded; the
        // old tab-2 details screen fed this from BtLinkDetails).
        let connected = w.get_bt_device_rows().iter().any(|row| row.connected);
        if w.get_bt_connected() != connected {
            w.set_bt_connected(connected);
        }

        if update.action_settled || update.toast.is_some() {
            w.set_bt_pending_address(String::new().into());
        }

        let screen_open = w.get_bt_devices_open();

        // Terminal pairings clear the pairing code so a previous device's
        // passkey can never linger on screen (stale-code lesson). When the
        // overlay is closed, clear spinner/code without mutating mode or
        // raising a stale toast upon reopen.
        if let Some(pairing) = &update.pairing {
            use speaker_bluetooth::PairingStatus;
            match pairing {
                PairingStatus::Connecting { name } => {
                    if screen_open {
                        w.set_bt_devices_mode(2);
                        w.set_bt_pair_device_name(name.to_string().into());
                        w.set_bt_pair_code(String::new().into());
                        w.set_bt_pair_spinning(true);
                    }
                }
                PairingStatus::AwaitingConfirmation { name, passkey } => {
                    if screen_open {
                        w.set_bt_devices_mode(2);
                        w.set_bt_pair_device_name(name.to_string().into());
                        w.set_bt_pair_code(super::pairing_code_text(*passkey).into());
                        w.set_bt_pair_spinning(true);
                    }
                }
                PairingStatus::Paired { .. } => {
                    if screen_open {
                        w.set_bt_devices_mode(0);
                    }
                    w.set_bt_pair_spinning(false);
                    w.set_bt_pair_code(String::new().into());
                }
                PairingStatus::Failed { message } => {
                    tracing::info!(message = %message, "Bluetooth pairing failed");
                    if screen_open {
                        w.set_bt_devices_mode(1);
                    }
                    w.set_bt_pair_spinning(false);
                    w.set_bt_pair_code(String::new().into());
                }
                PairingStatus::TimedOut | PairingStatus::Cancelled => {
                    if screen_open {
                        w.set_bt_devices_mode(1);
                    }
                    w.set_bt_pair_spinning(false);
                    w.set_bt_pair_code(String::new().into());
                }
            }
        }

        if let Some(text) = &update.toast
            && screen_open
        {
            // A refusal toast published WITHOUT a pairing session (agent
            // registration failure, no adapter, pairing-thread spawn failure)
            // must converge the optimistic pairing flip from on_bt_row_action;
            // otherwise the screen sits on the pairing view with a dead
            // spinner until manual back (review: error-path state desync).
            if update.pairing.is_none() && w.get_bt_devices_mode() == 2 {
                w.set_bt_devices_mode(1);
                w.set_bt_pair_spinning(false);
                w.set_bt_pair_code(String::new().into());
            }
            w.set_bt_toast_text(text.to_string().into());
            w.set_bt_toast_visible(true);
            w.set_bt_toast_seq(w.get_bt_toast_seq().wrapping_add(1));
        }
    }

    /// Applies a theme preset to the UI shell.
    pub fn apply_theme(&self, preset: ThemePreset) {
        preset.apply(&self.window);
        Self::refresh_marquee_strip(&self.window);
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

    /// Wires the Bluetooth devices screen callbacks to the `bt-monitor` worker
    /// device command queue (pairing, discovery, connect/disconnect/remove).
    pub fn wire_bluetooth_devices(&self, commands: &speaker_bluetooth::DeviceCommandQueue) {
        *self.device_commands.borrow_mut() = Some(commands.clone());
    }

    fn setup_callbacks(&self) {
        self.setup_navigation_callbacks();
        self.setup_bt_devices_callbacks();
        self.setup_bt_device_dialog_callbacks();
        self.setup_transport_callbacks();
        self.setup_overlay_backdrop_callback();
    }

    fn setup_overlay_backdrop_callback(&self) {
        let win_weak = self.window.as_weak();
        self.window.on_prepare_overlay_backdrop(move || {
            let Some(win) = win_weak.upgrade() else {
                return;
            };
            let top_inset = win.get_top_bar_height().round();
            let top_inset = if top_inset.is_finite() && top_inset >= 0.0 {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "top_inset is finite, >= 0.0, and bounded by the window height"
                )]
                {
                    top_inset as u32
                }
            } else {
                OVERLAY_TOP_INSET
            };
            let snapshot = match win.window().take_snapshot() {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        "Window snapshot unavailable for overlay backdrop; using scrim dimming"
                    );
                    return;
                }
            };
            match super::backdrop::blur_full_rgba_backdrop(
                snapshot.as_bytes(),
                snapshot.width(),
                snapshot.height(),
                top_inset,
                super::backdrop::BACKDROP_BLUR_RADIUS,
                super::backdrop::OUTSIDE_BLUR_RADIUS,
            ) {
                Ok(blurred) => {
                    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
                        blurred.width,
                        blurred.height,
                    );
                    buffer.make_mut_bytes().copy_from_slice(&blurred.pixels);
                    win.set_overlay_backdrop(slint::Image::from_rgba8_premultiplied(buffer));
                    win.set_overlay_backdrop_available(true);
                    tracing::debug!(
                        width = blurred.width,
                        height = blurred.height,
                        "Captured blurred overlay backdrop from window snapshot"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "Failed to blur window snapshot; falling back to scrim dimming"
                    );
                }
            }
        });
    }

    fn setup_settings_callbacks(&self) {
        let win = self.window.as_weak();
        let persist_tx = Rc::clone(&self.persist_tx);
        self.window.on_select_theme(move |idx| {
            if let Some(w) = win.upgrade() {
                let preset = ThemePreset::from_index(idx);
                preset.apply(&w);
                Self::persist_window_state(&w, &persist_tx);
            }
        });

        let win = self.window.as_weak();
        let models = self.settings_models.clone();
        let persist_tx = Rc::clone(&self.persist_tx);
        self.window.on_setting_step(move |id, dir| {
            let Some(setting_id) = SettingId::from_key(id.as_str()) else {
                tracing::warn!(setting_id = %id, "ignoring unknown setting_step id");
                return;
            };
            if let Some(w) = win.upgrade() {
                let mut config = Self::config_from_window(&w);
                if apply_step(&mut config, setting_id, dir) {
                    Self::apply_config_to_window(&w, &config);
                    Self::sync_settings_models_for(&w, &models);
                    Self::persist_window_state(&w, &persist_tx);
                }
            }
        });

        let win = self.window.as_weak();
        let models = self.settings_models.clone();
        let persist_tx = Rc::clone(&self.persist_tx);
        self.window.on_setting_toggle(move |id, right_selected| {
            let Some(setting_id) = SettingId::from_key(id.as_str()) else {
                tracing::warn!(setting_id = %id, "ignoring unknown setting_toggle id");
                return;
            };
            if let Some(w) = win.upgrade() {
                let mut config = Self::config_from_window(&w);
                if apply_toggle(&mut config, setting_id, right_selected) {
                    Self::apply_config_to_window(&w, &config);
                    Self::sync_settings_models_for(&w, &models);
                    Self::persist_window_state(&w, &persist_tx);
                }
            }
        });
    }

    /// Registers the Bluetooth Devices screen callbacks: device-queue
    /// command pushes plus optimistic dialog/mode navigation.
    fn setup_bt_devices_callbacks(&self) {
        // Bluetooth devices screen actions → bt-monitor worker device queue.
        let win = self.window.as_weak();
        let device_cmds = Rc::clone(&self.device_commands);
        self.window.on_bt_add_device_clicked(move || {
            // Enter the discovery view immediately (plan §5): discovered rows
            // render only in mode 1, so `+ Add Device` must flip the mode or
            // the scan results stay invisible.
            if let Some(w) = win.upgrade() {
                w.set_bt_devices_mode(1);
            }
            push_device_command(
                &device_cmds,
                speaker_bluetooth::DeviceCommand::StartDiscovery,
            );
        });

        let win = self.window.as_weak();
        let device_cmds = Rc::clone(&self.device_commands);
        self.window.on_bt_cancel_scan_clicked(move || {
            // Cancel returns to the paired list; the worker skips the stop
            // when this screen never started a scan.
            if let Some(w) = win.upgrade() {
                w.set_bt_devices_mode(0);
            }
            // Stop first, then refresh (FIFO) so the list settles on the
            // devices found during this scan.
            push_device_command(
                &device_cmds,
                speaker_bluetooth::DeviceCommand::StopDiscovery,
            );
            push_device_command(&device_cmds, speaker_bluetooth::DeviceCommand::Refresh);
        });

        let win = self.window.as_weak();
        let device_cmds = Rc::clone(&self.device_commands);
        self.window.on_bt_row_action(move |idx, from_discovery| {
            let Some(w) = win.upgrade() else {
                return;
            };
            let rows = if from_discovery {
                w.get_bt_discovered_rows()
            } else {
                w.get_bt_device_rows()
            };
            let Some(row) = usize::try_from(idx).ok().and_then(|i| rows.row_data(i)) else {
                tracing::debug!(idx, from_discovery, "Bluetooth row action index out of range");
                return;
            };
            if w.get_bt_pending_address().as_str() == row.address.as_str() {
                return;
            }
            let addr = match speaker_bluetooth::BtAddress::parse_str(row.address.as_str()) {
                Ok(addr) => addr,
                Err(e) => {
                    tracing::debug!(address = %row.address, error = %e, "Bluetooth row address unparseable");
                    return;
                }
            };
            let command = if from_discovery && !row.paired {
                // Optimistic pairing view; the worker's Connecting status
                // (or a refusal toast) converges it within a tick.
                w.set_bt_devices_mode(2);
                w.set_bt_pair_device_name(row.name.clone());
                w.set_bt_pair_code(String::new().into());
                w.set_bt_pair_spinning(true);
                speaker_bluetooth::DeviceCommand::Pair(addr)
            } else if row.connected {
                w.set_bt_pending_address(row.address.clone());
                speaker_bluetooth::DeviceCommand::Disconnect(addr)
            } else {
                // Includes already-paired devices seen in discovery: connect
                // without re-pairing (phone-style discovery list).
                w.set_bt_pending_address(row.address.clone());
                speaker_bluetooth::DeviceCommand::Connect(addr)
            };
            push_device_command(&device_cmds, command);
        });

        let win = self.window.as_weak();
        let device_cmds = Rc::clone(&self.device_commands);
        self.window.on_bt_pair_cancel_clicked(move || {
            push_device_command(
                &device_cmds,
                speaker_bluetooth::DeviceCommand::CancelPairing,
            );
            if let Some(w) = win.upgrade() {
                // Optimistic return to the Add Device list; a terminal status
                // already in flight converges the mode on the next publish.
                w.set_bt_devices_mode(1);
            }
        });
    }

    /// Registers the Bluetooth delete-confirmation dialog callbacks:
    /// index→address capture at trigger time (plan A2) plus confirm/cancel.
    fn setup_bt_device_dialog_callbacks(&self) {
        let win = self.window.as_weak();
        self.window.on_bt_row_delete(move |idx| {
            let Some(w) = win.upgrade() else {
                return;
            };
            let rows = w.get_bt_device_rows();
            let Some(row) = usize::try_from(idx).ok().and_then(|i| rows.row_data(i)) else {
                tracing::debug!(idx, "Bluetooth delete row index out of range");
                return;
            };
            // Resolve index → address at trigger time (plan A2): later
            // publishes may reorder rows, but the dialog targets the device
            // captured here.
            w.set_bt_delete_address(row.address.clone());
            w.set_bt_delete_device_name(row.name.clone());
            w.set_bt_delete_open(true);
        });

        let win = self.window.as_weak();
        let device_cmds = Rc::clone(&self.device_commands);
        self.window.on_bt_delete_confirm_clicked(move || {
            let Some(w) = win.upgrade() else {
                return;
            };
            w.set_bt_delete_open(false);
            let raw = w.get_bt_delete_address();
            match speaker_bluetooth::BtAddress::parse_str(raw.as_str()) {
                Ok(addr) => push_device_command(
                    &device_cmds,
                    speaker_bluetooth::DeviceCommand::Remove(addr),
                ),
                Err(e) => {
                    tracing::warn!(address = %raw, error = %e, "Bluetooth delete confirm with invalid address");
                }
            }
        });

        let win = self.window.as_weak();
        self.window.on_bt_delete_cancel_clicked(move || {
            if let Some(w) = win.upgrade() {
                w.set_bt_delete_open(false);
                tracing::debug!("Bluetooth delete dialog cancelled");
            }
        });
    }

    fn setup_navigation_callbacks(&self) {
        self.setup_settings_callbacks();

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

        let win = self.window.as_weak();
        let persist_tx = Rc::clone(&self.persist_tx);
        self.window.on_font_scale_changed(move |scale| {
            if let Some(w) = win.upgrade() {
                let clamped = if scale.is_finite() {
                    scale.clamp(Self::MIN_FONT_SCALE, Self::MAX_FONT_SCALE)
                } else {
                    Self::DEFAULT_FONT_SCALE
                };
                w.global::<TouchScale>().set_font_scale(clamped);
                Self::persist_window_state(&w, &persist_tx);
                tracing::debug!(font_scale = clamped, "Touch font scale updated");
            }
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
