//! Screen-initiated volume commands routed to the `audio-manager` amp.
//!
//! The on-screen slider, rotary encoder, and mute toggle must drive the
//! **amplifier**, not the Bluetooth PCM transport: the phone keeps owning
//! PCM volume (`bt-manager` syncs `BlueALSA` ↔ `audio-manager`), while the
//! screen owns the amp attenuation itself.
//!
//! A dedicated `amp-volume` worker thread owns its own blocking system-bus
//! connection so it is fully independent of Bluetooth availability: the
//! render loop always spawns it, even in mock-BT/headless sessions. Commands
//! flow through a condvar-backed queue (cloneable render-thread producer);
//! authoritative `AmpVolumeState` is published into the shared
//! [`crate::daemon::BtSharedUpdate`] after every command **and** after every
//! read-back, so an optimistic UI change that failed to reach audio-manager
//! reverts instead of sticking.
//!
//! `AudioManager.SetLevel(0)` mutes the amp; the queue layer additionally
//! pushes `SetMute(false)` after a `SetLevel` issued while muted so changing
//! the volume with the slider audibly unmutes, matching common amplifier UX.

use std::backtrace::Backtrace;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use zbus::MatchRule;

/// amp-volume worker idle-wait timeout on the command queue condvar.
///
/// The primary wake-up path is a `push` (encoder/slider command) or a
/// `notify_change` (audio-manager `PropertiesChanged` signal, i.e.
/// phone-initiated sync), so this bounds only the worst case where the
/// signal stream is wedged or dead — a low-frequency safety-net state read,
/// not the intended cadence.
pub const AMP_IDLE_WAIT: Duration = Duration::from_secs(2);

/// A single volume or mute mutation for the `audio-manager` amp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmpVolumeCommand {
    /// Sets the amp level (percent `0..=100`; `0` is the hard-mute anchor).
    SetLevel(u8),
    /// Sets or clears the amp serial mute.
    SetMute(bool),
}

/// Cloneable command queue bridging render-thread UI callbacks to the
/// dedicated `amp-volume` worker.
///
/// Both wake-up sources share one condvar: `push` (a UI command) and
/// `notify_change` (the `PropertiesChanged` signal watcher) wake the same
/// idle wait, so a command pushed mid-wait is applied immediately instead of
/// sitting behind a separate change-notification wait.
#[derive(Debug, Clone, Default)]
pub struct AmpVolumeCommandQueue {
    inner: Arc<(Mutex<QueueState>, Condvar)>,
}

#[derive(Debug, Default)]
struct QueueState {
    commands: VecDeque<AmpVolumeCommand>,
    /// Set by the signal watcher; consumed by `drain_idle`.
    change_pending: bool,
}

impl AmpVolumeCommandQueue {
    /// Creates an empty command queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one command for the `amp-volume` worker, waking it immediately.
    pub fn push(&self, command: AmpVolumeCommand) {
        let (lock, cv) = &*self.inner;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .push_back(command);
        cv.notify_all();
    }

    /// Wakes the worker because audio-manager reported an amp change
    /// (phone-initiated sync). The worker's next idle wait returns early and
    /// reads the authoritative state.
    pub fn notify_change(&self) {
        let (lock, cv) = &*self.inner;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.change_pending = true;
        cv.notify_all();
    }

    /// Drains every queued command (coalescing is the caller's concern).
    #[must_use]
    pub fn drain(&self) -> Vec<AmpVolumeCommand> {
        let (lock, _) = &*self.inner;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.commands.drain(..).collect()
    }

    /// Waits up to `timeout` for a UI command or an amp-change notification,
    /// then drains.
    ///
    /// Returns the queued commands and whether a change notification (a
    /// possibly coalesced signal) arrived. Never waits when either is already
    /// pending, so the worker reacts instantly to both sources.
    #[must_use]
    pub fn drain_idle(&self, timeout: Duration) -> (Vec<AmpVolumeCommand>, bool) {
        let (lock, cv) = &*self.inner;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.commands.is_empty() && !state.change_pending {
            let (guard, _) = cv
                .wait_timeout(state, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
        }
        (
            state.commands.drain(..).collect(),
            std::mem::take(&mut state.change_pending),
        )
    }

    /// Returns `true` if no command is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .is_empty()
    }
}

/// Authoritative amplifier level/mute snapshot mirrored on the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmpVolumeState {
    /// Amplifier level percent (`0..=100`).
    pub level: u8,
    /// Whether the amp serial mute bit is set.
    pub muted: bool,
}

