//! Event-driven MA12070P controller (Phase 2 state machine, plan §3.1).
//!
//! One async event loop owns the amplifier:
//!
//! ```text
//!               SetMute(true) / StreamStopping
//!   Playing ─────────────────────────────────────► Muted
//!      ▲                                            │ StreamStarting
//!      │ PLL locked (bounded one-shot handshake)    ▼
//!      └─────────────────────────────────────  AwaitingClock
//!                                                  │ handshake timeout
//!                                                  ▼
//!    Faulted ◄── GPIO error edge / I2C fault    Muted (warn!, stay muted)
//!      │  recovery: mute → /ENABLE cycle + full
//!      │  register re-init → Muted
//! ```
//!
//! Event-driven, no polling as the primary path: `tokio::select!` over the
//! command channel (D-Bus), the pipeline lifecycle channel, the GPIO
//! error-edge channel (blocking `rppal` watch → `mpsc` bridge), and a
//! low-frequency safety tick (configurable, diagnostics only). All I2C and
//! GPIO syscalls run in `spawn_blocking`; in-memory state is mutated
//! strictly **after** successful hardware writes (transactional mutation).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use speaker_hardware::AmpFault;

use crate::amp_backend::AmpBackend;
use crate::amp_gpio::AmpGpio;
use crate::amp_state::AmpState;
use crate::error::AmpResult;
use crate::state_file::StateFile;
use amp_curve::PERCENT_TO_RAW;

/// Production PLL-lock handshake timeout (plan §3.1: ≤200 ms one-shot).
///
/// Never `cfg(test)`-shrunk: tests call [`AmpController::pll_lock_handshake`]
/// directly with short real-time values (§5.3 unconditional-constants rule).
pub const PLL_LOCK_TIMEOUT: Duration = Duration::from_millis(200);

/// Single handshake poll step.
const PLL_POLL_STEP: Duration = Duration::from_millis(10);

/// Safety-tick interval default (low-frequency diagnostics tick).
pub const SAFETY_TICK_INTERVAL: Duration = Duration::from_millis(1000);

/// Controller state machine states (plan §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlState {
    /// Serial unmuted, hardware audio path live.
    Playing,
    /// Serial muted (or pending-unmute with no stream).
    Muted,
    /// PLL-lock handshake in progress (bounded, one-shot).
    AwaitingClock,
    /// Fault recovery in progress; ends in `Muted`.
    Faulted,
}

/// Command payload (without the reply channel).
#[derive(Debug)]
pub enum AmpCommandKind {
    /// Volume change (percent 0..100); valid in every state.
    SetLevel(u8),
    /// Mute/unmute. Unmute while a stream runs triggers the PLL handshake.
    SetMute(bool),
    /// Pipeline started (event source: systemd/camilladsp watch).
    StreamStarting,
    /// Pipeline stopping; mute across the transition (no pop).
    StreamStopping,
}

/// Command envelope with an optional reply channel for synchronous D-Bus
/// callers (`oneshot` fires with the transaction result, success or fault).
#[derive(Debug)]
pub struct AmpCommand {
    /// The operation.
    pub kind: AmpCommandKind,
    /// Reply endpoint for callers awaiting the hardware result (error is
    /// stringified; the canonical error stays in the controller journal).
    pub reply: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

impl AmpCommand {
    /// Convenience constructor without a reply (internal events/tests).
    #[must_use]
    pub fn fire_and_forget(kind: AmpCommandKind) -> Self {
        Self { kind, reply: None }
    }
}

/// Controller owning backend + GPIO + persisted state.
pub struct AmpController {
    backend: Arc<dyn AmpBackend>,
    device: Arc<speaker_hardware::AmpDevice>,
    gpio: AmpGpio,
    state_file: StateFile,
    /// In-memory logical state — mutated only after successful writes.
    state: AmpState,
    ctrl: CtrlState,
    /// Remembered unmute request while no stream is running.
    pending_unmute: bool,
    /// Read-only mirror of [`Self::state`] for D-Bus property reads.
    mirror: Arc<tokio::sync::Mutex<AmpState>>,
}

impl AmpController {
    /// Build a controller over the given backend/GPIO seams (tests inject
    /// mocked pairs).
    #[must_use]
    pub fn new(
        backend: Arc<dyn AmpBackend>,
        device: Arc<speaker_hardware::AmpDevice>,
        gpio: AmpGpio,
        state_file: StateFile,
        initial: AmpState,
    ) -> Self {
        Self {
            backend,
            device,
            gpio,
            state_file,
            state: initial,
            ctrl: CtrlState::Muted,
            pending_unmute: false,
            mirror: Arc::new(tokio::sync::Mutex::new(initial)),
        }
    }

