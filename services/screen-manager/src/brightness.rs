//! Embedded ambient-brightness engine and overlay command plumbing.
//!
//! The brightness policy loop runs inside `screen-manager` on a dedicated
//! background worker. It owns the BH1750 ambient sensor and the backlight sysfs
//! handle, so there is exactly one writer and no D-Bus hop between the UI and
//! the hardware. The render thread only pushes commands into a condvar-backed
//! queue and applies authoritative snapshots delivered through the shared
//! update channel (`M-NO-DBUS-ON-RENDER-THREAD`).
//!
//! Every control decision lives in [`run_tick`], a pure function over the
//! injectable [`LuxSource`] and [`BacklightIo`] seams. Host tests therefore
//! exercise external-write adoption, manual takeover, idle bias reset, and
//! sensor/write faults without touching I²C or sysfs.

use std::backtrace::Backtrace;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use speaker_hardware::{
    AutoBrightness, Backlight, BacklightError, Config as BrightnessConfig, DEFAULT_CONFIG,
    LightError, LightSensor,
};
use tracing::{debug, info, warn};

/// Control-loop tick: 50 ms keeps ambient response smooth on the in-order Pi 3.
pub const BRIGHTNESS_TICK_INTERVAL: Duration = Duration::from_millis(50);
/// Window in which an optimistic slider value is not overwritten by an older
/// authoritative snapshot (`M-PANIC-ON-BUG` settle-window convergence).
pub const BRIGHTNESS_SETTLE_WINDOW: Duration = Duration::from_millis(1_500);
/// Idle period after a manual interaction before its gamma bias is cleared.
pub const BRIGHTNESS_IDLE_RESET: Duration = Duration::from_secs(600);

/// A single brightness mutation queued by the overlay or settings screen.
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

    /// Wakes the worker after an out-of-band change notification.
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
                    reason = "policy reports bounded percentages/lux/gamma"
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

/// Injectable ambient-lux source (`LightSensor` in production, fakes in tests).
pub trait LuxSource {
    /// Returns the current ambient illuminance in lux.
    ///
    /// # Errors
    /// Returns a typed error when the underlying sensor read fails.
    fn read_lux(&self) -> Result<f32, BrightnessError>;
}

impl LuxSource for LightSensor {
    fn read_lux(&self) -> Result<f32, BrightnessError> {
        Ok(LightSensor::read_lux(self)?.lux())
    }
}

/// Injectable backlight sysfs adapter (`Backlight` in production).
pub trait BacklightIo {
    /// Returns the hardware maximum raw brightness value.
    fn max_brightness(&self) -> u32;

    /// Reads the current hardware raw brightness.
    ///
    /// # Errors
    /// Returns a typed error when sysfs cannot be read.
    fn read(&self) -> Result<u32, BrightnessError>;

    /// Writes a raw brightness value.
    ///
    /// # Errors
    /// Returns a typed error when sysfs rejects the write.
    fn write(&self, raw: u32) -> Result<(), BrightnessError>;
}

impl BacklightIo for Backlight {
    fn max_brightness(&self) -> u32 {
        Backlight::max_brightness(self)
    }

    fn read(&self) -> Result<u32, BrightnessError> {
        Ok(Backlight::read(self)?)
    }

    fn write(&self, raw: u32) -> Result<(), BrightnessError> {
        Backlight::write(self, raw)?;
        Ok(())
    }
}

/// Mutable state persisted across brightness ticks.
#[derive(Debug, Clone)]
pub struct LoopState {
    /// Last raw value the engine wrote (external-write detector reference).
    pub last_written: u32,
    /// Last manual interaction; drives the idle-bias reset timer.
    pub last_manual: Option<Instant>,
    /// Whether automatic writes are currently enabled.
    pub auto_enabled: bool,
    /// Last successfully read lux value (telemetry readout).
    pub last_lux: f32,
}

impl LoopState {
    /// Builds initial loop state from the backlight's current raw value.
    #[must_use]
    pub const fn new(initial_raw: u32) -> Self {
        Self::with_auto(initial_raw, true)
    }

    /// Builds initial loop state with an explicit auto-mode preference.
    #[must_use]
    pub const fn with_auto(initial_raw: u32, auto_enabled: bool) -> Self {
        Self {
            last_written: initial_raw,
            last_manual: None,
            auto_enabled,
            last_lux: 50.0,
        }
    }
}