impl AmpVolumeState {
    /// Builds a state snapshot from a percent/mute pair clamped into range.
    #[must_use]
    pub fn new(level: u8, muted: bool) -> Self {
        Self {
            level: level.min(100),
            muted,
        }
    }
}

/// Mock control handle for [`AmpVolumeClient::new_mocked`].
#[derive(Debug, Clone, Default)]
pub struct AmpVolumeMockCtrl {
    state: Arc<Mutex<MockState>>,
}

impl AmpVolumeMockCtrl {
    /// Records every command the client received.
    #[must_use]
    pub fn executed(&self) -> Vec<AmpVolumeCommand> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .executed
            .clone()
    }

    /// Injects a failure into the next `set_level`/`set_mute` call.
    pub fn fail_next_write(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail_next = true;
    }

    /// Overwrites the authoritative mocked amp state (simulates an external
    /// change, e.g. phone-initiated volume via `bt-manager` sync).
    pub fn set_state(&self, state: AmpVolumeState) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .state = Some(state);
    }
}

#[derive(Debug, Default)]
struct MockState {
    state: Option<AmpVolumeState>,
    executed: Vec<AmpVolumeCommand>,
    fail_next: bool,
}

/// Client to the `org.speaker.AudioManager1` amp D-Bus API (or a mock).
#[derive(Debug)]
pub struct AmpVolumeClient {
    backend: AmpVolumeBackend,
}

#[derive(Debug)]
enum AmpVolumeBackend {
    /// Owned blocking proxy over the system bus, built once when the worker
    /// starts (`new_owned` keeps the connection alive via its internal Arc).
    Hardware(zbus::blocking::Proxy<'static>),
    Mock(Arc<Mutex<MockState>>),
}

impl AmpVolumeClient {
    /// Connects to the system bus and targets `org.speaker.AudioManager1`.
    ///
    /// # Errors
    /// Returns an error when the system bus is unreachable.
    pub fn new_blocking() -> zbus::Result<Self> {
        let conn = zbus::blocking::Connection::system()?;
        Self::with_connection(&conn)
    }

    /// Builds the hardware client against an arbitrary (already-connected)
    /// bus — the DI seam private-bus integration tests inject a fake
    /// `org.speaker.AudioManager1` service through.
    ///
    /// # Errors
    /// Returns the zbus error when proxy construction fails.
    pub fn with_connection(conn: &zbus::blocking::Connection) -> zbus::Result<Self> {
        // Property caching must be off: the worker's read-back after a write
        // is the authoritative revert source, and a lazily-cached value can
        // be stale until the next PropertiesChanged round-trip propagates.
        let proxy = zbus::blocking::proxy::Builder::new(conn)
            .destination("org.speaker.AudioManager")?
            .path("/org/speaker/amp")?
            .interface("org.speaker.AudioManager1")?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()?;
        Ok(Self {
            backend: AmpVolumeBackend::Hardware(proxy),
        })
    }

    /// Builds a fully mocked client plus its controller (`M-MOCKABLE-SYSCALLS`).
    #[must_use]
    pub fn new_mocked(initial: AmpVolumeState) -> (Self, AmpVolumeMockCtrl) {
        let ctrl = AmpVolumeMockCtrl {
            state: Arc::new(Mutex::new(MockState {
                state: Some(initial),
                executed: Vec::new(),
                fail_next: false,
            })),
        };
        let client = Self {
            backend: AmpVolumeBackend::Mock(Arc::clone(&ctrl.state)),
        };
        (client, ctrl)
    }

    /// Returns the shared system-bus connection behind the hardware proxy
    /// (`None` for the mock backend), used to subscribe the
    /// `PropertiesChanged` signal watcher.
    fn connection(&self) -> Option<&zbus::blocking::Connection> {
        match &self.backend {
            AmpVolumeBackend::Hardware(proxy) => Some(proxy.connection()),
            AmpVolumeBackend::Mock(_) => None,
        }
    }

