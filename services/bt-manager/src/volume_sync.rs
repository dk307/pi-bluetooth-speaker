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

use std::collections::{HashMap, HashSet, VecDeque};
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

/// Initial reconnect backoff for the volume-sync reconnect loop.
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Maximum reconnect backoff for the volume-sync reconnect loop.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Consecutive reconnect attempts after which the failure is re-reported at
/// `warn!` (intermediate failures downgrade to `debug!` to avoid flooding
/// the journal while the bus is down for a long time).
const RECONNECT_WARN_EVERY: u32 = 20;

/// Flood-controlled reconnect-failure logging: `warn!` on the first failure
/// and every [`RECONNECT_WARN_EVERY`]th, `debug!` in between.
fn log_reconnect_failure(
    stage: &'static str,
    error: &zbus::Error,
    attempt: u32,
    backoff: Duration,
) {
    if attempt == 1 || attempt.is_multiple_of(RECONNECT_WARN_EVERY) {
        tracing::warn!(
            error = %error,
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "Volume sync reconnect failed: {stage}"
        );
    } else {
        tracing::debug!(error = %error, "Volume sync reconnect retry failed: {stage}");
    }
}

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
    /// A2DP PCM object paths observed via the startup snapshot or
    /// `InterfacesAdded`. A `PropertiesChanged` volume from an untracked path
    /// is rejected: a PCM that appears mid-session can emit its default-volume
    /// signal *before* the `InterfacesAdded` handler arms suppression, and an
    /// unbiased `select!` may deliver it first.
    known_pcm_paths: HashSet<Box<str>>,
}

