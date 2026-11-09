//! Bluetooth audio transport state reconciler between `BlueZ` 5.82 and `bluez-alsa` v5.0.0.
//!
//! # Problem Statement & Subsystem Divergence
//!
//! On a headless Linux audio appliance, Bluetooth audio streaming depends on two
//! disjoint system daemons communicating over the system D-Bus:
//!
//! 1. **`BlueZ` (`org.bluez`, `bluetoothd`)**: Manages the Bluetooth controller radio,
//!    inquiry/pairing state machines, baseband ACL connections, L2CAP channels, and
//!    A2DP profile negotiation (`org.bluez.MediaEndpoint1`, `org.bluez.MediaTransport1`).
//! 2. **`bluez-alsa` (`org.bluealsa`, `bluealsad`)**: Registers as an A2DP `MediaEndpoint`
//!    with `BlueZ`. When a remote peer connects A2DP, `BlueZ` acquires a streaming file
//!    descriptor (`fd*`) and passes it to `bluealsad`, which exposes ALSA PCM interfaces
//!    (`org.bluealsa.PCM1`) and forwards audio to ALSA or DSP pipelines.
//!
//! Because `BlueZ` and `bluealsad` are independent processes, several edge cases cause
//! their internal state machines to desynchronize silently:
//!
//! ## Case A: Orphaned `MediaTransport1` (e.g. `bluealsad` restart / crash)
//! When `bluealsad` restarts (e.g. crash, OOM, service restart, or configuration change),
//! it drops its registered `MediaEndpoint` and loses all existing PCM devices. However,
//! `BlueZ` continues to hold the open ACL connection and the active `MediaTransport1`
//! object (`/org/bluez/hciX/dev_XX_XX_XX_XX_XX_XX/fd*`).
//!
//! When the remote phone attempts to resume playback, `BlueZ` believes the transport is
//! already established and never negotiates a new endpoint with `bluealsad`. Calling
//! `ConnectProfile(A2DP_SOURCE)` on `org.bluez.Device1` is an effective **no-op** in this
//! state because `BlueZ` sees `MediaTransport1` already active.
//!
//! **Recovery:** The reconciler detects `has_bluez_transport == true && has_bluealsa_pcm == false`
//! persisting across $\ge 2$ polling ticks, and executes an atomic baseband cycle:
//! `Disconnect()` $\to$ settle delay $\to$ `Connect()`. This purges the orphaned `fd*`
//! transport in `BlueZ` and forces a fresh A2DP endpoint handshake with `bluealsad`.
//!
//! ## Case B: ACL Half-Reconnect (Phone reconnects baseband but skips A2DP)
//! When a phone reconnects after leaving and re-entering radio range, `BlueZ` frequently
//! completes the baseband ACL connection (`Connected == true`, `ServicesResolved == true`),
//! but neither `BlueZ` acquires a `MediaTransport1` nor does `bluealsad` expose a PCM.
//! The phone displays "Connected", but no audio routing exists.
//!
//! **Recovery:**
//! - **Stage 1 (Debounced to tick 2 / 6s):** Calls `org.bluez.Device1.ConnectProfile(A2DP_SOURCE)`
//!   non-disruptively without interrupting existing baseband or AVRCP channels.
//! - **Stage 2 (Escalation at tick 4 / 12s):** If `ConnectProfile` failed to establish both
//!   `MediaTransport1` and the `BlueALSA` PCM within two additional ticks, cycles the baseband
//!   connection (`Disconnect()` $\to$ settle delay $\to$ `Connect()`).
//!
//! # Architecture & Third-Party Audit Guardrails
//!
//! 1. **`bluez-alsa` v5.0.0 `ObjectManager` Root Path (`/org/bluealsa` vs `/`)**:
//!    `bluez-alsa` 4.x/5.x exposes standard `org.freedesktop.DBus.ObjectManager` on `/org/bluealsa`.
//!    The reconciler queries `/org/bluealsa` first. If `bluealsad` returns `UnknownObject` or
//!    `UnknownMethod`, it falls back to `/`.
//! 2. **`zbus` v5 Deserialization Safety**:
//!    In `zbus` v5 (`zvariant` v5), casting `Vec::<String>::try_from(owned_value)` on a D-Bus
//!    `as` string array inside an `a{sv}` map can fail when `OwnedValue` wraps `Value::Array`.
//!    [`extract_string_array`] safely unwraps both direct vectors and array variants.
//! 3. **Pairing & Voice Call Protection**:
//!    Devices undergoing active pairing (`paired == false` or `bonded == false`) and devices
//!    engaged in voice calls (HFP/HSP/SCO PCM active) legitimately do not expose an A2DP sink PCM.
//!    The reconciler ignores non-paired devices and active phone calls, preventing spurious kicks.
//! 4. **Permission & Restart Fault Immunity**:
//!    If `org.bluealsa` or `org.bluez` returns a D-Bus transport error (`AccessDenied`,
//!    `ServiceUnknown`, timeout), the tick is aborted. A failing D-Bus query is **never**
//!    interpreted as "0 PCMs registered", preventing disconnect loops during service restarts.
//! 5. **Anti-Thrashing Session Cap**:
//!    A maximum of [`MAX_HARD_RESETS_PER_SESSION`] (2) hard connection cycles is permitted per
//!    ACL session. If a device continues to fail after 2 hard resets, the reconciler backs off
//!    to avoid thrashing the phone battery.
//! 6. **Full Dependency Injection (`ReconcilerDbus`)**:
//!    All D-Bus interactions are abstracted behind the [`ReconcilerDbus`] trait, allowing
//!    100% hermetic unit and integration testing without a running D-Bus system bus.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::names::BusName;
use zbus::zvariant::{Array, OwnedValue, Value};

