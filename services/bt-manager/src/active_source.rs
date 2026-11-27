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
use speaker_bluetooth::reconciler::UUID_A2DP_SOURCE;
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
    /// `Some(addr)` when a lingering-paused active device should have its
    /// A2DP profile disconnected so `bluealsa-aplay --single-audio` releases
    /// the ALSA slot immediately for a newly playing device.
    pub release_device: Option<BtAddress>,
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
    player_playing: HashMap<BtAddress, bool>,
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
        self.player_playing.clear();
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
            release_device: None,
        }
    }

    /// Updates the known AVRCP `MediaPlayer1.Status` (`playing` vs paused/stopped)
    /// for `device`. When the active device is known to be paused/stopped while
    /// its A2DP PCM lingers `Running: true`, another device starting playback
    /// is allowed to take over immediately instead of being shut out.
    pub fn on_player_status_changed(&mut self, device: BtAddress, playing: bool) {
        self.player_playing.insert(device, playing);
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
        if !self.pcms.values().any(|e| e.address == removed.address) {
            self.player_playing.remove(&removed.address);
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
                        release_device: None,
                    }
                }
                Some(current) if current == address => ArbiterOutcome::default(),
                Some(current) if self.player_playing.get(&current) == Some(&false) => {
                    // The current active device's AVRCP player is already
                    // paused/stopped (e.g. a laptop holding its A2DP stream
                    // open after pause): hand over immediately and release its
                    // lingering A2DP transport so `--single-audio` unblocks.
                    self.active = Some(address);
                    self.paused_shutout.remove(&address);
                    tracing::info!(
                        previous = %current,
                        new_active = %address,
                        "Active Bluetooth source paused; handing over to newly playing device"
                    );
                    ArbiterOutcome {
                        active_changed: Some(Some(address)),
                        pause_device: None,
                        release_device: Some(current),
                    }
                }
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
                        release_device: None,
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
            release_device: None,
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

/// D-Bus interface serving `org.speaker.Bluetooth1` (`ActiveDevice`, `PauseActive`).
#[derive(Debug)]
pub struct ActiveSourceIface {
    shared: SharedActiveSource,
    control_conn: Option<Connection>,
}

impl ActiveSourceIface {
    /// Creates an interface backed by `shared` (without remote `BlueZ` pause control).
    #[must_use]
    pub fn new(shared: SharedActiveSource) -> Self {
        Self {
            shared,
            control_conn: None,
        }
    }

    /// Creates an interface backed by `shared` and `control_conn` for `PauseActive`.
    #[must_use]
    pub fn with_connection(shared: SharedActiveSource, control_conn: Connection) -> Self {
        Self {
            shared,
            control_conn: Some(control_conn),
        }
    }
}

#[zbus::interface(name = "org.speaker.Bluetooth1")]
impl ActiveSourceIface {
    /// Canonical MAC address of the active audio source, or `""` when idle.
    #[zbus(property)]
    fn active_device(&self) -> String {
        format_active_device(self.shared.get())
    }

    /// Pauses the currently active Bluetooth player when another audio source takes over.
    async fn pause_active(&self) {
        let Some(conn) = self.control_conn.as_ref() else {
            return;
        };
        if let Some(device) = self.shared.get() {
            pause_remote_player(conn, device).await;
        }
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
        .at(
            ACTIVE_SOURCE_OBJECT_PATH,
            ActiveSourceIface::with_connection(shared, connection.clone()),
        )
        .await?;
    connection.request_name(ACTIVE_SOURCE_DBUS_NAME).await?;
    let emitter = SignalEmitter::new(connection, ACTIVE_SOURCE_OBJECT_PATH)?.to_owned();
    Ok(emitter)
}

async fn fetch_bluez_objects(conn: &Connection) -> Option<ManagedObjects> {
    let builder = match ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .and_then(|b| b.path("/"))
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to configure BlueZ ObjectManager proxy");
            return None;
        }
    };
    let mgr = match builder.build().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to build BlueZ ObjectManager proxy");
            return None;
        }
    };
    match mgr.get_managed_objects().await {
        Ok(objs) => Some(objs),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query BlueZ managed objects");
            None
        }
    }
}

