//! Live D-Bus integration tests for [`check_systemd_unit`] against a fake
//! systemd service served on a private `dbus-daemon` session bus. Covers the
//! unit-found, not-found, and transport-fault branches that pure unit tests
//! cannot reach (`ServiceState` parsing itself is covered elsewhere).

use zbus::DBusError;
use zbus::zvariant::OwnedObjectPath;

use speaker_common::diag::spawn_private_bus;
use speaker_common::systemd::{ServiceState, check_systemd_unit};

/// Emulates systemd's `NoSuchUnit` reply for units that are not loaded.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.freedesktop.systemd1")]
enum SystemdUnitError {
    #[zbus(name = "NoSuchUnit")]
    NoSuchUnit(String),
}

/// Fake `org.freedesktop.systemd1.Manager` resolving unit names to paths.
struct FakeManager {
    unit_path: Option<OwnedObjectPath>,
}

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl FakeManager {
    #[allow(
        unused_variables,
        reason = "consumed by the generated zbus proxy dispatch"
    )]
    fn get_unit(&self, name: &str) -> Result<OwnedObjectPath, SystemdUnitError> {
        match &self.unit_path {
            Some(path) => Ok(path.clone()),
            None => Err(SystemdUnitError::NoSuchUnit(
                "Unit ghost.service not loaded.".into(),
            )),
        }
    }
}

/// Fake `org.freedesktop.systemd1.Unit` exposing `ActiveState` / `LoadState`.
struct FakeUnit {
    active: String,
    load: String,
}

#[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
impl FakeUnit {
    #[zbus(property)]
    fn active_state(&self) -> String {
        self.active.clone()
    }

    #[zbus(property)]
    fn load_state(&self) -> String {
        self.load.clone()
    }
}

/// Builds a fake systemd server owning `org.freedesktop.systemd1` on the
/// private bus. The connection must be kept alive by the caller.
fn serve_fake_systemd(
    address: &str,
    unit_path: Option<OwnedObjectPath>,
    active: &str,
    load: &str,
) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(address)
        .unwrap()
        .name("org.freedesktop.systemd1")
        .unwrap()
        .serve_at("/org/freedesktop/systemd1", FakeManager { unit_path })
        .unwrap()
        .serve_at(
            "/org/freedesktop/systemd1/unit/speaker",
            FakeUnit {
                active: active.into(),
                load: load.into(),
            },
        )
        .unwrap()
        .build()
        .unwrap()
}

fn connect(address: &str) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(address)
        .unwrap()
        .build()
        .unwrap()
}

fn unit_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/speaker").unwrap()
}

#[test]
fn active_unit_maps_to_service_active() {
    let (_kill, address) = spawn_private_bus();
    let _server = serve_fake_systemd(&address, Some(unit_path()), "active", "loaded");

    let conn = connect(&address);
    let manager = speaker_common::systemd::SystemdManagerProxyBlocking::new(&conn).unwrap();
    let state = check_systemd_unit(&conn, &manager, "speaker-dsp.service").unwrap();
    assert_eq!(state, ServiceState::Active);
    assert!(state.is_active());
}

#[test]
fn not_found_load_state_maps_to_service_not_found() {
    let (_kill, address) = spawn_private_bus();
    let _server = serve_fake_systemd(&address, Some(unit_path()), "inactive", "not-found");

    let conn = connect(&address);
    let manager = speaker_common::systemd::SystemdManagerProxyBlocking::new(&conn).unwrap();
    let state = check_systemd_unit(&conn, &manager, "missing.service").unwrap();
    assert_eq!(state, ServiceState::NotFound);
}

#[test]
fn absent_unit_error_maps_to_service_not_found() {
    let (_kill, address) = spawn_private_bus();
    let _server = serve_fake_systemd(&address, None, "active", "loaded");

    let conn = connect(&address);
    let manager = speaker_common::systemd::SystemdManagerProxyBlocking::new(&conn).unwrap();
    let state = check_systemd_unit(&conn, &manager, "ghost.service").unwrap();
    assert_eq!(state, ServiceState::NotFound);
}

#[test]
fn inactive_loaded_unit_maps_to_service_inactive() {
    let (_kill, address) = spawn_private_bus();
    let _server = serve_fake_systemd(&address, Some(unit_path()), "inactive", "loaded");

    let conn = connect(&address);
    let manager = speaker_common::systemd::SystemdManagerProxyBlocking::new(&conn).unwrap();
    let state = check_systemd_unit(&conn, &manager, "speaker-dsp.service").unwrap();
    assert_eq!(state, ServiceState::Inactive);
}

#[test]
fn no_systemd_on_bus_maps_to_failed_probe() {
    let (_kill, address) = spawn_private_bus();
    // No server owns `org.freedesktop.systemd1`: the manager proxy call must
    // surface an error, and `probe_systemd_service` degrades to `Failed`.
    let conn = connect(&address);
    let manager = speaker_common::systemd::SystemdManagerProxyBlocking::new(&conn).unwrap();
    let err = check_systemd_unit(&conn, &manager, "speaker-dsp.service")
        .expect_err("absent systemd must not report a healthy state");
    eprintln!("absent-systemd error: {err}");
    assert!(
        err.to_string().contains("org.freedesktop.systemd1")
            || err.to_string().contains("ServiceUnknown")
            || err.to_string().contains("UnknownObject"),
        "unexpected error shape: {err}"
    );
}
