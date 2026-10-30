//! Integration tests for the central core appliance daemon.

#[cfg(feature = "ui")]
use std::fs;
#[cfg(feature = "ui")]
use std::sync::Mutex;

use speaker_core::cli::core_daemon::{
    CoreCliOptions, ScreenTarget, headless_requested, parse_cli_args_from, parse_preview_secs,
    print_usage,
};
#[cfg(feature = "ui")]
use speaker_core::cli::core_daemon::{
    report_display, run_app, run_shell_on, run_splash_on, run_ui_display, run_ui_display_on_path,
    run_ui_lifecycle,
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
    run_ui_display(ScreenTarget::Splash, Some(1), true);
    run_ui_display(ScreenTarget::Shell, Some(1), true);
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
    run_ui_display_on_path(&missing, ScreenTarget::Splash, Some(1), true);
    run_ui_display_on_path(&missing, ScreenTarget::Shell, Some(1), true);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_renders_and_tracks_counters() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    assert!(fb.is_mocked());
    run_splash_on(&mut fb, None, false);
    assert_eq!(fb.unblank_count(), 1);
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_zero_preview_skips_loop() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    run_splash_on(&mut fb, Some(0), false);
    // Initial render + no loop iterations for zero-length preview
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_reports_unblank_failure_and_continues() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_unblank_error("simulated unblank failure");
    run_splash_on(&mut fb, None, false);
    // Unblank failure is non-fatal: splash still initializes and renders
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_splash_on_mocked_render_error_skips_loop() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_render_error("simulated render failure");
    run_splash_on(&mut fb, Some(0), false);
    assert_eq!(fb.render_count(), 0);
    assert_eq!(ctrl.counters(), (1, 0));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_renders_and_tracks_counters() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    assert!(fb.is_mocked());
    run_shell_on(&mut fb, None, false);
    assert_eq!(fb.unblank_count(), 1);
    assert_eq!(fb.render_count(), 1);
    assert_eq!(ctrl.counters(), (1, 1));
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_reports_unblank_failure_and_continues() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_unblank_error("simulated unblank failure");
    run_shell_on(&mut fb, None, false);
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 1);
}

#[cfg(feature = "ui")]
#[test]
fn run_shell_on_mocked_render_error_skips_loop() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    ctrl.set_render_error("simulated render failure");
    run_shell_on(&mut fb, Some(0), false);
    assert_eq!(fb.render_count(), 0);
    assert_eq!(ctrl.counters(), (1, 0));
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
    });
    assert_eq!(code, std::process::ExitCode::SUCCESS);

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

    run_ui_display_on_path(&dev, ScreenTarget::Splash, None, false);
    run_ui_display_on_path(&dev, ScreenTarget::Shell, None, false);

    // Invalid file (too short) to trigger Framebuffer::open failure path
    let bad_dev = dir.path().join("bad_fb");
    fs::write(&bad_dev, [0u8; 10]).unwrap();
    run_ui_display_on_path(&bad_dev, ScreenTarget::Splash, None, false);
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
    run_ui_display_on_path(&missing, ScreenTarget::Splash, None, false);
    run_ui_display_on_path(&missing, ScreenTarget::Shell, None, false);

    // SAFETY: serialized test guard
    unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
}
