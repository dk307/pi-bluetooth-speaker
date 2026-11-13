//! Event-driven volume synchronization between `BlueALSA` and audio-manager.
//!
//! Two independent D-Bus connections are used:
//!
//! - a **signal** connection that only feeds the three [`MessageStream`]s
//!   (`BlueALSA` `PropertiesChanged`, `BlueALSA` `InterfacesAdded`, and
//!   audio-manager `PropertiesChanged`);
//! - a **control** connection used for every method call and property read
//!   (`SetLevel`/`SetMute`, `Level`/`Muted`).
//!
//! Keeping them separate is a correctness requirement, not a preference: zbus
//! broadcasts every incoming message to each subscription queue with
//! `broadcast_direct(...).await`, so a saturated queue blocks the socket
//! reader. If method calls shared that connection, the reader could be blocked
//! on a full queue while the task awaited a method reply — the reply can never
//! be read and the task deadlocks (observed live after bursts of phone volume
//! changes).
//!
//! The state machine ([`VolumeSyncState`]) is pure and host-testable; only the
//! thin D-Bus/signal loop touches the bus.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use speaker_bluetooth::{BlueAlsaPcm, BluetoothDevice};
use zbus::message::Body;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

/// Number of queued `BlueALSA` `PropertiesChanged` messages.
///
/// Large enough that a busy phone volume drag cannot block the signal socket
/// reader; signals are cheap to buffer and the loop drains them immediately.
const BLUEALSA_PROPS_QUEUE: usize = 512;
/// Number of queued audio-manager `PropertiesChanged` messages.
const AUDIO_MANAGER_QUEUE: usize = 256;
/// Number of queued `BlueALSA` `InterfacesAdded` messages.
const BLUEALSA_IFACE_QUEUE: usize = 32;

/// Fresh-connect settle window during which PCM volume signals are ignored.
///
/// `BlueALSA` initializes a new PCM's `Volume` to max (or zero) before AVRCP
/// negotiates the phone's real level. The amp's persisted level owns the
/// initial state; forward syncs resume once the window expires.
pub const FRESH_PCM_SETTLE: Duration = Duration::from_secs(2);

/// Watchdog period for missed-signal reconciliation and heartbeat logging.
pub const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum number of queued forward `Level` echoes.
///
/// The oldest echo is dropped when the queue is full; the watchdog repairs
/// any missed echo classification.
pub const ECHO_CAPACITY: usize = 32;

/// Tolerance applied when matching an audio-manager `Level` signal against a
/// queued echo.
///
/// A forward `SetLevel(p)` can read back as `p ± 1` because the raw register
/// has 1 dB granularity while the perceptual curve steps by about 0.6 dB.
const ECHO_TOLERANCE: u8 = 1;

/// Pure state machine for the volume sync directions.
///
/// Forward (phone → amp): dedup the `BlueALSA` PCM volume, suppress the
/// fresh-connect window, and remember our own audio-manager echoes.
/// Reverse (amp → phone): dedup the level/mute values already pushed to
/// `BlueALSA`.
#[derive(Debug)]
pub struct VolumeSyncState {
    /// Last PCM percentage applied to, or seeded from, audio-manager.
    last_pcm_vol: Option<u8>,
    /// Last level pushed to the `BlueALSA` PCM.
    last_pushed_level: Option<u8>,
    /// Last mute state pushed to the `BlueALSA` PCM.
    last_pushed_muted: Option<bool>,
    /// Forward `SetLevel` values awaiting their audio-manager echo.
    pending_echoes: VecDeque<u8>,
    /// End of the fresh-connect suppression window, if armed.
    forward_suppress_until: Option<Instant>,
}

impl Default for VolumeSyncState {
    fn default() -> Self {
        Self {
            last_pcm_vol: None,
            last_pushed_level: None,
            last_pushed_muted: None,
            pending_echoes: VecDeque::with_capacity(ECHO_CAPACITY),
            forward_suppress_until: None,
        }
    }
}

