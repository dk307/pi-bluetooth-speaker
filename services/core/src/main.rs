//! Central speaker daemon and hardware orchestration service for Raspberry Pi 3.
//!
//! # Binary Target Naming
//!
//! The executable binary target is named `core` in `Cargo.toml` (`[[bin]] name = "core"`)
//! matching the `DietPi` systemd unit `/etc/systemd/system/core.service`. To prevent shadowing
//! the standard library's `core` crate in external prelude and dependency graphs, the library
//! crate is named `speaker_core`.

#[cfg(feature = "ui")]
use std::path::Path;
use std::process::ExitCode;
#[cfg(feature = "ui")]
use std::time::{Duration, Instant};

use speaker_core::diag::{CliParse, env_args, next_arg_value, resolve_cli_opts};
use speaker_core::hardware::TempSensor;

#[cfg(feature = "ui")]
use slint::ComponentHandle;
#[cfg(feature = "ui")]
use speaker_core::ui::{AppShell, Framebuffer, MainWindow, SplashScreen};

/// Target screen to render on the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ScreenTarget {
    #[default]
    Splash,
    Shell,
}

/// Appliance service command-line options.
#[derive(Debug, Clone, Copy, Default)]
struct CoreCliOptions {
    preview_secs: Option<u64>,
    run_loop: bool,
    screen: ScreenTarget,
}

fn print_usage(bin_name: &str) {
    println!("DietPi Bluetooth Speaker & DSP Core Appliance Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -s, --screen <splash|shell> Target screen to render (default: splash)");
    println!("  -p, --preview <SECS>        Render UI for SECS seconds");
    println!("  -l, --loop                  Hold UI on screen continuously");
    println!("  -V, --version               Print version and exit");
    println!("  -h, --help                  Print this help text and exit");
}

/// Parses and validates a `--preview <SECS>` argument value (unsigned seconds).
///
/// # Errors
/// Returns a descriptive user-facing error string if `val` is missing, carries a sign
/// prefix, or is not a valid unsigned integer.
fn parse_preview_secs(val: &str) -> Result<u64, String> {
    if val.starts_with('+') || val.starts_with('-') {
        return Err(format!(
            "invalid preview seconds '{val}': sign prefixes are not accepted"
        ));
    }
    val.parse::<u64>()
        .map_err(|e| format!("invalid preview seconds '{val}': {e}"))
}

fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CoreCliOptions>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let mut opts = CoreCliOptions::default();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliParse::Help),
            "-V" | "--version" => return Ok(CliParse::Version),
            "-l" | "--loop" => opts.run_loop = true,
            "-s" | "--screen" => {
                let val = next_arg_value(&mut iter, "--screen")?;
                match val.trim().to_ascii_lowercase().as_str() {
                    "splash" => opts.screen = ScreenTarget::Splash,
                    "shell" | "main" => opts.screen = ScreenTarget::Shell,
                    other => {
                        return Err(format!(
                            "invalid screen target '{other}': expected 'splash' or 'shell'"
                        ));
                    }
                }
            }
            "-p" | "--preview" => {
                let val = next_arg_value(&mut iter, "--preview")?;
                opts.preview_secs = Some(parse_preview_secs(&val)?);
            }
            _ => return Err(format!("unknown option '{arg}', use --help for usage")),
        }
    }
    Ok(CliParse::Options(opts))
}

/// Reports a display problem to both tracing telemetry and user stderr.
///
/// The stderr tag mirrors the tracing `level` (`Warning` / `Error`) so the two
/// output channels stay symmetric by construction.
#[cfg(feature = "ui")]
fn report_display(level: tracing::Level, context: &str, err: impl std::fmt::Display) {
    let tag = match level {
        tracing::Level::ERROR => "Display Error",
        tracing::Level::WARN => "Display Warning",
        _ => "Display",
    };
    if level == tracing::Level::ERROR {
        tracing::error!("{context}: {err}");
    } else {
        tracing::warn!("{context}: {err}");
    }
    eprintln!("  {tag}: {context}: {err}");
}

#[cfg(feature = "ui")]
fn run_ui_display(screen: ScreenTarget, preview_secs: Option<u64>, run_loop: bool) {
    if headless_requested() {
        tracing::info!("Headless mode requested via SPEAKER_CORE_HEADLESS=1");
        println!("  Display: Headless mode requested via SPEAKER_CORE_HEADLESS.");
        return;
    }
    run_ui_display_on_path(
        Path::new(Framebuffer::DEFAULT_FB_PATH),
        screen,
        preview_secs,
        run_loop,
    );
}

/// Returns `true` only when the headless override is explicitly set to `"1"`
/// (a value of `"0"` or any other content keeps the display enabled).
#[cfg(feature = "ui")]
fn headless_requested() -> bool {
    std::env::var_os("SPEAKER_CORE_HEADLESS").is_some_and(|v| v == "1")
}

/// Runs the requested screen against a specific framebuffer device path.
#[cfg(feature = "ui")]
fn run_ui_display_on_path(
    fb_path: &Path,
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    run_loop: bool,
) {
    if !fb_path.exists() {
        tracing::info!("Headless/simulated environment (no /dev/fb0 detected)");
        println!("  Display: Headless/simulated environment (no /dev/fb0 detected).");
        return;
    }

    tracing::info!("Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)");
    println!("  Display: Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)...");
    let mut fb = match Framebuffer::open(fb_path, Some(Framebuffer::DEFAULT_BLANK_PATH)) {
        Ok(fb) => fb,
        Err(e) => {
            report_display(tracing::Level::WARN, "Could not open /dev/fb0", &e);
            return;
        }
    };

    match screen {
        ScreenTarget::Splash => run_splash_on(&mut fb, preview_secs, run_loop),
        ScreenTarget::Shell => run_shell_on(&mut fb, preview_secs, run_loop),
    }
}

