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
//! its active D-Bus registrations vanish. However, `BlueZ` retains the allocated
//! `MediaTransport1` object (e.g. `/org/bluez/hci0/dev_XX_.../fd1`) because the underlying
//! ACL connection to the phone remains intact.
//!
//! Calling `ConnectProfile("0000110a-0000-1000-8000-00805f9b34fb")` in this state is a **no-op**
//! because `BlueZ` believes the A2DP profile and transport are already active. Remote mobile
//! operating systems (iOS, Android) show "Connected (Audio)", yet no ALSA PCM exists and no audio
//! can play.
//!
//! **Remedy**: The only reliable recovery is an explicit baseband ACL reset:
//! calling `Device1.Disconnect()`, waiting for the kernel and `BlueZ` to tear down the stale `fd*`
//! transport (`RECONNECT_SETTLE_DELAY`), and calling `Device1.Connect()` to initiate a fresh
//! handshake.
//!
//! ## Case B: ACL Half-Reconnect (Phone connects ACL, but A2DP fails to negotiate)
//! When a mobile device reconnects (or comes back in range), `Connected` flips to `true`,
//! but neither `BlueZ` creates an `fd*` transport nor does `bluealsad` expose a PCM.
//!
//! **Remedy**: A two-stage recovery:
//! - **Stage 1 (6 seconds)**: Non-disruptive nudge by calling `Device1.ConnectProfile(A2DP_SOURCE)`.
//! - **Stage 2 (12 seconds fallback)**: If Stage 1 fails to create a transport, cycle
//!   the ACL connection via `Disconnect` -> settle -> `Connect`.
//!
//! # Defensive Invariants & Third-Party Edge Cases
//!
//! 1. **`ObjectManager` Path Ambiguity (`/org/bluealsa` vs `/`)**:
//!    In `bluez-alsa` 4.x and 5.0.0, `bluealsad` serves `org.freedesktop.DBus.ObjectManager`
//!    on `/org/bluealsa`. However, standard GIO implementations may mount at `/`. This reconciler
//!    queries `/org/bluealsa` first. If and only if `bluealsad` returns `UnknownObject` or
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

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
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

/// Maximum timeout permitted for any single D-Bus query or method invocation.
pub const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Settling delay between `Disconnect()` and `Connect()` during an ACL hard reset.
///
/// `BlueZ` requires ~1.5s to cleanly tear down the kernel L2CAP channels and release
/// the internal `MediaTransport1` file descriptor before accepting a new connection.
pub const RECONNECT_SETTLE_DELAY: Duration = Duration::from_millis(1500);

/// Maximum number of `Disconnect -> Connect` hard cycles permitted per ACL session
/// before backing off to prevent endless connection loops.
pub const MAX_HARD_RESETS_PER_SESSION: u8 = 2;

/// Tracking state maintained for each connected Bluetooth device.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct DeviceWatchState {
    /// Number of consecutive 3-second ticks during which audio transport divergence persisted.
    pub divergence_ticks: u32,
    /// Number of `Disconnect -> Connect` hard resets attempted during the current ACL connection.
    pub hard_resets_attempted: u8,
}

/// Snapshot of `BlueZ` device properties and associated transport/PCM state.
#[expect(
    clippy::struct_excessive_bools,
    reason = "direct reflection of BlueZ Device1 properties and transport state"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluezDeviceSnapshot {
    /// `BlueZ` D-Bus object path (e.g. `/org/bluez/hci0/dev_30_E0_44_67_38_A2`).
    pub path: String,
    /// Whether baseband ACL connection is established (`org.bluez.Device1.Connected`).
    pub connected: bool,
    /// Whether the device has completed pairing (`org.bluez.Device1.Paired`).
    pub paired: bool,
    /// Whether the link key is stored in long-term bonding (`org.bluez.Device1.Bonded`).
    pub bonded: bool,
    /// Whether `BlueZ` has completed GATT/SDP service discovery (`ServicesResolved`).
    pub services_resolved: bool,
    /// Whether the device advertises A2DP Source capability in its UUID list.
    pub supports_a2dp_source: bool,
    /// Whether `BlueZ` currently holds an active streaming transport (`/org/bluez/.../fd*`).
    pub has_bluez_transport: bool,
    /// Whether `bluealsad` exposes an active A2DP sink PCM for this device (`/a2dpsnk/source`).
    pub has_bluealsa_pcm: bool,
    /// Whether `bluealsad` exposes an active voice call PCM (HFP/HSP/SCO).
    pub has_voice_call_pcm: bool,
}

/// Action determined by the deterministic reconciler state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Nudge `BlueZ` to establish the A2DP profile without dropping the baseband connection.
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
    tokio::spawn(async move {
        run_reconciler_loop(conn).await;
    })
}

