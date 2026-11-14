//! Additional integration tests for amp_backend module to increase coverage.

use audio_manager::AmpState;
use audio_manager::amp_backend::{AmpBackend, I2cAmpBackend, MockAmpBackend};

#[tokio::test]
async fn mock_ctrl_sticky_serial_mute_fail() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    // Set sticky failure mode
    ctrl.set_serial_mute_fail_sticky(true);

    // First call fails
    let err = backend.set_serial_mute(true).await.unwrap_err();
    assert!(err.to_string().contains("mock serial mute failure"));

    // Second call also fails (sticky)
    let err = backend.set_serial_mute(false).await.unwrap_err();
    assert!(err.to_string().contains("mock serial mute failure"));

    // Clear sticky and it succeeds
    ctrl.set_serial_mute_fail_sticky(false);
    backend.set_serial_mute(true).await.unwrap();
    assert!(backend.read_serial_mute().await.unwrap());
}

#[tokio::test]
async fn mock_ctrl_set_state_and_read() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    // Initial state (50% = raw 114)
    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 114); // default
    assert!(!state.muted);

    // Set new state
    ctrl.set_state(AmpState::new(75, true).unwrap());
    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 75);
    assert!(state.muted);
}

#[tokio::test]
async fn mock_backend_last_write_tracks_latest() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    backend
        .write_state(&AmpState::new(10, false).unwrap())
        .await
        .unwrap();
    assert_eq!(ctrl.last_write().map(|s| s.level_raw), Some(10));

    backend
        .write_state(&AmpState::new(20, true).unwrap())
        .await
        .unwrap();
    assert_eq!(ctrl.last_write().map(|s| s.level_raw), Some(20));
    assert_eq!(ctrl.last_write().map(|s| s.muted), Some(true));
}

#[tokio::test]
async fn mock_backend_serial_mute_calls_increment() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    assert_eq!(ctrl.serial_mute_calls(), 0);

    backend.set_serial_mute(true).await.unwrap();
    assert_eq!(ctrl.serial_mute_calls(), 1);

    backend.set_serial_mute(false).await.unwrap();
    assert_eq!(ctrl.serial_mute_calls(), 2);

    backend.set_serial_mute(true).await.unwrap();
    assert_eq!(ctrl.serial_mute_calls(), 3);
}

#[tokio::test]
async fn mock_backend_validate_called_count() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    assert_eq!(ctrl.call_counts().0, 0);

    let _ = backend.validate().await;
    assert_eq!(ctrl.call_counts().0, 1);

    let _ = backend.validate().await;
    assert_eq!(ctrl.call_counts().0, 2);
}

#[tokio::test]
async fn mock_backend_write_and_read_called_counts() {
    let (backend, ctrl) = MockAmpBackend::new_mocked();

    let (v, w, r) = ctrl.call_counts();
    assert_eq!(v, 0);
    assert_eq!(w, 0);
    assert_eq!(r, 0);

    let _ = backend
        .write_state(&AmpState::new(50, false).unwrap())
        .await;
    let (v, w, r) = ctrl.call_counts();
    assert_eq!(v, 0);
    assert_eq!(w, 1);
    assert_eq!(r, 0);

    let _ = backend.read_state().await;
    let (v, w, r) = ctrl.call_counts();
    assert_eq!(v, 0);
    assert_eq!(w, 1);
    assert_eq!(r, 1);
}

#[tokio::test]
async fn i2c_amp_backend_new_mocked() {
    let (backend, _mock) = I2cAmpBackend::new_mocked();

    assert_eq!(backend.backend_id(), "i2c-ma12070p@0x20");

    // Verify we can access the underlying device
    let device = backend.device();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.master_volume_raw, 116); // default
    assert!(!settings.serial_muted);
}

#[tokio::test]
async fn i2c_amp_backend_write_state_unmuted() {
    let (backend, mock) = I2cAmpBackend::new_mocked();

    let result = backend
        .write_state(&AmpState::new(144, false).unwrap())
        .await
        .unwrap();

    assert!(!result.muted);
    assert_eq!(result.level_raw, 144);
    assert_eq!(mock.mock_master_volume_raw(), 144);
    assert!(!mock.mock_serial_muted());
}

#[tokio::test]
async fn i2c_amp_backend_write_state_muted() {
    let (backend, mock) = I2cAmpBackend::new_mocked();

    let result = backend
        .write_state(&AmpState::new(0, true).unwrap())
        .await
        .unwrap();

    assert!(result.muted);
    assert_eq!(result.level_raw, 0);
    assert_eq!(mock.mock_master_volume_raw(), 0);
    assert!(mock.mock_serial_muted());
}

#[tokio::test]
async fn i2c_amp_backend_read_state() {
    let (backend, mock) = I2cAmpBackend::new_mocked();

    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 116);
    assert!(!state.muted);

    mock.set_mock_master_volume_raw(72);
    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 72);
}

