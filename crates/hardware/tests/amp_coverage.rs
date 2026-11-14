//! Comprehensive integration tests for the MA12070P amp driver (`amp.rs`)
//! covering register operations, settings, faults, mocks, and error paths.

use speaker_hardware::amp::{
    AmpDevice, AmpError, AmpErrorFlags, AmpFault, AmpSettings, AudioInputMode, DcuMonitor,
    I2sFormat, Pm2Config, PowerModeCtrl, PowerModeMon, PowerModeProfile,
};
use std::sync::Arc;

#[test]
fn amp_device_mock_new_mocked_returns_pair() {
    let (device, _mock) = AmpDevice::new_mocked();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.master_volume_raw, 116);
    assert!(!settings.serial_muted);
    assert_eq!(settings.pm_ctrl_pm.as_u8(), 0);
    assert_eq!(settings.pm_profile.as_u8(), 0);
    assert_eq!(settings.i2s_format.as_u8(), 1);
    assert_eq!(settings.audio_in_mode.as_u8(), 0);
}

#[test]
fn amp_device_mock_write_master_volume() {
    let (device, mock) = AmpDevice::new_mocked();
    device.write_master_volume(100).unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 100);
    assert!(!mock.mock_serial_muted());
    device.write_master_volume(0).unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 0);
}

#[test]
fn amp_device_mock_write_serial_mute() {
    let (device, mock) = AmpDevice::new_mocked();
    device.write_serial_mute(true).unwrap();
    assert!(mock.mock_serial_muted());
    device.write_serial_mute(false).unwrap();
    assert!(!mock.mock_serial_muted());
}

#[test]
fn amp_device_mock_read_settings_returns_current_state() {
    let (device, mock) = AmpDevice::new_mocked();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.master_volume_raw, 116);
    assert!(!settings.serial_muted);
    assert_eq!(settings.pm_ctrl_pm.as_u8(), 0);
    assert_eq!(settings.pm_profile.as_u8(), 0);
    assert_eq!(settings.i2s_format.as_u8(), 1);
    assert_eq!(settings.audio_in_mode.as_u8(), 0);
    mock.set_mock_master_volume_raw(72);
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.master_volume_raw, 72);
}

#[test]
fn amp_device_mock_write_limiter_thresholds() {
    let (device, mock) = AmpDevice::new_mocked();
    device.write_limiter_thresholds(-20).unwrap();
    assert_eq!(mock.mock_limiter_thresholds(), -20);
    device.write_limiter_thresholds(-15).unwrap();
    assert_eq!(mock.mock_limiter_thresholds(), -15);
}

#[test]
fn amp_device_mock_write_power_mode_ctrl() {
    let (device, _mock) = AmpDevice::new_mocked();
    device
        .write_power_mode_ctrl(PowerModeCtrl::from_field(1))
        .unwrap();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.pm_ctrl_pm.as_u8(), 1);
}

#[test]
fn amp_device_mock_write_power_mode_profile() {
    let (device, _mock) = AmpDevice::new_mocked();
    device
        .write_power_mode_profile(PowerModeProfile::from_raw(0))
        .unwrap();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.pm_profile.as_u8(), 0);
}

#[test]
fn amp_device_mock_write_i2s_format() {
    let (device, _mock) = AmpDevice::new_mocked();
    device.write_i2s_format(I2sFormat::from_raw(0)).unwrap();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.i2s_format.as_u8(), 0);
}

#[test]
fn amp_device_mock_write_audio_in_mode() {
    let (device, _mock) = AmpDevice::new_mocked();
    device
        .write_audio_in_mode(AudioInputMode::from_raw(0))
        .unwrap();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.audio_in_mode.as_u8(), 0);
}

#[test]
fn amp_device_mock_write_serial_mute_rmw() {
    let (device, mock) = AmpDevice::new_mocked();
    device.write_serial_mute(true).unwrap();
    assert!(mock.mock_serial_muted());
    device.write_master_volume(100).unwrap();
    assert!(mock.mock_serial_muted());
    assert_eq!(mock.mock_master_volume_raw(), 100);
}

#[test]
fn amp_device_mock_fault_injection() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_fault(Some("simulated I2C failure".into()));
    let err = device.write_master_volume(100).unwrap_err();
    assert!(err.is_mock_fault());
    assert!(err.to_string().contains("simulated I2C failure"));
}

#[test]
fn amp_device_mock_clear_fault_recovers() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_fault(Some("simulated I2C failure".into()));
    let _ = device.write_master_volume(100).unwrap_err();
    mock.set_fault(None);
    device.write_master_volume(100).unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 100);
}

