//! Integration tests for the central core appliance daemon.

#[cfg(feature = "ui")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "ui")]
use screen_manager::daemon::{
    BT_COMMAND_SETTLE, BT_KEEP_ALIVE, BT_PAIRING_TIMEOUT, BT_PLAYING_POLL, BtMonitorState,
    BtSharedUpdate, COVER_DECODE_RETRY, CoverState, SettleKind, SettleWindow, ShellInit,
    TARGET_COVER_SIZE, decode_cover_buffer, finish_boot_worker, init_shell, publish_playback_state,
    report_display, run_app, run_ui_display,
};
use screen_manager::daemon::{
    CoreCliOptions, ScreenTarget, headless_requested, parse_cli_args_from, parse_preview_secs,
    parse_tab_arg, print_usage,
};
#[cfg(feature = "ui")]
use screen_manager::ui::Framebuffer;
#[cfg(feature = "ui")]
use screen_manager::ui::runtime::{RuntimeOptions, run as runtime_run};
use speaker_common::diag::CliParse;
#[cfg(feature = "ui")]
use speaker_common::diag::ScopedTempDir;
#[cfg(feature = "ui")]
use std::time::{Duration, Instant};

#[cfg(feature = "ui")]
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Polls `condition` until it holds or the deadline expires.
///
/// Used to await a worker-thread side effect (a published state delta) without
/// sleeping for a fixed, flaky duration.
#[cfg(feature = "ui")]
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not met within the 5 s deadline");
}

#[cfg(feature = "ui")]
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn parse_preview_secs_rejects_sign_prefixes_and_non_numeric() {
    assert_eq!(parse_preview_secs("0").unwrap(), 0);
    assert_eq!(parse_preview_secs("42").unwrap(), 42);
    assert!(
        parse_preview_secs("+5")
            .unwrap_err()
            .contains("sign prefixes")
    );
    assert!(
        parse_preview_secs("-1")
            .unwrap_err()
            .contains("sign prefixes")
    );
    assert!(
        parse_preview_secs("abc")
            .unwrap_err()
            .contains("invalid preview seconds")
    );
    assert!(
        parse_preview_secs("")
            .unwrap_err()
            .contains("invalid preview seconds")
    );
}

#[test]
fn parse_cli_args_config_flag() {
    let _guard = lock_env();
    let parsed = parse_cli_args_from(["--config".into(), "/tmp/custom.json".into()]).unwrap();
    let CliParse::Options(opts) = parsed else {
        panic!("expected options");
    };
    assert_eq!(
        opts.config.as_deref(),
        Some(std::path::Path::new("/tmp/custom.json"))
    );
    let parsed = parse_cli_args_from(["-c".into(), "cfg.json".into()]).unwrap();
    let CliParse::Options(opts) = parsed else {
        panic!("expected options");
    };
    assert_eq!(
        opts.config.as_deref(),
        Some(std::path::Path::new("cfg.json"))
    );
    let err = parse_cli_args_from(["-c".into()]).unwrap_err();
    assert!(err.contains("missing value"));
    assert!(!err.contains("Backtrace") && !err.contains("panicked"));
    assert!(
        parse_cli_args_from(["--marquee-fps".into(), "30".into()])
            .unwrap_err()
            .contains("unknown option")
    );
    assert!(
        parse_cli_args_from(["--theme".into(), "peppy".into()])
            .unwrap_err()
            .contains("unknown option")
    );
}

#[test]
fn parse_tab_arg_valid_and_invalid() {
    assert_eq!(parse_tab_arg("0").unwrap(), 0);
    assert_eq!(parse_tab_arg("player").unwrap(), 0);
    assert_eq!(parse_tab_arg("now-playing").unwrap(), 0);
    assert_eq!(parse_tab_arg("nowplaying").unwrap(), 0);

    assert_eq!(parse_tab_arg("1").unwrap(), 1);
    assert_eq!(parse_tab_arg("vu").unwrap(), 1);
    assert_eq!(parse_tab_arg("vumeter").unwrap(), 1);
    assert_eq!(parse_tab_arg("meter").unwrap(), 1);
    assert_eq!(parse_tab_arg("peppy").unwrap(), 1);
    assert_eq!(parse_tab_arg("peepy").unwrap(), 1);
    assert_eq!(parse_tab_arg("peppymeter").unwrap(), 1);

    assert_eq!(parse_tab_arg("2").unwrap(), 2);
    assert_eq!(parse_tab_arg("bt").unwrap(), 2);
    assert_eq!(parse_tab_arg("bluetooth").unwrap(), 2);

    assert_eq!(parse_tab_arg("3").unwrap(), 3);
    assert_eq!(parse_tab_arg("cd").unwrap(), 3);
    assert_eq!(parse_tab_arg("optical").unwrap(), 3);

    assert_eq!(parse_tab_arg("4").unwrap(), 4);
    assert_eq!(parse_tab_arg("dsp").unwrap(), 4);
    assert_eq!(parse_tab_arg("tone").unwrap(), 4);
    assert_eq!(parse_tab_arg("camilla").unwrap(), 4);

    assert_eq!(parse_tab_arg("5").unwrap(), 5);
    assert_eq!(parse_tab_arg("settings").unwrap(), 5);
    assert_eq!(parse_tab_arg("prefs").unwrap(), 5);
    assert_eq!(parse_tab_arg("preferences").unwrap(), 5);
    assert_eq!(parse_tab_arg("theme").unwrap(), 5);

    assert!(parse_tab_arg("6").unwrap_err().contains("invalid tab"));
    assert!(
        parse_tab_arg("invalid")
            .unwrap_err()
            .contains("invalid tab")
    );
}

