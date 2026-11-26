//! Integration tests for the pure ambient brightness control policy.
//!
//! Exercises hysteresis deadbands, the zero-lux floor, asymmetric debounce and
//! slew timing, manual gamma bias, input sanitization, and configuration
//! validation without any I2C or sysfs hardware.

#![cfg(feature = "ui")]

use screen_manager::screen_brightness::{
    AutoBrightness, BrightnessConfig, BrightnessConfigError, DEFAULT_BRIGHTNESS_CONFIG,
};

type BrightnessConfigMutation = (&'static str, fn(&mut BrightnessConfig));

#[test]
fn default_config_is_valid_and_matches_documented_values() {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AutoBrightness>();
    assert_send_sync::<BrightnessConfig>();
    assert_send_sync::<BrightnessConfigError>();

    DEFAULT_BRIGHTNESS_CONFIG.validate().unwrap();
    let cfg = BrightnessConfig::default();
    assert_eq!(cfg, DEFAULT_BRIGHTNESS_CONFIG);
    assert!((cfg.ema_alpha - 0.2).abs() < 1e-6);
    assert!((cfg.hyst_up - 0.25).abs() < 1e-6);
    assert!((cfg.hyst_down - 0.30).abs() < 1e-6);
    assert!((cfg.debounce_up - 1.5).abs() < 1e-6);
    assert!((cfg.debounce_down - 5.0).abs() < 1e-6);
    assert!((cfg.rate_up - 0.40).abs() < 1e-6);
    assert!((cfg.rate_down - 0.15).abs() < 1e-6);
    assert!((cfg.b_min - 0.05).abs() < 1e-6);
    assert!((cfg.gamma_min - 0.30).abs() < 1e-6);
    assert!((cfg.gamma_max - 3.00).abs() < 1e-6);
    assert!((cfg.pwm_gamma - 2.20).abs() < 1e-6);
}

#[test]
fn base_curve_and_target_are_clamped_to_floor_and_ceiling() {
    const B_MIN: f32 = DEFAULT_BRIGHTNESS_CONFIG.b_min;
    let state = AutoBrightness::new(100.0, DEFAULT_BRIGHTNESS_CONFIG);

    let dark = state.base_curve(0.0);
    assert!((dark - B_MIN).abs() < 1e-6);
    assert!((state.base_curve(-50.0) - B_MIN).abs() < 1e-6);
    let bright = state.base_curve(1.0e9);
    assert!((bright - 1.0).abs() < 1e-6, "bright = {bright}");

    assert!((state.target_from_lux(0.0) - B_MIN).abs() < 1e-6);
    assert!((state.target_from_lux(100.0) - state.base_curve(100.0)).abs() < 1e-6);
}

#[test]
fn brightness_stays_inside_deadband() {
    let initial_lux = 100.0;
    let mut state = AutoBrightness::new(initial_lux, DEFAULT_BRIGHTNESS_CONFIG);
    let initial_target = state.target();
    let initial_anchor = state.lux_anchor();

    for idx in 0..40 {
        let lux = if idx % 2 == 0 { 95.0 } else { 105.0 };
        let _ = state.update(lux, 0.1);
        assert!((state.target() - initial_target).abs() < 1e-6);
        assert!((state.lux_anchor() - initial_anchor).abs() < 1e-6);
    }
}

#[test]
fn zero_lux_holds_floor_without_chatter() {
    let mut state = AutoBrightness::new(0.0, DEFAULT_BRIGHTNESS_CONFIG);
    let floor = DEFAULT_BRIGHTNESS_CONFIG.b_min;
    assert!((state.brightness() - floor).abs() < 1e-6);
    assert!((state.lux_anchor() - 0.0).abs() < 1e-6);

    for _ in 0..30 {
        let duty = state.update(0.0, 0.5);
        assert!((state.brightness() - floor).abs() < 1e-6);
        assert!((state.target() - floor).abs() < 1e-6);
        assert!((state.lux_anchor() - 0.0).abs() < 1e-6);
        assert!((duty - floor.powf(DEFAULT_BRIGHTNESS_CONFIG.pwm_gamma)).abs() < 1e-6);
    }
}

#[test]
fn upward_deviation_commits_before_downward_deviation() {
    let mut rising = AutoBrightness::new(100.0, DEFAULT_BRIGHTNESS_CONFIG);
    let mut falling = AutoBrightness::new(100.0, DEFAULT_BRIGHTNESS_CONFIG);
    let rising_initial = rising.target();
    let falling_initial = falling.target();

    for _ in 0..6 {
        let _ = rising.update(1000.0, 0.5);
        let _ = falling.update(0.0, 0.5);
    }

    assert!(
        rising.target() > rising_initial + 1e-3,
        "upward target did not commit: {} -> {}",
        rising_initial,
        rising.target()
    );
    assert!(
        (falling.target() - falling_initial).abs() < 1e-6,
        "downward target committed too early: {} -> {}",
        falling_initial,
        falling.target()
    );

    for _ in 0..12 {
        let _ = falling.update(0.0, 0.5);
    }
    assert!(
        falling.target() < falling_initial - 1e-3,
        "downward target never committed"
    );
}

#[test]
fn upward_and_downward_ramp_rates_are_asymmetric() {
    let mut rising = AutoBrightness::new(1000.0, DEFAULT_BRIGHTNESS_CONFIG);
    rising.on_manual_change(DEFAULT_BRIGHTNESS_CONFIG.b_min);
    rising.reset_manual_bias();
    let rise_start = rising.brightness();
    let rise_next = rising.update(1000.0, 0.1);
    let rise_delta = rising.brightness() - rise_start;

    let mut falling = AutoBrightness::new(0.0, DEFAULT_BRIGHTNESS_CONFIG);
    falling.on_manual_change(1.0);
    falling.reset_manual_bias();
    let fall_start = falling.brightness();
    let fall_next = falling.update(0.0, 0.1);
    let fall_delta = fall_start - falling.brightness();

    assert!(rise_delta > 0.0, "rising brightness did not increase");
    assert!(fall_delta > 0.0, "falling brightness did not decrease");
    assert!(
        rise_delta > fall_delta * 2.0,
        "expected faster rise ({rise_delta}) than fall ({fall_delta})"
    );
    assert!((0.0..=1.0).contains(&rise_next));
    assert!((0.0..=1.0).contains(&fall_next));
}

#[test]
fn integration_step_is_clamped_to_half_a_second() {
    let mut state = AutoBrightness::new(1000.0, DEFAULT_BRIGHTNESS_CONFIG);
    state.on_manual_change(DEFAULT_BRIGHTNESS_CONFIG.b_min);
    state.reset_manual_bias();
    let before = state.brightness();

    let _ = state.update(1000.0, 3600.0);
    let huge_delta = state.brightness() - before;
    let max_delta = DEFAULT_BRIGHTNESS_CONFIG.rate_up * 0.5 + 1e-6;
    assert!(huge_delta <= max_delta, "dt clamp ignored: {huge_delta}");

    let before_nan = state.brightness();
    let _ = state.update(1000.0, f32::NAN);
    assert!((state.brightness() - before_nan).abs() < 1e-6);

    let negative_before = state.brightness();
    let _ = state.update(1000.0, -5.0);
    assert!((state.brightness() - negative_before).abs() < 1e-6);
}

#[test]
fn manual_change_sets_gamma_and_snap_then_reset_clears_gamma_only() {
    let mut state = AutoBrightness::new(200.0, DEFAULT_BRIGHTNESS_CONFIG);
    let initial_brightness = state.brightness();

    state.on_manual_change(0.6);
    assert!((state.brightness() - 0.6).abs() < 1e-6);
    assert!((state.target() - 0.6).abs() < 1e-6);
    assert!(state.manual_gamma() >= DEFAULT_BRIGHTNESS_CONFIG.gamma_min);
    assert!(state.manual_gamma() <= DEFAULT_BRIGHTNESS_CONFIG.gamma_max);
    assert_ne!(initial_brightness, state.brightness());

    let snapped = state.brightness();
    state.reset_manual_bias();
    assert!((state.manual_gamma() - 1.0).abs() < 1e-6);
    assert!(
        (state.brightness() - snapped).abs() < 1e-6,
        "reset snapped brightness"
    );
    assert!((state.target() - state.base_curve(state.lux_ema())).abs() < 1e-6);
}

#[test]
fn bright_room_manual_dim_survives_hysteresis_reanchor() {
    let mut state = AutoBrightness::new(10_000.0, DEFAULT_BRIGHTNESS_CONFIG);
    state.on_manual_change(0.3);
    assert!((state.brightness() - 0.3).abs() < 1e-6);

    // A large sustained ambient rise re-anchors the auto curve. The bounded
    // gamma exponent alone cannot express 30% when base ~= 1; the multiplicative
    // bias must keep the user's level instead of slewing back to full.
    for _ in 0..40 {
        let _ = state.update(20_000.0, 0.5);
    }
    assert!(
        state.target() <= 0.32,
        "manual dim was undone in a bright room: target={}",
        state.target()
    );
    assert!(
        state.brightness() <= 0.32,
        "manual dim was undone in a bright room: brightness={}",
        state.brightness()
    );
}

#[test]
fn manual_change_clamps_bounds_and_ignores_non_finite_input() {
    let mut state = AutoBrightness::new(200.0, DEFAULT_BRIGHTNESS_CONFIG);

    state.on_manual_change(0.0001);
    assert!((state.brightness() - DEFAULT_BRIGHTNESS_CONFIG.b_min).abs() < 1e-6);
    state.on_manual_change(25.0);
    assert!((state.brightness() - 1.0).abs() < 1e-6);

    let before = state.manual_gamma();
    state.on_manual_change(f32::NAN);
    assert!((state.manual_gamma() - before).abs() < 1e-6);
    assert!((state.brightness() - 1.0).abs() < 1e-6);
}

#[test]
fn non_finite_and_negative_lux_do_not_poison_state() {
    let mut state = AutoBrightness::new(120.0, DEFAULT_BRIGHTNESS_CONFIG);
    let initial_ema = state.lux_ema();

    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -25.0] {
        let duty = state.update(bad, 0.1);
        assert!(state.lux_ema().is_finite());
        assert!(state.brightness().is_finite());
        assert!(duty.is_finite());
        assert!(
            (state.lux_ema() - initial_ema).abs() < 1e-6,
            "bad lux {bad} moved EMA"
        );
    }

    let floor = DEFAULT_BRIGHTNESS_CONFIG.b_min;
    assert!((state.base_curve(f32::NAN) - floor).abs() < 1e-6);
    assert!((state.target_from_lux(f32::INFINITY) - 1.0).abs() < 1e-6);
}

