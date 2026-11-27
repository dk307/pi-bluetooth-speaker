//! Integration tests for the volume-sync state machine and D-Bus parsers.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bt_manager::volume_sync::{
    ECHO_CAPACITY, FRESH_PCM_SETTLE, VolumeSyncState, a2dp_volume, interfaces_added_path,
    is_bluealsa_volume_signal,
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

#[test]
fn active_device_scoping_ignores_shutout_pcm_and_rearms_on_switch() {
    use bt_manager::volume_sync::a2dp_volume_for;
    use speaker_bluetooth::BtAddress;

    let pcm_a = "/org/bluealsa/hci0/dev_C4_03_A8_BD_EF_74/a2dpsnk/source";
    let pcm_b = "/org/bluealsa/hci0/dev_30_E0_44_67_38_A2/a2dpsnk/source";
    let addr_a = BtAddress::parse_str("C4:03:A8:BD:EF:74").unwrap();
    let addr_b = BtAddress::parse_str("30:E0:44:67:38:A2").unwrap();

    let t0 = Instant::now();
    let mut state = VolumeSyncState::new();
    assert!(state.is_active_pcm_path(pcm_a));
    assert!(state.is_active_pcm_path(pcm_b));

    assert!(state.set_active_device(Some(addr_a), t0));
    assert!(state.is_active_pcm_path(pcm_a));
    assert!(!state.is_active_pcm_path(pcm_b));
    assert!(state.suppression_active(t0));

    // Setting the same active device again does not re-arm suppression.
    assert!(!state.set_active_device(Some(addr_a), t0 + FRESH_PCM_SETTLE + Duration::from_secs(1)));

    // Switching to B re-arms suppression and swaps the allowed PCM path.
    let t2 = t0 + FRESH_PCM_SETTLE + Duration::from_secs(2);
    assert!(state.set_active_device(Some(addr_b), t2));
    assert!(!state.is_active_pcm_path(pcm_a));
    assert!(state.is_active_pcm_path(pcm_b));
    assert!(state.suppression_active(t2));

    let mut p_a = pcm(pcm_a, true, Some(77));
    p_a.device_address = addr_a;
    let mut p_b = pcm(pcm_b, true, Some(33));
    p_b.device_address = addr_b;
    let pcms = vec![p_b, p_a];
    assert_eq!(a2dp_volume_for(&pcms, Some(addr_a)), Some(77));
    assert_eq!(a2dp_volume_for(&pcms, Some(addr_b)), Some(33));
}

#[test]
fn active_audio_source_gates_forward_volume_and_rearms_on_switch() {
    use bt_manager::volume_sync::{is_bluetooth_audio_source, parse_active_source_changed};
    use zbus::zvariant::Value;

    assert!(is_bluetooth_audio_source(""));
    assert!(is_bluetooth_audio_source("bluetooth"));
    assert!(!is_bluetooth_audio_source("rmpd"));

    let mut props = HashMap::new();
    props.insert("ActiveSource".to_owned(), Value::from("rmpd"));
    assert_eq!(
        parse_active_source_changed("org.speaker.AudioSources1", &props),
        Some(false)
    );
    assert_eq!(
        parse_active_source_changed("org.speaker.AudioManager1", &props),
        None
    );

    let t0 = Instant::now();
    let mut state = VolumeSyncState::new();
    assert!(state.audio_source_active());
    assert!(state.should_forward_pcm(60, t0));

    // Switching to rmpd disables forward BT volume sync.
    assert!(!state.set_audio_source_active(false, t0));
    assert!(!state.audio_source_active());
    assert!(!state.should_forward_pcm(60, t0));

    // Switching back to bluetooth arms FRESH_PCM_SETTLE and requests reverse reconcile.
    let t1 = t0 + Duration::from_secs(5);
    assert!(state.set_audio_source_active(true, t1));
    assert!(state.audio_source_active());
    assert!(state.suppression_active(t1));
    assert!(!state.should_forward_pcm(60, t1));

    // After settle window expires, forward sync resumes.
    let t2 = t1 + FRESH_PCM_SETTLE + Duration::from_millis(1);
    assert!(state.should_forward_pcm(60, t2));
}
