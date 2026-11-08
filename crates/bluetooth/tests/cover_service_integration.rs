//! Integration tests for D-Bus absent service error classification.

use speaker_bluetooth::{is_absent_error_name, is_absent_service_error};

#[test]
fn absent_service_errors_are_classified() {
    use zbus::Error;
    use zbus::fdo;

    assert!(is_absent_error_name(
        "org.freedesktop.DBus.Error.ServiceUnknown"
    ));
    assert!(is_absent_error_name(
        "org.freedesktop.DBus.Error.NameHasNoOwner"
    ));
    assert!(!is_absent_error_name(
        "org.freedesktop.DBus.Error.AccessDenied"
    ));

    assert!(is_absent_service_error(&Error::FDO(Box::new(
        fdo::Error::NameHasNoOwner("org.speaker.CoverArt".into())
    ))));
    assert!(is_absent_service_error(&Error::FDO(Box::new(
        fdo::Error::ServiceUnknown("org.speaker.CoverArt".into())
    ))));
    assert!(!is_absent_service_error(&Error::InputOutput(
        std::io::Error::from(std::io::ErrorKind::ConnectionReset).into()
    )));
    assert!(!is_absent_service_error(&Error::FDO(Box::new(
        fdo::Error::AccessDenied("nope".into())
    ))));
}