impl Default for VolumeSyncState {
    fn default() -> Self {
        Self {
            last_pcm_vol: None,
            last_pushed_level: None,
            last_pushed_muted: None,
            pending_echoes: VecDeque::with_capacity(ECHO_CAPACITY),
            forward_suppress_until: None,
            known_pcm_paths: HashSet::new(),
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

    /// Marks a PCM path as known and returns `true` when it was newly seen.
    ///
    /// Called for every A2DP path in the startup snapshot and for every
    /// `InterfacesAdded` path. Forward volume signals from unknown paths are
    /// rejected until the PCM's fresh-connect state is established.
    pub fn mark_pcm_known(&mut self, path: &str) -> bool {
        self.known_pcm_paths.insert(Box::<str>::from(path))
    }

    /// Returns `true` when a PCM path has been observed and is tracked.
    #[must_use]
    pub fn is_pcm_known(&self, path: &str) -> bool {
        self.known_pcm_paths.contains(path)
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
    ///
    /// The applied value is also recorded as `last_pushed_level`, mirroring
    /// [`VolumeSyncState::record_reverse_push`]: the echo consumes the
    /// audio-manager signal, so without this the stale `last_pushed_level`
    /// would make `should_push_level` skip a legitimate return to the old
    /// level (phone and amp would stay out of sync).
    pub fn record_forward_applied(&mut self, pcm_vol: u8) {
        self.last_pcm_vol = Some(pcm_vol);
        self.last_pushed_level = Some(pcm_vol);
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
    path.is_some_and(speaker_bluetooth::transport::is_a2dp_pcm_path)
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
    let a2dp = || {
        pcms.iter()
            .filter(|pcm| speaker_bluetooth::transport::is_a2dp_pcm_path(&pcm.pcm_path))
    };
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
/// Resumes a blocking-task panic, or logs an expected cancellation.
///
/// `M-PANIC-ON-BUG`: a panicked worker is a programming bug, not a recoverable
/// volume-sync failure, so the panic is re-raised on the caller's task instead
/// of being logged and forgotten.
fn handle_join_error(context: &str, err: tokio::task::JoinError) {
    if err.is_panic() {
        std::panic::resume_unwind(err.into_panic());
    }
    tracing::warn!(task = context, error = %err, "Blocking task cancelled");
}

/// Pushes one level to the `BlueALSA` PCM (phone AVRCP volume follows).
async fn push_level(bt: &BluetoothDevice, state: &mut VolumeSyncState, level: u8) {
    let bt_for_push = bt.clone();
    match tokio::task::spawn_blocking(move || bt_for_push.set_volume(level)).await {
        Ok(Ok(())) => {
            state.record_reverse_push(Some(level), None);
            tracing::info!(level, "Volume sync: audio-manager -> BlueALSA");
        }
        Ok(Err(e)) => tracing::debug!(error = %e, "Reverse volume sync failed"),
        Err(e) => handle_join_error("reverse volume sync", e),
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
        Err(e) => handle_join_error("reverse mute sync", e),
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
            handle_join_error("PCM volume read", e);
            None
        }
    }
}

/// Seeds the forward dedup state from the current PCM volume without applying
/// it, so a stale `BlueALSA` default never overrides the persisted amp level.
/// Also marks every observed PCM path as known (fresh-PCM gating).
async fn seed_pcm_state(bt: &BluetoothDevice, state: &mut VolumeSyncState) {
    let bt_for_read = bt.clone();
    match tokio::task::spawn_blocking(move || bt_for_read.link_snapshot()).await {
        Ok(Ok(snapshot)) => {
            for pcm in &snapshot.pcms {
                state.mark_pcm_known(pcm.pcm_path.as_ref());
            }
            if let Some(pcm_vol) = a2dp_volume(&snapshot.pcms) {
                state.seed_pcm_volume(pcm_vol);
                tracing::info!(
                    pcm_volume = pcm_vol,
                    "Volume sync seeded from BlueALSA (not applied)"
                );
            }
        }
        Ok(Err(e)) => tracing::debug!(error = %e, "Volume sync: PCM seed read failed"),
        Err(e) => handle_join_error("PCM seed read", e),
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

/// Subscribes all three signal streams in a single attempt.
///
/// Failure is reported to the caller, which owns the reconnect policy —
/// including tearing down and rebuilding the connection itself (zbus
/// connections do not auto-reconnect after a bus restart).
///
/// # Errors
/// Returns the first subscription error; note the earlier streams of a
/// partially successful attempt are dropped (harmless — the whole connection
/// is rebuilt on the next cycle).
/// Drives the three live signal streams (plus the watchdog) until one of
/// them errors or closes, then returns so the caller can rebuild the
/// connections.
async fn run_signal_streams(
    amp: &Proxy<'_>,
    bt: &BluetoothDevice,
    state: &mut VolumeSyncState,
    watchdog: &mut tokio::time::Interval,
    mut props_stream: MessageStream,
    mut iface_stream: MessageStream,
    mut amp_stream: MessageStream,
) {
    loop {
        // Biased polling: `InterfacesAdded` is prioritized so a fresh PCM's
        // establishment (path tracking + suppression arming) is processed
        // before any of its queued `PropertiesChanged` volume signals.
        tokio::select! {
            biased;
            signal = iface_stream.next() => {
                if handle_interfaces_added_signal(signal, amp, bt, state).await.is_err() {
                    return;
                }
            }
            _ = watchdog.tick() => {
                watchdog_tick(amp, bt, state).await;
            }
            signal = props_stream.next() => {
                if handle_bluealsa_props_signal(signal, amp, state).await.is_err() {
                    return;
                }
            }
            signal = amp_stream.next() => {
                if handle_amp_signal(signal, bt, state).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Subscribes all three signal streams in a single attempt.
async fn subscribe_streams_once(
    signal_conn: &Connection,
    rules: (MatchRule<'static>, MatchRule<'static>, MatchRule<'static>),
) -> Result<(MessageStream, MessageStream, MessageStream), zbus::Error> {
    Ok((
        MessageStream::for_match_rule(rules.0.clone(), signal_conn, Some(BLUEALSA_PROPS_QUEUE))
            .await?,
        MessageStream::for_match_rule(rules.1.clone(), signal_conn, Some(BLUEALSA_IFACE_QUEUE))
            .await?,
        MessageStream::for_match_rule(rules.2.clone(), signal_conn, Some(AUDIO_MANAGER_QUEUE))
            .await?,
    ))
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
    // Fresh-PCM gating: a PCM that appears mid-session can emit its
    // default-volume `PropertiesChanged` before the `InterfacesAdded`
    // handler arms suppression. Reject volumes from untracked paths so a
    // default can never reach audio-manager ahead of the settle window.
    if let Some(pcm_path) = &path
        && !state.is_pcm_known(pcm_path)
    {
        tracing::debug!(
            pcm = %pcm_path,
            "Volume sync skipped: PCM not yet established (no InterfacesAdded)"
        );
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
    if speaker_bluetooth::transport::is_a2dp_pcm_path(&path) {
        tracing::info!("Fresh A2DP PCM: pushing amp level, settling forward sync");
        // Track the path BEFORE arming: any of this PCM's volume signals
        // already queued (default volume included) are now gated by the
        // fresh-connect suppression window instead of being forwarded.
        state.mark_pcm_known(&path);
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
    seed_pcm_state(&bt, &mut state).await;
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

    // Signal-Listener Reconnect Loop: if a stream dies or setup fails, tear
    // down and rebuild the CONNECTIONS from scratch after a backoff — zbus
    // connections do not auto-reconnect, so retrying subscriptions on a stale
    // connection would fail forever (e.g. after `systemctl restart dbus`).
    let mut backoff = RECONNECT_BACKOFF_BASE;
    let mut failures: u32 = 0;
    let mut signal_conn = signal_conn;
    let mut control_conn = control_conn;

    loop {
        if failures > 0 {
            match tokio::try_join!(Connection::system(), Connection::system()) {
                Ok((sig, ctrl)) => {
                    signal_conn = sig;
                    control_conn = ctrl;
                }
                Err(e) => {
                    failures += 1;
                    log_reconnect_failure("D-Bus connection", &e, failures, backoff);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                    continue;
                }
            }
        }

        let amp = match build_amp_proxy(&control_conn).await {
            Ok(amp) => amp,
            Err(e) => {
                failures += 1;
                log_reconnect_failure("audio-manager proxy", &e, failures, backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                continue;
            }
        };

        if failures > 0 {
            // Reconnect state reset: PCMs may have been recreated while the
            // listener was disconnected (their `InterfacesAdded` was missed),
            // so re-snapshot the paths, re-arm fresh-PCM protection, and
            // force a reverse reconcile before accepting forward updates.
            seed_pcm_state(&bt, &mut state).await;
            state.arm_fresh_pcm(Instant::now());
            reconcile_reverse(&amp, &bt, &mut state).await;
            tracing::info!("Volume sync state re-armed after D-Bus reconnect");
        }

        match subscribe_streams_once(
            &signal_conn,
            (
                props_rule.clone(),
                interfaces_rule.clone(),
                amp_rule.clone(),
            ),
        )
        .await
        {
            Ok((props_stream, iface_stream, amp_stream)) => {
                backoff = RECONNECT_BACKOFF_BASE;
                run_signal_streams(
                    &amp,
                    &bt,
                    &mut state,
                    &mut watchdog,
                    props_stream,
                    iface_stream,
                    amp_stream,
                )
                .await;
                // A stream died (bus hiccup or connection closure); the
                // `failures > 0` guard above rebuilds the connections next
                // cycle instead of re-subscribing on a possibly-stale socket.
                failures = 1;
                tracing::warn!("Volume sync signal stream ended; rebuilding D-Bus connections");
            }
            Err(e) => {
                failures += 1;
                log_reconnect_failure("signal subscription", &e, failures, backoff);
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}
