//! Active Bluetooth audio source arbiter (`org.speaker.Bluetooth`).
//!
//! `bluealsa-aplay --single-audio` enforces first-play-wins on the ALSA output
//! but does not announce which device won, and its built-in AVRCP `Pause`
//! hardcodes `.../player0` (which fails on `BlueZ` 5.88's `.../avrcp/player0`).
//! This module mirrors the same first-play-wins rule from `org.bluealsa`
//! signals, pauses any shut-out device using its real `MediaPlayer1` path,
//! and publishes `ActiveDevice` on the system D-Bus for `volume_sync`,
//! `cover_provider`, and `screen-manager`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use speaker_bluetooth::connection::{DbusClient, ManagedObjects};
use speaker_bluetooth::{
    ACTIVE_SOURCE_DBUS_NAME, ACTIVE_SOURCE_OBJECT_PATH, BluetoothDevice, BtAddress,
    device_address_from_path, format_active_device,
};
use tokio::sync::watch;
use zbus::fdo::ObjectManagerProxy;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

const PROPS_QUEUE: usize = 256;
const IFACE_QUEUE: usize = 64;
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Pure outcome of one arbiter state transition.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ArbiterOutcome {
    /// `Some(new_active)` when the active device changed.
    pub active_changed: Option<Option<BtAddress>>,
    /// `Some(addr)` when a shut-out device should be sent `MediaPlayer1.Pause`.
    pub pause_device: Option<BtAddress>,
}

#[derive(Debug, Clone)]
struct PcmEntry {
    address: BtAddress,
    running: bool,
    delay_seq: u64,
}

/// Pure, host-testable state machine for single-active-source arbitration.
#[derive(Debug, Default)]
pub struct ActiveSourceState {
    pcms: HashMap<Box<str>, PcmEntry>,
    active: Option<BtAddress>,
    paused_shutout: HashSet<BtAddress>,
    seq: u64,
}

impl ActiveSourceState {
    /// Creates an empty arbiter state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the currently active device, if any.
    #[must_use]
    pub const fn active(&self) -> Option<BtAddress> {
        self.active
    }

    /// Seeds state from an initial snapshot of `(pcm_path, running)` pairs.
    ///
    /// If multiple A2DP PCMs are already running at startup (e.g. `bt-manager`
    /// restarted mid-stream), picks the first running one deterministically
    /// (sorted by path) and logs a warning because `bluealsa-aplay` does not
    /// expose which worker holds the slot.
    pub fn seed<'a>(
        &mut self,
        entries: impl IntoIterator<Item = (&'a str, bool)>,
    ) -> ArbiterOutcome {
        let prev = self.active;
        self.pcms.clear();
        self.paused_shutout.clear();
        self.active = None;

        let mut running_addrs: Vec<(&str, BtAddress)> = Vec::new();
        for (path, running) in entries {
            if !speaker_bluetooth::transport::is_a2dp_pcm_path(path) {
                continue;
            }
            let Some(address) = device_address_from_path(path) else {
                continue;
            };
            self.pcms.insert(
                Box::from(path),
                PcmEntry {
                    address,
                    running,
                    delay_seq: 0,
                },
            );
            if running {
                running_addrs.push((path, address));
            }
        }

        if running_addrs.len() > 1 {
            running_addrs.sort_unstable_by_key(|(path, _)| *path);
            tracing::warn!(
                running_count = running_addrs.len(),
                chosen = %running_addrs[0].1,
                "Multiple A2DP PCMs already running on arbiter seed; picking first"
            );
        }
        self.active = running_addrs.first().map(|(_, addr)| *addr);

        let active_changed = (self.active != prev).then_some(self.active);
        let pause_device = running_addrs
            .iter()
            .map(|(_, addr)| *addr)
            .find(|addr| Some(*addr) != self.active);
        if let Some(addr) = pause_device {
            self.paused_shutout.insert(addr);
        }
        ArbiterOutcome {
            active_changed,
            pause_device,
        }
    }

    /// Handles a new A2DP PCM appearing (`InterfacesAdded`).
    pub fn on_pcm_added(&mut self, path: &str, running: bool) -> ArbiterOutcome {
        if !speaker_bluetooth::transport::is_a2dp_pcm_path(path) {
            return ArbiterOutcome::default();
        }
        let Some(address) = device_address_from_path(path) else {
            return ArbiterOutcome::default();
        };
        self.pcms.insert(
            Box::from(path),
            PcmEntry {
                address,
                running,
                delay_seq: 0,
            },
        );
        self.apply_running_transition(address, running)
    }

