//! Device administration: discovery, connection, pairing, and removal calls.
//!
//! [`DeviceCommand`] queues user-initiated actions from the render thread for
//! the `bt-monitor` worker to execute, [`PairingStatus`] carries pairing
//! progress back to the UI, and [`device_action_message`] maps transport
//! failures onto the short user-facing strings shown in toasts.
//!
//! All blocking D-Bus calls in this module must run on plain worker threads,
//! never inside a Tokio runtime context (the `zbus` blocking API drives a
//! global runtime internally).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use zbus::blocking::Connection as BlockingConnection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::connection::{BtAddress, BtError};

/// Maximum queued device commands before new arrivals are dropped and logged.
const QUEUE_CAPACITY: usize = 32;

/// User-initiated device action dispatched from the render thread to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceCommand {
    /// Force an immediate authoritative device-list publish.
    Refresh,
    /// Start adapter discovery for the Add Device screen.
    StartDiscovery,
    /// Stop adapter discovery (screen closed or pairing started).
    StopDiscovery,
    /// Connect to a discovered or paired device.
    Connect(BtAddress),
    /// Disconnect the active device.
    Disconnect(BtAddress),
    /// Remove a paired device from `BlueZ`.
    Remove(BtAddress),
    /// Begin pairing with a discovered device.
    Pair(BtAddress),
    /// Cancel an in-flight pairing attempt.
    CancelPairing,
    /// Release the screen-owned pairing agent after a terminal state.
    ReleaseAgent,
}

/// Thread-safe queue bridging Slint render-thread callbacks and the worker.
///
/// The render thread only `push`es (cheap mutex); the worker drains the queue
/// once per monitor tick and executes the blocking D-Bus calls off-thread.
#[derive(Debug, Clone, Default)]
pub struct DeviceCommandQueue {
    queue: Arc<Mutex<VecDeque<DeviceCommand>>>,
}

impl DeviceCommandQueue {
    /// Creates an empty device command queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Enqueues a device command for execution on the `bt-monitor` worker.
    ///
    /// # Errors
    /// Never: a poisoned mutex is recovered with `into_inner` so the enqueue
    /// still lands. When the 32-entry backlog is full the new command is
    /// dropped and logged at warn level.
    pub fn push(&self, command: DeviceCommand) {
        let mut lock = match self.queue.lock() {
            Ok(lock) => lock,
            Err(poisoned) => {
                tracing::warn!("DeviceCommandQueue mutex poisoned in push; recovering");
                poisoned.into_inner()
            }
        };
        if lock.len() < QUEUE_CAPACITY {
            lock.push_back(command);
        } else {
            tracing::warn!("device command queue full; dropping {command:?}");
        }
    }

    /// Returns `true` while no command is pending execution.
    ///
    /// # Errors
    /// Never: a poisoned mutex is recovered with `into_inner` so the queue
    /// state stays queryable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self.queue.lock() {
            Ok(lock) => lock.is_empty(),
            Err(poisoned) => {
                tracing::warn!("DeviceCommandQueue mutex poisoned in is_empty; recovering");
                poisoned.into_inner().is_empty()
            }
        }
    }

    /// Drains all pending commands in FIFO order.
    ///
    /// # Errors
    /// Never: a poisoned mutex is recovered with `into_inner` so pending
    /// commands are never silently dropped.
    #[must_use]
    pub fn drain(&self) -> Vec<DeviceCommand> {
        match self.queue.lock() {
            Ok(mut lock) => lock.drain(..).collect(),
            Err(poisoned) => {
                tracing::warn!("DeviceCommandQueue mutex poisoned in drain; recovering");
                let mut lock = poisoned.into_inner();
                lock.drain(..).collect()
            }
        }
    }
}

/// Pairing progress published from the worker to the Bluetooth screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    /// No pairing attempt is in flight.
    Idle,
    /// `Connect`/`Pair` dispatched; waiting for `BlueZ` to negotiate.
    Connecting {
        /// Display name of the target device.
        name: Box<str>,
    },
    /// Numeric comparison available for the user to verify on the device.
    AwaitingConfirmation {
        /// Display name of the target device.
        name: Box<str>,
        /// Six-digit numeric comparison shown on screen.
        passkey: u32,
    },
    /// Pairing completed successfully.
    Paired {
        /// Display name of the paired device.
        name: Box<str>,
    },
    /// Pairing failed with a user-facing message.
    Failed {
        /// Short, backtrace-free failure message for the toast banner.
        message: Box<str>,
    },
    /// Pairing exceeded the connection deadline and was abandoned.
    TimedOut,
    /// Pairing was cancelled by the user or the remote device.
    Cancelled,
}

/// Device-admin operation recorded by the mock controller for inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAction {
    /// `org.bluez.Adapter1.StartDiscovery` executed.
    StartDiscovery,
    /// `org.bluez.Adapter1.StopDiscovery` executed.
    StopDiscovery,
    /// `org.bluez.Device1.Connect` executed.
    Connect,
    /// `org.bluez.Device1.Disconnect` executed.
    Disconnect,
    /// `org.bluez.Device1.Pair` executed.
    Pair,
    /// `org.bluez.Device1.CancelPairing` executed.
    CancelPairing,
    /// `org.bluez.Adapter1.RemoveDevice` executed.
    Remove,
    /// `org.freedesktop.DBus.Properties.Set(Trusted)` executed.
    SetTrusted,
}

