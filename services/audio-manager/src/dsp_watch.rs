//! DSP stream-lifecycle watcher (plan §11.3).
//!
//! Polls the CamillaDSP engine state over the existing WebSocket client
//! ([`speaker_dsp::DspDevice`]) and translates **lifecycle edges** into
//! `StreamStarting`/`StreamStopping` commands on the controller's command
//! channel. The PLL handshake in [`crate::amp_controller`] remains the
//! authoritative clock gate regardless of watcher timeliness.
//!
//! Design (amended after the §11.3 design review):
//!
//! - **2-state collapse:** engine states map to
//!   [`EmittedLifecycle::{Stopped, Running}`]; `Stalled`, `Paused`, and
//!   `Inactive` all collapse to `Stopped`, so `Stalled→Paused→Inactive`
//!   emits at most one stop edge (zero dedup logic beyond the compare).
//! - **Bootstrap:** `current = Stopped` — a first poll seeing `Running`
//!   emits exactly one `StreamStarting`; a first poll seeing `Paused`
//!   emits nothing (stays boot-muted).
//! - **`Stalled` debounce:** `Stalled` is capture-side starvation (a
//!   transient BT stall would bounce `Stalled→Running` and cause a full
//!   mute + handshake dropout), so a `Stalled`-originated stop needs two
//!   consecutive `Stalled` polls. `Paused`/`Inactive` are immediate.
//! - **No-flap on telemetry failure (§4.5):** WS errors and
//!   `Starting`/`Unknown` never mutate the lifecycle — the last known
//!   state holds; the PLL-loss path covers real clock death.
//! - **Adaptive interval:** poll at half the configured idle interval while
//!   the lifecycle is `Running` (the stop edge can only arrive then).

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use speaker_dsp::{DspDevice, ProcessingState};

use crate::amp_controller::{AmpCommand, AmpCommandKind};

/// Minimum effective poll interval (guards a zero/1 ms config value).
/// Lower bound for the adaptive poll interval (exposed for the test suite,
/// M-TEST-UTIL).
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Internal 2-state lifecycle (plan §11.3). Everything but engine
/// `Running` collapses to [`EmittedLifecycle::Stopped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedLifecycle {
    /// No stream (engine `Paused`/`Inactive`/`Stalled`/`Starting`/unknown,
    /// or CamillaDSP unreachable).
    Stopped,
    /// Engine `Running` (a stream is flowing through the pipeline).
    Running,
}

/// How a freshly read engine state maps onto the lifecycle.
enum MappedState {
    /// `ProcessingState::Running`.
    Running,
    /// `Paused`/`Inactive`: immediate stop.
    StoppedNow,
    /// `Stalled`: capture-side starvation; needs a 1-poll confirmation.
    Stalled,
    /// `Starting`/`Unknown`: transient — hold the last lifecycle.
    Hold,
}

const fn map_state(state: &ProcessingState) -> MappedState {
    match state {
        ProcessingState::Running => MappedState::Running,
        ProcessingState::Paused | ProcessingState::Inactive => MappedState::StoppedNow,
        ProcessingState::Stalled => MappedState::Stalled,
        // `Starting` is a transient on the way to Running (or a config
        // reload mid-play); `Unknown` is forward-compat. Neither proves a
        // stream edge — hold the last lifecycle and let the next poll
        // resolve (PLL-loss handling covers a real clock death meanwhile).
        ProcessingState::Starting | ProcessingState::Unknown(_) => MappedState::Hold,
    }
}

/// Edge-detection state machine for the watcher (plan §11.3). Pure and
/// host-testable: [`Self::step`] folds one engine-state reading into the
/// lifecycle and returns the command to emit, if any.
#[derive(Debug)]
pub struct LifecycleTracker {
    current: EmittedLifecycle,
    /// First `Stalled` poll seen since the last non-`Stalled` state (the
    /// 1-poll confirmation debounce for `Stalled`-originated stops).
    pending_stalled: bool,
}

impl LifecycleTracker {
    /// Boot state: `Stopped` (the amp boots muted; plan §11.3 bootstrap).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: EmittedLifecycle::Stopped,
            pending_stalled: false,
        }
    }

    /// Current lifecycle (last successfully folded reading).
    #[must_use]
    pub const fn lifecycle(&self) -> EmittedLifecycle {
        self.current
    }

    /// Fold one engine-state reading; returns the lifecycle edge command to
    /// emit, if any. WS errors never reach here — callers skip folding on
    /// read failure entirely.
    pub fn step(&mut self, state: &ProcessingState) -> Option<AmpCommandKind> {
        let next = match map_state(state) {
            MappedState::Running => EmittedLifecycle::Running,
            MappedState::StoppedNow => EmittedLifecycle::Stopped,
            MappedState::Stalled => {
                if self.pending_stalled {
                    // Second consecutive `Stalled`: confirmed stop.
                    self.pending_stalled = false;
                    EmittedLifecycle::Stopped
                } else {
                    self.pending_stalled = true;
                    debug!("Stalled (first poll); debouncing before emitting a stop");
                    return None;
                }
            }
            // Hold: no mutation, no edge (§4.5).
            MappedState::Hold => return None,
        };
        self.pending_stalled = false;
        if next == self.current {
            return None;
        }
        self.current = next;
        Some(match next {
            EmittedLifecycle::Running => AmpCommandKind::StreamStarting,
            EmittedLifecycle::Stopped => AmpCommandKind::StreamStopping,
        })
    }
}