impl VolumeSyncState {
    /// Creates an empty state machine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the forward dedup state from the live PCM volume.
    ///
    /// The value is recorded only; it is never applied to the amplifier. A
    /// stale `BlueALSA` default (max/zero before AVRCP negotiation) must not
    /// win over the amp's persisted level.
    pub fn seed_pcm_volume(&mut self, pcm_vol: u8) {
        self.last_pcm_vol = Some(pcm_vol);
    }

    /// Records a successful amp → `BlueALSA` push.
    ///
    /// The pushed level is also recorded as the last PCM volume so the
    /// `BlueALSA` `Volume` signal echoing this write is not forwarded back.
    pub fn record_reverse_push(&mut self, level: Option<u8>, muted: Option<bool>) {
        if let Some(level) = level {
            self.last_pcm_vol = Some(level);
            self.last_pushed_level = Some(level);
        }
        if let Some(muted) = muted {
            self.last_pushed_muted = Some(muted);
        }
    }

    /// Returns `true` when `pcm_vol` is a genuine forward change to apply.
    #[must_use]
    pub fn should_forward_pcm(&self, pcm_vol: u8, now: Instant) -> bool {
        !self.suppression_active(now) && self.last_pcm_vol != Some(pcm_vol)
    }

    /// Records a PCM volume that was successfully applied to audio-manager
    /// and queues its `Level` echo.
    pub fn record_forward_applied(&mut self, pcm_vol: u8) {
        self.last_pcm_vol = Some(pcm_vol);
        if self.pending_echoes.len() >= ECHO_CAPACITY {
            self.pending_echoes.pop_front();
        }
        self.pending_echoes.push_back(pcm_vol);
    }

    /// Consumes one queued echo if `level` matches it within tolerance.
    ///
    /// Returns `true` when the signal was our own forward write and must not
    /// be pushed back to the phone.
    pub fn take_echo(&mut self, level: u8) -> bool {
        let Some(position) = self
            .pending_echoes
            .iter()
            .position(|echo| echo.abs_diff(level) <= ECHO_TOLERANCE)
        else {
            return false;
        };
        self.pending_echoes.remove(position);
        true
    }

    /// Arms the fresh-connect window and forces the next reverse reconcile to
    /// push the amp's level and mute state to the new PCM.
    pub fn arm_fresh_pcm(&mut self, now: Instant) {
        self.last_pcm_vol = None;
        self.pending_echoes.clear();
        self.last_pushed_level = None;
        self.last_pushed_muted = None;
        self.forward_suppress_until = Some(now + FRESH_PCM_SETTLE);
    }

    /// Returns `true` while the fresh-connect window is active.
    #[must_use]
    pub fn suppression_active(&self, now: Instant) -> bool {
        self.forward_suppress_until.is_some_and(|until| now < until)
    }

    /// Returns `true` when `level` is a speaker-side change to push.
    #[must_use]
    pub fn should_push_level(&self, level: u8) -> bool {
        self.last_pushed_level != Some(level)
    }

    /// Returns `true` when `muted` is a speaker-side change to push.
    #[must_use]
    pub fn should_push_muted(&self, muted: bool) -> bool {
        self.last_pushed_muted != Some(muted)
    }

    /// Last PCM percentage applied to, or seeded from, audio-manager.
    #[must_use]
    pub fn last_pcm_vol(&self) -> Option<u8> {
        self.last_pcm_vol
    }

    /// Last level pushed to the `BlueALSA` PCM.
    #[must_use]
    pub fn last_pushed_level(&self) -> Option<u8> {
        self.last_pushed_level
    }

    /// Last mute state pushed to the `BlueALSA` PCM.
    #[must_use]
    pub fn last_pushed_muted(&self) -> Option<bool> {
        self.last_pushed_muted
    }
}

/// Predicate: does this D-Bus `PropertiesChanged` signal announce a `BlueALSA`
/// A2DP PCM volume change?
///
/// Pure and host-testable; path/interface/key parsing never touches D-Bus.
#[must_use]
pub fn is_bluealsa_volume_signal<'a>(
    path: Option<&'a str>,
    interface: &str,
    mut changed_keys: impl Iterator<Item = &'a str>,
) -> bool {
    path.is_some_and(|p| p.contains("a2dpsnk"))
        && interface == "org.bluealsa.PCM1"
        && changed_keys.any(|k| k == "Volume")
}

