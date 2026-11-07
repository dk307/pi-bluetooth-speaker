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
    connect_profile_calls: Vec<(String, String)>,
    disconnect_calls: Vec<String>,
    connect_calls: Vec<String>,
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
    pub fn connect_profile_calls(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().connect_profile_calls.clone()
    }

    /// Returns all recorded `device_path` arguments passed to `disconnect`.
    #[must_use]
    pub fn disconnect_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().disconnect_calls.clone()
    }

    /// Returns all recorded `device_path` arguments passed to `connect`.
    #[must_use]
    pub fn connect_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().connect_calls.clone()
    }
}

/// Hermetic mock implementation of [`ReconcilerDbus`] for deterministic testing.
#[derive(Debug, Default, Clone)]
pub struct MockReconcilerDbus {
    state: Arc<Mutex<MockReconcilerState>>,
}

#[expect(clippy::missing_panics_doc, reason = "Internal mutex locks in mock")]
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
    pub fn connect_profile_calls(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().connect_profile_calls.clone()
    }

    /// Returns all recorded `device_path` arguments passed to `disconnect`.
    #[must_use]
    pub fn disconnect_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().disconnect_calls.clone()
    }

    /// Returns all recorded `device_path` arguments passed to `connect`.
    #[must_use]
    pub fn connect_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().connect_calls.clone()
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
            .push((device_path.to_string(), uuid.to_string()));
        match &guard.connect_profile_result {
            Some(Ok(())) | None => Ok(()),
            Some(Err(e)) => Err(zbus::Error::Failure(e.to_string())),
        }
    }

    async fn disconnect(&self, device_path: &str) -> zbus::Result<()> {
        let mut guard = self.state.lock().unwrap();
        guard.disconnect_calls.push(device_path.to_string());
        match &guard.disconnect_result {
            Some(Ok(())) | None => Ok(()),
            Some(Err(e)) => Err(zbus::Error::Failure(e.to_string())),
        }
    }

    async fn connect(&self, device_path: &str) -> zbus::Result<()> {
        let mut guard = self.state.lock().unwrap();
        guard.connect_calls.push(device_path.to_string());
        match &guard.connect_result {
            Some(Ok(())) | None => Ok(()),
            Some(Err(e)) => Err(zbus::Error::Failure(e.to_string())),
        }
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
    pub path: String,
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
        device_path: String,
    },
    /// Hard reset the baseband ACL connection by calling `Disconnect`, settling, and `Connect`.
    CycleConnection {
        /// `BlueZ` device object path.
        device_path: String,
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
    SkippedQueryError(String),
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

/// Core asynchronous reconciler loop backed by a live D-Bus connection.
pub async fn run_reconciler_loop(conn: Connection) {
    run_reconciler_loop_with(conn, POLL_INTERVAL, RECONNECT_SETTLE_DELAY).await;
}

/// Parameterized reconciler loop supporting dependency injection for testing.
pub async fn run_reconciler_loop_with<D: ReconcilerDbus>(
    dbus: D,
    poll_interval: Duration,
    settle_delay: Duration,
) {
    let mut tracked: HashMap<String, DeviceWatchState> = HashMap::new();
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
    tracked: &mut HashMap<String, DeviceWatchState, S>,
    settle_delay: Duration,
) -> TickOutcome {
    let snapshots = match timeout(DBUS_CALL_TIMEOUT, collect_snapshots(dbus)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "BT transport reconciler skipped tick (D-Bus query error)");
            return TickOutcome::SkippedQueryError(e.to_string());
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
    execute_reconcile_actions_with_settle(dbus, &actions, settle_delay).await;
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
    // 1. Identify all BlueZ device paths that currently own a MediaTransport1 (/fd*)
    let mut devices_with_fd = HashSet::new();
    for (path, ifaces) in bluez_objs {
        if find_iface(ifaces, "org.bluez.MediaTransport1").is_some()
            && let Some((dev_prefix, fd_suffix)) = path.as_str().rsplit_once('/')
            && fd_suffix.starts_with("fd")
        {
            devices_with_fd.insert(dev_prefix.to_string());
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
        // Priority 2: Map "/org/bluealsa/hciX/dev_XX_..." -> "/org/bluez/hciX/dev_XX_..."
        let dev_path = extract_object_path(pcm_props, "Device").or_else(|| {
            let s = path.as_str();
            let stripped = s
                .strip_suffix("/a2dpsnk/source")
                .or_else(|| s.strip_suffix("/a2dpsnk/sink"))
                .or_else(|| s.strip_suffix("/hfphf/source"))
                .or_else(|| s.strip_suffix("/hfphf/sink"))
                .or_else(|| s.strip_suffix("/hfp_hf/source"))
                .or_else(|| s.strip_suffix("/hfp_hf/sink"))
                .or_else(|| s.strip_suffix("/hsphs/source"))
                .or_else(|| s.strip_suffix("/hsphs/sink"))
                .or_else(|| s.strip_suffix("/hsp_hs/source"))
                .or_else(|| s.strip_suffix("/hsp_hs/sink"))
                .or_else(|| s.strip_suffix("/sco/source"))
                .or_else(|| s.strip_suffix("/sco/sink"))
                .unwrap_or(s);
            stripped
                .strip_prefix("/org/bluealsa/")
                .map(|rest| format!("/org/bluez/{rest}"))
        });

        let Some(dev_path) = dev_path else {
            continue;
        };

        let path_str = path.as_str();
        let transport_str = extract_str(pcm_props, "Transport").unwrap_or("");

        // Check for A2DP sink (audio from phone playing through speaker)
        if path_str.ends_with("/a2dpsnk/source") || transport_str == "A2DP-sink" {
            devices_with_pcm.insert(dev_path.clone());
        }

        // Check for active HFP/HSP/SCO voice call
        if path_str.contains("/hfp")
            || path_str.contains("/hsp")
            || path_str.contains("/sco")
            || transport_str.starts_with("HFP")
            || transport_str.starts_with("HSP")
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

        let path_str = path.as_str().to_string();
        let connected = extract_bool(dev_props, "Connected").unwrap_or(false);
        let paired = extract_bool(dev_props, "Paired").unwrap_or(false);
        // Bonded indicates permanent bonding keys exist; defaults to paired if property is absent
        let bonded = extract_bool(dev_props, "Bonded").unwrap_or(paired);
        let services_resolved = extract_bool(dev_props, "ServicesResolved").unwrap_or(false);
        let supports_a2dp_source = extract_string_array(dev_props, "UUIDs")
            .iter()
            .any(|u| u.eq_ignore_ascii_case(UUID_A2DP_SOURCE));

        snapshots.push(BluezDeviceSnapshot {
            has_bluez_transport: devices_with_fd.contains(&path_str),
            has_bluealsa_pcm: devices_with_pcm.contains(&path_str),
            has_voice_call_pcm: devices_with_voice.contains(&path_str),
            path: path_str,
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
    tracked: &mut HashMap<String, DeviceWatchState, S>,
    snapshots: &[BluezDeviceSnapshot],
) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();
    let current_paths: HashSet<&str> = snapshots.iter().map(|s| s.path.as_str()).collect();

    // Prune devices that were removed from BlueZ entirely
    tracked.retain(|path, _| current_paths.contains(path.as_str()));

    for dev in snapshots {
        // Never interfere with unpaired/unbonded devices (active pairing) or non-A2DP devices
        if !dev.connected || !dev.paired || !dev.bonded || !dev.supports_a2dp_source {
            tracked.remove(&dev.path);
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
        if state.hard_resets_attempted >= MAX_HARD_RESETS_PER_SESSION {
            tracing::debug!(
                device = %dev.path,
                hard_resets = state.hard_resets_attempted,
                "Device reached maximum hard resets for this ACL session; backing off"
            );
            continue;
        }

        state.divergence_ticks = state.divergence_ticks.saturating_add(1);
        tracing::warn!(
            device = %dev.path,
            tick = state.divergence_ticks,
            services_resolved = dev.services_resolved,
            bluez_fd = dev.has_bluez_transport,
            bluealsa_pcm = dev.has_bluealsa_pcm,
            "BT transport divergence detected"
        );

        // CASE A: Orphaned fd* in BlueZ.
        // BlueZ holds MediaTransport1, but bluealsad has dropped the device / has 0 PCMs.
        // ConnectProfile is ineffective; we must Disconnect -> Settle -> Connect.
        if dev.has_bluez_transport && !dev.has_bluealsa_pcm && state.divergence_ticks >= 2 {
            tracing::warn!(
                device = %dev.path,
                "Orphaned BlueZ MediaTransport1 detected; scheduling Disconnect -> Connect cycle"
            );
            actions.push(ReconcileAction::CycleConnection {
                device_path: dev.path.clone(),
                reason: DisconnectReason::OrphanedBluezTransport,
            });
            state.divergence_ticks = 0;
            state.hard_resets_attempted = state.hard_resets_attempted.saturating_add(1);
            continue;
        }

        // CASE B: ACL Half-Reconnect.
        // Connected=true, no BlueZ fd* transport, and no BlueALSA PCM.
        if !dev.has_bluez_transport && !dev.has_bluealsa_pcm {
            if state.divergence_ticks == 2 {
                tracing::info!(
                    device = %dev.path,
                    "Stage 1 recovery: requesting ConnectProfile(A2DP_SOURCE)"
                );
                actions.push(ReconcileAction::ConnectA2dpProfile {
                    device_path: dev.path.clone(),
                });
            } else if state.divergence_ticks >= 4 {
                tracing::warn!(
                    device = %dev.path,
                    "Stage 2 recovery: ConnectProfile failed to establish transport; scheduling Disconnect -> Connect cycle"
                );
                actions.push(ReconcileAction::CycleConnection {
                    device_path: dev.path.clone(),
                    reason: DisconnectReason::AclHalfReconnect,
                });
                state.divergence_ticks = 0;
                state.hard_resets_attempted = state.hard_resets_attempted.saturating_add(1);
            }
        }
    }

    actions
}

/// Executes decided actions against `BlueZ` using the default settle delay.
pub async fn execute_reconcile_actions<D: ReconcilerDbus>(dbus: &D, actions: &[ReconcileAction]) {
    execute_reconcile_actions_with_settle(dbus, actions, RECONNECT_SETTLE_DELAY).await;
}

/// Executes decided actions with a specified settle delay.
pub async fn execute_reconcile_actions_with_settle<D: ReconcilerDbus>(
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
    if let Ok(arr) = <&Array<'_>>::try_from(val) {
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

/// Cycles a Bluetooth connection by invoking `Disconnect()`, sleeping for
/// [`RECONNECT_SETTLE_DELAY`], and invoking `Connect()`.
pub async fn cycle_device_connection<D: ReconcilerDbus>(dbus: &D, device_path: &str) {
    cycle_device_connection_with_settle(dbus, device_path, RECONNECT_SETTLE_DELAY).await;
}

/// Cycles a Bluetooth connection with a specified settle delay.
pub async fn cycle_device_connection_with_settle<D: ReconcilerDbus>(
    dbus: &D,
    device_path: &str,
    settle_delay: Duration,
) {
    match timeout(DBUS_CALL_TIMEOUT, dbus.disconnect(device_path)).await {
        Ok(Ok(())) => {
            tracing::info!(device = %device_path, "Disconnect call succeeded");
        }
        Ok(Err(e)) => {
            tracing::warn!(device = %device_path, error = %e, "Disconnect returned D-Bus error");
        }
        Err(_elapsed) => {
            tracing::warn!(device = %device_path, "Disconnect timed out after {DBUS_CALL_TIMEOUT:?}");
        }
    }

    sleep(settle_delay).await;

    match timeout(DBUS_CALL_TIMEOUT, dbus.connect(device_path)).await {
        Ok(Ok(())) => {
            tracing::info!(device = %device_path, "Connect call succeeded");
        }
        Ok(Err(e)) => {
            tracing::warn!(device = %device_path, error = %e, "Connect returned D-Bus error");
        }
        Err(_elapsed) => {
            tracing::warn!(device = %device_path, "Connect timed out after {DBUS_CALL_TIMEOUT:?}");
        }
    }
}

/// Helper invoking a method on `org.bluez.Device1` over D-Bus.
///
/// # Errors
/// Returns a [`zbus::Error`] if the remote D-Bus method invocation fails.
pub async fn call_device_method<B: serde::Serialize + zbus::zvariant::DynamicType>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::names::OwnedInterfaceName;
    use zbus::zvariant::OwnedObjectPath;

    fn to_val<T: Into<Value<'static>>>(v: T) -> OwnedValue {
        OwnedValue::try_from(v.into()).unwrap()
    }

    fn owned_iface(s: &str) -> OwnedInterfaceName {
        OwnedInterfaceName::try_from(s).expect("valid iface")
    }

    fn owned_path(s: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(s).expect("valid path")
    }

    #[test]
    fn extract_bool_handles_various_types() {
        let mut props = HashMap::new();
        props.insert("Connected".to_string(), to_val(true));
        props.insert("Paired".to_string(), to_val(false));
        props.insert("NonBool".to_string(), to_val("hello"));

        assert_eq!(extract_bool(&props, "Connected"), Some(true));
        assert_eq!(extract_bool(&props, "Paired"), Some(false));
        assert_eq!(extract_bool(&props, "NonBool"), None);
        assert_eq!(extract_bool(&props, "Missing"), None);
    }

    #[test]
    fn extract_str_and_object_path_coverage() {
        let mut props = HashMap::new();
        props.insert("Name".to_string(), to_val("Pixel"));
        props.insert("DevPath".to_string(), to_val(owned_path("/dev/test")));
        props.insert("NonStr".to_string(), to_val(42i32));

        assert_eq!(extract_str(&props, "Name"), Some("Pixel"));
        assert_eq!(extract_str(&props, "NonStr"), None);
        assert_eq!(extract_str(&props, "Missing"), None);

        assert_eq!(
            extract_object_path(&props, "DevPath"),
            Some("/dev/test".to_string())
        );
        assert_eq!(
            extract_object_path(&props, "Name"),
            Some("Pixel".to_string())
        );
        assert_eq!(extract_object_path(&props, "NonStr"), None);
        assert_eq!(extract_object_path(&props, "Missing"), None);
    }

    #[test]
    fn extract_string_array_handles_vec_and_array() {
        let mut props = HashMap::new();
        let list = vec!["item1".to_string(), "item2".to_string()];
        props.insert("List1".to_string(), to_val(list.clone()));

        let extracted = extract_string_array(&props, "List1");
        assert_eq!(extracted, list);

        let array_val = Value::Array(Array::from(vec![Value::from("alpha"), Value::from("beta")]));
        props.insert("List2".to_string(), to_val(array_val));
        let extracted2 = extract_string_array(&props, "List2");
        assert_eq!(extracted2, vec!["alpha".to_string(), "beta".to_string()]);

        assert_eq!(
            extract_string_array(&props, "Missing"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn build_snapshots_classifies_healthy_session() {
        let dev_path = "/org/bluez/hci1/dev_30_E0_44_67_38_A2";
        let mut bluez_objs = ManagedObjects::new();

        let mut dev_props = HashMap::new();
        dev_props.insert("Connected".to_string(), to_val(true));
        dev_props.insert("Paired".to_string(), to_val(true));
        dev_props.insert("Bonded".to_string(), to_val(true));
        dev_props.insert("ServicesResolved".to_string(), to_val(true));
        dev_props.insert("UUIDs".to_string(), to_val(vec![UUID_A2DP_SOURCE]));

        let mut dev_ifaces = HashMap::new();
        dev_ifaces.insert(owned_iface("org.bluez.Device1"), dev_props);
        bluez_objs.insert(owned_path(dev_path), dev_ifaces);

        // Add MediaTransport1 object
        let fd_path = format!("{dev_path}/fd1");
        let mut transport_ifaces = HashMap::new();
        transport_ifaces.insert(owned_iface("org.bluez.MediaTransport1"), HashMap::new());
        bluez_objs.insert(owned_path(&fd_path), transport_ifaces);

        // Add bluealsa PCM object
        let mut bluealsa_objs = ManagedObjects::new();
        let pcm_path = "/org/bluealsa/hci1/dev_30_E0_44_67_38_A2/a2dpsnk/source";
        let mut pcm_props = HashMap::new();
        pcm_props.insert("Transport".to_string(), to_val("A2DP-sink"));
        pcm_props.insert("Device".to_string(), to_val(owned_path(dev_path)));
        let mut pcm_ifaces = HashMap::new();
        pcm_ifaces.insert(owned_iface("org.bluealsa.PCM1"), pcm_props);
        bluealsa_objs.insert(owned_path(pcm_path), pcm_ifaces);

        let snapshots = build_snapshots(&bluez_objs, &bluealsa_objs);
        assert_eq!(snapshots.len(), 1);
        let s = &snapshots[0];
        assert_eq!(s.path, dev_path);
        assert!(s.connected);
        assert!(s.paired);
        assert!(s.bonded);
        assert!(s.services_resolved);
        assert!(s.supports_a2dp_source);
        assert!(s.has_bluez_transport);
        assert!(s.has_bluealsa_pcm);
        assert!(!s.has_voice_call_pcm);
    }

    #[test]
    fn reconcile_snapshots_case_a_orphaned_transport() {
        let dev_path = "/org/bluez/hci1/dev_orphan".to_string();
        let mut tracked = HashMap::new();

        let snapshot = BluezDeviceSnapshot {
            path: dev_path.clone(),
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: true,
            has_bluealsa_pcm: false, // orphaned fd1 in BlueZ
            has_voice_call_pcm: false,
        };

        // Tick 1: observed divergence, tick counter advances to 1, no action yet (debounce)
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 1);

        // Tick 2: divergence persists, triggers CycleConnection
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::CycleConnection {
                device_path: dev_path.clone(),
                reason: DisconnectReason::OrphanedBluezTransport,
            }
        );
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 0);
        assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 1);
    }

    #[test]
    fn reconcile_snapshots_case_b_acl_half_reconnect() {
        let dev_path = "/org/bluez/hci1/dev_half_conn".to_string();
        let mut tracked = HashMap::new();

        let snapshot = BluezDeviceSnapshot {
            path: dev_path.clone(),
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: false, // no transport
            has_bluealsa_pcm: false,    // no PCM
            has_voice_call_pcm: false,
        };

        // Tick 1 (3s): debounce
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);

        // Tick 2 (6s): Stage 1 - non-disruptive ConnectProfile(A2DP_SOURCE)
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::ConnectA2dpProfile {
                device_path: dev_path.clone(),
            }
        );
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 2);

        // Tick 3 (9s): debounce wait for transport to appear
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 3);

        // Tick 4 (12s): Stage 2 - escalation to Disconnect -> Connect cycle
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::CycleConnection {
                device_path: dev_path.clone(),
                reason: DisconnectReason::AclHalfReconnect,
            }
        );
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 0);
        assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 1);
    }

    #[test]
    fn reconcile_snapshots_ignores_unpaired_or_non_a2dp_devices() {
        let mut tracked = HashMap::new();

        let unpaired = BluezDeviceSnapshot {
            path: "/org/bluez/hci1/dev_unpaired".to_string(),
            connected: true,
            paired: false,
            bonded: false,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: false,
            has_bluealsa_pcm: false,
            has_voice_call_pcm: false,
        };

        let non_a2dp = BluezDeviceSnapshot {
            path: "/org/bluez/hci1/dev_keyboard".to_string(),
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: false,
            has_bluez_transport: false,
            has_bluealsa_pcm: false,
            has_voice_call_pcm: false,
        };

        let actions = reconcile_snapshots(&mut tracked, &[unpaired, non_a2dp]);
        assert_eq!(actions, []);
        assert!(tracked.is_empty());
    }

    #[test]
    fn reconcile_snapshots_voice_call_protection() {
        let dev_path = "/org/bluez/hci1/dev_call".to_string();
        let mut tracked = HashMap::new();

        let in_call = BluezDeviceSnapshot {
            path: dev_path.clone(),
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: false,
            has_bluealsa_pcm: false,
            has_voice_call_pcm: true, // phone call active!
        };

        let actions = reconcile_snapshots(&mut tracked, &[in_call]);
        assert_eq!(actions, []);
        // Voice call must not accumulate divergence ticks
        assert_eq!(tracked.get(&dev_path).map_or(0, |s| s.divergence_ticks), 0);
    }

    #[test]
    fn reconcile_snapshots_backs_off_after_max_resets() {
        let dev_path = "/org/bluez/hci1/dev_stubborn".to_string();
        let mut tracked = HashMap::new();
        tracked.insert(
            dev_path.clone(),
            DeviceWatchState {
                divergence_ticks: 3,
                hard_resets_attempted: MAX_HARD_RESETS_PER_SESSION,
            },
        );

        let snapshot = BluezDeviceSnapshot {
            path: dev_path,
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: true,
            has_bluealsa_pcm: false,
            has_voice_call_pcm: false,
        };

        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);
    }

    #[test]
    fn is_unknown_object_or_method_matches_names() {
        let err1 = zbus::fdo::Error::UnknownObject("object not found".into());
        let err2 = zbus::fdo::Error::UnknownInterface("iface not found".into());
        let err3 = zbus::fdo::Error::UnknownMethod("method not found".into());
        let err4 = zbus::fdo::Error::AccessDenied("rejected".into());

        assert!(is_unknown_object_or_method(&err1));
        assert!(is_unknown_object_or_method(&err2));
        assert!(is_unknown_object_or_method(&err3));
        assert!(!is_unknown_object_or_method(&err4));
    }

    #[tokio::test]
    async fn mock_dbus_collect_snapshots_fast_path_and_errors() {
        let mock = MockReconcilerDbus::new();

        // 1. BlueZ query fails with D-Bus error
        mock.set_bluez_error(zbus::Error::Address("unreachable bus".into()));
        let res = collect_snapshots(&mock).await;
        let _ = res.unwrap_err();

        // 2. Fast-path: BlueZ has only disconnected devices -> skips BlueALSA query
        let mut bluez_objs = ManagedObjects::new();
        let mut dev_props = HashMap::new();
        dev_props.insert("Connected".to_string(), to_val(false));
        dev_props.insert("Paired".to_string(), to_val(true));
        let mut dev_ifaces = HashMap::new();
        dev_ifaces.insert(owned_iface("org.bluez.Device1"), dev_props);
        bluez_objs.insert(owned_path("/org/bluez/hci0/dev_1"), dev_ifaces);

        mock.set_bluez_objects(bluez_objs.clone());
        mock.set_bluealsa_error(zbus::Error::Failure("must not be called".into()));
        let snapshots = collect_snapshots(&mock).await.expect("fast path succeeds");
        assert_eq!(snapshots.len(), 1);
        assert!(!snapshots[0].connected);

        // 3. Connected device triggers BlueALSA query; BlueALSA failure aborts tick
        let mut connected_dev_props = HashMap::new();
        connected_dev_props.insert("Connected".to_string(), to_val(true));
        connected_dev_props.insert("Paired".to_string(), to_val(true));
        let mut connected_ifaces = HashMap::new();
        connected_ifaces.insert(owned_iface("org.bluez.Device1"), connected_dev_props);
        let mut connected_bluez = ManagedObjects::new();
        connected_bluez.insert(owned_path("/org/bluez/hci0/dev_2"), connected_ifaces);

        mock.set_bluez_objects(connected_bluez);
        mock.set_bluealsa_error(zbus::Error::Failure("bluealsa down".into()));
        let err_res = collect_snapshots(&mock).await;
        let _ = err_res.unwrap_err();
    }

    #[tokio::test]
    async fn mock_dbus_execute_actions_and_cycle_connection() {
        let mock = MockReconcilerDbus::new();
        let dev_path = "/org/bluez/hci0/dev_action_test";

        // Successful ConnectProfile
        let action1 = ReconcileAction::ConnectA2dpProfile {
            device_path: dev_path.to_string(),
        };
        execute_reconcile_actions_with_settle(&mock, &[action1], Duration::from_millis(1)).await;
        assert_eq!(
            mock.connect_profile_calls(),
            vec![(dev_path.to_string(), UUID_A2DP_SOURCE.to_string())]
        );

        // Failing ConnectProfile (handles error branch cleanly)
        mock.set_connect_profile_result(Err(zbus::Error::Failure("profile rejected".into())));
        let action2 = ReconcileAction::ConnectA2dpProfile {
            device_path: dev_path.to_string(),
        };
        execute_reconcile_actions_with_settle(&mock, &[action2], Duration::from_millis(1)).await;

        // Successful connection cycle (Disconnect -> Settle -> Connect)
        let action3 = ReconcileAction::CycleConnection {
            device_path: dev_path.to_string(),
            reason: DisconnectReason::OrphanedBluezTransport,
        };
        execute_reconcile_actions_with_settle(&mock, &[action3], Duration::from_millis(1)).await;
        assert_eq!(mock.disconnect_calls(), vec![dev_path.to_string()]);
        assert_eq!(mock.connect_calls(), vec![dev_path.to_string()]);

        // Failing Disconnect and Connect
        mock.set_disconnect_result(Err(zbus::Error::Failure("disconnect failed".into())));
        mock.set_connect_result(Err(zbus::Error::Failure("connect failed".into())));
        cycle_device_connection_with_settle(&mock, dev_path, Duration::from_millis(1)).await;
        assert_eq!(mock.disconnect_calls().len(), 2);
        assert_eq!(mock.connect_calls().len(), 2);
    }

    #[tokio::test]
    async fn mock_dbus_reconciler_tick_outcomes() {
        let mock = MockReconcilerDbus::new();
        let mut tracked = HashMap::new();

        // 1. Query error outcome
        mock.set_bluez_error(zbus::Error::Failure("service absent".into()));
        let outcome1 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
        assert!(matches!(outcome1, TickOutcome::SkippedQueryError(_)));

        // 2. Normal tick with no actions needed (empty devices)
        mock.set_bluez_objects(ManagedObjects::new());
        let outcome2 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
        assert_eq!(outcome2, TickOutcome::ActionsExecuted(0));

        // 3. Normal tick with recovery action executed
        let dev_path = "/org/bluez/hci0/dev_tick_test";
        let mut dev_props = HashMap::new();
        dev_props.insert("Connected".to_string(), to_val(true));
        dev_props.insert("Paired".to_string(), to_val(true));
        dev_props.insert("Bonded".to_string(), to_val(true));
        dev_props.insert("ServicesResolved".to_string(), to_val(true));
        dev_props.insert("UUIDs".to_string(), to_val(vec![UUID_A2DP_SOURCE]));
        let mut dev_ifaces = HashMap::new();
        dev_ifaces.insert(owned_iface("org.bluez.Device1"), dev_props);
        let mut bluez_objs = ManagedObjects::new();
        bluez_objs.insert(owned_path(dev_path), dev_ifaces);

        // Orphaned transport: BlueZ has /fd0, BlueALSA has no PCMs
        let mut fd_ifaces = HashMap::new();
        fd_ifaces.insert(owned_iface("org.bluez.MediaTransport1"), HashMap::new());
        bluez_objs.insert(owned_path(&format!("{dev_path}/fd0")), fd_ifaces);

        mock.set_bluez_objects(bluez_objs.clone());
        mock.set_bluealsa_objects(ManagedObjects::new());

        // Tick 1 (debounce): 0 actions executed
        let outcome3 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
        assert_eq!(outcome3, TickOutcome::ActionsExecuted(0));
        assert_eq!(tracked.get(dev_path).unwrap().divergence_ticks, 1);

        // Tick 2 (escalate): 1 action executed (CycleConnection)
        mock.set_bluez_objects(bluez_objs);
        mock.set_bluealsa_objects(ManagedObjects::new());
        let outcome4 = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
        assert_eq!(outcome4, TickOutcome::ActionsExecuted(1));
        assert_eq!(tracked.get(dev_path).unwrap().divergence_ticks, 0);
        assert_eq!(tracked.get(dev_path).unwrap().hard_resets_attempted, 1);
    }

    #[tokio::test]
    async fn test_mock_reconciler_new_mocked_and_controller() {
        let (mock, ctrl) = MockReconcilerDbus::new_mocked();

        // Test controller setters and mock calls
        ctrl.set_bluez_error(zbus::Error::Failure("bluez failure".into()));
        let bluez_err = mock.fetch_bluez_objects().await;
        let _ = bluez_err.unwrap_err();

        ctrl.set_bluealsa_error(zbus::Error::Failure("bluealsa failure".into()));
        let bluealsa_err = mock.fetch_bluealsa_objects().await;
        let _ = bluealsa_err.unwrap_err();

        ctrl.set_connect_profile_result(Err(zbus::Error::Failure("profile err".into())));
        let cp_res = mock.connect_profile("/dev/test", "uuid").await;
        let _ = cp_res.unwrap_err();

        ctrl.set_disconnect_result(Err(zbus::Error::Failure("disc err".into())));
        let disc_res = mock.disconnect("/dev/test").await;
        let _ = disc_res.unwrap_err();

        ctrl.set_connect_result(Err(zbus::Error::Failure("conn err".into())));
        let conn_res = mock.connect("/dev/test").await;
        let _ = conn_res.unwrap_err();

        // Verify calls recorded through controller
        assert_eq!(
            ctrl.connect_profile_calls(),
            vec![("/dev/test".to_string(), "uuid".to_string())]
        );
        assert_eq!(ctrl.disconnect_calls(), vec!["/dev/test".to_string()]);
        assert_eq!(ctrl.connect_calls(), vec!["/dev/test".to_string()]);

        // Test &T blanket impl of ReconcilerDbus
        let mock_ref = &mock;
        let empty_bluez = mock_ref.fetch_bluez_objects().await.unwrap();
        assert!(empty_bluez.is_empty());
        let empty_bluealsa = mock_ref.fetch_bluealsa_objects().await.unwrap();
        assert!(empty_bluealsa.is_empty());

        ctrl.set_connect_profile_result(Ok(()));
        ctrl.set_disconnect_result(Ok(()));
        ctrl.set_connect_result(Ok(()));

        mock_ref.connect_profile("/dev/ref", "uuid2").await.unwrap();
        mock_ref.disconnect("/dev/ref").await.unwrap();
        mock_ref.connect("/dev/ref").await.unwrap();

        assert_eq!(mock.connect_profile_calls().len(), 2);
        assert_eq!(mock.disconnect_calls().len(), 2);
        assert_eq!(mock.connect_calls().len(), 2);
    }
}
