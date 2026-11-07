//! Systemd service unit states and status helpers.

use serde::{Deserialize, Serialize};
use std::fmt::{self, Display, Formatter};

/// Operational state of a systemd background service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Service unit is running and operational.
    Active,
    /// Service unit is loaded but currently inactive or stopped.
    Inactive,
    /// Service unit entered a failed or crashed state.
    Failed,
    /// Service unit file was not found on the host system.
    NotFound,
}

impl ServiceState {
    /// Returns `true` if the service is actively running.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns a human-readable state description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Active => "Running",
            Self::Inactive => "INACTIVE / STOPPED",
            Self::Failed => "FAILED",
            Self::NotFound => "NOT INSTALLED / NOT FOUND",
        }
    }
}

impl Display for ServiceState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

/// Parses active and load state strings into a [`ServiceState`].
#[must_use]
pub fn parse_systemd_state(active: &str, load: &str) -> ServiceState {
    if active == "active" {
        ServiceState::Active
    } else if load == "not-found" || active == "not-found" {
        ServiceState::NotFound
    } else if active == "failed" {
        ServiceState::Failed
    } else {
        ServiceState::Inactive
    }
}

#[zbus::proxy(
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    interface = "org.freedesktop.systemd1.Manager",
    gen_async = false,
    blocking_name = "SystemdManagerProxyBlocking"
)]
pub trait SystemdManager {
    fn get_unit(&self, name: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.systemd1",
    interface = "org.freedesktop.systemd1.Unit",
    gen_async = false,
    blocking_name = "SystemdUnitProxyBlocking"
)]
pub trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn load_state(&self) -> zbus::Result<String>;
}

/// Reports whether a D-Bus error indicates that a queried systemd unit was not found.
#[must_use]
pub fn is_no_such_unit_error(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => {
            let s = name.as_str();
            s == "org.freedesktop.systemd1.NoSuchUnit"
                || s == "org.freedesktop.systemd1.UnknownUnit"
                || s.ends_with(".NoSuchUnit")
                || s.ends_with(".UnknownUnit")
        }
        zbus::Error::FDO(fdo_err) => matches!(**fdo_err, zbus::fdo::Error::UnknownObject(_)),
        other => {
            let s = other.to_string();
            s.contains("NoSuchUnit") || s.contains("UnknownUnit")
        }
    }
}

/// Checks the state of a systemd unit using an existing connection and manager proxy.
///
/// # Errors
///
/// Returns an error if the D-Bus connection fails or the unit properties cannot be queried.
pub fn check_systemd_unit(
    conn: &zbus::blocking::Connection,
    manager: &SystemdManagerProxyBlocking<'_>,
    name: &str,
) -> zbus::Result<ServiceState> {
    match manager.get_unit(name) {
        Ok(path) => {
            let unit = match SystemdUnitProxyBlocking::builder(conn)
                .path(path)
                .and_then(zbus::blocking::proxy::Builder::build)
            {
                Ok(u) => u,
                Err(ref e) if is_no_such_unit_error(e) => return Ok(ServiceState::NotFound),
                Err(e) => return Err(e),
            };
            let active = unit.active_state()?;
            let load = unit.load_state()?;
            Ok(parse_systemd_state(&active, &load))
        }
        Err(ref e) if is_no_such_unit_error(e) => Ok(ServiceState::NotFound),
        Err(e) => Err(e),
    }
}

/// Probes systemd for a service unit state using D-Bus, distinguishing
/// unit absence (`NoSuchUnit`) from degradation/failure.
#[must_use]
pub fn probe_systemd_service(unit_name: &str) -> ServiceState {
    let Ok(conn) = zbus::blocking::Connection::system() else {
        return ServiceState::Failed;
    };
    let Ok(manager) = SystemdManagerProxyBlocking::new(&conn) else {
        return ServiceState::Failed;
    };
    check_systemd_unit(&conn, &manager, unit_name).unwrap_or(ServiceState::Failed)
}
