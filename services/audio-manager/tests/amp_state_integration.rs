//! Integration tests for amp_state module.

use audio_manager::AmpState;
use speaker_hardware::amp::PERCENT_TO_RAW;

#[test]
fn default_state() {
    let s = AmpState::default();
    assert_eq!(s.level_raw, 114);
    assert!(!s.muted);
}

#[test]
fn hardware_level_muted_keeps_level() {
    let s = AmpState {
        level_raw: 100,
        muted: true,
    };
    // Mute no longer touches the level: the serial mute register silences
    // the output while the mixer keeps its volume.
    assert_eq!(s.hardware_level(), 100);
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
fn mute_keeps_level() {
    let s = AmpState {
        level_raw: 100,
        muted: false,
    }
    .mute();
    assert!(s.muted);
    assert_eq!(s.level_raw, 100);
    assert_eq!(s.hardware_level(), 100);
}

#[test]
fn unmute_keeps_level() {
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
    assert_eq!(s.hardware_level(), PERCENT_TO_RAW[80]);
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
    for (p, &raw) in PERCENT_TO_RAW.iter().enumerate() {
        let s = AmpState {
            level_raw: raw,
            muted: false,
        };
        let back = s.level_percent(&PERCENT_TO_RAW);
        // The 1 dB register step means at most two percents share a raw
        // value, so the midpoint inverse is always within 1 %.
        let diff = (back as i16 - p as i16).abs();
        assert!(
            diff <= 1,
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
    for (p, &raw) in PERCENT_TO_RAW.iter().enumerate() {
        let s = AmpState {
            level_raw: raw,
            muted: false,
        };
        let back = s.hardware_level_percent(&PERCENT_TO_RAW);
        let diff = (back as i16 - p as i16).abs();
        // Midpoint inverse; at most two percents share a raw step.
        assert!(
            diff <= 1,
            "percent {} -> raw {} -> back {}",
            p,
            PERCENT_TO_RAW[p],
            back
        );
    }
}

#[test]
fn hardware_level_percent_muted_keeps_level() {
    // Mute no longer clamps the hardware level; it reads back as the level.
    let s = AmpState {
        level_raw: 100,
        muted: true,
    };
    let p = s.hardware_level_percent(&PERCENT_TO_RAW);
    // raw 100 (-44 dB) reads back as the midpoint of percent 27/28.
    assert_eq!(p, 28);
}
