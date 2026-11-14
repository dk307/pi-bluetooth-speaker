//! Additional integration tests for error module to increase coverage.

use audio_manager::{AmpError, AmpResult};

#[test]
fn amp_error_io() {
    let err = AmpError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "file not found",
    ));
    assert!(err.to_string().contains("I/O error"));
    assert!(matches!(err, AmpError::Io(_)));
}

#[test]
fn amp_error_json() {
    let err = AmpError::Json(serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid json",
    )));
    assert!(err.to_string().contains("JSON error"));
    assert!(matches!(err, AmpError::Json(_)));
}

#[test]
fn amp_error_dbus() {
    let err = AmpError::DBus(zbus::Error::Failure("test".into()));
    assert!(err.to_string().contains("D-Bus error"));
    assert!(matches!(err, AmpError::DBus(_)));
}

#[test]
fn amp_error_anyhow() {
    let err = AmpError::Anyhow(anyhow::anyhow!("test error"));
    assert!(err.to_string().contains("test error"));
    assert!(matches!(err, AmpError::Anyhow(_)));
}

#[test]
fn amp_error_dbus_name_taken() {
    let err = AmpError::DBusNameTaken;
    assert_eq!(
        err.to_string(),
        "D-Bus name org.speaker.AudioManager already taken"
    );
    assert!(matches!(err, AmpError::DBusNameTaken));
}

#[test]
fn amp_error_hardware_validation() {
    let err = AmpError::HardwareValidation {
        reason: "validation failed".into(),
    };
    assert!(
        err.to_string()
            .contains("Hardware validation failed: validation failed")
    );
    assert!(matches!(err, AmpError::HardwareValidation { .. }));
}

#[test]
fn amp_error_card_missing() {
    let err = AmpError::CardMissing {
        card: "mycard".into(),
    };
    assert!(err.to_string().contains("Amp card not found: mycard"));
    assert!(matches!(err, AmpError::CardMissing { .. }));
}

#[test]
fn amp_error_control_error() {
    let err = AmpError::ControlError("control missing".into());
    assert!(
        err.to_string()
            .contains("Mixer control error: control missing")
    );
    assert!(matches!(err, AmpError::ControlError(_)));
}

#[test]
fn amp_error_state_file() {
    let err = AmpError::StateFile("corrupted".into());
    assert!(err.to_string().contains("State file error: corrupted"));
    assert!(matches!(err, AmpError::StateFile(_)));
}

#[test]
fn amp_error_internal() {
    let err = AmpError::Internal("internal error".into());
    assert!(err.to_string().contains("Internal error: internal error"));
    assert!(matches!(err, AmpError::Internal(_)));
}

#[test]
fn amp_error_summary_non_hardware() {
    let err = AmpError::Internal("test".into());
    let summary = err.summary();
    assert_eq!(summary, "Internal error: test");
}

#[test]
fn amp_error_constructors() {
    let err = AmpError::hardware_validation("test reason");
    assert!(matches!(err, AmpError::HardwareValidation { .. }));

    let err = AmpError::card_missing("test card");
    assert!(matches!(err, AmpError::CardMissing { .. }));

    let err = AmpError::control_error("test control");
    assert!(matches!(err, AmpError::ControlError(_)));

    let err = AmpError::state_file("test file");
    assert!(matches!(err, AmpError::StateFile(_)));

    let err = AmpError::internal("test internal");
    assert!(matches!(err, AmpError::Internal(_)));
}

#[test]
fn amp_result_type_alias() {
    // Verify AmpResult works as expected
    let ok: AmpResult<i32> = Ok(42);
    match ok {
        Ok(v) => assert_eq!(v, 42),
        Err(_) => panic!("expected Ok"),
    }

    let err: AmpResult<i32> = Err(AmpError::Internal("test".into()));
    assert!(err.is_err());
}

#[test]
fn amp_error_from_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "test");
    let amp_err: AmpError = io_err.into();
    assert!(matches!(amp_err, AmpError::Io(_)));
}

#[test]
fn amp_error_from_json() {
    let json_err = serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid",
    ));
    let amp_err: AmpError = json_err.into();
    assert!(matches!(amp_err, AmpError::Json(_)));
}

#[test]
fn amp_error_from_dbus() {
    let dbus_err = zbus::Error::Failure("test".into());
    let amp_err: AmpError = dbus_err.into();
    assert!(matches!(amp_err, AmpError::DBus(_)));
}

#[test]
fn amp_error_from_anyhow() {
    let anyhow_err = anyhow::anyhow!("test");
    let amp_err: AmpError = anyhow_err.into();
    assert!(matches!(amp_err, AmpError::Anyhow(_)));
}

#[test]
fn amp_error_from_hardware() {
    // Use speaker_hardware's mock fault to create a hardware error
    let (amp, _mock) = speaker_hardware::AmpDevice::new_mocked();
    let hw_err = amp.write_master_volume(999).unwrap_err();
    let amp_err: AmpError = hw_err.into();
    assert!(matches!(amp_err, AmpError::Hardware(_)));
}

#[test]
fn amp_error_debug_impl() {
    let err = AmpError::Internal("test".into());
    let debug_str = format!("{:?}", err);
    assert!(debug_str.contains("Internal"));
    assert!(debug_str.contains("test"));
}

#[test]
fn amp_error_display_all_variants() {
    // Test display for variants we can construct
    let variants = vec![
        AmpError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "test")),
        AmpError::Json(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "test",
        ))),
        AmpError::DBus(zbus::Error::Failure("test".into())),
        AmpError::Anyhow(anyhow::anyhow!("test")),
        AmpError::DBusNameTaken,
        AmpError::HardwareValidation {
            reason: "test".into(),
        },
        AmpError::CardMissing {
            card: "test".into(),
        },
        AmpError::ControlError("test".into()),
        AmpError::StateFile("test".into()),
        AmpError::Internal("test".into()),
    ];

    for err in variants {
        let display = err.to_string();
        assert!(!display.is_empty());
    }
}

#[test]
fn amp_error_summary_hardware_from_device() {
    let (amp, _mock) = speaker_hardware::AmpDevice::new_mocked();
    let inner = amp.write_master_volume(999).unwrap_err();
    let wrapped = AmpError::from(inner);

    let summary = wrapped.summary();
    // Hardware summary should be single line without backtrace
    assert!(!summary.contains('\n'));
    assert!(!summary.contains("Backtrace"));
    assert!(summary.contains("master_volume_raw"));
}
