//! Declarative catalog of Screen Settings metadata and Basic/Advanced tiering.
//!
//! [`SETTINGS_CATALOG`] is the single source of truth for:
//! - Which settings belong to [`SettingTier::Basic`] vs [`SettingTier::Advanced`].
//! - Human-readable labels, control kinds ([`SettingControl::Stepper`] or
//!   [`SettingControl::TwoOption`]), and domain bounds/steps sourced directly
//!   from [`crate::config`].
//! - Pure [`CoreConfig`] mutation and row-state projection helpers shared by the
//!   Slint UI shell and integration tests.

use crate::config::{
    CODEC_HUD_TIMEOUT_STEP_MS, CodecHudTimeoutMs, CoreConfig, MAX_CODEC_HUD_TIMEOUT_MS,
    MAX_VISUALIZER_AUTO_START_SECS, MAX_VOLUME_HUD_FADE_MS, MAX_VOLUME_HUD_TIMEOUT_MS,
    MAX_VOLUME_STEP_PERCENT, MIN_CODEC_HUD_TIMEOUT_MS, MIN_VISUALIZER_AUTO_START_SECS,
    MIN_VOLUME_HUD_FADE_MS, MIN_VOLUME_HUD_TIMEOUT_MS, MIN_VOLUME_STEP_PERCENT, MarqueeFps,
    TitleDisplayMode, VISUALIZER_AUTO_START_STEP_SECS, VOLUME_HUD_FADE_STEP_MS,
    VisualizerAutoStartSecs, VolumeHudFadeMs, VolumeHudTimeoutMs, VolumeStepPercent,
};

/// Visibility tier for a setting on the Screen Settings UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingTier {
    /// Shown directly on the Screen Settings screen.
    Basic,
    /// Shown in the dedicated Visualizer Settings sub-screen.
    Visualizer,
    /// Hidden by default; revealed only after entering the Advanced sub-screen.
    Advanced,
}

impl SettingTier {
    /// Returns `true` when this tier is [`SettingTier::Advanced`].
    #[must_use]
    pub const fn is_advanced(self) -> bool {
        matches!(self, Self::Advanced)
    }
}

/// Strongly typed identifier for every catalog-driven setting row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingId {
    /// Track title display mode (`Marquee` vs `Split`).
    TitleDisplayMode,
    /// Marquee scroll rate (`30` vs `60` fps).
    MarqueeFps,
    /// Rotary encoder volume step per detent (`1..=25` %).
    VolumeStepPercent,
    /// Volume HUD auto-dismiss timeout (`250..=30000` ms).
    VolumeHudTimeoutMs,
    /// Volume HUD fade-out duration (`0..=2000` ms).
    VolumeHudFadeMs,
    /// Codec HUD auto-dismiss timeout (`250..=300000` ms).
    CodecHudTimeoutMs,
    /// Automatic ambient-light brightness control (`Manual` vs `Auto`).
    AutoBrightness,
    /// Retro segmented LED slit mask over the 24-band spectrum (`Smooth` vs `LED`).
    SpectrumLedMask,
    /// Automatically open the visualizer after playback starts (`Off` vs `On`).
    VisualizerAutoStart,
    /// Delay in seconds before auto-starting the visualizer (`1..=60` s).
    VisualizerAutoStartSecs,
    /// Default visualization mode (`Radial` vs `Spectrum`).
    DefaultVisualizer,
    /// Force 60 FPS on music visualizations (`30` = ~33 Hz vs `60` = 60 Hz).
    VisualizerFps,
    /// Dynamic Material 3 cover-art theming mode (`Off`, `Harmonize`, `Subtle Tint`, `Auto Cover`).
    CoverThemeMode,
}

impl SettingId {
    /// All variants of [`SettingId`], used to prove catalog completeness at compile time.
    pub const ALL: [Self; 13] = [
        Self::CoverThemeMode,
        Self::TitleDisplayMode,
        Self::MarqueeFps,
        Self::VolumeStepPercent,
        Self::VolumeHudTimeoutMs,
        Self::VolumeHudFadeMs,
        Self::CodecHudTimeoutMs,
        Self::AutoBrightness,
        Self::SpectrumLedMask,
        Self::VisualizerAutoStart,
        Self::VisualizerAutoStartSecs,
        Self::DefaultVisualizer,
        Self::VisualizerFps,
    ];

