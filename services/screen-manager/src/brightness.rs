//! Screen-initiated display brightness commands routed to the `autobright` daemon.
//!
//! The brightness overlay must never write backlight sysfs directly: the
//! `autobright` daemon is the single writer, so competing writes (and false
//! external-override detection) are impossible. A dedicated worker thread owns
//! the blocking D-Bus connection; the render thread only pushes commands into
//! a condvar-backed queue and reads authoritative state delivered through the
//! shared update channel (`M-NO-DBUS-ON-RENDER-THREAD`).

use std::backtrace::Backtrace;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use std::time::Duration;

use zbus::MatchRule;

/// Bus name of the auto-brightness daemon.
pub const AUTOBRIGHT_BUS_NAME: &str = "org.speaker.Autobright";
/// Object path exported by the auto-brightness daemon.
pub const AUTOBRIGHT_PATH: &str = "/org/speaker/autobright";
/// D-Bus interface implemented by the auto-brightness daemon.
pub const AUTOBRIGHT_INTERFACE: &str = "org.speaker.Autobright1";

/// Worker idle wait: short enough that an optimistic overlay change converges
/// quickly even when the daemon's `PropertiesChanged` signal is unavailable.
pub const BRIGHTNESS_IDLE_WAIT: Duration = Duration::from_millis(500);
/// Reconnect delay for the properties signal iterator after the bus drops.
pub const BRIGHTNESS_SIGNAL_RETRY: Duration = Duration::from_millis(500);
/// Window in which an optimistic slider value is not overwritten by an older
/// authoritative snapshot (`M-PANIC-ON-BUG` settle-window convergence).
pub const BRIGHTNESS_SETTLE_WINDOW: Duration = Duration::from_millis(1_500);

/// A single brightness mutation for the `autobright` daemon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BrightnessCommand {
    /// Sets the manual brightness percent (`0.0..=100.0`).
    SetPercent(f32),
    /// Enables or disables the ambient-driven auto curve.
    SetAuto(bool),
    /// Drops the learned manual gamma bias.
    ResetBias,
}

/// Cloneable queue bridging render-thread overlay callbacks to the worker.
#[derive(Debug, Clone, Default)]
pub struct BrightnessCommandQueue {
    inner: Arc<(Mutex<QueueState>, Condvar)>,
}

#[derive(Debug, Default)]
struct QueueState {
    commands: VecDeque<BrightnessCommand>,
    change_pending: bool,
}

impl BrightnessCommandQueue {
    /// Creates an empty command queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one command and wakes the worker immediately.
    pub fn push(&self, command: BrightnessCommand) {
        let (lock, cv) = &*self.inner;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .push_back(command);
        cv.notify_all();
    }

    /// Wakes the worker after a `PropertiesChanged` signal from the daemon.
    pub fn notify_change(&self) {
        let (lock, cv) = &*self.inner;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.change_pending = true;
        cv.notify_all();
    }

    /// Drains every queued command without waiting.
    #[must_use]
    pub fn drain(&self) -> Vec<BrightnessCommand> {
        self.drain_idle(Duration::ZERO).0
    }

    /// Waits up to `timeout` for work, then drains every queued command.
    #[must_use]
    pub fn drain_idle(&self, timeout: Duration) -> (Vec<BrightnessCommand>, bool) {
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
}

/// Authoritative brightness snapshot mirrored on the UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrightnessState {
    /// Whether ambient-auto control is enabled.
    pub auto_enabled: bool,
    /// Current brightness as a percent of panel maximum.
    pub brightness_percent: f32,
    /// Latest ambient reading in lux.
    pub ambient_lux: f32,
    /// Current manual gamma bias applied to the auto curve.
    pub manual_gamma: f32,
}

impl BrightnessState {
    /// Builds a snapshot, sanitizing non-finite numeric fields to safe defaults.
    #[must_use]
    pub fn new(
        auto_enabled: bool,
        brightness_percent: f64,
        ambient_lux: f64,
        manual_gamma: f64,
    ) -> Self {
        let finite = |v: f64, fallback: f32| {
            if v.is_finite() {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "daemon reports bounded percentages/lux/gamma"
                )]
                let narrowed = v as f32;
                narrowed
            } else {
                fallback
            }
        };
        Self {
            auto_enabled,
            brightness_percent: finite(brightness_percent, 0.0).clamp(0.0, 100.0),
            ambient_lux: finite(ambient_lux, 0.0).max(0.0),
            manual_gamma: finite(manual_gamma, 1.0).max(0.0),
        }
    }
}

