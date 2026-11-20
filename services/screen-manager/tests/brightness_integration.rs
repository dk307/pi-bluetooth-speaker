//! Integration tests for the brightness overlay client, command queue, and shell wiring.

#![cfg(feature = "ui")]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use screen_manager::brightness::{
    BrightnessClient, BrightnessCommand, BrightnessCommandQueue, BrightnessState, coalesce,
    run_worker,
};
use screen_manager::daemon::BtSharedUpdate;

fn state(auto: bool, percent: f32) -> BrightnessState {
    BrightnessState::new(auto, f64::from(percent), 120.0, 1.0)
}

#[test]
fn state_sanitizes_non_finite_values() {
    let s = BrightnessState::new(true, f64::NAN, f64::INFINITY, f64::NAN);
    assert_eq!(s.brightness_percent, 0.0);
    assert_eq!(s.ambient_lux, 0.0);
    assert_eq!(s.manual_gamma, 1.0);
    let clamped = BrightnessState::new(false, 250.0, -5.0, -1.0);
    assert_eq!(clamped.brightness_percent, 100.0);
    assert_eq!(clamped.ambient_lux, 0.0);
    assert_eq!(clamped.manual_gamma, 0.0);
}

#[test]
fn coalesce_keeps_last_value_per_kind() {
    let out = coalesce(vec![
        BrightnessCommand::SetPercent(10.0),
        BrightnessCommand::SetAuto(true),
        BrightnessCommand::SetPercent(70.0),
        BrightnessCommand::SetAuto(false),
        BrightnessCommand::ResetBias,
    ]);
    assert_eq!(
        out,
        vec![
            BrightnessCommand::SetPercent(70.0),
            BrightnessCommand::SetAuto(false),
            BrightnessCommand::ResetBias,
        ]
    );
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
fn mock_client_applies_commands_and_reports_failures() {
    let (client, ctrl) = BrightnessClient::new_mocked(state(true, 100.0));
    assert_eq!(client.state().expect("state"), state(true, 100.0));

    let next = client
        .apply(BrightnessCommand::SetPercent(35.5))
        .expect("apply percent");
    assert!(!next.auto_enabled);
    assert_eq!(next.brightness_percent, 35.5);
    assert_eq!(next.manual_gamma, 1.0);

    client
        .apply(BrightnessCommand::SetAuto(true))
        .expect("apply auto");
    assert!(client.state().expect("state").auto_enabled);

    ctrl.set_state(state(false, 12.0));
    assert_eq!(client.state().expect("state").brightness_percent, 12.0);

    ctrl.fail_next_write();
    client
        .apply(BrightnessCommand::ResetBias)
        .expect_err("injected mock failure");
    // The injected failure is returned before the command is recorded.
    assert_eq!(ctrl.executed().len(), 2);
}

#[test]
fn worker_publishes_applied_state_to_shared_update() {
    let (client, _ctrl) = BrightnessClient::new_mocked(state(true, 100.0));
    let queue = BrightnessCommandQueue::new();
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));

    let worker_shared = Arc::clone(&shared);
    let worker_running = Arc::clone(&running);
    let worker_queue = queue.clone();
    let handle = std::thread::spawn(move || {
        run_worker(&client, &worker_shared, &worker_queue, &worker_running);
    });

    queue.push(BrightnessCommand::SetPercent(64.0));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let published = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .brightness;
        if published.is_some_and(|s| (s.brightness_percent - 64.0).abs() < 0.01) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not publish brightness state"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    running.store(false, Ordering::Relaxed);
    queue.notify_change();
    handle.join().expect("brightness worker panicked");
}

#[test]
fn config_round_trips_brightness_preferences() {
    use screen_manager::config::CoreConfig;
    use speaker_common::diag::ScopedTempDir;

    let dir = ScopedTempDir::new("brightness-config");
    let path = dir.path().join("config.json");
    let config = CoreConfig {
        auto_brightness_enabled: false,
        brightness_manual_percent: 37,
        ..CoreConfig::default()
    };
    config.save(&path).expect("save config");
    let loaded = CoreConfig::load(&path).expect("load config");
    assert!(!loaded.auto_brightness_enabled);
    assert_eq!(loaded.brightness_manual_percent, 37);
}