#[cfg(feature = "ui")]
#[test]
fn headless_requested_requires_explicit_one() {
    let _guard = lock_env();
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // SPEAKER_CORE_HEADLESS while this guard is held.
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
    assert!(!headless_requested());
    // SAFETY: same serialized-guard invariant as above.
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "1") };
    assert!(headless_requested());
    // SAFETY: same serialized-guard invariant as above.
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "0") };
    assert!(
        !headless_requested(),
        "any value other than '1' must keep the display enabled"
    );
    // SAFETY: same serialized-guard invariant as above.
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}

#[cfg(feature = "ui")]
#[test]
fn run_ui_display_headless_short_circuits() {
    let _guard = lock_env();
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // SPEAKER_CORE_HEADLESS while this guard is held.
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "1") };
    // The headless guard in run_ui_display must return before any framebuffer/sysfs access.
    run_ui_display(ScreenTarget::Appliance, Some(1), None, None, 60);
    run_ui_display(ScreenTarget::Splash, Some(1), None, None, 60);
    run_ui_display(ScreenTarget::Shell, Some(1), None, None, 60);
    // SAFETY: same serialized-guard invariant as above.
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}

#[cfg(feature = "ui")]
#[test]
fn run_ui_display_returns_early_without_drm_device() {
    let _guard = lock_env();
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // SPEAKER_CORE_HEADLESS while this guard is held.
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
    if std::path::Path::new("/dev/dri/card0").exists() {
        // Never probe the KMS path on a host with a live DRM device: it would
        // attempt to become DRM master. CI runners have no /dev/dri.
        return;
    }
    run_ui_display(ScreenTarget::Appliance, Some(1), None, None, 60);
    run_ui_display(ScreenTarget::Splash, Some(1), None, None, 60);
    run_ui_display(ScreenTarget::Shell, Some(1), None, None, 60);
}

#[cfg(feature = "ui")]
fn instant_probes(text: Result<&'static str, &'static str>) -> screen_manager::ui::BootProbe {
    use screen_manager::ui::BootProbe;

    BootProbe::from_probes(std::array::from_fn(|_| {
        Box::new(move || text.map(Into::into).map_err(Into::into))
            as Box<dyn FnOnce() -> Result<Box<str>, Box<str>> + Send>
    }))
}

#[cfg(feature = "ui")]
#[test]
fn runtime_splash_drains_checks_and_quits() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();

    // A short-lived timer proves the hermetic event loop advanced timers
    // before the boot-check settle quit ended the session.
    let ticked = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = std::rc::Rc::clone(&ticked);
    slint::Timer::single_shot(Duration::from_millis(5), move || flag.set(true));

    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Splash,
        preview_secs: None,
        tab: None,
        config: &CoreConfig::default(),
        probe: instant_probes(Ok("all good")),
        mocked: true,
    });
    assert!(
        ticked.get(),
        "splash event loop must advance timers before quitting"
    );
}

#[cfg(feature = "ui")]
#[test]
fn runtime_splash_failed_probes_still_terminate() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Splash,
        preview_secs: None,
        tab: None,
        config: &CoreConfig::default(),
        probe: instant_probes(Err("probe blew up")),
        mocked: true,
    });
}

#[cfg(feature = "ui")]
#[test]
fn runtime_splash_zero_preview_exits_after_checks() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Splash,
        preview_secs: Some(0),
        tab: None,
        config: &CoreConfig::default(),
        probe: instant_probes(Ok("ok")),
        mocked: true,
    });
}

#[cfg(feature = "ui")]
#[test]
fn runtime_shell_runs_timers_and_shuts_down() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Shell,
        preview_secs: Some(1),
        tab: Some(1),
        config: &CoreConfig {
            theme: Some(screen_manager::ui::ThemePreset::Peppy),
            marquee_fps: screen_manager::config::MarqueeFps::FPS_30,
            ..CoreConfig::default()
        },
        probe: instant_probes(Ok("unused")),
        mocked: true,
    });
}

