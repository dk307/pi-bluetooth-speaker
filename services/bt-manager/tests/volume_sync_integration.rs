//! Integration tests for the volume-sync state machine and D-Bus parsers.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bt_manager::volume_sync::{
    ECHO_CAPACITY, FRESH_PCM_SETTLE, VolumeSyncState, a2dp_volume, interfaces_added_path,
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
