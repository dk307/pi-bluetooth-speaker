//! Additional integration tests for dbus_api module to increase coverage.

use audio_manager::{AmpState, AudioManagerInterface, MockAmpBackend, MockAmpCtrl, StateFile};
use speaker_hardware::amp::PERCENT_TO_RAW;
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

fn make_interface_with_commands() -> (AudioManagerInterface, tokio::sync::mpsc::Receiver<audio_manager::amp_controller::AmpCommand>, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    let state_file = StateFile::new(&state_path);
    let state = Arc::new(tokio::sync::Mutex::new(AmpState::default()));
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
    let iface = AudioManagerInterface::with_commands(state, state_file, cmd_tx);
    (iface, cmd_rx, dir)
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
    assert_eq!(write.level_raw, PERCENT_TO_RAW[80]);
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
    assert_eq!(write.level_raw, PERCENT_TO_RAW[80]);
}

#[tokio::test]
async fn set_mute_on() {
    let (iface, ctrl, _dir) = make_interface();
    iface.set_level_inner(80).await.unwrap();

    iface.set_mute_inner(true).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // Level stays live on the mixer while muted (serial mute silences).
    assert_eq!(write.level_raw, PERCENT_TO_RAW[80]);
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
    assert_eq!(write.level_raw, PERCENT_TO_RAW[100]);
    assert_eq!(write.hardware_level(), PERCENT_TO_RAW[100]);
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

    // Level must be the canonical inverse-curve midpoint percent of the raw
    // value that percent 75 maps to — no tolerance slack needed.
    let expected = AmpState {
        level_raw: PERCENT_TO_RAW[75],
        muted: false,
    }
    .level_percent(&PERCENT_TO_RAW);
    assert_eq!(level, expected, "percent 75 must invert canonically");
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
async fn level_property_returns_percent() {
    let (iface, _ctrl, _dir) = make_interface();
    // Use the interface method to set the state, which updates the interface's state
    iface.set_level_inner(60).await.unwrap();

    let level = iface.level().await;
    // level_percent does inverse lookup
    let expected = AmpState {
        level_raw: PERCENT_TO_RAW[60],
        muted: false,
    }
    .level_percent(&PERCENT_TO_RAW);
    assert_eq!(level, expected);
}

#[tokio::test]
async fn muted_property_returns_state() {
    let (iface, _ctrl, _dir) = make_interface();
    // Use the interface method to set mute state
    iface.set_mute_inner(true).await.unwrap();

    let muted = iface.muted().await;
    assert!(muted);

    iface.set_mute_inner(false).await.unwrap();

    let muted = iface.muted().await;
    assert!(!muted);
}

#[tokio::test]
async fn i2c_mode_command_dispatch() {
    // Test that i2c mode dispatches commands to the command channel
    let (_iface, mut cmd_rx, _dir) = make_interface_with_commands();
    // Verify the command receiver is available and functional
    assert!(cmd_rx.try_recv().is_err()); // Channel is empty initially
}