    /// Reads the current authoritative amp state.
    ///
    /// # Errors
    /// Returns an error on D-Bus/mock failure.
    #[expect(
        clippy::result_large_err,
        reason = "canonical error with backtrace for debuggability"
    )]
    pub fn state(&self) -> Result<AmpVolumeState, VolumeError> {
        match &self.backend {
            AmpVolumeBackend::Hardware(amp) => {
                let level: u8 = amp.get_property("Level").map_err(VolumeError::bus)?;
                let muted: bool = amp.get_property("Muted").map_err(VolumeError::bus)?;
                Ok(AmpVolumeState::new(level, muted))
            }
            AmpVolumeBackend::Mock(state) => {
                let lock = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                lock.state
                    .ok_or_else(|| VolumeError::failed("mock amp state unset"))
            }
        }
    }

    /// Applies one command, returning the post-write read-back state.
    ///
    /// # Errors
    /// Returns an error on D-Bus/mock failure; the caller must publish the
    /// last known-good state so the UI reverts an optimistic flip.
    #[expect(
        clippy::result_large_err,
        reason = "canonical error with backtrace for debuggability"
    )]
    pub fn apply(&self, command: AmpVolumeCommand) -> Result<AmpVolumeState, VolumeError> {
        match &self.backend {
            AmpVolumeBackend::Hardware(amp) => {
                match command {
                    AmpVolumeCommand::SetLevel(percent) => {
                        amp.call::<_, _, ()>("SetLevel", &(percent,))
                            .map_err(VolumeError::bus)?;
                    }
                    AmpVolumeCommand::SetMute(muted) => {
                        amp.call::<_, _, ()>("SetMute", &(muted,))
                            .map_err(VolumeError::bus)?;
                    }
                }
                self.state()
            }
            AmpVolumeBackend::Mock(state) => {
                let mut lock = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if lock.fail_next {
                    lock.fail_next = false;
                    return Err(VolumeError::failed("injected mock write failure"));
                }
                if let Some(inner) = lock.state.as_mut() {
                    match command {
                        AmpVolumeCommand::SetLevel(percent) => inner.level = percent.min(100),
                        AmpVolumeCommand::SetMute(muted) => inner.muted = muted,
                    }
                }
                lock.executed.push(command);
                lock.state
                    .ok_or_else(|| VolumeError::failed("mock amp state unset"))
            }
        }
    }
}

/// Canonical error for the amp-volume client/worker.
#[derive(Debug)]
pub struct VolumeError {
    kind: VolumeErrorKind,
    backtrace: Backtrace,
}

#[derive(Debug)]
enum VolumeErrorKind {
    /// D-Bus transport or remote-call failure.
    Bus(zbus::Error),
    /// Any other failure surfaced as a message.
    Failed(Box<str>),
}

impl VolumeError {
    /// Wraps a D-Bus error.
    fn bus(source: zbus::Error) -> Self {
        Self {
            kind: VolumeErrorKind::Bus(source),
            backtrace: Backtrace::capture(),
        }
    }

    /// Wraps a plain failure message.
    fn failed(message: &str) -> Self {
        Self {
            kind: VolumeErrorKind::Failed(Box::from(message)),
            backtrace: Backtrace::capture(),
        }
    }

    fn fmt_kind(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            VolumeErrorKind::Bus(e) => write!(f, "amp volume D-Bus error: {e}"),
            VolumeErrorKind::Failed(msg) => write!(f, "amp volume error: {msg}"),
        }
    }
}

speaker_common::impl_canonical_error_display!(VolumeError);

impl std::error::Error for VolumeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            VolumeErrorKind::Bus(e) => Some(e),
            VolumeErrorKind::Failed(_) => None,
        }
    }
}

impl From<zbus::Error> for VolumeError {
    fn from(e: zbus::Error) -> Self {
        Self::bus(e)
    }
}