use crate::connection::{ManagedObjects, find_iface};

/// Standard Bluetooth A2DP Audio Source UUID (Audio Source role).
pub const UUID_A2DP_SOURCE: &str = "0000110a-0000-1000-8000-00805f9b34fb";

/// State polling and evaluation interval between reconciler ticks.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Bounded timeout for individual D-Bus queries or method calls to prevent async task stalls.
pub const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Settling delay after `Disconnect()` before calling `Connect()` to let `BlueZ` clean up ACL state.
pub const RECONNECT_SETTLE_DELAY: Duration = Duration::from_millis(1500);

/// Maximum hard connection resets (`Disconnect -> Connect`) allowed per ACL session before backing off.
pub const MAX_HARD_RESETS_PER_SESSION: u8 = 2;

/// Abstract interface for D-Bus operations performed by the Bluetooth audio reconciler.
///
/// Permits dependency injection so the complete reconciler lifecycle, timeout handling,
/// error fallbacks, and recovery actions can be tested hermetically without a live D-Bus daemon.
pub trait ReconcilerDbus: Send + Sync {
    /// Queries `org.bluez` at `/` for all managed objects.
    fn fetch_bluez_objects(&self) -> impl Future<Output = zbus::Result<ManagedObjects>> + Send;

    /// Queries `org.bluealsa` for managed objects.
    fn fetch_bluealsa_objects(&self) -> impl Future<Output = zbus::Result<ManagedObjects>> + Send;

    /// Calls `ConnectProfile(UUID)` on the specified `org.bluez.Device1` object path.
    fn connect_profile(
        &self,
        device_path: &str,
        uuid: &str,
    ) -> impl Future<Output = zbus::Result<()>> + Send;

    /// Calls `Disconnect()` on the specified `org.bluez.Device1` object path.
    fn disconnect(&self, device_path: &str) -> impl Future<Output = zbus::Result<()>> + Send;

    /// Calls `Connect()` on the specified `org.bluez.Device1` object path.
    fn connect(&self, device_path: &str) -> impl Future<Output = zbus::Result<()>> + Send;
}

impl ReconcilerDbus for Connection {
    async fn fetch_bluez_objects(&self) -> zbus::Result<ManagedObjects> {
        let bluez_mgr = ObjectManagerProxy::builder(self)
            .destination("org.bluez")?
            .path("/")?
            .build()
            .await?;
        bluez_mgr.get_managed_objects().await.map_err(Into::into)
    }

    async fn fetch_bluealsa_objects(&self) -> zbus::Result<ManagedObjects> {
        fetch_bluealsa_objects(self).await
    }

    async fn connect_profile(&self, device_path: &str, uuid: &str) -> zbus::Result<()> {
        call_device_method(self, device_path, "ConnectProfile", &(uuid,)).await
    }

    async fn disconnect(&self, device_path: &str) -> zbus::Result<()> {
        call_device_method(self, device_path, "Disconnect", &()).await
    }

    async fn connect(&self, device_path: &str) -> zbus::Result<()> {
        call_device_method(self, device_path, "Connect", &()).await
    }
}

impl<T: ReconcilerDbus + Sync> ReconcilerDbus for &T {
    fn fetch_bluez_objects(&self) -> impl Future<Output = zbus::Result<ManagedObjects>> + Send {
        (**self).fetch_bluez_objects()
    }

    fn fetch_bluealsa_objects(&self) -> impl Future<Output = zbus::Result<ManagedObjects>> + Send {
        (**self).fetch_bluealsa_objects()
    }

    fn connect_profile(
        &self,
        device_path: &str,
        uuid: &str,
    ) -> impl Future<Output = zbus::Result<()>> + Send {
        (**self).connect_profile(device_path, uuid)
    }

    fn disconnect(&self, device_path: &str) -> impl Future<Output = zbus::Result<()>> + Send {
        (**self).disconnect(device_path)
    }

    fn connect(&self, device_path: &str) -> impl Future<Output = zbus::Result<()>> + Send {
        (**self).connect(device_path)
    }
}

