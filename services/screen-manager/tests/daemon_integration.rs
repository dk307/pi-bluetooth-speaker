//! Integration tests for the central core appliance daemon.

#[cfg(feature = "ui")]
use std::fs;
#[cfg(feature = "ui")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "ui")]
use screen_manager::daemon::{
    BT_COMMAND_SETTLE, BT_KEEP_ALIVE, BT_PLAYING_POLL, BtMonitorState, BtSharedUpdate,
    COVER_DECODE_RETRY, CoverState, SettleKind, SettleWindow, ShellInit, TARGET_COVER_SIZE,
    decode_cover_buffer, finish_boot_worker, idle_sleep, init_shell, publish_playback_state,
    report_display, run_app, run_render_loop, run_shell_on, run_splash_on, run_ui_display,
    run_ui_display_on_path, run_ui_lifecycle,
};
use screen_manager::daemon::{
    CoreCliOptions, ScreenTarget, headless_requested, parse_cli_args_from, parse_preview_secs,
    parse_tab_arg, print_usage,
};
#[cfg(feature = "ui")]
use screen_manager::ui::Framebuffer;
use speaker_common::diag::CliParse;
#[cfg(feature = "ui")]
use speaker_common::diag::ScopedTempDir;
#[cfg(feature = "ui")]
use std::time::{Duration, Instant};