/// Per-tick inputs that are not owned by the state machine.
#[derive(Debug, Clone, Copy)]
pub struct TickInput<'a> {
    /// Monotonic timestamp for this tick.
    pub now: Instant,
    /// Seconds elapsed since the previous tick.
    pub dt: f32,
    /// Commands queued by the overlay since the previous tick.
    pub commands: &'a [BrightnessCommand],
    /// Idle period after which the manual bias is cleared.
    pub idle_reset: Duration,
}

/// The single action a tick took, reported for deterministic assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickAction {
    /// No write was needed.
    Idle,
    /// An external backlight write was adopted as a manual override.
    ExternalAdopt {
        /// Raw hardware value adopted.
        raw: u32,
    },
    /// An overlay manual-percent command was written immediately.
    ManualWrite {
        /// Raw hardware value written.
        raw: u32,
    },
    /// A normal auto-policy write was applied.
    AutoWrite {
        /// Raw hardware value written.
        raw: u32,
    },
    /// The ambient sensor failed; brightness was held.
    SensorFault,
    /// The backlight write failed; the previous value is retried next tick.
    WriteFault {
        /// Raw hardware value that could not be written.
        raw: u32,
    },
    /// Automatic writes are disabled; brightness was held.
    AutoDisabled,
    /// The idle timer cleared the manual gamma bias.
    IdleReset,
}

/// Result of one engine tick.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    /// Action taken, used by diagnostics and integration tests.
    pub action: TickAction,
    /// Snapshot to publish to the UI after this tick.
    pub state: BrightnessState,
}

/// Executes one control-loop tick over injectable hardware seams.
///
/// Commands are applied first (with an immediate backlight write for a manual
/// percent), then external writes are adopted, then the idle timer and
/// automatic write path run. A manual command or external adoption suppresses
/// the auto path for that tick so the UI never fights the engine.
#[expect(
    clippy::too_many_lines,
    reason = "single tick decision point keeps command, external-write, idle, and auto ordering explicit"
)]
#[must_use]
pub fn run_tick<S, B>(
    sensor: &S,
    backlight: &B,
    ab: &mut AutoBrightness,
    cfg: &BrightnessConfig,
    loop_state: &mut LoopState,
    input: TickInput<'_>,
) -> TickOutcome
where
    S: LuxSource + ?Sized,
    B: BacklightIo + ?Sized,
{
    let max = backlight.max_brightness();
    let mut action = TickAction::Idle;
    let mut suppressed = false;

    for command in input.commands {
        match *command {
            BrightnessCommand::SetPercent(percent)
                if percent.is_finite() && (0.0..=100.0).contains(&percent) =>
            {
                let perceptual =
                    (f64::from(percent) / 100.0).clamp(f64::from(cfg.b_min), 1.0) as f32;
                ab.on_manual_change(perceptual);
                let duty = ab.brightness().powf(cfg.pwm_gamma);
                let raw = raw_from_duty(duty, max);
                match backlight.write(raw) {
                    Ok(()) => loop_state.last_written = raw,
                    Err(e) => warn!(error = %e, raw, "manual backlight write failed"),
                }
                // A slider drag is an explicit manual takeover: the overlay
                // only sends SetPercent, so the engine leaves auto mode itself
                // (SetAuto(true) re-enables it).
                loop_state.auto_enabled = false;
                loop_state.last_manual = Some(input.now);
                info!(
                    percent,
                    raw,
                    auto_enabled = false,
                    "manual brightness applied; auto mode disabled"
                );
                action = TickAction::ManualWrite { raw };
                suppressed = true;
            }
            BrightnessCommand::SetPercent(percent) => {
                warn!(percent, "ignored non-finite or out-of-range manual percent");
            }
            BrightnessCommand::SetAuto(enabled) => {
                if loop_state.auto_enabled != enabled {
                    info!(auto_enabled = enabled, "automatic brightness mode changed");
                }
                loop_state.auto_enabled = enabled;
                loop_state.last_manual = Some(input.now);
            }
            BrightnessCommand::ResetBias => {
                ab.reset_manual_bias();
                loop_state.last_manual = Some(input.now);
                info!("manual brightness bias reset");
                action = TickAction::IdleReset;
            }
        }
    }

    if !suppressed {
        match backlight.read() {
            Ok(hw) if hw.abs_diff(loop_state.last_written) > 1 => {
                let ratio = (f64::from(hw) / f64::from(max.max(1))).clamp(0.0, 1.0) as f32;
                let perceptual = ratio.powf(1.0 / cfg.pwm_gamma).clamp(cfg.b_min, 1.0);
                ab.on_manual_change(perceptual);
                loop_state.last_written = hw;
                loop_state.last_manual = Some(input.now);
                info!(
                    raw = hw,
                    percent = round_percent(perceptual),
                    "external brightness change adopted as manual override"
                );
                action = TickAction::ExternalAdopt { raw: hw };
                suppressed = true;
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "backlight read failed; skipping external-write detection"),
        }
    }

    if !suppressed {
        if let Some(last) = loop_state.last_manual
            && input.now.duration_since(last) >= input.idle_reset
        {
            ab.reset_manual_bias();
            loop_state.last_manual = None;
            info!(
                idle_reset_secs = input.idle_reset.as_secs(),
                "idle timer cleared manual brightness bias"
            );
            action = TickAction::IdleReset;
        }

        if loop_state.auto_enabled {
            match sensor.read_lux() {
                Ok(lux) if lux.is_finite() => {
                    loop_state.last_lux = lux;
                    let duty = ab.update(lux, input.dt);
                    let raw = raw_from_duty(duty, max);
                    if raw != loop_state.last_written {
                        match backlight.write(raw) {
                            Ok(()) => {
                                loop_state.last_written = raw;
                                action = TickAction::AutoWrite { raw };
                                debug!(raw, lux, dt = input.dt, "auto brightness write");
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    target_raw = raw,
                                    "backlight write failed; retrying with backoff next tick"
                                );
                                action = TickAction::WriteFault { raw };
                            }
                        }
                    }
                }
                Ok(lux) => {
                    warn!(lux, "ignored non-finite lux sample; holding brightness");
                    action = TickAction::SensorFault;
                }
                Err(e) => {
                    warn!(error = %e, "lux read failed; holding last brightness");
                    action = TickAction::SensorFault;
                }
            }
        } else {
            action = TickAction::AutoDisabled;
        }
    }

    TickOutcome {
        action,
        state: snapshot(ab, loop_state),
    }
}

