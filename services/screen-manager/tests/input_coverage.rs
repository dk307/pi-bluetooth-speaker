#![cfg(feature = "ui")]
//! Coverage tests for touch-input error branches, mock-fallback paths, and
//! formatting helpers that the main `ui_input_integration` suite misses.
//!
//! Everything runs against the mock syscall backend, pipes, or `/dev/null`;
//! no real evdev device node is ever opened.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use screen_manager::ui::input::{
    ABS_MT_TRACKING_ID, ABS_X, EV_ABS, EV_KEY, EV_SYN, EvdevSyscall, MockDeviceConfig,
    MockEvdevSyscall, NativeEvdevSyscall, SYN_REPORT, TouchCalibration, TouchDecoder,
    TouchInputDevice,
};

/// Linux relative-axis event type (never produced by touch digitizers).
const EV_REL: u16 = 0x02;

fn mock_backend() -> (Arc<MockEvdevSyscall>, Arc<dyn EvdevSyscall>) {
    let mock = Arc::new(MockEvdevSyscall::new());
    let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;
    (mock, syscall)
}

#[test]
fn native_syscall_open_succeeds_on_dev_null() {
    let native = NativeEvdevSyscall;
    let fd = native.open(Path::new("/dev/null")).expect("open /dev/null");
    assert!(fd >= 0);
    native.close(fd).expect("close fd");
}

#[test]
fn mock_syscall_ebadf_branches_for_unknown_descriptors() {
    let (mock, syscall) = mock_backend();
    // Pushing raw events to an unknown fd is a no-op, not a panic.
    mock.push_raw_events(9999, &[0_u8; 4]);

    let nb = syscall
        .set_nonblocking(9999)
        .expect_err("unknown fd must fail");
    assert_eq!(nb.raw_os_error(), Some(libc::EBADF));
    let rd = syscall
        .read(9999, &mut [0_u8; 8])
        .expect_err("unknown fd must fail");
    assert_eq!(rd.raw_os_error(), Some(libc::EBADF));
    let abs = syscall
        .query_abs(9999, ABS_X)
        .expect_err("unknown fd must fail");
    assert_eq!(abs.raw_os_error(), Some(libc::EBADF));
    let name = syscall.query_name(9999).expect_err("unknown fd must fail");
    assert_eq!(name.raw_os_error(), Some(libc::EBADF));
}

#[test]
fn mock_syscall_rejects_unsupported_abs_codes() {
    let (mock, syscall) = mock_backend();
    let path = PathBuf::from("/dev/input/event_unsupported_abs");
    mock.register_path(&path, MockDeviceConfig::default());
    let fd = syscall.open(&path).expect("open registered path");

    // Neither single- nor multi-touch axis: explicit `InvalidInput`.
    let err = syscall
        .query_abs(fd, ABS_MT_TRACKING_ID)
        .expect_err("unsupported abs code");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn decoder_ignores_unknown_event_codes_and_types() {
    let mut decoder = TouchDecoder::new(TouchCalibration::default());

    // Unknown absolute-axis code is dropped without state changes.
    assert!(
        decoder
            .process_event(&screen_manager::ui::input::make_input_event(
                EV_ABS, 0x3A, 7
            ))
            .is_none()
    );
    // Key event for anything other than BTN_TOUCH is dropped.
    assert!(
        decoder
            .process_event(&screen_manager::ui::input::make_input_event(
                EV_KEY, 0x140, 1
            ))
            .is_none()
    );
    // Entirely unknown event type is dropped.
    assert!(
        decoder
            .process_event(&screen_manager::ui::input::make_input_event(
                EV_REL, 0x00, 1
            ))
            .is_none()
    );
}

#[test]
fn decoder_idle_syn_report_emits_no_action() {
    let mut decoder = TouchDecoder::new(TouchCalibration::default());
    // A sync packet with no pending press transition while released is a no-op.
    let action = decoder.process_event(&screen_manager::ui::input::make_input_event(
        EV_SYN, SYN_REPORT, 0,
    ));
    assert_eq!(action, None);
}

#[test]
fn touch_backend_debug_formatting_covers_both_variants() {
    // Mock backend.
    let (mut mock_dev, _ctrl) = TouchInputDevice::new_mocked();
    let mock_debug = format!("{mock_dev:?}");
    assert!(mock_debug.contains("Mock"), "{mock_debug}");

    // Hardware backend (mock syscall supplies the descriptor).
    let (mock, syscall) = mock_backend();
    let path = PathBuf::from("/dev/input/event_debug");
    mock.register_path(&path, MockDeviceConfig::default());
    mock_dev = TouchInputDevice::open_with(&path, syscall).expect("open_with");
    let hw_debug = format!("{mock_dev:?}");
    assert!(hw_debug.contains("Hardware"), "{hw_debug}");
    assert!(hw_debug.contains("path"), "{hw_debug}");
    assert!(hw_debug.contains("fd"), "{hw_debug}");
}

#[test]
fn open_with_logs_calibration_through_active_subscriber() {
    let (mock, syscall) = mock_backend();
    let path = PathBuf::from("/dev/input/event_subscriber");
    mock.register_path(&path, MockDeviceConfig::default());

    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let dev = TouchInputDevice::open_with(&path, syscall).expect("open_with");
        assert_eq!(dev.calibration().max_x, 800.0);
    });
}

#[test]
fn axis_calibration_falls_back_to_logical_screen_bounds() {
    let (mock, syscall) = mock_backend();
    let path = PathBuf::from("/dev/input/event_zero_max");
    // A digitizer reporting `maximum = 0` has no usable range; the calibration
    // must fall back to the canonical 800x480 panel dimensions.
    mock.register_path(
        &path,
        MockDeviceConfig {
            abs_x: libc::input_absinfo {
                value: 0,
                minimum: 0,
                maximum: 0,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
            abs_y: libc::input_absinfo {
                value: 0,
                minimum: 0,
                maximum: 0,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
            ..Default::default()
        },
    );

    let dev = TouchInputDevice::open_with(&path, syscall).expect("open_with");
    let calibration = dev.calibration();
    assert_eq!(calibration.max_x, 800.0);
    assert_eq!(calibration.max_y, 480.0);
    assert!(calibration.is_valid());
}

/// Minimal [`EvdevSyscall`] whose `read` reports EOF immediately, exercising
/// the `Ok(0)` termination branch of the hardware polling loop.
#[derive(Debug)]
struct EofSyscall;

impl EvdevSyscall for EofSyscall {
    fn open(&self, _path: &Path) -> std::io::Result<i32> {
        Ok(4242)
    }

    fn set_nonblocking(&self, _fd: i32) -> std::io::Result<()> {
        Ok(())
    }

    fn read(&self, _fd: i32, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }

    fn query_abs(&self, _fd: i32, _abs_code: u16) -> std::io::Result<libc::input_absinfo> {
        Ok(libc::input_absinfo {
            value: 0,
            minimum: 0,
            maximum: 800,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        })
    }

    fn query_name(&self, _fd: i32) -> std::io::Result<String> {
        Ok("eof-device".to_string())
    }

    fn close(&self, _fd: i32) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn poll_actions_stops_on_eof_read() {
    let mut dev = TouchInputDevice::open_with("/dev/input/eof-device", Arc::new(EofSyscall))
        .expect("open_with eof backend");
    let actions = dev.poll_actions().expect("EOF must terminate the loop");
    assert_eq!(actions, [] as [screen_manager::ui::TouchAction; 0]);
}
