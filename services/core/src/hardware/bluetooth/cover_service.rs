//! D-Bus consumer for the cover-art publisher service.
//!
//! `bt-manager` hosts the direct-BIP cover-art provider (see
//! [`crate::cli::cover_provider`]), which publishes the active track's
//! artwork on the system bus. This module is the read side of that
//! contract: `core` queries the published snapshot instead of running its
//! own obexd/OBEX pipeline, so exactly one BIP session exists per phone.
//!
//! Contract (served by [`crate::cli::cover_provider::CoverArtIface`]):
//! - bus name [`COVER_ART_DBUS_NAME`]
//! - object path [`COVER_ART_OBJECT_PATH`]
//! - interface [`COVER_ART_INTERFACE`]
//! - properties `device`/`handle`/`path` (empty string when inactive)

/// D-Bus well-known bus name of the cover-art service.
pub const COVER_ART_DBUS_NAME: &str = "org.speaker.CoverArt";
/// D-Bus object path of the cover-art service.
pub const COVER_ART_OBJECT_PATH: &str = "/org/speaker/coverart";
/// D-Bus interface name of the cover-art service.
pub const COVER_ART_INTERFACE: &str = "org.speaker.CoverArt1";

/// A published cover-art snapshot read from the cover-art service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArtSnapshot {
    /// Canonical MAC address of the device the artwork belongs to.
    pub device: Box<str>,
    /// AOSP BIP image handle of the published image.
    pub handle: Box<str>,
    /// Cache file path of the published JPEG (atomic tmp+rename writes).
    pub path: Box<str>,
}

/// What a `Properties.GetAll` against the cover-art service revealed.
///
/// The distinction matters for consumers: a genuine [`CoverSnapshot::Absent`]
/// service is the only state where stale-independent fallbacks (e.g. the bare
/// disk cache) are defensible; a [`CoverSnapshot::Fault`] means the truth is
/// unknown and must not be substituted with a healthy-looking default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverSnapshot {
    /// The provider published an active image for this (device, handle).
    Published(CoverArtSnapshot),
    /// The provider is running and reported an empty (cleared) state.
    Cleared,
    /// The service is absent (bt-manager provider not running).
    Absent,
    /// The query failed (transport, permission, malformed reply): the
    /// published truth is unknown.
    Fault(Box<str>),
}

/// Reads the currently published cover-art snapshot state.
///
/// Distinguishes absence ([`CoverSnapshot::Absent`]) from transport faults
/// ([`CoverSnapshot::Fault`]) and from an actively cleared state
/// ([`CoverSnapshot::Cleared`]), per the "Distinguish D-Bus Absence from
/// Transport Faults" rule; a fault is never masked as a healthy default.
#[must_use]
pub fn query_snapshot_state(conn: &zbus::blocking::Connection) -> CoverSnapshot {
    query_snapshot_impl(conn)
}

/// Reads the currently published cover-art snapshot.
///
/// Convenience over [`query_snapshot_state`]: returns the snapshot when the
/// provider published an active image, `None` otherwise (service absent,
/// cleared state, or fault — the fault is logged there). Use
/// [`query_snapshot_state`] when the distinction matters, e.g. before
/// falling back to disk-cache lookups.
#[must_use]
pub fn query_snapshot(conn: &zbus::blocking::Connection) -> Option<CoverArtSnapshot> {
    match query_snapshot_state(conn) {
        CoverSnapshot::Published(snapshot) => Some(snapshot),
        CoverSnapshot::Cleared | CoverSnapshot::Absent | CoverSnapshot::Fault(_) => None,
    }
}

fn query_snapshot_impl(conn: &zbus::blocking::Connection) -> CoverSnapshot {
    use std::collections::HashMap;
    use zbus::zvariant::OwnedValue;

    // One `Properties.GetAll` round-trip: the provider publishes all three
    // fields under one lock, so a single reply can never straddle a
    // concurrent publish/clear (three `Get` calls could).
    let proxy = match zbus::blocking::Proxy::new(
        conn,
        COVER_ART_DBUS_NAME,
        COVER_ART_OBJECT_PATH,
        "org.freedesktop.DBus.Properties",
    ) {
        Ok(proxy) => proxy,
        Err(e) => {
            log_query_failure("cannot build cover-art proxy", &e);
            return CoverSnapshot::Fault(e.to_string().into());
        }
    };
    let reply = match proxy.call_method("GetAll", &(COVER_ART_INTERFACE,)) {
        Ok(reply) => reply,
        Err(e) => {
            if is_absent_service_error(&e) {
                tracing::debug!(
                    error = %e,
                    "cover-art service not available (bt-manager provider not running?)"
                );
                return CoverSnapshot::Absent;
            }
            log_query_failure("cover-art property query failed", &e);
            return CoverSnapshot::Fault(e.to_string().into());
        }
    };
    let Ok(properties) = reply.body().deserialize::<HashMap<String, OwnedValue>>() else {
        tracing::warn!(context = "cover-art GetAll returned an unexpected body");
        return CoverSnapshot::Fault("unexpected GetAll body shape".into());
    };
    let property = |name: &str| {
        properties
            .get(name)
            .and_then(|v| String::try_from(v.clone()).ok())
    };
    let (Some(device), Some(handle), Some(path)) =
        (property("Device"), property("Handle"), property("Path"))
    else {
        tracing::warn!(context = "cover-art snapshot misses properties");
        return CoverSnapshot::Fault("published properties missing or non-string".into());
    };
    if handle.is_empty() {
        tracing::trace!("cover-art service has no published image");
        return CoverSnapshot::Cleared;
    }
    CoverSnapshot::Published(CoverArtSnapshot {
        device: device.into(),
        handle: handle.into(),
        path: path.into(),
    })
}

/// Whether `e` reports the service (name) being absent rather than a
/// transport/permission fault.
///
/// zbus surfaces remote D-Bus errors as `MethodError` carrying the remote
/// error name (e.g. `org.freedesktop.DBus.Error.ServiceUnknown`), with some
/// canonical names also converted into the typed `FDO` variant.
fn is_absent_service_error(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, _, _) => is_absent_error_name(name.as_str()),
        zbus::Error::FDO(fdo) => {
            matches!(
                fdo.as_ref(),
                zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
            )
        }
        _ => false,
    }
}

/// Whether a remote D-Bus error name reports the service (name) being absent.
fn is_absent_error_name(name: &str) -> bool {
    name.ends_with("ServiceUnknown") || name.ends_with("NameHasNoOwner")
}

fn log_query_failure(context: &str, e: &zbus::Error) {
    tracing::warn!(error = %e, context);
}

#[cfg(test)]
mod tests {
    use super::{is_absent_error_name, is_absent_service_error};

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
}
