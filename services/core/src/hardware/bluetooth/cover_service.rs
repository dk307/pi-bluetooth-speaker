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

/// Reads the currently published cover-art snapshot.
///
/// Returns `None` when the service is absent (provider not running) or the
/// published state is empty; transient transport faults also yield `None`
/// (the caller simply retries on its next poll). Absence and transport
/// faults are logged at different levels so a misconfigured appliance is
/// diagnosable from journald alone.
#[must_use]
pub fn query_snapshot(conn: &zbus::blocking::Connection) -> Option<CoverArtSnapshot> {
    let proxy = match zbus::blocking::Proxy::new(
        conn,
        COVER_ART_DBUS_NAME,
        COVER_ART_OBJECT_PATH,
        COVER_ART_INTERFACE,
    ) {
        Ok(proxy) => proxy,
        Err(e) => {
            log_query_failure("cannot build cover-art proxy", &e);
            return None;
        }
    };
    let (device, handle, path) = match (
        proxy.get_property::<String>("Device"),
        proxy.get_property::<String>("Handle"),
        proxy.get_property::<String>("Path"),
    ) {
        (Ok(device), Ok(handle), Ok(path)) => (device, handle, path),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            if is_absent_service_error(&e) {
                tracing::debug!(
                    error = %e,
                    "cover-art service not available (bt-manager provider not running?)"
                );
            } else {
                log_query_failure("cover-art property query failed", &e);
            }
            return None;
        }
    };
    if handle.is_empty() {
        tracing::trace!("cover-art service has no published image");
        return None;
    }
    Some(CoverArtSnapshot {
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
