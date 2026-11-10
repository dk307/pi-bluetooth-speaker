//! Shared perceptual volume curve for Merus MA12070P amplifier.
//!
//! This crate provides the single source of truth for volume mapping between:
//! - AVRCP absolute volume (0..127) from BlueZ/phone
//! - Percent volume (0..100) for the D-Bus API (`SetLevel`)
//! - Raw amplifier register (0..144, where 0 = -144 dB mute, 144 = 0 dB)
//!
//! The curve is defined by the live-verified calibration table in
//! `docs/rpi_audio_bluetooth_report.md` §7.6.4, which maps AVRCP volume to
//! mixer raw values via a perceptual dB curve. The Merus raw register is
//! linear in dB: `raw = 144 + dB` (where dB ≤ 0). The calibration points
//! give AVRCP → dB directly; we interpolate piecewise-linear on the dB domain
//! for both the percent and AVRCP mappings.

/// Raw amplifier register range (0 = -144 dB mute, 144 = 0 dB).
pub const RAW_MAX: u16 = 144;
/// Percent volume range (0..100).
pub const PERCENT_MAX: u8 = 100;
/// AVRCP absolute volume range (0..127).
pub const AVRCP_MAX: u8 = 127;

/// Calibration points from live measurement (AVRCP, raw, dB).
/// Source: docs/rpi_audio_bluetooth_report.md §7.6.4 table.
/// The dB values are derived from raw: dB = raw - 144 (since raw 0 = -144 dB).
const CALIBRATION_POINTS: &[(u8, u16, i16)] = &[
    (0, 0, -144), // mute endpoint
    (10, 44, -100),
    (20, 75, -69),
    (30, 93, -51),
    (40, 106, -38),
    (50, 116, -28),
    (60, 123, -21),
    (70, 128, -16),
    (80, 132, -12),
    (90, 136, -8),
    (100, 139, -5),
    (110, 141, -3),
    (120, 143, -1),
    (127, 144, 0), // 0 dB endpoint
];

/// Piecewise-linear interpolation on the dB domain.
/// Given a value `x` in domain [x0, x1] with corresponding dB values [db0, db1],
/// returns the interpolated dB (as tenths of dB for precision).
const fn interp_db_tenths(x: u16, x0: u16, x1: u16, db0: i16, db1: i16) -> i16 {
    if x <= x0 {
        return db0 * 10;
    }
    if x >= x1 {
        return db1 * 10;
    }
    // Linear interpolation: db = db0 + (db1 - db0) * (x - x0) / (x1 - x0)
    // Scaled by 10 for tenths of dB precision.
    let num = (db1 - db0) as i32 * (x - x0) as i32 * 10;
    let den = (x1 - x0) as i32;
    db0 * 10 + (num / den) as i16
}

/// Build the PERCENT_TO_RAW table at compile time.
/// Percent 0..100 maps to raw 0..144 via the perceptual dB curve.
const fn build_percent_to_raw() -> [u16; 101] {
    let mut table = [0u16; 101];
    let mut p = 0u16;
    while p <= 100 {
        // Convert percent to AVRCP domain for interpolation: avrcp = percent * 127 / 100
        let avrcp_x = (p * 127) / 100;
        // Find the calibration segment
        let mut db_tenths = 0i16;
        let mut i = 0;
        while i < CALIBRATION_POINTS.len() - 1 {
            let (avrcp0, _raw0, db0) = CALIBRATION_POINTS[i];
            let (avrcp1, _raw1, db1) = CALIBRATION_POINTS[i + 1];
            if (avrcp_x as u8) <= avrcp1 {
                db_tenths = interp_db_tenths(avrcp_x, avrcp0 as u16, avrcp1 as u16, db0, db1);
                break;
            }
            i += 1;
        }
        if db_tenths == 0 && p > 0 {
            // Past the last point (should only happen at p=100, avrcp=127)
            db_tenths = CALIBRATION_POINTS.last().unwrap().2 * 10;
        }
        // raw = 144 + dB, where dB = db_tenths / 10
        // Since dB ≤ 0, raw = 144 - |dB|
        let db = db_tenths / 10;
        let raw = (144i16 + db) as u16;
        if raw > RAW_MAX {
            table[p as usize] = RAW_MAX;
        } else {
            table[p as usize] = raw;
        }
        p += 1;
    }
    table
}

