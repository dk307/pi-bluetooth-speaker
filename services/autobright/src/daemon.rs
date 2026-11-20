//! Auto-brightness daemon loop and its host-testable tick seam.
//!
//! The production loop runs on a dedicated `std::thread` at a configurable
//! tick interval. Every decision is made by [`run_tick`], a pure function over
//! injectable [`LuxSource`] / [`BacklightIo`] traits so all policy branches —
//! external-write adoption, manual commands, idle reset, and fault holding —
//! are exercised on any development host without I²C or sysfs hardware.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use speaker_hardware::{AutoBrightness, Backlight, Config as BrightnessConfig, LightSensor};
use tracing::{debug, error, info, warn};
use zbus::Connection;

use crate::config::{AutobrightOptions, ResolvedConfig};
use crate::dbus::{self, PublishedState};
use crate::error::AutobrightError;

/// Injectable ambient-lux source (`LightSensor` in production, fakes in tests).
pub trait LuxSource {
    /// Returns the current ambient illuminance in lux.
    ///
    /// # Errors
    /// Returns an [`AutobrightError`] when the underlying sensor read fails.
    fn read_lux(&self) -> Result<f32, AutobrightError>;
}

impl LuxSource for LightSensor {
    fn read_lux(&self) -> Result<f32, AutobrightError> {
        Ok(speaker_hardware::LightSensor::read_lux(self)?.lux())
    }
}

/// Injectable backlight sysfs adapter (`Backlight` in production).
pub trait BacklightIo {
    /// Returns the hardware maximum raw brightness value.
    fn max_brightness(&self) -> u32;

    /// Reads the current hardware raw brightness.
    ///
    /// # Errors
    /// Returns an [`AutobrightError`] when sysfs cannot be read.
    fn read(&self) -> Result<u32, AutobrightError>;

    /// Writes a raw brightness value.
    ///
    /// # Errors
    /// Returns an [`AutobrightError`] when sysfs rejects the write.
    fn write(&self, raw: u32) -> Result<(), AutobrightError>;
}

impl BacklightIo for Backlight {
    fn max_brightness(&self) -> u32 {
        speaker_hardware::Backlight::max_brightness(self)
    }

    fn read(&self) -> Result<u32, AutobrightError> {
        Ok(speaker_hardware::Backlight::read(self)?)
    }

    fn write(&self, raw: u32) -> Result<(), AutobrightError> {
        speaker_hardware::Backlight::write(self, raw)?;
        Ok(())
    }
}

/// A control command enqueued by the D-Bus interface for the tick loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// Apply a manual brightness percentage in `0.0..=100.0`.
    SetManualPercent(f64),
    /// Enable or suppress automatic brightness writes.
    SetAutoEnabled(bool),
    /// Clear the manual gamma bias without snapping brightness.
    ResetManualBias,
}

/// Mutable state persisted across daemon ticks.
#[derive(Debug, Clone)]
pub struct LoopState {
    /// Last raw value this daemon wrote (external-write detector reference).
    pub last_written: u32,
    /// Last manual interaction; drives the idle-bias reset timer.
    pub last_manual: Option<Instant>,
    /// Whether automatic writes are currently enabled.
    pub auto_enabled: bool,
    /// Last successfully read lux value (EMA-free telemetry readout).
    pub last_lux: f32,
}