    /// Shared state mirror for D-Bus property reads (updated after every
    /// successful hardware transaction).
    #[must_use]
    pub fn mirror(&self) -> Arc<tokio::sync::Mutex<AmpState>> {
        Arc::clone(&self.mirror)
    }

    /// Current logical amp state (level + muted).
    #[must_use]
    pub fn state(&self) -> AmpState {
        self.state
    }

    /// Current state-machine state.
    #[must_use]
    pub fn ctrl_state(&self) -> CtrlState {
        self.ctrl
    }

    /// Full boot bring-up (plan §3.1 "Boot" row): wake the amp, replicate
    /// the kernel driver `probe()` configuration, reconcile the state file,
    /// leave the amp muted until the first `StreamStarting`.
    ///
    /// # Errors
    /// Returns [`AmpError`] on any bus/GPIO fault.
    pub async fn boot_init(&mut self, persisted: AmpState) -> AmpResult<()> {
        info!(
            level = persisted.level_raw,
            muted = persisted.muted,
            "Boot reconcile starting"
        );
        // 1. Wake the amp (GPIO /ENABLE LOW). With hifiberry-dac the chip
        //    boots in hardware reset: I2C writes NACK until this succeeds.
        self.gpio.wake()?;
        // Establish the known GPIO orientation: /MUTE LOW (muted) until the
        // first PLL handshake releases it.
        self.gpio.set_hardware_mute(true)?;
        // 2. Register init replicating ma120x0p probe(): standard I2S,
        //    VLP enabled, limiter enabled with -15 dBFS thresholds.
        self.device.write_i2s_format_async(0).await?;
        self.device.enable_audio_processor_async().await?;
        self.device.enable_limiter_async().await?;
        self.device.write_limiter_thresholds_async(-15).await?;
        // 3. Reconcile: hardware truth vs persisted state. On mismatch the
        //    persisted user intent wins (volume + mute bit).
        let hw = self.backend.read_state().await.unwrap_or_else(|e| {
            warn!(error = %e, "Boot telemetry read failed; assuming defaults");
            AmpState::default()
        });
        if hw.level_raw != persisted.level_raw {
            debug!(
                hw = hw.level_raw,
                persisted = persisted.level_raw,
                "Boot level reconcile"
            );
            self.backend.write_state(&persisted).await?;
        } else if hw.muted != persisted.muted {
            debug!(
                hw_muted = hw.muted,
                persisted_muted = persisted.muted,
                "Boot mute reconcile"
            );
            self.backend.set_serial_mute(persisted.muted).await?;
        }
        // 4. GPIO /MUTE stays LOW (hardware muted) until the first PLL
        //    handshake releases it; serial mute governs the audio path.
        self.state = persisted;
        self.ctrl = CtrlState::Muted;
        *self.mirror.lock().await = persisted;
        info!("Boot reconcile complete; amp awake, muted awaiting first stream");
        Ok(())
    }

    /// Handle one command (also drives the state machine in tests).
    ///
    /// # Errors
    /// Returns [`AmpError`] on hardware faults (the command is logged and
    /// the prior state preserved per §4.5 transient-error rules).
    pub async fn handle_command(&mut self, cmd: AmpCommandKind) -> AmpResult<()> {
        match cmd {
            AmpCommandKind::SetLevel(percent) => {
                let new_state = self.state.with_percent(percent, &PERCENT_TO_RAW);
                let applied = self.backend.write_state(&new_state).await?;
                self.state = applied;
                self.persist(&applied).await;
                info!(
                    percent,
                    level = applied.level_raw,
                    muted = applied.muted,
                    "SetLevel applied"
                );
            }
            AmpCommandKind::SetMute(true) => {
                self.apply_muted(true).await?;
                self.ctrl = CtrlState::Muted;
                self.pending_unmute = false;
                // GPIO /MUTE mirrors the idle (non-playing) state.
                self.gpio.set_hardware_mute(true)?;
                info!("SetMute(true) applied");
            }
            AmpCommandKind::SetMute(false) => {
                if self.ctrl == CtrlState::Playing {
                    // Brief mute->unmute cycle is idempotent; just clear the bit.
                    self.apply_muted(false).await?;
                } else if self.ctrl == CtrlState::Muted || self.ctrl == CtrlState::Faulted {
                    self.pending_unmute = true;
                    info!("Unmute requested without running stream; pending until StreamStarting");
                }
                // In AwaitingClock the handshake outcome governs.
            }
            AmpCommandKind::StreamStopping => {
                self.apply_muted(true).await?;
                self.ctrl = CtrlState::Muted;
                // Re-assert GPIO /MUTE for a pop-free standby.
                self.gpio.set_hardware_mute(true)?;
                debug!("StreamStopping applied (muted)");
            }
            AmpCommandKind::StreamStarting => {
                self.start_stream().await?;
            }
        }
        Ok(())
    }