/// Mock control handle for [`BrightnessClient::new_mocked`].
#[derive(Debug, Clone, Default)]
pub struct BrightnessMockCtrl {
    state: Arc<Mutex<MockState>>,
}

impl BrightnessMockCtrl {
    /// Returns every command the client has applied, in order.
    #[must_use]
    pub fn executed(&self) -> Vec<BrightnessCommand> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .executed
            .clone()
    }

    /// Injects a failure into the next command apply.
    pub fn fail_next_write(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail_next = true;
    }

    /// Overwrites the authoritative mocked state (simulates a daemon change).
    pub fn set_state(&self, state: BrightnessState) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .state = Some(state);
    }
}

#[derive(Debug, Default)]
struct MockState {
    state: Option<BrightnessState>,
    executed: Vec<BrightnessCommand>,
    fail_next: bool,
}

/// Client to the `org.speaker.Autobright1` D-Bus API (or a mock).
#[derive(Debug)]
pub struct BrightnessClient {
    backend: BrightnessBackend,
}

#[derive(Debug)]
enum BrightnessBackend {
    /// Blocking proxy whose internal `Arc` keeps the connection alive.
    Hardware(zbus::blocking::Proxy<'static>),
    /// Deterministic in-memory backend used by host tests.
    Mock(Arc<Mutex<MockState>>),
}

impl BrightnessClient {
    /// Connects to the system bus and targets the auto-brightness daemon.
    ///
    /// # Errors
    /// Returns an error when the system bus is unreachable.
    pub fn new_blocking() -> zbus::Result<Self> {
        let conn = zbus::blocking::Connection::system()?;
        Self::with_connection(&conn)
    }

    /// Builds the hardware client against an arbitrary connected bus.
    ///
    /// # Errors
    /// Returns the zbus error when proxy construction fails.
    pub fn with_connection(conn: &zbus::blocking::Connection) -> zbus::Result<Self> {
        let proxy = zbus::blocking::proxy::Builder::new(conn)
            .destination(AUTOBRIGHT_BUS_NAME)?
            .path(AUTOBRIGHT_PATH)?
            .interface(AUTOBRIGHT_INTERFACE)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()?;
        Ok(Self {
            backend: BrightnessBackend::Hardware(proxy),
        })
    }

    /// Builds a fully mocked client plus its controller.
    #[must_use]
    pub fn new_mocked(initial: BrightnessState) -> (Self, BrightnessMockCtrl) {
        let ctrl = BrightnessMockCtrl {
            state: Arc::new(Mutex::new(MockState {
                state: Some(initial),
                executed: Vec::new(),
                fail_next: false,
            })),
        };
        (
            Self {
                backend: BrightnessBackend::Mock(Arc::clone(&ctrl.state)),
            },
            ctrl,
        )
    }

    /// Returns the connection behind the hardware proxy, if any.
    fn connection(&self) -> Option<&zbus::blocking::Connection> {
        match &self.backend {
            BrightnessBackend::Hardware(proxy) => Some(proxy.connection()),
            BrightnessBackend::Mock(_) => None,
        }
    }