/// Spawns the `amp-volume-signal` watcher thread: it blocks on the
/// audio-manager `PropertiesChanged` match-rule stream and wakes the worker
/// (through the command queue's condvar) for an immediate state read. The
/// thread exits when the connection dies; the worker's fallback read keeps
/// the UI converging until the process is restarted.
///
/// Called from `run_worker` (not client construction) so the watcher only
/// exists while a worker is draining the queue, and so it can notify that
/// queue directly — a single condvar serves commands and change signals.
fn spawn_signal_watcher(conn: &zbus::blocking::Connection, queue: &AmpVolumeCommandQueue) {
    let rule = MatchRule::builder()
        .sender("org.speaker.AudioManager")
        .and_then(|b| {
            b.path("/org/speaker/amp")?
                .interface("org.freedesktop.DBus.Properties")?
                .member("PropertiesChanged")
        })
        .map(zbus::match_rule::Builder::build);
    let Ok(rule) = rule else {
        tracing::warn!("Failed to build amp-volume signal match rule; falling back to polling");
        return;
    };
    let Ok(iterator) = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(8)) else {
        tracing::warn!("Failed to subscribe amp-volume PropertiesChanged; falling back to polling");
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("amp-volume-signal".into())
        .spawn({
            let queue = queue.clone();
            move || {
                for message in iterator {
                    match message {
                        Ok(_) => queue.notify_change(),
                        Err(e) => {
                            tracing::debug!(error = %e, "amp-volume signal stream errored");
                            break;
                        }
                    }
                }
                tracing::debug!("amp-volume signal watcher ended (connection closed)");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "Failed to spawn amp-volume signal watcher");
    }
}

/// Worker loop: drains commands, applies them, and publishes authoritative
/// state into `shared`. Exits when `running` is cleared.
///
/// A write failure is published as a **read-back of the last known-good
/// state**, reverting the shell's optimistic flip; the failing command is
/// logged and re-attempted on the next queued command (never silently).
pub fn run_worker(
    client: &AmpVolumeClient,
    shared: &Mutex<crate::daemon::BtSharedUpdate>,
    commands: &AmpVolumeCommandQueue,
    running: &std::sync::atomic::AtomicBool,
) {
    let mut last: Option<AmpVolumeState> = None;
    publish(client.state().ok(), shared, &mut last);
    if let Some(conn) = client.connection() {
        spawn_signal_watcher(conn, commands);
    }
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        let (drained, _changed) = commands.drain_idle(AMP_IDLE_WAIT);
        if drained.is_empty() {
            // Idle: the wait returned either on a PropertiesChanged wakeup
            // (phone-initiated sync via bt-manager) or on the fallback
            // timeout. Either way, read state and republish only on change.
            match client.state() {
                Ok(state) if last != Some(state) => {
                    tracing::info!(
                        level = state.level,
                        muted = state.muted,
                        "Amp volume externally changed; publishing"
                    );
                    publish(Some(state), shared, &mut last);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Amp volume state read failed (idle poll)");
                }
                _ => {}
            }
            continue;
        }

        // Coalesce a level+unmute pair so one slider move issues one write
        // batch; SetLevel wins over SetMute ordering conflicts.
        for command in coalesce(drained) {
            match client.apply(command) {
                Ok(state) => {
                    tracing::info!(
                        level = state.level,
                        muted = state.muted,
                        "Amp volume command applied"
                    );
                    publish(Some(state), shared, &mut last);
                }
                Err(e) => {
                    tracing::warn!(error = %e, command = ?command,
                        "Amp volume command failed; reverting optimistic UI");
                    // Force republish of last known-good state even if the
                    // optimistic shell flip differs from `last`.
                    publish_force(last, shared);
                }
            }
        }
    }
    tracing::info!("Amp-volume worker stopped");
}

/// Coalesces a drained batch.
///
/// Keeps only the last `SetLevel` and the last `SetMute` value per kind, in
/// first-seen order: rapid slider drags collapse to one level write, and mute
/// flips collapse to one mute write, while the relative order of the two kinds
/// is preserved so a level command followed by a mute stays in that order.
#[must_use]
pub fn coalesce(drained: Vec<AmpVolumeCommand>) -> Vec<AmpVolumeCommand> {
    let mut level: Option<usize> = None;
    let mut mute: Option<usize> = None;
    let mut out: Vec<AmpVolumeCommand> = Vec::with_capacity(2);
    for command in drained {
        match command {
            AmpVolumeCommand::SetLevel(pct) => {
                if let Some(idx) = level {
                    out[idx] = AmpVolumeCommand::SetLevel(pct);
                } else {
                    level = Some(out.len());
                    out.push(command);
                }
            }
            AmpVolumeCommand::SetMute(m) => {
                if let Some(idx) = mute {
                    out[idx] = AmpVolumeCommand::SetMute(m);
                } else {
                    mute = Some(out.len());
                    out.push(command);
                }
            }
        }
    }
    out
}

/// Publishes `state` into the shared update when it differs from `last`.
fn publish(
    state: Option<AmpVolumeState>,
    shared: &Mutex<crate::daemon::BtSharedUpdate>,
    last: &mut Option<AmpVolumeState>,
) {
    if state != *last && state.is_some() {
        *last = state;
        publish_force(*last, shared);
    }
}

/// Publishes the current `last` state unconditionally (poison-safe).
fn publish_force(last: Option<AmpVolumeState>, shared: &Mutex<crate::daemon::BtSharedUpdate>) {
    let Some(state) = last else {
        return;
    };
    let mut lock = shared.lock().unwrap_or_else(|p| {
        tracing::warn!("BtSharedUpdate mutex poisoned; recovering inner state");
        p.into_inner()
    });
    lock.amp_volume = Some(state);
}