/// Build the AVRCP_TO_PERCENT table at compile time.
/// Simple linear rescaling: AVRCP 0..127 → Percent 0..100.
/// The perceptual curve is applied in PERCENT_TO_RAW (percent → raw).
const fn build_avrcp_to_percent() -> [u8; 128] {
    let mut table = [0u8; 128];
    let mut avrcp = 0u8;
    while avrcp <= AVRCP_MAX {
        // Linear mapping: percent = round(avrcp * 100 / 127)
        let percent = ((avrcp as u16 * 100 + 63) / 127) as u8;
        table[avrcp as usize] = percent;
        avrcp += 1;
    }
    table
}

/// Build the RAW_TO_DB_TENTHS table at compile time.
/// Raw 0..144 maps to dB × 10 (tenths of dB). Since raw = 144 + dB,
/// dB = raw - 144. This is exact, no interpolation needed.
const fn build_raw_to_db_tenths() -> [i16; 145] {
    let mut table = [0i16; 145];
    let mut raw = 0u16;
    while raw <= RAW_MAX {
        let db = raw as i16 - 144;
        table[raw as usize] = db * 10;
        raw += 1;
    }
    table
}

/// Percent (0..100) → Raw amplifier register (0..144).
/// This is the mapping used by audio-manager for `SetLevel(percent)`.
pub const PERCENT_TO_RAW: [u16; 101] = build_percent_to_raw();

/// AVRCP (0..127) → Percent (0..100).
/// This is the mapping used by bt-manager to translate phone AVRCP volume
/// to the audio-manager D-Bus API. It is the inverse of PERCENT_TO_RAW
/// composed with the calibration curve.
pub const AVRCP_TO_PERCENT: [u8; 128] = build_avrcp_to_percent();

/// Raw (0..144) → dB × 10 (tenths of dB).
/// Used for logging/debugging. Exact: dB = raw - 144.
pub const RAW_TO_DB_TENTHS: [i16; 145] = build_raw_to_db_tenths();

/// Convert percent to raw (runtime convenience, matches const table).
#[inline]
pub const fn percent_to_raw(percent: u8) -> u16 {
    PERCENT_TO_RAW[percent as usize]
}

/// Convert AVRCP to percent (runtime convenience, matches const table).
#[inline]
pub const fn avrcp_to_percent(avrcp: u8) -> u8 {
    AVRCP_TO_PERCENT[avrcp as usize]
}

/// Convert raw to dB (tenths).
#[inline]
pub const fn raw_to_db_tenths(raw: u16) -> i16 {
    RAW_TO_DB_TENTHS[raw as usize]
}

/// Convert raw to dB (float, for display).
#[inline]
pub fn raw_to_db(raw: u16) -> f32 {
    (raw as i16 - 144) as f32
}

/// Compile-time invariant proofs.
const _: () = {
    // Endpoints exact
    assert!(PERCENT_TO_RAW[0] == 0); // 0% → mute (raw 0 = -144 dB)
    assert!(PERCENT_TO_RAW[100] == 144); // 100% → 0 dB (raw 144)
    assert!(AVRCP_TO_PERCENT[0] == 0); // AVRCP 0 → 0%
    assert!(AVRCP_TO_PERCENT[127] == 100); // AVRCP 127 → 100%
                                           // Monotonicity
    let mut p = 1;
    while p <= 100 {
        assert!(PERCENT_TO_RAW[p] >= PERCENT_TO_RAW[p - 1]);
        p += 1;
    }
    let mut a = 1;
    while a <= 127 {
        assert!(AVRCP_TO_PERCENT[a] >= AVRCP_TO_PERCENT[a - 1]);
        a += 1;
    }
    // Raw to dB exact
    assert!(RAW_TO_DB_TENTHS[0] == -1440);
    assert!(RAW_TO_DB_TENTHS[144] == 0);
    let mut r = 1;
    while r <= 144 {
        assert!(RAW_TO_DB_TENTHS[r] == (r as i16 - 144) * 10);
        r += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;

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
        for raw in 0..=144 {
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
}