#[zbus::proxy(
    default_service = "org.bluez",
    interface = "org.bluez.Adapter1",
    gen_async = false,
    blocking_name = "BlueZAdapterAdminProxyBlocking"
)]
trait BlueZAdapterAdmin {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    default_service = "org.bluez",
    interface = "org.bluez.Device1",
    gen_async = false,
    blocking_name = "BlueZDevice1ProxyBlocking"
)]
trait BlueZDevice1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn pair(&self) -> zbus::Result<()>;
    fn cancel_pairing(&self) -> zbus::Result<()>;
}

/// Builds the canonical `org.bluez` device object path for `addr` on `adapter`.
///
/// # Errors
/// Returns [`BtError::adapter_not_found`] when `adapter` is empty or contains
/// characters that cannot form a D-Bus object-path segment: such an id can
/// never resolve to a bus object.
pub fn device_object_path(adapter: &str, addr: BtAddress) -> Result<OwnedObjectPath, BtError> {
    if adapter.is_empty() || !adapter.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(BtError::adapter_not_found(adapter));
    }
    let raw = format!(
        "/org/bluez/{adapter}/dev_{}",
        addr.to_string().replace(':', "_")
    );
    OwnedObjectPath::try_from(raw).map_err(|e| {
        BtError::dbus_with_source(format!("invalid Bluetooth device object path: {e}"), e)
    })
}

/// Extracts the device address from a `BlueZ` device object path.
///
/// Delegates to the shared `dev_` segment parser used for `BlueALSA` PCM paths.
#[must_use]
pub fn device_address_from_path(path: &str) -> Option<BtAddress> {
    crate::connection::DbusClient::extract_mac_from_pcm_path(path)
        .and_then(|raw| BtAddress::parse_str(&raw).ok())
}

/// Maps a device-action failure to a short, backtrace-free user message.
///
/// The authentication variants are only produced by `org.bluez.Device1.Pair`,
/// so one table serves pairing and connect/disconnect/remove alike.
#[must_use]
pub fn device_action_message(err: &BtError) -> &'static str {
    if err.is_adapter_not_found() {
        return "Bluetooth adapter not available";
    }
    if err.is_invalid_address() {
        return "Invalid device address";
    }
    if err.is_cancelled() {
        return "Operation cancelled";
    }
    if err.is_mock_fault() {
        return "Operation failed";
    }
    let summary = err.summary();
    for (needle, message) in ERROR_TABLE {
        if summary.contains(needle) {
            return message;
        }
    }
    "Bluetooth operation failed"
}

/// `org.bluez` error-name needles mapped to user-facing toast text.
///
/// More specific names must precede generic prefixes; entries whose text equals
/// the fallback are omitted.
const ERROR_TABLE: [(&str, &str); 16] = [
    ("Error.AuthenticationCanceled", "Pairing cancelled"),
    ("Error.AuthenticationFailed", "Pairing failed"),
    ("Error.AuthenticationRejected", "Pairing rejected"),
    ("Error.AuthenticationTimeout", "Pairing timed out"),
    ("Error.AlreadyConnected", "Device already connected"),
    (
        "Error.ConnectionAttemptFailed",
        "Could not connect to device",
    ),
    ("Error.DoesNotExist", "Device not available"),
    (".UnknownObject", "Device not available"),
    ("Error.NotReady", "Bluetooth adapter not ready"),
    ("Error.NotConnected", "Device not connected"),
    ("Error.NotSupported", "Operation not supported"),
    ("Error.InProgress", "Operation already in progress"),
    ("Error.NotAuthorized", "Not authorized"),
    ("Error.NotAvailable", "Device unavailable"),
    ("Error.AlreadyExists", "Device already paired"),
    ("Error.InvalidArguments", "Invalid device address"),
];

/// Builds the adapter object path for `adapter`.
fn adapter_path(adapter: &str) -> String {
    format!("/org/bluez/{adapter}")
}

/// Maps an adapter-scoped call failure, mirroring [`crate::connection::DbusClient`].
fn map_adapter_error(err: zbus::Error, adapter: &str) -> BtError {
    let msg = err.to_string();
    if msg.contains("UnknownObject") || msg.contains("does not exist") {
        BtError::adapter_not_found(adapter)
    } else {
        BtError::from(err)
    }
}

/// Starts Bluetooth inquiry on `adapter`.
///
/// # Errors
/// Returns [`BtError::adapter_not_found`] when the adapter is absent, or the
/// underlying `BlueZ` failure otherwise.
pub(crate) fn start_discovery(conn: &BlockingConnection, adapter: &str) -> Result<(), BtError> {
    let path = adapter_path(adapter);
    let proxy = match BlueZAdapterAdminProxyBlocking::builder(conn)
        .path(path.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, "starting Bluetooth discovery");
    proxy
        .start_discovery()
        .map_err(|e| map_adapter_error(e, adapter))
}