#[tokio::test]
async fn i2c_amp_backend_set_read_serial_mute() {
    let (backend, mock) = I2cAmpBackend::new_mocked();

    backend.set_serial_mute(true).await.unwrap();
    assert!(mock.mock_serial_muted());
    assert!(backend.read_serial_mute().await.unwrap());

    backend.set_serial_mute(false).await.unwrap();
    assert!(!backend.read_serial_mute().await.unwrap());
}

#[tokio::test]
async fn i2c_amp_backend_validate() {
    let (backend, _mock) = I2cAmpBackend::new_mocked();
    backend.validate().await.unwrap();
}

#[tokio::test]
async fn i2c_amp_backend_hardware_fault_maps_to_canonical_error() {
    let (backend, mock) = I2cAmpBackend::new_mocked();
    mock.set_fault(Some("simulated bus failure".into()));

    let err = backend
        .write_state(&AmpState::new(100, false).unwrap())
        .await
        .unwrap_err();

    assert!(err.to_string().contains("simulated bus failure"));
    assert!(matches!(err, audio_manager::AmpError::Hardware(_)));
}

#[tokio::test]
async fn i2c_amp_backend_pll_fault_flags_observable() {
    use speaker_hardware::{AmpErrorFlags, AmpFault};

    let (backend, mock) = I2cAmpBackend::new_mocked();
    mock.set_live_errors(AmpErrorFlags::from_bits(1 << AmpFault::PllClockLoss.bit()));

    let flags = backend.device().read_live_errors().unwrap();
    assert!(flags.has_fault(AmpFault::PllClockLoss));
}

#[test]
fn amp_state_new_constructor() {
    // Test the AmpState::new constructor
    let state = AmpState::new(100, false).unwrap();
    assert_eq!(state.level_raw, 100);
    assert!(!state.muted);

    let state = AmpState::new(0, true).unwrap();
    assert_eq!(state.level_raw, 0);
    assert!(state.muted);
}

#[test]
fn amp_state_new_invalid_raw() {
    // Raw must be <= 144
    let result = AmpState::new(145, false);
    assert!(result.is_err());

    let result = AmpState::new(200, false);
    assert!(result.is_err());
}

#[test]
fn amp_state_hardware_level() {
    // hardware_level returns the raw level regardless of mute
    let state = AmpState {
        level_raw: 100,
        muted: true,
    };
    assert_eq!(state.hardware_level(), 100);

    let state = AmpState {
        level_raw: 50,
        muted: false,
    };
    assert_eq!(state.hardware_level(), 50);
}

#[test]
fn amp_state_mute_unmute_methods() {
    let state = AmpState {
        level_raw: 100,
        muted: false,
    }
    .mute();
    assert!(state.muted);
    assert_eq!(state.level_raw, 100);

    let state = AmpState {
        level_raw: 100,
        muted: true,
    }
    .unmute();
    assert!(!state.muted);
    assert_eq!(state.level_raw, 100);
}

#[test]
fn amp_state_with_percent() {
    use speaker_hardware::PERCENT_TO_RAW;

    // When muted, only stores the level
    let state = AmpState {
        level_raw: 50,
        muted: true,
    }
    .with_percent(80, &PERCENT_TO_RAW);
    assert!(state.muted);
    assert_eq!(state.level_raw, PERCENT_TO_RAW[80]);

    // When unmuted, applies the level
    let state = AmpState {
        level_raw: 50,
        muted: false,
    }
    .with_percent(80, &PERCENT_TO_RAW);
    assert!(!state.muted);
    assert_eq!(state.level_raw, PERCENT_TO_RAW[80]);
}

#[test]
fn amp_state_level_percent_inverse() {
    use speaker_hardware::PERCENT_TO_RAW;

    for (p, &raw) in PERCENT_TO_RAW.iter().enumerate() {
        let state = AmpState {
            level_raw: raw,
            muted: false,
        };
        let back = state.level_percent(&PERCENT_TO_RAW);
        let diff = (back as i16 - p as i16).abs();
        assert!(
            diff <= 4,
            "percent {} -> raw {} -> back {}",
            p,
            PERCENT_TO_RAW[p],
            back
        );
    }
}

#[test]
fn amp_state_level_percent_muted() {
    use speaker_hardware::PERCENT_TO_RAW;

    let state = AmpState {
        level_raw: 100,
        muted: true,
    };
    let p = state.level_percent(&PERCENT_TO_RAW);
    assert!(p > 0);
}

#[test]
fn amp_state_hardware_level_percent_muted() {
    use speaker_hardware::amp::PERCENT_TO_RAW;

    // Mute no longer clamps the hardware level; it reads back as the level.
    let state = AmpState {
        level_raw: 100,
        muted: true,
    };
    let p = state.hardware_level_percent(&PERCENT_TO_RAW);
    // With the new linear calibration (1% = -60 dB, 100% = 0 dB), raw 100
    // maps exactly to percent 27 and 28. The midpoint inverse returns the
    // ceiling of the average: (27 + 28).div_ceil(2) = 28.
    assert_eq!(p, 28);
}