    /// Stable slug key used to identify this setting across the Rust/Slint boundary.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::CoverThemeMode => "cover_theme_mode",
            Self::TitleDisplayMode => "title_display_mode",
            Self::MarqueeFps => "marquee_fps",
            Self::VolumeStepPercent => "volume_step_percent",
            Self::VolumeHudTimeoutMs => "volume_hud_timeout_ms",
            Self::VolumeHudFadeMs => "volume_hud_fade_ms",
            Self::CodecHudTimeoutMs => "codec_hud_timeout_ms",
            Self::AutoBrightness => "auto_brightness",
            Self::SpectrumLedMask => "spectrum_led_mask",
            Self::VisualizerAutoStart => "visualizer_auto_start",
            Self::VisualizerAutoStartSecs => "visualizer_auto_start_secs",
            Self::DefaultVisualizer => "default_visualizer",
            Self::VisualizerFps => "visualizer_fps",
        }
    }

    /// Parses a setting slug key back into [`SettingId`], returning `None` if unknown.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        SETTINGS_CATALOG
            .iter()
            .find(|desc| desc.key() == key)
            .map(|desc| desc.id)
    }
}

/// UI control shape and domain constraints for a catalog-driven setting row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingControl {
    /// Numeric `−` / `+` stepper bounded by `min..=max` with `step` increments.
    Stepper {
        /// Inclusive minimum value.
        min: i32,
        /// Inclusive maximum value.
        max: i32,
        /// Step increment per tap.
        step: i32,
        /// Display unit suffix (e.g. `"%"` or `"ms"`).
        unit: &'static str,
    },
    /// Two-option segmented toggle (`left_label` when `false`, `right_label` when `true`).
    TwoOption {
        /// Label for the left (`false`) option.
        left_label: &'static str,
        /// Label for the right (`true`) option.
        right_label: &'static str,
    },
    /// Cyclic `<` / `>` selector over named options (scalable to 2+ choices).
    Selector,
}

/// Metadata descriptor for a single setting row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingDescriptor {
    /// Strongly typed setting identifier.
    pub id: SettingId,
    /// User-facing row label.
    pub label: &'static str,
    /// Whether this setting is Basic or Advanced.
    pub tier: SettingTier,
    /// Control kind and domain bounds.
    pub control: SettingControl,
}

impl SettingDescriptor {
    /// Stable slug key for this setting.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.id.key()
    }

    /// Returns `true` when this setting belongs to [`SettingTier::Advanced`].
    #[must_use]
    pub const fn is_advanced(&self) -> bool {
        self.tier.is_advanced()
    }
}