#[cfg(feature = "ui")]
#[test]
fn runtime_shell_zero_preview_returns_immediately() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Shell,
        preview_secs: Some(0),
        tab: None,
        config: &CoreConfig::default(),
        probe: instant_probes(Ok("unused")),
        mocked: true,
    });
}

#[cfg(feature = "ui")]
#[test]
fn runtime_appliance_settles_boot_overlay_then_runs_shell() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    let opts = RuntimeOptions {
        screen: ScreenTarget::Appliance,
        preview_secs: Some(1),
        tab: Some(2),
        config: &CoreConfig::default(),
        probe: instant_probes(Ok("all good")),
        mocked: true,
    };
    // Exercise the manual Debug impl (BootProbe itself has no Debug derive).
    assert!(format!("{opts:?}").contains("RuntimeOptions"));
    runtime_run(opts);
}

#[cfg(feature = "ui")]
#[test]
fn runtime_appliance_abandons_hung_boot_probe_on_preview_expiry() {
    use screen_manager::config::CoreConfig;
    use screen_manager::ui::BootProbe;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    // A probe that outlives the preview window forces the deadline branch:
    // the boot phase ends unsettled and the worker is abandoned with a warning.
    let hung = BootProbe::from_probes(std::array::from_fn(|_| {
        Box::new(move || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok::<Box<str>, Box<str>>("late".into())
        }) as Box<dyn FnOnce() -> Result<Box<str>, Box<str>> + Send>
    }));
    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Appliance,
        preview_secs: Some(1),
        tab: None,
        config: &CoreConfig::default(),
        probe: hung,
        mocked: true,
    });
}

#[cfg(feature = "ui")]
#[test]
fn runtime_appliance_zero_preview_abandons_boot_before_first_tick() {
    use screen_manager::config::CoreConfig;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    // The preview single-shot (0 s) fires before the 16 ms drain timer ever
    // runs, so the post-loop abandon path must release the worker handle.
    runtime_run(RuntimeOptions {
        screen: ScreenTarget::Appliance,
        preview_secs: Some(0),
        tab: None,
        config: &CoreConfig::default(),
        probe: instant_probes(Ok("ok")),
        mocked: true,
    });
}

#[test]
fn parse_cli_args_flags_and_values() {
    let parsed = |args: &[&str]| {
        parse_cli_args_from(args.iter().map(ToString::to_string).collect::<Vec<_>>())
    };

    match parsed(&[]).unwrap() {
        CliParse::Options(o) => {
            assert!(o.preview_secs.is_none());
            assert_eq!(o.screen, ScreenTarget::Appliance);
            assert!(o.config.is_none());
            assert!(o.tab.is_none());
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-p", "5", "--screen", "shell"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.preview_secs, Some(5));
            assert_eq!(o.screen, ScreenTarget::Shell);
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-s", "appliance"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.screen, ScreenTarget::Appliance);
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-s", "auto"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.screen, ScreenTarget::Appliance);
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-s", "full"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.screen, ScreenTarget::Appliance);
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-s", "main"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.screen, ScreenTarget::Shell);
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-c", "override.json", "-T", "vu"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(
                o.config.as_deref(),
                Some(std::path::Path::new("override.json"))
            );
            assert_eq!(o.tab, Some(1));
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["--tab", "bt"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.tab, Some(2));
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["--tab", "cd"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.tab, Some(3));
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["--tab", "dsp"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.tab, Some(4));
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-s", "splash"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.screen, ScreenTarget::Splash);
        }
        other => panic!("expected Options, got {other:?}"),
    }

    assert!(matches!(parsed(&["-h"]).unwrap(), CliParse::Help));
    assert!(matches!(parsed(&["--version"]).unwrap(), CliParse::Version));

    for args in [
        &["-p"][..],
        &["-p", "+5"][..],
        &["-p", "nan"][..],
        &["-s"][..],
        &["-s", "invalid"][..],
        &["-t"][..],
        &["-t", "unknown"][..],
        &["-T"][..],
        &["-T", "invalid"][..],
        &["-x"][..],
        &["-l"][..],
        &["--loop"][..],
        &["--preview", "1", "extra"][..],
    ] {
        assert!(parsed(args).is_err(), "expected rejection for {args:?}");
    }
}

