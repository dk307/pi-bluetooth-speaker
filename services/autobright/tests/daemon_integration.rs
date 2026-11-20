//! Host integration tests for the autobright tick seam.
//!
//! Local fakes implement [`autobright::LuxSource`] and
//! [`autobright::BacklightIo`], so every control-loop branch is exercised
//! without an I²C bus or a real sysfs backlight.

use std::cell::{Cell, RefCell};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use autobright::dbus::AutobrightProxy;
use autobright::{
    AutobrightError, AutobrightOptions, BacklightIo, Command, LoopState, LuxSource, ResolvedConfig,
    TickAction, TickInput, run_tick, run_with,
};
use speaker_common::diag::spawn_private_bus;
use speaker_hardware::Config as BrightnessConfig;
use zbus::connection::Builder;

const MAX: u32 = 255;

#[derive(Debug)]
struct FakeSensor {
    reading: Result<f32, &'static str>,
}

impl FakeSensor {
    const fn new(lux: f32) -> Self {
        Self { reading: Ok(lux) }
    }

    const fn fault() -> Self {
        Self {
            reading: Err("simulated I2C fault"),
        }
    }
}

impl LuxSource for FakeSensor {
    fn read_lux(&self) -> Result<f32, AutobrightError> {
        self.reading.map_err(AutobrightError::mock_fault)
    }
}

#[derive(Debug)]
struct FakeBacklight {
    raw: Cell<u32>,
    read_error: Cell<bool>,
    write_error: Cell<bool>,
    writes: RefCell<Vec<u32>>,
    reads: Cell<usize>,
}

impl FakeBacklight {
    fn new(raw: u32) -> Self {
        Self {
            raw: Cell::new(raw),
            read_error: Cell::new(false),
            write_error: Cell::new(false),
            writes: RefCell::new(Vec::new()),
            reads: Cell::new(0),
        }
    }
}

impl BacklightIo for FakeBacklight {
    fn max_brightness(&self) -> u32 {
        MAX
    }

    fn read(&self) -> Result<u32, AutobrightError> {
        self.reads.set(self.reads.get() + 1);
        if self.read_error.get() {
            return Err(AutobrightError::mock_fault("simulated sysfs read fault"));
        }
        Ok(self.raw.get())
    }

    fn write(&self, raw: u32) -> Result<(), AutobrightError> {
        if self.write_error.get() {
            return Err(AutobrightError::mock_fault("simulated EACCES"));
        }
        self.writes.borrow_mut().push(raw);
        self.raw.set(raw);
        Ok(())
    }
}

fn tick(
    sensor: &FakeSensor,
    backlight: &FakeBacklight,
    ab: &mut speaker_hardware::AutoBrightness,
    cfg: &BrightnessConfig,
    state: &mut LoopState,
    commands: &[Command],
) -> TickAction {
    run_tick(
        sensor,
        backlight,
        ab,
        cfg,
        state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands,
            idle_reset: Duration::from_secs(600),
        },
    )
    .action
}

#[test]
fn normal_tick_writes_auto_brightness() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);

    assert!(matches!(action, TickAction::AutoWrite { raw } if raw > 0 && raw <= MAX));
    assert_eq!(state.last_written, backlight.writes.borrow()[0]);
}

#[test]
fn external_write_is_adopted_as_manual_override() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(200);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);

    assert_eq!(action, TickAction::ExternalAdopt { raw: 200 });
    assert!(
        backlight.writes.borrow().is_empty(),
        "auto path must be skipped"
    );
    assert!(
        (cfg.gamma_min..=cfg.gamma_max).contains(&ab.manual_gamma()),
        "adopted override must shift gamma within bounds"
    );
    assert_eq!(state.last_written, 200);
    assert!(state.last_manual.is_some());
}