/// Internal shared state for [`MockReconcilerDbus`] and [`MockReconcilerController`].
#[derive(Debug, Default)]
struct MockReconcilerState {
    bluez_result: Option<zbus::Result<ManagedObjects>>,
    bluealsa_result: Option<zbus::Result<ManagedObjects>>,
    connect_profile_result: Option<zbus::Result<()>>,
    disconnect_result: Option<zbus::Result<()>>,
    connect_result: Option<zbus::Result<()>>,
    connect_profile_calls: Vec<(Box<str>, Box<str>)>,
    disconnect_calls: Vec<Box<str>>,
    connect_calls: Vec<Box<str>>,
}

/// Controller handle for injecting responses and inspecting invocations on [`MockReconcilerDbus`].
#[derive(Debug, Clone)]
pub struct MockReconcilerController {
    state: Arc<Mutex<MockReconcilerState>>,
}

#[expect(clippy::missing_panics_doc, reason = "Internal mutex locks in mock")]
impl MockReconcilerController {
    /// Sets the managed objects to return on the next `fetch_bluez_objects` call.
    pub fn set_bluez_objects(&self, objs: ManagedObjects) {
        self.state.lock().unwrap().bluez_result = Some(Ok(objs));
    }

    /// Sets the error to return on the next `fetch_bluez_objects` call.
    pub fn set_bluez_error(&self, err: zbus::Error) {
        self.state.lock().unwrap().bluez_result = Some(Err(err));
    }

    /// Sets the managed objects to return on the next `fetch_bluealsa_objects` call.
    pub fn set_bluealsa_objects(&self, objs: ManagedObjects) {
        self.state.lock().unwrap().bluealsa_result = Some(Ok(objs));
    }

    /// Sets the error to return on the next `fetch_bluealsa_objects` call.
    pub fn set_bluealsa_error(&self, err: zbus::Error) {
        self.state.lock().unwrap().bluealsa_result = Some(Err(err));
    }

    /// Sets the result to return on subsequent `connect_profile` calls.
    pub fn set_connect_profile_result(&self, res: zbus::Result<()>) {
        self.state.lock().unwrap().connect_profile_result = Some(res);
    }

    /// Sets the result to return on subsequent `disconnect` calls.
    pub fn set_disconnect_result(&self, res: zbus::Result<()>) {
        self.state.lock().unwrap().disconnect_result = Some(res);
    }

    /// Sets the result to return on subsequent `connect` calls.
    pub fn set_connect_result(&self, res: zbus::Result<()>) {
        self.state.lock().unwrap().connect_result = Some(res);
    }

    /// Returns all recorded `(device_path, uuid)` arguments passed to `connect_profile`.
    #[must_use]
    pub fn connect_profile_calls(&self) -> Vec<(Box<str>, Box<str>)> {
        self.state.lock().unwrap().connect_profile_calls.clone()
    }

    /// Returns all recorded `device_path` arguments passed to `disconnect`.
    #[must_use]
    pub fn disconnect_calls(&self) -> Vec<Box<str>> {
        self.state.lock().unwrap().disconnect_calls.clone()
    }

    /// Returns all recorded `device_path` arguments passed to `connect`.
    #[must_use]
    pub fn connect_calls(&self) -> Vec<Box<str>> {
        self.state.lock().unwrap().connect_calls.clone()
    }
}

/// Hermetic mock implementation of [`ReconcilerDbus`] for deterministic testing.
#[derive(Debug, Default, Clone)]
pub struct MockReconcilerDbus {
    state: Arc<Mutex<MockReconcilerState>>,
}

impl MockReconcilerDbus {
    /// Creates a mocked reconciler D-Bus backend along with its fault-injection controller (M-MOCKABLE-SYSCALLS).
    #[must_use]
    pub fn new_mocked() -> (Self, MockReconcilerController) {
        let state = Arc::new(Mutex::new(MockReconcilerState::default()));
        (
            Self {
                state: Arc::clone(&state),
            },
            MockReconcilerController { state },
        )
    }

    /// Creates a new empty mock reconciler.
    #[must_use]
    pub fn new() -> Self {
        Self::new_mocked().0
    }
}

#[expect(
    clippy::unused_async_trait_impl,
    reason = "Mock satisfies async trait without await"
)]
impl ReconcilerDbus for MockReconcilerDbus {
    async fn fetch_bluez_objects(&self) -> zbus::Result<ManagedObjects> {
        let mut guard = self.state.lock().unwrap();
        guard
            .bluez_result
            .take()
            .unwrap_or_else(|| Ok(ManagedObjects::new()))
    }

    async fn fetch_bluealsa_objects(&self) -> zbus::Result<ManagedObjects> {
        let mut guard = self.state.lock().unwrap();
        guard
            .bluealsa_result
            .take()
            .unwrap_or_else(|| Ok(ManagedObjects::new()))
    }

    async fn connect_profile(&self, device_path: &str, uuid: &str) -> zbus::Result<()> {
        let mut guard = self.state.lock().unwrap();
        guard
            .connect_profile_calls
            .push((device_path.into(), uuid.into()));
        guard.connect_profile_result.clone().unwrap_or(Ok(()))
    }

    async fn disconnect(&self, device_path: &str) -> zbus::Result<()> {
        let mut guard = self.state.lock().unwrap();
        guard.disconnect_calls.push(device_path.into());
        guard.disconnect_result.clone().unwrap_or(Ok(()))
    }

