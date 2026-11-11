//! Integration tests for amp_backend module.

use audio_manager::{AmpBackend, AmpState, MockAmpBackend};

#[tokio::test]
async fn mock_backend_write_state() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_state(AmpState {
        level_raw: 50,
        muted: false,
    });

    let result = backend
        .write_state(&AmpState {
            level_raw: 80,
            muted: false,
        })
        .await
        .unwrap();
    assert!(!result.muted);
    assert_eq!(result.level_raw, 80);

    let write = ctrl.last_write().unwrap();
    assert!(!write.muted);
    assert_eq!(write.level_raw, 80);
}

#[tokio::test]
async fn mock_backend_write_state_muted() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_state(AmpState {
        level_raw: 50,
        muted: true,
    });

    let result = backend
        .write_state(&AmpState {
            level_raw: 80,
            muted: true,
        })
        .await
        .unwrap();
    assert!(result.muted);
    // `write_state` returns the logical requested state; the level stays
    // live on the mixer even while muted (serial mute silences output).
    assert_eq!(result.level_raw, 80);
    assert_eq!(result.hardware_level(), 80);

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    assert_eq!(write.level_raw, 80);
}

#[tokio::test]
async fn mock_backend_read_state() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_state(AmpState {
        level_raw: 100,
        muted: false,
    });

    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 100);
    assert!(!state.muted);
}

#[tokio::test]
async fn mock_backend_validate() {
    let (backend, _ctrl) = MockAmpBackend::new_mocked();

    let result = backend.validate().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn mock_backend_validate_fail() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_validate_fail(true);

    let result = backend.validate().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn mock_backend_write_fail() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_write_fail(true);

    let result = backend
        .write_state(&AmpState {
            level_raw: 50,
            muted: false,
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn mock_backend_read_fail() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_read_fail(true);

    let result = backend.read_state().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn mock_ctrl_call_counts() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_state(AmpState {
        level_raw: 50,
        muted: false,
    });

    let _ = backend.validate().await;
    let _ = backend
        .write_state(&AmpState {
            level_raw: 80,
            muted: false,
        })
        .await;
    let _ = backend.read_state().await;

    let (v, w, r) = ctrl.call_counts();
    assert_eq!(v, 1);
    assert_eq!(w, 1);
    assert_eq!(r, 1);
}

#[tokio::test]
async fn mock_serial_mute_write_and_read() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    backend.set_serial_mute(true).await.unwrap();
    assert_eq!(ctrl.serial_mute_calls(), 1);
    assert_eq!(ctrl.last_serial_mute(), Some(true));
    assert!(backend.read_serial_mute().await.unwrap());

    backend.set_serial_mute(false).await.unwrap();
    assert_eq!(ctrl.serial_mute_calls(), 2);
    assert!(!backend.read_serial_mute().await.unwrap());
}

#[tokio::test]
async fn mock_serial_mute_fault_injection() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    ctrl.set_serial_mute_fail(true);
    let err = backend.set_serial_mute(true).await.unwrap_err();
    assert!(err.to_string().contains("mock serial mute failure"));
    // One-shot failure: next call succeeds.
    backend.set_serial_mute(false).await.unwrap();
}

#[tokio::test]
async fn mock_write_state_applies_serial_mute() {
    // Mock fidelity: the real backend's write_state applies the serial mute
    // bit after the mixer write; the mock mirrors that coupling.
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    backend
        .write_state(&AmpState {
            level_raw: 80,
            muted: true,
        })
        .await
        .unwrap();
    assert!(ctrl.last_write().unwrap().muted);
    assert_eq!(ctrl.last_serial_mute(), Some(true));
    assert!(backend.read_serial_mute().await.unwrap());

    backend
        .write_state(&AmpState {
            level_raw: 80,
            muted: false,
        })
        .await
        .unwrap();
    assert_eq!(ctrl.last_serial_mute(), Some(false));
    assert!(!backend.read_serial_mute().await.unwrap());
}