#[expect(
    clippy::cast_sign_loss,
    reason = "duty is clamped to the non-negative range 0..=max before the cast"
)]
fn raw_from_duty(duty: f32, max: u32) -> u32 {
    (duty * max as f32).round().clamp(0.0, max as f32) as u32
}

fn round_percent(perceptual: f32) -> f64 {
    f64::from((perceptual * 100.0).clamp(0.0, 100.0))
}

fn snapshot(ab: &AutoBrightness, state: &LoopState) -> BrightnessState {
    BrightnessState::new(
        state.auto_enabled,
        round_percent(ab.brightness()),
        f64::from(state.last_lux),
        f64::from(ab.manual_gamma()),
    )
}

/// Owns the hardware and the pure brightness policy for the worker thread.
#[derive(Debug)]
pub struct BrightnessEngine<S, B> {
    sensor: S,
    backlight: B,
    ab: AutoBrightness,
    cfg: BrightnessConfig,
    state: LoopState,
}

impl<S, B> BrightnessEngine<S, B>
where
    S: LuxSource,
    B: BacklightIo,
{
    /// Builds an engine, seeding the policy and external-write detector from
    /// authoritative hardware reads.
    ///
    /// # Errors
    /// Returns a typed error when the initial backlight or lux read fails; a
    /// nominal fallback here would misclassify the next write as an override.
    pub fn try_new(
        sensor: S,
        backlight: B,
        cfg: BrightnessConfig,
        auto_enabled: bool,
    ) -> Result<Self, BrightnessError> {
        let initial_raw = backlight.read()?;
        let initial_lux = sensor.read_lux()?;
        let ab = AutoBrightness::new(initial_lux, cfg);
        let state = LoopState::with_auto(initial_raw, auto_enabled);
        info!(
            initial_raw,
            initial_lux,
            auto_enabled,
            ema_alpha = cfg.ema_alpha,
            hyst_up = cfg.hyst_up,
            hyst_down = cfg.hyst_down,
            debounce_up = cfg.debounce_up,
            debounce_down = cfg.debounce_down,
            rate_up = cfg.rate_up,
            rate_down = cfg.rate_down,
            b_min = cfg.b_min,
            pwm_gamma = cfg.pwm_gamma,
            "Brightness engine initialized"
        );
        Ok(Self {
            sensor,
            backlight,
            ab,
            cfg,
            state,
        })
    }

    /// Returns the current authoritative snapshot without touching hardware.
    #[must_use]
    pub fn state(&self) -> BrightnessState {
        snapshot(&self.ab, &self.state)
    }

    /// Advances the policy by one tick and returns the action it took.
    #[must_use]
    pub fn tick(
        &mut self,
        now: Instant,
        dt: f32,
        commands: &[BrightnessCommand],
        idle_reset: Duration,
    ) -> TickOutcome {
        run_tick(
            &self.sensor,
            &self.backlight,
            &mut self.ab,
            &self.cfg,
            &mut self.state,
            TickInput {
                now,
                dt,
                commands,
                idle_reset,
            },
        )
    }
}

