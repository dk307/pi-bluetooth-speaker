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
//! This test pins both sides together: every bound/step literal the
//! `SettingsStepperRow` invocations rely on is listed as a needle built from
//! the corresponding Rust constant, and each needle must match exactly once
//! with a non-digit character after it — so widening a Slint bound (e.g.
//! `> 1` to `> 15`) breaks the guard instead of silently passing as a
//! substring.

use screen_manager::config::{
    CODEC_HUD_TIMEOUT_STEP_MS, MAX_CODEC_HUD_TIMEOUT_MS, MAX_VOLUME_HUD_FADE_MS,
    MAX_VOLUME_HUD_TIMEOUT_MS, MAX_VOLUME_STEP_PERCENT, MIN_CODEC_HUD_TIMEOUT_MS,
    MIN_VOLUME_HUD_FADE_MS, MIN_VOLUME_HUD_TIMEOUT_MS, MIN_VOLUME_STEP_PERCENT,
    VOLUME_HUD_FADE_STEP_MS,
};

/// The Slint source of the Settings view, embedded at compile time.
const SETTINGS_VIEW_SLINT: &str = include_str!("../ui/settings_view.slint");

/// Asserts `needle` appears exactly once in the settings view source and is
/// not the prefix of a longer number (e.g. `> 1` must not match `> 15`).
fn assert_bound_literal(needle: &str) {
    let mut matches = SETTINGS_VIEW_SLINT.match_indices(needle);
    let Some((_, _)) = matches.next() else {
        panic!("missing `{needle}` in settings_view.slint");
    };
    assert!(
        matches.next().is_none(),
        "duplicate `{needle}` in settings_view.slint"
    );
    let end = SETTINGS_VIEW_SLINT.find(needle).unwrap_or_default() + needle.len();
    let next = SETTINGS_VIEW_SLINT[end..].chars().next();
    assert!(
        !next.is_some_and(|c| c.is_ascii_digit()),
        "`{needle}` in settings_view.slint is the prefix of a longer number \
         (next char {next:?}); the Slint bound widened without the Rust domain"
    );
}

#[test]
fn stepper_bounds_match_rust_domains() {
    // (needle template, human-readable context) — templates use inline `{}`.
    let cases: Vec<String> = vec![
        // Encoder Volume Step: can_decrease/can_increase bounds + ±1 step.
        format!("root.volume_step_percent > {MIN_VOLUME_STEP_PERCENT}"),
        format!("root.volume_step_percent < {MAX_VOLUME_STEP_PERCENT}"),
        "root.volume_step_percent - 1".into(),
        "root.volume_step_percent + 1".into(),
        // Volume HUD Timeout: 250..=30000, step ±250 (MIN doubles as the step).
        format!("root.volume_hud_timeout_ms > {MIN_VOLUME_HUD_TIMEOUT_MS}"),
        format!("root.volume_hud_timeout_ms < {MAX_VOLUME_HUD_TIMEOUT_MS}"),
        format!("root.volume_hud_timeout_ms - {MIN_VOLUME_HUD_TIMEOUT_MS}"),
        format!("root.volume_hud_timeout_ms + {MIN_VOLUME_HUD_TIMEOUT_MS}"),
        // Volume HUD Fade: 0..=2000, step ±100.
        format!("root.volume_hud_fade_ms > {MIN_VOLUME_HUD_FADE_MS}"),
        format!("root.volume_hud_fade_ms < {MAX_VOLUME_HUD_FADE_MS}"),
        format!("root.volume_hud_fade_ms - {VOLUME_HUD_FADE_STEP_MS}"),
        format!("root.volume_hud_fade_ms + {VOLUME_HUD_FADE_STEP_MS}"),
        // Codec HUD Timeout: 250..=300000, step ±1000 (NOT MIN — the grid is
        // anchored at 250 + n*1000, see CODEC_HUD_TIMEOUT_STEP_MS docs).
        format!("root.codec_hud_timeout_ms > {MIN_CODEC_HUD_TIMEOUT_MS}"),
        format!("root.codec_hud_timeout_ms < {MAX_CODEC_HUD_TIMEOUT_MS}"),
        format!("root.codec_hud_timeout_ms - {CODEC_HUD_TIMEOUT_STEP_MS}"),
        format!("root.codec_hud_timeout_ms + {CODEC_HUD_TIMEOUT_STEP_MS}"),
    ];
    for needle in &cases {
        assert_bound_literal(needle);
    }
}
