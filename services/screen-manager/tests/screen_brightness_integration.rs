//! Integration tests for the embedded brightness engine, command queue, and shell wiring.

#![cfg(feature = "ui")]

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use screen_manager::config::ManualBrightnessPercent;
use screen_manager::daemon::BtSharedUpdate;
use screen_manager::screen_brightness::{
    AutoBrightness, BacklightIo, BrightnessCommand, BrightnessCommandQueue, BrightnessConfig,
    BrightnessEngine, BrightnessError, BrightnessState, LoopState, LuxSource, TickAction,
    TickInput, UnavailableLuxSource, default_policy, run_tick, run_worker,
};

const MAX: u32 = 255;
const IDLE_RESET: Duration = Duration::from_secs(600);

fn state(auto: bool, percent: f32) -> BrightnessState {
    BrightnessState::new(auto, f64::from(percent), Some(120.0), 1.0)
}

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
    fn read_lux(&self) -> Result<f32, BrightnessError> {
        self.reading.map_err(BrightnessError::failed)
    }
}

#[derive(Debug)]
struct FakeBacklight {
    raw: Cell<u32>,
    read_error: Cell<bool>,
    write_error: Cell<bool>,
    writes: RefCell<Vec<u32>>,
}

impl FakeBacklight {
    fn new(raw: u32) -> Self {
        Self {
            raw: Cell::new(raw),
            read_error: Cell::new(false),
            write_error: Cell::new(false),
            writes: RefCell::new(Vec::new()),
        }
    }
}

impl BacklightIo for FakeBacklight {
    fn max_brightness(&self) -> u32 {
        MAX
    }

    fn read(&self) -> Result<u32, BrightnessError> {
        if self.read_error.get() {
            return Err(BrightnessError::failed("simulated sysfs read fault"));
        }
        Ok(self.raw.get())
    }

    fn write(&self, raw: u32) -> Result<(), BrightnessError> {
        if self.write_error.get() {
            return Err(BrightnessError::failed("simulated EACCES"));
        }
        self.writes.borrow_mut().push(raw);
        self.raw.set(raw);
        Ok(())
    }
}

fn tick(
    sensor: &FakeSensor,
    backlight: &FakeBacklight,
    ab: &mut AutoBrightness,
    cfg: &BrightnessConfig,
    state: &mut LoopState,
    commands: &[BrightnessCommand],
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
            idle_reset: IDLE_RESET,
        },
    )
    .action
}

#[test]
fn floor_writes_a_nonzero_raw_duty() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(0.0);
    let backlight = FakeBacklight::new(100);
    let mut ab = AutoBrightness::new(0.0, cfg);
    let mut loop_state = LoopState::new(100);

    let auto = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);
    assert!(matches!(auto, TickAction::AutoWrite { raw } if raw >= 1));

    let manual = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetPercent(1.0)],
    );
    assert!(matches!(manual, TickAction::ManualWrite { raw } if raw >= 1));
    assert!(backlight.writes.borrow().iter().all(|&raw| raw >= 1));
}

#[test]
fn state_sanitizes_non_finite_values() {
    let s = BrightnessState::new(true, f64::NAN, Some(f64::INFINITY), f64::NAN);
    assert_eq!(s.brightness_percent, 0.0);
    assert_eq!(s.ambient_lux, None);
    assert_eq!(s.manual_gamma, 1.0);
    let clamped = BrightnessState::new(false, 250.0, Some(-5.0), -1.0);
    assert_eq!(clamped.brightness_percent, 100.0);
    assert_eq!(clamped.ambient_lux, None);
    assert_eq!(clamped.manual_gamma, 0.0);
}

#[test]
fn command_queue_drains_and_notifies() {
    let queue = BrightnessCommandQueue::new();
    queue.push(BrightnessCommand::SetPercent(42.0));
    queue.notify_change();
    let (commands, changed) = queue.drain_idle(Duration::from_millis(1));
    assert_eq!(commands, vec![BrightnessCommand::SetPercent(42.0)]);
    assert!(changed);
}