#[cfg(feature = "ui")]
#[test]
fn run_app_headless_executes_successfully() {
    use screen_manager::config::CoreConfig;

    let _guard = lock_env();
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // SPEAKER_CORE_HEADLESS while this guard is held.
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "1") };

    // Absent config paths keep the test hermetic on any host (no reliance on
    // the real /etc/speaker-core/config.json) while exercising the
    // missing-file defaults branch.
    let dir = ScopedTempDir::new("run_app_cfg");
    let absent = dir.path().join("absent.json");

    let code = run_app(&CoreCliOptions {
        config: Some(absent),
        preview_secs: Some(1),
        screen: ScreenTarget::Splash,
        tab: None,
    });
    assert_eq!(code, std::process::ExitCode::SUCCESS);

    // A config file with a theme override and reduced marquee rate drives the UI.
    let cfg_path = dir.path().join("config.json");
    CoreConfig {
        theme: Some(screen_manager::ui::ThemePreset::Peppy),
        marquee_fps: screen_manager::config::MarqueeFps::FPS_30,
        ..CoreConfig::default()
    }
    .save(&cfg_path)
    .expect("save config");

    let code_appliance = run_app(&CoreCliOptions {
        config: Some(cfg_path),
        preview_secs: Some(1),
        screen: ScreenTarget::Appliance,
        tab: Some(1),
    });
    assert_eq!(code_appliance, std::process::ExitCode::SUCCESS);

    let code_shell = run_app(&CoreCliOptions {
        config: Some(dir.path().join("defaults-shell.json")),
        preview_secs: Some(1),
        screen: ScreenTarget::Shell,
        tab: Some(1),
    });
    assert_eq!(code_shell, std::process::ExitCode::SUCCESS);

    // A malformed config must fail the daemon with a hard error.
    let bad_path = dir.path().join("bad.json");
    std::fs::write(&bad_path, "{not json").expect("write malformed config");
    let code_bad = run_app(&CoreCliOptions {
        config: Some(bad_path),
        preview_secs: Some(1),
        screen: ScreenTarget::Shell,
        tab: None,
    });
    assert_eq!(code_bad, std::process::ExitCode::FAILURE);

    // SAFETY: same serialized-guard invariant as above.
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}

#[cfg(feature = "ui")]
#[test]
fn report_display_logs_all_levels() {
    report_display(
        tracing::Level::INFO,
        "Info test context",
        "info error message",
    );
    report_display(
        tracing::Level::WARN,
        "Warn test context",
        "warn error message",
    );
    report_display(
        tracing::Level::ERROR,
        "Error test context",
        "error error message",
    );
}

#[test]
fn print_usage_executes_without_panic() {
    print_usage("core");
}

#[cfg(feature = "ui")]
#[test]
fn run_ui_display_with_headless_disabled_checks_device() {
    let _guard = lock_env();
    // SAFETY: serialized test guard
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "0") };

    assert!(
        !headless_requested(),
        "SPEAKER_CORE_HEADLESS=0 must not enable headless mode"
    );

    if std::path::Path::new("/dev/dri/card0").exists() {
        // Never probe the KMS path on a host with a live DRM device: it would
        // attempt to become DRM master. CI runners have no /dev/dri.
        // SAFETY: serialized test guard
        unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
        return;
    }
    run_ui_display(ScreenTarget::Splash, Some(0), None, None, 60);
    run_ui_display(ScreenTarget::Shell, Some(0), None, None, 60);
    run_ui_display(ScreenTarget::Appliance, Some(0), None, None, 60);

    // SAFETY: serialized test guard
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}

#[cfg(feature = "ui")]
#[test]
fn test_core_daemon_main_entrypoint() {
    let _guard = lock_env();
    // SAFETY: serialized test guard
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "1") };
    assert_eq!(
        screen_manager::daemon::main(),
        std::process::ExitCode::SUCCESS
    );
    // SAFETY: serialized test guard
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}

#[cfg(feature = "ui")]
#[test]
fn test_core_daemon_run_ui_display_unheadless_and_init_shell_direct() {
    use screen_manager::ui::{Framebuffer, MainWindow};

    // 1. run_ui_display with headless disabled reaches the DRM device probe
    let _guard = lock_env();
    // SAFETY: serialized test guard
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "0") };
    if !std::path::Path::new("/dev/dri/card0").exists() {
        // Early-return path: safe to probe on hosts without a DRM device.
        run_ui_display(ScreenTarget::Splash, Some(0), None, None, 60);
        run_ui_display(ScreenTarget::Shell, Some(0), None, None, 60);
        run_ui_display(ScreenTarget::Appliance, Some(0), None, None, 60);
    }
    // SAFETY: serialized test guard
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };

    // 2. init_shell with mocked_device = false exercises production Bluetooth connection path
    let (fb, _) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    let main_window = MainWindow::new().unwrap();
    let ShellInit { shell, .. } = init_shell(
        main_window,
        false,
        Some(screen_manager::ui::ThemePreset::Peppy),
        Some(1),
        60,
    );
    assert!(!shell.is_playing());
    assert_eq!(
        shell.current_theme(),
        screen_manager::ui::ThemePreset::Peppy
    );
    assert_eq!(shell.current_tab(), 1);

    // 3. init_shell with mocked_device = true exercises mocked Bluetooth path and playback sync
    let main_window_mock = MainWindow::new().unwrap();
    let ShellInit {
        shell: shell_mock,
        bt: bt_mock,
        ..
    } = init_shell(
        main_window_mock,
        true,
        Some(screen_manager::ui::ThemePreset::CyberpunkNeon),
        Some(2),
        30,
    );
    assert!(bt_mock.is_some());
    assert!(shell_mock.is_playing());
    // Tab 2 is retired (plan A6): `--tab bt|bluetooth|2` opens the full-screen
    // devices view instead of selecting a dock tab, so the active tab stays 0.
    assert_eq!(shell_mock.current_tab(), 0);
    assert!(shell_mock.window().get_bt_devices_open());
}