/// Ordered catalog of all list-row settings rendered in Screen Settings and Advanced Settings.
pub const SETTINGS_CATALOG: &[SettingDescriptor] = &[
    SettingDescriptor {
        id: SettingId::CoverThemeMode,
        label: "Cover Art Theming",
        tier: SettingTier::Basic,
        control: SettingControl::Selector,
    },
    SettingDescriptor {
        id: SettingId::TitleDisplayMode,
        label: "Track Title",
        tier: SettingTier::Basic,
        control: SettingControl::TwoOption {
            left_label: "Marquee",
            right_label: "Split",
        },
    },
    SettingDescriptor {
        id: SettingId::MarqueeFps,
        label: "Marquee FPS",
        tier: SettingTier::Advanced,
        control: SettingControl::TwoOption {
            left_label: "30",
            right_label: "60",
        },
    },
    SettingDescriptor {
        id: SettingId::VolumeStepPercent,
        label: "Encoder Volume Step",
        tier: SettingTier::Advanced,
        control: SettingControl::Stepper {
            min: MIN_VOLUME_STEP_PERCENT as i32,
            max: MAX_VOLUME_STEP_PERCENT as i32,
            step: 1,
            unit: "%",
        },
    },
    SettingDescriptor {
        id: SettingId::VolumeHudTimeoutMs,
        label: "Volume HUD Timeout",
        tier: SettingTier::Advanced,
        control: SettingControl::Stepper {
            min: MIN_VOLUME_HUD_TIMEOUT_MS.cast_signed(),
            max: MAX_VOLUME_HUD_TIMEOUT_MS.cast_signed(),
            step: MIN_VOLUME_HUD_TIMEOUT_MS.cast_signed(),
            unit: "ms",
        },
    },
    SettingDescriptor {
        id: SettingId::VolumeHudFadeMs,
        label: "Volume HUD Fade",
        tier: SettingTier::Advanced,
        control: SettingControl::Stepper {
            min: MIN_VOLUME_HUD_FADE_MS.cast_signed(),
            max: MAX_VOLUME_HUD_FADE_MS.cast_signed(),
            step: VOLUME_HUD_FADE_STEP_MS.cast_signed(),
            unit: "ms",
        },
    },
    SettingDescriptor {
        id: SettingId::CodecHudTimeoutMs,
        label: "Codec HUD Timeout",
        tier: SettingTier::Advanced,
        control: SettingControl::Stepper {
            min: MIN_CODEC_HUD_TIMEOUT_MS.cast_signed(),
            max: MAX_CODEC_HUD_TIMEOUT_MS.cast_signed(),
            step: CODEC_HUD_TIMEOUT_STEP_MS.cast_signed(),
            unit: "ms",
        },
    },
    SettingDescriptor {
        id: SettingId::AutoBrightness,
        label: "Auto Brightness",
        tier: SettingTier::Basic,
        control: SettingControl::TwoOption {
            left_label: "Manual",
            right_label: "Auto",
        },
    },
    SettingDescriptor {
        id: SettingId::VisualizerAutoStart,
        label: "Auto-Start Visualizer",
        tier: SettingTier::Visualizer,
        control: SettingControl::TwoOption {
            left_label: "Off",
            right_label: "On",
        },
    },
    SettingDescriptor {
        id: SettingId::VisualizerAutoStartSecs,
        label: "Auto-Start Delay",
        tier: SettingTier::Visualizer,
        control: SettingControl::Stepper {
            min: MIN_VISUALIZER_AUTO_START_SECS.cast_signed(),
            max: MAX_VISUALIZER_AUTO_START_SECS.cast_signed(),
            step: VISUALIZER_AUTO_START_STEP_SECS.cast_signed(),
            unit: "s",
        },
    },
    SettingDescriptor {
        id: SettingId::DefaultVisualizer,
        label: "Default Visualizer",
        tier: SettingTier::Visualizer,
        control: SettingControl::Selector,
    },
    SettingDescriptor {
        id: SettingId::SpectrumLedMask,
        label: "24-Band LED Mask",
        tier: SettingTier::Visualizer,
        control: SettingControl::TwoOption {
            left_label: "Off",
            right_label: "On",
        },
    },
    SettingDescriptor {
        id: SettingId::VisualizerFps,
        label: "Visualizer FPS",
        tier: SettingTier::Advanced,
        control: SettingControl::TwoOption {
            left_label: "30",
            right_label: "60",
        },
    },
];

const _: () = {
    assert!(SETTINGS_CATALOG.len() == SettingId::ALL.len());

    let mut i = 0;
    while i < SETTINGS_CATALOG.len() {
        let desc = &SETTINGS_CATALOG[i];
        if let SettingControl::Stepper { min, max, step, .. } = desc.control {
            assert!(min <= max, "stepper min must not exceed max");
            assert!(step > 0, "stepper step must be positive");
        }
        i += 1;
    }

    // Prove every SettingId variant appears exactly once in SETTINGS_CATALOG.
    let mut v = 0;
    while v < SettingId::ALL.len() {
        let target = SettingId::ALL[v];
        let mut count = 0usize;
        let mut j = 0;
        while j < SETTINGS_CATALOG.len() {
            if SETTINGS_CATALOG[j].id as u8 == target as u8 {
                count += 1;
            }
            j += 1;
        }
        assert!(
            count == 1,
            "every SettingId must appear once in SETTINGS_CATALOG"
        );
        v += 1;
    }
};

/// Looks up a setting descriptor by [`SettingId`].
///
/// # Panics
///
/// Panics if `id` is missing from [`SETTINGS_CATALOG`] (impossible when all
/// variants of [`SettingId`] are registered).
#[must_use]
pub fn find_setting(id: SettingId) -> &'static SettingDescriptor {
    SETTINGS_CATALOG
        .iter()
        .find(|d| d.id == id)
        .expect("every SettingId variant has an entry in SETTINGS_CATALOG")
}

/// Looks up a setting descriptor by its slug key.
#[must_use]
pub fn find_setting_by_key(key: &str) -> Option<&'static SettingDescriptor> {
    SettingId::from_key(key).map(find_setting)
}

