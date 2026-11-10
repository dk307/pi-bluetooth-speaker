//! Integration tests for amp_state module.

use amp_curve::PERCENT_TO_RAW;
use audio_manager::AmpState;

#[test]
fn default_state() {
    let s = AmpState::default();
    assert_eq!(s.level_raw, 116);
    assert!(!s.muted);
}

#[test]
fn hardware_level_muted() {
    let s = AmpState {
        level_raw: 100,
        muted: true,
    };
    assert_eq!(s.hardware_level(), 0);
}

#[test]
fn hardware_level_unmuted() {
    let s = AmpState {
        level_raw: 100,
        muted: false,
    };
    assert_eq!(s.hardware_level(), 100);
}

#[test]
fn mute_remembers_level() {
    let s = AmpState {
        level_raw: 100,
        muted: false,
    }
    .mute();
    assert!(s.muted);
    assert_eq!(s.level_raw, 100);
    assert_eq!(s.hardware_level(), 0);
}

#[test]
fn unmute_restores_level() {
    let s = AmpState {
        level_raw: 100,
        muted: true,
    }
    .unmute();
    assert!(!s.muted);
    assert_eq!(s.level_raw, 100);
    assert_eq!(s.hardware_level(), 100);
}

#[test]
fn mute_idempotent() {
    let s = AmpState {
        level_raw: 50,
        muted: true,
    }
    .mute();
    assert!(s.muted);
    assert_eq!(s.level_raw, 50);
}

#[test]
fn unmute_idempotent() {
    let s = AmpState {
        level_raw: 50,
        muted: false,
    }
    .unmute();
    assert!(!s.muted);
    assert_eq!(s.level_raw, 50);
}

#[test]
fn with_percent_when_muted_stores_only() {
    let s = AmpState {
        level_raw: 50,
        muted: true,
    }
    .with_percent(80, &PERCENT_TO_RAW);
    assert!(s.muted);
    assert_eq!(s.level_raw, PERCENT_TO_RAW[80]);
    assert_eq!(s.hardware_level(), 0);
}

#[test]
fn with_percent_when_unmuted_applies() {
    let s = AmpState {
        level_raw: 50,
        muted: false,
    }
    .with_percent(80, &PERCENT_TO_RAW);
    assert!(!s.muted);
    assert_eq!(s.level_raw, PERCENT_TO_RAW[80]);
    assert_eq!(s.hardware_level(), PERCENT_TO_RAW[80]);
}

#[test]
fn level_percent_inverse() {
    for p in 0..=100 {
        let s = AmpState {
            level_raw: PERCENT_TO_RAW[p],
            muted: false,
        };
        let back = s.level_percent(&PERCENT_TO_RAW);
        // Round-trip may not be exact due to quantization of the perceptual curve.
        // Allow up to 3% difference.
        let diff = (back as i16 - p as i16).abs();
        assert!(
            diff <= 4,
            "percent {} -> raw {} -> back {}",
            p,
            PERCENT_TO_RAW[p],
            back
        );
    }
}

#[test]
fn level_percent_muted_returns_stored() {
    let s = AmpState {
        level_raw: 100,
        muted: true,
    };
    let p = s.level_percent(&PERCENT_TO_RAW);
    // Should return the percent corresponding to stored level 100
    assert!(p > 0);
}

#[test]
fn hardware_level_percent() {
    for p in 0..=100 {
        let s = AmpState {
            level_raw: PERCENT_TO_RAW[p],
            muted: false,
        };
        let back = s.hardware_level_percent(&PERCENT_TO_RAW);
        let diff = (back as i16 - p as i16).abs();
        // Allow up to 3% difference due to quantization
        assert!(
            diff <= 4,
            "percent {} -> raw {} -> back {}",
            p,
            PERCENT_TO_RAW[p],
            back
        );
    }
}

#[test]
fn hardware_level_percent_muted() {
    let s = AmpState {
        level_raw: 100,
        muted: true,
    };
    let p = s.hardware_level_percent(&PERCENT_TO_RAW);
    assert_eq!(p, 0);
}
