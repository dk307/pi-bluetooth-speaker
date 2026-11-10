//! Host tests for the pure serial-mute bit helpers.

use audio_manager::i2c_mute::{apply_mute_bit, is_muted_bit};

#[test]
fn apply_mute_bit_sets_bit7_preserving_rest() {
    // Datasheet default 0x41 (limiter on, play) -> 0xC1 when muted.
    assert_eq!(apply_mute_bit(0x41, true), 0xC1);
    // Already-muted register stays unchanged.
    assert_eq!(apply_mute_bit(0xC1, true), 0xC1);
}

#[test]
fn apply_mute_bit_clears_bit7_preserving_rest() {
    assert_eq!(apply_mute_bit(0xC1, false), 0x41);
    // Already-playing register stays unchanged.
    assert_eq!(apply_mute_bit(0x41, false), 0x41);
    // Limiter/attack bits (0x35 style values) survive the RMW.
    assert_eq!(apply_mute_bit(0x88, true), 0x88 | 0x80);
    assert_eq!(apply_mute_bit(0xFF, false), 0x7F);
    assert_eq!(apply_mute_bit(0x00, true), 0x80);
    assert_eq!(apply_mute_bit(0x80, false), 0x00);
}

#[test]
fn is_muted_bit_reads_bit7_only() {
    assert!(!is_muted_bit(0x41));
    assert!(is_muted_bit(0xC1));
    assert!(is_muted_bit(0x80));
    assert!(!is_muted_bit(0x00));
    assert!(!is_muted_bit(0x7F));
}

#[test]
fn mute_bit_roundtrip_is_idempotent() {
    for reg in [0x00u8, 0x41, 0x80, 0xC1, 0xFF, 0x88] {
        let muted = apply_mute_bit(reg, true);
        assert!(is_muted_bit(muted));
        let restored = apply_mute_bit(muted, false);
        assert_eq!(restored, reg & !0x80);
    }
}
