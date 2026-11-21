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

use speaker_hardware::{Backlight, BacklightError, LightError, LightSensor};
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

/// Ambient source used when the BH1750 cannot be opened at startup.
///
/// Keeping the brightness worker alive with a permanently faulting source
/// preserves manual slider control; every tick reports the sensor fault
/// through the normal auto path instead of silently disabling the overlay.
#[derive(Debug, Clone, Copy)]
pub struct UnavailableLuxSource;

impl LuxSource for UnavailableLuxSource {
    fn read_lux(&self) -> Result<f32, BrightnessError> {
        Err(BrightnessError::failed("ambient light sensor unavailable"))
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
    /// Manual percent/raw pair whose write failed and must be retried before
    /// any further automatic or external-write processing.
    pub pending_manual: Option<(f32, u32)>,
    /// Whether the current sensor-fault episode has already been logged at
    /// `warn!` level; subsequent ticks are downgraded to `debug!`.
    pub sensor_fault_logged: bool,
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
            pending_manual: None,
            sensor_fault_logged: false,
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
    let mut manual_attempted = false;

    for command in input.commands {
        match *command {
            BrightnessCommand::SetPercent(percent)
                if percent.is_finite() && (0.0..=100.0).contains(&percent) =>
            {
                manual_attempted = true;
                let perceptual =
                    (f64::from(percent) / 100.0).clamp(f64::from(cfg.b_min), 1.0) as f32;
                let raw = raw_from_duty(perceptual.powf(cfg.pwm_gamma), max);
                // A slider drag is an explicit manual takeover: the overlay
                // only sends SetPercent, so the engine leaves auto mode itself
                // (SetAuto(true) re-enables it). This records user intent even
                // when the sysfs write fails; the requested level is retried
                // next tick instead of being reported as applied.
                loop_state.auto_enabled = false;
                loop_state.last_manual = Some(input.now);
                match backlight.write(raw) {
                    Ok(()) => {
                        ab.on_manual_change(perceptual);
                        loop_state.last_written = raw;
                        loop_state.pending_manual = None;
                        info!(
                            percent,
                            raw,
                            auto_enabled = false,
                            "manual brightness applied; auto mode disabled"
                        );
                        action = TickAction::ManualWrite { raw };
                    }
                    Err(e) => {
                        loop_state.pending_manual = Some((perceptual, raw));
                        warn!(
                            error = %e,
                            percent,
                            raw,
                            "manual backlight write failed; auto mode disabled, retrying next tick"
                        );
                        action = TickAction::WriteFault { raw };
                    }
                }
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
                if enabled {
                    // Re-enabling auto cancels any manual write that failed
                    // earlier; the ambient policy owns the backlight again.
                    loop_state.pending_manual = None;
                }
            }
            BrightnessCommand::ResetBias => {
                ab.reset_manual_bias();
                loop_state.last_manual = Some(input.now);
                info!("manual brightness bias reset");
                action = TickAction::IdleReset;
            }
        }
    }

    // A failed manual write is retried on later ticks before external-write
    // detection or the auto path, so a transient sysfs fault cannot leave the
    // UI showing a level that was never applied.
    if !manual_attempted && let Some((percent, raw)) = loop_state.pending_manual {
        match backlight.write(raw) {
            Ok(()) => {
                ab.on_manual_change(percent);
                loop_state.last_written = raw;
                loop_state.pending_manual = None;
                info!(percent, raw, "retried manual brightness applied");
                action = TickAction::ManualWrite { raw };
            }
            Err(e) => {
                warn!(error = %e, percent, raw, "retried manual backlight write failed");
                action = TickAction::WriteFault { raw };
            }
        }
        suppressed = true;
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
                    loop_state.sensor_fault_logged = false;
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
                    if loop_state.sensor_fault_logged {
                        debug!(lux, "non-finite lux sample; holding brightness");
                    } else {
                        warn!(lux, "ignored non-finite lux sample; holding brightness");
                        loop_state.sensor_fault_logged = true;
                    }
                    action = TickAction::SensorFault;
                }
                Err(e) => {
                    if loop_state.sensor_fault_logged {
                        debug!(error = %e, "lux read still failing; holding last brightness");
                    } else {
                        warn!(error = %e, "lux read failed; holding last brightness");
                        loop_state.sensor_fault_logged = true;
                    }
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
        let initial_lux = sensor.read_lux()?;
        Self::new_seeded(sensor, backlight, cfg, auto_enabled, initial_lux)
    }

    /// Builds an engine with an explicitly supplied initial lux seed.
    ///
    /// Used when the ambient sensor could not be opened: manual brightness
    /// control must still work, and each tick reports the sensor fault through
    /// the normal auto path. The backlight read remains fail-loud so the
    /// external-write detector starts from the authoritative hardware value.
    ///
    /// # Errors
    /// Returns a typed error when the initial backlight read fails.
    pub fn new_seeded(
        sensor: S,
        backlight: B,
        cfg: BrightnessConfig,
        auto_enabled: bool,
        initial_lux: f32,
    ) -> Result<Self, BrightnessError> {
        let initial_raw = backlight.read()?;
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

/// Returns the appliance brightness policy defaults (`DEFAULT_BRIGHTNESS_CONFIG`).
#[must_use]
pub const fn default_policy() -> BrightnessConfig {
    DEFAULT_BRIGHTNESS_CONFIG
}

// ---------------------------------------------------------------------------
// Pure ambient brightness policy.
//
// This is appliance policy, not a peripheral driver, so it lives in
// `screen-manager` rather than in `speaker-hardware`. It stays I/O-free and is
// exercised by `tests/brightness_policy_integration.rs` on any host.
// ---------------------------------------------------------------------------

/// Default brightness policy configuration matching the appliance display panel.
pub const DEFAULT_BRIGHTNESS_CONFIG: BrightnessConfig = BrightnessConfig {
    ema_alpha: 0.2,
    hyst_up: 0.25,
    hyst_down: 0.30,
    debounce_up: 1.5,
    debounce_down: 5.0,
    rate_up: 0.40,
    rate_down: 0.15,
    b_min: 0.05,
    gamma_min: 0.30,
    gamma_max: 3.00,
    pwm_gamma: 2.20,
};

/// Exponential moving average weight applied to each fresh lux sample.
const BASE_CURVE_SLOPE: f32 = 0.103;

/// Maximum integration step accepted by [`AutoBrightness::update`] in seconds.
const MAX_DT_SECS: f32 = 0.5;

/// Largest illuminance reference that cannot trigger hysteresis chatter.
const MIN_HYSTERESIS_ANCHOR_LUX: f32 = 2.0;

/// Tolerance for treating a base curve value as degenerate in gamma calculations.
const DEGENERATE_EPSILON: f32 = 1e-6;

/// Tuning parameters for the ambient brightness policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrightnessConfig {
    /// Exponential moving average weight for incoming lux samples (`0.0..=1.0`).
    pub ema_alpha: f32,
    /// Fractional rise above the anchor required to start an upward transition.
    pub hyst_up: f32,
    /// Fractional fall below the anchor required to start a downward transition.
    pub hyst_down: f32,
    /// Seconds an upward deviation must persist before its target is committed.
    pub debounce_up: f32,
    /// Seconds a downward deviation must persist before its target is committed.
    pub debounce_down: f32,
    /// Maximum perceptual brightness increase per second.
    pub rate_up: f32,
    /// Maximum perceptual brightness decrease per second.
    pub rate_down: f32,
    /// Minimum perceptual brightness floor (`0.0..1.0`).
    pub b_min: f32,
    /// Minimum manual gamma exponent.
    pub gamma_min: f32,
    /// Maximum manual gamma exponent.
    pub gamma_max: f32,
    /// Exponent mapping perceptual brightness to the hardware duty cycle.
    pub pwm_gamma: f32,
}

impl BrightnessConfig {
    /// Validates that every tuning parameter is finite and within its valid range.
    ///
    /// # Errors
    ///
    /// Returns [`BrightnessConfigError`] identifying the first field that is non-finite or
    /// outside its documented interval.
    pub const fn validate(self) -> Result<(), BrightnessConfigError> {
        if !self.ema_alpha.is_finite()
            || !self.hyst_up.is_finite()
            || !self.hyst_down.is_finite()
            || !self.debounce_up.is_finite()
            || !self.debounce_down.is_finite()
            || !self.rate_up.is_finite()
            || !self.rate_down.is_finite()
            || !self.b_min.is_finite()
            || !self.gamma_min.is_finite()
            || !self.gamma_max.is_finite()
            || !self.pwm_gamma.is_finite()
        {
            return Err(BrightnessConfigError::not_finite(
                "configuration contains a non-finite field",
            ));
        }
        if self.ema_alpha <= 0.0 || self.ema_alpha > 1.0 {
            return Err(BrightnessConfigError::out_of_range(
                "ema_alpha must be in (0, 1]",
            ));
        }
        if self.hyst_up < 0.0 || self.hyst_down < 0.0 {
            return Err(BrightnessConfigError::out_of_range(
                "hyst_up and hyst_down must be non-negative",
            ));
        }
        if self.debounce_up <= 0.0 || self.debounce_down <= 0.0 {
            return Err(BrightnessConfigError::out_of_range(
                "debounce times must be positive",
            ));
        }
        if self.rate_up <= 0.0 || self.rate_down <= 0.0 {
            return Err(BrightnessConfigError::out_of_range(
                "slew rates must be positive",
            ));
        }
        if self.b_min <= 0.0 || self.b_min >= 1.0 {
            return Err(BrightnessConfigError::out_of_range(
                "b_min must be in (0, 1)",
            ));
        }
        if self.gamma_min <= 0.0 || self.gamma_max < self.gamma_min {
            return Err(BrightnessConfigError::out_of_range(
                "gamma_min must be positive and gamma_max >= gamma_min",
            ));
        }
        if self.pwm_gamma <= 0.0 {
            return Err(BrightnessConfigError::out_of_range(
                "pwm_gamma must be positive",
            ));
        }
        Ok(())
    }
}

impl Default for BrightnessConfig {
    fn default() -> Self {
        DEFAULT_BRIGHTNESS_CONFIG
    }
}

/// Internal classification of brightness configuration failures.
#[derive(Debug)]
pub(crate) enum BrightnessConfigErrorKind {
    /// A tuning parameter was not a finite floating-point value.
    NotFinite { detail: &'static str },
    /// A tuning parameter was finite but outside its accepted interval.
    OutOfRange { detail: &'static str },
}

/// Canonical error returned when a [`BrightnessConfig`] fails validation.
#[derive(Debug)]
pub struct BrightnessConfigError {
    kind: BrightnessConfigErrorKind,
    backtrace: Backtrace,
}

impl BrightnessConfigError {
    const fn not_finite(detail: &'static str) -> Self {
        Self {
            kind: BrightnessConfigErrorKind::NotFinite { detail },
            backtrace: Backtrace::disabled(),
        }
    }

    const fn out_of_range(detail: &'static str) -> Self {
        Self {
            kind: BrightnessConfigErrorKind::OutOfRange { detail },
            backtrace: Backtrace::disabled(),
        }
    }

    /// Returns `true` if a tuning parameter was non-finite.
    #[must_use]
    pub const fn is_not_finite(&self) -> bool {
        matches!(self.kind, BrightnessConfigErrorKind::NotFinite { .. })
    }

    /// Returns `true` if a tuning parameter was outside its accepted range.
    #[must_use]
    pub const fn is_out_of_range(&self) -> bool {
        matches!(self.kind, BrightnessConfigErrorKind::OutOfRange { .. })
    }

    fn fmt_kind(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BrightnessConfigErrorKind::NotFinite { detail }
            | BrightnessConfigErrorKind::OutOfRange { detail } => f.write_str(detail),
        }
    }
}

speaker_common::impl_canonical_error_display!(BrightnessConfigError);

impl std::error::Error for BrightnessConfigError {}

/// Direction of a deviation currently waiting for its debounce interval to expire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingDir {
    /// Illuminance is inside the hysteresis deadband.
    None,
    /// Illuminance is above the upper hysteresis threshold.
    Up,
    /// Illuminance is below the lower hysteresis threshold.
    Down,
}

/// Pure state machine converting ambient lux into a smoothed brightness duty cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoBrightness {
    cfg: BrightnessConfig,
    lux_ema: f32,
    lux_anchor: f32,
    b_current: f32,
    b_target: f32,
    manual_gamma: f32,
    pending_dir: PendingDir,
    pending_elapsed: f32,
}

impl AutoBrightness {
    /// Creates a policy instance anchored at the supplied initial illuminance.
    #[must_use]
    pub fn new(initial_lux: f32, cfg: BrightnessConfig) -> Self {
        let lux = sanitize_lux(initial_lux, 0.0);
        let b = target_for(cfg, 1.0, lux);
        Self {
            cfg,
            lux_ema: lux,
            lux_anchor: lux,
            b_current: b,
            b_target: b,
            manual_gamma: 1.0,
            pending_dir: PendingDir::None,
            pending_elapsed: 0.0,
        }
    }