    /// Handles a `PropertiesChanged` update for `Running` on a PCM path.
    pub fn on_pcm_running_changed(&mut self, path: &str, running: bool) -> ArbiterOutcome {
        if !speaker_bluetooth::transport::is_a2dp_pcm_path(path) {
            return ArbiterOutcome::default();
        }
        let Some(address) = device_address_from_path(path) else {
            return ArbiterOutcome::default();
        };
        match self.pcms.get_mut(path) {
            Some(entry) => {
                if entry.running == running {
                    return ArbiterOutcome::default();
                }
                entry.running = running;
            }
            None => {
                self.pcms.insert(
                    Box::from(path),
                    PcmEntry {
                        address,
                        running,
                        delay_seq: 0,
                    },
                );
            }
        }
        self.apply_running_transition(address, running)
    }

    /// Records a `ClientDelay` update on `path` (used to break ties on restart).
    pub fn on_pcm_client_delay(&mut self, path: &str) {
        if let Some(entry) = self.pcms.get_mut(path) {
            self.seq = self.seq.saturating_add(1);
            entry.delay_seq = self.seq;
        }
    }

    /// Handles a PCM disappearing (`InterfacesRemoved`).
    pub fn on_pcm_removed(&mut self, path: &str) -> ArbiterOutcome {
        let Some(removed) = self.pcms.remove(path) else {
            return ArbiterOutcome::default();
        };
        if !self.has_running_pcm(removed.address) {
            self.paused_shutout.remove(&removed.address);
        }
        if self.active == Some(removed.address) && !self.has_running_pcm(removed.address) {
            return self.promote_next_running();
        }
        ArbiterOutcome::default()
    }

    fn has_running_pcm(&self, address: BtAddress) -> bool {
        self.pcms
            .values()
            .any(|e| e.running && e.address == address)
    }

    fn apply_running_transition(&mut self, address: BtAddress, running: bool) -> ArbiterOutcome {
        if running {
            match self.active {
                None => {
                    self.active = Some(address);
                    self.paused_shutout.remove(&address);
                    tracing::info!(device = %address, "Active Bluetooth source acquired");
                    ArbiterOutcome {
                        active_changed: Some(Some(address)),
                        pause_device: None,
                    }
                }
                Some(current) if current == address => ArbiterOutcome::default(),
                Some(current) => {
                    let pause_device = self.paused_shutout.insert(address).then_some(address);
                    if pause_device.is_some() {
                        tracing::info!(
                            active = %current,
                            shut_out = %address,
                            "Shut-out Bluetooth source detected; requesting AVRCP Pause"
                        );
                    }
                    ArbiterOutcome {
                        active_changed: None,
                        pause_device,
                    }
                }
            }
        } else {
            if !self.has_running_pcm(address) {
                self.paused_shutout.remove(&address);
            }
            if self.active == Some(address) && !self.has_running_pcm(address) {
                return self.promote_next_running();
            }
            ArbiterOutcome::default()
        }
    }

    fn promote_next_running(&mut self) -> ArbiterOutcome {
        let mut next: Vec<(&str, &PcmEntry)> = self
            .pcms
            .iter()
            .filter(|(_, e)| e.running)
            .map(|(p, e)| (p.as_ref(), e))
            .collect();
        // Prefer the stream with the most recent ClientDelay if any, otherwise
        // deterministic lexicographic path order.
        next.sort_by(|(pa, ea), (pb, eb)| eb.delay_seq.cmp(&ea.delay_seq).then_with(|| pa.cmp(pb)));
        let new_active = next.first().map(|(_, e)| e.address);
        let prev = self.active;
        self.active = new_active;
        if let Some(addr) = new_active {
            self.paused_shutout.remove(&addr);
            tracing::info!(device = %addr, "Active Bluetooth source promoted");
        } else if prev.is_some() {
            tracing::info!("Active Bluetooth source released (idle)");
        }
        ArbiterOutcome {
            active_changed: (new_active != prev).then_some(new_active),
            pause_device: None,
        }
    }
}

/// Shared active-device state mirrored on D-Bus.
#[derive(Debug, Clone, Default)]
pub struct SharedActiveSource(Arc<Mutex<Option<BtAddress>>>);

impl SharedActiveSource {
    /// Creates an empty shared active source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the current active device.
    #[must_use]
    pub fn get(&self) -> Option<BtAddress> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Updates the current active device.
    pub fn set(&self, device: Option<BtAddress>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = device;
    }
}

/// D-Bus interface serving `org.speaker.Bluetooth1` (`ActiveDevice`).
#[derive(Debug)]
pub struct ActiveSourceIface {
    shared: SharedActiveSource,
}

impl ActiveSourceIface {
    /// Creates an interface backed by `shared`.
    #[must_use]
    pub fn new(shared: SharedActiveSource) -> Self {
        Self { shared }
    }
}