#[test]
fn config_validation_rejects_each_invalid_boundary() {
    let mut changes: Vec<BrightnessConfigMutation> = vec![
        ("ema_alpha_nan", |c| c.ema_alpha = f32::NAN),
        ("hyst_inf", |c| c.hyst_down = f32::INFINITY),
        ("ema_zero", |c| c.ema_alpha = 0.0),
        ("ema_over_one", |c| c.ema_alpha = 1.1),
        ("hyst_negative", |c| c.hyst_up = -0.1),
        ("debounce_zero", |c| c.debounce_up = 0.0),
        ("rate_negative", |c| c.rate_down = -0.1),
        ("b_min_zero", |c| c.b_min = 0.0),
        ("b_min_one", |c| c.b_min = 1.0),
        ("gamma_zero", |c| c.gamma_min = 0.0),
        ("gamma_inverted", |c| c.gamma_max = 0.1),
        ("pwm_zero", |c| c.pwm_gamma = 0.0),
    ];

    for (name, mutate) in changes.drain(..) {
        let mut cfg = DEFAULT_BRIGHTNESS_CONFIG;
        mutate(&mut cfg);
        let err = cfg.validate().unwrap_err();
        assert!(
            err.is_not_finite() || err.is_out_of_range(),
            "unexpected error kind for {name}"
        );
        assert!(!err.summary().is_empty(), "empty summary for {name}");
        assert!(
            !err.summary().contains("Backtrace"),
            "backtrace leaked for {name}"
        );
    }
}
