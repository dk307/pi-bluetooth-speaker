//! Additional integration tests for dbus_api module to increase coverage.

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

fn make_interface_with_commands() -> (AudioManagerInterface, tokio::sync::mpsc::Sender<audio_manager::amp_controller::AmpCommand>, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    let state_file = StateFile::new(&state_path);
    let state = Arc::new(tokio::sync::Mutex::new(AmpState::default()));
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(32);
    let iface = AudioManagerInterface::with_commands(state, state_file, cmd_tx.clone());
    (iface, cmd_tx, dir)
}

#[tokio::test]
async fn dbus_api_new_constructor() {
    let (_iface, _ctrl, _dir) = make_interface();
    // Just verifies construction works
}

#[tokio::test]
async fn dbus_api_with_commands_constructor() {
    let (_iface, _cmd_tx, _dir) = make_interface_with_commands();
    // Just verifies construction works
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
async fn set_level_while_muted_applies_level() {
    let (iface, ctrl, _dir) = make_interface();
    // Set the interface to muted state first
    iface.set_mute(true).await.unwrap();

    iface.set_level(80).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // Level applies to the mixer even while muted (serial mute silences
    // output; unmute is instant).
    assert_eq!(write.level_raw, amp_curve::PERCENT_TO_RAW[80]);
}

#[tokio::test]
async fn set_mute_on() {
    let (iface, ctrl, _dir) = make_interface();
    iface.set_level(80).await.unwrap();

    iface.set_mute(true).await.unwrap();

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // Level stays live on the mixer while muted (serial mute silences).
    assert_eq!(write.level_raw, amp_curve::PERCENT_TO_RAW[80]);
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

#[tokio::test]
async fn set_level_i2c_mode_dispatches() {
    let (iface, _cmd_tx, _dir) = make_interface_with_commands();
    // In i2c mode, set_level should dispatch to the command channel
    let result = iface.set_level(50).await;
    // The command channel is open but nothing is listening, so it will fail
    // with "controller command channel closed"
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("controller command channel closed"));
}

#[tokio::test]
async fn set_mute_i2c_mode_dispatches() {
    let (iface, _cmd_tx, _dir) = make_interface_with_commands();
    // In i2c mode, set_mute should dispatch to the command channel
    let result = iface.set_mute(true).await;
    // The command channel is open but nothing is listening
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("controller command channel closed"));
}

#[tokio::test]
async fn level_property_returns_percent() {
    let (iface, _ctrl, _dir) = make_interface();
    // Use the interface method to set the state, which updates the interface's state
    iface.set_level(60).await.unwrap();

    let level = iface.level().await;
    // level_percent does inverse lookup
    assert!((level as i16 - 60).abs() <= 4);
}

#[tokio::test]
async fn muted_property_returns_state() {
    let (iface, _ctrl, _dir) = make_interface();
    // Use the interface method to set mute state
    iface.set_mute(true).await.unwrap();

    let muted = iface.muted().await;
    assert!(muted);

    iface.set_mute(false).await.unwrap();

    let muted = iface.muted().await;
    assert!(!muted);
}

#[tokio::test]
async fn interface_creation_modes() {
    // Test both constructor modes by observing behavior
    // Non-i2c mode: has backend, no commands
    let (iface1, _ctrl1, _dir1) = make_interface();
    // We can verify by checking that set_level works (uses backend)
    let result1 = iface1.set_level(50).await;
    assert!(result1.is_ok());

    // i2c mode: no backend, has commands
    let (iface2, _cmd_tx, _dir2) = make_interface_with_commands();
    // In i2c mode, set_level dispatches to command channel
    let result2 = iface2.set_level(50).await;
    assert!(result2.is_err());
    assert!(result2.unwrap_err().to_string().contains("controller command channel closed"));
}