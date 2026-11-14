//! Integration tests for the shared `BlueALSA` A2DP PCM-path predicate.

use speaker_bluetooth::transport::is_a2dp_pcm_path;

#[test]
fn accepts_both_known_transport_spellings() {
    // bluez-alsa has shipped `a2dpsnk` and `a2dpsink`; both are A2DP PCMs.
    assert!(is_a2dp_pcm_path(
        "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsnk/source"
    ));
    assert!(is_a2dp_pcm_path(
        "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsink/source"
    ));
    assert!(is_a2dp_pcm_path(
        "/org/bluealsa/hci1/dev_AA_BB_CC_DD_EE_FF/a2dpsrc/sink"
    ));
}

#[test]
fn rejects_hfp_sco_and_unrelated_paths() {
    for path in [
        "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/hfphf/sink",
        "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/hfp_hf/source",
        "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/sco/source",
        "/org/bluez/hci0/dev_30_E0_44_67_38_A2",
        "/org/bluealsa",
        "",
    ] {
        assert!(!is_a2dp_pcm_path(path), "false positive on {path}");
    }
}

#[test]
fn only_matches_whole_path_segments() {
    // A longer segment merely containing `a2dp` must not match.
    assert!(!is_a2dp_pcm_path(
        "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/nota2dpsnk/source"
    ));
}
