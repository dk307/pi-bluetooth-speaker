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
use speaker_core::ui::{Framebuffer, SplashScreen};

/// Appliance service command-line options.
#[derive(Debug, Clone, Copy, Default)]
struct CoreCliOptions {
    preview_secs: Option<u64>,
    run_loop: bool,
}

fn print_usage(bin_name: &str) {
    println!("DietPi Bluetooth Speaker & DSP Core Appliance Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -p, --preview <SECS>  Render boot splash UI for SECS seconds");
    println!("  -l, --loop            Hold the boot splash UI on screen continuously");
    println!("  -V, --version         Print version and exit");
    println!("  -h, --help            Print this help text and exit");
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
            "-p" | "--preview" => {
                let val = next_arg_value(&mut iter, "--preview")?;
                opts.preview_secs = Some(parse_preview_secs(&val)?);
            }
            _ => return Err(format!("unknown option '{arg}', use --help for usage")),
        }
    }
    Ok(CliParse::Options(opts))
}

/// Reports a non-fatal display problem to both tracing telemetry and user stderr.
#[cfg(feature = "ui")]
fn report_display_warning(context: &str, err: impl std::fmt::Display) {
    tracing::warn!("{context}: {err}");
    eprintln!("  Display Warning: {context}: {err}");
}

/// Reports a fatal display problem to both tracing telemetry and user stderr.
#[cfg(feature = "ui")]
fn report_display_error(context: &str, err: impl std::fmt::Display) {
    tracing::error!("{context}: {err}");
    eprintln!("  Display Error: {context}: {err}");
}

#[cfg(feature = "ui")]
fn run_ui_boot_splash(preview_secs: Option<u64>, run_loop: bool) {
    if std::env::var_os("SPEAKER_CORE_HEADLESS").is_some_and(|v| v == "1") {
        tracing::info!("Headless mode requested via SPEAKER_CORE_HEADLESS=1");
        println!("  Display: Headless mode requested via SPEAKER_CORE_HEADLESS.");
        return;
    }

    let fb_path = Path::new(Framebuffer::DEFAULT_FB_PATH);
    if !fb_path.exists() {
        tracing::info!("Headless/simulated environment (no /dev/fb0 detected)");
        println!("  Display: Headless/simulated environment (no /dev/fb0 detected).");
        return;
    }

    tracing::info!("Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)");
    println!("  Display: Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)...");
    let mut fb = match Framebuffer::new() {
        Ok(fb) => fb,
        Err(e) => {
            report_display_warning("Could not open /dev/fb0", &e);
            return;
        }
    };

    run_splash_on(&mut fb, preview_secs, run_loop);
}

/// Runs the splash lifecycle (unblank, platform init, component show, render loop)
/// on an opened framebuffer backend, keeping the path testable with mocked hardware.
#[cfg(feature = "ui")]
fn run_splash_on(fb: &mut Framebuffer, preview_secs: Option<u64>, run_loop: bool) {
    if let Err(e) = fb.unblank() {
        report_display_warning("Failed to unblank display", &e);
    }

    let window = match fb.init_platform() {
        Ok(win) => win,
        Err(e) => {
            report_display_error("Failed to initialize platform", &e);
            return;
        }
    };

    let splash = match SplashScreen::new() {
        Ok(s) => s,
        Err(e) => {
            report_display_error("Failed to initialize SplashScreen", &e);
            return;
        }
    };

    if let Err(e) = splash.show() {
        report_display_warning("Failed to show SplashScreen", &e);
        return;
    }

    match fb.render_frame(&window) {
        Ok(true) => {
            tracing::info!("Boot splash screen rendered to /dev/fb0 successfully");
            println!("  Display: Boot splash screen rendered to /dev/fb0 successfully!");
        }
        Ok(false) => {
            tracing::debug!("Screen buffer already current");
            println!("  Display: Screen buffer already current.");
        }
        Err(e) => {
            report_display_error("Failed to render frame", &e);
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
                report_display_error("Render loop frame failed", &e);
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        tracing::info!("Preview complete");
        println!("  Display: Preview complete.");
    }
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
    run_ui_boot_splash(opts.preview_secs, opts.run_loop);

    #[cfg(not(feature = "ui"))]
    let _ = (opts.preview_secs, opts.run_loop);
    #[cfg(not(feature = "ui"))]
    println!("  Display: UI feature disabled at compile-time.");

    println!("============================================================");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

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

        match parsed(&["-p", "5", "-l"]).unwrap() {
            CliParse::Options(o) => {
                assert_eq!(o.preview_secs, Some(5));
                assert!(o.run_loop);
            }
            other => panic!("expected Options, got {other:?}"),
        }

        assert!(matches!(parsed(&["-h"]).unwrap(), CliParse::Help));
        assert!(matches!(parsed(&["--version"]).unwrap(), CliParse::Version));

        for args in [
            &["-p"][..],
            &["-p", "+5"][..],
            &["-p", "nan"][..],
            &["-x"][..],
            &["--loop", "--preview", "1", "extra"][..],
        ] {
            assert!(parsed(args).is_err(), "expected rejection for {args:?}");
        }
    }
}