#[cfg(feature = "ui")]
static ENV_GUARD: Mutex<()> = Mutex::new(());

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
fn run_ui_display_missing_device_returns_early() {
    let _guard = lock_env();
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // SPEAKER_CORE_HEADLESS while this guard is held.
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
    let dir = ScopedTempDir::new("fb-boot-splash-missing");
    let missing = dir.path().join("fb0");
    run_ui_display_on_path(&missing, ScreenTarget::Appliance, Some(1), None, None, 60);
    run_ui_display_on_path(&missing, ScreenTarget::Splash, Some(1), None, None, 60);
    run_ui_display_on_path(&missing, ScreenTarget::Shell, Some(1), None, None, 60);
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
fn run_splash_on_mocked_drains_checks_and_renders_frames() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    assert!(fb.is_mocked());
    run_splash_on(&mut fb, instant_probes(Ok("all good")), None, None);
    assert_eq!(fb.unblank_count(), 1);
    assert!(
        fb.render_count() >= 2,
        "initial frame plus check-loop frames"
    );
    assert_eq!(ctrl.counters(), (1, fb.render_count()));
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_failed_probes_still_terminate() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    run_splash_on(&mut fb, instant_probes(Err("probe blew up")), None, None);
    assert!(fb.render_count() >= 2);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_zero_preview_exits_after_checks() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    run_splash_on(&mut fb, instant_probes(Ok("ok")), Some(0), None);
    assert!(fb.render_count() >= 2);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_reports_unblank_failure_and_continues() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_unblank_error("simulated unblank failure");
    run_splash_on(
        &mut fb,
        instant_probes(Ok("ok")),
        None,
        Some(screen_manager::ui::ThemePreset::Peppy),
    );
    // Unblank failure is non-fatal: splash still initializes and renders
    assert_eq!(fb.unblank_count(), 0);
    assert!(fb.render_count() >= 2);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_render_error_does_not_join_check_thread() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_render_error("simulated render failure");
    run_splash_on(&mut fb, instant_probes(Ok("ok")), None, None);
    assert_eq!(fb.render_count(), 0);
    assert_eq!(ctrl.counters(), (1, 0));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_renders_and_tracks_counters() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    assert!(fb.is_mocked());
    run_shell_on(&mut fb, Some(0), None, None, 60);
    assert_eq!(fb.unblank_count(), 1);
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_reports_unblank_failure_and_continues() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_unblank_error("simulated unblank failure");
    run_shell_on(&mut fb, Some(0), None, None, 60);
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_render_error_skips_loop() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_render_error("simulated render failure");
    run_shell_on(&mut fb, Some(0), None, None, 60);
    assert_eq!(fb.render_count(), 0);
    assert_eq!(ctrl.counters(), (1, 0));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_with_theme_and_tab() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    run_shell_on(
        &mut fb,
        Some(0),
        Some(screen_manager::ui::ThemePreset::Peppy),
        Some(1),
        30,
    );
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_render_loop_mocked_and_bluetooth_paths() {
    use screen_manager::ui::{AppShell, MainWindow};

    let (mut fb, ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().unwrap();
    let main_window = MainWindow::new().unwrap();
    let shell = AppShell::new(main_window);

    // 1. None bluetooth path breaking on render failure
    ctrl.set_render_error("simulated loop failure");
    let commands = speaker_bluetooth::TransportCommandQueue::new();
    run_render_loop(
        &mut fb,
        &window,
        &shell,
        None,
        &commands,
        &screen_manager::volume::AmpVolumeCommandQueue::new(),
        Some(10),
    );

    // 2. Bluetooth sync branch with mocked BluetoothDevice breaking on render failure
    let (bt, _) = speaker_bluetooth::BluetoothDevice::new_mocked();
    ctrl.set_render_error("simulated bt loop failure");
    let commands = speaker_bluetooth::TransportCommandQueue::new();
    run_render_loop(
        &mut fb,
        &window,
        &shell,
        Some(&bt),
        &commands,
        &screen_manager::volume::AmpVolumeCommandQueue::new(),
        Some(10),
    );

    // 3. Bluetooth sync branch running short preview loop to completion (clear fault first)
    ctrl.clear_render_error();
    let commands = speaker_bluetooth::TransportCommandQueue::new();
    run_render_loop(
        &mut fb,
        &window,
        &shell,
        Some(&bt),
        &commands,
        &screen_manager::volume::AmpVolumeCommandQueue::new(),
        Some(1),
    );

    // 4. run_shell_on with render error
    ctrl.set_render_error("simulated shell render error");
    run_shell_on(&mut fb, Some(0), None, None, 60);

    // 5. run_shell_on with unblank error
    ctrl.clear_render_error();
    ctrl.set_unblank_error("simulated unblank error");
    run_shell_on(&mut fb, Some(0), None, None, 60);
    ctrl.clear_unblank_error();
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
fn run_ui_display_on_valid_and_invalid_file_paths() {
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

    let dir = ScopedTempDir::new("fb-valid-display-test");
    let dev = dir.path().join("fake_fb");
    fs::write(&dev, vec![0u8; 800 * 480 * 4]).unwrap();

    run_ui_display_on_path(&dev, ScreenTarget::Appliance, Some(0), None, None, 60);
    run_ui_display_on_path(&dev, ScreenTarget::Splash, Some(0), None, None, 60);
    run_ui_display_on_path(
        &dev,
        ScreenTarget::Shell,
        Some(0),
        Some(screen_manager::ui::ThemePreset::Peppy),
        Some(1),
        60,
    );

    // Directory path to trigger Framebuffer::open failure path
    run_ui_display_on_path(dir.path(), ScreenTarget::Splash, Some(0), None, None, 60);
}

#[cfg(feature = "ui")]
#[test]
fn run_ui_lifecycle_error_and_preview_paths() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    // Test setup failure
    run_ui_lifecycle(&mut fb, "Failing Component", Some(0), || {
        Err::<(), Box<dyn std::error::Error>>("simulated setup failure".into())
    });

    // Test already current buffer (second render without request_redraw returns Ok(false))
    run_ui_lifecycle(&mut fb, "Already Current", Some(0), || Ok(()));

    // Test preview loop without error
    run_ui_lifecycle(&mut fb, "Active Render Loop", Some(1), || Ok(()));

    // Test preview loop break on render error
    ctrl.set_render_error("simulated render loop error");
    run_ui_lifecycle(&mut fb, "Failing Render Loop", Some(1), || Ok(()));
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

    let dir = ScopedTempDir::new("fb-headless-disabled");
    let missing = dir.path().join("fb0");
    run_ui_display_on_path(&missing, ScreenTarget::Splash, Some(0), None, None, 60);
    run_ui_display_on_path(&missing, ScreenTarget::Shell, Some(0), None, None, 60);
    run_ui_display_on_path(&missing, ScreenTarget::Appliance, Some(0), None, None, 60);

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

    // 1. run_ui_display with headless disabled calls run_ui_display_on_path
    let _guard = lock_env();
    // SAFETY: serialized test guard
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "0") };
    run_ui_display(ScreenTarget::Splash, Some(0), None, None, 60);
    run_ui_display(ScreenTarget::Shell, Some(0), None, None, 60);
    run_ui_display(ScreenTarget::Appliance, Some(0), None, None, 60);
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
    assert_eq!(shell_mock.current_tab(), 2);
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
fn run_render_loop_with_devices_polls_rotary_encoder_and_faults_gracefully() {
    use screen_manager::daemon::run_render_loop_with_devices;
    use screen_manager::ui::{AppShell, Framebuffer, MainWindow};
    use slint::ComponentHandle;
    use speaker_bluetooth::TransportCommandQueue;
    use speaker_hardware::EncoderDevice;

    let (mut fb, _) = Framebuffer::new_mocked();
    let window = fb.init_platform().unwrap();
    let main_window = MainWindow::new().unwrap();
    let shell = AppShell::new(main_window);
    shell.set_volume(50.0);
    shell.window().show().unwrap();
    fb.render_frame(&window).unwrap();

    let (enc, enc_mock) = EncoderDevice::new_mocked();
    enc_mock.simulate_rotation(speaker_hardware::RotationDirection::Clockwise, 2);
    let commands = TransportCommandQueue::new();
    // Encoder volume is executed by the internal amp-volume worker (mock
    // client because the framebuffer backend is mocked); wire the same queue
    // it receives.
    let amp_commands = screen_manager::volume::AmpVolumeCommandQueue::new();
    shell.wire_amp_volume(&amp_commands);

    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        None,
        &commands,
        &amp_commands,
        Some(1),
        None,
        Some(&enc),
    );
    assert!((shell.volume() - 54.0).abs() < 1e-4);
    assert!(shell.is_volume_hud_visible());
}