    /// Computes the base logarithmic brightness curve clamped to the configured floor.
    #[must_use]
    pub fn base_curve(&self, lux: f32) -> f32 {
        base_curve_for(self.cfg, lux)
    }

    /// Computes the perceptual target brightness for a lux sample under the manual bias.
    #[must_use]
    pub fn target_from_lux(&self, lux: f32) -> f32 {
        target_for(self.cfg, self.manual_gamma, lux)
    }

    /// Applies a user brightness value as a gamma bias over the smoothed lux base.
    ///
    /// The perceptual level snaps to `user_b`; a non-finite value is ignored.
    pub fn on_manual_change(&mut self, user_b: f32) {
        if !user_b.is_finite() {
            return;
        }
        let clamped = user_b.clamp(self.cfg.b_min, 1.0);
        let base = self.base_curve(self.lux_ema);
        if base > DEGENERATE_EPSILON && (base - 1.0).abs() > DEGENERATE_EPSILON && base.is_finite()
        {
            let gamma = clamped.ln() / base.ln();
            self.manual_gamma = if gamma.is_finite() {
                gamma.clamp(self.cfg.gamma_min, self.cfg.gamma_max)
            } else {
                1.0
            };
        } else {
            self.manual_gamma = 1.0;
        }
        self.b_current = clamped;
        self.b_target = clamped;
        self.pending_dir = PendingDir::None;
        self.pending_elapsed = 0.0;
    }