#[test]
fn amp_device_mock_set_live_errors() {
    let (device, mock) = AmpDevice::new_mocked();
    let flags = AmpErrorFlags::from_bits(1 << AmpFault::PllClockLoss.bit());
    mock.set_live_errors(flags);
    let read = device.read_live_errors().unwrap();
    assert!(read.has_fault(AmpFault::PllClockLoss));
}

#[test]
fn amp_device_mock_set_latched_errors() {
    let (device, mock) = AmpDevice::new_mocked();
    let flags = AmpErrorFlags::from_bits(1 << AmpFault::OvercurrentProtect.bit());
    mock.set_latched_errors(flags);
    let read = device.read_latched_errors().unwrap();
    assert!(read.has_fault(AmpFault::OvercurrentProtect));
}

#[test]
fn amp_device_error_from_i2c() {
    let i2c_err = rppal::i2c::Error::InvalidSlaveAddress(0x99);
    let amp_err: AmpError = i2c_err.into();
    assert!(amp_err.is_i2c());
    // Verify the slave address is preserved in the error chain
    let display = amp_err.to_string();
    eprintln!("Error display: {display}");
    assert!(display.contains("99") || display.contains("0x99") || display.contains("153"));
}

#[test]
fn amp_device_error_summary() {
    let (device, _mock) = AmpDevice::new_mocked();
    let inner = device.write_master_volume(999).unwrap_err();
    let summary = inner.summary();
    assert!(!summary.contains('\n'));
    assert!(!summary.contains("Backtrace"));
}

#[test]
fn amp_error_flags_operations() {
    let flags = AmpErrorFlags::NONE;
    assert!(!flags.has_fault(AmpFault::PllClockLoss));
    let flags = AmpErrorFlags::from_bits(1 << AmpFault::PllClockLoss.bit());
    assert!(flags.has_fault(AmpFault::PllClockLoss));
    assert!(!flags.has_fault(AmpFault::OvercurrentProtect));
    let combined = AmpErrorFlags::from_bits(
        (1 << AmpFault::PllClockLoss.bit()) | (1 << AmpFault::HighTempWarning.bit()),
    );
    assert!(combined.has_fault(AmpFault::PllClockLoss));
    assert!(combined.has_fault(AmpFault::HighTempWarning));
    assert!(!combined.has_fault(AmpFault::OvercurrentProtect));
}

#[test]
fn amp_settings_debug_and_display() {
    let (device, _mock) = AmpDevice::new_mocked();
    let settings = device.read_settings().unwrap();
    let debug_str = format!("{settings:?}");
    assert!(debug_str.contains("AmpSettings"));
    assert!(debug_str.contains("master_volume_raw"));
}

#[tokio::test]
async fn amp_device_async_write_master_volume() {
    let (device, mock) = AmpDevice::new_mocked();
    device.write_master_volume_async(120).await.unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 120);
}

#[tokio::test]
async fn amp_device_async_write_serial_mute() {
    let (device, mock) = AmpDevice::new_mocked();
    device.write_serial_mute_async(true).await.unwrap();
    assert!(mock.mock_serial_muted());
}

#[tokio::test]
async fn amp_device_async_read_settings() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_mock_master_volume_raw(80);
    let settings = device.read_settings_async().await.unwrap();
    assert_eq!(settings.master_volume_raw, 80);
}

#[test]
fn amp_device_with_bus_and_address() {
    let result = AmpDevice::with_bus_and_address(99, speaker_hardware::AmpI2cAddress::default());
    let err = result.unwrap_err();
    eprintln!("Error: {err:?}");
    assert!(err.is_unsupported_bus() || err.is_invalid_address());
}

#[test]
fn amp_device_arc_clone_works() {
    let (device, _mock) = AmpDevice::new_mocked();
    let device = Arc::new(device);
    let cloned = Arc::clone(&device);
    let settings = cloned.read_settings().unwrap();
    assert_eq!(settings.master_volume_raw, 116);
}

#[test]
fn amp_device_i2c_bus_and_address() {
    let (device, _mock) = AmpDevice::new_mocked();
    assert_eq!(device.i2c_bus(), 1);
    assert_eq!(device.i2c_address().as_u8(), 0x20);
}

#[test]
fn amp_device_hardware_info() {
    let (device, _mock) = AmpDevice::new_mocked();
    let info = device.hardware_info();
    assert_eq!(info.bus, 1);
    assert_eq!(info.address.as_u8(), 0x20);
}