#[zbus::interface(name = "org.speaker.Bluetooth1")]
impl ActiveSourceIface {
    /// Canonical MAC address of the active audio source, or `""` when idle.
    #[zbus(property)]
    fn active_device(&self) -> String {
        format_active_device(self.shared.get())
    }
}

/// Serves [`ActiveSourceIface`] on `connection` under `org.speaker.Bluetooth`.
///
/// # Errors
/// Returns `zbus::Error` if serving the object or requesting the bus name fails.
pub async fn serve_active_source_on(
    connection: &Connection,
    shared: SharedActiveSource,
) -> Result<SignalEmitter<'static>, zbus::Error> {
    connection
        .object_server()
        .at(ACTIVE_SOURCE_OBJECT_PATH, ActiveSourceIface::new(shared))
        .await?;
    connection.request_name(ACTIVE_SOURCE_DBUS_NAME).await?;
    let emitter = SignalEmitter::new(connection, ACTIVE_SOURCE_OBJECT_PATH)?.to_owned();
    Ok(emitter)
}

async fn pause_remote_player(conn: &Connection, device: BtAddress) {
    let Ok(mgr) = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .and_then(|b| b.path("/"))
    else {
        return;
    };
    let Ok(mgr) = mgr.build().await else {
        return;
    };
    let Ok(objects): zbus::Result<ManagedObjects> =
        mgr.get_managed_objects().await.map_err(Into::into)
    else {
        return;
    };
    let Some(player_path) = DbusClient::find_player_path_for_device(&objects, device) else {
        tracing::debug!(%device, "No BlueZ MediaPlayer1 found for shut-out device");
        return;
    };
    let Ok(builder) = zbus::proxy::Builder::<Proxy<'_>>::new(conn)
        .destination("org.bluez")
        .and_then(|b| b.path(player_path.as_ref()))
        .and_then(|b| b.interface("org.bluez.MediaPlayer1"))
    else {
        return;
    };
    let Ok(proxy) = builder.build().await else {
        return;
    };
    match proxy.call_method("Pause", &()).await {
        Ok(_) => tracing::info!(%device, path = %player_path, "Paused shut-out Bluetooth player"),
        Err(e) => {
            tracing::debug!(%device, path = %player_path, error = %e, "Failed to pause shut-out player");
        }
    }
}

async fn apply_outcome(outcome: ArbiterOutcome, ctx: &ArbiterCtx<'_>) {
    if let Some(new_active) = outcome.active_changed {
        ctx.shared.set(new_active);
        ctx.bt.set_active_source(new_active);
        let _ = ctx.active_tx.send(new_active);
        if let Some(cover_art) = ctx.cover_art {
            cover_art.wake();
        }
        if let Some(emitter) = ctx.emitter
            && let Ok(iface_ref) = ctx
                .control_conn
                .object_server()
                .interface::<_, ActiveSourceIface>(ACTIVE_SOURCE_OBJECT_PATH)
                .await
        {
            let _ = iface_ref.get().await.active_device_changed(emitter).await;
        }
    }
    if let Some(addr) = outcome.pause_device {
        pause_remote_player(ctx.control_conn, addr).await;
    }
}

async fn seed_from_bt(state: &mut ActiveSourceState, ctx: &ArbiterCtx<'_>) {
    let Ok(snapshot) = ctx.bt.link_snapshot_async().await else {
        return;
    };
    let outcome = state.seed(
        snapshot
            .pcms
            .iter()
            .map(|p| (p.pcm_path.as_ref(), p.running)),
    );
    apply_outcome(outcome, ctx).await;
}

fn build_rules() -> Option<(MatchRule<'static>, MatchRule<'static>, MatchRule<'static>)> {
    let props = MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build)
        .ok()?;
    let added = MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.ObjectManager"))
        .and_then(|b| b.member("InterfacesAdded"))
        .map(zbus::match_rule::Builder::build)
        .ok()?;
    let removed = MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.ObjectManager"))
        .and_then(|b| b.member("InterfacesRemoved"))
        .map(zbus::match_rule::Builder::build)
        .ok()?;
    Some((props, added, removed))
}

type DbusIfaces = HashMap<String, HashMap<String, OwnedValue>>;

struct ArbiterCtx<'a> {
    bt: &'a BluetoothDevice,
    shared: &'a SharedActiveSource,
    emitter: Option<&'a SignalEmitter<'static>>,
    active_tx: &'a watch::Sender<Option<BtAddress>>,
    control_conn: &'a Connection,
    cover_art: Option<&'a crate::cover_provider::CoverArtShutdown>,
}