/// Predicate: does this D-Bus `PropertiesChanged` signal announce an
/// audio-manager amp level/mute change?
///
/// Pure and host-testable; path/interface/key parsing never touches D-Bus.
#[must_use]
pub fn is_audio_manager_amp_signal<'a>(
    interface: &str,
    mut changed_keys: impl Iterator<Item = &'a str>,
) -> bool {
    interface == "org.speaker.AudioManager1" && changed_keys.any(|k| k == "Level" || k == "Muted")
}

/// Extracts the object path from an `InterfacesAdded` signal body.
///
/// The signal header path is always the `ObjectManager` path
/// (`/org/bluealsa`), so the newly added object lives in the body's first
/// field (`oa{sa{sv}}`).
#[must_use]
pub fn interfaces_added_path(body: &Body) -> Option<Box<str>> {
    type Interfaces = HashMap<String, HashMap<String, OwnedValue>>;
    let (path, _interfaces) = body.deserialize::<(OwnedObjectPath, Interfaces)>().ok()?;
    Some(path.as_str().into())
}

/// Extracts the per-channel volume byte array from a D-Bus property value.
///
/// Pure and host-testable; no D-Bus round-trips.
#[must_use]
pub fn volume_bytes_from_value(value: &Value<'_>) -> Option<Vec<u8>> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .map(|v| match v {
                Value::U8(b) => Some(*b),
                _ => None,
            })
            .collect(),
        Value::U8(b) => Some(vec![*b]),
        _ => None,
    }
}

/// Returns the active A2DP sink PCM volume percentage, if any.
///
/// Prefers a running transport, then falls back to any A2DP sink PCM. HFP and
/// other PCMs are ignored so a voice call cannot change the music volume.
#[must_use]
pub fn a2dp_volume(pcms: &[BlueAlsaPcm]) -> Option<u8> {
    let a2dp = || pcms.iter().filter(|pcm| pcm.pcm_path.contains("a2dpsnk"));
    a2dp()
        .find(|pcm| pcm.running)
        .or_else(|| a2dp().next())
        .and_then(|pcm| pcm.volume_percent)
}

/// Applies one PCM volume percentage to audio-manager, deduplicated.
async fn apply_pcm_volume(
    amp: &Proxy<'_>,
    state: &mut VolumeSyncState,
    pcm_vol: u8,
) -> Result<(), zbus::Error> {
    amp.call_method("SetLevel", &pcm_vol).await?;
    state.record_forward_applied(pcm_vol);
    tracing::info!(
        pcm_volume = pcm_vol,
        percent = pcm_vol,
        "Volume sync: BlueALSA PCM -> audio-manager"
    );
    Ok(())
}

/// Pushes one level to the `BlueALSA` PCM (propagating to the phone via AVRCP).
async fn push_level(bt: &BluetoothDevice, state: &mut VolumeSyncState, level: u8) {
    let bt_for_push = bt.clone();
    match tokio::task::spawn_blocking(move || bt_for_push.set_volume(level)).await {
        Ok(Ok(())) => {
            state.record_reverse_push(Some(level), None);
            tracing::info!(level, "Volume sync: audio-manager -> BlueALSA");
        }
        Ok(Err(e)) => tracing::debug!(error = %e, "Reverse volume sync failed"),
        Err(e) => tracing::debug!(error = %e, "Reverse volume sync task panicked"),
    }
}

/// Pushes one mute state to the `BlueALSA` PCM (phone AVRCP mute follows).
async fn push_muted(bt: &BluetoothDevice, state: &mut VolumeSyncState, muted: bool) {
    let bt_for_push = bt.clone();
    match tokio::task::spawn_blocking(move || bt_for_push.set_mute(muted)).await {
        Ok(Ok(())) => {
            state.record_reverse_push(None, Some(muted));
            tracing::info!(muted, "Mute sync: audio-manager -> BlueALSA");
        }
        Ok(Err(e)) => tracing::debug!(error = %e, "Reverse mute sync failed"),
        Err(e) => tracing::debug!(error = %e, "Reverse mute sync task panicked"),
    }
}