#[cfg(feature = "ui")]
#[test]
fn live_init_defers_bluez_connect_to_bootstrap() {
    use screen_manager::ui::MainWindow;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();

    // Live init must not touch the system bus: the blocking BlueZ connect
    // runs on the `bt-bootstrap` worker, so `bt` stays `None` here and first
    // paint can never be delayed by a slow D-Bus.
    let main_window = MainWindow::new().unwrap();
    let ShellInit { bt, .. } = init_shell(main_window, false, None, None, 60);
    assert!(
        bt.is_none(),
        "live init must defer the blocking BlueZ connect"
    );
}

#[cfg(feature = "ui")]
#[test]
fn bootstrap_worker_publishes_state_without_touching_the_ui_thread() {
    use screen_manager::ui::MainWindow;
    use std::sync::atomic::AtomicBool;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();

    let main_window = MainWindow::new().unwrap();
    let ShellInit {
        shell, commands, ..
    } = init_shell(main_window, false, None, None, 60);
    let shell = std::rc::Rc::new(shell);
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));
    let connected = Arc::new(AtomicBool::new(false));

    // The injected mock connection stands in for the blocking system-bus
    // connect: the worker runs it off the render thread and publishes the
    // resulting state through the shared update channel.
    let (mock_bt, _mock_ctrl) = speaker_bluetooth::BluetoothDevice::new_mocked();
    let worker = screen_manager::daemon::spawn_bt_bootstrap_worker(
        || Ok(mock_bt),
        &shared,
        &commands,
        &speaker_bluetooth::DeviceCommandQueue::new(),
        &running,
        &connected,
    )
    .expect("bt-bootstrap thread spawns");

    // The connect succeeded and the monitor published the initial snapshot.
    wait_for(|| connected.load(std::sync::atomic::Ordering::Relaxed));
    wait_for(|| shared.lock().is_ok_and(|state| state.playback.is_some()));

    // The render thread applies the published state to the shell.
    screen_manager::daemon::apply_shared_bt_state(&shell, &shared);
    assert!(shell.is_playing(), "published playback state applied");
    assert!(connected.load(std::sync::atomic::Ordering::Relaxed));

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    worker.join().expect("bt-bootstrap worker exits cleanly");
}

#[cfg(feature = "ui")]
#[test]
fn bootstrap_worker_reports_connect_failure_without_publishing_state() {
    use screen_manager::ui::MainWindow;
    use std::sync::atomic::AtomicBool;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();

    let main_window = MainWindow::new().unwrap();
    let ShellInit { commands, .. } = init_shell(main_window, false, None, None, 60);
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let running = Arc::new(AtomicBool::new(true));
    let connected = Arc::new(AtomicBool::new(false));

    // A failed connection is reported at WARN on the worker thread: no state
    // is published and the connected flag stays down, so the UI never wires
    // the transport callbacks.
    let worker = screen_manager::daemon::spawn_bt_bootstrap_worker(
        || Err(speaker_bluetooth::BtError::dbus("no system bus")),
        &shared,
        &commands,
        &speaker_bluetooth::DeviceCommandQueue::new(),
        &running,
        &connected,
    )
    .expect("bt-bootstrap thread spawns");
    worker.join().expect("bt-bootstrap worker exits cleanly");

    assert!(
        !connected.load(std::sync::atomic::Ordering::Relaxed),
        "failed connect must not raise the connected flag"
    );
    let state = shared.lock().expect("shared state lock");
    assert!(
        state.playback.is_none()
            && state.pcm.is_none()
            && state.cover.is_none()
            && state.amp_volume.is_none()
            && state.devices.is_none(),
        "failed connect must not publish any state"
    );
}

#[cfg(feature = "ui")]
fn sample_png(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    image::DynamicImage::new_rgb8(64, 64)
        .save_with_format(&path, image::ImageFormat::Png)
        .expect("sample png write");
    path
}

#[cfg(feature = "ui")]
fn playback(cover: Option<&std::path::Path>, playing: bool) -> speaker_bluetooth::BtPlaybackState {
    speaker_bluetooth::BtPlaybackState {
        cover_art_path: cover.map(|p| p.display().to_string().into_boxed_str()),
        status: if playing {
            "playing".into()
        } else {
            "paused".into()
        },
        ..speaker_bluetooth::BtPlaybackState::default()
    }
}