#[test]
fn normal_tick_writes_auto_brightness() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);

    assert!(matches!(action, TickAction::AutoWrite { raw } if raw > 0 && raw <= MAX));
    assert_eq!(loop_state.last_written, backlight.writes.borrow()[0]);
}

#[test]
fn external_write_is_adopted_as_manual_override() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(200);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);

    assert_eq!(action, TickAction::ExternalAdopt { raw: 200 });
    assert!(
        backlight.writes.borrow().is_empty(),
        "auto path must be skipped"
    );
    assert!(
        (cfg.gamma_min..=cfg.gamma_max).contains(&ab.manual_gamma()),
        "adopted override must shift gamma within bounds"
    );
    assert_eq!(loop_state.last_written, 200);
    assert!(loop_state.last_manual.is_some());
}

#[test]
fn manual_percent_command_writes_immediately_and_leaves_auto_mode() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let outcome = run_tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands: &[BrightnessCommand::SetPercent(50.0)],
            idle_reset: IDLE_RESET,
        },
    );

    assert!(matches!(outcome.action, TickAction::ManualWrite { raw } if raw > 0 && raw <= MAX));
    assert_eq!(backlight.writes.borrow().len(), 1);
    assert_eq!(loop_state.last_written, backlight.writes.borrow()[0]);
    assert!(loop_state.last_manual.is_some());
    assert!(!loop_state.auto_enabled, "slider drag must leave auto mode");
    assert!(!outcome.state.auto_enabled, "auto=false must be published");
}

#[test]
fn auto_disabled_suppresses_subsequent_writes() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(1000.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let first = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetAuto(false)],
    );
    assert_eq!(first, TickAction::AutoDisabled);
    let writes_after_disable = backlight.writes.borrow().len();

    let second = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);
    assert_eq!(second, TickAction::AutoDisabled);
    assert_eq!(backlight.writes.borrow().len(), writes_after_disable);
}

#[test]
fn manual_mode_keeps_ambient_lux_fresh() {
    let cfg = default_policy();
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let sensor = FakeSensor::new(321.0);
    let first = run_tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands: &[BrightnessCommand::SetAuto(false)],
            idle_reset: IDLE_RESET,
        },
    );
    assert_eq!(first.action, TickAction::AutoDisabled);
    assert!((first.state.ambient_lux.unwrap() - 321.0).abs() < f32::EPSILON);

    let brighter = FakeSensor::new(654.0);
    let second = run_tick(
        &brighter,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands: &[],
            idle_reset: IDLE_RESET,
        },
    );
    assert_eq!(second.action, TickAction::AutoDisabled);
    assert!((second.state.ambient_lux.unwrap() - 654.0).abs() < f32::EPSILON);
    assert!((second.state.brightness_percent - first.state.brightness_percent).abs() < 0.5);
}

#[test]
fn manual_drag_suppresses_auto_until_reenabled() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let manual = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetPercent(60.0)],
    );
    assert!(matches!(manual, TickAction::ManualWrite { .. }));
    let writes_after_drag = backlight.writes.borrow().len();

    let held = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);
    assert_eq!(held, TickAction::AutoDisabled);
    assert_eq!(backlight.writes.borrow().len(), writes_after_drag);

    let reenabled = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetAuto(true)],
    );
    assert!(loop_state.auto_enabled);
    assert!(
        matches!(reenabled, TickAction::AutoWrite { .. } | TickAction::Idle),
        "ambient control resumes after SetAuto(true)"
    );
}

#[test]
fn idle_timer_clears_manual_bias() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetPercent(35.0)],
    );
    assert!((ab.manual_gamma() - 1.0).abs() > f32::EPSILON);

    loop_state.last_manual = Some(
        Instant::now()
            .checked_sub(Duration::from_secs(601))
            .expect("current time is after the six-minute epoch"),
    );
    tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);

    assert!((ab.manual_gamma() - 1.0).abs() <= f32::EPSILON);
    assert!(loop_state.last_manual.is_none());
}

