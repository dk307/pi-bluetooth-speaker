//! Integration tests for the volume-sync state machine and D-Bus parsers.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bt_manager::volume_sync::{
    a2dp_volume, interfaces_added_path, is_bluealsa_volume_signal, VolumeSyncState, ECHO_CAPACITY,
    FRESH_PCM_SETTLE,
};
use speaker_bluetooth::{BlueAlsaPcm, BtAddress};

fn pcm(path: &str, running: bool, volume_percent: Option<u8>) -> BlueAlsaPcm {
    BlueAlsaPcm {
        pcm_path: path.into(),
        device_address: BtAddress::new([0x30, 0xE0, 0x44, 0x67, 0x38, 0xA2]),
        resolved_name: "Pixel".into(),
        transport: "A2DP-sink".into(),
        running,
        selected_codec: "SBC".into(),
        available_codecs: Box::new([]),
        sampling_rate: 44_100,
        channels: 2,
        format: "S16_LE".into(),
        volume: None,
        volume_percent,
        muted: false,
        delay: None,
    }
}

const A2DP: &str = "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsnk/source";
/// Alternate transport token emitted by other bluez-alsa releases.
const A2DP_ALT: &str = "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsink/source";
const HFP: &str = "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/hfphf/sink";

#[test]
fn seeded_volume_is_not_forwarded() {
    let now = Instant::now();
    let mut state = VolumeSyncState::new();
    state.seed_pcm_volume(100);
    assert!(
        !state.should_forward_pcm(100, now),
        "a seeded BlueALSA default must never be applied to the amp"
    );
    assert!(state.should_forward_pcm(99, now));
}

#[test]
fn forward_dedup_tracks_applied_values() {
    let now = Instant::now();
    let mut state = VolumeSyncState::new();
    assert!(state.should_forward_pcm(50, now));
    state.record_forward_applied(50);
    assert!(!state.should_forward_pcm(50, now));
    assert!(state.should_forward_pcm(51, now));
}

#[test]
fn pcm_path_gating_tracks_established_pcms() {
    // Regression (CodeRabbit, heavy-lift race): a fresh PCM's default-volume
    // `PropertiesChanged` can arrive before its `InterfacesAdded`. Signals
    // from untracked paths are gated; only after establishment is the path
    // accepted (and the fresh-connect suppression window applies).
    let mut state = VolumeSyncState::new();
    assert!(!state.is_pcm_known(A2DP));
    assert!(state.mark_pcm_known(A2DP), "first sighting is new");
    assert!(!state.mark_pcm_known(A2DP), "second sighting is not new");
    assert!(state.is_pcm_known(A2DP));
    assert!(
        !state.is_pcm_known(A2DP_ALT),
        "alternate spelling is its own path"
    );

    // The alternate-spelling path is tracked independently; both spellings
    // map through `is_a2dp_pcm_path` at the call site.
    assert!(state.mark_pcm_known(A2DP_ALT));
    assert!(state.is_pcm_known(A2DP_ALT));
}

#[test]
fn forward_apply_records_last_pushed_level() {
    // Regression (CodeRabbit): a forward write must record the applied value
    // as `last_pushed_level`, mirroring `record_reverse_push`. Without it,
    // amp/phone at 40 with a stale `last_pushed_level == Some(40)` makes the
    // return to 40 a no-op after the 60 echo is consumed: the phone stays at
    // 60 while the amp is at 40 until the level changes or the service
    // restarts.
    let mut state = VolumeSyncState::new();
    state.record_forward_applied(40);
    state.record_forward_applied(60);
    assert!(state.take_echo(60), "the 60 echo must be consumed");
    assert!(
        state.should_push_level(40),
        "returning to 40 after the consumed 60 echo must push back to the phone"
    );
    assert!(!state.should_push_level(60));
}