#[cfg(feature = "ui")]
#[test]
fn decode_cover_buffer_upscales_and_caches() {
    let dir = ScopedTempDir::try_new("cover_decode_up").expect("temp dir");
    let src = sample_png(dir.path(), "art.png");
    let buffer = decode_cover_buffer(src.to_str().expect("utf8")).expect("decoded");
    assert_eq!(buffer.width(), TARGET_COVER_SIZE);
    let cache = dir.path().join("art.png.480.jpg");
    assert!(cache.is_file(), "cache file should be written");
    // Second call serves from the cache and still decodes.
    assert!(decode_cover_buffer(src.to_str().expect("utf8")).is_some());
    let _ = buffer;
}

#[cfg(feature = "ui")]
#[test]
fn decode_cover_buffer_missing_file_is_none() {
    assert!(decode_cover_buffer("/nonexistent/cover/art.png").is_none());
}

#[cfg(feature = "ui")]
#[test]
fn decode_cover_buffer_regenerates_corrupt_cache() {
    let dir = ScopedTempDir::try_new("cover_decode_corrupt").expect("temp dir");
    let src = sample_png(dir.path(), "art.png");
    let cache = dir.path().join("art.png.480.jpg");
    std::fs::write(&cache, b"truncated-jpeg-garbage").expect("cache write");
    assert!(
        decode_cover_buffer(src.to_str().expect("utf8")).is_some(),
        "corrupt cache must be regenerated from the source"
    );
    assert!(image::open(&cache).is_ok(), "cache must be valid again");
}

#[cfg(feature = "ui")]
#[test]
fn settle_window_suppresses_stale_seek_position() {
    // A snapshot still carrying the pre-seek AVRCP position (far from the
    // scrub target) is suppressed so the optimistic scrubber is not
    // snapped back; once the peer reports near the target it publishes.
    let fresh = Instant::now();
    let stale = SettleWindow {
        kind: SettleKind::Seek { target_ms: 120_000 },
        started: fresh,
    };
    let mut far = playback(None, true);
    far.position_ms = 1_000;
    assert!(stale.suppresses(&far), "pre-seek position is stale");
    let mut near = playback(None, true);
    near.position_ms = 120_400;
    assert!(!stale.suppresses(&near), "caught-up position publishes");
}

#[cfg(feature = "ui")]
#[test]
fn settle_window_releases_on_expiry() {
    let mut pb = playback(None, true);
    pb.position_ms = 1_000;
    let mut w = SettleWindow {
        kind: SettleKind::Seek { target_ms: 120_000 },
        started: Instant::now(),
    };
    assert!(w.suppresses(&pb));
    w.started = Instant::now()
        .checked_sub(BT_COMMAND_SETTLE + Duration::from_secs(1))
        .expect("monotonic instant before start");
    assert!(!w.suppresses(&pb), "expired window never suppresses");
}

#[cfg(feature = "ui")]
#[test]
fn publish_suppresses_matching_settle_snapshot() {
    let (bt, mock) = speaker_bluetooth::BluetoothDevice::new_mocked();
    // The mocked snapshot must differ from `last_pb` (advanced position,
    // exactly what happens while playing) while `is_playing` still matches
    // `pre_playing`; otherwise the test passes even with suppression deleted.
    let mut ticking = playback(None, true);
    ticking.position_ms = 1000;
    mock.set_playback(Some(ticking));
    let mut update = BtSharedUpdate::default();
    let mut last_pb = Some(playback(None, true));
    let mut cover = CoverState::default();
    let mut settle = Some(SettleWindow {
        kind: SettleKind::Toggle { pre_playing: true },
        started: Instant::now(),
    });
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);
    assert!(update.playback.is_none(), "settling must suppress publish");
    assert!(settle.is_some(), "settle window stays armed during window");
}

#[cfg(feature = "ui")]
#[test]
fn publish_after_settle_expiry_publishes() {
    let (bt, mock) = speaker_bluetooth::BluetoothDevice::new_mocked();
    mock.set_playback(Some(playback(None, true)));
    let started = Instant::now()
        .checked_sub(BT_COMMAND_SETTLE + Duration::from_secs(1))
        .expect("monotonic instant before start");
    let mut settle = Some(SettleWindow {
        kind: SettleKind::Toggle { pre_playing: true },
        started,
    });
    let mut last_pb: Option<speaker_bluetooth::BtPlaybackState> = None;
    let mut update = BtSharedUpdate::default();
    let mut cover = CoverState::default();
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);
    assert!(update.playback.is_some(), "expiry must force a publish");
    assert!(settle.is_none(), "window must be disarmed after expiry");
}