    /// Clears the manual gamma bias while preserving the current brightness level.
    pub fn reset_manual_bias(&mut self) {
        self.manual_gamma = 1.0;
        self.b_target = self.target_from_lux(self.lux_ema);
    }

    /// Advances the policy by one tick and returns the hardware duty cycle.
    ///
    /// Non-finite or negative lux samples leave the exponential average unchanged,
    /// and integration steps are clamped to at most half a second.
    #[must_use]
    pub fn update(&mut self, raw_lux: f32, dt_secs: f32) -> f32 {
        let dt = if dt_secs.is_finite() {
            dt_secs.clamp(0.0, MAX_DT_SECS)
        } else {
            0.0
        };
        let lux = sanitize_lux(raw_lux, self.lux_ema);
        self.lux_ema += self.cfg.ema_alpha * (lux - self.lux_ema);
        self.lux_ema = sanitize_lux(self.lux_ema, 0.0);

        let reference = self.lux_anchor.max(MIN_HYSTERESIS_ANCHOR_LUX);
        let upper = reference * (1.0 + self.cfg.hyst_up);
        let lower = (reference * (1.0 - self.cfg.hyst_down)).max(0.0);
        let direction = if self.lux_ema > upper {
            PendingDir::Up
        } else if self.lux_ema < lower {
            PendingDir::Down
        } else {
            PendingDir::None
        };

        if direction != self.pending_dir {
            self.pending_dir = direction;
            self.pending_elapsed = 0.0;
        }
        if self.pending_dir != PendingDir::None {
            self.pending_elapsed += dt;
            let required = if self.pending_dir == PendingDir::Up {
                self.cfg.debounce_up
            } else {
                self.cfg.debounce_down
            };
            if self.pending_elapsed >= required {
                self.lux_anchor = self.lux_ema;
                self.b_target = self.target_from_lux(self.lux_ema);
                self.pending_dir = PendingDir::None;
                self.pending_elapsed = 0.0;
            }
        }

        let diff = self.b_target - self.b_current;
        let step = if diff > 0.0 {
            self.cfg.rate_up * dt
        } else {
            self.cfg.rate_down * dt
        };
        self.b_current = (self.b_current + diff.clamp(-step, step)).clamp(self.cfg.b_min, 1.0);
        self.b_current.powf(self.cfg.pwm_gamma).clamp(0.0, 1.0)
    }