#[test]
fn echo_queue_tolerates_one_percent_and_is_bounded() {
    let mut state = VolumeSyncState::new();
    state.record_forward_applied(50);
    assert!(state.take_echo(51), "raw quantization may read back ±1");
    assert!(!state.take_echo(50), "an echo is consumed exactly once");

    let mut state = VolumeSyncState::new();
    for index in 0..ECHO_CAPACITY {
        // Spacing of two avoids ±1 cross-matches between queued entries.
        state.record_forward_applied((index as u8) * 2);
    }
    state.record_forward_applied(120);
    assert!(
        !state.take_echo(0),
        "the oldest echo must be evicted at capacity"
    );
    assert!(state.take_echo(2));
    assert!(state.take_echo(120));
}

#[test]
fn reverse_dedup_only_pushes_genuine_changes() {
    let mut state = VolumeSyncState::new();
    assert!(state.should_push_level(40));
    assert!(state.should_push_muted(false));
    state.record_reverse_push(Some(40), Some(false));
    assert!(!state.should_push_level(40));
    assert!(!state.should_push_muted(false));
    assert_eq!(state.last_pcm_vol(), Some(40));
    assert!(!state.should_forward_pcm(40, Instant::now()));
}

#[test]
fn fresh_pcm_suppresses_forward_and_forces_reverse() {
    let now = Instant::now();
    let mut state = VolumeSyncState::new();
    state.record_forward_applied(30);
    state.record_reverse_push(Some(40), Some(false));

    state.arm_fresh_pcm(now);
    assert!(state.suppression_active(now + Duration::from_millis(1)));
    assert!(!state.suppression_active(now + FRESH_PCM_SETTLE));
    assert!(
        !state.should_forward_pcm(60, now),
        "the settle window must suppress BlueALSA's initial volume"
    );
    assert!(
        state.should_push_level(40),
        "a fresh PCM must receive the amp level even when unchanged"
    );
    assert!(
        state.should_push_muted(false),
        "a fresh PCM must receive the amp mute state"
    );
    assert!(state.last_pcm_vol().is_none());
}

#[test]
fn interfaces_added_body_path_is_used() {
    let message = zbus::Message::signal(
        "/org/bluealsa",
        "org.freedesktop.DBus.ObjectManager",
        "InterfacesAdded",
    )
    .expect("build InterfacesAdded signal")
    .build(&(
        zbus::zvariant::ObjectPath::try_from(A2DP).expect("valid object path"),
        HashMap::<String, HashMap<String, zbus::zvariant::OwnedValue>>::new(),
    ))
    .expect("serialize InterfacesAdded body");

    assert_eq!(
        interfaces_added_path(&message.body()).as_deref(),
        Some(A2DP),
        "the body path must win over the constant ObjectManager header path"
    );
}

#[test]
fn a2dp_volume_ignores_hfp_and_prefers_running_stream() {
    let pcms = vec![
        pcm(HFP, true, Some(10)),
        pcm(A2DP, false, Some(40)),
        pcm(A2DP, true, Some(55)),
    ];
    assert_eq!(a2dp_volume(&pcms), Some(55));

    let pcms = vec![pcm(HFP, true, Some(10)), pcm(A2DP, false, Some(40))];
    assert_eq!(a2dp_volume(&pcms), Some(40));

    let pcms = vec![pcm(HFP, true, Some(10))];
    assert_eq!(a2dp_volume(&pcms), None);
}

#[test]
fn a2dp_detection_accepts_both_transport_spellings() {
    // bluez-alsa has shipped `a2dpsnk` and `a2dpsink`; either must drive sync.
    for path in [A2DP, A2DP_ALT] {
        assert_eq!(a2dp_volume(&[pcm(path, true, Some(42))]), Some(42));
        assert!(
            is_bluealsa_volume_signal(Some(path), "org.bluealsa.PCM1", ["Volume"].into_iter()),
            "volume signal must be recognised on {path}"
        );
    }
    assert!(!is_bluealsa_volume_signal(
        Some(HFP),
        "org.bluealsa.PCM1",
        ["Volume"].into_iter()
    ));
}
