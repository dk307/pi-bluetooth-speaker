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

/// amp-volume worker read-back poll cadence while idle.
pub const AMP_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
        let drained: Vec<_> = queue.drain(..).collect();
        if drained.is_empty() {
            // Wait until a command lands or the poll cadence elapses; the
            // returned guard is dropped to release the lock.
            let (_guard, _timeout) = cv
                .wait_timeout(queue, AMP_POLL_INTERVAL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drained
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
    /// Blocking system-bus proxy; each method call opens on the shared
    /// connection owned by this worker.
    Hardware(zbus::blocking::Connection),
    Mock(Arc<Mutex<MockState>>),
}

impl AmpVolumeClient {
    /// Connects to the system bus and targets `org.speaker.AudioManager1`.
    ///
    /// # Errors
    /// Returns an error when the system bus is unreachable.
    pub fn new_blocking() -> zbus::Result<Self> {
        let conn = zbus::blocking::Connection::system()?;
        Ok(Self {
            backend: AmpVolumeBackend::Hardware(conn),
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
            AmpVolumeBackend::Hardware(conn) => {
                let amp = amp_proxy(conn)?;
                Ok(AmpVolumeState::new(amp.level()?, amp.muted()?))
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
            AmpVolumeBackend::Hardware(conn) => {
                let amp = amp_proxy(conn)?;
                match command {
                    AmpVolumeCommand::SetLevel(percent) => {
                        amp.set_level(percent)?;
                    }
                    AmpVolumeCommand::SetMute(muted) => {
                        amp.set_mute(muted)?;
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

#[expect(
    clippy::result_large_err,
    reason = "canonical error with backtrace for debuggability"
)]
fn amp_proxy(
    conn: &zbus::blocking::Connection,
) -> Result<VolumeProxyProxyBlocking<'_>, VolumeError> {
    VolumeProxyProxyBlocking::new(conn).map_err(VolumeError::bus)
}

/// Typed client proxy over the `audio-manager` amp interface, driven via the
/// blocking zbus API from the dedicated `amp-volume` worker thread.
#[zbus::proxy(
    interface = "org.speaker.AudioManager1",
    default_service = "org.speaker.AudioManager",
    default_path = "/org/speaker/amp"
)]
trait VolumeProxy {
    /// Reads the amp level percent.
    fn level(&self) -> zbus::Result<u8>;
    /// Reads the amp mute state.
    fn muted(&self) -> zbus::Result<bool>;
    /// Sets the amp level percent.
    fn set_level(&self, percent: u8) -> zbus::Result<()>;
    /// Sets the amp mute state.
    fn set_mute(&self, muted: bool) -> zbus::Result<()>;
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
            // Idle: poll for external changes (phone-initiated sync via
            // bt-manager) and republish only on change.
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
