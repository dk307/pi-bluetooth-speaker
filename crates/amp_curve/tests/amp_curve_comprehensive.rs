//! Comprehensive runtime tests for the amp-curve tables and transforms.
//!
//! The compile-time const initializers only evaluate the `build_*` functions
//! during const-evaluation (which yields no runtime coverage), so these tests
//! rebuild every table at runtime and verify it agrees with the exported const.

use amp_curve::{
    avrcp_avg_to_percent, build_avrcp_to_percent, build_percent_to_raw, build_raw_to_db_tenths,
    interp_db_tenths, raw_to_db, raw_to_db_tenths, AVRCP_MAX, AVRCP_TO_PERCENT, CALIBRATION_POINTS,
    PERCENT_MAX, PERCENT_TO_RAW, RAW_MAX, RAW_TO_DB_TENTHS,
};

#[test]
fn interp_db_tenths_clamps_below_and_above_domain() {
    // x <= x0 returns the low anchor (in tenths).
    assert_eq!(interp_db_tenths(0, 10, 20, -100, -50), -1000);
    assert_eq!(interp_db_tenths(10, 10, 20, -100, -50), -1000);
    // x >= x1 returns the high anchor (in tenths).
    assert_eq!(interp_db_tenths(20, 10, 20, -100, -50), -500);
    assert_eq!(interp_db_tenths(30, 10, 20, -100, -50), -500);
    // In-domain interpolation is linear.
    assert_eq!(interp_db_tenths(15, 10, 20, -100, -50), -750);
    // Positive dB segment (not used by the curve, but the fn is generic).
    assert_eq!(interp_db_tenths(5, 0, 10, 100, 200), 1500);
}

#[test]
fn interp_db_tenths_reproduces_calibration_segments() {
    // Segment boundaries of CALIBRATION_POINTS: (0,-144) (32,-20) (64,-10)
    // (96,-4) (127,0).
    assert_eq!(interp_db_tenths(0, 0, 32, -144, -20), -1440);
    assert_eq!(interp_db_tenths(32, 0, 32, -144, -20), -200);
    assert_eq!(interp_db_tenths(16, 0, 32, -144, -20), -820); // midpoint

    assert_eq!(interp_db_tenths(32, 32, 64, -20, -10), -200);
    assert_eq!(interp_db_tenths(64, 32, 64, -20, -10), -100);
    assert_eq!(interp_db_tenths(48, 32, 64, -20, -10), -150);

    assert_eq!(interp_db_tenths(64, 64, 96, -10, -4), -100);
    assert_eq!(interp_db_tenths(96, 64, 96, -10, -4), -40);
    assert_eq!(interp_db_tenths(80, 64, 96, -10, -4), -70);

    assert_eq!(interp_db_tenths(96, 96, 127, -4, 0), -40);
    assert_eq!(interp_db_tenths(127, 96, 127, -4, 0), 0);
    // 600/31 truncates to 19, so the in-domain result is -40 + 19 = -21.
    assert_eq!(interp_db_tenths(111, 96, 127, -4, 0), -21);
}

#[test]
fn build_percent_to_raw_matches_const_table() {
    let built = build_percent_to_raw();
    assert_eq!(built, PERCENT_TO_RAW, "runtime rebuild must equal const");
    // Endpoints.
    assert_eq!(built[0], 0);
    assert_eq!(built[100], 144);
    // Monotonic.
    for i in 1..=100 {
        assert!(built[i] >= built[i - 1], "non-monotonic at {i}");
    }
    // Every entry is in the hardware register range.
    for (i, &raw) in built.iter().enumerate() {
        assert!(raw <= RAW_MAX, "percent {i} maps to raw {raw} > MAX");
    }
}

#[test]
fn build_avrcp_to_percent_matches_const_table() {
    let built = build_avrcp_to_percent();
    assert_eq!(built, AVRCP_TO_PERCENT, "runtime rebuild must equal const");
    assert_eq!(built[0], 0);
    assert_eq!(built[127], 100);
    // Linear rescaling: percent = round(avrcp * 100 / 127).
    for avrcp in 0..=127u16 {
        let expected = ((avrcp * 100 + 63) / 127) as u8;
        assert_eq!(built[avrcp as usize], expected, "avrcp={avrcp}");
    }
    for i in 1..=127 {
        assert!(built[i] >= built[i - 1], "non-monotonic at {i}");
    }
}

#[test]
fn build_raw_to_db_tenths_matches_const_table() {
    let built = build_raw_to_db_tenths();
    assert_eq!(built, RAW_TO_DB_TENTHS, "runtime rebuild must equal const");
    assert_eq!(built[0], -1440);
    assert_eq!(built[144], 0);
    for raw in 0..=144usize {
        assert_eq!(built[raw], (raw as i16 - 144) * 10, "raw={raw}");
    }
}