impl Default for LifecycleTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Effective poll interval for the current lifecycle (adaptive: half the
/// idle interval while `Running`, floored at [`MIN_POLL_INTERVAL`]).
///
/// Exposed unconditionally (M-TEST-UTIL): pure interval math used by the
/// integration test suite to pin the adaptive-poll contract.
#[must_use]
pub fn poll_interval(idle: Duration, current: EmittedLifecycle) -> Duration {
    if current == EmittedLifecycle::Running {
        let half_ms = idle.as_millis() / 2;
        let floor = MIN_POLL_INTERVAL.as_millis();
        if half_ms <= floor {
            MIN_POLL_INTERVAL
        } else {
            Duration::from_millis(half_ms as u64)
        }
    } else if idle < MIN_POLL_INTERVAL {
        MIN_POLL_INTERVAL
    } else {
        idle
    }
}

/// Re-issue `StreamStarting` roughly every [`START_RETRY_POLLS`] Running
/// polls (~2 s at the 250 ms adaptive running poll) until
/// [`MAX_START_RETRIES`] attempts, then give up (warn once) until the
/// lifecycle leaves `Running`.
const START_RETRY_POLLS: u32 = 8;
const MAX_START_RETRIES: u32 = 15;

/// Run the watcher until the command channel closes (the receiver is owned
/// by [`crate::amp_controller::AmpController::run`], so dropping the
/// controller ends the watcher — no separate shutdown plumbing).
///
/// Only the engine STATE is read each poll (single `GetState` query): a
/// failure in ancillary telemetry (rate adjust, buffer level, …) can
/// never block a start/stop edge.
///
/// Failed starts are retried (bounded): the watcher sends `StreamStarting`
/// fire-and-forget and cannot observe the controller's handshake outcome,
/// so while the DSP stays `Running` the edge is re-issued every
/// [`START_RETRY_POLLS`] polls up to [`MAX_START_RETRIES`] times. A resend
/// on an already-playing controller is a cheap no-op.
///
/// # Errors/Naming
/// Hardware/WS faults are logged (state held, §4.5) and never close the
/// loop; the loop exits only when the command channel closes.
pub async fn run(device: DspDevice, commands: mpsc::Sender<AmpCommand>, idle_interval: Duration) {
    let mut tracker = LifecycleTracker::new();
    info!(
        idle_interval_ms = idle_interval.as_millis() as u64,
        "DSP stream-lifecycle watcher started (emits StreamStarting/Stopping edges)"
    );
    let mut telemetry_failed = false;
    let mut polls_since_start: u32 = 0;
    let mut start_retries: u32 = 0;
    loop {
        // The receiver lives in the controller loop; when it is dropped
        // (daemon shutdown), exit promptly even without a pending edge.
        if commands.is_closed() {
            info!("Command channel closed; DSP watcher exiting");
            return;
        }
        tokio::time::sleep(poll_interval(idle_interval, tracker.lifecycle())).await;
        match device.read_state_async().await {
            Ok(state) => {
                if telemetry_failed {
                    telemetry_failed = false;
                    info!("DSP telemetry recovered");
                }
                let edge = tracker.step(&state);
                if tracker.lifecycle() == EmittedLifecycle::Running {
                    if edge.is_some() {
                        polls_since_start = 0;
                    } else {
                        polls_since_start += 1;
                        if polls_since_start >= START_RETRY_POLLS
                            && start_retries < MAX_START_RETRIES
                        {
                            polls_since_start = 0;
                            start_retries += 1;
                            debug!(
                                retry = start_retries,
                                "Re-issuing StreamStarting (start not yet confirmed)"
                            );
                            if commands
                                .send(AmpCommand::fire_and_forget(AmpCommandKind::StreamStarting))
                                .await
                                .is_err()
                            {
                                info!("Command channel closed; DSP watcher exiting");
                                return;
                            }
                        }
                    }
                } else {
                    // Stopped lifecycle: the retry budget resets for the
                    // next start episode.
                    start_retries = 0;
                    polls_since_start = 0;
                }
                if let Some(kind) = edge {
                    info!(?kind, "DSP lifecycle edge");
                    if commands
                        .send(AmpCommand::fire_and_forget(kind))
                        .await
                        .is_err()
                    {
                        info!("Command channel closed; DSP watcher exiting");
                        return;
                    }
                }
            }
            Err(e) => {
                // §4.5: a transient telemetry hiccup must hold the last
                // known lifecycle, not flap the amp. warn on transition
                // into the failure state, debug on repeats (journal
                // discipline at 250 ms cadence).
                if !telemetry_failed {
                    telemetry_failed = true;
                    warn!(error = %e, "DSP telemetry read failed; holding last lifecycle");
                } else {
                    debug!(error = %e, "DSP telemetry read still failing");
                }
            }
        }
    }
}

/// The lifecycle tracker is shared across the async watcher task and
/// command production; compile-time proof of thread-safety.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LifecycleTracker>();
};