#[test]
fn reset_bias_command_clears_manual_gamma() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetPercent(75.0)],
    );
    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::ResetBias],
    );

    assert!((ab.manual_gamma() - 1.0).abs() <= f32::EPSILON);
}

#[test]
fn sensor_fault_holds_last_brightness() {
    let cfg = default_policy();
    let sensor = FakeSensor::fault();
    let backlight = FakeBacklight::new(42);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(42);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);

    assert_eq!(action, TickAction::SensorFault);
    assert!(backlight.writes.borrow().is_empty());
    assert_eq!(loop_state.last_written, 42);
}

#[test]
fn write_fault_retries_on_a_later_tick() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    backlight.write_error.set(true);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let failed = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);
    assert!(matches!(failed, TickAction::WriteFault { .. }));
    assert!(loop_state.backlight_fault_logged);
    assert_eq!(
        loop_state.last_written, 0,
        "failed write must not advance last_written"
    );

    backlight.write_error.set(false);
    let recovered = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);
    assert!(matches!(recovered, TickAction::AutoWrite { raw } if raw > 0));
    assert!(!loop_state.backlight_fault_logged);
    assert_eq!(loop_state.last_written, backlight.writes.borrow()[0]);
}

#[test]
fn manual_write_fault_does_not_advance_last_written() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    backlight.write_error.set(true);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let initial_brightness = ab.brightness();
    let mut loop_state = LoopState::new(0);

    let failed = tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[BrightnessCommand::SetPercent(50.0)],
    );

    assert!(matches!(failed, TickAction::WriteFault { .. }));
    assert!(backlight.writes.borrow().is_empty());
    assert_eq!(loop_state.last_written, 0);
    assert_eq!(
        loop_state.pending_manual.map(|(percent, _)| percent),
        Some(0.5)
    );
    assert!(!loop_state.auto_enabled);
    // The failed level must never be reported as authoritative telemetry.
    assert!((ab.brightness() - initial_brightness).abs() < f32::EPSILON);

    backlight.write_error.set(false);
    let recovered = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);
    assert!(matches!(recovered, TickAction::ManualWrite { raw } if raw > 0));
    assert_eq!(loop_state.pending_manual, None);
    assert_eq!(loop_state.last_written, backlight.writes.borrow()[0]);
    assert!((ab.brightness() - 0.5).abs() < 0.001);
}

#[test]
fn backlight_read_fault_skips_external_detection_but_keeps_auto() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    backlight.read_error.set(true);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);

    assert!(matches!(action, TickAction::AutoWrite { .. }));
    assert_eq!(loop_state.last_written, backlight.writes.borrow()[0]);
}

#[test]
fn invalid_manual_percent_is_ignored() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        &[
            BrightnessCommand::SetPercent(f32::NAN),
            BrightnessCommand::SetPercent(150.0),
        ],
    );

    assert!((ab.manual_gamma() - 1.0).abs() <= f32::EPSILON);
    assert_eq!(loop_state.last_manual, None);
}

#[test]
fn non_finite_lux_sample_is_ignored() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(f32::NAN);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let action = tick(&sensor, &backlight, &mut ab, &cfg, &mut loop_state, &[]);

    assert_eq!(action, TickAction::SensorFault);
    assert!(backlight.writes.borrow().is_empty());
}

#[test]
fn published_state_reflects_policy_telemetry() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    let outcome = run_tick(
        &sensor,
        &backlight,
        &mut ab,
        &cfg,
        &mut loop_state,
        TickInput {
            now: Instant::now(),
            dt: 0.05,
            commands: &[],
            idle_reset: IDLE_RESET,
        },
    );

    assert!(outcome.state.auto_enabled);
    assert!((0.0..=100.0).contains(&outcome.state.brightness_percent));
    assert!((outcome.state.ambient_lux.unwrap() - 100.0).abs() < f32::EPSILON);
    assert!((outcome.state.manual_gamma - 1.0).abs() < f32::EPSILON);
}