/// Core asynchronous reconciler loop.
pub async fn run_reconciler_loop(conn: Connection) {
    let mut tracked: HashMap<String, DeviceWatchState> = HashMap::new();
    tracing::info!(
        "Bluetooth transport state reconciler started (BlueZ 5.82 <-> bluez-alsa v5.0.0)"
    );

    loop {
        sleep(POLL_INTERVAL).await;

        let snapshots = match timeout(DBUS_CALL_TIMEOUT, collect_snapshots(&conn)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                // CRITICAL SAFETY GUARD: If org.bluealsa or org.bluez failed to respond
                // (e.g. daemon restarting or D-Bus AccessDenied), DO NOT run recovery
                // or we would falsely disconnect healthy streams.
                tracing::warn!(error = %e, "BT transport reconciler skipped tick (D-Bus query error)");
                continue;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    "BT transport reconciler skipped tick (D-Bus call timed out after {DBUS_CALL_TIMEOUT:?})"
                );
                continue;
            }
        };

        let actions = reconcile_snapshots(&mut tracked, &snapshots);
        execute_reconcile_actions(&conn, &actions).await;
    }
}

/// Queries `BlueZ` and `BlueALSA` over D-Bus to construct per-device snapshots.
///
/// # Errors
/// Returns a [`zbus::Error`] if either D-Bus daemon fails to respond or denies access.
/// Propagates errors via `?`: if either daemon fails to respond or denies access,
/// the tick aborts so recovery is not triggered on partial or ambiguous information.
pub async fn collect_snapshots(conn: &Connection) -> zbus::Result<Vec<BluezDeviceSnapshot>> {
    let bluez_mgr = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let bluez_objs: ManagedObjects = bluez_mgr.get_managed_objects().await?;

    // Fast path: if there are no connected devices in BlueZ, skip querying org.bluealsa.
    let has_connected_device = bluez_objs.values().any(|ifaces| {
        find_iface(ifaces, "org.bluez.Device1")
            .and_then(|props| extract_bool(props, "Connected"))
            .unwrap_or(false)
    });

    let bluealsa_objs = if has_connected_device {
        fetch_bluealsa_objects(conn).await?
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

/// Executes decided actions against `BlueZ` over D-Bus with explicit timeouts.
pub async fn execute_reconcile_actions(conn: &Connection, actions: &[ReconcileAction]) {
    for action in actions {
        match action {
            ReconcileAction::ConnectA2dpProfile { device_path } => {
                tracing::info!(device = %device_path, "Calling ConnectProfile(A2DP_SOURCE)");
                match timeout(
                    DBUS_CALL_TIMEOUT,
                    call_device_method(conn, device_path, "ConnectProfile", &(UUID_A2DP_SOURCE,)),
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
                cycle_device_connection(conn, device_path).await;
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

/// Safely extracts a D-Bus `as` (array of strings) across zbus v4 and v5 without failing on `Value::Array`.
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
    if let Value::Array(arr) = &**val {
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
pub async fn cycle_device_connection(conn: &Connection, device_path: &str) {
    match timeout(
        DBUS_CALL_TIMEOUT,
        call_device_method(conn, device_path, "Disconnect", &()),
    )
    .await
    {
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

    sleep(RECONNECT_SETTLE_DELAY).await;

    match timeout(
        DBUS_CALL_TIMEOUT,
        call_device_method(conn, device_path, "Connect", &()),
    )
    .await
    {
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
        pcm_props.insert(
            "Device".to_string(),
            to_val(zbus::zvariant::ObjectPath::try_from(dev_path).unwrap()),
        );
        pcm_props.insert("Transport".to_string(), to_val("A2DP-sink"));
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
        assert!(s.supports_a2dp_source);
        assert!(s.has_bluez_transport);
        assert!(s.has_bluealsa_pcm);
        assert!(!s.has_voice_call_pcm);
    }

    #[test]
    fn reconcile_snapshots_case_a_orphaned_transport() {
        let dev_path = "/org/bluez/hci1/dev_30_E0_44_67_38_A2".to_string();
        let mut tracked = HashMap::new();

        let snapshot = BluezDeviceSnapshot {
            path: dev_path.clone(),
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: true, // fd* exists in BlueZ
            has_bluealsa_pcm: false,   // bluealsad has NO PCM!
            has_voice_call_pcm: false,
        };

        // Tick 1: detects divergence, no action yet
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 1);
        assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 0);

        // Tick 2: divergence_ticks >= 2, triggers Disconnect -> Connect cycle
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
        let dev_path = "/org/bluez/hci1/dev_30_E0_44_67_38_A2".to_string();
        let mut tracked = HashMap::new();

        let snapshot = BluezDeviceSnapshot {
            path: dev_path.clone(),
            connected: true,
            paired: true,
            bonded: true,
            services_resolved: true,
            supports_a2dp_source: true,
            has_bluez_transport: false, // No transport in BlueZ
            has_bluealsa_pcm: false,    // No PCM in bluealsad
            has_voice_call_pcm: false,
        };

        // Tick 1: initial detection
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);

        // Tick 2: Stage 1 soft recovery (ConnectProfile)
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::ConnectA2dpProfile {
                device_path: dev_path.clone(),
            }
        );
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 2);

        // Tick 3: still divergent
        let actions = reconcile_snapshots(&mut tracked, std::slice::from_ref(&snapshot));
        assert_eq!(actions, []);
        assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 3);

        // Tick 4: Stage 2 hard recovery (CycleConnection)
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
}