impl LoopState {
    /// Builds initial loop state from the backlight's current raw value.
    #[must_use]
    pub const fn new(initial_raw: u32) -> Self {
        Self {
            last_written: initial_raw,
            last_manual: None,
            auto_enabled: true,
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
    /// Commands drained from the D-Bus queue since the previous tick.
    pub commands: &'a [Command],
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
    /// A UI manual-percent command was written immediately.
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

/// Result of one daemon tick.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    /// Action taken, used by diagnostics and integration tests.
    pub action: TickAction,
    /// The state published to D-Bus after this tick.
    pub state: PublishedState,
}

/// Executes one control-loop tick over injectable hardware seams.
///
/// Commands are applied first (with an immediate backlight write for a manual
/// percent), then external writes are adopted, then the idle timer and
/// automatic write path run. A manual command or external adoption suppresses
/// the auto path for that tick so the UI never fights the daemon.
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

    for cmd in input.commands {
        match *cmd {
            Command::SetManualPercent(pct) if pct.is_finite() && (0.0..=100.0).contains(&pct) => {
                let perceptual = (pct / 100.0).clamp(f64::from(cfg.b_min), 1.0) as f32;
                ab.on_manual_change(perceptual);
                let duty = ab.brightness().powf(cfg.pwm_gamma);
                let raw = raw_from_duty(duty, max);
                match backlight.write(raw) {
                    Ok(()) => loop_state.last_written = raw,
                    Err(e) => warn!(error = %e, raw, "manual backlight write failed"),
                }
                // A slider drag is an explicit manual takeover: the overlay
                // only sends SetManualPercent, so the daemon must leave auto
                // mode itself (SetAutoEnabled(true) re-enables it).
                loop_state.auto_enabled = false;
                loop_state.last_manual = Some(input.now);
                info!(
                    percent = pct,
                    raw,
                    auto_enabled = false,
                    "manual brightness applied; auto mode disabled"
                );
                action = TickAction::ManualWrite { raw };
                suppressed = true;
            }
            Command::SetManualPercent(pct) => {
                warn!(
                    percent = pct,
                    "ignored non-finite or out-of-range manual percent"
                );
            }
            Command::SetAutoEnabled(enabled) => {
                if loop_state.auto_enabled != enabled {
                    info!(auto_enabled = enabled, "automatic brightness mode changed");
                }
                loop_state.auto_enabled = enabled;
                loop_state.last_manual = Some(input.now);
            }
            Command::ResetManualBias => {
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
        state: PublishedState {
            auto_enabled: loop_state.auto_enabled,
            brightness_percent: round_percent(ab.brightness()),
            ambient_lux: f64::from(loop_state.last_lux),
            manual_gamma: f64::from(ab.manual_gamma()),
            available: true,
        },
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

/// Runs the autobright service with CLI options already parsed.
///
/// # Errors
/// Returns [`ExitCode::FAILURE`] when configuration, hardware, or D-Bus
/// bootstrap fails; transient tick faults are logged and retried in-loop.
#[must_use]
pub async fn main(opts: AutobrightOptions) -> ExitCode {
    let cfg = match ResolvedConfig::resolve(opts) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!(error = %e, "failed to resolve autobright configuration");
            return ExitCode::FAILURE;
        }
    };

    let sensor = match LightSensor::new() {
        Ok(sensor) => sensor,
        Err(e) => {
            error!(error = %e, "failed to open BH1750 ambient light sensor");
            return ExitCode::FAILURE;
        }
    };
    let backlight = match Backlight::with_device(&cfg.backlight_device) {
        Ok(backlight) => backlight,
        Err(e) => {
            error!(error = %e, device = %cfg.backlight_device, "failed to open backlight sysfs device");
            return ExitCode::FAILURE;
        }
    };
    let connection = match Connection::system().await {
        Ok(connection) => connection,
        Err(e) => {
            error!(error = %e, "failed to connect to the system D-Bus");
            return ExitCode::FAILURE;
        }
    };
    run_with(cfg, Box::new(sensor), Box::new(backlight), &connection).await
}

/// Runs the daemon against injectable hardware and a supplied D-Bus connection.
///
/// Host integration tests pass fakes plus a private bus; [`main`] supplies the
/// real BH1750, sysfs backlight, and system bus. Keeping this seam in
/// production code makes the full bootstrap and tick loop coverage-testable.
///
/// # Errors
/// Returns [`ExitCode::FAILURE`] when either authoritative bootstrap read,
/// D-Bus export, or the tick-thread spawn fails; transient faults during the
/// running loop are logged and retried in-loop.
#[must_use]
pub async fn run_with(
    cfg: ResolvedConfig,
    sensor: Box<dyn LuxSource + Send>,
    backlight: Box<dyn BacklightIo + Send>,
    connection: &Connection,
) -> ExitCode {
    // These two reads seed the external-write detector and the brightness
    // policy. A nominal fallback here would silently misclassify the next
    // hardware write as an external override, so bootstrap must fail loudly.
    let initial_raw = match backlight.read() {
        Ok(raw) => raw,
        Err(e) => {
            error!(error = %e, device = %cfg.backlight_device, "initial backlight read failed; refusing to seed external-write detector with a fallback");
            return ExitCode::FAILURE;
        }
    };
    let initial_lux = match sensor.read_lux() {
        Ok(lux) => lux,
        Err(e) => {
            error!(error = %e, "initial ambient lux read failed; refusing to seed brightness policy with a fallback");
            return ExitCode::FAILURE;
        }
    };
    let ab = AutoBrightness::new(initial_lux, cfg.policy);
    let mut loop_state = LoopState::new(initial_raw);
    let initial_state = initial_published_state(&ab, &loop_state);

    let shared = Arc::new(std::sync::Mutex::new(initial_state));
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel::<PublishedState>();

    if let Err(e) = dbus::serve(connection, Arc::clone(&shared), cmd_tx).await {
        error!(error = %e, "failed to export org.speaker.Autobright1");
        return ExitCode::FAILURE;
    }

    let publisher_conn = connection.clone();
    tokio::spawn(async move {
        dbus::publish_loop(publisher_conn, state_rx).await;
    });
    let _ = state_tx.send(initial_state);

    if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
        warn!(error = %e, "sd_notify READY=1 failed; continuing");
    }
    info!(
        name = dbus::DBUS_NAME,
        path = dbus::DBUS_PATH,
        "Autobright ready, entering control loop"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_signal = Arc::clone(&shutdown);
    if let Err(e) = ctrlc::set_handler(move || {
        shutdown_signal.store(true, Ordering::SeqCst);
    }) {
        warn!(error = %e, "signal handler could not be installed; graceful shutdown degraded");
    }

    let tick_interval = Duration::from_millis(cfg.tick_ms);
    let idle_reset = Duration::from_secs(cfg.idle_reset_secs);
    let deadline =
        (cfg.duration_secs > 0).then(|| Instant::now() + Duration::from_secs(cfg.duration_secs));
    let policy = cfg.policy;

    let worker = std::thread::Builder::new()
        .name("autobright-tick".into())
        .spawn(move || {
            run_loop(
                &*sensor,
                &*backlight,
                ab,
                policy,
                &mut loop_state,
                &cmd_rx,
                &shared,
                &state_tx,
                &shutdown,
                tick_interval,
                idle_reset,
                deadline,
            );
        });
    match worker {
        Ok(handle) => {
            if let Err(payload) = handle.join() {
                std::panic::resume_unwind(payload);
            }
        }
        Err(e) => {
            error!(error = %e, "failed to spawn autobright tick thread");
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Stopping]) {
        debug!(error = %e, "sd_notify STOPPING=1 failed");
    }
    info!("Autobright stopped cleanly");
    ExitCode::SUCCESS
}

#[allow(
    clippy::too_many_arguments,
    reason = "single tick-loop orchestration point owns all injected resources"
)]
fn run_loop<S, B>(
    sensor: &S,
    backlight: &B,
    mut ab: AutoBrightness,
    policy: BrightnessConfig,
    loop_state: &mut LoopState,
    cmd_rx: &Receiver<Command>,
    shared: &Arc<std::sync::Mutex<PublishedState>>,
    state_tx: &tokio::sync::mpsc::UnboundedSender<PublishedState>,
    shutdown: &AtomicBool,
    tick_interval: Duration,
    idle_reset: Duration,
    deadline: Option<Instant>,
) where
    S: LuxSource + ?Sized,
    B: BacklightIo + ?Sized,
{
    let mut last_tick = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("shutdown requested, stopping autobright tick loop");
            break;
        }
        std::thread::sleep(tick_interval);

        let now = Instant::now();
        let dt = now.duration_since(last_tick).as_secs_f32();
        last_tick = now;

        let mut commands = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            commands.push(cmd);
        }

        let outcome = run_tick(
            sensor,
            backlight,
            &mut ab,
            &policy,
            loop_state,
            TickInput {
                now,
                dt,
                commands: &commands,
                idle_reset,
            },
        );

        let mut guard = match shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("published-state mutex poisoned; recovering stored state");
                poisoned.into_inner()
            }
        };
        *guard = outcome.state;
        let _ = state_tx.send(outcome.state);

        if deadline.is_some_and(|limit| now >= limit) {
            info!("bounded probe duration elapsed, stopping autobright tick loop");
            break;
        }
    }
}

fn initial_published_state(ab: &AutoBrightness, state: &LoopState) -> PublishedState {
    PublishedState {
        auto_enabled: state.auto_enabled,
        brightness_percent: round_percent(ab.brightness()),
        ambient_lux: f64::from(state.last_lux),
        manual_gamma: f64::from(ab.manual_gamma()),
        available: true,
    }
}
