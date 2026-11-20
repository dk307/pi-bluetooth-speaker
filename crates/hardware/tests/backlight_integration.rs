//! Integration tests for the sysfs backlight driver and its mock controller.
//!
//! Uses a hermetic synthetic sysfs tree for filesystem paths and the paired
//! mock backend for fault injection, call counters, and clamp verification.

use std::error::Error as _;
use std::fs;
use std::io;

use speaker_common::diag::ScopedTempDir;
use speaker_hardware::{BACKLIGHT_SYSFS_BASE, Backlight, BacklightError, DEFAULT_BACKLIGHT_DEVICE};

fn make_sysfs_tree(prefix: &str, device: &str, max: &str, raw: &str) -> ScopedTempDir {
    let dir = ScopedTempDir::new(prefix);
    let device_dir = dir.path().join(device);
    fs::create_dir_all(&device_dir).expect("create synthetic backlight device dir");
    fs::write(device_dir.join("max_brightness"), max).expect("write max_brightness");
    fs::write(device_dir.join("brightness"), raw).expect("write brightness");
    dir
}

#[test]
fn constants_match_appliance_panel() {
    assert_eq!(BACKLIGHT_SYSFS_BASE, "/sys/class/backlight");
    assert_eq!(DEFAULT_BACKLIGHT_DEVICE, "rpi_backlight");
}

#[test]
fn sysfs_read_write_round_trip_and_max_clamp() {
    let dir = make_sysfs_tree("backlight_round_trip", "rpi_backlight", "255\n", "100\n");
    let backlight = Backlight::with_root(dir.path(), "rpi_backlight").unwrap();

    assert_eq!(backlight.max_brightness(), 255);
    assert_eq!(backlight.read().unwrap(), 100);

    backlight.write(300).unwrap();
    assert_eq!(backlight.read().unwrap(), 255);
    let written = fs::read_to_string(dir.path().join("rpi_backlight/brightness")).unwrap();
    assert_eq!(written.trim(), "255");

    backlight.write(0).unwrap();
    assert_eq!(backlight.read().unwrap(), 0);
    backlight.write(128).unwrap();
    assert_eq!(backlight.read().unwrap(), 128);
}

#[test]
fn malformed_max_and_brightness_are_invalid_data() {
    let dir = make_sysfs_tree(
        "backlight_bad_max",
        "rpi_backlight",
        "not-a-number\n",
        "0\n",
    );
    let err = Backlight::with_root(dir.path(), "rpi_backlight").unwrap_err();
    assert!(err.is_invalid_data(), "expected invalid data, got: {err}");
    assert!(err.summary().contains("invalid max_brightness"));
    assert!(!err.summary().contains("Backtrace"));

    let dir = make_sysfs_tree("backlight_zero_max", "rpi_backlight", "0\n", "0\n");
    let err = Backlight::with_root(dir.path(), "rpi_backlight").unwrap_err();
    assert!(err.is_invalid_data(), "expected invalid data for zero max");

    let dir = make_sysfs_tree("backlight_bad_raw", "rpi_backlight", "255\n", "garbage\n");
    let backlight = Backlight::with_root(dir.path(), "rpi_backlight").unwrap();
    let err = backlight.read().unwrap_err();
    assert!(err.is_invalid_data(), "expected invalid data, got: {err}");
    assert!(err.summary().contains("invalid brightness"));
}

#[test]
fn missing_device_maps_to_not_found_and_preserves_source() {
    let dir = ScopedTempDir::new("backlight_missing");
    let err = Backlight::with_root(dir.path(), "not_installed").unwrap_err();
    assert!(err.is_not_found(), "expected NotFound, got: {err}");
    assert!(!err.is_permission_denied());
    assert!(!err.is_invalid_data());
    assert!(!err.is_mock_fault());

    let source = err.source().expect("io error source must be preserved");
    let io_err = source
        .downcast_ref::<io::Error>()
        .expect("source must be io::Error");
    assert_eq!(io_err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn permission_denied_errors_are_classified() {
    let err = BacklightError::from(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "sysfs write denied",
    ));
    assert!(err.is_permission_denied());
    assert!(!err.is_not_found());
    assert!(err.source().is_some());
}

#[test]
fn mock_counters_clamp_and_fault_injection() {
    let (backlight, mock) = Backlight::new_mocked();
    assert_eq!(backlight.max_brightness(), 255);
    assert_eq!(mock.read_count(), 0);
    assert_eq!(mock.write_count(), 0);

    mock.set_raw(42);
    assert_eq!(backlight.read().unwrap(), 42);
    assert_eq!(mock.read_count(), 1);

    backlight.write(300).unwrap();
    assert_eq!(mock.write_count(), 1);
    assert_eq!(mock.last_written(), 255);
    assert_eq!(backlight.read().unwrap(), 255);
    assert_eq!(mock.read_count(), 2);

    mock.set_max_brightness(128);
    backlight.write(200).unwrap();
    assert_eq!(mock.last_written(), 128);
    assert_eq!(backlight.max_brightness(), 128);

    mock.set_read_error(Some("panel I2C lockup".into()));
    let err = backlight.read().unwrap_err();
    assert!(err.is_mock_fault());
    assert!(err.summary().contains("panel I2C lockup"));
    assert_eq!(mock.read_count(), 3);

    mock.set_write_error(Some("backlight write rejected".into()));
    let err = backlight.write(10).unwrap_err();
    assert!(err.is_mock_fault());
    assert!(err.summary().contains("backlight write rejected"));
    assert_eq!(mock.write_count(), 3);

    mock.set_read_error(None);
    mock.set_write_error(None);
    assert_eq!(backlight.read().unwrap(), 128);
    backlight.write(64).unwrap();
    assert_eq!(mock.last_written(), 64);
}

#[tokio::test]
async fn async_wrappers_delegate_to_blocking_backend() {
    let (backlight, mock) = Backlight::new_mocked();
    mock.set_raw(77);

    assert_eq!(backlight.read_async().await.unwrap(), 77);
    assert_eq!(mock.read_count(), 1);

    backlight.write_async(999).await.unwrap();
    assert_eq!(mock.write_count(), 1);
    assert_eq!(mock.last_written(), 255);
    assert_eq!(backlight.read_async().await.unwrap(), 255);

    mock.set_write_error(Some("async fault".into()));
    let err = backlight.write_async(10).await.unwrap_err();
    assert!(err.is_mock_fault());
    assert!(err.summary().contains("async fault"));
}