#[test]
fn engine_bootstrap_fails_loudly_on_hardware_faults() {
    let cfg = default_policy();
    let sensor = FakeSensor::fault();
    let backlight = FakeBacklight::new(0);
    let err = BrightnessEngine::try_new(sensor, backlight, cfg, true)
        .expect_err("initial lux read must fail loudly");
    assert!(err.summary().contains("brightness error"));
    assert!(!err.summary().contains("Backtrace"));

    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    // A write fault does not affect the bootstrap read; verify the engine is
    // usable and reports the configured auto preference.
    let engine = BrightnessEngine::try_new(sensor, backlight, cfg, false)
        .expect("mocked bootstrap succeeds");
    assert!(!engine.state().auto_enabled);
}

#[test]
fn unavailable_sensor_keeps_manual_brightness_usable() {
    let cfg = default_policy();
    let backlight = FakeBacklight::new(10);
    let mut engine = BrightnessEngine::new_seeded(UnavailableLuxSource, backlight, cfg, true, 50.0)
        .expect("backlight-only bootstrap succeeds");

    let auto = engine.tick(Instant::now(), 0.05, &[], IDLE_RESET);
    assert!(matches!(auto.action, TickAction::SensorFault));
    assert_eq!(auto.state.ambient_lux, None);
    assert!(engine.state().auto_enabled);

    let manual = engine.tick(
        Instant::now(),
        0.0,
        &[BrightnessCommand::SetPercent(60.0)],
        IDLE_RESET,
    );
    assert!(matches!(manual.action, TickAction::ManualWrite { .. }));
    assert!(!engine.state().auto_enabled);
    assert!((engine.state().brightness_percent - 60.0).abs() < 0.5);
}

#[test]
fn transient_first_lux_read_falls_back_to_manual_control() {
    let cfg = default_policy();
    assert!(
        BrightnessEngine::try_new(FakeSensor::fault(), FakeBacklight::new(10), cfg, true).is_err(),
        "try_new still reports a genuine bootstrap sensor fault"
    );

    // The worker seeds nominally in this case so a transient I2C hiccup cannot
    // kill manual brightness for the whole process lifetime.
    let mut engine =
        BrightnessEngine::new_seeded(FakeSensor::fault(), FakeBacklight::new(10), cfg, true, 50.0)
            .expect("seeded bootstrap succeeds");
    let manual = engine.tick(
        Instant::now(),
        0.0,
        &[BrightnessCommand::SetPercent(55.0)],
        IDLE_RESET,
    );
    assert!(matches!(manual.action, TickAction::ManualWrite { .. }));
    assert!(!engine.state().auto_enabled);
    assert!((engine.state().brightness_percent - 55.0).abs() < 0.5);
}

