//! Integration tests for the amp volume-curve tables and transforms.

use amp_curve::{
    AVRCP_TO_PERCENT, CALIBRATION_POINTS, PERCENT_TO_RAW, avrcp_to_percent, percent_to_raw,
    raw_to_db,
};

#[test]
fn percent_to_raw_endpoints() {
    assert_eq!(percent_to_raw(0), 0);
    assert_eq!(percent_to_raw(100), 144);
}

#[test]
fn avrcp_to_percent_endpoints() {
    assert_eq!(avrcp_to_percent(0), 0);
    assert_eq!(avrcp_to_percent(127), 100);
}

#[test]
fn monotonic_percent_to_raw() {
    for i in 1..=100 {
        assert!(
            PERCENT_TO_RAW[i] >= PERCENT_TO_RAW[i - 1],
            "non-monotonic at {}",
            i
        );
    }
}

#[test]
fn monotonic_avrcp_to_percent() {
    for i in 1..=127 {
        assert!(
            AVRCP_TO_PERCENT[i] >= AVRCP_TO_PERCENT[i - 1],
            "non-monotonic at {}",
            i
        );
    }
}

#[test]
fn roundtrip_avrcp_percent_raw_preserves_calibration() {
    // At the calibration AVRCP points, the roundtrip should give the calibrated raw
    for (avrcp, expected_raw, _db) in CALIBRATION_POINTS {
        let percent = avrcp_to_percent(*avrcp);
        let raw = percent_to_raw(percent);
        // Allow ±1 raw step due to quantization of percent (0..100 vs 0..127)
        let diff = (raw as i16 - *expected_raw as i16).abs();
        assert!(
            diff <= 1,
            "avrcp={} percent={} raw={} expected={} diff={}",
            avrcp,
            percent,
            raw,
            expected_raw,
            diff
        );
    }
}

#[test]
fn raw_to_db_exact() {
    for raw in 0..=144u16 {
        let db = raw_to_db(raw);
        let expected = (raw as i16 - 144) as f32;
        assert!(
            (db - expected).abs() < 0.001,
            "raw={} db={} expected={}",
            raw,
            db,
            expected
        );
    }
}
