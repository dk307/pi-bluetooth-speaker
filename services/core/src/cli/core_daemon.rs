//! Central appliance daemon and display orchestration service logic.
//!
//! Provides command-line options parsing, headless detection, display lifecycle
//! execution, and Slint framebuffer rendering coordination.

#[cfg(feature = "ui")]
use std::path::Path;
use std::process::ExitCode;
#[cfg(feature = "ui")]
use std::rc::Rc;
#[cfg(feature = "ui")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "ui")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "ui")]
use std::time::{Duration, Instant};

use crate::diag::{CliParse, env_args, next_arg_value, resolve_cli_opts};
use crate::hardware::TempSensor;
#[cfg(feature = "ui")]
use crate::ui::{AppShell, Framebuffer, MainWindow, SplashScreen};
#[cfg(feature = "ui")]
use slint::ComponentHandle;
#[cfg(feature = "ui")]
use slint::platform::WindowAdapter;
#[cfg(feature = "ui")]
use slint::platform::software_renderer::MinimalSoftwareWindow;

/// Target screen to render on the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenTarget {
    /// Boot splash sequence followed by main UI shell (production appliance default).
    #[default]
    Appliance,
    /// Boot splash screen only.
    Splash,
    /// Main user interface shell only.
    Shell,
}

/// Appliance service command-line options.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreCliOptions {
    /// Optional preview duration in seconds.
    pub preview_secs: Option<u64>,
    /// Screen target to initialize and display.
    pub screen: ScreenTarget,
    /// Theme preset to apply on startup.
    #[cfg(feature = "ui")]
    pub theme: Option<crate::ui::ThemePreset>,
    /// Navigation dock tab to open on startup (0 to 5).
    pub tab: Option<i32>,
}