    async fn connect(&self, device_path: &str) -> zbus::Result<()> {
        let mut guard = self.state.lock().unwrap();
        guard.connect_calls.push(device_path.into());
        guard.connect_result.clone().unwrap_or(Ok(()))
    }
}

/// Divergence tracking state for an individual paired Bluetooth device.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct DeviceWatchState {
    /// Number of consecutive polling ticks this device has exhibited transport divergence.
    pub divergence_ticks: u32,
    /// Number of `Disconnect -> Connect` hard cycles attempted during this connection session.
    pub hard_resets_attempted: u8,
}

/// Reconciler device snapshot evaluated at a single polling tick.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Hardware state snapshot flags"
)]
pub struct BluezDeviceSnapshot {
    /// D-Bus object path of the `org.bluez.Device1` (e.g. `/org/bluez/hci1/dev_30_E0_44_67_38_A2`).
    pub path: Box<str>,
    /// Baseband ACL connection state from `org.bluez.Device1.Connected`.
    pub connected: bool,
    /// Pairing state from `org.bluez.Device1.Paired`.
    pub paired: bool,
    /// Bonded state from `org.bluez.Device1.Bonded`.
    pub bonded: bool,
    /// GATT/SDP service resolution state from `org.bluez.Device1.ServicesResolved`.
    pub services_resolved: bool,
    /// True if the device advertises the standard A2DP Audio Source UUID.
    pub supports_a2dp_source: bool,
    /// True if `BlueZ` currently holds an active streaming transport (`/org/bluez/hciX/dev_XX/fd*`).
    pub has_bluez_transport: bool,
    /// True if `bluez-alsa` exposes an active `org.bluealsa.PCM1` for this device.
    pub has_bluealsa_pcm: bool,
    /// True if the device is currently in an active HFP/HSP/SCO phone call PCM.
    pub has_voice_call_pcm: bool,
}

/// Action decided by the reconciler to heal transport state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Request non-disruptive A2DP profile connection via `org.bluez.Device1.ConnectProfile`.
    ConnectA2dpProfile {
        /// `BlueZ` device object path.
        device_path: Box<str>,
    },
    /// Hard reset the baseband ACL connection by calling `Disconnect`, settling, and `Connect`.
    CycleConnection {
        /// `BlueZ` device object path.
        device_path: Box<str>,
        /// Explanatory reason for the connection cycle.
        reason: DisconnectReason,
    },
}

/// Diagnostic reason explaining why a connection cycle was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Case A: `BlueZ` has an orphaned `MediaTransport1` (`fd*`), but `bluealsad` has no PCM.
    OrphanedBluezTransport,
    /// Case B Stage 2: ACL half-reconnect where `ConnectProfile` failed to create a transport.
    AclHalfReconnect,
}

/// Result of a single reconciler tick execution.
#[derive(Debug, PartialEq, Eq)]
pub enum TickOutcome {
    /// Divergence or health evaluation completed; actions executed.
    ActionsExecuted(usize),
    /// Tick skipped due to D-Bus query error.
    SkippedQueryError(Box<str>),
    /// Tick skipped due to D-Bus call timeout.
    SkippedTimeout,
}

/// Spawns the verified `BlueZ 5.82` ↔ `bluez-alsa v5.0.0` state reconciler task.
///
/// Runs continuously in the background, polling D-Bus every [`POLL_INTERVAL`] (3s).
/// All D-Bus calls are bounded by [`DBUS_CALL_TIMEOUT`] (2s) so a hung daemon never
/// stalls the tokio runtime.
///
/// # Invariant & Non-Destructive Operation
/// If `org.bluealsa` or `org.bluez` returns a D-Bus error (`AccessDenied`, `ServiceUnknown`,
/// timeout), the tick is skipped immediately. Recovery is NEVER attempted when D-Bus state
/// cannot be authoritatively queried.
#[must_use]
pub fn spawn_bt_transport_reconciler(conn: Connection) -> tokio::task::JoinHandle<()> {
    spawn_bt_transport_reconciler_with(conn, POLL_INTERVAL, RECONNECT_SETTLE_DELAY)
}

/// Spawns the reconciler with custom intervals and any [`ReconcilerDbus`] implementation.
#[must_use]
pub fn spawn_bt_transport_reconciler_with<D: ReconcilerDbus + 'static>(
    dbus: D,
    poll_interval: Duration,
    settle_delay: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_reconciler_loop_with(dbus, poll_interval, settle_delay).await;
    })
}

/// Parameterized reconciler loop supporting dependency injection for testing.
pub async fn run_reconciler_loop_with<D: ReconcilerDbus>(
    dbus: D,
    poll_interval: Duration,
    settle_delay: Duration,
) {
    let mut tracked: HashMap<Box<str>, DeviceWatchState> = HashMap::new();
    tracing::info!(
        "Bluetooth transport state reconciler started (BlueZ 5.82 <-> bluez-alsa v5.0.0)"
    );

    loop {
        sleep(poll_interval).await;
        let _ = reconciler_tick(&dbus, &mut tracked, settle_delay).await;
    }
}

