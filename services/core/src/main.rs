//! Central speaker daemon and hardware orchestration service for Raspberry Pi 3.
//!
//! # Binary Target Naming
//!
//! The executable binary target is named `core` in `Cargo.toml` (`[[bin]] name = "core"`)
//! matching the `DietPi` systemd unit `/etc/systemd/system/core.service`. To prevent shadowing
//! the standard library's `core` crate in external prelude and dependency graphs, the library
//! crate is named `speaker_core`.

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

#[cfg(feature = "ui")]
fn run_ui_boot_splash(preview_secs: Option<u64>, run_loop: bool) {
    if std::env::var_os("SPEAKER_CORE_HEADLESS").is_some() {
        tracing::info!("Headless mode requested via SPEAKER_CORE_HEADLESS");
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
            tracing::warn!("Could not open /dev/fb0: {e}");
            eprintln!("  Display Warning: Could not open /dev/fb0: {e}");
            return;
        }
    };

    if let Err(e) = fb.unblank() {
        tracing::warn!("Failed to unblank display: {e}");
        eprintln!("  Display Warning: Failed to unblank display: {e}");
    }

    let window = match fb.init_platform() {
        Ok(win) => win,
        Err(e) => {
            tracing::error!("Failed to initialize platform: {e}");
            eprintln!("  Display Error: Failed to initialize platform: {e}");
            return;
        }
    };

    let splash = match SplashScreen::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to initialize SplashScreen: {e}");
            eprintln!("  Display Error: Failed to initialize SplashScreen: {e}");
            return;
        }
    };

    if let Err(e) = splash.show() {
        tracing::warn!("Failed to show SplashScreen: {e}");
        eprintln!("  Display Warning: Failed to show SplashScreen: {e}");
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
            tracing::error!("Failed to render frame: {e}");
            eprintln!("  Display Error: Failed to render frame: {e}");
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
                tracing::error!("Render loop frame failed: {e}");
                eprintln!("  Display Error: render loop frame failed: {e}");
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
