//! Integration tests for dbus_api module.

use audio_manager::{AmpState, AudioManagerInterface, MockAmpBackend, MockAmpCtrl, StateFile};
use std::sync::Arc;
use tempfile::tempdir;

fn make_interface() -> (AudioManagerInterface, MockAmpCtrl, tempfile::TempDir) {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    let state_file = StateFile::new(&state_path);
    let state = Arc::new(tokio::sync::Mutex::new(AmpState::default()));
    let iface = AudioManagerInterface::new(state, Arc::new(backend), state_file);
    (iface, ctrl, dir)
}

#[tokio::test]
async fn set_level_unmuted() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState {
        level_raw: 50,
        muted: false,
    });

    iface.set_level_inner(80).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(!write.muted);
    assert_eq!(write.level_raw, speaker_hardware::amp::PERCENT_TO_RAW[80]);
}

#[tokio::test]
async fn set_level_while_muted_applies_level() {
    let (iface, ctrl, _dir) = make_interface();
    // Set the interface to muted state first
    iface.set_mute_inner(true).await.unwrap();

    iface.set_level_inner(80).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // Level applies to the mixer even while muted (serial mute silences
    // output; unmute is instant).
    assert_eq!(write.level_raw, speaker_hardware::amp::PERCENT_TO_RAW[80]);
}

#[tokio::test]
async fn set_mute_on() {
    let (iface, ctrl, _dir) = make_interface();
    iface.set_level_inner(80).await.unwrap();

    iface.set_mute_inner(true).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // Level stays live on the mixer while muted (serial mute silences).
    assert_eq!(write.level_raw, speaker_hardware::amp::PERCENT_TO_RAW[80]);
}

#[tokio::test]
async fn set_mute_off_restores() {
    let (iface, ctrl, _dir) = make_interface();
    // Set the interface to muted state with a known level
    iface.set_level_inner(100).await.unwrap();
    iface.set_mute_inner(true).await.unwrap();

    iface.set_mute_inner(false).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(!write.muted);
    assert_eq!(write.level_raw, speaker_hardware::amp::PERCENT_TO_RAW[100]);
    assert_eq!(
        write.hardware_level(),
        speaker_hardware::amp::PERCENT_TO_RAW[100]
    );
}

#[tokio::test]
async fn set_level_out_of_range() {
    let (iface, _ctrl, _dir) = make_interface();

    let err = iface.set_level_inner(101).await.unwrap_err();
    assert!(err.to_string().contains("out of range"));
}

#[tokio::test]
async fn properties_reflect_state() {
    let (iface, _ctrl, _dir) = make_interface();
    // Use the interface method to set the state, which updates both mock and interface
    iface.set_level_inner(75).await.unwrap();

    let level = iface.level().await;
    let muted = iface.muted().await;

    // level should be close to 75 (inverse table lookup with quantization tolerance)
    assert!((level as i16 - 75).abs() <= 4);
    assert!(!muted);
}

#[tokio::test]
async fn backend_failure_propagates() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_write_fail(true);

    let err = iface.set_level_inner(50).await.unwrap_err();
    assert!(err.to_string().contains("mock write failure"));
}

#[tokio::test]
async fn set_level_multiple_calls() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState::default());

    iface.set_level_inner(25).await.unwrap();
    iface.set_level_inner(50).await.unwrap();
    iface.set_level_inner(75).await.unwrap();
    iface.set_level_inner(100).await.unwrap();

    let writes = ctrl.all_writes();
    assert_eq!(writes.len(), 4);
    assert_eq!(
        writes[0].level_raw,
        speaker_hardware::amp::PERCENT_TO_RAW[25]
    );
    assert_eq!(
        writes[1].level_raw,
        speaker_hardware::amp::PERCENT_TO_RAW[50]
    );
    assert_eq!(
        writes[2].level_raw,
        speaker_hardware::amp::PERCENT_TO_RAW[75]
    );
    assert_eq!(
        writes[3].level_raw,
        speaker_hardware::amp::PERCENT_TO_RAW[100]
    );
}

#[tokio::test]
async fn set_mute_multiple_calls() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState::default());

    iface.set_mute_inner(true).await.unwrap();
    iface.set_mute_inner(false).await.unwrap();
    iface.set_mute_inner(true).await.unwrap();

    let writes = ctrl.all_writes();
    assert_eq!(writes.len(), 3);
    assert!(writes[0].muted);
    assert!(!writes[1].muted);
    assert!(writes[2].muted);
}

#[tokio::test]
async fn set_mute_off_when_already_unmuted() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState {
        level_raw: 100,
        muted: false,
    });

    iface.set_mute_inner(false).await.unwrap();

    let writes = ctrl.all_writes();
    assert_eq!(writes.len(), 1);
    assert!(!writes[0].muted);
}

#[tokio::test]
async fn set_mute_on_when_already_muted() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState {
        level_raw: 50,
        muted: true,
    });

    iface.set_mute_inner(true).await.unwrap();

    let writes = ctrl.all_writes();
    assert_eq!(writes.len(), 1);
    assert!(writes[0].muted);
}

#[tokio::test]
async fn properties_reflect_muted_state() {
    let (iface, _ctrl, _dir) = make_interface();
    // Update the interface's state directly (ctrl.set_state only affects mock backend)
    {
        let mut state = iface.state.lock().await;
        // PERCENT_TO_RAW[10] is the midpoint of the raw flat segment for 10 %.
        *state = AmpState {
            level_raw: speaker_hardware::amp::PERCENT_TO_RAW[10],
            muted: true,
        };
    }

    let muted = iface.muted().await;
    assert!(muted);
    let level = iface.level().await;
    // The midpoint inverse reads the 10 % raw level back as 10 %.
    assert_eq!(level, 10);
}

#[tokio::test]
async fn backend_write_failure_on_set_mute() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_write_fail(true);

    let err = iface.set_mute_inner(true).await.unwrap_err();
    assert!(err.to_string().contains("mock write failure"));
}

#[tokio::test]
async fn backend_write_failure_on_set_mute_off() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_write_fail(true);

    let err = iface.set_mute_inner(false).await.unwrap_err();
    assert!(err.to_string().contains("mock write failure"));
}

#[tokio::test]
async fn state_persists_across_dbus_calls() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState::default());

    iface.set_level_inner(60).await.unwrap();
    iface.set_mute_inner(true).await.unwrap();
    iface.set_mute_inner(false).await.unwrap();
    iface.set_level_inner(80).await.unwrap();

    let final_level = iface.level().await;
    let final_muted = iface.muted().await;

    // The inverse table lookup should find 80
    assert!((final_level as i16 - 80).abs() <= 4);
    assert!(!final_muted);
}
