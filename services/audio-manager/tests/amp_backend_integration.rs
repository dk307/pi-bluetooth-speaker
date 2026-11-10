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
    // `write_state` returns the logical requested state, preserving the stored
    // level even when muted; hardware level clamps to 0.
    assert_eq!(result.level_raw, 80);
    assert_eq!(result.hardware_level(), 0);

    let write = ctrl.last_write().unwrap();
    assert!(write.muted);
    // The stored level is preserved; hardware level is 0 while muted
    assert_eq!(write.level_raw, 80);
    assert_eq!(write.hardware_level(), 0);
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
