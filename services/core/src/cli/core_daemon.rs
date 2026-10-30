//! Central appliance daemon and display orchestration service logic.
//!
//! Provides command-line options parsing, headless detection, display lifecycle
//! execution, and Slint framebuffer rendering coordination.

#[cfg(feature = "ui")]
use std::path::Path;
use std::process::ExitCode;
#[cfg(feature = "ui")]
use std::time::{Duration, Instant};

use crate::diag::{CliParse, env_args, next_arg_value, resolve_cli_opts};
use crate::hardware::TempSensor;
#[cfg(feature = "ui")]
use crate::ui::{AppShell, Framebuffer, MainWindow, SplashScreen};
#[cfg(feature = "ui")]
use slint::ComponentHandle;

/// Target screen to render on the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenTarget {
    /// Boot splash screen.
    #[default]
    Splash,
    /// Main user interface shell.
    Shell,
}

/// Appliance service command-line options.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreCliOptions {
    /// Optional preview duration in seconds.
    pub preview_secs: Option<u64>,
    /// Whether to hold the UI in an infinite animation loop.
    pub run_loop: bool,
    /// Screen target to initialize and display.
    pub screen: ScreenTarget,
    /// Whether to populate the Bluetooth Playing screen with a demo track.
    pub demo_playback: bool,
}

/// Prints usage text for the core daemon executable.
pub fn print_usage(bin_name: &str) {
    println!("DietPi Bluetooth Speaker & DSP Core Appliance Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -s, --screen <splash|shell> Target screen to render (default: splash)");
    println!("  -d, --demo-playback         Render Bluetooth Playing screen with demo track");
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
pub fn parse_preview_secs(val: &str) -> Result<u64, String> {
    if val.starts_with('+') || val.starts_with('-') {
        return Err(format!(
            "invalid preview seconds '{val}': sign prefixes are not accepted"
        ));
    }
    val.parse::<u64>()
        .map_err(|e| format!("invalid preview seconds '{val}': {e}"))
}

/// Parses command-line arguments into structured [`CoreCliOptions`].
///
/// # Errors
/// Returns an error message if an argument or flag value is malformed.
pub fn parse_cli_args_from<I>(args: I) -> Result<CliParse<CoreCliOptions>, String>
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
            "-d" | "--demo" | "--demo-playback" => {
                opts.demo_playback = true;
                opts.screen = ScreenTarget::Shell;
            }
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
#[cfg(feature = "ui")]
pub fn report_display(level: tracing::Level, context: &str, err: impl std::fmt::Display) {
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

/// Runs the UI display loop on the default `/dev/fb0` device path.
#[cfg(feature = "ui")]
pub fn run_ui_display(
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    run_loop: bool,
    demo_playback: bool,
) {
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
        demo_playback,
    );
}

/// Returns `true` only when the headless override is explicitly set to `"1"`
/// (a value of `"0"` or any other content keeps the display enabled).
#[must_use]
pub fn headless_requested() -> bool {
    std::env::var_os("SPEAKER_CORE_HEADLESS").is_some_and(|v| v == "1")
}

/// Runs the requested screen against a specific framebuffer device path.
#[cfg(feature = "ui")]
pub fn run_ui_display_on_path(
    fb_path: &Path,
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    run_loop: bool,
    demo_playback: bool,
) {
    if !fb_path.exists() {
        tracing::info!("Headless/simulated environment (no /dev/fb0 detected)");
        println!("  Display: Headless/simulated environment (no /dev/fb0 detected).");
        return;
    }

    tracing::info!("Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)");
    println!("  Display: Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)...");
    let blank_path = (fb_path == Path::new(Framebuffer::DEFAULT_FB_PATH))
        .then_some(Framebuffer::DEFAULT_BLANK_PATH);
    let mut fb = match Framebuffer::open(fb_path, blank_path) {
        Ok(fb) => fb,
        Err(e) => {
            report_display(tracing::Level::WARN, "Could not open /dev/fb0", &e);
            return;
        }
    };

    match screen {
        ScreenTarget::Splash => run_splash_on(&mut fb, preview_secs, run_loop),
        ScreenTarget::Shell => run_shell_on(&mut fb, preview_secs, run_loop, demo_playback),
    }
}

/// Runs the common UI component lifecycle on an opened framebuffer backend.
#[cfg(feature = "ui")]
pub fn run_ui_lifecycle<C, F>(
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
pub fn run_splash_on(fb: &mut Framebuffer, preview_secs: Option<u64>, run_loop: bool) {
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
pub fn run_shell_on(
    fb: &mut Framebuffer,
    preview_secs: Option<u64>,
    run_loop: bool,
    demo_playback: bool,
) {
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

    let main_window = match MainWindow::new() {
        Ok(win) => win,
        Err(e) => {
            report_display(
                tracing::Level::ERROR,
                "Failed to initialize UI Shell MainWindow",
                &e,
            );
            return;
        }
    };
    let shell = AppShell::new(main_window);
    if demo_playback {
        shell.apply_demo_playback();
    }
    if let Err(e) = shell.window().show() {
        report_display(
            tracing::Level::ERROR,
            "Failed to show UI Shell MainWindow",
            &e,
        );
        return;
    }

    match fb.render_frame(&window) {
        Ok(true) => {
            tracing::info!("UI Shell rendered to /dev/fb0 successfully");
            println!("  Display: UI Shell rendered to /dev/fb0 successfully!");
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

        let mut sim_pos_ms = 105_000u32;
        let sim_dur_ms = 230_000u32;
        let mut sim_tick = 0u32;

        while run_loop || duration.is_some_and(|d| start.elapsed() < d) {
            if demo_playback {
                sim_pos_ms = (sim_pos_ms + 16) % sim_dur_ms;
                sim_tick = sim_tick.wrapping_add(1);
                shell.set_playback_state(true, sim_pos_ms, sim_dur_ms);
                let l_peak = 0.65 + 0.15 * ((sim_tick as f32 * 0.1).sin()).abs();
                let r_peak = 0.60 + 0.18 * ((sim_tick as f32 * 0.12).cos()).abs();
                shell.set_peak_levels(l_peak, r_peak);
                window.request_redraw();
            }

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

/// Executes the core appliance daemon with parsed options.
#[must_use]
pub fn run_app(opts: CoreCliOptions) -> ExitCode {
    let temp_sensor = TempSensor::new();

    println!("============================================================");
    println!("  Speaker Core Service is running on Raspberry Pi 3!");
    println!("  Target: ARM Cortex-A53 (aarch64)");
    println!("  Thermal Sensor: {}", temp_sensor.gpio_info());

    #[cfg(feature = "ui")]
    run_ui_display(
        opts.screen,
        opts.preview_secs,
        opts.run_loop,
        opts.demo_playback,
    );

    #[cfg(not(feature = "ui"))]
    let _ = (
        opts.screen,
        opts.preview_secs,
        opts.run_loop,
        opts.demo_playback,
    );
    #[cfg(not(feature = "ui"))]
    println!("  Display: UI feature disabled at compile-time.");

    println!("============================================================");
    ExitCode::SUCCESS
}

/// CLI main entrypoint for the `core` binary.
pub fn main() -> ExitCode {
    match resolve_cli_opts(parse_cli_args_from(env_args()), "core", print_usage) {
        Ok(opts) => run_app(opts),
        Err(exit_code) => exit_code,
    }
}
