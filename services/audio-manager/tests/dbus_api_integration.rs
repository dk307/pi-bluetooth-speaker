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

    iface.set_level(80).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(!write.muted);
    assert_eq!(write.level_raw, amp_curve::PERCENT_TO_RAW[80]);
}

#[tokio::test]
async fn set_level_muted_stores_only() {
    let (iface, ctrl, _dir) = make_interface();
    // Set the interface to muted state first
    iface.set_mute(true).await.unwrap();

    iface.set_level(80).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // Logical level is preserved when muted; hardware level clamps to 0
    assert_eq!(write.level_raw, amp_curve::PERCENT_TO_RAW[80]);
    assert_eq!(write.hardware_level(), 0);
    // The call succeeded without changing mute state
}

#[tokio::test]
async fn set_mute_on() {
    let (iface, ctrl, _dir) = make_interface();
    ctrl.set_state(AmpState {
        level_raw: 100,
        muted: false,
    });

    iface.set_mute(true).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // The interface's remembered logical level is preserved; hardware level clamps to 0
    assert_eq!(write.hardware_level(), 0);
}

#[tokio::test]
async fn set_mute_off_restores() {
    let (iface, ctrl, _dir) = make_interface();
    // Set the interface to muted state with a known level
    iface.set_level(100).await.unwrap();
    iface.set_mute(true).await.unwrap();

    iface.set_mute(false).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(!write.muted);
    assert_eq!(write.level_raw, amp_curve::PERCENT_TO_RAW[100]);
    assert_eq!(write.hardware_level(), amp_curve::PERCENT_TO_RAW[100]);
}

#[tokio::test]
async fn set_level_out_of_range() {
    let (iface, _ctrl, _dir) = make_interface();

    let err = iface.set_level(101).await.unwrap_err();
    assert!(err.to_string().contains("out of range"));
}

#[tokio::test]
async fn properties_reflect_state() {
    let (iface, _ctrl, _dir) = make_interface();
    // Use the interface method to set the state, which updates both mock and interface
    iface.set_level(75).await.unwrap();

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

    let err = iface.set_level(50).await.unwrap_err();
    assert!(err.to_string().contains("mock write failure"));
}