/// Reads audio-manager `Level`/`Muted` and pushes genuine changes to `BlueALSA`.
///
/// Used at startup, on a fresh PCM, and by the watchdog. Values are recorded
/// only after a successful push, so a failed push is retried later.
async fn reconcile_reverse(amp: &Proxy<'_>, bt: &BluetoothDevice, state: &mut VolumeSyncState) {
    let level: Option<u8> = amp.get_property("Level").await.ok();
    let muted: Option<bool> = amp.get_property("Muted").await.ok();
    if let Some(level) = level.filter(|&value| state.should_push_level(value)) {
        push_level(bt, state, level).await;
    }
    if let Some(muted) = muted.filter(|&value| state.should_push_muted(value)) {
        push_muted(bt, state, muted).await;
    }
}

/// Reads the live A2DP PCM volume from `BlueALSA`.
async fn read_pcm_volume(bt: &BluetoothDevice) -> Option<u8> {
    let bt_for_read = bt.clone();
    match tokio::task::spawn_blocking(move || bt_for_read.link_snapshot()).await {
        Ok(Ok(snapshot)) => a2dp_volume(&snapshot.pcms),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "Volume sync: PCM read failed");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "Volume sync: PCM read task panicked");
            None
        }
    }
}

/// Seeds the forward dedup state from the current PCM volume without applying
/// it, so a stale `BlueALSA` default never overrides the persisted amp level.
async fn seed_pcm_volume(bt: &BluetoothDevice, state: &mut VolumeSyncState) {
    if let Some(pcm_vol) = read_pcm_volume(bt).await {
        state.seed_pcm_volume(pcm_vol);
        tracing::info!(
            pcm_volume = pcm_vol,
            "Volume sync seeded from BlueALSA (not applied)"
        );
    }
}

/// Watchdog tick: heartbeat, reverse reconciliation, and forward repair of
/// missed `BlueALSA` volume signals.
async fn watchdog_tick(amp: &Proxy<'_>, bt: &BluetoothDevice, state: &mut VolumeSyncState) {
    tracing::debug!(
        last_pcm_vol = ?state.last_pcm_vol(),
        last_pushed_level = ?state.last_pushed_level(),
        last_pushed_muted = ?state.last_pushed_muted(),
        "Volume sync watchdog tick"
    );
    reconcile_reverse(amp, bt, state).await;

    let now = Instant::now();
    if state.suppression_active(now) {
        return;
    }
    if let Some(pcm_vol) = read_pcm_volume(bt).await
        && state.should_forward_pcm(pcm_vol, now)
        && let Err(e) = apply_pcm_volume(amp, state, pcm_vol).await
    {
        tracing::debug!(error = %e, "Watchdog forward volume sync failed");
    }
}

/// Runs the volume sync task forever.
///
/// The caller must supply two independent connections: one for signal
/// subscriptions and one for method calls. See the module documentation for
/// why sharing a single connection can deadlock.
/// Builds the audio-manager proxy on the control connection.
///
/// # Errors
/// Returns the zbus error when proxy construction fails.
async fn build_amp_proxy(control_conn: &Connection) -> Result<Proxy<'_>, zbus::Error> {
    zbus::proxy::Builder::new(control_conn)
        .destination("org.speaker.AudioManager")?
        .path("/org/speaker/amp")?
        .interface("org.speaker.AudioManager1")?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// Builds the three volume-sync signal match rules (`BlueALSA` property
/// changes, `BlueALSA` interface additions, `audio-manager` property changes).
///
/// Returns `None` (with an `error!` log) when any rule cannot be built.
fn build_match_rules() -> Option<(MatchRule<'static>, MatchRule<'static>, MatchRule<'static>)> {
    let props_rule = MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build);
    let interfaces_rule = MatchRule::builder()
        .sender("org.bluealsa")
        .and_then(|b| b.interface("org.freedesktop.DBus.ObjectManager"))
        .and_then(|b| b.member("InterfacesAdded"))
        .map(zbus::match_rule::Builder::build);
    let amp_rule = MatchRule::builder()
        .sender("org.speaker.AudioManager")
        .and_then(|b| b.path("/org/speaker/amp"))
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build);
    let (props_rule, interfaces_rule, amp_rule) = match (props_rule, interfaces_rule, amp_rule) {
        (Ok(props), Ok(interfaces), Ok(amp)) => (props, interfaces, amp),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            tracing::error!(error = %e, "Failed to build signal match rule");
            return None;
        }
    };
    Some((props_rule, interfaces_rule, amp_rule))
}