#[test]
fn engine_applies_manual_percent_when_auto_disabled_at_boot() {
    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut engine = BrightnessEngine::try_new(sensor, backlight, cfg, false)
        .expect("mocked bootstrap succeeds");
    let outcome = engine.tick(
        Instant::now(),
        0.0,
        &[BrightnessCommand::SetPercent(40.0)],
        IDLE_RESET,
    );
    assert!(matches!(outcome.action, TickAction::ManualWrite { .. }));
    assert!(!engine.state().auto_enabled);
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
fn typed_error_sources_are_preserved() {
    let (sensor, sensor_ctrl) = speaker_hardware::LightSensor::new_mocked();
    sensor_ctrl.set_fault(Some("sensor fault".into()));
    let err = LuxSource::read_lux(&sensor).expect_err("injected sensor fault");
    assert!(err.summary().contains("ambient sensor error"));
    assert!(std::error::Error::source(&err).is_some());

    let (backlight, backlight_ctrl) = speaker_hardware::Backlight::new_mocked();
    backlight_ctrl.set_read_error(Some("sysfs fault".into()));
    let err = BacklightIo::read(&backlight).expect_err("injected sysfs fault");
    assert!(err.summary().contains("backlight error"));
    assert!(std::error::Error::source(&err).is_some());

    let plain = BrightnessError::failed("injected");
    assert!(std::error::Error::source(&plain).is_none());
}

#[test]
fn config_round_trips_brightness_preferences() {
    use screen_manager::config::CoreConfig;
    use speaker_common::diag::ScopedTempDir;

    let dir = ScopedTempDir::new("brightness-config");
    let path = dir.path().join("config.json");
    let config = CoreConfig {
        auto_brightness_enabled: false,
        brightness_manual_percent: ManualBrightnessPercent::new(37),
        ..CoreConfig::default()
    };
    config.save(&path).expect("save config");
    let loaded = CoreConfig::load(&path).expect("load config");
    assert!(!loaded.auto_brightness_enabled);
    assert_eq!(
        loaded.brightness_manual_percent,
        ManualBrightnessPercent::new(37)
    );
}

#[test]
fn settings_catalog_exposes_auto_brightness() {
    use screen_manager::config::CoreConfig;
    use screen_manager::settings_catalog::{SettingId, apply_toggle, find_setting, row_state};

    let mut config = CoreConfig::default();
    assert!(config.auto_brightness_enabled);
    let descriptor = find_setting(SettingId::AutoBrightness);
    assert!(row_state(&config, descriptor).right_selected);
    assert!(apply_toggle(&mut config, SettingId::AutoBrightness, false));
    assert!(!config.auto_brightness_enabled);
    assert!(!row_state(&config, descriptor).right_selected);
}

#[test]
fn shell_brightness_overlay_callbacks_drive_the_engine() {
    use screen_manager::ui::{AppShell, Framebuffer, MainWindow};

    let fb = Framebuffer::new_mocked().0;
    let _platform = fb.init_platform().expect("offscreen platform");
    let window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(window);
    let queue = BrightnessCommandQueue::new();
    shell.wire_brightness(&queue);

    assert!(shell.window().get_auto_brightness_enabled());
    assert_eq!(shell.window().get_brightness_value(), 100.0);

    shell.show_brightness_hud();
    assert!(shell.is_brightness_hud_active());
    assert!(shell.is_brightness_hud_visible());
    shell.dismiss_brightness_hud();
    assert!(!shell.is_brightness_hud_visible());

    let cfg = default_policy();
    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut ab = AutoBrightness::new(100.0, cfg);
    let mut loop_state = LoopState::new(0);

    shell.window().invoke_brightness_changed(42.0);
    assert_eq!(shell.window().get_brightness_value(), 42.0);
    assert!(!shell.window().get_auto_brightness_enabled());
    for command in queue.drain() {
        let outcome = run_tick(
            &sensor,
            &backlight,
            &mut ab,
            &cfg,
            &mut loop_state,
            TickInput {
                now: Instant::now(),
                dt: 0.05,
                commands: &[command],
                idle_reset: IDLE_RESET,
            },
        );
        shell.apply_brightness(&outcome.state);
    }
    assert!(!shell.window().get_auto_brightness_enabled());

    shell.window().invoke_auto_brightness_toggled(true);
    assert!(shell.window().get_auto_brightness_enabled());
    for command in queue.drain() {
        let outcome = run_tick(
            &sensor,
            &backlight,
            &mut ab,
            &cfg,
            &mut loop_state,
            TickInput {
                now: Instant::now(),
                dt: 0.05,
                commands: &[command],
                idle_reset: IDLE_RESET,
            },
        );
        shell.apply_brightness(&outcome.state);
    }
    assert!(shell.window().get_auto_brightness_enabled());

    // A stale in-flight snapshot must not yank the thumb back while the settle
    // window is fresh; after the window expires the authoritative value wins.
    shell.window().invoke_brightness_changed(63.0);
    assert_eq!(shell.window().get_brightness_value(), 63.0);
    shell.apply_brightness(&state(false, 100.0));
    assert_eq!(shell.window().get_brightness_value(), 63.0);
    std::thread::sleep(std::time::Duration::from_millis(1_600));
    shell.apply_brightness(&state(false, 100.0));
    assert_eq!(shell.window().get_brightness_value(), 100.0);
    assert!(shell.window().get_ambient_lux_available());
    shell.apply_brightness(&BrightnessState::new(false, 50.0, None, 1.0));
    assert!(!shell.window().get_ambient_lux_available());
    let _ = queue.drain();

    // The Settings row must match the HUD toggle: disabling AUTO also drops
    // the learned gamma bias.
    shell
        .window()
        .invoke_setting_toggle("auto_brightness".into(), false);
    assert_eq!(
        queue.drain(),
        vec![
            BrightnessCommand::SetAuto(false),
            BrightnessCommand::ResetBias
        ]
    );
}

#[test]
fn brightness_hud_renders_at_every_font_size_setting() {
    use screen_manager::ui::{AppShell, Framebuffer, MainWindow};
    use slint::ComponentHandle;

    let mut fb = Framebuffer::new_mocked().0;
    let platform_window = fb.init_platform().expect("offscreen platform");
    let window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(window);
    shell.window().show().expect("show MainWindow");
    shell.show_brightness_hud();
    assert!(shell.is_brightness_hud_active());

    // The Settings font-size slider drives TouchScale; the overlay must lay out
    // and render at both ends of the supported range without clipping or panics.
    for scale in [1.0, 1.35] {
        shell.set_font_scale(scale);
        assert_eq!(shell.font_scale(), scale);
        fb.render_frame_full_redraw(&platform_window)
            .unwrap_or_else(|e| panic!("render brightness HUD at font scale {scale}: {e}"));
    }
}

#[test]
fn commit_update_merges_brightness_delta() {
    use screen_manager::daemon::BtMonitorState;

    let shared = Mutex::new(BtSharedUpdate::default());
    let snapshot = state(false, 42.0);
    BtMonitorState::commit_update(
        &shared,
        BtSharedUpdate {
            brightness: Some(snapshot),
            ..BtSharedUpdate::default()
        },
    );

    let lock = shared.lock().expect("shared state lock");
    assert_eq!(lock.brightness, Some(snapshot));
    assert!(lock.playback.is_none(), "unrelated deltas stay untouched");
}

#[test]
fn apply_shared_bt_state_consumes_brightness_snapshot() {
    use screen_manager::ui::{AppShell, Framebuffer, MainWindow};

    let fb = Framebuffer::new_mocked().0;
    let _platform = fb.init_platform().expect("offscreen platform");
    let window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(window);
    let shared = Arc::new(Mutex::new(BtSharedUpdate {
        brightness: Some(state(false, 37.0)),
        ..BtSharedUpdate::default()
    }));

    screen_manager::daemon::apply_shared_bt_state(&shell, &shared);

    assert!((shell.window().get_brightness_value() - 37.0).abs() < 0.01);
    assert!(!shell.window().get_auto_brightness_enabled());
    assert!(
        shared
            .lock()
            .expect("shared state lock")
            .brightness
            .is_none(),
        "one-shot brightness snapshot must be consumed"
    );
}

#[test]
fn apply_shared_bt_state_recovers_poisoned_lock_and_applies_cover() {
    use screen_manager::ui::{AppShell, Framebuffer, MainWindow};

    let fb = Framebuffer::new_mocked().0;
    let _platform = fb.init_platform().expect("offscreen platform");
    let window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(window);
    let shared = Arc::new(Mutex::new(BtSharedUpdate {
        cover: Some(Some(slint::SharedPixelBuffer::new(1, 1))),
        brightness: Some(state(true, 55.0)),
        ..BtSharedUpdate::default()
    }));

    let poison_target = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = poison_target.lock().expect("poison lock");
        panic!("intentional poison");
    })
    .join();

    screen_manager::daemon::apply_shared_bt_state(&shell, &shared);
    assert!(shell.window().get_has_cover_art());
    assert!((shell.window().get_brightness_value() - 55.0).abs() < 0.01);

    let mut lock = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(lock.cover.is_none());
    assert!(lock.brightness.is_none());
    lock.cover = Some(None);
    drop(lock);

    screen_manager::daemon::apply_shared_bt_state(&shell, &shared);
    assert!(!shell.window().get_has_cover_art());
}