/// Executes a single tick of the transport state reconciler.
///
/// Queries snapshots, reconciles divergence tracking, executes decided actions,
/// and returns the [`TickOutcome`].
pub async fn reconciler_tick<D: ReconcilerDbus, S: BuildHasher>(
    dbus: &D,
    tracked: &mut HashMap<Box<str>, DeviceWatchState, S>,
    settle_delay: Duration,
) -> TickOutcome {
    let snapshots = match timeout(DBUS_CALL_TIMEOUT, collect_snapshots(dbus)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "BT transport reconciler skipped tick (D-Bus query error)");
            return TickOutcome::SkippedQueryError(e.to_string().into());
        }
        Err(_elapsed) => {
            tracing::warn!(
                "BT transport reconciler skipped tick (D-Bus call timed out after {DBUS_CALL_TIMEOUT:?})"
            );
            return TickOutcome::SkippedTimeout;
        }
    };

    let actions = reconcile_snapshots(tracked, &snapshots);
    let count = actions.len();
    execute_reconcile_actions_with_settle(tracked, dbus, &actions, settle_delay).await;
    TickOutcome::ActionsExecuted(count)
}

/// Queries `BlueZ` and `BlueALSA` over D-Bus to construct per-device snapshots.
///
/// # Errors
/// Returns a [`zbus::Error`] if either D-Bus daemon fails to respond or denies access.
/// Propagates errors via `?`: if either daemon fails to respond or denies access,
/// the tick aborts so recovery is not triggered on partial or ambiguous information.
pub async fn collect_snapshots<D: ReconcilerDbus>(
    dbus: &D,
) -> zbus::Result<Vec<BluezDeviceSnapshot>> {
    let bluez_objs = dbus.fetch_bluez_objects().await?;

    // Fast path: if there are no connected devices in BlueZ, skip querying org.bluealsa.
    let has_connected_device = bluez_objs.values().any(|ifaces| {
        find_iface(ifaces, "org.bluez.Device1")
            .and_then(|props| extract_bool(props, "Connected"))
            .unwrap_or(false)
    });

    let bluealsa_objs = if has_connected_device {
        dbus.fetch_bluealsa_objects().await?
    } else {
        ManagedObjects::new()
    };

    Ok(build_snapshots(&bluez_objs, &bluealsa_objs))
}

/// Pure deterministic builder mapping raw D-Bus `GetManagedObjects` structures into
/// high-level [`BluezDeviceSnapshot`] items.
///
/// 100% unit-testable without requiring live D-Bus or physical Bluetooth hardware.
#[must_use]
pub fn build_snapshots(
    bluez_objs: &ManagedObjects,
    bluealsa_objs: &ManagedObjects,
) -> Vec<BluezDeviceSnapshot> {
    // 1. Identify all BlueZ device paths that currently own an A2DP MediaTransport1 (/fd*)
    let mut devices_with_fd = HashSet::new();
    for (path, ifaces) in bluez_objs {
        if let Some(transport_props) = find_iface(ifaces, "org.bluez.MediaTransport1")
            && let Some((dev_prefix, fd_suffix)) = path.as_str().rsplit_once('/')
            && fd_suffix.starts_with("fd")
        {
            let is_a2dp = extract_str(transport_props, "UUID").is_none_or(|u| {
                u.eq_ignore_ascii_case(UUID_A2DP_SOURCE) || u.contains("110a") || u.contains("110b")
            });
            if is_a2dp {
                devices_with_fd.insert(dev_prefix.to_string());
            }
        }
    }

    // 2. Identify all BlueZ device paths that have an active A2DP sink PCM or voice call PCM in org.bluealsa
    let mut devices_with_pcm = HashSet::new();
    let mut devices_with_voice = HashSet::new();

    for (path, ifaces) in bluealsa_objs {
        let Some(pcm_props) = find_iface(ifaces, "org.bluealsa.PCM1") else {
            continue;
        };

        // Determine the associated BlueZ device path:
        // Priority 1: "Device" object path property on org.bluealsa.PCM1
        // Priority 2: Extract "/org/bluealsa/hciX/dev_XX_..." prefix -> "/org/bluez/hciX/dev_XX_..."
        let dev_path = extract_object_path(pcm_props, "Device").or_else(|| {
            let s = path.as_str().strip_prefix("/org/bluealsa/")?;
            let idx = s.find("/dev_")?;
            let dev_suffix = &s[idx + 1..];
            let dev_len = dev_suffix.find('/').unwrap_or(dev_suffix.len());
            let subpath = &s[..idx + 1 + dev_len];
            Some(format!("/org/bluez/{subpath}"))
        });

        let Some(dev_path) = dev_path else {
            tracing::warn!(
                pcm_path = %path.as_str(),
                "Failed to resolve device path from BlueALSA PCM object"
            );
            continue;
        };

        let path_str = path.as_str();
        let transport_str = extract_str(pcm_props, "Transport").unwrap_or("");

        // Check for A2DP sink (audio from phone playing through speaker)
        if path_str.contains("a2dp") || transport_str.contains("A2DP") {
            devices_with_pcm.insert(dev_path.clone());
        }

        // Check for active HFP/HSP/SCO voice call
        if path_str.contains("hfp")
            || path_str.contains("hsp")
            || path_str.contains("sco")
            || transport_str.starts_with("HFP")
            || transport_str.starts_with("HSP")
            || transport_str.starts_with("SCO")
        {
            devices_with_voice.insert(dev_path);
        }
    }

    // 3. Build per-device snapshots from org.bluez.Device1 objects
    let mut snapshots = Vec::new();
    for (path, ifaces) in bluez_objs {
        let Some(dev_props) = find_iface(ifaces, "org.bluez.Device1") else {
            continue;
        };

        let path_str = path.as_str();
        let connected = extract_bool(dev_props, "Connected").unwrap_or(false);
        let paired = extract_bool(dev_props, "Paired").unwrap_or(false);
        // Bonded indicates permanent bonding keys exist; defaults to paired if property is absent
        let bonded = extract_bool(dev_props, "Bonded").unwrap_or(paired);
        let services_resolved = extract_bool(dev_props, "ServicesResolved").unwrap_or(false);
        let supports_a2dp_source = extract_string_array(dev_props, "UUIDs")
            .iter()
            .any(|u| u.eq_ignore_ascii_case(UUID_A2DP_SOURCE));

        snapshots.push(BluezDeviceSnapshot {
            has_bluez_transport: devices_with_fd.contains(path_str),
            has_bluealsa_pcm: devices_with_pcm.contains(path_str),
            has_voice_call_pcm: devices_with_voice.contains(path_str),
            path: path_str.into(),
            connected,
            paired,
            bonded,
            services_resolved,
            supports_a2dp_source,
        });
    }

    snapshots.sort_by(|a, b| a.path.cmp(&b.path));
    snapshots
}