async fn drive_arbiter_streams(
    state: &mut ActiveSourceState,
    ctx: &ArbiterCtx<'_>,
    mut props_stream: MessageStream,
    mut added_stream: MessageStream,
    mut removed_stream: MessageStream,
) {
    loop {
        tokio::select! {
            biased;
            msg = added_stream.next() => {
                let Some(Ok(msg)) = msg else { break; };
                if let Ok((path, ifaces)) = msg.body().deserialize::<(OwnedObjectPath, DbusIfaces)>()
                    && let Some(props) = ifaces.get("org.bluealsa.PCM1")
                {
                    let running = props
                        .get("Running")
                        .and_then(|v| bool::try_from(v).ok())
                        .unwrap_or(false);
                    let outcome = state.on_pcm_added(path.as_str(), running);
                    apply_outcome(outcome, ctx).await;
                }
            }
            msg = removed_stream.next() => {
                let Some(Ok(msg)) = msg else { break; };
                if let Ok((path, ifaces)) = msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>()
                    && ifaces.iter().any(|i| i == "org.bluealsa.PCM1")
                {
                    let outcome = state.on_pcm_removed(path.as_str());
                    apply_outcome(outcome, ctx).await;
                }
            }
            msg = props_stream.next() => {
                let Some(Ok(msg)) = msg else { break; };
                let Some(path) = msg.header().path().map(ToString::to_string) else { continue; };
                if let Ok((iface, changed, _)) = msg.body().deserialize::<(Box<str>, HashMap<String, Value<'_>>, Vec<String>)>()
                    && iface.as_ref() == "org.bluealsa.PCM1"
                {
                    if changed.contains_key("ClientDelay") {
                        state.on_pcm_client_delay(&path);
                    }
                    if let Some(running) = changed.get("Running").and_then(|v| bool::try_from(v).ok()) {
                        let outcome = state.on_pcm_running_changed(&path, running);
                        apply_outcome(outcome, ctx).await;
                    }
                }
            }
        }
    }
}

async fn publish_active_source(
    control_conn: &Connection,
    shared: &SharedActiveSource,
) -> Option<SignalEmitter<'static>> {
    match serve_active_source_on(control_conn, shared.clone()).await {
        Ok(emitter) => {
            tracing::info!(
                bus = ACTIVE_SOURCE_DBUS_NAME,
                path = ACTIVE_SOURCE_OBJECT_PATH,
                "Active Bluetooth source D-Bus service published"
            );
            Some(emitter)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to publish org.speaker.Bluetooth on D-Bus; continuing locally");
            None
        }
    }
}

/// Runs the active-source arbiter task forever.
pub async fn run_task(
    bt: BluetoothDevice,
    mut signal_conn: Connection,
    mut control_conn: Connection,
    active_tx: watch::Sender<Option<BtAddress>>,
    cover_art: Option<crate::cover_provider::CoverArtShutdown>,
) {
    let shared = SharedActiveSource::new();
    let mut emitter = publish_active_source(&control_conn, &shared).await;

    let Some((props_rule, added_rule, removed_rule)) = build_rules() else {
        tracing::error!("Failed to build active-source D-Bus match rules");
        return;
    };

    let mut state = ActiveSourceState::new();
    let mut backoff = RECONNECT_BACKOFF_BASE;
    let mut need_reconnect = false;
    let mut seeded = false;

    loop {
        if need_reconnect {
            match tokio::try_join!(Connection::system(), Connection::system()) {
                Ok((sig, ctrl)) => {
                    signal_conn = sig;
                    control_conn = ctrl;
                    emitter = publish_active_source(&control_conn, &shared).await;
                    seeded = false;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Active-source arbiter D-Bus reconnect failed");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                    continue;
                }
            }
        }

        let ctx = ArbiterCtx {
            bt: &bt,
            shared: &shared,
            emitter: emitter.as_ref(),
            active_tx: &active_tx,
            control_conn: &control_conn,
            cover_art: cover_art.as_ref(),
        };
        if !seeded {
            seed_from_bt(&mut state, &ctx).await;
            seeded = true;
        }

        let streams = tokio::try_join!(
            MessageStream::for_match_rule(props_rule.clone(), &signal_conn, Some(PROPS_QUEUE)),
            MessageStream::for_match_rule(added_rule.clone(), &signal_conn, Some(IFACE_QUEUE)),
            MessageStream::for_match_rule(removed_rule.clone(), &signal_conn, Some(IFACE_QUEUE)),
        );
        let Ok((props_stream, added_stream, removed_stream)) = streams else {
            need_reconnect = true;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
            continue;
        };
        backoff = RECONNECT_BACKOFF_BASE;

        drive_arbiter_streams(&mut state, &ctx, props_stream, added_stream, removed_stream).await;

        need_reconnect = true;
        tracing::warn!("Active-source arbiter signal stream ended; reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}