#[test]
fn worker_publishes_initial_state_and_recovers_poisoned_shared() {
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));
    let commands = BrightnessCommandQueue::new();

    let poison_target = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = poison_target.lock().expect("poison lock");
        panic!("intentional poison");
    })
    .join();

    let sensor = FakeSensor::new(100.0);
    let backlight = FakeBacklight::new(0);
    let mut engine = BrightnessEngine::try_new(sensor, backlight, default_policy(), true)
        .expect("mocked engine bootstrap");
    let worker_shared = Arc::clone(&shared);
    let worker_commands = commands.clone();
    let worker_running = Arc::clone(&running);
    let worker = std::thread::spawn(move || {
        run_worker(
            &mut engine,
            &worker_shared,
            &worker_commands,
            &worker_running,
            IDLE_RESET,
        );
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let published = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .brightness;
        if published.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker never published initial state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    commands.notify_change();
    worker.join().expect("brightness worker panicked");
}

#[test]
fn spawned_mocked_brightness_worker_publishes_and_joins() {
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));
    let queue = BrightnessCommandQueue::new();

    let handle = screen_manager::daemon::spawn_brightness_worker(
        &shared,
        &running,
        &queue,
        true,
        true,
        ManualBrightnessPercent::DEFAULT,
    )
    .expect("spawn mocked brightness worker");
    let deadline = Instant::now() + Duration::from_secs(3);
    while shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .brightness
        .is_none()
    {
        assert!(Instant::now() < deadline, "worker never published state");
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    queue.notify_change();
    handle.join().expect("mocked brightness worker panicked");
}