/// Pure deterministic state reconciliation function.
///
/// Evaluates the current snapshots against historical divergence tracking, updating
/// `tracked` state and returning the list of [`ReconcileAction`] commands to execute.
///
/// 100% unit-testable without D-Bus.
#[must_use]
pub fn reconcile_snapshots<S: BuildHasher>(
    tracked: &mut HashMap<Box<str>, DeviceWatchState, S>,
    snapshots: &[BluezDeviceSnapshot],
) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();
    let current_paths: HashSet<&str> = snapshots.iter().map(|s| s.path.as_ref()).collect();

    // Prune devices that were removed from BlueZ entirely
    tracked.retain(|path, _| current_paths.contains(path.as_ref()));

    for dev in snapshots {
        // Never interfere with unpaired/unbonded devices (active pairing) or non-A2DP devices
        if !dev.paired || !dev.bonded || !dev.supports_a2dp_source {
            tracked.remove(&dev.path);
            continue;
        }

        // When device is temporarily disconnected, pause divergence tracking without resetting the session cap
        if !dev.connected {
            if let Some(st) = tracked.get_mut(&dev.path) {
                st.divergence_ticks = 0;
            }
            continue;
        }

        // Voice call guard: during HFP/HSP/SCO call, A2DP sink PCM may be inactive
        if dev.has_voice_call_pcm {
            if let Some(st) = tracked.get_mut(&dev.path) {
                st.divergence_ticks = 0;
            }
            continue;
        }

        // Healthy state: bluealsad exposes the A2DP sink PCM
        if dev.has_bluealsa_pcm {
            if let Some(st) = tracked.get_mut(&dev.path) {
                if st.divergence_ticks > 0 {
                    tracing::info!(device = %dev.path, "BT audio & volume path healthy; divergence cleared");
                }
                st.divergence_ticks = 0;
                st.hard_resets_attempted = 0;
            }
            continue;
        }

        let state = tracked.entry(dev.path.clone()).or_default();
        if let Some(action) = evaluate_divergent_device(state, dev) {
            actions.push(action);
        }
    }

    actions
}