#[test]
fn shell_brightness_overlay_callbacks_drive_the_daemon() {
    use screen_manager::ui::{AppShell, Framebuffer, MainWindow};

    let fb = Framebuffer::new_mocked().0;
    let _platform = fb.init_platform().expect("offscreen platform");
    let window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(window);
    let (client, _ctrl) = BrightnessClient::new_mocked(state(true, 100.0));
    let queue = BrightnessCommandQueue::new();
    shell.wire_brightness(&queue);

    assert!(shell.window().get_auto_brightness_enabled());
    assert_eq!(shell.window().get_brightness_value(), 100.0);

    shell.show_brightness_hud();
    assert!(shell.is_brightness_hud_active());
    assert!(shell.is_brightness_hud_visible());
    shell.dismiss_brightness_hud();
    assert!(!shell.is_brightness_hud_visible());

    shell.window().invoke_brightness_changed(42.0);
    assert_eq!(shell.window().get_brightness_value(), 42.0);
    assert!(!shell.window().get_auto_brightness_enabled());
    for command in queue.drain() {
        let next = client.apply(command).expect("apply brightness command");
        shell.apply_brightness(&next);
    }
    assert_eq!(shell.window().get_brightness_value(), 42.0);

    shell.window().invoke_auto_brightness_toggled(true);
    assert!(shell.window().get_auto_brightness_enabled());
    for command in queue.drain() {
        let next = client.apply(command).expect("apply auto command");
        shell.apply_brightness(&next);
    }
    assert!(shell.window().get_auto_brightness_enabled());

    // A rejected write publishes the pre-command state: the overlay must
    // converge to it immediately rather than holding the optimistic percent
    // for the whole settle window.
    shell.window().invoke_brightness_changed(63.0);
    assert_eq!(shell.window().get_brightness_value(), 63.0);
    shell.apply_brightness(&state(false, 100.0));
    assert_eq!(shell.window().get_brightness_value(), 100.0);
    let _ = queue.drain();
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
fn worker_reverts_to_last_known_state_on_command_failure() {
    let (client, ctrl) = BrightnessClient::new_mocked(state(true, 100.0));
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let commands = BrightnessCommandQueue::new();
    let running = Arc::new(AtomicBool::new(true));

    // A poisoned shared update must be recovered, not silently dropped.
    let poison_target = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = poison_target.lock().expect("poison lock");
        panic!("intentional poison");
    })
    .join();

    let worker_shared = Arc::clone(&shared);
    let worker_commands = commands.clone();
    let worker_running = Arc::clone(&running);
    let worker = std::thread::spawn(move || {
        run_worker(&client, &worker_shared, &worker_commands, &worker_running);
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .brightness
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "worker never published initial state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    ctrl.fail_next_write();
    commands.push(BrightnessCommand::SetPercent(50.0));
    // A failed mock write is not recorded by `executed()`, so wait for the
    // worker's failure branch to run and re-publish the last known state.
    std::thread::sleep(Duration::from_millis(100));
    std::thread::sleep(Duration::from_millis(50));
    let published = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .brightness
        .expect("published state");
    assert!(published.auto_enabled);
    assert_eq!(published.brightness_percent, 100.0);

    running.store(false, Ordering::Relaxed);
    worker.join().expect("worker panicked");
}

#[test]
fn failed_client_error_has_clean_summary_and_no_source() {
    let (client, ctrl) = BrightnessClient::new_mocked(state(true, 50.0));
    ctrl.fail_next_write();
    let err = client
        .apply(BrightnessCommand::ResetBias)
        .expect_err("injected failure");
    assert!(err.summary().contains("brightness error"));
    assert!(!err.summary().contains("Backtrace"));
    assert!(std::error::Error::source(&err).is_none());
}

#[test]
fn mock_client_clamps_non_finite_percent_and_resets_bias() {
    let (client, ctrl) =
        BrightnessClient::new_mocked(BrightnessState::new(false, 80.0, 100.0, 1.5));

    let zeroed = client
        .apply(BrightnessCommand::SetPercent(f32::NAN))
        .expect("clamp NaN percent");
    assert_eq!(zeroed.brightness_percent, 0.0);
    assert!(!zeroed.auto_enabled);

    let reset = client
        .apply(BrightnessCommand::ResetBias)
        .expect("reset manual bias");
    assert_eq!(reset.manual_gamma, 1.0);

    let auto = client
        .apply(BrightnessCommand::SetAuto(true))
        .expect("enable auto");
    assert!(auto.auto_enabled);
    assert_eq!(ctrl.executed().len(), 3);
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
fn spawned_mocked_brightness_worker_publishes_and_joins() {
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));
    let queue = BrightnessCommandQueue::new();

    let handle = screen_manager::daemon::spawn_brightness_worker(&shared, &running, &queue, true)
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
fn live_brightness_worker_exits_when_stopped_before_connect() {
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(false));
    let queue = BrightnessCommandQueue::new();

    // No D-Bus is touched: the loop checks the stop flag before connecting.
    let handle = screen_manager::daemon::spawn_brightness_worker(&shared, &running, &queue, false)
        .expect("spawn brightness worker");
    handle.join().expect("brightness worker panicked");
}

#[test]
fn join_brightness_worker_handles_spawn_failure_and_rethrows_panic() {
    screen_manager::daemon::join_brightness_worker(Err(io::Error::other("spawn denied")));
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