async fn pause_remote_player(conn: &Connection, device: BtAddress) {
    let Some(objects) = fetch_bluez_objects(conn).await else {
        return;
    };
    let Some(player_path) = DbusClient::find_player_path_for_device(&objects, device) else {
        tracing::debug!(%device, "No BlueZ MediaPlayer1 found for shut-out device");
        return;
    };
    let builder = match zbus::proxy::Builder::<Proxy<'_>>::new(conn)
        .destination("org.bluez")
        .and_then(|b| b.path(player_path.as_ref()))
        .and_then(|b| b.interface("org.bluez.MediaPlayer1"))
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(%device, path = %player_path, error = %e, "Invalid MediaPlayer1 proxy params");
            return;
        }
    };
    let proxy = match builder.build().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%device, path = %player_path, error = %e, "Failed to build MediaPlayer1 proxy");
            return;
        }
    };
    match proxy.call_method("Pause", &()).await {
        Ok(_) => tracing::info!(%device, path = %player_path, "Paused shut-out Bluetooth player"),
        Err(e) => {
            tracing::warn!(%device, path = %player_path, error = %e, "Failed to pause shut-out player");
        }
    }
}

async fn release_lingering_a2dp(conn: &Connection, device: BtAddress) {
    let Some(objects) = fetch_bluez_objects(conn).await else {
        return;
    };
    let Some(dev_path) = DbusClient::find_device_path(&objects, device) else {
        tracing::warn!(%device, "No BlueZ Device1 object found to release lingering A2DP");
        return;
    };
    let builder = match zbus::proxy::Builder::<Proxy<'_>>::new(conn)
        .destination("org.bluez")
        .and_then(|b| b.path(dev_path.as_ref()))
        .and_then(|b| b.interface("org.bluez.Device1"))
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(%device, path = %dev_path, error = %e, "Invalid Device1 proxy params");
            return;
        }
    };
    let proxy = match builder.build().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%device, path = %dev_path, error = %e, "Failed to build Device1 proxy");
            return;
        }
    };
    match proxy
        .call_method("DisconnectProfile", &(UUID_A2DP_SOURCE,))
        .await
    {
        Ok(_) => tracing::info!(
            %device,
            path = %dev_path,
            "Disconnected lingering A2DP profile on paused device so new source can play"
        ),
        Err(e) => tracing::warn!(
            %device,
            path = %dev_path,
            error = %e,
            "Failed to disconnect lingering A2DP profile on paused device"
        ),
    }
}

async fn apply_outcome(outcome: ArbiterOutcome, ctx: &ArbiterCtx<'_>) {
    if let Some(addr) = outcome.release_device {
        release_lingering_a2dp(ctx.control_conn, addr).await;
    }
    if let Some(new_active) = outcome.active_changed {
        ctx.shared.set(new_active);
        ctx.bt.set_active_source(new_active);
        if ctx.active_tx.send(new_active).is_err() {
            tracing::warn!(device = ?new_active, "Volume sync active-device receiver dropped");
        }
        if let Some(cover_art) = ctx.cover_art {
            cover_art.wake();
        }
        if let Some(emitter) = ctx.emitter
            && let Ok(iface_ref) = ctx
                .control_conn
                .object_server()
                .interface::<_, ActiveSourceIface>(ACTIVE_SOURCE_OBJECT_PATH)
                .await
            && let Err(e) = iface_ref.get().await.active_device_changed(emitter).await
        {
            tracing::warn!(error = %e, "Failed to emit ActiveDevice D-Bus property change");
        }
    }
    if let Some(addr) = outcome.pause_device {
        pause_remote_player(ctx.control_conn, addr).await;
    }
}

async fn seed_from_bt(state: &mut ActiveSourceState, ctx: &ArbiterCtx<'_>) -> bool {
    let snapshot = match ctx.bt.link_snapshot_async().await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            tracing::warn!(error = %e, "Active-source arbiter seed snapshot failed; will retry on reconnect");
            return false;
        }
    };
    let outcome = state.seed(
        snapshot
            .pcms
            .iter()
            .map(|p| (p.pcm_path.as_ref(), p.running)),
    );
    if let Some(objects) = fetch_bluez_objects(ctx.control_conn).await {
        for (addr, playing) in DbusClient::player_playing_states(&objects) {
            state.on_player_status_changed(addr, playing);
        }
    }
    apply_outcome(outcome, ctx).await;
    true
}

