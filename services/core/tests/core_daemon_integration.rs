//! Integration tests for the central core appliance daemon.

#[cfg(feature = "ui")]
use std::fs;
#[cfg(feature = "ui")]
use std::sync::Mutex;

use speaker_core::cli::core_daemon::{
    CoreCliOptions, ScreenTarget, headless_requested, parse_cli_args_from, parse_preview_secs,
    parse_tab_arg, print_usage,
};
#[cfg(feature = "ui")]
use speaker_core::cli::core_daemon::{
    init_shell, report_display, run_app, run_render_loop, run_shell_on, run_splash_on,
    run_ui_display, run_ui_display_on_path, run_ui_lifecycle,
};
use speaker_core::diag::CliParse;
#[cfg(feature = "ui")]
use speaker_core::diag::ScopedTempDir;
#[cfg(feature = "ui")]
use speaker_core::ui::Framebuffer;

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
    run_ui_display(ScreenTarget::Appliance, Some(1), true, None, None);
    run_ui_display(ScreenTarget::Splash, Some(1), true, None, None);
    run_ui_display(ScreenTarget::Shell, Some(1), true, None, None);
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
    run_ui_display_on_path(&missing, ScreenTarget::Appliance, Some(1), true, None, None);
    run_ui_display_on_path(&missing, ScreenTarget::Splash, Some(1), true, None, None);
    run_ui_display_on_path(&missing, ScreenTarget::Shell, Some(1), true, None, None);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_renders_and_tracks_counters() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    assert!(fb.is_mocked());
    run_splash_on(&mut fb, None, false, None);
    assert_eq!(fb.unblank_count(), 1);
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_zero_preview_skips_loop() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    run_splash_on(&mut fb, Some(0), false, None);
    // Initial render + no loop iterations for zero-length preview
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_reports_unblank_failure_and_continues() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_unblank_error("simulated unblank failure");
    run_splash_on(
        &mut fb,
        None,
        false,
        Some(speaker_core::ui::ThemePreset::Peppy),
    );
    // Unblank failure is non-fatal: splash still initializes and renders
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_render_error_skips_loop() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_render_error("simulated render failure");
    run_splash_on(&mut fb, Some(0), false, None);
    assert_eq!(fb.render_count(), 0);
    assert_eq!(ctrl.counters(), (1, 0));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_renders_and_tracks_counters() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    assert!(fb.is_mocked());
    run_shell_on(&mut fb, None, false, None, None);
    assert_eq!(fb.unblank_count(), 1);
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_reports_unblank_failure_and_continues() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_unblank_error("simulated unblank failure");
    run_shell_on(&mut fb, None, false, None, None);
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_render_error_skips_loop() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_render_error("simulated render failure");
    run_shell_on(&mut fb, Some(0), false, None, None);
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
        false,
        Some(speaker_core::ui::ThemePreset::Peppy),
        Some(1),
    );
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_render_loop_mocked_and_bluetooth_paths() {
    use speaker_core::ui::{AppShell, MainWindow};

    let (mut fb, ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().unwrap();
    let main_window = MainWindow::new().unwrap();
    let shell = AppShell::new(main_window);

    // 1. None bluetooth path breaking on render failure
    ctrl.set_render_error("simulated loop failure");
    run_render_loop(&mut fb, &window, &shell, None, Some(10), true);

    // 2. Bluetooth sync branch with mocked BluetoothDevice breaking on render failure
    let (bt, _) = speaker_core::hardware::BluetoothDevice::new_mocked();
    ctrl.set_render_error("simulated bt loop failure");
    run_render_loop(&mut fb, &window, &shell, Some(&bt), Some(10), true);

    // 3. Bluetooth sync branch running short preview loop to completion
    run_render_loop(&mut fb, &window, &shell, Some(&bt), Some(1), false);
}

#[test]
fn parse_cli_args_flags_and_values() {
    let parsed = |args: &[&str]| {
        parse_cli_args_from(args.iter().map(ToString::to_string).collect::<Vec<_>>())
    };

    match parsed(&[]).unwrap() {
        CliParse::Options(o) => {
            assert!(o.preview_secs.is_none());
            assert!(!o.run_loop);
            assert_eq!(o.screen, ScreenTarget::Appliance);
            assert!(o.theme.is_none());
            assert!(o.tab.is_none());
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-p", "5", "-l", "--screen", "shell"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.preview_secs, Some(5));
            assert!(o.run_loop);
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

    match parsed(&["--theme", "peppy", "-T", "vu"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.theme, Some(speaker_core::ui::ThemePreset::Peppy));
            assert_eq!(o.tab, Some(1));
        }
        other => panic!("expected Options, got {other:?}"),
    }

    match parsed(&["-t", "marantz", "--tab", "settings"]).unwrap() {
        CliParse::Options(o) => {
            assert_eq!(o.theme, Some(speaker_core::ui::ThemePreset::MarantzAmber));
            assert_eq!(o.tab, Some(5));
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
        &["--loop", "--preview", "1", "extra"][..],
    ] {
        assert!(parsed(args).is_err(), "expected rejection for {args:?}");
    }
}

#[cfg(feature = "ui")]
#[test]
fn run_app_headless_executes_successfully() {
    let _guard = lock_env();
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // SPEAKER_CORE_HEADLESS while this guard is held.
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "1") };

    let code = run_app(CoreCliOptions {
        preview_secs: Some(1),
        run_loop: false,
        screen: ScreenTarget::Splash,
        theme: None,
        tab: None,
    });
    assert_eq!(code, std::process::ExitCode::SUCCESS);

    let code_appliance = run_app(CoreCliOptions {
        preview_secs: Some(1),
        run_loop: false,
        screen: ScreenTarget::Appliance,
        theme: Some(speaker_core::ui::ThemePreset::Peppy),
        tab: Some(1),
    });
    assert_eq!(code_appliance, std::process::ExitCode::SUCCESS);

    let code_shell = run_app(CoreCliOptions {
        preview_secs: Some(1),
        run_loop: false,
        screen: ScreenTarget::Shell,
        theme: Some(speaker_core::ui::ThemePreset::Peppy),
        tab: Some(1),
    });
    assert_eq!(code_shell, std::process::ExitCode::SUCCESS);

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

    run_ui_display_on_path(&dev, ScreenTarget::Appliance, None, false, None, None);
    run_ui_display_on_path(&dev, ScreenTarget::Splash, None, false, None, None);
    run_ui_display_on_path(
        &dev,
        ScreenTarget::Shell,
        None,
        false,
        Some(speaker_core::ui::ThemePreset::Peppy),
        Some(1),
    );

    // Invalid file (too short) to trigger Framebuffer::open failure path
    let bad_dev = dir.path().join("bad_fb");
    fs::write(&bad_dev, [0u8; 10]).unwrap();
    run_ui_display_on_path(&bad_dev, ScreenTarget::Splash, None, false, None, None);
}

#[cfg(feature = "ui")]
#[test]
fn run_ui_lifecycle_error_and_preview_paths() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    // Test setup failure
    run_ui_lifecycle(&mut fb, "Failing Component", None, false, || {
        Err::<(), Box<dyn std::error::Error>>("simulated setup failure".into())
    });

    // Test already current buffer (second render without request_redraw returns Ok(false))
    run_ui_lifecycle(&mut fb, "Already Current", None, false, || Ok(()));

    // Test preview loop without error
    run_ui_lifecycle(&mut fb, "Active Render Loop", Some(1), false, || Ok(()));

    // Test preview loop break on render error
    ctrl.set_render_error("simulated render loop error");
    run_ui_lifecycle(&mut fb, "Failing Render Loop", Some(1), false, || Ok(()));
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
    run_ui_display_on_path(&missing, ScreenTarget::Splash, None, false, None, None);
    run_ui_display_on_path(&missing, ScreenTarget::Shell, None, false, None, None);
    run_ui_display_on_path(&missing, ScreenTarget::Appliance, None, false, None, None);

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
        speaker_core::cli::core_daemon::main(),
        std::process::ExitCode::SUCCESS
    );
    // SAFETY: serialized test guard
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}