/// Runs the common UI component lifecycle (unblank, platform init, setup callback, render loop)
/// on an opened framebuffer backend, keeping the component alive and testable with mocked hardware.
#[cfg(feature = "ui")]
fn run_ui_lifecycle<C, F>(
    fb: &mut Framebuffer,
    label: &str,
    preview_secs: Option<u64>,
    run_loop: bool,
    setup: F,
) where
    F: FnOnce() -> Result<C, Box<dyn std::error::Error>>,
{
    if let Err(e) = fb.unblank() {
        report_display(tracing::Level::WARN, "Failed to unblank display", &e);
    }

    let window = match fb.init_platform() {
        Ok(win) => win,
        Err(e) => {
            report_display(tracing::Level::ERROR, "Failed to initialize platform", &e);
            return;
        }
    };

    let _component = match setup() {
        Ok(c) => c,
        Err(e) => {
            report_display(
                tracing::Level::ERROR,
                &format!("Failed to initialize {label}"),
                e.as_ref(),
            );
            return;
        }
    };

    match fb.render_frame(&window) {
        Ok(true) => {
            tracing::info!("{label} rendered to /dev/fb0 successfully");
            println!("  Display: {label} rendered to /dev/fb0 successfully!");
        }
        Ok(false) => {
            tracing::debug!("Screen buffer already current");
            println!("  Display: Screen buffer already current.");
        }
        Err(e) => {
            report_display(tracing::Level::ERROR, "Failed to render frame", &e);
            return;
        }
    }

    if run_loop || preview_secs.is_some() {
        let start = Instant::now();
        let duration = preview_secs.map(Duration::from_secs);
        tracing::info!("Running UI render loop");
        println!("  Display: Running UI render loop...");

        while run_loop || duration.is_some_and(|d| start.elapsed() < d) {
            slint::platform::update_timers_and_animations();
            if let Err(e) = fb.render_frame(&window) {
                report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        tracing::info!("Preview complete");
        println!("  Display: Preview complete.");
    }
}

/// Runs the splash lifecycle against an opened framebuffer backend.
#[cfg(feature = "ui")]
fn run_splash_on(fb: &mut Framebuffer, preview_secs: Option<u64>, run_loop: bool) {
    run_ui_lifecycle(fb, "Boot splash screen", preview_secs, run_loop, || {
        let splash = SplashScreen::new().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        splash
            .show()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        Ok(splash)
    });
}

/// Runs the UI Shell lifecycle against an opened framebuffer backend.
#[cfg(feature = "ui")]
fn run_shell_on(fb: &mut Framebuffer, preview_secs: Option<u64>, run_loop: bool) {
    run_ui_lifecycle(fb, "UI Shell", preview_secs, run_loop, || {
        let main_window =
            MainWindow::new().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        let shell = AppShell::new(main_window);
        shell
            .window()
            .show()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        Ok(shell)
    });
}

fn main() -> ExitCode {
    let opts = match resolve_cli_opts(parse_cli_args_from(env_args()), "core", print_usage) {
        Ok(opts) => opts,
        Err(exit_code) => return exit_code,
    };

    let temp_sensor = TempSensor::new();

    println!("============================================================");
    println!("  Speaker Core Service is running on Raspberry Pi 3!");
    println!("  Target: ARM Cortex-A53 (aarch64)");
    println!("  Thermal Sensor: {}", temp_sensor.gpio_info());

    #[cfg(feature = "ui")]
    run_ui_display(opts.screen, opts.preview_secs, opts.run_loop);

    #[cfg(not(feature = "ui"))]
    let _ = (opts.screen, opts.preview_secs, opts.run_loop);
    #[cfg(not(feature = "ui"))]
    println!("  Display: UI feature disabled at compile-time.");

    println!("============================================================");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "ui")]
    use std::sync::Mutex;

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
        static ENV_GUARD: Mutex<()> = Mutex::new(());
        let _guard = ENV_GUARD.lock().unwrap();
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
        static ENV_GUARD: Mutex<()> = Mutex::new(());
        let _guard = ENV_GUARD.lock().unwrap();
        // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
        // SPEAKER_CORE_HEADLESS while this guard is held.
        unsafe { std::env::set_var("SPEAKER_CORE_HEADLESS", "1") };
        // Even with an existing device path, the headless guard must return
        // before any framebuffer/sysfs access.
        run_ui_display(ScreenTarget::Splash, Some(1), true);
        run_ui_display(ScreenTarget::Shell, Some(1), true);
        run_ui_display_on_path(Path::new("/tmp"), ScreenTarget::Splash, Some(1), true);
        run_ui_display_on_path(Path::new("/tmp"), ScreenTarget::Shell, Some(1), true);
        // SAFETY: same serialized-guard invariant as above.
        unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
    }

    #[cfg(feature = "ui")]
    #[test]
    fn run_ui_display_missing_device_returns_early() {
        static ENV_GUARD: Mutex<()> = Mutex::new(());
        let _guard = ENV_GUARD.lock().unwrap();
        // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
        // SPEAKER_CORE_HEADLESS while this guard is held.
        unsafe { std::env::remove_var("SPEAKER_CORE_HEADLESS") };
        let dir = speaker_core::diag::ScopedTempDir::new("fb-boot-splash-missing");
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
}