    /// Returns the current smoothed perceptual brightness level.
    #[must_use]
    pub const fn brightness(&self) -> f32 {
        self.b_current
    }

    /// Returns the perceptual brightness target toward which the policy slews.
    #[must_use]
    pub const fn target(&self) -> f32 {
        self.b_target
    }

    /// Returns the active manual gamma exponent (`1.0` when unbiased).
    #[must_use]
    pub const fn manual_gamma(&self) -> f32 {
        self.manual_gamma
    }

    /// Returns the exponentially smoothed ambient illuminance in lux.
    #[must_use]
    pub const fn lux_ema(&self) -> f32 {
        self.lux_ema
    }

    /// Returns the lux anchor used as the hysteresis reference.
    #[must_use]
    pub const fn lux_anchor(&self) -> f32 {
        self.lux_anchor
    }

    /// Returns the configuration used by this policy instance.
    #[must_use]
    pub const fn config(&self) -> BrightnessConfig {
        self.cfg
    }
}

fn sanitize_lux(raw: f32, fallback: f32) -> f32 {
    if raw.is_finite() && raw >= 0.0 {
        raw
    } else {
        fallback
    }
}

fn base_curve_for(cfg: BrightnessConfig, lux: f32) -> f32 {
    // `f32::max` maps NaN and negative infinity to zero, while positive
    // infinity saturates to the configured ceiling through the final clamp.
    let safe_lux = lux.max(0.0);
    (BASE_CURVE_SLOPE * (safe_lux + 1.0).ln() + cfg.b_min).clamp(cfg.b_min, 1.0)
}

fn target_for(cfg: BrightnessConfig, gamma: f32, lux: f32) -> f32 {
    base_curve_for(cfg, lux).powf(gamma).clamp(cfg.b_min, 1.0)
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AutoBrightness>();
    assert_send_sync::<BrightnessConfig>();
    assert_send_sync::<BrightnessConfigError>();
};