#[cfg(feature = "ui")]
#[test]
fn bt_monitor_state_commands_settle_and_commit() {
    use speaker_bluetooth::{BluetoothDevice, TransportCommand, TransportCommandQueue};

    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let commands = TransportCommandQueue::new();
    let mut state = BtMonitorState::default();

    // Initial tick populates last_pb, last_pcms, and shared state.
    assert!(state.poll_tick(&bt, &shared, &commands));
    assert!(state.has_playback());
    assert_eq!(state.keep_alive_interval(), BT_PLAYING_POLL);
    assert!(shared.lock().unwrap().playback.is_some());
    assert!(shared.lock().unwrap().pcm.is_some());

    // Arm seek settle window, then toggle settle window, and exercise all commands.
    commands.push(TransportCommand::Seek(45_000));
    assert!(state.poll_tick(&bt, &shared, &commands));
    assert!(matches!(
        state.settle().map(|s| &s.kind),
        Some(SettleKind::Seek { target_ms: 45_000 })
    ));

    commands.push(TransportCommand::TogglePlayPause);
    commands.push(TransportCommand::NextTrack);
    commands.push(TransportCommand::PreviousTrack);
    assert!(state.poll_tick(&bt, &shared, &commands));
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
    assert!(state.poll_tick(&bt, &shared, &commands));
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
            details: Some(screen_manager::ui::BtLinkDetails::default()),
            cover: Some(None),
            amp_volume: None,
        },
    );
    let recovered = poisoned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(recovered.playback.is_some());
    assert!(recovered.cover.is_some());
    assert!(recovered.details.is_some());
}

#[cfg(feature = "ui")]
#[test]
fn publish_link_state_stages_details_once_and_resets() {
    use speaker_bluetooth::BluetoothDevice;

    let (bt, _mock) = BluetoothDevice::new_mocked();
    let mut state = BtMonitorState::default();

    let mut first = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut first);
    let details = first.details.expect("first tick publishes details");
    assert!(details.device.is_some());
    assert!(details.pcm.is_some());
    assert!(first.pcm.is_some());

    // Unchanged snapshot is deduplicated.
    let mut second = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut second);
    assert!(second.details.is_none());
    assert!(second.pcm.is_none());

    // A dedup reset forces a full republish.
    state.reset_dedup();
    let mut third = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut third);
    assert!(third.details.is_some());
}

#[cfg(feature = "ui")]
#[test]
fn publish_link_state_keeps_state_on_query_fault() {
    use speaker_bluetooth::BluetoothDevice;

    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.set_fault(Some("link fault".into()));
    let mut state = BtMonitorState::default();
    let mut update = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut update);
    assert!(update.details.is_none());
    assert!(state.last_details().is_none());
}

#[cfg(feature = "ui")]
#[test]
fn publish_link_state_clears_details_on_disconnect() {
    use speaker_bluetooth::BluetoothDevice;

    let (bt, mock) = BluetoothDevice::new_mocked();
    let mut state = BtMonitorState::default();
    state.publish_link_state(&bt, &mut BtSharedUpdate::default());

    mock.clear_devices();
    mock.clear_pcms();
    let mut update = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut update);
    let details = update.details.expect("disconnect republishes details");
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

#[cfg(feature = "ui")]
#[test]
fn idle_sleep_various_fd_conditions() {
    // 1. None fd (always sleeps timeout)
    idle_sleep(None);

    // 2. Negative fd (POSIX specifies negative fds are ignored and revents=0, times out safely)
    idle_sleep(Some(-1));

    // 3. Pipe fd with no data (poll times out, sleeps safely)
    let mut fds = [0; 2];
    // SAFETY: pipe creates 2 valid descriptors.
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(ret, 0);
    let [rfd, wfd] = fds;

    idle_sleep(Some(rfd));

    // 4. Pipe fd with pending data (poll wakes immediately)
    let data = b"touch";
    // SAFETY: writing test bytes into valid pipe.
    let _ = unsafe { libc::write(wfd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
    idle_sleep(Some(rfd));

    // SAFETY: close test pipe descriptors.
    unsafe {
        libc::close(rfd);
        libc::close(wfd)
    };

    // 5. Pipe fd with write end closed (poll immediately wakes with POLLHUP, sleeps to avoid busy-spin)
    let mut fds2 = [0; 2];
    // SAFETY: pipe creates 2 valid descriptors.
    let ret2 = unsafe { libc::pipe(fds2.as_mut_ptr()) };
    assert_eq!(ret2, 0);
    let [rfd2, wfd2] = fds2;
    // SAFETY: closing writer triggers POLLHUP on reader without pending data.
    unsafe { libc::close(wfd2) };
    idle_sleep(Some(rfd2));
    // SAFETY: clean up test reader.
    unsafe { libc::close(rfd2) };

    // 6. Closed positive fd (poll returns POLLNVAL on Linux/POSIX, sleeps to avoid busy-spin)
    idle_sleep(Some(rfd2));
}