/// Prints usage text for the core daemon executable.
pub fn print_usage(bin_name: &str) {
    println!("DietPi Bluetooth Speaker & DSP Core Appliance Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "  -s, --screen <appliance|splash|shell> Target screen to render (default: appliance)"
    );
    println!(
        "  -t, --theme <PRESET>        Appliance theme preset (mcintosh, marantz, cyberpunk, studio-dark, peppy)"
    );
    println!(
        "  -T, --tab <TAB>             Active UI tab on startup (0-5 or name: player, vu, bt, cd, dsp, settings)"
    );
    println!("  -p, --preview <SECS>        Render UI for SECS seconds (preview/testing mode)");
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

/// Parses and validates a `--tab <TAB>` argument value.
///
/// Accepts integer tab indices (0 to 5) or standard tab slug names:
/// `player`, `vu` / `peppy`, `bluetooth`, `cd`, `dsp`, `settings`.
///
/// # Errors
/// Returns a descriptive error string if `val` is invalid.
pub fn parse_tab_arg(val: &str) -> Result<i32, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "0" | "player" | "now-playing" | "nowplaying" => Ok(0),
        "1" | "vu" | "vumeter" | "meter" | "peppy" | "peepy" | "peppymeter" => Ok(1),
        "2" | "bt" | "bluetooth" => Ok(2),
        "3" | "cd" | "optical" => Ok(3),
        "4" | "dsp" | "tone" | "camilla" => Ok(4),
        "5" | "settings" | "prefs" | "preferences" | "theme" => Ok(5),
        other => Err(format!(
            "invalid tab '{other}': expected 0-5 or 'player', 'vu', 'bluetooth', 'cd', 'dsp', 'settings'"
        )),
    }
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
            "-t" | "--theme" => {
                let val = next_arg_value(&mut iter, "--theme")?;
                #[cfg(feature = "ui")]
                {
                    let preset = crate::ui::ThemePreset::parse_slug(&val)?;
                    opts.theme = Some(preset);
                }
                #[cfg(not(feature = "ui"))]
                let _ = val;
            }
            "-T" | "--tab" => {
                let val = next_arg_value(&mut iter, "--tab")?;
                opts.tab = Some(parse_tab_arg(&val)?);
            }
            "-s" | "--screen" => {
                let val = next_arg_value(&mut iter, "--screen")?;
                match val.trim().to_ascii_lowercase().as_str() {
                    "appliance" | "full" | "auto" => opts.screen = ScreenTarget::Appliance,
                    "splash" => opts.screen = ScreenTarget::Splash,
                    "shell" | "main" => opts.screen = ScreenTarget::Shell,
                    other => {
                        return Err(format!(
                            "invalid screen target '{other}': expected 'appliance', 'splash', or 'shell'"
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
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
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
        theme,
        tab,
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
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
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
        ScreenTarget::Appliance => {
            // Default appliance sequence: POST splash screen, then main UI shell
            let splash_secs = preview_secs.map_or(2, |p| p.min(2));
            run_splash_on(&mut fb, Some(splash_secs), theme);
            let remaining_preview = preview_secs.and_then(|p| p.checked_sub(splash_secs));
            run_shell_on(&mut fb, remaining_preview, theme, tab);
        }
        ScreenTarget::Splash => {
            let splash_secs = preview_secs.or(Some(2));
            run_splash_on(&mut fb, splash_secs, theme);
        }
        ScreenTarget::Shell => {
            run_shell_on(&mut fb, preview_secs, theme, tab);
        }
    }
}

/// Runs the common UI component lifecycle on an opened framebuffer backend.
#[cfg(feature = "ui")]
pub fn run_ui_lifecycle<C, F>(
    fb: &mut Framebuffer,
    label: &str,
    preview_secs: Option<u64>,
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

    let start = Instant::now();
    let duration = preview_secs.map(Duration::from_secs);
    tracing::info!("Running UI render loop");
    println!("  Display: Running UI render loop...");

    while duration.is_none_or(|d| start.elapsed() < d) {
        slint::platform::update_timers_and_animations();
        if let Err(e) = fb.render_frame(&window) {
            report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    tracing::info!("Render loop finished");
    println!("  Display: Render loop finished.");
}

/// Runs the splash lifecycle against an opened framebuffer backend.
#[cfg(feature = "ui")]
pub fn run_splash_on(
    fb: &mut Framebuffer,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
) {
    run_ui_lifecycle(fb, "Boot splash screen", preview_secs, || {
        let splash = SplashScreen::new().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        if let Some(t) = theme {
            t.apply_splash(&splash);
        }
        splash
            .show()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
        Ok(splash)
    });
}

#[cfg(feature = "ui")]
#[must_use]
pub fn init_shell(
    main_window: MainWindow,
    mocked_device: bool,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
) -> (AppShell, Option<crate::hardware::BluetoothDevice>) {
    let shell = AppShell::new(main_window);
    if let Some(t) = theme {
        shell.apply_theme(t);
    }
    let maybe_bt = if mocked_device {
        let (mock_bt, _) = crate::hardware::BluetoothDevice::new_mocked();
        if let Ok(Some(ref pb)) = mock_bt.playback_state() {
            shell.apply_bluetooth_playback(pb);
        }
        shell.wire_bluetooth_transport(mock_bt.clone());
        Some(mock_bt)
    } else {
        match crate::hardware::BluetoothDevice::new() {
            Ok(bt) => {
                if let Ok(Some(ref pb)) = bt.playback_state() {
                    shell.apply_bluetooth_playback(pb);
                }
                shell.wire_bluetooth_transport(bt.clone());
                Some(bt)
            }
            Err(e) => {
                report_display(
                    tracing::Level::WARN,
                    "Failed to connect to Bluetooth D-Bus for UI transport",
                    &e,
                );
                None
            }
        }
    };
    if let Some(tb) = tab {
        shell.set_tab(tb);
    }
    (shell, maybe_bt)
}

#[cfg(feature = "ui")]
pub fn run_render_loop(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    shell: &AppShell,
    maybe_bt: Option<&crate::hardware::BluetoothDevice>,
    preview_secs: Option<u64>,
) {
    let start = Instant::now();
    let duration = preview_secs.map(Duration::from_secs);
    tracing::info!("Running UI render loop");
    println!("  Display: Running UI render loop...");

    let bg_running = Arc::new(AtomicBool::new(true));
    let bg_running_worker = Arc::clone(&bg_running);
    let shared_state: Arc<
        Mutex<(
            Option<crate::hardware::BtPlaybackState>,
            Option<crate::hardware::BlueAlsaPcm>,
        )>,
    > = Arc::new(Mutex::new((None, None)));
    let shared_state_worker = Arc::clone(&shared_state);

    let bt_worker = maybe_bt.map(|bt| {
        let bt = bt.clone();
        std::thread::Builder::new()
            .name("bt-monitor".into())
            .spawn(move || {
                let mut last_pb: Option<crate::hardware::BtPlaybackState> = None;
                let mut last_pcms: Option<Box<[crate::hardware::BlueAlsaPcm]>> = None;

                while bg_running_worker.load(Ordering::Relaxed) {
                    let is_playing = last_pb
                        .as_ref()
                        .is_some_and(crate::hardware::BtPlaybackState::is_playing);
                    let poll_interval = if is_playing {
                        Duration::from_millis(250)
                    } else {
                        Duration::from_millis(750)
                    };

                    if let Ok(Some(ref pb)) = bt.playback_state() {
                        if last_pb.as_ref() != Some(pb) {
                            if let Ok(mut lock) = shared_state_worker.lock() {
                                lock.0 = Some(pb.clone());
                            }
                            last_pb = Some(pb.clone());
                        }
                    }
                    if let Ok(ref pcms) = bt.audio_pcms() {
                        if last_pcms.as_ref() != Some(pcms) {
                            if let Some(active) =
                                pcms.iter().find(|p| p.running).or_else(|| pcms.first())
                            {
                                if let Ok(mut lock) = shared_state_worker.lock() {
                                    lock.1 = Some(active.clone());
                                }
                            }
                            last_pcms = Some(pcms.clone());
                        }
                    }

                    let deadline = Instant::now() + poll_interval;
                    while bg_running_worker.load(Ordering::Relaxed) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            })
    });

    while duration.is_none_or(|d| start.elapsed() < d) {
        let frame_start = Instant::now();

        if let Ok(mut lock) = shared_state.try_lock() {
            if let Some(pb) = lock.0.take() {
                shell.apply_bluetooth_playback(&pb);
            }
            if let Some(pcm) = lock.1.take() {
                shell.apply_pcm_stream(&pcm);
            }
        }

        let is_animating = shell.window().get_is_animating();
        if is_animating {
            window.window().request_redraw();
        }

        slint::platform::update_timers_and_animations();
        let did_render = match fb.render_frame(window) {
            Ok(rendered) => rendered,
            Err(e) => {
                report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
                break;
            }
        };

        let has_animations = window.window().has_active_animations();
        if did_render || has_animations || is_animating {
            // Target precise 60 FPS (16,667 µs per frame) for buttery smooth, tear-free animations
            const TARGET_FRAME_TIME: Duration = Duration::from_micros(16_667);
            let elapsed = frame_start.elapsed();
            if let Some(remaining) = TARGET_FRAME_TIME.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
        } else {
            // Adaptive power-saving sleep when UI is completely static (0% CPU at idle)
            let timer_duration = slint::platform::duration_until_next_timer_update();
            let sleep_duration = match timer_duration {
                Some(d) => d.min(Duration::from_millis(50)),
                None => Duration::from_millis(50),
            };
            std::thread::sleep(sleep_duration.max(Duration::from_millis(10)));
        }
    }

    bg_running.store(false, Ordering::Relaxed);
    if let Some(Ok(worker)) = bt_worker {
        let _ = worker.join();
    }
    tracing::info!("Render loop finished");
    println!("  Display: Render loop finished.");
}

/// Runs the UI Shell lifecycle against an opened framebuffer backend.
#[cfg(feature = "ui")]
pub fn run_shell_on(
    fb: &mut Framebuffer,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
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

    let (shell, maybe_bt) = init_shell(main_window, !fb.is_hardware_panel(), theme, tab);
    if let Err(e) = shell.window().show() {
        report_display(
            tracing::Level::ERROR,
            "Failed to show UI Shell MainWindow",
            &e,
        );
        return;
    }

    if let Err(e) = fb.render_frame(&window) {
        report_display(tracing::Level::ERROR, "Failed to render frame", &e);
        return;
    }
    tracing::info!("UI Shell rendered to /dev/fb0 successfully");
    println!("  Display: UI Shell rendered to /dev/fb0 successfully!");

    run_render_loop(fb, &window, &shell, maybe_bt.as_ref(), preview_secs);
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
    run_ui_display(opts.screen, opts.preview_secs, opts.theme, opts.tab);

    #[cfg(not(feature = "ui"))]
    let _ = opts;
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
