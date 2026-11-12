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

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use speaker_hardware::{AmpFault, LimiterSlew};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
///
/// Mute state is split in two: `state.muted` is the **persisted user
/// intent** (survives reboot; drives the state file), while
/// `hardware_muted` is the **actual electrical mute** of the audio path.
/// Transient mutes (stream stop, PLL loss, fault recovery) drive only the
/// hardware; the user intent survives so the next stream auto-resumes.
pub struct AmpController {
    backend: Arc<dyn AmpBackend>,
    device: Arc<speaker_hardware::AmpDevice>,
    gpio: AmpGpio,
    state_file: StateFile,
    /// In-memory logical state — mutated only after successful writes.
    /// `level_raw` always holds the **user** level; the thermal throttle is
    /// applied at write time via [`Self::effective_raw`] and never persisted.
    state: AmpState,
    ctrl: CtrlState,
    /// Actual electrical mute state of the audio path (serial bit + GPIO
    /// `/MUTE`). Transient mutes (stream stop, PLL loss, fault recovery)
    /// drive this WITHOUT touching the persisted user intent in
    /// [`Self::state::muted`], so the next stream auto-resumes at the
    /// user's level. [`Self::state`] `muted` is the *user's* mute request.
    hardware_muted: bool,
    /// `true` while a downstream stream is live (between `StreamStarting`
    /// and `StreamStopping`). A user mute during playback drops the
    /// controller to `Muted` WITHOUT ending the stream, so a later
    /// `SetMute(false)` can restore playback immediately.
    stream_live: bool,
    /// Active thermal-throttle attenuation in raw units (0 = none). Applied
    /// at hardware-write time so the user level survives in state + state
    /// file and is restored when the warning clears.
    thermal_offset_raw: u16,
    /// `true` once the register model (VLP, limiter, I2S, audio-in) is
    /// confirmed written. An `/ENABLE` reset wipes every register to
    /// defaults — VLP bypassed and limiter off, i.e. full-scale output — so
    /// the audio path must stay muted until [`Self::reinit_registers`]
    /// succeeds again. Unmuting with reset defaults would run 0 dB through
    /// the speakers (the failure mode boot's mute-first sequencing exists
    /// to prevent).
    regs_initialized: bool,
    /// Read-only mirror of [`Self::state`] for D-Bus property reads.
    mirror: Arc<tokio::sync::Mutex<AmpState>>,
}

