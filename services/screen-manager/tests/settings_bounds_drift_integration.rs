#![cfg(feature = "ui")]
//! Drift guard between the Slint stepper bounds in `ui/settings_view.slint`
//! and the Rust config domains in `crate::config`.
//!
//! The Advanced settings rows hardcode their min/max/step values directly in
//! `.slint` markup (`root.volume_step_percent > 1`, `- 250`, ...). Those
//! literals mirror the `MIN_*`/`MAX_*` constants that the Rust callbacks clamp
//! against; if one side moves without the other, the UI either enables a
//! stepper that the Rust side clamps (dead taps at the bound) or disables one
//! the Rust side would accept (settings that cannot be reached).
//!
//! This test pins both sides together: it extracts every numeric literal from
//! the `SettingsStepperRow` invocations and asserts each equals the
//! corresponding Rust constant.

use screen_manager::config::{
    MAX_CODEC_HUD_TIMEOUT_MS, MAX_VOLUME_HUD_FADE_MS, MAX_VOLUME_HUD_TIMEOUT_MS,
    MAX_VOLUME_STEP_PERCENT, MIN_CODEC_HUD_TIMEOUT_MS, MIN_VOLUME_HUD_TIMEOUT_MS,
    MIN_VOLUME_STEP_PERCENT,
};

/// The Slint source of the Settings view, embedded at compile time.
const SETTINGS_VIEW_SLINT: &str = include_str!("../ui/settings_view.slint");

/// Asserts `needle` appears exactly once in the settings view source.
fn assert_bound_literal(needle: &str) {
    let count = SETTINGS_VIEW_SLINT.matches(needle).count();
    assert_eq!(
        count, 1,
        "expected exactly one occurrence of `{needle}` in settings_view.slint, found {count}"
    );
}

#[test]
fn stepper_bounds_match_rust_domains() {
    // Encoder Volume Step: can_decrease/can_increase bounds + ±1 step.
    assert_bound_literal(&format!(
        "root.volume_step_percent > {MIN_VOLUME_STEP_PERCENT}"
    ));
    assert_bound_literal(&format!(
        "root.volume_step_percent < {MAX_VOLUME_STEP_PERCENT}"
    ));
    assert_bound_literal("root.volume_step_percent - 1");
    assert_bound_literal("root.volume_step_percent + 1");

    // Volume HUD Timeout: 250..=30000, step ±250.
    assert_bound_literal(&format!(
        "root.volume_hud_timeout_ms > {MIN_VOLUME_HUD_TIMEOUT_MS}"
    ));
    assert_bound_literal(&format!(
        "root.volume_hud_timeout_ms < {MAX_VOLUME_HUD_TIMEOUT_MS}"
    ));
    assert_bound_literal(&format!(
        "root.volume_hud_timeout_ms - {MIN_VOLUME_HUD_TIMEOUT_MS}"
    ));
    assert_bound_literal(&format!(
        "root.volume_hud_timeout_ms + {MIN_VOLUME_HUD_TIMEOUT_MS}"
    ));

    // Volume HUD Fade: 0..=2000, step ±100.
    assert_bound_literal("root.volume_hud_fade_ms > 0");
    assert_bound_literal(&format!(
        "root.volume_hud_fade_ms < {MAX_VOLUME_HUD_FADE_MS}"
    ));
    assert_bound_literal("root.volume_hud_fade_ms - 100");
    assert_bound_literal("root.volume_hud_fade_ms + 100");

    // Codec HUD Timeout: 250..=300000, step ±1000.
    assert_bound_literal(&format!(
        "root.codec_hud_timeout_ms > {MIN_CODEC_HUD_TIMEOUT_MS}"
    ));
    assert_bound_literal(&format!(
        "root.codec_hud_timeout_ms < {MAX_CODEC_HUD_TIMEOUT_MS}"
    ));
    assert_bound_literal("root.codec_hud_timeout_ms - 1000");
    assert_bound_literal("root.codec_hud_timeout_ms + 1000");
}
