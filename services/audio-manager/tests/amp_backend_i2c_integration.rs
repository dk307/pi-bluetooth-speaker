//! Integration tests for the Phase 2 `I2cAmpBackend` (mocked I2C device).

use audio_manager::amp_backend::{AmpBackend, I2cAmpBackend};
use audio_manager::amp_state::AmpState;
use speaker_hardware::{AmpDeviceMock, AmpErrorFlags, AmpFault};

fn fixture() -> (I2cAmpBackend, AmpDeviceMock) {
    I2cAmpBackend::new_mocked()
}

#[tokio::test]
async fn write_state_encodes_vlp_volume_and_mute() {
    let (backend, mock) = fixture();

    // raw 144 (0 dB) → reg 0x18 (24); raw 0 (−144 dB) → reg 0xA8 (168).
    backend
        .write_state(&AmpState::new(144, false).unwrap())
        .await
        .unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 144);
    assert!(!mock.mock_serial_muted());

    backend
        .write_state(&AmpState::new(0, true).unwrap())
        .await
        .unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 0);
    assert!(mock.mock_serial_muted());
}

#[tokio::test]
async fn read_state_decodes_hardware_encoding() {
    let (backend, mock) = fixture();
    // Default mock volume raw 116 must read back unchanged.
    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 116);
    assert!(!state.muted);

    mock.set_mock_master_volume_raw(72);
    let state = backend.read_state().await.unwrap();
    assert_eq!(state.level_raw, 72);
}

#[tokio::test]
async fn set_and_read_serial_mute_round_trip() {
    let (backend, mock) = fixture();
    backend.set_serial_mute(true).await.unwrap();
    assert!(mock.mock_serial_muted());
    assert!(backend.read_serial_mute().await.unwrap());
    backend.set_serial_mute(false).await.unwrap();
    assert!(!backend.read_serial_mute().await.unwrap());
}

#[tokio::test]
async fn validate_reads_settings() {
    let (backend, _mock) = fixture();
    backend.validate().await.unwrap();
}

#[tokio::test]
async fn backend_id_reports_device() {
    let (backend, _mock) = fixture();
    assert_eq!(backend.backend_id(), "i2c-ma12070p@0x20");
}

#[tokio::test]
async fn hardware_fault_maps_to_canonical_error() {
    let (backend, mock) = fixture();
    mock.set_fault(Some("simulated bus failure".into()));
    let err = backend
        .write_state(&AmpState::new(100, false).unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("simulated bus failure"));
    assert!(matches!(err, audio_manager::AmpError::Hardware(_)));
}

#[tokio::test]
async fn pll_fault_flags_observable_through_device() {
    let (backend, mock) = fixture();
    mock.set_live_errors(AmpErrorFlags::from_bits(1 << AmpFault::PllClockLoss.bit()));
    let flags = backend.device().read_live_errors().unwrap();
    assert!(flags.has_fault(AmpFault::PllClockLoss));
}