#[cfg(feature = "ui")]
#[test]
fn publish_failed_cover_retry_after_backoff() {
    let (bt, mock) = speaker_bluetooth::BluetoothDevice::new_mocked();
    let missing = "/nonexistent/cover/missing.png";
    let pb = speaker_bluetooth::BtPlaybackState {
        cover_art_path: Some(missing.into()),
        ..speaker_bluetooth::BtPlaybackState::default()
    };
    mock.set_playback(Some(pb.clone()));
    let mut last_pb: Option<speaker_bluetooth::BtPlaybackState> = None;
    let mut cover = CoverState::default();
    let mut settle = None;
    let mut update = BtSharedUpdate::default();
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);
    assert_eq!(cover.path.as_deref(), Some(missing));
    assert!(cover.failed_since.is_some());
    assert!(update.cover.is_none(), "failed decode keeps prior art");
    // Rewind the failure timestamp past the backoff and confirm the retry
    // is attempted (and re-fails, since the path does not exist).
    cover.failed_since = Some(
        Instant::now()
            .checked_sub(COVER_DECODE_RETRY + Duration::from_secs(1))
            .expect("monotonic instant before start"),
    );
    update = BtSharedUpdate::default();
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);
    assert!(
        cover.failed_since.is_some(),
        "retry must be re-attempted after the backoff"
    );
}

#[cfg(feature = "ui")]
#[test]
fn publish_device_gone_clears_cover() {
    let (bt, mock) = speaker_bluetooth::BluetoothDevice::new_mocked();
    // The mock initializes with a default playback present; clear it so
    // `playback_state()` really returns `Ok(None)` and the disconnect
    // placeholder branch is exercised.
    mock.set_playback(None);
    let mut last_pb = Some(playback(None, true));
    let mut cover = CoverState {
        path: Some("/gone.png".into()),
        failed_since: None,
    };
    let mut update = BtSharedUpdate::default();
    let mut settle = None;
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);
    assert!(
        update.playback.is_some(),
        "placeholder must publish on Ok(None)"
    );
    assert!(
        update.cover.is_some_and(|c| c.is_none()),
        "cover must clear on disconnect"
    );
    assert!(cover.path.is_none());
}

#[cfg(feature = "ui")]
#[test]
fn poll_encoder_input_applies_rotation_and_faults_gracefully() {
    use screen_manager::daemon::poll_encoder_input;
    use screen_manager::ui::{AppShell, EncoderVolumeTracker, MainWindow};
    use slint::ComponentHandle;
    use speaker_hardware::EncoderDevice;

    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    let main_window = MainWindow::new().unwrap();
    let shell = AppShell::new(main_window);
    shell.set_volume(50.0);
    shell.window().show().unwrap();

    let (enc, enc_mock) = EncoderDevice::new_mocked();
    enc_mock.simulate_rotation(speaker_hardware::RotationDirection::Clockwise, 2);
    // Encoder volume is executed by the internal amp-volume worker (mock
    // client because the session is mocked); wire the same queue it receives.
    let amp_commands = screen_manager::volume::AmpVolumeCommandQueue::new();
    shell.wire_amp_volume(&amp_commands);

    let mut tracker = EncoderVolumeTracker::new();
    let mut warned = false;
    poll_encoder_input(Some(&enc), &mut tracker, &shell, &mut warned);
    assert!((shell.volume() - 54.0).abs() < 1e-4);
    assert!(shell.is_volume_hud_visible());
    assert!(!warned, "successful polls must not latch the warning flag");

    // A missing encoder device is a silent no-op (early return path).
    poll_encoder_input(None, &mut tracker, &shell, &mut warned);
    assert!((shell.volume() - 54.0).abs() < 1e-4);
}