/// Evaluates divergence state for an actively connected, divergent device and returns any healing action.
fn evaluate_divergent_device(
    state: &mut DeviceWatchState,
    dev: &BluezDeviceSnapshot,
) -> Option<ReconcileAction> {
    state.divergence_ticks = state.divergence_ticks.saturating_add(1);
    if state.divergence_ticks == 1 {
        tracing::warn!(
            device = %dev.path,
            tick = state.divergence_ticks,
            services_resolved = dev.services_resolved,
            bluez_fd = dev.has_bluez_transport,
            bluealsa_pcm = dev.has_bluealsa_pcm,
            "BT transport divergence detected"
        );
    } else {
        tracing::debug!(
            device = %dev.path,
            tick = state.divergence_ticks,
            services_resolved = dev.services_resolved,
            bluez_fd = dev.has_bluez_transport,
            bluealsa_pcm = dev.has_bluealsa_pcm,
            "BT transport divergence persisting"
        );
    }

    // CASE A: Orphaned fd* in BlueZ.
    // BlueZ holds MediaTransport1, but bluealsad has dropped the device / has 0 PCMs.
    // ConnectProfile is ineffective; we must Disconnect -> Settle -> Connect.
    if dev.has_bluez_transport && !dev.has_bluealsa_pcm && state.divergence_ticks >= 2 {
        if state.hard_resets_attempted >= MAX_HARD_RESETS_PER_SESSION {
            tracing::debug!(
                device = %dev.path,
                hard_resets = state.hard_resets_attempted,
                "Device reached maximum hard resets for this ACL session; backing off"
            );
            return None;
        }
        tracing::warn!(
            device = %dev.path,
            "Orphaned BlueZ MediaTransport1 detected; scheduling Disconnect -> Connect cycle"
        );
        state.divergence_ticks = 0;
        return Some(ReconcileAction::CycleConnection {
            device_path: dev.path.clone(),
            reason: DisconnectReason::OrphanedBluezTransport,
        });
    }

    // CASE B: ACL Half-Reconnect.
    // Connected=true, no BlueZ fd* transport, and no BlueALSA PCM.
    if !dev.has_bluez_transport && !dev.has_bluealsa_pcm {
        if state.divergence_ticks == 2 {
            tracing::info!(
                device = %dev.path,
                "Stage 1 recovery: requesting ConnectProfile(A2DP_SOURCE)"
            );
            return Some(ReconcileAction::ConnectA2dpProfile {
                device_path: dev.path.clone(),
            });
        }
        if state.divergence_ticks >= 4 {
            if state.hard_resets_attempted >= MAX_HARD_RESETS_PER_SESSION {
                tracing::debug!(
                    device = %dev.path,
                    hard_resets = state.hard_resets_attempted,
                    "Device reached maximum hard resets for this ACL session; backing off"
                );
                return None;
            }
            tracing::warn!(
                device = %dev.path,
                "Stage 2 recovery: ConnectProfile failed to establish transport; scheduling Disconnect -> Connect cycle"
            );
            state.divergence_ticks = 0;
            return Some(ReconcileAction::CycleConnection {
                device_path: dev.path.clone(),
                reason: DisconnectReason::AclHalfReconnect,
            });
        }
    }

    None
}

/// Executes decided actions with a specified settle delay.
///
/// Increments `hard_resets_attempted` on `tracked` whenever a `Disconnect -> Connect`
/// cycle is attempted (governing both successful and failing attempts to prevent thrashing).
pub async fn execute_reconcile_actions_with_settle<D: ReconcilerDbus, S: BuildHasher>(
    tracked: &mut HashMap<Box<str>, DeviceWatchState, S>,
    dbus: &D,
    actions: &[ReconcileAction],
    settle_delay: Duration,
) {
    for action in actions {
        match action {
            ReconcileAction::ConnectA2dpProfile { device_path } => {
                tracing::info!(device = %device_path, "Calling ConnectProfile(A2DP_SOURCE)");
                match timeout(
                    DBUS_CALL_TIMEOUT,
                    dbus.connect_profile(device_path, UUID_A2DP_SOURCE),
                )
                .await
                {
                    Ok(Ok(())) => {
                        tracing::info!(device = %device_path, "ConnectProfile(A2DP_SOURCE) succeeded");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(device = %device_path, error = %e, "ConnectProfile(A2DP_SOURCE) returned D-Bus error");
                    }
                    Err(_elapsed) => {
                        tracing::warn!(device = %device_path, "ConnectProfile(A2DP_SOURCE) timed out after {DBUS_CALL_TIMEOUT:?}");
                    }
                }
            }
            ReconcileAction::CycleConnection {
                device_path,
                reason,
            } => {
                tracing::warn!(
                    device = %device_path,
                    reason = ?reason,
                    "Executing connection cycle (Disconnect -> Settle -> Connect)"
                );
                cycle_device_connection_with_settle(dbus, device_path, settle_delay).await;
                let state = tracked.entry(device_path.clone()).or_default();
                state.hard_resets_attempted = state.hard_resets_attempted.saturating_add(1);
            }
        }
    }
}

/// Queries `org.bluealsa` `ObjectManager` at `/org/bluealsa` (standard for v4/v5).
///
/// # Errors
/// Returns a [`zbus::Error`] if D-Bus communication with `org.bluealsa` fails with an
/// error other than `UnknownObject` or `UnknownMethod`.
pub async fn fetch_bluealsa_objects(conn: &Connection) -> zbus::Result<ManagedObjects> {
    let dest: BusName<'_> = "org.bluealsa".try_into()?;
    let mgr = ObjectManagerProxy::builder(conn)
        .destination(dest.clone())?
        .path("/org/bluealsa")?
        .build()
        .await?;

    match mgr.get_managed_objects().await {
        Ok(objs) => Ok(objs),
        Err(e) if is_unknown_object_or_method(&e) => {
            tracing::debug!(
                "org.bluealsa ObjectManager not mounted on /org/bluealsa; checking root path '/'"
            );
            let root_mgr = ObjectManagerProxy::builder(conn)
                .destination(dest)?
                .path("/")?
                .build()
                .await?;
            root_mgr.get_managed_objects().await.map_err(Into::into)
        }
        Err(e) => Err(e.into()),
    }
}