/// Stops Bluetooth inquiry on `adapter`.
///
/// # Errors
/// Returns [`BtError::adapter_not_found`] when the adapter is absent, or the
/// underlying `BlueZ` failure otherwise.
pub(crate) fn stop_discovery(conn: &BlockingConnection, adapter: &str) -> Result<(), BtError> {
    let path = adapter_path(adapter);
    let proxy = match BlueZAdapterAdminProxyBlocking::builder(conn)
        .path(path.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, "stopping Bluetooth discovery");
    proxy
        .stop_discovery()
        .map_err(|e| map_adapter_error(e, adapter))
}

/// Removes the paired device `addr` from `adapter`'s `BlueZ` device cache.
///
/// # Errors
/// Returns [`BtError::adapter_not_found`] when the adapter is absent, or the
/// underlying `BlueZ` failure otherwise.
pub(crate) fn remove_device(
    conn: &BlockingConnection,
    adapter: &str,
    addr: BtAddress,
) -> Result<(), BtError> {
    let path = device_object_path(adapter, addr)?;
    let adapter_obj = adapter_path(adapter);
    let proxy = match BlueZAdapterAdminProxyBlocking::builder(conn)
        .path(adapter_obj.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, device = %addr, "removing Bluetooth device");
    proxy
        .remove_device(&path)
        .map_err(|e| map_adapter_error(e, adapter))
}

/// Connects to device `addr` on `adapter`.
///
/// # Errors
/// Returns the underlying `BlueZ` failure, including `DoesNotExist` when the
/// device object is unknown to `BlueZ`.
pub(crate) fn connect_device(
    conn: &BlockingConnection,
    adapter: &str,
    addr: BtAddress,
) -> Result<(), BtError> {
    let path = device_object_path(adapter, addr)?;
    let proxy = match BlueZDevice1ProxyBlocking::builder(conn)
        .path(path.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, device = %addr, "connecting to Bluetooth device");
    proxy.connect().map_err(BtError::from)
}

/// Disconnects device `addr` on `adapter`.
///
/// # Errors
/// Returns the underlying `BlueZ` failure.
pub(crate) fn disconnect_device(
    conn: &BlockingConnection,
    adapter: &str,
    addr: BtAddress,
) -> Result<(), BtError> {
    let path = device_object_path(adapter, addr)?;
    let proxy = match BlueZDevice1ProxyBlocking::builder(conn)
        .path(path.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, device = %addr, "disconnecting Bluetooth device");
    proxy.disconnect().map_err(BtError::from)
}

/// Pairs with device `addr` on `adapter`; blocks until pairing settles.
///
/// The screen-owned pairing agent must already be registered so
/// `RequestConfirmation` can deliver the numeric comparison.
///
/// # Errors
/// Returns the underlying `BlueZ` pairing failure.
pub(crate) fn pair_device(
    conn: &BlockingConnection,
    adapter: &str,
    addr: BtAddress,
) -> Result<(), BtError> {
    let path = device_object_path(adapter, addr)?;
    let proxy = match BlueZDevice1ProxyBlocking::builder(conn)
        .path(path.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, device = %addr, "pairing with Bluetooth device");
    proxy.pair().map_err(BtError::from)
}

/// Cancels an in-flight pairing attempt with device `addr`.
///
/// # Errors
/// Returns the underlying `BlueZ` failure.
pub(crate) fn cancel_pairing(
    conn: &BlockingConnection,
    adapter: &str,
    addr: BtAddress,
) -> Result<(), BtError> {
    let path = device_object_path(adapter, addr)?;
    let proxy = match BlueZDevice1ProxyBlocking::builder(conn)
        .path(path.as_str())
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    tracing::info!(adapter, device = %addr, "cancelling Bluetooth pairing");
    proxy.cancel_pairing().map_err(BtError::from)
}

/// Sets the `Trusted` property on device `addr` for auto-reconnect parity.
///
/// # Errors
/// Returns the underlying `BlueZ` failure.
pub(crate) fn set_trusted(
    conn: &BlockingConnection,
    adapter: &str,
    addr: BtAddress,
    trusted: bool,
) -> Result<(), BtError> {
    let path = device_object_path(adapter, addr)?;
    let proxy = match zbus::blocking::fdo::PropertiesProxy::builder(conn)
        .destination("org.bluez")
        .and_then(|b| b.path(path.as_str()))
        .and_then(zbus::blocking::proxy::Builder::build)
    {
        Ok(proxy) => proxy,
        Err(e) => return Err(BtError::from(e)),
    };
    let iface = zbus::names::InterfaceName::try_from("org.bluez.Device1")
        .map_err(|e| zbus::Error::Address(e.to_string()))?;
    tracing::info!(adapter, device = %addr, trusted, "updating Bluetooth device trust");
    proxy
        .set(iface, "Trusted", zbus::zvariant::Value::Bool(trusted))
        .map_err(BtError::from)
}
