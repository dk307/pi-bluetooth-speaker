#![cfg(feature = "ui")]
//! Catalog consistency and drift guard between `crate::settings_catalog` and
//! `crate::config`, plus verification that `ui/settings_view.slint` contains
//! no hardcoded per-setting bounds.

use screen_manager::config::{
    CODEC_HUD_TIMEOUT_STEP_MS, CoreConfig, MAX_CODEC_HUD_TIMEOUT_MS, MAX_VOLUME_HUD_FADE_MS,
    MAX_VOLUME_HUD_TIMEOUT_MS, MAX_VOLUME_STEP_PERCENT, MIN_CODEC_HUD_TIMEOUT_MS,
    MIN_VOLUME_HUD_FADE_MS, MIN_VOLUME_HUD_TIMEOUT_MS, MIN_VOLUME_STEP_PERCENT,
    VOLUME_HUD_FADE_STEP_MS,
};
use screen_manager::settings_catalog::{
    SETTINGS_CATALOG, SettingControl, SettingId, SettingTier, apply_step, apply_toggle,
    find_setting, find_setting_by_key, row_state,
};

const SETTINGS_VIEW_SLINT: &str = include_str!("../ui/settings_view.slint");

#[test]
fn catalog_bounds_match_rust_config_domains() {
    let checks: &[(SettingId, i32, i32, i32)] = &[
        (
            SettingId::VolumeStepPercent,
            i32::from(MIN_VOLUME_STEP_PERCENT),
            i32::from(MAX_VOLUME_STEP_PERCENT),
            1,
        ),
        (
            SettingId::VolumeHudTimeoutMs,
            MIN_VOLUME_HUD_TIMEOUT_MS.cast_signed(),
            MAX_VOLUME_HUD_TIMEOUT_MS.cast_signed(),
            MIN_VOLUME_HUD_TIMEOUT_MS.cast_signed(),
        ),
        (
            SettingId::VolumeHudFadeMs,
            MIN_VOLUME_HUD_FADE_MS.cast_signed(),
            MAX_VOLUME_HUD_FADE_MS.cast_signed(),
            VOLUME_HUD_FADE_STEP_MS.cast_signed(),
        ),
        (
            SettingId::CodecHudTimeoutMs,
            MIN_CODEC_HUD_TIMEOUT_MS.cast_signed(),
            MAX_CODEC_HUD_TIMEOUT_MS.cast_signed(),
            CODEC_HUD_TIMEOUT_STEP_MS.cast_signed(),
        ),
    ];

    for &(id, expected_min, expected_max, expected_step) in checks {
        let desc = find_setting(id);
        match desc.control {
            SettingControl::Stepper { min, max, step, .. } => {
                assert_eq!(min, expected_min, "min mismatch for {id:?}");
                assert_eq!(max, expected_max, "max mismatch for {id:?}");
                assert_eq!(step, expected_step, "step mismatch for {id:?}");
            }
            SettingControl::TwoOption { .. } => {
                panic!("expected Stepper control for {id:?}");
            }
        }
    }
}

#[test]
fn catalog_keys_round_trip_and_tiers_are_populated() {
    assert!(
        SETTINGS_CATALOG
            .iter()
            .any(|d| d.tier == SettingTier::Basic)
    );
    assert!(
        SETTINGS_CATALOG
            .iter()
            .any(|d| d.tier == SettingTier::Advanced)
    );
    for desc in SETTINGS_CATALOG {
        assert_eq!(SettingId::from_key(desc.key()), Some(desc.id));
        assert_eq!(find_setting_by_key(desc.key()), Some(desc));
    }
    assert_eq!(SettingId::from_key("unknown_setting"), None);
    assert_eq!(find_setting_by_key("unknown_setting"), None);
}

#[test]
fn catalog_step_and_toggle_helpers_clamp_and_format() {
    let mut cfg = CoreConfig::default();

    assert!(apply_step(&mut cfg, SettingId::VolumeStepPercent, 1));
    assert_eq!(cfg.volume_step_percent.value(), 3);
    assert!(apply_step(&mut cfg, SettingId::VolumeStepPercent, -1));
    assert_eq!(cfg.volume_step_percent.value(), 2);
    // Direction 0 is a no-op.
    assert!(!apply_step(&mut cfg, SettingId::VolumeStepPercent, 0));
    // Step on a TwoOption setting returns false.
    assert!(!apply_step(&mut cfg, SettingId::TitleDisplayMode, 1));

    // Toggle on a Stepper setting returns false.
    assert!(!apply_toggle(&mut cfg, SettingId::VolumeStepPercent, true));
    assert!(apply_toggle(&mut cfg, SettingId::TitleDisplayMode, true));
    assert!(cfg.title_display_mode.is_split());
    assert!(apply_toggle(&mut cfg, SettingId::MarqueeFps, false));
    assert_eq!(cfg.marquee_fps.value(), 30);

    let state = row_state(&cfg, find_setting(SettingId::VolumeStepPercent));
    assert_eq!(state.value_text, "2 %");
    assert!(state.can_decrease);
    assert!(state.can_increase);

    // Exercise upper-bound and lower-bound clamping for every Stepper setting.
    for desc in SETTINGS_CATALOG {
        let SettingControl::Stepper { min, max, step, .. } = desc.control else {
            continue;
        };
        let steps_to_max = ((max - min) / step) + 5;
        let mut c = CoreConfig::default();
        for _ in 0..steps_to_max {
            assert!(apply_step(&mut c, desc.id, 1));
        }
        let at_max = row_state(&c, desc);
        assert!(!at_max.can_increase, "{:?} should clamp at max", desc.id);
        assert!(
            at_max.can_decrease,
            "{:?} should allow decrease at max",
            desc.id
        );

        for _ in 0..steps_to_max {
            assert!(apply_step(&mut c, desc.id, -1));
        }
        let at_min = row_state(&c, desc);
        assert!(!at_min.can_decrease, "{:?} should clamp at min", desc.id);
        assert!(
            at_min.can_increase,
            "{:?} should allow increase at min",
            desc.id
        );
    }
}

#[test]
fn settings_view_slint_does_not_hardcode_setting_rows() {
    for forbidden in [
        "Encoder Volume Step",
        "Volume HUD Timeout",
        "Volume HUD Fade",
        "Codec HUD Timeout",
        "Marquee FPS",
    ] {
        assert!(
            !SETTINGS_VIEW_SLINT.contains(forbidden),
            "settings_view.slint must render `{forbidden}` generically from SettingRowItem"
        );
    }
}