#[cfg(feature = "ui")]
#[test]
fn test_core_daemon_run_ui_display_unheadless_and_init_shell_direct() {
    use speaker_core::ui::{Framebuffer, MainWindow};

    // 1. run_ui_display with headless disabled calls run_ui_display_on_path
    let _guard = lock_env();
    // SAFETY: serialized test guard
    unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "0") };
    run_ui_display(ScreenTarget::Splash, None, false, None, None);
    run_ui_display(ScreenTarget::Shell, None, false, None, None);
    run_ui_display(ScreenTarget::Appliance, None, false, None, None);
    // SAFETY: serialized test guard
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };

    // 2. init_shell with mocked_device = false exercises production Bluetooth connection path
    let (fb, _) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();
    let main_window = MainWindow::new().unwrap();
    let (shell, _) = init_shell(
        main_window,
        false,
        Some(speaker_core::ui::ThemePreset::Peppy),
        Some(1),
    );
    assert!(!shell.is_playing());
    assert_eq!(shell.current_theme(), speaker_core::ui::ThemePreset::Peppy);
    assert_eq!(shell.current_tab(), 1);

    // 3. init_shell with mocked_device = true exercises mocked Bluetooth path and playback sync
    let main_window_mock = MainWindow::new().unwrap();
    let (shell_mock, bt_mock) = init_shell(
        main_window_mock,
        true,
        Some(speaker_core::ui::ThemePreset::CyberpunkNeon),
        Some(2),
    );
    assert!(bt_mock.is_some());
    assert!(shell_mock.is_playing());
    assert_eq!(shell_mock.current_tab(), 2);
}
