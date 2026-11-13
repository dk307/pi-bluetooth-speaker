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

/// amp-volume worker idle-wait cadence on the command queue (a cheap local
/// condvar wait, no D-Bus traffic; bounds worst-case command latency).
pub const AMP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Fallback amp state read cadence while idle, used only when no
/// `PropertiesChanged` signal arrives (e.g. a dropped or silently wedged
/// signal stream).
///
/// The primary wake-up path is the audio-manager `PropertiesChanged`
/// signal, so this is a low-frequency safety net, not the intended cadence.
pub const AMP_FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
#[derive(Debug, Clone, Default)]
pub struct AmpVolumeCommandQueue {
    inner: Arc<(Mutex<VecDeque<AmpVolumeCommand>>, Condvar)>,
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
            .push_back(command);
        cv.notify_all();
    }

    /// Drains every queued command (coalescing is the caller's concern).
    #[must_use]
    pub fn drain(&self) -> Vec<AmpVolumeCommand> {
        let (lock, cv) = &*self.inner;
        let mut queue = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.is_empty() {
            // Wait until a command lands or the poll cadence elapses, then
            // re-check so a command pushed during the wait is returned
            // immediately instead of on the next tick.
            let (guard, _timeout) = cv
                .wait_timeout(queue, AMP_POLL_INTERVAL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue = guard;
        }
        queue.drain(..).collect()
    }

    /// Returns `true` if no command is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    Hardware {
        proxy: zbus::blocking::Proxy<'static>,
        /// Wakes the worker when audio-manager emits `PropertiesChanged`
        /// for the amp (phone-initiated sync), replacing an idle poll.
        change_wakeup: Arc<ChangeWakeup>,
    },
    Mock(Arc<Mutex<MockState>>),
}

/// Condvar pair used by the signal-watcher thread to wake the worker when a
/// D-Bus `PropertiesChanged` signal arrives.
#[derive(Default, Debug)]
struct ChangeWakeup {
    pending: Mutex<bool>,
    cv: Condvar,
}

impl ChangeWakeup {
    /// Signals the worker that a change notification arrived.
    fn notify(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *pending = true;
        self.cv.notify_all();
    }

    /// Waits until a change notification arrives or `timeout` elapses.
    /// Returns `true` when a (possibly coalesced) notification arrived.
    fn wait(&self, timeout: Duration) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*pending {
            let (guard, timed_out) = self
                .cv
                .wait_timeout(pending, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending = guard;
            if timed_out.timed_out() && !*pending {
                return false;
            }
        }
        *pending = false;
        true
    }
}

impl AmpVolumeClient {
    /// Connects to the system bus and targets `org.speaker.AudioManager1`.
    ///
    /// # Errors
    /// Returns an error when the system bus is unreachable.
    pub fn new_blocking() -> zbus::Result<Self> {
        let conn = zbus::blocking::Connection::system()?;
        let proxy = zbus::blocking::Proxy::new_owned(
            conn.clone(),
            "org.speaker.AudioManager",
            "/org/speaker/amp",
            "org.speaker.AudioManager1",
        )?;
        let change_wakeup = Arc::new(ChangeWakeup::default());
        spawn_signal_watcher(&conn, Arc::clone(&change_wakeup));
        Ok(Self {
            backend: AmpVolumeBackend::Hardware {
                proxy,
                change_wakeup,
            },
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

    /// Waits for an amp-change notification, up to `timeout`.
    ///
    /// The mock backend has no signal source, so it just waits out the
    /// timeout (the caller's fallback read then dedups against `last`).
    fn wait_for_change(&self, timeout: Duration) {
        match &self.backend {
            AmpVolumeBackend::Hardware { change_wakeup, .. } => {
                change_wakeup.wait(timeout);
            }
            AmpVolumeBackend::Mock(_) => {
                std::thread::sleep(timeout);
            }
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
            AmpVolumeBackend::Hardware { proxy: amp, .. } => {
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
            AmpVolumeBackend::Hardware { proxy: amp, .. } => {
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
/// for an immediate state read, replacing the former 4 Hz idle poll with an
/// event-driven one (2 Hz→0.5 Hz worst-case fallback remains as a safety
/// net). The thread exits when the connection dies; the worker's fallback
/// poll keeps the UI converging until the process is restarted.
fn spawn_signal_watcher(conn: &zbus::blocking::Connection, wakeup: Arc<ChangeWakeup>) {
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
        .spawn(move || {
            for message in iterator {
                match message {
                    Ok(_) => wakeup.notify(),
                    Err(e) => {
                        tracing::debug!(error = %e, "amp-volume signal stream errored");
                        break;
                    }
                }
            }
            tracing::debug!("amp-volume signal watcher ended (connection closed)");
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
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        let drained = commands.drain();
        if drained.is_empty() {
            // Idle: wait for an audio-manager PropertiesChanged wakeup
            // (phone-initiated sync via bt-manager) instead of hot-polling,
            // with a low-frequency fallback read as a safety net. Republish
            // only on change.
            client.wait_for_change(AMP_FALLBACK_POLL_INTERVAL);
            let now = client.state();
            match now {
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