    /// Event: GPIO error line falling edge (fault asserted).
    ///
    /// # Errors
    /// Propagated hardware faults; callers log and keep running.
    pub async fn handle_error_edge(&mut self) -> AmpResult<()> {
        info!("GPIO error line asserted; reading live error register");
        let flags = self.device.read_live_errors_async().await?;
        self.handle_faults(flags).await
    }

    /// Event: safety tick (low-frequency telemetry check).
    ///
    /// # Errors
    /// Propagated hardware faults; callers log and keep running.
    pub async fn handle_safety_tick(&mut self) -> AmpResult<()> {
        debug!("Safety tick");
        let flags = self.device.read_live_errors_async().await?;
        if flags == speaker_hardware::AmpErrorFlags::NONE && self.ctrl == CtrlState::Playing {
            return Ok(());
        }
        self.handle_faults(flags).await
    }

    /// Classify and act on live error flags (plan §3.1 GPIO edge row).
    ///
    /// # Errors
    /// Propagated hardware faults.
    pub async fn handle_faults(&mut self, flags: speaker_hardware::AmpErrorFlags) -> AmpResult<()> {
        if flags == speaker_hardware::AmpErrorFlags::NONE {
            return Ok(());
        }
        if flags.has_fault(AmpFault::HighTempWarning) && !flags.has_fault(AmpFault::ThermalShutdown)
        {
            // Thermal warn: throttle -6 dB (raw +6), keep playing.
            let throttled = self
                .state
                .level_raw
                .saturating_add(6)
                .min(AmpState::MAX_RAW);
            info!(
                from = self.state.level_raw,
                to = throttled,
                "Thermal warning; throttling -6 dB"
            );
            self.apply_level_raw(throttled).await?;
            self.device.clear_latched_errors_async().await?;
            return Ok(());
        }
        let critical = [
            AmpFault::OvercurrentProtect,
            AmpFault::DcOffsetProtect,
            AmpFault::FlyingCapOvervoltage,
            AmpFault::ThermalShutdown,
            AmpFault::PinShortCircuit,
        ];
        if critical.iter().any(|f| flags.has_fault(*f)) {
            warn!(?flags, "Critical amp fault; entering fault recovery");
            self.ctrl = CtrlState::Faulted;
            self.fault_recovery().await?;
            self.ctrl = CtrlState::Muted;
        }
        Ok(())
    }

    /// Fault recovery: mute → /ENABLE reset cycle → full register re-init
    /// → muted. Also used from boot on persistent faults.
    ///
    /// # Errors
    /// Returns [`AmpError`] if any recovery step fails.
    pub async fn fault_recovery(&mut self) -> AmpResult<()> {
        // Serial mute first (audio path off even if recovery fails).
        self.apply_muted(true).await?;
        // /ENABLE HIGH (reset) then LOW (wake) wipes all registers.
        self.gpio.wake()?;
        self.device.write_i2s_format_async(0).await?;
        self.device.enable_audio_processor_async().await?;
        self.device.enable_limiter_async().await?;
        self.device.write_limiter_thresholds_async(-15).await?;
        // Restore the user's volume; keep muted until the next StreamStarting.
        self.device
            .write_master_volume_async(self.state.level_raw)
            .await?;
        self.device.clear_latched_errors_async().await?;
        warn!("Fault recovery complete; amp re-initialized, muted awaiting stream");
        Ok(())
    }

