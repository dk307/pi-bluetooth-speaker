//! Integration tests for Bluetooth connection probing and D-Bus method error classification.

use speaker_bluetooth::bluetooth_test_helpers::{
    is_unknown_method_error, run_probe_command, run_probe_command_with_deadline,
};

/// Builds a `MethodError` the way zbus surfaces remote method failures.
fn method_error(name: &str) -> zbus::Error {
    let message = zbus::message::Message::method_call("/org/example", "Call")
        .expect("valid method call")
        .build(&())
        .expect("buildable message");
    zbus::Error::MethodError(name.try_into().expect("valid error name"), None, message)
}

#[test]
fn is_unknown_method_error_matches_error_names() {
    assert!(is_unknown_method_error(&method_error(
        "org.freedesktop.DBus.Error.UnknownMethod"
    )));
    assert!(is_unknown_method_error(&method_error(
        "org.freedesktop.DBus.Error.UnknownObject"
    )));
}

#[test]
fn is_unknown_method_error_rejects_other_names() {
    assert!(!is_unknown_method_error(&method_error(
        "org.freedesktop.DBus.Error.AccessDenied"
    )));
    assert!(!is_unknown_method_error(&method_error(
        "org.freedesktop.DBus.Error.ServiceUnknown"
    )));
}

#[test]
fn is_unknown_method_error_matches_fdo_typed_variants() {
    assert!(is_unknown_method_error(&zbus::Error::FDO(Box::new(
        zbus::fdo::Error::UnknownMethod("method".into())
    ))));
    assert!(is_unknown_method_error(&zbus::Error::FDO(Box::new(
        zbus::fdo::Error::UnknownObject("object".into())
    ))));
    assert!(!is_unknown_method_error(&zbus::Error::FDO(Box::new(
        zbus::fdo::Error::AccessDenied("denied".into())
    ))));
}

#[test]
fn run_probe_command_captures_stdout() {
    let out = run_probe_command("sh", &["-c", "printf hello"]).expect("probe must succeed");
    assert_eq!(out, "hello");
}

#[test]
fn run_probe_command_rejects_non_success_status() {
    let error = run_probe_command("sh", &["-c", "exit 3"])
        .expect_err("non-zero exit must surface as an error");
    assert!(error.to_string().contains("non-success status"));
}

#[test]
fn run_probe_command_rejects_missing_binary() {
    let error = run_probe_command("/nonexistent/speaker-probe", &[])
        .expect_err("missing binary must surface as an error");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn run_probe_command_kills_on_timeout() {
    let error = run_probe_command_with_deadline(
        "sh",
        &["-c", "sleep 5"],
        std::time::Duration::from_millis(150),
    )
    .expect_err("an unresponsive probe must time out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn is_unknown_method_error_rejects_transport_faults() {
    assert!(!is_unknown_method_error(&zbus::Error::InputOutput(
        std::sync::Arc::new(std::io::Error::other("broken pipe"))
    )));
}