#[test]
fn build_percent_to_raw_calibration_roundtrip() {
    // At each calibration AVRCP point, percent->raw must land within a few
    // raw steps of the calibrated raw (quantization 0..100 vs 0..127).
    let built = build_percent_to_raw();
    for &(avrcp, expected_raw, _db) in CALIBRATION_POINTS {
        let percent = ((u16::from(avrcp) * 100 + 50) / 127) as u8;
        let raw = built[usize::from(percent)];
        let diff = (i16::try_from(raw).unwrap() - i16::try_from(expected_raw).unwrap()).abs();
        assert!(
            diff <= 3,
            "avrcp={avrcp} percent={percent} raw={raw} expected={expected_raw} diff={diff}"
        );
    }
}

#[test]
fn avrcp_avg_to_percent_edge_cases() {
    // Exact boundaries.
    assert_eq!(avrcp_avg_to_percent(0.0), 0);
    assert_eq!(avrcp_avg_to_percent(127.0), 100);
    // Clamping beyond the AVRCP domain.
    assert_eq!(avrcp_avg_to_percent(-1.0), 0);
    assert_eq!(avrcp_avg_to_percent(128.0), 100);
    assert_eq!(avrcp_avg_to_percent(200.0), 100);
    // Fractional (per-channel average) inputs.
    assert_eq!(avrcp_avg_to_percent(63.5), 50);
    assert_eq!(avrcp_avg_to_percent(31.75), 25);
    assert_eq!(avrcp_avg_to_percent(95.25), 75);
    // NaN rounds to 0.0 then clamps to 0; infinities clamp to the ends.
    assert_eq!(avrcp_avg_to_percent(f64::NAN), 0);
    assert_eq!(avrcp_avg_to_percent(f64::INFINITY), 100);
    assert_eq!(avrcp_avg_to_percent(f64::NEG_INFINITY), 0);
}

#[test]
fn avrcp_avg_to_percent_matches_table_for_integers() {
    for avrcp in 0..=127u8 {
        assert_eq!(
            avrcp_avg_to_percent(f64::from(avrcp)),
            AVRCP_TO_PERCENT[usize::from(avrcp)],
            "avrcp={avrcp}"
        );
    }
}

#[test]
fn raw_to_db_tenths_exact_formula() {
    for raw in 0..=144 {
        assert_eq!(raw_to_db_tenths(raw), (raw as i16 - 144) * 10, "raw={raw}");
    }
}

#[test]
fn raw_to_db_float_exact() {
    assert_eq!(raw_to_db(0), -144.0);
    assert_eq!(raw_to_db(144), 0.0);
    assert_eq!(raw_to_db(72), -72.0);
    for raw in 0..=144 {
        let db = raw_to_db(raw);
        let expected = f32::from(raw as i16 - 144);
        assert!((db - expected).abs() < 0.001, "raw={raw} db={db}");
    }
}

#[test]
fn runtime_wrappers_match_const_tables() {
    for p in 0..=100u8 {
        assert_eq!(amp_curve::percent_to_raw(p), PERCENT_TO_RAW[usize::from(p)]);
    }
    for a in 0..=127u8 {
        assert_eq!(
            amp_curve::avrcp_to_percent(a),
            AVRCP_TO_PERCENT[usize::from(a)]
        );
    }
}

#[test]
fn calibration_points_structure() {
    assert_eq!(CALIBRATION_POINTS.len(), 5);
    assert_eq!(CALIBRATION_POINTS[0], (0, 0, -144));
    assert_eq!(CALIBRATION_POINTS[1], (32, 124, -20));
    assert_eq!(CALIBRATION_POINTS[2], (64, 134, -10));
    assert_eq!(CALIBRATION_POINTS[3], (96, 140, -4));
    assert_eq!(CALIBRATION_POINTS[4], (127, 144, 0));

    for i in 1..CALIBRATION_POINTS.len() {
        assert!(
            CALIBRATION_POINTS[i].0 > CALIBRATION_POINTS[i - 1].0,
            "avrcp not increasing at {i}"
        );
        assert!(
            CALIBRATION_POINTS[i].1 >= CALIBRATION_POINTS[i - 1].1,
            "raw not increasing at {i}"
        );
        assert!(
            CALIBRATION_POINTS[i].2 > CALIBRATION_POINTS[i - 1].2,
            "db not increasing at {i}"
        );
    }
}

#[test]
fn roundtrip_avrcp_to_percent_to_raw_preserves_calibration() {
    for &(avrcp, expected_raw, _db) in CALIBRATION_POINTS {
        let percent = avrcp_avg_to_percent(f64::from(avrcp));
        let raw = PERCENT_TO_RAW[usize::from(percent)];
        let diff = (i16::try_from(raw).unwrap() - i16::try_from(expected_raw).unwrap()).abs();
        assert!(
            diff <= 3,
            "avrcp={avrcp} percent={percent} raw={raw} expected={expected_raw} diff={diff}"
        );
    }
}

#[test]
fn constants_values() {
    assert_eq!(RAW_MAX, 144);
    assert_eq!(PERCENT_MAX, 100);
    assert_eq!(AVRCP_MAX, 127);
}