/// Internal classification of brightness failures.
#[derive(Debug)]
enum BrightnessErrorKind {
    /// Ambient sensor read failed.
    Sensor(Box<LightError>),
    /// Backlight sysfs read/write failed.
    Backlight(Box<BacklightError>),
    /// A deterministic failure supplied by a test fake.
    Failed(Box<str>),
}

/// Canonical error for the embedded brightness engine.
#[derive(Debug)]
pub struct BrightnessError {
    kind: BrightnessErrorKind,
    backtrace: Backtrace,
}

impl BrightnessError {
    fn sensor(source: LightError) -> Self {
        Self {
            kind: BrightnessErrorKind::Sensor(Box::new(source)),
            backtrace: Backtrace::capture(),
        }
    }

    fn backlight(source: BacklightError) -> Self {
        Self {
            kind: BrightnessErrorKind::Backlight(Box::new(source)),
            backtrace: Backtrace::capture(),
        }
    }

    /// Builds a deterministic fault for host test fakes.
    #[must_use]
    pub fn failed(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: BrightnessErrorKind::Failed(message.into()),
            backtrace: Backtrace::capture(),
        }
    }

    fn fmt_kind(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BrightnessErrorKind::Sensor(e) => write!(f, "ambient sensor error: {e}"),
            BrightnessErrorKind::Backlight(e) => write!(f, "backlight error: {e}"),
            BrightnessErrorKind::Failed(msg) => write!(f, "brightness error: {msg}"),
        }
    }
}

speaker_common::impl_canonical_error_display!(BrightnessError);

impl std::error::Error for BrightnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            BrightnessErrorKind::Sensor(e) => Some(e),
            BrightnessErrorKind::Backlight(e) => Some(e),
            BrightnessErrorKind::Failed(_) => None,
        }
    }
}

impl From<LightError> for BrightnessError {
    fn from(e: LightError) -> Self {
        Self::sensor(e)
    }
}

impl From<BacklightError> for BrightnessError {
    fn from(e: BacklightError) -> Self {
        Self::backlight(e)
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

/// Runs the embedded brightness control loop until `running` is cleared.
///
/// Commands wake the worker immediately through the condvar; otherwise each
/// tick waits for [`BRIGHTNESS_TICK_INTERVAL`]. Every tick publishes its
/// authoritative snapshot so external writes and sensor faults are visible to
/// the overlay without polling D-Bus.
pub fn run_worker<S, B>(
    engine: &mut BrightnessEngine<S, B>,
    shared: &Mutex<crate::daemon::BtSharedUpdate>,
    commands: &BrightnessCommandQueue,
    running: &Arc<AtomicBool>,
    idle_reset: Duration,
) where
    S: LuxSource,
    B: BacklightIo,
{
    publish(engine.state(), shared);
    let mut last_tick = Instant::now();
    while running.load(Ordering::Relaxed) {
        let (drained, _changed) = commands.drain_idle(BRIGHTNESS_TICK_INTERVAL);
        if !running.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let dt = now.duration_since(last_tick).as_secs_f32();
        last_tick = now;
        let outcome = engine.tick(now, dt, &drained, idle_reset);
        publish(outcome.state, shared);
    }
    info!("Embedded brightness worker stopped");
}

/// Returns the appliance brightness policy defaults (`DEFAULT_CONFIG`).
#[must_use]
pub const fn default_policy() -> BrightnessConfig {
    DEFAULT_CONFIG
}