/// Critical faults that warrant a full `/ENABLE` reset recovery (plan
/// §3.1): OCP, DC offset, flying-cap overvoltage, thermal shutdown, pin
/// short, PVDD undervoltage. Evaluated FIRST in every fault triage so a
/// thermal warning can never mask them.
fn has_critical_fault(flags: speaker_hardware::AmpErrorFlags) -> bool {
    const CRITICAL: [AmpFault; 6] = [
        AmpFault::OvercurrentProtect,
        AmpFault::DcOffsetProtect,
        AmpFault::FlyingCapOvervoltage,
        AmpFault::ThermalShutdown,
        AmpFault::PinShortCircuit,
        AmpFault::UndervoltagePvdd,
    ];
    CRITICAL.iter().any(|f| flags.has_fault(*f))
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
            // Boot brings the amp up muted (mute-first); the user intent
            // lives in `initial.muted`.
            hardware_muted: true,
            stream_live: false,
            thermal_offset_raw: 0,
            // Registers are unproven until boot_init's init sequence
            // completes; the audio path stays muted until then.
            regs_initialized: false,
            mirror: Arc::new(tokio::sync::Mutex::new(initial)),
        }
    }

    /// Shared state mirror for D-Bus property reads (updated after every
    /// successful hardware transaction).
    #[must_use]
    pub fn mirror(&self) -> Arc<tokio::sync::Mutex<AmpState>> {
        Arc::clone(&self.mirror)
    }

    /// Current logical amp state (level + the **user's mute intent**;
    /// transient hardware mutes are not reflected here — see
    /// [`Self::hardware_muted`] in the struct docs).
    #[must_use]
    pub fn state(&self) -> AmpState {
        self.state
    }

    /// Current state-machine state.
    #[must_use]
    pub fn ctrl_state(&self) -> CtrlState {
        self.ctrl
    }

    /// Full boot bring-up (plan §3.1 "Boot" row), kernel-parity sequence:
    /// booster HIGH + PVDD settle → `/ENABLE` LOW + settle → serial mute
    /// asserted immediately (register reset defaults are unmuted at 0 dB) →
    /// `probe()`-equivalent register init → state reconcile (always written; a
    /// failed telemetry read must never leave reset defaults live) → amp
    /// muted until the first `StreamStarting`.
    ///
    /// # Errors
    /// Returns [`AmpError`] on any bus/GPIO fault.
    pub async fn boot_init(&mut self, persisted: AmpState) -> AmpResult<()> {
        info!(
            level = persisted.level_raw,
            muted = persisted.muted,
            "Boot reconcile starting"
        );
        // 1. Kernel-parity power-up: booster on + 200 ms PVDD settle, then
        //    /ENABLE LOW + 50 ms settle (with hifiberry-dac the chip boots
        //    in hardware reset and NACKs writes until woken).
        let gpio = self.gpio.clone();
        tokio::task::spawn_blocking(move || gpio.power_up())
            .await
            .context("GPIO power-up task panicked")??;
        // 2. Mute FIRST: register reset defaults are unmuted at 0 dB; never
        //    touch any other register before the audio path is off.
        self.backend.set_serial_mute(true).await?;
        self.gpio.set_hardware_mute(true)?;
        // 3. Register init replicating ma120x0p probe(): standard I2S,
        //    VLP enabled, limiter enabled with -15 dBFS thresholds.
        self.reinit_registers().await?;
        // 4. Reconcile: log the hardware snapshot for diagnostics, then
        //    ALWAYS write the hardware state. Treating a failed read as
        //    "defaults" would skip the write and leave reset defaults
        //    (0 dB, unmuted) live — a speaker-damaging failure mode.
        match self.backend.read_state().await {
            Ok(hw) => debug!(
                hw = hw.level_raw,
                hw_muted = hw.muted,
                persisted = persisted.level_raw,
                "Boot telemetry snapshot"
            ),
            Err(e) => warn!(error = %e, "Boot telemetry read failed; forcing state write"),
        }
        // The SERIAL mute is forced ON at boot regardless of the user's
        // mute intent: `persisted.muted == false` is user intent for the
        // NEXT stream, not a license to unmute the audio path while no
        // clock is running. Only the first PLL handshake releases it.
        // (Writing `persisted` directly would clear 0x36 bit 7 and leave
        // the DSP unmuted without a locked PLL until the first stream.)
        let boot_hw = AmpState::new(persisted.level_raw, true)?;
        self.backend.write_state(&boot_hw).await?;
        // 5. GPIO /MUTE stays LOW (hardware muted) until the first PLL
        //    handshake releases it; the serial mute mirrors the boot mute,
        //    NOT the persisted user intent (kept verbatim in `self.state`
        //    so `start_stream()` resumes at the user's mute preference).
        self.hardware_muted = true;
        self.state = persisted;
        self.ctrl = CtrlState::Muted;
        // The register model is now proven written; only an `/ENABLE`
        // reset (fault recovery) invalidates it again.
        self.regs_initialized = true;
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
                // Hardware receives the thermally effective level and the
                // ACTUAL hardware mute (transient mutes are separate from
                // the persisted user intent).
                let effective = new_state.level_raw.saturating_sub(self.thermal_offset_raw);
                let hw_state = AmpState::new(effective, self.hardware_muted)?;
                self.backend.write_state(&hw_state).await?;
                self.state = new_state;
                self.persist(&new_state).await;
                info!(
                    percent,
                    level = new_state.level_raw,
                    muted = new_state.muted,
                    "SetLevel applied"
                );
            }
            AmpCommandKind::SetMute(true) => {
                self.apply_muted(true).await?;
                self.ctrl = CtrlState::Muted;
                // GPIO /MUTE mirrors the mute; stream_live is unchanged so
                // the stream keeps running muted and a later unmute can
                // restore it without a new handshake.
                self.gpio.set_hardware_mute(true)?;
                info!("SetMute(true) applied");
            }
            AmpCommandKind::SetMute(false) => {
                if self.ctrl == CtrlState::Playing
                    || (self.ctrl == CtrlState::Muted && self.stream_live)
                {
                    // Stream still live: clear the serial bit AND release
                    // /MUTE (mirrored both directions), back to Playing.
                    self.apply_muted(false).await?;
                    self.gpio.set_hardware_mute(false)?;
                    self.ctrl = CtrlState::Playing;
                    self.stream_live = true;
                    info!("SetMute(false) applied while the stream is live");
                } else if self.ctrl == CtrlState::Muted || self.ctrl == CtrlState::Faulted {
                    // No running stream (or faulted): record the user's
                    // unmute INTENT immediately — mirror and state file
                    // reflect it at once and it survives a daemon restart
                    // — while the HARDWARE stays muted until the next
                    // StreamStarting runs the PLL handshake. No separate
                    // pending flag: `state.muted == false` IS the pending
                    // unmute (audit round 4, Issue 1).
                    let new_state = self.state.unmute();
                    self.state = new_state;
                    self.persist(&new_state).await;
                    info!(
                        "Unmute recorded without running stream; audio path stays muted until StreamStarting"
                    );
                }
                // In AwaitingClock the handshake outcome governs (the state
                // is transient — see start_stream's error guard).
            }
            AmpCommandKind::StreamStopping => {
                // Transient mute: hardware only — the user's mute intent is
                // untouched so the next stream auto-resumes at full level.
                // Electrical mute first (cannot fail on the bus); the
                // serial write follows.
                self.gpio.set_hardware_mute(true)?;
                self.mute_hardware().await?;
                self.ctrl = CtrlState::Muted;
                self.stream_live = false;
                // Re-assert GPIO /MUTE for a pop-free standby.
                self.gpio.set_hardware_mute(true)?;
                debug!("StreamStopping applied (hardware muted, user intent preserved)");
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
        // Historical fault accumulation (0x6D) rides along for diagnostics:
        // it shows whether the live fault is a repeat event since boot. The
        // read is best-effort — a diagnostic must never delay or drop the
        // live-fault handling below.
        match self.device.read_latched_errors_async().await {
            Ok(latched) if latched != speaker_hardware::AmpErrorFlags::NONE => {
                debug!(?latched, "Latched error accumulation at edge");
            }
            Ok(_) => {}
            Err(e) => debug!(error = %e, "Latched error read failed at edge"),
        }
        self.handle_faults(flags).await
    }

    /// Event: safety tick (low-frequency telemetry check). Also restores
    /// the user level when a thermal throttle is active and the warning
    /// has cleared.
    ///
    /// # Errors
    /// Propagated hardware faults; callers log and keep running.
    pub async fn handle_safety_tick(&mut self) -> AmpResult<()> {
        debug!("Safety tick");
        // A `Faulted` controller must keep retrying recovery every tick:
        // staying faulted forever (because the recovery's first attempt
        // failed mid-way) is worse than repeated retry attempts, and the
        // retry path itself re-mutes before anything else.
        if self.ctrl == CtrlState::Faulted {
            self.fault_recovery().await?;
            self.ctrl = CtrlState::Muted;
            return Ok(());
        }
        let flags = self.device.read_live_errors_async().await?;
        if flags == speaker_hardware::AmpErrorFlags::NONE {
            if self.thermal_offset_raw != 0 {
                self.thermal_offset_raw = 0;
                let restored = self.effective_raw();
                info!(
                    level = restored,
                    "Thermal warning cleared; restoring user level"
                );
                self.write_hw_level(restored).await?;
            }
            if self.ctrl == CtrlState::Playing {
                return Ok(());
            }
        }
        self.handle_faults(flags).await
    }

    /// Classify and act on live error flags (plan §3.1 GPIO edge row).
    /// Critical faults are evaluated FIRST so a thermal warning can never
    /// mask an OCP/DC/flying-cap condition; `PllClockLoss` during playback
    /// is treated as a stream stop (mute, not recovery).
    ///
    /// # Errors
    /// Propagated hardware faults.
    pub async fn handle_faults(&mut self, flags: speaker_hardware::AmpErrorFlags) -> AmpResult<()> {
        if flags == speaker_hardware::AmpErrorFlags::NONE {
            return Ok(());
        }
        // 1. Critical faults → full recovery (priority over thermal warn).
        if has_critical_fault(flags) {
            warn!(?flags, "Critical amp fault; entering fault recovery");
            self.ctrl = CtrlState::Faulted;
            self.stream_live = false;
            self.fault_recovery().await?;
            self.ctrl = CtrlState::Muted;
            return Ok(());
        }
        // 2. PLL clock loss while a stream is live (playing OR user-muted
        //    mid-stream) = the stream died: mute across the gap; the next
        //    StreamStarting runs the handshake again. `stream_live` must be
        //    cleared in BOTH cases — a stale `stream_live` would let a later
        //    `SetMute(false)` unmute into the dead clock without a
        //    handshake. GPIO /MUTE is asserted alongside the serial bit:
        //    with the PLL unlocked the synchronous digital domain cannot be
        //    trusted to propagate the register mute cleanly, and the
        //    electrical mute is the only path that cannot fail.
        if flags.has_fault(AmpFault::PllClockLoss)
            && (self.ctrl == CtrlState::Playing
                || (self.ctrl == CtrlState::Muted && self.stream_live))
        {
            warn!("PLL clock lost during playback; muting until the stream restarts");
            // Electrical mute first: the only path that cannot fail, and
            // the digital domain cannot be trusted while the PLL is open.
            self.gpio.set_hardware_mute(true)?;
            self.mute_hardware().await?;
            self.ctrl = CtrlState::Muted;
            self.stream_live = false;
            return Ok(());
        }
        // 3. Thermal warn: throttle −6 dB ONCE per warning episode (the
        //    offset is write-time only and never persisted; the safety tick
        //    restores the user level when the warning clears).
        if flags.has_fault(AmpFault::HighTempWarning) {
            if self.thermal_offset_raw == 0 {
                self.thermal_offset_raw = 6;
                let effective = self.effective_raw();
                info!(
                    user_level = self.state.level_raw,
                    effective, "Thermal warning; throttling -6 dB (not persisted)"
                );
                self.write_hw_level(effective).await?;
            }
            self.device.clear_latched_errors_async().await?;
        }
        Ok(())
    }

    /// Fault recovery: mute → `/ENABLE` reset pulse (HIGH→LOW wipes all
    /// registers) → serial mute re-asserted (the wipe clears the mute bit)
    /// → full register re-init → muted. Without the HIGH pulse the
    /// registers are never wiped and "re-init" is a no-op.
    ///
    /// The mute is **transient**: the persisted user intent (`state.muted`)
    /// is untouched, so the next `StreamStarting` auto-resumes playback.
    ///
    /// # Errors
    /// Returns [`AmpError`] if any recovery step fails.
    pub async fn fault_recovery(&mut self) -> AmpResult<()> {
        // From here until re-init completes the registers are UNPROVEN: the
        // upcoming /ENABLE pulse wipes them to defaults (VLP bypassed,
        // limiter off = full-scale output).
        self.regs_initialized = false;
        // ELECTRICAL mute FIRST: it does not depend on the I2C bus, and bus
        // faults are exactly the condition that triggered recovery. Only
        // then attempt the serial mute (best-effort before a reset — the
        // wipe clears the bit anyway; a failure must not skip the reset).
        self.gpio.set_hardware_mute(true)?;
        self.hardware_muted = true;
        if let Err(e) = self.mute_hardware().await {
            warn!(error = %e, "Serial mute failed before reset; GPIO /MUTE asserted; continuing with reset");
        }
        // /ENABLE reset cycle: registers return to true reset defaults.
        // The reset itself drives /MUTE LOW (pop-safe during the pulse).
        let gpio = self.gpio.clone();
        tokio::task::spawn_blocking(move || gpio.reset())
            .await
            .context("GPIO reset task panicked")??;
        // The /ENABLE wipe cleared the serial mute bit: re-assert it BEFORE
        // re-init so the audio path never comes back unmuted at 0 dB.
        self.mute_hardware().await?;
        self.reinit_registers().await?;
        // Registers proven again; restore the user's (thermally effective)
        // volume; keep muted until the next StreamStarting.
        self.regs_initialized = true;
        self.device
            .write_master_volume_async(self.effective_raw())
            .await?;
        self.device.clear_latched_errors_async().await?;
        warn!("Fault recovery complete; amp re-initialized, muted awaiting stream");
        Ok(())
    }

    /// Register init shared by boot and recovery (standard I2S, VLP,
    /// limiter, −15 dBFS thresholds).
    ///
    /// # Errors
    /// Returns [`AmpError`] on bus faults.
    async fn reinit_registers(&mut self) -> AmpResult<()> {
        self.device.write_i2s_format_async(0).await?;
        self.device.enable_audio_processor_async().await?;
        self.device.enable_limiter_async().await?;
        // Kernel-parity limiter timing: fast attack, slow release
        // (ma120x0p probe(): attack bits 7:6 = 0x80, release bits 5:4 = 0x00).
        self.device
            .configure_limiter_slew_async(LimiterSlew::Fast, LimiterSlew::Slow)
            .await?;
        self.device.write_limiter_thresholds_async(-15).await?;
        Ok(())
    }

    /// Stream starting: bounded one-shot PLL handshake, then unmute **only
    /// if the user wants audio** (a pending unmute request). A user mute is
    /// never overridden by a stream event.
    ///
    /// # Errors
    /// Returns [`AmpError`] on bus faults.
    pub async fn start_stream(&mut self) -> AmpResult<()> {
        if self.ctrl == CtrlState::Playing {
            // Already live (late duplicate event); no-op.
            return Ok(());
        }
        self.ctrl = CtrlState::AwaitingClock;
        let result = self.start_stream_locked().await;
        // Error guard (audit round 4, Issue 2): any `?`-return inside the
        // body must never strand the machine in `AwaitingClock` — in that
        // state `SetMute(false)` no-ops, the PLL-loss branch cannot match,
        // and the safety tick cannot reset it. Fail safe to muted; the
        // next StreamStarting retries the handshake.
        if result.is_err() {
            self.ctrl = CtrlState::Muted;
        }
        result
    }

    /// Handshake body of [`Self::start_stream`] (called with
    /// `ctrl == AwaitingClock` already set).
    ///
    /// # Errors
    /// Returns [`AmpError`] on bus faults.
    async fn start_stream_locked(&mut self) -> AmpResult<()> {
        // Registers wiped by a prior failed fault recovery are unproven:
        // re-run recovery BEFORE any handshake/unmute decision so the audio
        // path can never be released while VLP is bypassed (reset defaults
        // would put full-scale output through the speakers).
        if !self.regs_initialized {
            self.ctrl = CtrlState::Faulted;
            self.fault_recovery().await?;
            self.ctrl = CtrlState::Muted;
        }
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
        // Re-check live faults before releasing the audio path, in
        // priority order: never unmute into a fault condition.
        let flags = self.device.read_live_errors_async().await?;
        if flags.has_fault(AmpFault::PllClockLoss) {
            warn!(
                ?flags,
                "PLL clock loss still live at stream start; staying muted"
            );
            self.ctrl = CtrlState::Muted;
            return Ok(());
        }
        if has_critical_fault(flags) {
            warn!(
                ?flags,
                "Critical fault at stream start; running fault recovery instead of unmute"
            );
            self.fault_recovery().await?;
            self.ctrl = CtrlState::Muted;
            return Ok(());
        }
        if flags.has_fault(AmpFault::HighTempWarning) {
            // Warm amp at stream start: apply the -6 dB throttle instead of
            // a full reset (a reset would not cool the silicon anyway).
            if self.thermal_offset_raw == 0 {
                self.thermal_offset_raw = 6;
                let effective = self.effective_raw();
                info!(
                    user_level = self.state.level_raw,
                    effective, "Thermal warning at stream start; throttling -6 dB (not persisted)"
                );
                self.write_hw_level(effective).await?;
            }
            self.device.clear_latched_errors_async().await?;
        }
        // Decide from USER INTENT captured BEFORE any mutation: a user
        // mute is never overridden, but a normally unmuted state (or an
        // unmute recorded while idle) plays on stream start. (Mutating
        // first via `apply_muted(false)` would make `!state.muted` a
        // tautology — the original bug.)
        let want_audio = !self.state.muted;
        if want_audio {
            self.device.clear_latched_errors_async().await?;
            self.apply_muted(false).await?;
            self.gpio.set_hardware_mute(false)?;
            self.ctrl = CtrlState::Playing;
            self.stream_live = true;
            info!("Stream starting: PLL locked, amp unmuted");
        } else {
            // User muted: hardware ready, stays muted. The stream is still
            // live, so a later SetMute(false) restores it immediately.
            self.ctrl = CtrlState::Muted;
            self.stream_live = true;
            debug!("Stream starting: PLL locked but user mute active; staying muted");
        }
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

    /// Apply the serial mute bit (audio-path mute; level untouched) AND
    /// record the user's mute intent: `state.muted` is updated and
    /// persisted. Used for explicit `SetMute` commands and the stream-start
    /// unmute; transient mutes use [`Self::mute_hardware`] instead.
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
        self.hardware_muted = muted;
        self.persist(&new_state).await;
        Ok(())
    }

    /// Transient hardware-only mute: asserts the serial mute bit and marks
    /// the audio path muted WITHOUT changing or persisting the user's mute
    /// intent (`state.muted`). Used by stream stop, PLL loss, and fault
    /// recovery so the next stream auto-resumes at the user's level.
    ///
    /// # Errors
    /// Returns [`AmpError`] on hardware faults.
    async fn mute_hardware(&mut self) -> AmpResult<()> {
        self.backend.set_serial_mute(true).await?;
        self.hardware_muted = true;
        Ok(())
    }

    /// The hardware-effective level: user level minus the active thermal
    /// throttle offset (raw units are dB attenuation, so subtracting
    /// attenuates). 0-clamped via [`AmpState::new`] on write.
    fn effective_raw(&self) -> u16 {
        self.state.level_raw.saturating_sub(self.thermal_offset_raw)
    }

    /// Write the given (effective) level to hardware WITHOUT changing or
    /// persisting the user level in [`Self::state`]. Used by the thermal
    /// throttle; `state.level_raw` stays the user's intent.
    ///
    /// # Errors
    /// Returns [`AmpError`] on hardware faults.
    async fn write_hw_level(&mut self, raw: u16) -> AmpResult<()> {
        let hw_state = AmpState::new(raw, self.hardware_muted)?;
        self.backend.write_state(&hw_state).await?;
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
        mut error_edges: Option<mpsc::Receiver<()>>,
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
                            .send(
                                result
                                    .as_ref()
                                    .map(|_| ())
                                    .map_err(|e| e.summary()),
                            );
                    }
                    if let Err(e) = result {
                        warn!(error = %e, "Command handling failed; prior state preserved");
                    }
                }
                // `if error_edges.is_some()` disables the arm once the watch
                // task exits (recv() would otherwise return None immediately
                // forever and hot-spin the loop).
                edge = async { error_edges.as_mut().expect("checked").recv().await },
                    if error_edges.is_some() =>
                {
                    if edge.is_none() {
                        warn!("Error-edge channel closed; edge watch disabled (safety tick remains active)");
                        error_edges = None;
                    } else if let Err(e) = self.handle_error_edge().await {
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

impl std::fmt::Debug for AmpController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl: the backend is a `dyn` trait object and the device
        // handle is hardware — neither implements `Debug` (M-DEBUG-PUBLIC).
        f.debug_struct("AmpController")
            .field("state", &self.state)
            .field("ctrl", &self.ctrl)
            .field("hardware_muted", &self.hardware_muted)
            .field("stream_live", &self.stream_live)
            .field("thermal_offset_raw", &self.thermal_offset_raw)
            .field("regs_initialized", &self.regs_initialized)
            .finish_non_exhaustive()
    }
}