#[test]
fn manual_percent_command_writes_immediately_and_leaves_auto_mode() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let outcome = run_tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands: &[Command::SetManualPercent(50.0)],
            idle_reset: Duration::from_secs(600),
        },
    );

    assert!(matches!(
        outcome.action,
        TickAction::ManualWrite { raw } if raw > 0 && raw <= MAX
    ));
    assert_eq!(backlight.writes.borrow().len(), 1);
    assert_eq!(state.last_written, backlight.writes.borrow()[0]);
    assert!(state.last_manual.is_some());
    assert!(!state.auto_enabled, "slider drag must leave auto mode");
    assert!(!outcome.state.auto_enabled, "auto=false must be published");
}

#[test]
fn auto_disabled_suppresses_subsequent_writes() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(1000.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let first = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::SetAutoEnabled(false)],
    );
    assert_eq!(first, TickAction::AutoDisabled);
    let writes_after_disable = backlight.writes.borrow().len();

    let second = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);
    assert_eq!(second, TickAction::AutoDisabled);
    assert_eq!(backlight.writes.borrow().len(), writes_after_disable);
}

#[test]
fn manual_drag_suppresses_auto_until_reenabled() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let manual = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::SetManualPercent(60.0)],
    );
    assert!(matches!(manual, TickAction::ManualWrite { .. }));
    let writes_after_drag = backlight.writes.borrow().len();

    let held = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);
    assert_eq!(held, TickAction::AutoDisabled);
    assert_eq!(backlight.writes.borrow().len(), writes_after_drag);

    let reenabled = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::SetAutoEnabled(true)],
    );
    assert!(state.auto_enabled);
    assert!(
        matches!(reenabled, TickAction::AutoWrite { .. } | TickAction::Idle),
        "ambient control resumes after SetAutoEnabled(true)"
    );
}

#[test]
fn idle_timer_clears_manual_bias() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::SetManualPercent(35.0)],
    );
    assert!((ab.manual_gamma() - 1.0).abs() > f32::EPSILON);

    state.last_manual = Some(
        Instant::now()
            .checked_sub(Duration::from_secs(601))
            .expect("current time is after the six-minute epoch"),
    );
    tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);

    assert!((ab.manual_gamma() - 1.0).abs() <= f32::EPSILON);
    assert!(state.last_manual.is_none());
}

#[test]
fn reset_bias_command_clears_manual_gamma() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::SetManualPercent(75.0)],
    );
    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::ResetManualBias],
    );

    assert!((ab.manual_gamma() - 1.0).abs() <= f32::EPSILON);
}

#[test]
fn sensor_fault_holds_last_brightness() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::fault();
    let backlight = FakeBacklight::new(42);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(42);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);

    assert_eq!(action, TickAction::SensorFault);
    assert!(backlight.writes.borrow().is_empty());
    assert_eq!(state.last_written, 42);
}

#[test]
fn write_fault_retries_on_a_later_tick() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    backlight.write_error.set(true);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let failed = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);
    assert!(matches!(failed, TickAction::WriteFault { .. }));
    assert_eq!(
        state.last_written, 0,
        "failed write must not advance last_written"
    );

    backlight.write_error.set(false);
    let recovered = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);
    assert!(matches!(recovered, TickAction::AutoWrite { raw } if raw > 0));
    assert_eq!(state.last_written, backlight.writes.borrow()[0]);
}

#[test]
fn invalid_manual_percent_is_ignored() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[
            Command::SetManualPercent(f64::NAN),
            Command::SetManualPercent(150.0),
        ],
    );

    assert!((ab.manual_gamma() - 1.0).abs() <= f32::EPSILON);
    assert_eq!(state.last_manual, None);
}

