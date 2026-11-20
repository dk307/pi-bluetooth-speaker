//! Canonical error contract integration tests.

use std::io;

use autobright::AutobrightError;

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AutobrightError>();
};

#[test]
fn constructors_expose_predicates_and_clean_summaries() {
    let config = AutobrightError::config("bad tick");
    assert!(config.is_config());
    assert!(!config.is_invalid_argument());
    assert_eq!(config.summary(), "invalid configuration: bad tick");
    assert!(!config.summary().contains('\n'));

    let argument = AutobrightError::invalid_argument("bad flag");
    assert!(argument.is_invalid_argument());
    assert_eq!(argument.summary(), "invalid argument: bad flag");

    let cancelled = AutobrightError::cancelled("tick worker");
    assert!(cancelled.is_cancelled());
    assert_eq!(cancelled.summary(), "task cancelled: tick worker");

    let fault = AutobrightError::mock_fault("injected");
    assert!(fault.is_mock_fault());
    assert_eq!(fault.summary(), "mock fault: injected");
}

#[test]
fn foreign_errors_are_preserved_as_typed_sources() {
    let io_error = AutobrightError::from(io::Error::new(io::ErrorKind::PermissionDenied, "EACCES"));
    assert!(std::error::Error::source(&io_error).is_some());
    assert!(io_error.summary().contains("EACCES"));

    let dbus_error = AutobrightError::from(zbus::Error::Failure("boom".into()));
    assert!(dbus_error.is_dbus());
    assert!(std::error::Error::source(&dbus_error).is_some());

    let fdo_error = AutobrightError::from(zbus::fdo::Error::Failed("fdo boom".into()));
    assert!(fdo_error.is_dbus());

    assert!(std::error::Error::source(&AutobrightError::config("x")).is_none());
}

#[test]
fn hardware_error_sources_are_preserved_without_stringification() {
    let light = speaker_hardware::light_test_helpers::make_mock_fault_error("lux fault");
    let wrapped = AutobrightError::from(light);
    assert!(wrapped.is_light());
    assert!(std::error::Error::source(&wrapped).is_some());
    assert!(wrapped.summary().contains("lux fault"));

    let (backlight, ctrl) = speaker_hardware::Backlight::new_mocked();
    ctrl.set_read_error(Some("sysfs fault".into()));
    let backlight_error = backlight.read().expect_err("injected read fault");
    let wrapped = AutobrightError::from(backlight_error);
    assert!(wrapped.is_backlight());
    assert!(std::error::Error::source(&wrapped).is_some());
    assert!(wrapped.summary().contains("sysfs fault"));
}