/// UI-ready state snapshot for a single setting row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRowState {
    /// Formatted current value for stepper rows (e.g. `"2 %"`).
    pub value_text: String,
    /// Whether the stepper decrease button should be enabled.
    pub can_decrease: bool,
    /// Whether the stepper increase button should be enabled.
    pub can_increase: bool,
    /// Whether the right option is selected for two-option rows.
    pub right_selected: bool,
}

/// Projects the current [`CoreConfig`] value for `desc` into a [`SettingRowState`].
#[must_use]
pub fn row_state(config: &CoreConfig, desc: &SettingDescriptor) -> SettingRowState {
    match desc.control {
        SettingControl::Stepper { min, max, unit, .. } => {
            let raw = stepper_value(config, desc.id);
            SettingRowState {
                value_text: format!("{raw} {unit}"),
                can_decrease: raw > min,
                can_increase: raw < max,
                right_selected: false,
            }
        }
        SettingControl::TwoOption { .. } => SettingRowState {
            value_text: String::new(),
            can_decrease: false,
            can_increase: false,
            right_selected: two_option_is_right(config, desc.id),
        },
        SettingControl::Selector => SettingRowState {
            value_text: selector_value_text(config, desc.id),
            can_decrease: true,
            can_increase: true,
            right_selected: false,
        },
    }
}

/// Applies a relative step (`direction` = `-1` or `+1`, clamped via `signum()`)
/// to a stepper or cyclic selector setting in `config`.
///
/// Returns `true` if `id` is a stepper/selector setting and `config` was updated.
pub fn apply_step(config: &mut CoreConfig, id: SettingId, direction: i32) -> bool {
    let dir = direction.signum();
    if dir == 0 {
        return false;
    }
    let desc = find_setting(id);
    match desc.control {
        SettingControl::Stepper { min, max, step, .. } => {
            let current = stepper_value(config, id);
            let next = current
                .saturating_add(dir.saturating_mul(step))
                .clamp(min, max);
            set_stepper_value(config, id, next)
        }
        SettingControl::Selector => apply_selector_step(config, id, dir),
        SettingControl::TwoOption { .. } => false,
    }
}

/// Applies a two-option selection (`right_selected`) to `config`.
///
/// Returns `true` if `id` is a two-option setting and `config` was updated.
pub fn apply_toggle(config: &mut CoreConfig, id: SettingId, right_selected: bool) -> bool {
    match id {
        SettingId::TitleDisplayMode => {
            config.title_display_mode = TitleDisplayMode::from_split(right_selected);
            true
        }
        SettingId::MarqueeFps => {
            config.marquee_fps = if right_selected {
                MarqueeFps::FPS_60
            } else {
                MarqueeFps::FPS_30
            };
            true
        }
        SettingId::AutoBrightness => {
            config.auto_brightness_enabled = right_selected;
            true
        }
        SettingId::SpectrumLedMask => {
            config.spectrum_led_mask = right_selected;
            true
        }
        SettingId::VisualizerAutoStart => {
            config.visualizer_auto_start = right_selected;
            true
        }
        SettingId::VisualizerFps => {
            config.visualizer_force_60fps = right_selected;
            true
        }
        SettingId::VolumeStepPercent
        | SettingId::VolumeHudTimeoutMs
        | SettingId::VolumeHudFadeMs
        | SettingId::CodecHudTimeoutMs
        | SettingId::VisualizerAutoStartSecs
        | SettingId::DefaultVisualizer
        | SettingId::CoverThemeMode => false,
    }
}

fn selector_value_text(config: &CoreConfig, id: SettingId) -> String {
    match id {
        SettingId::DefaultVisualizer => config.default_visualizer.display_name().to_string(),
        SettingId::CoverThemeMode => config.cover_theme_mode.display_name().to_string(),
        _ => String::new(),
    }
}

fn apply_selector_step(config: &mut CoreConfig, id: SettingId, dir: i32) -> bool {
    match id {
        SettingId::DefaultVisualizer => {
            config.default_visualizer = if dir < 0 {
                config.default_visualizer.prev()
            } else {
                config.default_visualizer.next()
            };
            true
        }
        SettingId::CoverThemeMode => {
            config.cover_theme_mode = config.cover_theme_mode.step(dir);
            true
        }
        _ => false,
    }
}