#[cfg(feature = "ui")]
#[test]
fn bt_monitor_state_commands_settle_and_commit() {
    use speaker_bluetooth::{BluetoothDevice, TransportCommand, TransportCommandQueue};

    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let commands = TransportCommandQueue::new();
    let device_commands = speaker_bluetooth::DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();

    // Initial tick populates last_pb, last_pcms, and shared state.
    assert!(state.poll_tick(
        &bt,
        &shared,
        &commands,
        &device_commands,
        BT_PAIRING_TIMEOUT
    ));
    assert!(state.has_playback());
    assert_eq!(state.keep_alive_interval(), BT_PLAYING_POLL);
    assert!(shared.lock().unwrap().playback.is_some());
    assert!(shared.lock().unwrap().pcm.is_some());

    // Arm seek settle window, then toggle settle window, and exercise all commands.
    commands.push(TransportCommand::Seek(45_000));
    assert!(state.poll_tick(
        &bt,
        &shared,
        &commands,
        &device_commands,
        BT_PAIRING_TIMEOUT
    ));
    assert!(matches!(
        state.settle().map(|s| &s.kind),
        Some(SettleKind::Seek { target_ms: 45_000 })
    ));

    commands.push(TransportCommand::TogglePlayPause);
    commands.push(TransportCommand::NextTrack);
    commands.push(TransportCommand::PreviousTrack);
    assert!(state.poll_tick(
        &bt,
        &shared,
        &commands,
        &device_commands,
        BT_PAIRING_TIMEOUT
    ));
    assert!(matches!(
        state.settle().map(|s| &s.kind),
        Some(SettleKind::Toggle { .. })
    ));
    assert_eq!(state.keep_alive_interval(), BT_KEEP_ALIVE);

    // (Volume/mute commands no longer exist on the Bluetooth transport: the
    // screen routes them to the dedicated `amp-volume` worker instead, which
    // is covered by the amp-volume integration tests.)

    // Fault injection on transport commands logs warning without panicking.
    mock.set_fault(Some("dbus transport fault".into()));
    commands.push(TransportCommand::NextTrack);
    assert!(state.poll_tick(
        &bt,
        &shared,
        &commands,
        &device_commands,
        BT_PAIRING_TIMEOUT
    ));
    mock.set_fault(None);

    // Reset dedup helper clears cached snapshots.
    state.reset_dedup();
    assert!(!state.has_playback());
    assert!(!state.has_pcms());
    assert!(state.cover_path().is_none());

    // Poisoned mutex recovery in commit_update.
    let poisoned = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let p_clone = Arc::clone(&poisoned);
    let _ = std::thread::spawn(move || {
        let _guard = p_clone.lock().unwrap();
        panic!("intentional poison");
    })
    .join();
    BtMonitorState::commit_update(
        &poisoned,
        BtSharedUpdate {
            playback: Some(playback(None, false)),
            pcm: None,
            cover: Some(None),
            amp_volume: None,
            devices: None,
        },
    );
    let recovered = poisoned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(recovered.playback.is_some());
    assert!(recovered.cover.is_some());
}

#[cfg(feature = "ui")]
#[test]
fn publish_link_state_stages_pcm_once_and_resets() {
    use speaker_bluetooth::BluetoothDevice;

    let (bt, _mock) = BluetoothDevice::new_mocked();
    let mut state = BtMonitorState::default();

    let mut first = BtSharedUpdate::default();
    let _ = state.publish_link_state(&bt, &mut first);
    assert!(first.pcm.is_some());
    let details = state.last_details().expect("first tick caches details");
    assert!(details.device.is_some());
    assert!(details.pcm.is_some());

    // Unchanged snapshot is deduplicated.
    let mut second = BtSharedUpdate::default();
    let _ = state.publish_link_state(&bt, &mut second);
    assert!(second.pcm.is_none());
    assert!(state.last_details().is_some());

    // A dedup reset forces a full republish.
    state.reset_dedup();
    assert!(state.last_details().is_none());
    let mut third = BtSharedUpdate::default();
    let _ = state.publish_link_state(&bt, &mut third);
    assert!(third.pcm.is_some());
    assert!(state.last_details().is_some());
}

#[cfg(feature = "ui")]
#[test]
fn publish_link_state_keeps_state_on_query_fault() {
    use speaker_bluetooth::BluetoothDevice;

    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.set_fault(Some("link fault".into()));
    let mut state = BtMonitorState::default();
    let mut update = BtSharedUpdate::default();
    assert!(state.publish_link_state(&bt, &mut update).is_none());
    assert!(state.last_details().is_none());
}

#[cfg(feature = "ui")]
#[test]
fn publish_link_state_clears_details_on_disconnect() {
    use speaker_bluetooth::BluetoothDevice;

    let (bt, mock) = BluetoothDevice::new_mocked();
    let mut state = BtMonitorState::default();
    let _ = state.publish_link_state(&bt, &mut BtSharedUpdate::default());

    mock.clear_devices();
    mock.clear_pcms();
    let _ = state.publish_link_state(&bt, &mut BtSharedUpdate::default());
    let details = state
        .last_details()
        .expect("disconnect republishes details");
    assert!(details.device.is_none());
    assert!(details.pcm.is_none());
}

#[cfg(feature = "ui")]
#[test]
fn finish_boot_worker_abandons_or_joins() {
    // Hung / unfinished worker path (`checks_done == false`).
    finish_boot_worker(None, false);

    // Normal worker completion path (`checks_done == true`).
    let ok_handle = std::thread::spawn(|| {});
    finish_boot_worker(Some(ok_handle), true);

    // Worker panic propagation path (`M-PANIC-ON-BUG`).
    let panic_handle = std::thread::spawn(|| panic!("boot worker panic"));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        finish_boot_worker(Some(panic_handle), true);
    }));
    assert!(res.is_err(), "worker panic must be re-raised");
}