/// Detects `UnknownObject`, `UnknownMethod`, or `UnknownInterface` D-Bus errors.
#[must_use]
pub fn is_unknown_object_or_method(err: &zbus::fdo::Error) -> bool {
    matches!(
        err,
        zbus::fdo::Error::UnknownInterface(_)
            | zbus::fdo::Error::UnknownObject(_)
            | zbus::fdo::Error::UnknownMethod(_)
    ) || {
        let s = err.to_string();
        s.contains("UnknownInterface") || s.contains("UnknownObject") || s.contains("UnknownMethod")
    }
}

/// Extracts a boolean property safely from a property dictionary.
#[must_use]
pub fn extract_bool<S: BuildHasher>(
    props: &HashMap<String, OwnedValue, S>,
    key: &str,
) -> Option<bool> {
    props.get(key).and_then(|v| {
        bool::try_from(v).ok().or_else(|| match &**v {
            Value::Bool(b) => Some(*b),
            _ => v.downcast_ref::<bool>().ok(),
        })
    })
}

/// Extracts a string property safely from a property dictionary.
#[must_use]
pub fn extract_str<'a, S: BuildHasher>(
    props: &'a HashMap<String, OwnedValue, S>,
    key: &str,
) -> Option<&'a str> {
    props.get(key).and_then(|v| match &**v {
        Value::Str(s) => Some(s.as_str()),
        _ => v.downcast_ref::<&str>().ok(),
    })
}

/// Extracts an object path property safely from a property dictionary.
#[must_use]
pub fn extract_object_path<S: BuildHasher>(
    props: &HashMap<String, OwnedValue, S>,
    key: &str,
) -> Option<String> {
    props.get(key).and_then(|v| match &**v {
        Value::ObjectPath(p) => Some(p.as_str().to_string()),
        Value::Str(s) => Some(s.as_str().to_string()),
        _ => v.downcast_ref::<&str>().ok().map(|s| (*s).to_string()),
    })
}

/// Extracts a string array safely handling both `zvariant::Array` and nested vectors.
#[must_use]
pub fn extract_string_array<S: BuildHasher>(
    props: &HashMap<String, OwnedValue, S>,
    key: &str,
) -> Vec<String> {
    let Some(val) = props.get(key) else {
        return Vec::new();
    };

    if let Ok(vec) = Vec::<String>::try_from(val.clone()) {
        return vec;
    }
    if let Ok(arr) = Array::try_from(val.clone()) {
        return arr
            .iter()
            .filter_map(|item| match item {
                Value::Str(s) => Some(s.as_str().to_string()),
                _ => item.downcast_ref::<&str>().ok().map(|s| (*s).to_string()),
            })
            .collect();
    }
    Vec::new()
}

/// Cycles a Bluetooth connection with a specified settle delay.
///
/// Returns `true` if both `Disconnect()` and `Connect()` succeeded within [`DBUS_CALL_TIMEOUT`].
pub async fn cycle_device_connection_with_settle<D: ReconcilerDbus>(
    dbus: &D,
    device_path: &str,
    settle_delay: Duration,
) -> bool {
    let disconnect_ok = match timeout(DBUS_CALL_TIMEOUT, dbus.disconnect(device_path)).await {
        Ok(Ok(())) => {
            tracing::info!(device = %device_path, "Disconnect call succeeded");
            true
        }
        Ok(Err(e)) => {
            tracing::warn!(device = %device_path, error = %e, "Disconnect returned D-Bus error");
            false
        }
        Err(_elapsed) => {
            tracing::warn!(device = %device_path, "Disconnect timed out after {DBUS_CALL_TIMEOUT:?}");
            false
        }
    };

    sleep(settle_delay).await;

    let connect_ok = match timeout(DBUS_CALL_TIMEOUT, dbus.connect(device_path)).await {
        Ok(Ok(())) => {
            tracing::info!(device = %device_path, "Connect call succeeded");
            true
        }
        Ok(Err(e)) => {
            tracing::warn!(device = %device_path, error = %e, "Connect returned D-Bus error");
            false
        }
        Err(_elapsed) => {
            tracing::warn!(device = %device_path, "Connect timed out after {DBUS_CALL_TIMEOUT:?}");
            false
        }
    };

    disconnect_ok && connect_ok
}

/// Helper invoking a method on `org.bluez.Device1` over D-Bus.
async fn call_device_method<B: serde::Serialize + zbus::zvariant::DynamicType>(
    conn: &Connection,
    device_path: &str,
    method: &str,
    body: &B,
) -> zbus::Result<()> {
    conn.call_method(
        Some("org.bluez"),
        device_path,
        Some("org.bluez.Device1"),
        method,
        body,
    )
    .await?;
    Ok(())
}
