//! Integration tests for the MA12070P master-volume percent → raw curve.
//!
//! The curve is linear in dB across a 60 dB usable range: 0 % is the hard-mute
//! anchor (raw 0 = −144 dB), 1 % is −60 dB, and 100 % is 0 dB.

use speaker_hardware::amp::{MASTER_VOLUME_RAW_MAX, MIN_USABLE_DB, PERCENT_TO_RAW};

/// Master-volume raw as a dB value (raw 0 = −144 dB, 144 = 0 dB).
fn raw_db(raw: u16) -> f64 {
    f64::from(raw) - f64::from(MASTER_VOLUME_RAW_MAX)
}

/// Expected raw level for a percent before quantization: `144 + dB`, with the
/// dB domain linear from −60 dB at 1 % to 0 dB at 100 %.
fn expected_raw(percent: u8) -> f64 {
    let db = f64::from(MIN_USABLE_DB)
        + f64::from(-MIN_USABLE_DB) * f64::from(percent - 1) / f64::from(99);
    144.0 + db
}

#[test]
fn curve_endpoints_are_mute_and_full_scale() {
    assert_eq!(PERCENT_TO_RAW[0], 0, "0 % must be the hard-mute anchor");
    assert_eq!(
        PERCENT_TO_RAW[100], MASTER_VOLUME_RAW_MAX,
        "100 % must be 0 dB"
    );
}

#[test]
fn curve_perceptual_anchors() {
    assert_eq!(PERCENT_TO_RAW[1], 84, "1 % must be −60 dB");
    assert_eq!(PERCENT_TO_RAW[50], 114, "50 % must be about −30 dB");
}

#[test]
fn curve_is_monotonic_without_large_steps() {
    for i in 2..=100 {
        let delta = PERCENT_TO_RAW[i] - PERCENT_TO_RAW[i - 1];
        assert!(
            delta <= 1,
            "percent step {i} jumps {delta} raw steps (expected <= 1)"
        );
    }
}

#[test]
fn every_percent_matches_linear_db_curve() {
    for p in 1..=100u8 {
        let expected = expected_raw(p);
        let actual = f64::from(PERCENT_TO_RAW[usize::from(p)]);
        assert!(
            (actual - expected).abs() <= 0.500_001,
            "percent {p}: raw {actual} expected {expected}"
        );
    }
}

#[test]
fn midpoint_is_half_scale_in_db() {
    let db = raw_db(PERCENT_TO_RAW[50]);
    assert!(
        (-31.0..=-29.0).contains(&db),
        "50 % maps to {db} dB, expected about −30 dB"
    );
}

#[test]
fn adjacent_percent_steps_stay_under_one_and_a_half_db() {
    // The usable range is 60 dB over 99 steps. Raw quantization may add up to
    // one extra 1 dB step, so assert the dB jump stays below 1.5 dB.
    for p in 2..=100u8 {
        let a = raw_db(PERCENT_TO_RAW[usize::from(p - 1)]);
        let b = raw_db(PERCENT_TO_RAW[usize::from(p)]);
        let step = b - a;
        assert!(
            (0.0..1.5).contains(&step),
            "percent {p}: step {step} dB outside 0..1.5"
        );
    }
}

#[test]
fn avrcp_level_rescaled_through_percent_lands_within_tolerance() {
    // End-to-end contract with the Bluetooth side: AVRCP 0..127 is rescaled
    // linearly to percent (0..127 → 0..100), then percent → raw here. Verify
    // the composite tracks the ideal 84 + 60·avrcp/127 raw level.
    for avrcp in 0..=127u8 {
        let percent = (u16::from(avrcp) * 100 + 63) / 127;
        let percent = u8::try_from(percent).expect("0..=100 fits in u8");
        let raw = f64::from(PERCENT_TO_RAW[usize::from(percent)]);
        if avrcp == 0 {
            // AVRCP minimum means silence; percent 0 is the hard-mute anchor.
            assert_eq!(percent, 0);
            assert_eq!(raw, 0.0, "avrcp 0 must hard-mute");
            continue;
        }
        // Two quantizations stack: the 0.79 % AVRCP→percent rescale
        // (±0.30 dB) and the 1 dB register rounding (±0.5 dB); 1.5 dB is the
        // tight bound covering both.
        let expected = 84.0 + 60.0 * f64::from(avrcp) / 127.0;
        assert!(
            (raw - expected).abs() <= 1.5,
            "avrcp {avrcp}: percent {percent} raw {raw} expected {expected}"
        );
    }
}