#[test]
fn amp_device_read_status() {
    let (device, _mock) = AmpDevice::new_mocked();
    let status = device.read_status().unwrap();
    assert_eq!(status.pm_mon, PowerModeMon::Pm2);
    assert_eq!(status.pm2_config.baltype, 1);
    assert!(status.dcu_monitor.ch0.is_active());
    assert!(!status.latched_errors.has_fault(AmpFault::PllClockLoss));
}

#[test]
fn amp_read_status_after_writes() {
    let (device, _mock) = AmpDevice::new_mocked();
    device.write_master_volume(80).unwrap();
    device.write_serial_mute(true).unwrap();
    let status = device.read_status().unwrap();
    assert_eq!(status.pm_mon, PowerModeMon::Pm2);
}

#[test]
fn amp_settings_display() {
    let (device, _mock) = AmpDevice::new_mocked();
    let settings = device.read_settings().unwrap();
    let display = format!("{settings}");
    assert!(display.contains("Master Volume"));
    assert!(display.contains("raw"));
    assert!(display.contains("frac"));
}

#[test]
fn amp_settings_from_raw_registers() {
    let regs = [0x30, 0x00, 0x00, 0x01, 0x01, 0x34, 0x00];
    let settings = AmpSettings::from_raw_registers(regs);
    assert_eq!(settings.master_volume_raw, 116);
    assert!(!settings.serial_muted);
    assert_eq!(settings.pm_ctrl_pm.as_u8(), 0);
    assert_eq!(settings.pm_profile.as_u8(), 0);
    assert_eq!(settings.audio_in_mode.as_u8(), 0);
    assert_eq!(settings.i2s_format.as_u8(), 1);
}

#[test]
fn amp_mock_state_debug() {
    let (_device, mock) = AmpDevice::new_mocked();
    let debug_str = format!("{mock:?}");
    assert!(debug_str.contains("AmpDeviceMock"));
}

#[test]
fn amp_device_mock_transient_write_failures() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_transient_write_failures(2);
    let err = device.write_master_volume(50).unwrap_err();
    assert!(err.is_mock_fault());
    let err = device.write_master_volume(50).unwrap_err();
    assert!(err.is_mock_fault());
    device.write_master_volume(50).unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 50);
}

#[test]
fn amp_device_mock_set_pm_mon() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_pm_mon(PowerModeMon::Pm1);
    let status = device.read_status().unwrap();
    assert_eq!(status.pm_mon, PowerModeMon::Pm1);
}

#[test]
fn amp_device_mock_set_pm2_config() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_pm2_config(Pm2Config::from_raw(0x41));
    let status = device.read_status().unwrap();
    assert!(status.pm2_config.usespread);
    assert_eq!(status.pm2_config.baltype, 1);
}

#[test]
fn amp_device_mock_set_dcu_monitor() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_dcu_monitor(DcuMonitor::from_raw(0x10, 0x00));
    let status = device.read_status().unwrap();
    assert!(status.dcu_monitor.ch0.is_active());
    assert!(!status.dcu_monitor.ch1.is_active());
}

#[test]
fn amp_device_mock_set_live_errors_changes_status() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_live_errors(AmpErrorFlags::from_bits(1 << AmpFault::PllClockLoss.bit()));
    let status = device.read_status().unwrap();
    assert!(status.live_errors.has_fault(AmpFault::PllClockLoss));
}

#[test]
fn amp_device_mock_set_latched_errors_changes_status() {
    let (device, mock) = AmpDevice::new_mocked();
    mock.set_latched_errors(AmpErrorFlags::from_bits(
        1 << AmpFault::OvercurrentProtect.bit(),
    ));
    let status = device.read_status().unwrap();
    assert!(
        status
            .latched_errors
            .has_fault(AmpFault::OvercurrentProtect)
    );
}

#[test]
fn amp_settings_audio_in_mode_as_u8() {
    let (device, _mock) = AmpDevice::new_mocked();
    device
        .write_audio_in_mode(AudioInputMode::from_raw(0x40))
        .unwrap();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.audio_in_mode.as_u8(), 2);
}

#[test]
fn amp_settings_i2s_format_as_u8() {
    let (device, _mock) = AmpDevice::new_mocked();
    device.write_i2s_format(I2sFormat::from_raw(4)).unwrap();
    let settings = device.read_settings().unwrap();
    assert_eq!(settings.i2s_format.as_u8(), 4);
}