    /// Reads the daemon's authoritative brightness snapshot.
    ///
    /// # Errors
    /// Returns an error on D-Bus or mock failure.
    #[expect(
        clippy::result_large_err,
        reason = "canonical error with backtrace for debuggability"
    )]
    pub fn state(&self) -> Result<BrightnessState, BrightnessError> {
        match &self.backend {
            BrightnessBackend::Hardware(daemon) => {
                let raw: (bool, f64, f64, f64) =
                    daemon.call("GetState", &()).map_err(BrightnessError::bus)?;
                Ok(BrightnessState::new(raw.0, raw.1, raw.2, raw.3))
            }
            BrightnessBackend::Mock(state) => state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state
                .ok_or_else(|| BrightnessError::failed("mock brightness state unset")),
        }
    }

    /// Applies one command and returns the post-write authoritative state.
    ///
    /// # Errors
    /// Returns an error on D-Bus or mock failure; callers must republish the
    /// last known-good state so an optimistic overlay flip reverts.
    #[expect(
        clippy::result_large_err,
        reason = "canonical error with backtrace for debuggability"
    )]
    pub fn apply(&self, command: BrightnessCommand) -> Result<BrightnessState, BrightnessError> {
        match &self.backend {
            BrightnessBackend::Hardware(daemon) => {
                match command {
                    BrightnessCommand::SetPercent(percent) => {
                        let clamped = if percent.is_finite() {
                            percent.clamp(0.0, 100.0)
                        } else {
                            0.0
                        };
                        daemon
                            .call::<_, _, ()>("SetManualPercent", &(f64::from(clamped),))
                            .map_err(BrightnessError::bus)?;
                    }
                    BrightnessCommand::SetAuto(enabled) => {
                        daemon
                            .call::<_, _, ()>("SetAutoEnabled", &(enabled,))
                            .map_err(BrightnessError::bus)?;
                    }
                    BrightnessCommand::ResetBias => {
                        daemon
                            .call::<_, _, ()>("ResetManualBias", &())
                            .map_err(BrightnessError::bus)?;
                    }
                }
                self.state()
            }
            BrightnessBackend::Mock(state) => {
                let mut lock = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if lock.fail_next {
                    lock.fail_next = false;
                    return Err(BrightnessError::failed("injected mock write failure"));
                }
                if let Some(inner) = lock.state.as_mut() {
                    match command {
                        BrightnessCommand::SetPercent(percent) => {
                            inner.brightness_percent = if percent.is_finite() {
                                percent.clamp(0.0, 100.0)
                            } else {
                                0.0
                            };
                            inner.auto_enabled = false;
                            inner.manual_gamma = 1.0;
                        }
                        BrightnessCommand::SetAuto(enabled) => inner.auto_enabled = enabled,
                        BrightnessCommand::ResetBias => inner.manual_gamma = 1.0,
                    }
                }
                lock.executed.push(command);
                lock.state
                    .ok_or_else(|| BrightnessError::failed("mock brightness state unset"))
            }
        }
    }
}

/// Canonical error for the brightness client/worker.
#[derive(Debug)]
pub struct BrightnessError {
    kind: BrightnessErrorKind,
    backtrace: Backtrace,
}

#[derive(Debug)]
enum BrightnessErrorKind {
    /// D-Bus transport or remote-call failure.
    Bus(zbus::Error),
    /// Any other failure surfaced as a message.
    Failed(Box<str>),
}

impl BrightnessError {
    fn bus(source: zbus::Error) -> Self {
        Self {
            kind: BrightnessErrorKind::Bus(source),
            backtrace: Backtrace::capture(),
        }
    }

    fn failed(message: &str) -> Self {
        Self {
            kind: BrightnessErrorKind::Failed(Box::from(message)),
            backtrace: Backtrace::capture(),
        }
    }

    fn fmt_kind(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            BrightnessErrorKind::Bus(e) => write!(f, "brightness D-Bus error: {e}"),
            BrightnessErrorKind::Failed(msg) => write!(f, "brightness error: {msg}"),
        }
    }
}

speaker_common::impl_canonical_error_display!(BrightnessError);

impl std::error::Error for BrightnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            BrightnessErrorKind::Bus(e) => Some(e),
            BrightnessErrorKind::Failed(_) => None,
        }
    }
}

impl From<zbus::Error> for BrightnessError {
    fn from(e: zbus::Error) -> Self {
        Self::bus(e)
    }
}