struct ArbiterRules {
    props: MatchRule<'static>,
    added: MatchRule<'static>,
    removed: MatchRule<'static>,
    player_props: MatchRule<'static>,
}

fn build_rules() -> Option<ArbiterRules> {
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
    let player_props = MatchRule::builder()
        .sender("org.bluez")
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .and_then(|b| b.arg0ns("org.bluez.MediaPlayer1"))
        .map(zbus::match_rule::Builder::build)
        .ok()?;
    Some(ArbiterRules {
        props,
        added,
        removed,
        player_props,
    })
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

struct ArbiterStreams {
    props: MessageStream,
    added: MessageStream,
    removed: MessageStream,
    player_props: MessageStream,
}

async fn drive_arbiter_streams(
    state: &mut ActiveSourceState,
    ctx: &ArbiterCtx<'_>,
    mut streams: ArbiterStreams,
) {
    loop {
        tokio::select! {
            biased;
            msg = streams.added.next() => {
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
            msg = streams.removed.next() => {
                let Some(Ok(msg)) = msg else { break; };
                if let Ok((path, ifaces)) = msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>()
                    && ifaces.iter().any(|i| i == "org.bluealsa.PCM1")
                {
                    let outcome = state.on_pcm_removed(path.as_str());
                    apply_outcome(outcome, ctx).await;
                }
            }
            msg = streams.props.next() => {
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
            msg = streams.player_props.next() => {
                let Some(Ok(msg)) = msg else { break; };
                let Some(path) = msg.header().path().map(ToString::to_string) else { continue; };
                if let Some(addr) = device_address_from_path(&path)
                    && let Ok((iface, changed, _)) = msg.body().deserialize::<(Box<str>, HashMap<String, Value<'_>>, Vec<String>)>()
                    && iface.as_ref() == "org.bluez.MediaPlayer1"
                    && let Some(status) = changed.get("Status").and_then(|v| <&str>::try_from(v).ok())
                {
                    state.on_player_status_changed(addr, status.eq_ignore_ascii_case("playing"));
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

    let Some(rules) = build_rules() else {
        tracing::error!("Failed to build active-source D-Bus match rules");
        return;
    };

    let mut state = ActiveSourceState::new();
    let mut backoff = RECONNECT_BACKOFF_BASE;
    let mut need_reconnect = false;
    let mut seeded = false;

    loop {
        if need_reconnect {
            // Drop the previous SignalEmitter before requesting the bus name on
            // the replacement connection so the old connection cannot hold it.
            emitter = None;
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

        let streams = tokio::try_join!(
            MessageStream::for_match_rule(rules.props.clone(), &signal_conn, Some(PROPS_QUEUE)),
            MessageStream::for_match_rule(rules.added.clone(), &signal_conn, Some(IFACE_QUEUE)),
            MessageStream::for_match_rule(rules.removed.clone(), &signal_conn, Some(IFACE_QUEUE)),
            MessageStream::for_match_rule(
                rules.player_props.clone(),
                &signal_conn,
                Some(IFACE_QUEUE)
            ),
        );
        let Ok((props, added, removed, player_props)) = streams else {
            need_reconnect = true;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
            continue;
        };
        // Subscribe BEFORE seeding so no `Running` / `InterfacesAdded` /
        // `InterfacesRemoved` signal can be missed between snapshot and stream
        // registration (subscribe-then-snapshot).
        let ctx = ArbiterCtx {
            bt: &bt,
            shared: &shared,
            emitter: emitter.as_ref(),
            active_tx: &active_tx,
            control_conn: &control_conn,
            cover_art: cover_art.as_ref(),
        };
        if !seeded {
            seeded = seed_from_bt(&mut state, &ctx).await;
            if !seeded {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                continue;
            }
        }
        backoff = RECONNECT_BACKOFF_BASE;

        drive_arbiter_streams(
            &mut state,
            &ctx,
            ArbiterStreams {
                props,
                added,
                removed,
                player_props,
            },
        )
        .await;

        need_reconnect = true;
        tracing::warn!("Active-source arbiter signal stream ended; reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}