#[test]
fn sleep_or_stop_returns_promptly_when_running_is_cleared() {
    let running = Arc::new(AtomicBool::new(true));
    let stopper = Arc::clone(&running);
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        stopper.store(false, Ordering::Relaxed);
    });
    let started = Instant::now();
    // A 5 s sleep with 100 ms slices must abort within a fraction of that
    // once `running` is cleared (worker shutdown must not hang on backoff).
    screen_manager::daemon::sleep_or_stop(Duration::from_secs(5), &running);
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "sleep_or_stop must return promptly when stopped, took {:?}",
        started.elapsed()
    );
    stopper.join().expect("stopper thread panicked");

    // With `running` still true it sleeps for the full requested duration.
    let running = AtomicBool::new(true);
    let started = Instant::now();
    screen_manager::daemon::sleep_or_stop(Duration::from_millis(30), &running);
    assert!(started.elapsed() >= Duration::from_millis(30));
}

#[test]
fn live_brightness_worker_exits_when_stopped_before_hardware_setup() {
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(false));
    let queue = BrightnessCommandQueue::new();

    // The worker checks the stop flag before opening I²C or sysfs, so this
    // stays hermetic on developer hosts.
    let handle = screen_manager::daemon::spawn_brightness_worker(
        &shared,
        &running,
        &queue,
        false,
        true,
        ManualBrightnessPercent::DEFAULT,
    )
    .expect("spawn brightness worker");
    handle.join().expect("brightness worker panicked");
}

#[test]
fn join_brightness_worker_handles_spawn_failure_and_rethrows_panic() {
    screen_manager::daemon::join_brightness_worker(Err(std::io::Error::other("spawn denied")));
    let clean = std::thread::spawn(|| {});
    screen_manager::daemon::join_brightness_worker(Ok(clean));

    let panicking = std::thread::spawn(|| panic!("intentional worker bug"));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        screen_manager::daemon::join_brightness_worker(Ok(panicking));
    }));
    assert!(
        outcome.is_err(),
        "worker panics must resume_unwind (M-PANIC-ON-BUG)"
    );
}