/// Subscribes all three signal streams, retrying with backoff until every
/// subscription succeeds (Signal-Listener Reconnect Loop, no hot-spin).
async fn subscribe_streams(
    signal_conn: &Connection,
    rules: (MatchRule<'static>, MatchRule<'static>, MatchRule<'static>),
) -> (MessageStream, MessageStream, MessageStream) {
    const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
    loop {
        match (
            MessageStream::for_match_rule(rules.0.clone(), signal_conn, Some(BLUEALSA_PROPS_QUEUE))
                .await,
            MessageStream::for_match_rule(rules.1.clone(), signal_conn, Some(BLUEALSA_IFACE_QUEUE))
                .await,
            MessageStream::for_match_rule(rules.2.clone(), signal_conn, Some(AUDIO_MANAGER_QUEUE))
                .await,
        ) {
            (Ok(props), Ok(interfaces), Ok(amp)) => {
                return (props, interfaces, amp);
            }
            (Err(e), _, _) => {
                tracing::warn!(error = %e, "Failed to subscribe to BlueALSA PropertiesChanged");
            }
            (_, Err(e), _) => {
                tracing::warn!(error = %e, "Failed to subscribe to BlueALSA InterfacesAdded");
            }
            (_, _, Err(e)) => {
                tracing::warn!(error = %e, "Failed to subscribe to audio-manager PropertiesChanged");
            }
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// Handles one `BlueALSA` `PropertiesChanged` signal: parses the PCM volume and
/// forwards it to audio-manager when not deduplicated. Returns `Err(())` when
/// the caller must reconnect (stream error/closed).
async fn handle_bluealsa_props_signal(
    signal: Option<Result<zbus::Message, zbus::Error>>,
    amp: &Proxy<'_>,
    state: &mut VolumeSyncState,
) -> Result<(), ()> {
    let Some(Ok(signal)) = signal else {
        tracing::warn!("BlueALSA volume signal stream closed; reconnecting");
        return Err(());
    };
    let path = signal.header().path().map(std::string::ToString::to_string);
    let body = signal.body();
    let Ok((interface, props, _invalidated)) =
        body.deserialize::<(Box<str>, HashMap<String, Value<'_>>, Vec<String>)>()
    else {
        return Ok(());
    };
    if !is_bluealsa_volume_signal(
        path.as_deref(),
        &interface,
        props.keys().map(String::as_str),
    ) {
        return Ok(());
    }
    let Some(pcm_vol) = props
        .get("Volume")
        .and_then(volume_bytes_from_value)
        .as_deref()
        .and_then(|bytes| speaker_bluetooth::format_volume_bytes(bytes, None).2)
    else {
        tracing::debug!("Volume property present but unparseable");
        return Ok(());
    };
    if !state.should_forward_pcm(pcm_vol, Instant::now()) {
        tracing::debug!(
            pcm_volume = pcm_vol,
            "Volume sync skipped: unchanged or fresh-connect settle"
        );
        return Ok(());
    }
    if let Err(e) = apply_pcm_volume(amp, state, pcm_vol).await {
        tracing::debug!(error = %e, "Volume sync failed");
    }
    Ok(())
}

/// Handles one audio-manager `PropertiesChanged` signal: echoes are skipped,
/// speaker-side level/mute changes are pushed to the phone. Returns
/// `Err(())` when the caller must reconnect.
async fn handle_amp_signal(
    signal: Option<Result<zbus::Message, zbus::Error>>,
    bt: &BluetoothDevice,
    state: &mut VolumeSyncState,
) -> Result<(), ()> {
    let Some(Ok(signal)) = signal else {
        tracing::warn!("audio-manager signal stream closed; reconnecting");
        return Err(());
    };
    let body = signal.body();
    let Ok((interface, props, _invalidated)) =
        body.deserialize::<(Box<str>, HashMap<String, Value<'_>>, Vec<String>)>()
    else {
        return Ok(());
    };
    if !is_audio_manager_amp_signal(&interface, props.keys().map(String::as_str)) {
        return Ok(());
    }
    if let Some(level) = props.get("Level").and_then(|v| v.try_into().ok()) {
        if state.take_echo(level) {
            tracing::debug!(level, "Volume sync echo skipped");
        } else if state.should_push_level(level) {
            push_level(bt, state, level).await;
        }
    }
    if let Some(muted) = props.get("Muted").and_then(|v| v.try_into().ok())
        && state.should_push_muted(muted)
    {
        push_muted(bt, state, muted).await;
    }
    Ok(())
}

/// Handles one `BlueALSA` `InterfacesAdded` signal: a fresh A2DP PCM arms the
/// fresh-connect settle window and reconciles the amp level. Returns
/// `Err(())` when the caller must reconnect.
async fn handle_interfaces_added_signal(
    signal: Option<Result<zbus::Message, zbus::Error>>,
    amp: &Proxy<'_>,
    bt: &BluetoothDevice,
    state: &mut VolumeSyncState,
) -> Result<(), ()> {
    let Some(Ok(signal)) = signal else {
        tracing::warn!("BlueALSA InterfacesAdded stream closed; reconnecting");
        return Err(());
    };
    let Some(path) = interfaces_added_path(&signal.body()) else {
        return Ok(());
    };
    if path.contains("a2dpsnk") {
        tracing::info!("Fresh A2DP PCM: pushing amp level, settling forward sync");
        state.arm_fresh_pcm(Instant::now());
        reconcile_reverse(amp, bt, state).await;
    }
    Ok(())
}

/// Runs the volume sync task forever.
///
/// The caller must supply two independent connections: one for signal
/// subscriptions and one for method calls. See the module documentation for
/// why sharing a single connection can deadlock.
pub(crate) async fn run_task(
    bt: BluetoothDevice,
    signal_conn: Connection,
    control_conn: Connection,
) {
    tracing::info!(
        bluealsa_props_queue = BLUEALSA_PROPS_QUEUE,
        audio_manager_queue = AUDIO_MANAGER_QUEUE,
        interfaces_queue = BLUEALSA_IFACE_QUEUE,
        "Volume sync task started (dedicated signal/control connections)"
    );

    let amp = match build_amp_proxy(&control_conn).await {
        Ok(amp) => amp,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create audio-manager proxy; volume sync disabled");
            return;
        }
    };

    let mut state = VolumeSyncState::new();
    seed_pcm_volume(&bt, &mut state).await;
    reconcile_reverse(&amp, &bt, &mut state).await;

    let Some((props_rule, interfaces_rule, amp_rule)) = build_match_rules() else {
        tracing::error!("Failed to build signal match rules; volume sync disabled");
        return;
    };

    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Discard the immediate tick so the first watchdog pass runs one period
    // after startup instead of racing the initial catch-up.
    watchdog.tick().await;

    // Signal-Listener Reconnect Loop: if a stream dies or setup fails, rebuild
    // it from scratch after a backoff instead of hot-spinning.
    loop {
        let (mut props_stream, mut iface_stream, mut amp_stream) = subscribe_streams(
            &signal_conn,
            (
                props_rule.clone(),
                interfaces_rule.clone(),
                amp_rule.clone(),
            ),
        )
        .await;

        loop {
            tokio::select! {
                _ = watchdog.tick() => {
                    watchdog_tick(&amp, &bt, &mut state).await;
                }
                signal = props_stream.next() => {
                    if handle_bluealsa_props_signal(signal, &amp, &mut state).await.is_err() {
                        break;
                    }
                }
                signal = amp_stream.next() => {
                    if handle_amp_signal(signal, &bt, &mut state).await.is_err() {
                        break;
                    }
                }
                signal = iface_stream.next() => {
                    if handle_interfaces_added_signal(signal, &amp, &bt, &mut state).await.is_err() {
                        break;
                    }
                }
            }
        }

        // Backoff before rebuilding the match rules / streams.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
