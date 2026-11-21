//! Integration tests for the sysfs backlight driver and its mock controller.
//!
//! Uses a hermetic synthetic sysfs tree for filesystem paths and the paired
//! mock backend for fault injection, call counters, and clamp verification.

use std::error::Error as _;
use std::fs;
use std::io;

use speaker_common::diag::ScopedTempDir;
use speaker_common::runtime::FromJoinError;
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
fn appliance_default_prefers_the_historical_device_name() {
    let dir = make_sysfs_tree("backlight_preferred", "rpi_backlight", "255\n", "100\n");
    let other = dir.path().join("10-0045");
    fs::create_dir_all(&other).expect("create i2c backlight dir");
    fs::write(other.join("max_brightness"), "255\n").expect("write max");
    fs::write(other.join("brightness"), "80\n").expect("write raw");

    let (backlight, device) = Backlight::with_appliance_default_in(dir.path()).unwrap();
    assert_eq!(&*device, "rpi_backlight");
    assert_eq!(backlight.read().unwrap(), 100);
}

#[test]
fn appliance_default_discovers_i2c_backlight_device() {
    let dir = make_sysfs_tree("backlight_discovery", "10-0045", "255\n", "200\n");

    let (backlight, device) = Backlight::with_appliance_default_in(dir.path()).unwrap();
    assert_eq!(&*device, "10-0045");
    assert_eq!(backlight.max_brightness(), 255);
    assert_eq!(backlight.read().unwrap(), 200);
}

#[test]
fn appliance_default_follows_sysfs_class_symlinks() {
    // `/sys/class/backlight/<device>` entries are symlinks into `/sys/devices`.
    // A directory-only filter would skip every real device.
    let target = ScopedTempDir::new("backlight_symlink_target");
    let device = target.path().join("10-0045");
    fs::create_dir_all(&device).expect("create symlink target device dir");
    fs::write(device.join("max_brightness"), "255\n").expect("write max");
    fs::write(device.join("brightness"), "123\n").expect("write raw");

    let root = ScopedTempDir::new("backlight_symlink_root");
    std::os::unix::fs::symlink(&device, root.path().join("10-0045"))
        .expect("create sysfs-class style symlink");

    let (backlight, name) = Backlight::with_appliance_default_in(root.path()).unwrap();
    assert_eq!(&*name, "10-0045");
    assert_eq!(backlight.read().unwrap(), 123);
}

#[test]
fn appliance_default_skips_malformed_devices_and_fails_when_none_usable() {
    let dir = ScopedTempDir::new("backlight_no_usable");
    let broken = dir.path().join("aaa-broken");
    fs::create_dir_all(&broken).expect("create malformed device dir");
    fs::write(broken.join("max_brightness"), "not-a-number\n").expect("write max");
    fs::write(broken.join("brightness"), "0\n").expect("write raw");

    let err = Backlight::with_appliance_default_in(dir.path()).unwrap_err();
    assert!(err.is_not_found(), "expected NotFound, got: {err}");
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

#[test]
fn cancelled_and_invalid_data_errors_expose_full_contract() {
    let cancelled = BacklightError::from_cancelled("worker stopped during shutdown");
    assert!(cancelled.is_cancelled());
    assert!(!cancelled.is_mock_fault());
    assert!(!cancelled.is_not_found());
    assert!(
        cancelled
            .summary()
            .contains("task cancelled: worker stopped"),
        "unexpected summary: {}",
        cancelled.summary()
    );
    assert!(!cancelled.summary().contains("Backtrace"));

    let dir = make_sysfs_tree("backlight_contract", "rpi_backlight", "nope\n", "0\n");
    let invalid = Backlight::with_root(dir.path(), "rpi_backlight").unwrap_err();
    assert!(invalid.is_invalid_data());
    assert!(
        std::error::Error::source(&invalid).is_none(),
        "non-I/O errors must not fabricate a source"
    );
    assert_ne!(invalid.backtrace().to_string(), "");
}

#[test]
fn default_constructor_uses_appliance_backlight_device() {
    // On a developer host the panel sysfs node is absent; the constructor must
    // still attempt the canonical device and report a typed NotFound error.
    match Backlight::new() {
        Ok(backlight) => assert_eq!(backlight.max_brightness(), 255),
        Err(err) => {
            assert!(err.is_not_found(), "unexpected default-device error: {err}");
        }
    }
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