/// Spawns the `brightness-signal` watcher, reconnecting with backoff whenever
/// the blocking iterator ends so a bus restart cannot permanently blind the UI.
fn spawn_signal_watcher(
    conn: &zbus::blocking::Connection,
    queue: &BrightnessCommandQueue,
    running: &Arc<AtomicBool>,
) {
    let Ok(rule) = MatchRule::builder()
        .sender(AUTOBRIGHT_BUS_NAME)
        .and_then(|b| {
            b.path(AUTOBRIGHT_PATH)?
                .interface("org.freedesktop.DBus.Properties")?
                .member("PropertiesChanged")
        })
        .map(zbus::match_rule::Builder::build)
    else {
        tracing::warn!("Failed to build brightness signal match rule; falling back to polling");
        return;
    };
    let owned_conn = conn.clone();
    let spawned = std::thread::Builder::new()
        .name("brightness-signal".into())
        .spawn({
            let queue = queue.clone();
            let running = Arc::clone(running);
            move || {
                while running.load(Ordering::Relaxed) {
                    let iterator = zbus::blocking::MessageIterator::for_match_rule(
                        rule.clone(),
                        &owned_conn,
                        Some(8),
                    );
                    let Ok(iterator) = iterator else {
                        std::thread::sleep(BRIGHTNESS_SIGNAL_RETRY);
                        continue;
                    };
                    for message in iterator {
                        if message.is_ok() {
                            queue.notify_change();
                        } else {
                            tracing::debug!("brightness signal stream errored");
                            break;
                        }
                    }
                    std::thread::sleep(BRIGHTNESS_SIGNAL_RETRY);
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "Failed to spawn brightness signal watcher");
    }
}

/// Coalesces a drained batch to the last percent and auto value, preserving
/// explicit bias resets.
#[must_use]
pub fn coalesce(drained: Vec<BrightnessCommand>) -> Vec<BrightnessCommand> {
    let mut percent: Option<usize> = None;
    let mut auto: Option<usize> = None;
    let mut out: Vec<BrightnessCommand> = Vec::new();
    for command in drained {
        match command {
            BrightnessCommand::SetPercent(_) => {
                if let Some(idx) = percent {
                    out[idx] = command;
                } else {
                    percent = Some(out.len());
                    out.push(command);
                }
            }
            BrightnessCommand::SetAuto(_) => {
                if let Some(idx) = auto {
                    out[idx] = command;
                } else {
                    auto = Some(out.len());
                    out.push(command);
                }
            }
            BrightnessCommand::ResetBias => out.push(command),
        }
    }
    out
}

/// Publishes a state snapshot into the shared update, recovering a poisoned guard.
fn publish(state: BrightnessState, shared: &Mutex<crate::daemon::BtSharedUpdate>) {
    let mut lock = shared.lock().unwrap_or_else(|p| {
        tracing::warn!("BtSharedUpdate mutex poisoned; recovering inner state");
        p.into_inner()
    });
    lock.brightness = Some(state);
}

/// Worker loop: applies overlay commands and republishes authoritative state.
pub fn run_worker(
    client: &BrightnessClient,
    shared: &Mutex<crate::daemon::BtSharedUpdate>,
    commands: &BrightnessCommandQueue,
    running: &Arc<AtomicBool>,
) {
    let mut last: Option<BrightnessState> = client.state().ok();
    if let Some(state) = last {
        publish(state, shared);
    }
    if let Some(conn) = client.connection() {
        spawn_signal_watcher(conn, commands, running);
    }
    while running.load(Ordering::Relaxed) {
        let (drained, _changed) = commands.drain_idle(BRIGHTNESS_IDLE_WAIT);
        if drained.is_empty() {
            match client.state() {
                Ok(state) if last != Some(state) => {
                    tracing::info!(
                        auto_enabled = state.auto_enabled,
                        brightness_percent = state.brightness_percent,
                        "Brightness externally changed; publishing"
                    );
                    last = Some(state);
                    publish(state, shared);
                }
                Err(e) => tracing::debug!(error = %e, "Brightness state read failed (idle poll)"),
                _ => {}
            }
            continue;
        }
        for command in coalesce(drained) {
            match client.apply(command) {
                Ok(state) => {
                    tracing::info!(
                        auto_enabled = state.auto_enabled,
                        brightness_percent = state.brightness_percent,
                        "Brightness command applied"
                    );
                    last = Some(state);
                    publish(state, shared);
                }
                Err(e) => {
                    tracing::warn!(error = %e, ?command,
                        "Brightness command failed; reverting optimistic overlay");
                    if let Some(state) = last {
                        // Force a republish even if the shell's optimistic value
                        // equals `last`, so the overlay converges (settle window).
                        publish(state, shared);
                    }
                }
            }
        }
    }
    tracing::info!("Brightness worker stopped");
}