fn stepper_value(config: &CoreConfig, id: SettingId) -> i32 {
    match id {
        SettingId::VolumeStepPercent => i32::from(config.volume_step_percent.value()),
        SettingId::VolumeHudTimeoutMs => i32::try_from(config.volume_hud_timeout_ms.value())
            .expect("validated volume_hud_timeout_ms fits in i32"),
        SettingId::VolumeHudFadeMs => i32::try_from(config.volume_hud_fade_ms.value())
            .expect("validated volume_hud_fade_ms fits in i32"),
        SettingId::CodecHudTimeoutMs => i32::try_from(config.codec_hud_timeout_ms.value())
            .expect("validated codec_hud_timeout_ms fits in i32"),
        SettingId::VisualizerAutoStartSecs => {
            i32::try_from(config.visualizer_auto_start_secs.value())
                .expect("validated visualizer_auto_start_secs fits in i32")
        }
        SettingId::TitleDisplayMode
        | SettingId::MarqueeFps
        | SettingId::AutoBrightness
        | SettingId::SpectrumLedMask
        | SettingId::VisualizerAutoStart
        | SettingId::DefaultVisualizer
        | SettingId::VisualizerFps
        | SettingId::CoverThemeMode => 0,
    }
}

fn set_stepper_value(config: &mut CoreConfig, id: SettingId, value: i32) -> bool {
    match id {
        SettingId::VolumeStepPercent => {
            let clamped = u8::try_from(value.clamp(
                i32::from(MIN_VOLUME_STEP_PERCENT),
                i32::from(MAX_VOLUME_STEP_PERCENT),
            ))
            .expect("clamped volume_step_percent fits in u8");
            config.volume_step_percent = VolumeStepPercent::new(clamped);
            true
        }
        SettingId::VolumeHudTimeoutMs => {
            let clamped = value
                .max(0)
                .cast_unsigned()
                .clamp(MIN_VOLUME_HUD_TIMEOUT_MS, MAX_VOLUME_HUD_TIMEOUT_MS);
            config.volume_hud_timeout_ms = VolumeHudTimeoutMs::new(clamped);
            true
        }
        SettingId::VolumeHudFadeMs => {
            let clamped = value
                .max(0)
                .cast_unsigned()
                .clamp(MIN_VOLUME_HUD_FADE_MS, MAX_VOLUME_HUD_FADE_MS);
            config.volume_hud_fade_ms = VolumeHudFadeMs::new(clamped);
            true
        }
        SettingId::CodecHudTimeoutMs => {
            let clamped = value
                .max(0)
                .cast_unsigned()
                .clamp(MIN_CODEC_HUD_TIMEOUT_MS, MAX_CODEC_HUD_TIMEOUT_MS);
            config.codec_hud_timeout_ms = CodecHudTimeoutMs::new(clamped);
            true
        }
        SettingId::VisualizerAutoStartSecs => {
            let clamped = value.max(0).cast_unsigned().clamp(
                MIN_VISUALIZER_AUTO_START_SECS,
                MAX_VISUALIZER_AUTO_START_SECS,
            );
            config.visualizer_auto_start_secs = VisualizerAutoStartSecs::new(clamped);
            true
        }
        SettingId::TitleDisplayMode
        | SettingId::MarqueeFps
        | SettingId::AutoBrightness
        | SettingId::SpectrumLedMask
        | SettingId::VisualizerAutoStart
        | SettingId::DefaultVisualizer
        | SettingId::VisualizerFps
        | SettingId::CoverThemeMode => false,
    }
}

fn two_option_is_right(config: &CoreConfig, id: SettingId) -> bool {
    match id {
        SettingId::TitleDisplayMode => config.title_display_mode.is_split(),
        SettingId::MarqueeFps => config.marquee_fps == MarqueeFps::FPS_60,
        SettingId::AutoBrightness => config.auto_brightness_enabled,
        SettingId::SpectrumLedMask => config.spectrum_led_mask,
        SettingId::VisualizerAutoStart => config.visualizer_auto_start,
        SettingId::VisualizerFps => config.visualizer_force_60fps,
        SettingId::VolumeStepPercent
        | SettingId::VolumeHudTimeoutMs
        | SettingId::VolumeHudFadeMs
        | SettingId::CodecHudTimeoutMs
        | SettingId::VisualizerAutoStartSecs
        | SettingId::DefaultVisualizer
        | SettingId::CoverThemeMode => false,
    }
}