    /// Stream starting: bounded one-shot PLL handshake, then unmute.
    ///
    /// # Errors
    /// Returns [`AmpError`] on bus faults.
    pub async fn start_stream(&mut self) -> AmpResult<()> {
        if self.ctrl == CtrlState::Playing {
            // Already live (late duplicate event); no-op.
            return Ok(());
        }
        self.ctrl = CtrlState::AwaitingClock;
        let locked = self
            .pll_lock_handshake(PLL_LOCK_TIMEOUT)
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "PLL handshake telemetry read failed; treating as unlocked");
                false
            });
        if !locked {
            warn!(
                timeout_ms = PLL_LOCK_TIMEOUT.as_millis() as u64,
                "PLL lock handshake timed out; staying muted (next StreamStarting retries)"
            );
            self.ctrl = CtrlState::Muted;
            return Ok(());
        }
        self.device.clear_latched_errors_async().await?;
        self.apply_muted(false).await?;
        self.gpio.set_hardware_mute(false)?;
        self.ctrl = if self.pending_unmute || !self.state.muted {
            self.pending_unmute = false;
            CtrlState::Playing
        } else {
            CtrlState::Muted
        };
        info!("Stream starting: PLL locked, amp unmuted");
        Ok(())
    }

    /// Bounded one-shot PLL handshake: poll live errors until
    /// `PllClockLoss` clears or `timeout` elapses. Parameterized for tests;
    /// production callers use [`PLL_LOCK_TIMEOUT`].
    ///
    /// # Errors
    /// Returns [`AmpError`] on bus faults.
    pub async fn pll_lock_handshake(&self, timeout: Duration) -> AmpResult<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let flags = self.device.read_live_errors_async().await?;
            if !flags.has_fault(AmpFault::PllClockLoss) {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(PLL_POLL_STEP).await;
        }
    }

    /// Apply the serial mute bit (audio-path mute; level untouched).
    ///
    /// # Errors
    /// Returns [`AmpError`] on hardware faults; state unchanged on failure.
    pub async fn apply_muted(&mut self, muted: bool) -> AmpResult<()> {
        let new_state = if muted {
            self.state.mute()
        } else {
            self.state.unmute()
        };
        self.backend.set_serial_mute(muted).await?;
        self.state = new_state;
        self.persist(&new_state).await;
        Ok(())
    }

    /// Write a raw level directly (fault throttle path).
    ///
    /// # Errors
    /// Returns [`AmpError`] on hardware faults; state unchanged on failure.
    pub async fn apply_level_raw(&mut self, raw: u16) -> AmpResult<()> {
        let new_state = AmpState::new(raw, self.state.muted)?;
        let applied = self.backend.write_state(&new_state).await?;
        self.state = applied;
        self.persist(&applied).await;
        Ok(())
    }

    async fn persist(&self, state: &AmpState) {
        if let Err(e) = self.state_file.save(state).await {
            warn!(error = %e, "Failed to persist state file");
        }
        *self.mirror.lock().await = *state;
    }

    /// Run the event loop: `tokio::select!` over commands, error edges, and
    /// the safety tick. Returns on channel shutdown.
    ///
    /// # Errors
    /// Hardware faults are logged (state preserved) and never close the
    /// loop; the loop exits only when the command channel closes.
    pub async fn run(
        mut self,
        mut commands: mpsc::Receiver<AmpCommand>,
        mut error_edges: mpsc::Receiver<()>,
        mut shutdown: mpsc::Receiver<()>,
        safety: bool,
    ) {
        let mut tick = tokio::time::interval(SAFETY_TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await; // first tick is immediate
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("Controller shutdown signal; exiting event loop");
                    break;
                }
                cmd = commands.recv() => {
                    let Some(cmd) = cmd else {
                        info!("Command channel closed; controller exiting");
                        break;
                    };
                    let result = self.handle_command(cmd.kind).await;
                    if let Some(reply) = cmd.reply {
                        let _ = reply
                            .send(result.as_ref().map(|_| ()).map_err(|e| e.to_string()));
                    }
                    if let Err(e) = result {
                        warn!(error = %e, "Command handling failed; prior state preserved");
                    }
                }
                edge = error_edges.recv() => {
                    if edge.is_some()
                        && let Err(e) = self.handle_error_edge().await
                    {
                        warn!(error = %e, "Error-edge handling failed");
                    }
                }
                _ = tick.tick(), if safety => {
                    if let Err(e) = self.handle_safety_tick().await {
                        debug!(error = %e, "Safety tick telemetry read failed");
                    }
                }
            }
        }
    }
}
