//! Integration tests for the AVRCP absolute-volume → percent rescale.
//!
//! The rescale is deliberately plain and linear: the perceptual dB curve is
//! applied once, by the amplifier's percent → register mapping. AVRCP 0 maps
//! to 0 %, which the amplifier treats as a hard mute.

use speaker_bluetooth::{
    AVRCP_VOLUME_MAX, PERCENT_VOLUME_MAX, avrcp_avg_to_percent, percent_to_bluealsa_level,
};

#[test]
fn endpoints_map_to_silence_and_full_scale() {
    assert_eq!(avrcp_avg_to_percent(0.0), 0);
    assert_eq!(
        avrcp_avg_to_percent(f64::from(AVRCP_VOLUME_MAX)),
        PERCENT_VOLUME_MAX
    );
}

#[test]
fn midpoint_maps_to_fifty_percent() {
    let mid = f64::from(AVRCP_VOLUME_MAX) / 2.0;
    assert_eq!(avrcp_avg_to_percent(mid), 50);
}

#[test]
fn every_avrcp_level_is_monotonic() {
    let mut previous = 0;
    for avrcp in 0..=AVRCP_VOLUME_MAX {
        let percent = avrcp_avg_to_percent(f64::from(avrcp));
        assert!(
            percent >= previous,
            "non-monotonic at avrcp {avrcp}: {percent} < {previous}"
        );
        previous = percent;
    }
}

#[test]
fn matches_rounding_of_the_integer_table_formula() {
    // The fractional helper and the documented integer formula must agree:
    // percent = round(avrcp * 100 / 127).
    for avrcp in 0..=AVRCP_VOLUME_MAX {
        let expected = (u16::from(avrcp) * 100 + 63) / 127;
        let expected = u8::try_from(expected).expect("0..=100 fits in u8");
        assert_eq!(
            avrcp_avg_to_percent(f64::from(avrcp)),
            expected,
            "avrcp {avrcp}"
        );
    }
}

#[test]
fn out_of_range_inputs_clamp() {
    assert_eq!(avrcp_avg_to_percent(-50.0), 0);
    assert_eq!(avrcp_avg_to_percent(400.0), PERCENT_VOLUME_MAX);
    // A single channel at maximum (not averaged) is the upper bound seen live.
    assert_eq!(avrcp_avg_to_percent(127.0), PERCENT_VOLUME_MAX);
}

#[test]
fn integer_roundtrip_through_bluealsa_encoding_is_stable() {
    // AVRCP level → percent → 7-bit BlueALSA level must recover the original
    // integer level for every representable step (the encoder and decoder are
    // exact inverses for integer percentages).
    for avrcp in 0..=AVRCP_VOLUME_MAX {
        let percent = avrcp_avg_to_percent(f64::from(avrcp));
        let encoded = percent_to_bluealsa_level(percent);
        let decoded = avrcp_avg_to_percent(f64::from(encoded));
        assert_eq!(decoded, percent, "avrcp {avrcp} percent {percent}");
    }
}