#[test]
fn published_state_reflects_policy_telemetry() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let outcome = run_tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands: &[],
            idle_reset: Duration::from_secs(600),
        },
    );

    assert!(outcome.state.available);
    assert!(outcome.state.auto_enabled);
    assert!((0.0..=100.0).contains(&outcome.state.brightness_percent));
    assert!((outcome.state.ambient_lux - 100.0).abs() < f64::EPSILON);
    assert!((outcome.state.manual_gamma - 1.0).abs() < f64::EPSILON);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_daemon_bootstrap_runs_on_a_private_bus() {
    let (_bus, address) = spawn_private_bus();
    let server = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("server connection");
    let client = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("client connection");

    let cfg = ResolvedConfig::resolve(AutobrightOptions {
        backlight_device: Some("fake_backlight".into()),
        tick_ms: Some(10),
        duration_secs: Some(1),
        ..AutobrightOptions::default()
    })
    .expect("resolve bounded probe config");

    let proxy = AutobrightProxy::new(&client).await.expect("client proxy");
    let command_proxy = proxy.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = command_proxy.set_manual_percent(30.0).await;
    });

    let code = run_with(
        cfg,
        Box::new(FakeSensor::new(100.0)),
        Box::new(FakeBacklight::new(0)),
        &server,
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS);

    let (auto_enabled, percent, lux, gamma) = proxy.get_state().await.expect("final state");
    assert!(!auto_enabled, "manual command must leave auto mode");
    assert!(
        (percent - 30.0).abs() < 1e-3,
        "expected 30 percent, got auto={auto_enabled} percent={percent} lux={lux} gamma={gamma}"
    );
}

#[test]
fn production_trait_adapters_work_over_mock_backends() {
    let (sensor, sensor_ctrl) = speaker_hardware::LightSensor::new_mocked();
    sensor_ctrl.set_lux(321.0);
    let lux = LuxSource::read_lux(&sensor).expect("mocked lux read");
    assert!((lux - 321.0).abs() < f32::EPSILON);

    let (backlight, backlight_ctrl) = speaker_hardware::Backlight::new_mocked();
    let max = BacklightIo::max_brightness(&backlight);
    assert!(max > 0);
    let raw = BacklightIo::read(&backlight).expect("mocked backlight read");
    BacklightIo::write(&backlight, raw.min(max)).expect("mocked backlight write");
    assert_eq!(backlight_ctrl.last_written(), raw.min(max));
}

#[test]
fn manual_write_fault_is_logged_without_advancing_last_written() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    backlight.write_error.set(true);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let action = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut state,
        &[Command::SetManualPercent(50.0)],
    );

    assert!(matches!(action, TickAction::ManualWrite { .. }));
    assert!(backlight.writes.borrow().is_empty());
    assert_eq!(state.last_written, 0);
    assert!(!state.auto_enabled);
}

#[test]
fn backlight_read_fault_skips_external_detection_but_keeps_auto() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    backlight.read_error.set(true);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);

    assert!(matches!(action, TickAction::AutoWrite { .. }));
    assert_eq!(state.last_written, backlight.writes.borrow()[0]);
}

#[test]
fn non_finite_lux_sample_is_ignored() {
    let cfg = BrightnessConfig::default();
    let sensor = FakeSensor::new(f32::NAN);
    let backlight = FakeBacklight::new(0);
    let mut ab = speaker_hardware::AutoBrightness::new(100.0, cfg);
    let mut state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut state, &[]);

    assert_eq!(action, TickAction::SensorFault);
    assert!(backlight.writes.borrow().is_empty());
}

fn probe_config() -> ResolvedConfig {
    ResolvedConfig::resolve(AutobrightOptions {
        tick_ms: Some(10),
        duration_secs: Some(1),
        ..AutobrightOptions::default()
    })
    .expect("bounded probe config")
}

async fn private_server() -> (speaker_common::diag::PrivateBusGuard, zbus::Connection) {
    let (guard, address) = spawn_private_bus();
    let connection = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("server connection");
    (guard, connection)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_fails_loudly_on_initial_backlight_read_fault() {
    let (_bus, server) = private_server().await;
    let backlight = FakeBacklight::new(0);
    backlight.read_error.set(true);

    let code = run_with(
        probe_config(),
        Box::new(FakeSensor::new(100.0)),
        Box::new(backlight),
        &server,
    )
    .await;

    assert_eq!(code, ExitCode::FAILURE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_fails_loudly_on_initial_sensor_fault() {
    let (_bus, server) = private_server().await;

    let code = run_with(
        probe_config(),
        Box::new(FakeSensor::fault()),
        Box::new(FakeBacklight::new(0)),
        &server,
    )
    .await;

    assert_eq!(code, ExitCode::FAILURE);
}
