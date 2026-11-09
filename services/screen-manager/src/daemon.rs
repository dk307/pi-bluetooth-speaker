//! Central appliance daemon and display orchestration service logic.
//!
//! Provides command-line options parsing, headless detection, display lifecycle
//! execution, and Slint framebuffer rendering coordination.

// `PathBuf` backs the unconditionally compiled `-c/--config` CLI field;
// `Path` is only referenced by the ui-gated framebuffer paths.
#[cfg(feature = "ui")]
use std::os::fd::RawFd;
#[cfg(feature = "ui")]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(feature = "ui")]
use std::rc::Rc;
#[cfg(feature = "ui")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "ui")]
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(feature = "ui")]
use std::time::Instant;

#[cfg(feature = "ui")]
use crate::config::CoreConfig;
#[cfg(feature = "ui")]
use crate::ui::theme::ThemePreset;
#[cfg(feature = "ui")]
use crate::ui::{
    AppShell, BootCheckLine, BootCheckStatus, BootCheckUpdate, BootProbe, FrameTiming, Framebuffer,
    FramebufferError, MainWindow, SplashScreen,
};
#[cfg(feature = "ui")]
use slint::ComponentHandle;
#[cfg(feature = "ui")]
use slint::platform::WindowAdapter;
#[cfg(feature = "ui")]
use slint::platform::software_renderer::MinimalSoftwareWindow;
use speaker_common::diag::{
    CliParse, DEFAULT_CONFIG_PATH, env_args, next_arg_value, resolve_cli_opts,
};
use speaker_hardware::TempSensor;

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
///
/// Theme and marquee scroll rate come from the JSON configuration file
/// (see [`crate::config::CoreConfig`]), not from command-line flags.
#[derive(Debug, Clone, Default)]
pub struct CoreCliOptions {
    /// Optional preview duration in seconds.
    pub preview_secs: Option<u64>,
    /// Screen target to initialize and display.
    pub screen: ScreenTarget,
    /// Optional override of the JSON configuration file path.
    pub config: Option<PathBuf>,
    /// Navigation dock tab to open on startup (0 to 5).
    pub tab: Option<i32>,
}

/// Prints usage text for the core daemon executable.
pub fn print_usage(bin_name: &str) {
    #[cfg(feature = "ui")]
    let default_config_path = DEFAULT_CONFIG_PATH;
    // `crate::config` is ui-gated (it stores `ThemePreset`), so the daemon binary
    // built without the `ui` feature needs the path literal restated here.
    #[cfg(not(feature = "ui"))]
    let default_config_path = "/etc/speaker-core/config.json";
    println!("DietPi Touchscreen Appliance Console & Screen Manager Service");
    println!();
    println!("Usage: {bin_name} [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "  -s, --screen <appliance|splash|shell> Target screen to render (default: appliance)"
    );
    println!("  -c, --config <PATH>         JSON config file (default: {default_config_path})");
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
            "-c" | "--config" => {
                let val = next_arg_value(&mut iter, "--config")?;
                opts.config = Some(PathBuf::from(val));
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
        tracing::error!(target: "display", "{tag}: {context}: {err}");
    } else {
        tracing::warn!(target: "display", "{tag}: {context}: {err}");
    }
}

#[cfg(feature = "ui")]
fn legacy_config(theme: Option<crate::ui::ThemePreset>, marquee_fps: u32) -> CoreConfig {
    CoreConfig {
        theme,
        marquee_fps: crate::config::MarqueeFps::try_from(marquee_fps).unwrap_or_default(),
        ..CoreConfig::default()
    }
}

/// Runs the UI display loop on the default `/dev/fb0` device path.
#[cfg(feature = "ui")]
pub fn run_ui_display(
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
    marquee_fps: u32,
) {
    let config = legacy_config(theme, marquee_fps);
    run_ui_display_with_config(screen, preview_secs, tab, &config);
}

/// Runs the requested screen applying the full [`CoreConfig`] parameters.
#[cfg(feature = "ui")]
pub fn run_ui_display_with_config(
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
) {
    if headless_requested() {
        tracing::info!("Headless mode requested via SPEAKER_CORE_HEADLESS=1");
        return;
    }
    run_ui_display_on_path_with_config(
        Path::new(Framebuffer::DEFAULT_FB_PATH),
        screen,
        preview_secs,
        tab,
        config,
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
    marquee_fps: u32,
) {
    let config = legacy_config(theme, marquee_fps);
    run_ui_display_on_path_with_config(fb_path, screen, preview_secs, tab, &config);
}

/// Runs the requested screen against a specific framebuffer device path using [`CoreConfig`].
#[cfg(feature = "ui")]
pub fn run_ui_display_on_path_with_config(
    fb_path: &Path,
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
) {
    if !fb_path.exists() {
        tracing::info!("Headless/simulated environment (no /dev/fb0 detected)");
        return;
    }

    tracing::info!("Initializing Framebuffer /dev/fb0 (800x480 32bpp BGRX)");
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
            // Default appliance sequence: boot checks on the splash screen
            // (no artificial minimum display time), then the main UI shell
            // (now-playing) until stopped.
            let splash_start = Instant::now();
            run_splash_on(&mut fb, BootProbe::real(), preview_secs, config.theme);
            let remaining_preview =
                preview_secs.map(|p| p.saturating_sub(splash_start.elapsed().as_secs()));
            run_shell_on_with_config(&mut fb, remaining_preview, tab, config);
        }
        ScreenTarget::Splash => {
            run_splash_on(&mut fb, BootProbe::real(), preview_secs, config.theme);
        }
        ScreenTarget::Shell => {
            run_shell_on_with_config(&mut fb, preview_secs, tab, config);
        }
    }
}

/// Unblanks the display (logging a warning on failure) and initializes the
/// Slint software platform window on `fb`.
#[cfg(feature = "ui")]
fn unblank_and_init_platform(fb: &mut Framebuffer) -> Option<Rc<MinimalSoftwareWindow>> {
    if let Err(e) = fb.unblank() {
        report_display(tracing::Level::WARN, "Failed to unblank display", &e);
    }
    match fb.init_platform() {
        Ok(win) => Some(win),
        Err(e) => {
            report_display(tracing::Level::ERROR, "Failed to initialize platform", &e);
            None
        }
    }
}

/// Renders the initial frame of `window` to `fb`, logging the outcome and returning
/// `true` on success or `false` if the framebuffer blit failed.
#[cfg(feature = "ui")]
fn render_initial_frame(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    label: &str,
) -> bool {
    match fb.render_frame(window) {
        Ok(true) => {
            tracing::info!("{label} rendered to /dev/fb0 successfully");
            true
        }
        Ok(false) => {
            tracing::debug!("Screen buffer already current");
            true
        }
        Err(e) => {
            report_display(tracing::Level::ERROR, "Failed to render frame", &e);
            false
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
    let Some(window) = unblank_and_init_platform(fb) else {
        return;
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

    if !render_initial_frame(fb, &window, label) {
        return;
    }

    let start = Instant::now();
    let duration = preview_secs.map(Duration::from_secs);
    tracing::info!("Running UI render loop");

    while duration.is_none_or(|d| start.elapsed() < d) {
        slint::platform::update_timers_and_animations();
        if let Err(e) = fb.render_frame(&window) {
            report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    tracing::info!("Render loop finished");
}

/// Applies one boot-check update to the splash screen's single check line.
#[cfg(feature = "ui")]
fn apply_check_update(splash: &SplashScreen, update: &BootCheckUpdate) {
    let line = BootCheckLine {
        text: slint::SharedString::from(&*update.text),
        running: update.status == BootCheckStatus::Running,
        ok: update.status == BootCheckStatus::Passed,
    };
    splash.set_current_check(line);
}

/// Hard upper bound for the splash when no `preview_secs` window is given, so
/// a hung hardware probe (blocking D-Bus/I2C/sysfs call) can never stall the
/// appliance boot indefinitely.
const MAX_SPLASH_WAIT: Duration = Duration::from_secs(30);

/// Initializes and shows the [`SplashScreen`], applying an optional startup theme.
#[cfg(feature = "ui")]
fn init_splash_screen(theme: Option<crate::ui::ThemePreset>) -> Option<SplashScreen> {
    let splash = match SplashScreen::new() {
        Ok(splash) => splash,
        Err(e) => {
            report_display(
                tracing::Level::ERROR,
                "Failed to initialize Boot splash screen",
                &e,
            );
            return None;
        }
    };
    if let Some(t) = theme {
        t.apply_splash(&splash);
    }
    if let Err(e) = splash.show() {
        report_display(
            tracing::Level::WARN,
            "Failed to show Boot splash screen",
            &e,
        );
        return None;
    }
    Some(splash)
}

/// Drains all currently queued boot-check updates from `rx` into `splash`,
/// returning `true` once the sender has disconnected (all checks settled).
#[cfg(feature = "ui")]
fn drain_boot_updates(
    splash: &SplashScreen,
    rx: &std::sync::mpsc::Receiver<BootCheckUpdate>,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(update) => apply_check_update(splash, &update),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

/// Pumps splash frames until all boot checks finish, `deadline` expires, or rendering fails.
#[cfg(feature = "ui")]
fn pump_splash_frames(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    splash: &SplashScreen,
    rx: &std::sync::mpsc::Receiver<BootCheckUpdate>,
    deadline: Duration,
    mut checks_done: bool,
) -> bool {
    if !render_initial_frame(fb, window, "Boot splash screen") {
        return checks_done;
    }

    let start = Instant::now();
    tracing::info!("Running UI render loop");

    loop {
        checks_done |= drain_boot_updates(splash, rx);
        slint::platform::update_timers_and_animations();
        if let Err(e) = fb.render_frame(window) {
            report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
            break;
        }
        if checks_done || start.elapsed() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    checks_done
}

/// Joins the `boot-checks` worker when `checks_done` is true (re-raising any worker
/// panic via `resume_unwind`), or logs a warning and abandons a hung worker.
#[cfg(feature = "ui")]
pub fn finish_boot_worker(checks: Option<std::thread::JoinHandle<()>>, checks_done: bool) {
    if checks_done {
        if let Some(handle) = checks
            && let Err(err) = handle.join()
        {
            std::panic::resume_unwind(err);
        }
    } else {
        tracing::warn!("Boot checks did not finish within the splash window; abandoning worker");
    }
}

/// Runs the splash lifecycle against an opened framebuffer backend.
///
/// Boot checks run on a background thread; the render loop drains their
/// updates into the single bottom check line and exits immediately once all
/// checks have settled (no artificial minimum display time) or once the
/// deadline expires, whichever comes first. `preview_secs` bounds the splash;
/// without it `MAX_SPLASH_WAIT` applies. If the deadline expires with checks
/// still running (a hung probe), the worker is abandoned with a warning so
/// boot always proceeds; a worker panic is re-raised on the normal path only.
///
/// # Panics
/// Re-raises a panic from the `boot-checks` worker when it finished normally.
#[cfg(feature = "ui")]
pub fn run_splash_on(
    fb: &mut Framebuffer,
    probe: BootProbe,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
) {
    let Some(window) = unblank_and_init_platform(fb) else {
        return;
    };
    let Some(splash) = init_splash_screen(theme) else {
        return;
    };

    let (tx, rx) = std::sync::mpsc::channel::<BootCheckUpdate>();
    // Graceful degradation instead of a panic (M-PANIC-IS-STOP): if the worker
    // thread cannot be spawned (resource exhaustion), boot checks are skipped
    // with a warning and the appliance still boots into the shell.
    let checks = match std::thread::Builder::new()
        .name("boot-checks".into())
        .spawn(move || probe.run(&mut |update| drop(tx.send(update))))
    {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!("Failed to spawn boot-checks worker, skipping checks: {err}");
            None
        }
    };
    // Hard upper bound: without a preview window the splash may still not
    // wait forever on a hung hardware probe (blocking D-Bus/I2C calls).
    let deadline = preview_secs.map_or(MAX_SPLASH_WAIT, Duration::from_secs);
    let checks_done = pump_splash_frames(fb, &window, &splash, &rx, deadline, checks.is_none());
    finish_boot_worker(checks, checks_done);
    tracing::info!("Splash render loop finished");
}

/// Fully-initialized shell plus its Bluetooth transport wiring.
#[cfg(feature = "ui")]
#[derive(Debug)]
pub struct ShellInit {
    /// Application shell bound to the Slint window.
    pub shell: AppShell,
    /// Connected Bluetooth device handle, if D-Bus was reachable.
    pub bt: Option<speaker_bluetooth::BluetoothDevice>,
    /// Queue bridging render-thread transport callbacks to the `bt-monitor` worker.
    pub commands: speaker_bluetooth::TransportCommandQueue,
}

#[cfg(feature = "ui")]
#[must_use]
pub fn init_shell(
    main_window: MainWindow,
    mocked_device: bool,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
    marquee_fps: u32,
) -> ShellInit {
    let config = legacy_config(theme, marquee_fps);
    init_shell_with_config(main_window, mocked_device, tab, &config)
}

/// Initializes the UI shell applying all runtime parameters from [`CoreConfig`].
#[cfg(feature = "ui")]
#[must_use]
pub fn init_shell_with_config(
    main_window: MainWindow,
    mocked_device: bool,
    tab: Option<i32>,
    config: &CoreConfig,
) -> ShellInit {
    let shell = AppShell::new(main_window);
    shell.apply_config(config);
    let commands = speaker_bluetooth::TransportCommandQueue::new();
    let maybe_bt = if mocked_device {
        let (mock_bt, _) = speaker_bluetooth::BluetoothDevice::new_mocked();
        apply_initial_bt_state(&shell, &mock_bt);
        shell.wire_bluetooth_transport(&commands);
        Some(mock_bt)
    } else {
        match speaker_bluetooth::BluetoothDevice::new() {
            Ok(bt) => {
                apply_initial_bt_state(&shell, &bt);
                shell.wire_bluetooth_transport(&commands);
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
    ShellInit {
        shell,
        bt: maybe_bt,
        commands,
    }
}

#[cfg(feature = "ui")]
fn apply_initial_bt_state(shell: &AppShell, bt: &speaker_bluetooth::BluetoothDevice) {
    let preferred = match bt.playback_state() {
        Ok(Some(pb)) => {
            shell.apply_bluetooth_playback(&pb);
            pb.device_address
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query initial Bluetooth playback state");
            None
        }
    };
    match bt.link_snapshot() {
        Ok(ref snapshot) => {
            let details = crate::ui::BtLinkDetails::select(snapshot, preferred);
            if let Some(ref pcm) = details.pcm {
                shell.apply_pcm_stream(pcm);
            }
            shell.apply_bt_link_details(&details);
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query initial Bluetooth link snapshot");
        }
    }
}

/// Worker-side snapshot of Bluetooth state published to the render thread.
///
/// Each field is an `Option` delta: `None` means "no change this tick", while the
/// inner value carries the authoritative latest state. Cover art is decoded and
/// upscaled here (off the render thread) and published as a pre-rendered image.
#[cfg(feature = "ui")]
#[derive(Debug, Default)]
pub struct BtSharedUpdate {
    /// Latest playback snapshot, if it changed since the last publish.
    pub playback: Option<speaker_bluetooth::BtPlaybackState>,
    /// Active PCM stream, if it changed since the last publish.
    pub pcm: Option<speaker_bluetooth::BlueAlsaPcm>,
    /// Bluetooth connection details for the details screen, if they changed.
    pub details: Option<crate::ui::BtLinkDetails>,
    /// Cover art delta: outer `Some` = cover state changed; inner `Some` = decoded
    /// pixel buffer to display, inner `None` = clear the displayed cover art.
    /// Carried as a `SharedPixelBuffer` (which is `Send + Sync`) rather than a
    /// `slint::Image` (which is not `Send`): the expensive decode/upscale happens
    /// on the worker, and the render thread only wraps the buffer in an image.
    // The three cover states (unchanged / clear / show buffer) are intentional.
    pub cover: Option<Option<CoverPixelBuffer>>,
}

/// Alias for the cross-thread pixel buffer backing cover art.
#[cfg(feature = "ui")]
pub type CoverPixelBuffer = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// Interval between retries of a failed cover-art decode for the same path.
///
/// A failed decode must not block a future successful retry (the file may still
/// be written by OBEX), but re-decoding on every monitor tick would repeatedly
/// pay a full Lanczos3 resize on the worker for a persistently broken file.
#[cfg(feature = "ui")]
pub const COVER_DECODE_RETRY: Duration = Duration::from_secs(10);
/// Target side length (px) for upscaled cover-art cache images.
#[cfg(feature = "ui")]
pub const TARGET_COVER_SIZE: u32 = 480;

/// Peer status can lag a local transport command by hundreds of milliseconds.
///
/// A post-command snapshot may still carry the pre-command playing state. This
/// window bounds how long such an unchanged snapshot is suppressed to keep the
/// shell's optimistic flip from reverting.
#[cfg(feature = "ui")]
pub const BT_COMMAND_SETTLE: Duration = Duration::from_millis(750);

/// Tracks the cover-art dedup key and when the current path last failed to
/// decode, so the retry backoff in [`COVER_DECODE_RETRY`] can gate re-attempts.
#[cfg(feature = "ui")]
#[derive(Debug, Default)]
pub struct CoverState {
    /// Path whose art is currently reflected in the published state (or the
    /// path of the last failed decode attempt while awaiting its retry).
    pub path: Option<Box<str>>,
    /// Instant of the most recent failed decode attempt, if the last attempt
    /// for [`CoverState::path`] failed; `None` once a decode succeeded.
    pub failed_since: Option<Instant>,
}

/// Optimistic-UI convergence state for a pending transport command.
///
/// How the optimistic UI was updated ahead of the authoritative state.
///
/// Play/pause moves the status indicator; seek scrubs the progress bar. Both
/// are applied optimistically by the shell before the peer confirms, so both
/// can be reverted by a stale pre-command snapshot.
#[cfg(feature = "ui")]
#[derive(Debug)]
pub enum SettleKind {
    /// Suppress snapshots still reporting the pre-command playing state.
    Toggle {
        /// Playing state observed before the optimistic flip was applied.
        pre_playing: bool,
    },
    /// Suppress snapshots still reporting the pre-seek AVRCP position.
    Seek {
        /// Target position (ms) the shell scrubbed the progress bar to.
        target_ms: u32,
    },
    /// Suppress PCM snapshots that have not yet converged to the target volume or mute state.
    Volume {
        /// Target volume percentage (`0..=100`), if a volume change was issued.
        target_pct: Option<u8>,
        /// Target mute state expected after the command.
        target_muted: bool,
    },
}

/// A settle window during which conflicting snapshots are suppressed.
///
/// The window expires after [`BT_COMMAND_SETTLE`] and the expiry
/// force-publishes one authoritative snapshot, so a failed command always
/// reconciles.
#[cfg(feature = "ui")]
#[derive(Debug)]
pub struct SettleWindow {
    /// Which optimistic update this window protects.
    pub kind: SettleKind,
    /// When the command was issued; the window expires after
    /// [`BT_COMMAND_SETTLE`].
    pub started: Instant,
}

#[cfg(feature = "ui")]
impl SettleWindow {
    /// Reports whether `pb` still conflicts with the optimistic state this
    /// window protects (and is therefore withheld from the shell).
    #[must_use]
    pub fn suppresses(&self, pb: &speaker_bluetooth::BtPlaybackState) -> bool {
        if self.started.elapsed() >= BT_COMMAND_SETTLE {
            return false;
        }
        match &self.kind {
            SettleKind::Toggle { pre_playing } => pb.is_playing() == *pre_playing,
            SettleKind::Seek { target_ms } => {
                (i64::from(pb.position_ms) - i64::from(*target_ms)).abs()
                    > i64::from(BT_SEEK_STALE_TOLERANCE_MS)
            }
            SettleKind::Volume { .. } => false,
        }
    }

    /// Reports whether `pcm` still conflicts with an optimistic volume or mute
    /// update protected by this settle window.
    #[must_use]
    pub fn suppresses_pcm(&self, pcm: &speaker_bluetooth::BlueAlsaPcm) -> bool {
        if self.started.elapsed() >= BT_COMMAND_SETTLE {
            return false;
        }
        match &self.kind {
            SettleKind::Volume {
                target_pct,
                target_muted,
            } => {
                if pcm.muted != *target_muted {
                    return true;
                }
                if let (Some(target), Some(actual)) = (target_pct, pcm.volume_percent) {
                    actual.abs_diff(*target) > 1
                } else {
                    false
                }
            }
            SettleKind::Toggle { .. } | SettleKind::Seek { .. } => false,
        }
    }
}

/// Maximum AVRCP position distance (ms) from a seek target that is treated as
/// caught up; larger distances during a settle window are stale pre-seek
/// snapshots and are suppressed. Tiny seeks within this tolerance publish
/// immediately (a snap-back is visually indistinguishable).
#[cfg(feature = "ui")]
const BT_SEEK_STALE_TOLERANCE_MS: u32 = 800;

/// Decodes cover art (upscaling first) into a cross-thread pixel buffer.
///
/// Runs on the `bt-monitor` worker thread: small AVRCP cover art is upscaled to
/// panel resolution with Lanczos3 (cached next to the source file), decoded to
/// RGBA, and returned as a [`CoverPixelBuffer`] so the render thread never pays
/// for the decode or the resample during a frame.
#[cfg(feature = "ui")]
pub fn decode_cover_buffer(path: &str) -> Option<CoverPixelBuffer> {
    tracing::debug!("COVER_DECODE {:?}", path);
    let src = Path::new(path);
    let file_name = src.file_name()?;
    let mut upscaled_name = file_name.to_os_string();
    upscaled_name.push(".480.jpg");
    let out_path = src.with_file_name(upscaled_name);

    // A corrupt cache file (truncated by a power loss mid-write) must not
    // poison the path forever: remove it and regenerate from the intact
    // source so the next decode retry succeeds.
    let img = match image::open(&out_path) {
        Ok(img) => img,
        Err(e) => {
            if out_path.is_file() {
                tracing::warn!(error = %e, "corrupt cover cache {}; regenerating", out_path.display());
                let _ = std::fs::remove_file(&out_path);
            }
            let img = image::open(src).ok()?;
            if img.width() >= TARGET_COVER_SIZE || img.height() >= TARGET_COVER_SIZE {
                img
            } else {
                let upscaled = img.resize_exact(
                    TARGET_COVER_SIZE,
                    TARGET_COVER_SIZE,
                    image::imageops::FilterType::Lanczos3,
                );
                // A failed cache write must not discard the already-decoded
                // image; the in-memory buffer still renders this frame.
                if let Err(e) = upscaled.save_with_format(&out_path, image::ImageFormat::Jpeg) {
                    tracing::warn!(error = %e, "cover cache write failed for {}", out_path.display());
                }
                upscaled
            }
        }
    };

    let rgba = img.to_rgba8();
    Some(CoverPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}

/// Queries the Bluetooth device once and stages playback/cover deltas into `update`.
///
/// On `Ok(None)` publishes the connection placeholder so stale optimistic UI
/// state is cleared; on query errors keeps the last published state. Cover art
/// changes are decoded here (off the render thread); a failed decode keeps the
/// previous art and is retried after [`COVER_DECODE_RETRY`] elapses.
#[cfg(feature = "ui")]
pub fn publish_playback_state(
    bt: &speaker_bluetooth::BluetoothDevice,
    update: &mut BtSharedUpdate,
    last_pb: &mut Option<speaker_bluetooth::BtPlaybackState>,
    cover: &mut CoverState,
    settle: &mut Option<SettleWindow>,
) {
    match bt.playback_state() {
        Ok(Some(ref pb)) => {
            // Force-publish one authoritative snapshot after the settle window
            // expires: if the command failed silently (or the peer never changed
            // status), the shell's optimistic flip must be reconciled instead of
            // diverging forever.
            if settle
                .as_ref()
                .is_some_and(|w| w.started.elapsed() >= BT_COMMAND_SETTLE)
            {
                *settle = None;
                *last_pb = None;
            }
            let settling = settle.as_ref().is_some_and(|w| w.suppresses(pb));
            if !settling && last_pb.as_ref() != Some(pb) {
                update.playback = Some(pb.clone());
                *last_pb = Some(pb.clone());
            }
            // Cover decoding is independent of playback-snapshot changes so a
            // failed decode is still retried after the backoff elapses while
            // the device is paused (identical snapshots would otherwise starve
            // the retry). A changed path always decodes; an unchanged path is
            // re-attempted only once the retry backoff has passed, so a
            // persistently broken file cannot trigger a full Lanczos3 resize
            // on every tick while a transient failure is still retried.
            let cover_path = pb.cover_art_path.clone();
            let retry_due = cover
                .failed_since
                .is_some_and(|t| t.elapsed() >= COVER_DECODE_RETRY);
            if (cover.path != cover_path || retry_due)
                && let Some(path) = cover_path.as_ref()
            {
                if let Some(buffer) = decode_cover_buffer(path) {
                    update.cover = Some(Some(buffer));
                    cover.path = cover_path;
                    cover.failed_since = None;
                } else {
                    tracing::warn!("cover decode failed for {path}");
                    // Keep the previously published art (never blank it)
                    // and schedule a retry after the backoff.
                    cover.path = cover_path;
                    cover.failed_since = Some(Instant::now());
                }
            }
            // No cached file (handle-less track or fetch pending) keeps the
            // previously published art: a snapshot never blanks the cover;
            // only a device disconnect clears it explicitly.
        }
        Ok(None) => {
            let placeholder = speaker_bluetooth::BtPlaybackState::default();
            if last_pb.as_ref() != Some(&placeholder) {
                if cover.path.is_some() {
                    update.cover = Some(None);
                    *cover = CoverState::default();
                }
                update.playback = Some(placeholder.clone());
                *last_pb = Some(placeholder);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "playback_state query failed; keeping last state");
        }
    }
}

/// Signal-driven keep-alive interval when idle or paused.
#[cfg(feature = "ui")]
pub const BT_KEEP_ALIVE: Duration = Duration::from_secs(1);
/// Refresh cadence while playing, matching the old adaptive poll interval.
#[cfg(feature = "ui")]
pub const BT_PLAYING_POLL: Duration = Duration::from_millis(250);
/// Maximum wait slice per signal poll so queued transport commands are drained promptly.
#[cfg(feature = "ui")]
const BT_WAIT_SLICE: Duration = Duration::from_millis(100);

/// Host-testable state machine for the `bt-monitor` worker loop.
#[cfg(feature = "ui")]
#[derive(Debug, Default)]
pub struct BtMonitorState {
    last_pb: Option<speaker_bluetooth::BtPlaybackState>,
    last_pcms: Option<Box<[speaker_bluetooth::BlueAlsaPcm]>>,
    last_details: Option<crate::ui::BtLinkDetails>,
    cover: CoverState,
    settle: Option<SettleWindow>,
    pcm_settle: Option<SettleWindow>,
}

#[cfg(all(feature = "ui", feature = "test-util"))]
impl BtMonitorState {
    /// Returns `true` if a playback snapshot is cached.
    #[must_use]
    pub fn has_playback(&self) -> bool {
        self.last_pb.is_some()
    }

    /// Returns `true` if PCM device snapshots are cached.
    #[must_use]
    pub fn has_pcms(&self) -> bool {
        self.last_pcms.is_some()
    }

    /// Returns cached Bluetooth link details.
    #[must_use]
    pub fn last_details(&self) -> Option<&crate::ui::BtLinkDetails> {
        self.last_details.as_ref()
    }

    /// Returns the cached cover art path.
    #[must_use]
    pub fn cover_path(&self) -> Option<&str> {
        self.cover.path.as_deref()
    }

    /// Returns the active settle window, if armed.
    #[must_use]
    pub fn settle(&self) -> Option<&SettleWindow> {
        self.settle.as_ref()
    }
}

#[cfg(feature = "ui")]
impl BtMonitorState {
    /// Arms the optimistic-UI [`SettleWindow`] when a command batch contains a
    /// `TogglePlayPause`, `Seek`, `SetVolume`, or `SetMute` command.
    fn arm_settle_for_batch(&mut self, drained: &[speaker_bluetooth::TransportCommand]) {
        // Play/pause flips the shell's status indicator and seek scrubs the
        // progress bar, both optimistically ahead of peer confirmation. A
        // snapshot fetched immediately after either command can still carry
        // the pre-command state (AVRCP lags by hundreds of milliseconds), so
        // conflicting snapshots are suppressed until the peer catches up. The
        // window is only re-armed on a fresh optimistic command so an
        // in-flight window survives unrelated command batches.
        let toggle = drained
            .iter()
            .any(|c| matches!(c, speaker_bluetooth::TransportCommand::TogglePlayPause));
        let seek = drained.iter().find_map(|c| match c {
            speaker_bluetooth::TransportCommand::Seek(pos) => Some(*pos),
            _ => None,
        });
        if toggle || seek.is_some() {
            // A toggle arm wins when both land in one batch: the status flip is
            // the more visible artifact, and a same-batch seek is rare.
            self.settle = self.last_pb.as_ref().map(|pre| SettleWindow {
                kind: if toggle {
                    SettleKind::Toggle {
                        pre_playing: pre.is_playing(),
                    }
                } else {
                    SettleKind::Seek {
                        target_ms: seek.unwrap_or_default(),
                    }
                },
                started: Instant::now(),
            });
        }

        let mut target_pct = None;
        let mut target_muted = None;
        for c in drained {
            match c {
                speaker_bluetooth::TransportCommand::SetVolume(pct) => {
                    target_pct = Some(*pct);
                    target_muted = Some(false);
                }
                speaker_bluetooth::TransportCommand::SetMute(muted) => {
                    target_muted = Some(*muted);
                }
                _ => {}
            }
        }
        if target_pct.is_some() || target_muted.is_some() {
            self.pcm_settle = Some(SettleWindow {
                kind: SettleKind::Volume {
                    target_pct,
                    target_muted: target_muted.unwrap_or(false),
                },
                started: Instant::now(),
            });
        }
    }

    /// Executes a single [`speaker_bluetooth::TransportCommand`] against `bt`,
    /// logging a warning on failure.
    pub fn execute_command(
        bt: &speaker_bluetooth::BluetoothDevice,
        command: speaker_bluetooth::TransportCommand,
    ) {
        // A failed command is logged (never silently dropped) so operators can
        // diagnose why the authoritative state failed to reconcile the
        // optimistic UI flip; the next authoritative publish corrects the UI.
        if let Err(e) = match command {
            speaker_bluetooth::TransportCommand::TogglePlayPause => bt.player_toggle().map(|_| ()),
            speaker_bluetooth::TransportCommand::NextTrack => bt.player_next(),
            speaker_bluetooth::TransportCommand::PreviousTrack => bt.player_previous(),
            speaker_bluetooth::TransportCommand::Seek(pos) => bt.player_seek(pos),
            speaker_bluetooth::TransportCommand::SetVolume(pct) => bt.set_volume(pct),
            speaker_bluetooth::TransportCommand::SetMute(muted) => bt.set_mute(muted),
        } {
            tracing::warn!(error = %e, "Bluetooth transport command failed");
        }
    }

    /// Queries one lightweight link snapshot and stages PCM and connection-detail
    /// changes into `update`.
    pub fn publish_link_state(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        update: &mut BtSharedUpdate,
    ) {
        if self
            .pcm_settle
            .as_ref()
            .is_some_and(|w| w.started.elapsed() >= BT_COMMAND_SETTLE)
        {
            self.pcm_settle = None;
            self.last_pcms = None;
        }
        let snapshot = match bt.link_snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to query Bluetooth link snapshot");
                return;
            }
        };
        if self.last_pcms.as_ref() != Some(&snapshot.pcms) {
            if let Some(active) = speaker_bluetooth::select_active_pcm(&snapshot.pcms) {
                let settling = self
                    .pcm_settle
                    .as_ref()
                    .is_some_and(|w| w.suppresses_pcm(active));
                if !settling {
                    update.pcm = Some(active.clone());
                    self.last_pcms = Some(snapshot.pcms.clone());
                }
            } else {
                self.last_pcms = Some(snapshot.pcms.clone());
            }
        }
        let preferred = self.last_pb.as_ref().and_then(|pb| pb.device_address);
        let details = crate::ui::BtLinkDetails::select(&snapshot, preferred);
        if self.last_details.as_ref() != Some(&details) {
            update.details = Some(details.clone());
            self.last_details = Some(details);
        }
    }

    /// Resets dedup state when a new command arrives mid-query so the next tick
    /// performs a full authoritative publish after executing the fresh command.
    pub fn reset_dedup(&mut self) {
        self.last_pb = None;
        self.cover.path = None;
        self.last_pcms = None;
        self.last_details = None;
    }

    /// Merges non-empty delta fields from `update` into `shared`, recovering from
    /// a poisoned mutex guard.
    pub fn commit_update(shared: &Mutex<BtSharedUpdate>, update: BtSharedUpdate) {
        if update.playback.is_some()
            || update.pcm.is_some()
            || update.details.is_some()
            || update.cover.is_some()
        {
            // A poisoned mutex is recovered (not silently dropped): the snapshot
            // was already computed and the dedup state advanced, so losing it
            // here would skip that state until the device changes again.
            let mut lock = shared.lock().unwrap_or_else(|p| {
                tracing::warn!("BtSharedUpdate mutex poisoned; recovering inner state");
                p.into_inner()
            });
            if update.playback.is_some() {
                lock.playback = update.playback;
            }
            if update.pcm.is_some() {
                lock.pcm = update.pcm;
            }
            if update.details.is_some() {
                lock.details = update.details;
            }
            if update.cover.is_some() {
                lock.cover = update.cover;
            }
        }
    }

    /// Runs one command-drain + state-query + shared-publish cycle, returning
    /// `true` if a full snapshot poll completed or `false` if a newly queued
    /// command interrupted the tick and should be drained immediately.
    pub fn poll_tick(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        shared: &Mutex<BtSharedUpdate>,
        commands: &speaker_bluetooth::TransportCommandQueue,
    ) -> bool {
        let drained = commands.drain();
        self.arm_settle_for_batch(&drained);
        for command in drained {
            Self::execute_command(bt, command);
        }

        // A command queued between the drain and the snapshot query would make
        // the query execute a stale pre-command publish; skip the query so the
        // fresh command is executed first and the next tick reads post-command
        // state.
        if !commands.is_empty() {
            return false;
        }

        let mut update = BtSharedUpdate::default();
        // Publish the authoritative state; transient D-Bus errors keep the last
        // published state instead of flashing the placeholder over live playback.
        publish_playback_state(
            bt,
            &mut update,
            &mut self.last_pb,
            &mut self.cover,
            &mut self.settle,
        );
        self.publish_link_state(bt, &mut update);

        // A command that landed while the blocking snapshot query ran must not
        // merge a pre-command snapshot. The query already advanced the dedup
        // state, so it is reset here to force a full re-publish on the next
        // tick (after the fresh commands are executed).
        if !commands.is_empty() {
            self.reset_dedup();
            return false;
        }

        Self::commit_update(shared, update);
        true
    }

    /// Returns the adaptive keep-alive poll cadence based on whether playback is active.
    #[must_use]
    pub fn keep_alive_interval(&self) -> Duration {
        // BlueZ does not reliably re-emit AVRCP Position during playback, so the
        // keep-alive doubles as the progress refresh: shorten it while playing
        // so the position bar advances in ~250 ms steps instead of 1 s jumps.
        if self
            .last_pb
            .as_ref()
            .is_some_and(speaker_bluetooth::BtPlaybackState::is_playing)
        {
            BT_PLAYING_POLL
        } else {
            BT_KEEP_ALIVE
        }
    }
}

/// Runs on the dedicated `bt-monitor` thread until `running` is cleared.
/// Panics here are re-raised by the render loop's `join()` so a dead monitor
/// fails the daemon instead of leaving the playback UI silently stale.
#[cfg(feature = "ui")]
fn bt_monitor_loop(
    bt: &speaker_bluetooth::BluetoothDevice,
    shared: &Mutex<BtSharedUpdate>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    running: &AtomicBool,
) {
    let mut state = BtMonitorState::default();

    while running.load(Ordering::Relaxed) {
        if !state.poll_tick(bt, shared, commands) {
            continue;
        }

        let deadline = Instant::now() + state.keep_alive_interval();
        while running.load(Ordering::Relaxed) && Instant::now() < deadline {
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(BT_WAIT_SLICE);
            // A `true` return means a PropertiesChanged signal arrived: breaking
            // out re-polls immediately so track/status/position changes surface
            // without waiting for the remaining keep-alive window.
            if bt.wait_for_property_change(slice) || !commands.is_empty() {
                break;
            }
        }
    }
}

#[cfg(feature = "ui")]
pub fn run_render_loop(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    shell: &AppShell,
    maybe_bt: Option<&speaker_bluetooth::BluetoothDevice>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    preview_secs: Option<u64>,
) {
    let touch_dev = crate::ui::TouchInputDevice::auto_discover().ok();
    let encoder_dev = fb
        .is_hardware_panel()
        .then(|| match speaker_hardware::EncoderDevice::new() {
            Ok(enc) => Some(enc),
            Err(e) => {
                report_display(
                    tracing::Level::WARN,
                    "Failed to initialize EC12 rotary encoder for volume control",
                    &e,
                );
                None
            }
        })
        .flatten();
    run_render_loop_with_devices(
        fb,
        window,
        shell,
        maybe_bt,
        commands,
        preview_secs,
        touch_dev,
        encoder_dev.as_ref(),
    );
}

/// Runs the UI render loop with explicit touch input device injection.
#[cfg(feature = "ui")]
pub fn run_render_loop_with_input(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    shell: &AppShell,
    maybe_bt: Option<&speaker_bluetooth::BluetoothDevice>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    preview_secs: Option<u64>,
    touch_dev: Option<crate::ui::TouchInputDevice>,
) {
    run_render_loop_with_devices(
        fb,
        window,
        shell,
        maybe_bt,
        commands,
        preview_secs,
        touch_dev,
        None,
    );
}

#[cfg(feature = "ui")]
fn poll_encoder_input(
    encoder_dev: Option<&speaker_hardware::EncoderDevice>,
    encoder_tracker: &mut crate::ui::EncoderVolumeTracker,
    shell: &AppShell,
    window: &Rc<MinimalSoftwareWindow>,
    encoder_warned: &mut bool,
) {
    let Some(enc) = encoder_dev else { return };
    match encoder_tracker.poll_and_apply(enc, shell) {
        Ok(changed) => {
            *encoder_warned = false;
            if changed {
                window.window().request_redraw();
            }
        }
        Err(e) => {
            if *encoder_warned {
                tracing::debug!(error = %e, "Rotary encoder poll failed (repeating)");
            } else {
                tracing::warn!(error = %e, "Rotary encoder poll failed");
                *encoder_warned = true;
            }
        }
    }
}

/// Runs the UI render loop with explicit touch and rotary encoder device injection.
#[cfg(feature = "ui")]
#[expect(
    clippy::too_many_arguments,
    reason = "explicit peripheral injection seam for hermetic touch + encoder integration testing"
)]
pub fn run_render_loop_with_devices(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    shell: &AppShell,
    maybe_bt: Option<&speaker_bluetooth::BluetoothDevice>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    preview_secs: Option<u64>,
    mut touch_dev: Option<crate::ui::TouchInputDevice>,
    encoder_dev: Option<&speaker_hardware::EncoderDevice>,
) {
    let start = Instant::now();
    let duration = preview_secs.map(Duration::from_secs);
    tracing::info!("Running UI render loop");

    let bg_running = Arc::new(AtomicBool::new(true));
    let bg_running_worker = Arc::clone(&bg_running);
    let shared_state: Arc<Mutex<BtSharedUpdate>> = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let shared_state_worker = Arc::clone(&shared_state);
    let commands_worker = commands.clone();

    let bt_worker = maybe_bt.map(|bt| {
        let bt = bt.clone();
        std::thread::Builder::new()
            .name("bt-monitor".into())
            .spawn(move || {
                bt_monitor_loop(
                    &bt,
                    &shared_state_worker,
                    &commands_worker,
                    &bg_running_worker,
                );
            })
    });

    let mut loop_timing = FrameLoopTiming::default();
    let mut overlay_theme: Option<ThemePreset> = None;
    let mut encoder_tracker = crate::ui::EncoderVolumeTracker::new();
    let mut encoder_warned = false;
    while duration.is_none_or(|d| start.elapsed() < d) {
        let frame_start = Instant::now();

        // Process and dispatch pending touch events directly on the UI render thread
        if let Some(ref mut touch) = touch_dev {
            match touch.dispatch_to_window(window) {
                Ok(count) => {
                    if count > 0 {
                        window.window().request_redraw();
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Touch dispatch failed");
                }
            }
        }

        // Poll physical/mocked EC12 rotary encoder detents and switch presses
        poll_encoder_input(
            encoder_dev,
            &mut encoder_tracker,
            shell,
            window,
            &mut encoder_warned,
        );

        if let Err(e) = sync_marquee_overlay(fb, shell, window, &mut overlay_theme) {
            report_display(tracing::Level::ERROR, "Marquee overlay sync failed", &e);
            break;
        }

        apply_shared_bt_state(shell, &shared_state);

        // No unconditional `request_redraw()` here: the marquee timers and real Slint
        // animations mark the window dirty themselves, so the loop idles when the UI is
        // static instead of burning a full render pass every 16.7 ms frame.
        let is_animating = shell.window().get_is_animating();

        slint::platform::update_timers_and_animations();
        let did_render = match fb.render_frame(window) {
            Ok(rendered) => rendered,
            Err(e) => {
                report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
                break;
            }
        };
        if let Some(timing) = fb.last_frame_timing() {
            loop_timing.record(&timing);
        }

        let has_animations = window.window().has_active_animations();
        if did_render || has_animations || is_animating {
            // Target precise 60 FPS (16,667 µs per frame) for buttery smooth, tear-free animations
            const TARGET_FRAME_TIME: Duration = Duration::from_micros(16_667);
            let elapsed = frame_start.elapsed();
            if let Some(remaining) = TARGET_FRAME_TIME.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
        } else {
            let touch_fd = touch_dev
                .as_ref()
                .and_then(crate::ui::TouchInputDevice::as_raw_fd);
            idle_sleep(touch_fd);
        }
    }

    loop_timing.report();
    bg_running.store(false, Ordering::Relaxed);
    if let Some(worker) = bt_worker {
        // Propagate a worker panic to this thread: a crashed bt-monitor must fail
        // the daemon (fail-fast) rather than leave playback state silently stale.
        match worker {
            Ok(handle) => {
                if let Err(panic) = handle.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(e) => {
                report_display(
                    tracing::Level::WARN,
                    "bt-monitor thread could not be started",
                    &e,
                );
            }
        }
    }
    tracing::info!("Render loop finished");
}

/// Applies queued Bluetooth playback/PCM/cover updates from the worker to the shell.
#[cfg(feature = "ui")]
fn apply_shared_bt_state(shell: &AppShell, shared_state: &Arc<Mutex<BtSharedUpdate>>) {
    // A poisoned guard is recovered so a panicked worker cannot permanently
    // freeze the UI; a busy lock is simply skipped until the next frame.
    let lock = match shared_state.try_lock() {
        Ok(lock) => Some(lock),
        Err(std::sync::TryLockError::Poisoned(err)) => {
            tracing::warn!("BtSharedUpdate mutex poisoned; recovering inner state");
            Some(err.into_inner())
        }
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    if let Some(mut lock) = lock {
        if let Some(pb) = lock.playback.take() {
            shell.apply_bluetooth_playback(&pb);
        }
        if let Some(pcm) = lock.pcm.take() {
            shell.apply_pcm_stream(&pcm);
        }
        if let Some(details) = lock.details.take() {
            shell.apply_bt_link_details(&details);
        }
        if let Some(cover) = lock.cover.take() {
            match cover {
                // Cheap on the render thread: just wraps the pre-decoded buffer
                // into an image handle; the expensive decode happened on the worker.
                Some(buffer) => shell.set_cover_image(slint::Image::from_rgba8(buffer)),
                None => shell.set_has_cover_art(false),
            }
        }
    }
}

/// Keeps the Rust-side marquee composer synchronized with the UI shell.
///
/// Captures a one-frame clean row background snapshot on first use and after every theme
/// change (strips render nothing for that frame), then refreshes the per-iteration scroll
/// specifications read from the shared `MarqueeCompose` global.
///
/// # Errors
///
/// Propagates [`FramebufferError`] from the capture render or region snapshots.
#[cfg(feature = "ui")]
fn sync_marquee_overlay(
    fb: &mut Framebuffer,
    shell: &AppShell,
    window: &MinimalSoftwareWindow,
    overlay_theme: &mut Option<ThemePreset>,
) -> Result<(), FramebufferError> {
    if shell.should_suppress_marquee() {
        // Marquee rows exist on the Now Playing tab only and are suppressed while the
        // Volume HUD overlay is visible; re-arming (capture redraw) happens when the
        // Now Playing tab is unobstructed again.
        fb.set_marquee_rows(Vec::new());
        *overlay_theme = None;
        return Ok(());
    }
    let theme = shell.current_theme();
    if !fb.marquee_overlay_ready() || overlay_theme.as_ref() != Some(&theme) {
        shell.set_marquee_strips_visible(false);
        // The capture frame MUST repaint the whole window: with partial rendering the
        // hidden strips leave no dirty region, so the shadow buffer would still hold
        // previously composed strip pixels and they would be baked into the captured
        // "background" (visible as ghost/double text under the strip).
        // The composer must also be inert during the capture render: render_frame
        // composes the still-installed (old) specs AFTER the full redraw, which would
        // write the old strip straight back into the snapshot region.
        fb.set_marquee_rows(Vec::new());
        fb.render_frame_full_redraw(window)?;
        let rects = shell.marquee_geometry();
        let mut backgrounds = Vec::with_capacity(rects.len());
        for (x, y, w, h) in &rects {
            backgrounds.push(fb.capture_region(*x, *y, *w, *h)?);
        }
        // Composition must be enabled before the specs are built: the specs are gated on
        // the compose flag, and rows must exist before their backgrounds are installed.
        shell.set_marquee_compose_in_rust(true);
        fb.set_marquee_rows(shell.marquee_row_specs());
        fb.install_marquee_backgrounds(backgrounds);
        shell.set_marquee_strips_visible(true);
        *overlay_theme = Some(theme);
    }
    fb.set_marquee_rows(shell.marquee_row_specs());
    Ok(())
}

/// Adaptive power-saving sleep when the UI is completely static (0% CPU at idle).
///
/// If a touch device file descriptor is present, polls it with a bounded timeout
/// so physical touches wake the render loop immediately (<20 ms) rather than sleeping.
#[cfg(feature = "ui")]
pub fn idle_sleep(touch_fd: Option<RawFd>) {
    let timeout = slint::platform::duration_until_next_timer_update()
        .unwrap_or(Duration::from_millis(50))
        .clamp(Duration::from_millis(10), Duration::from_millis(50));

    let should_sleep = match touch_fd {
        Some(fd) => {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll with 1 descriptor and bounded timeout is safe.
            let (ret, last_err) = unsafe {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "bounded 10..=50 ms fits in c_int"
                )]
                let r = libc::poll(&raw mut pfd, 1, timeout.as_millis() as libc::c_int);
                let err = (r < 0).then(std::io::Error::last_os_error);
                (r, err)
            };
            // Sleep on unrecoverable poll errors (ret < 0, excluding EINTR) or if hangup/error revents are signaled
            // without readable data, preventing a 100% CPU busy-spin while preserving pending touch events and prompt EINTR retry.
            // When poll times out (ret == 0), poll has already blocked for the full duration, avoiding double-sleep.
            (ret < 0
                && last_err.as_ref().and_then(std::io::Error::raw_os_error) != Some(libc::EINTR))
                || ((pfd.revents & libc::POLLIN) == 0
                    && (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0))
        }
        None => true,
    };

    if should_sleep {
        std::thread::sleep(timeout);
    }
}

/// Aggregated frame phase timings for render-loop CPU diagnostics.
#[derive(Debug, Default)]
#[cfg(feature = "ui")]
struct FrameLoopTiming {
    frames: u64,
    rendered: u64,
    draw_us: u64,
    vsync_us: u64,
    blit_us: u64,
    compose_us: u64,
    max_draw_us: u64,
    dirty_px: u64,
    max_dirty_px: u64,
    max_dirty_box: (u32, u32, u32, u32),
}

#[cfg(feature = "ui")]
impl FrameLoopTiming {
    fn record(&mut self, timing: &FrameTiming) {
        self.frames += 1;
        let active = timing.draw_us > 0 || timing.blit_us > 0;
        if !active {
            return;
        }
        self.rendered += 1;
        self.draw_us += timing.draw_us;
        self.vsync_us += timing.vsync_us;
        self.blit_us += timing.blit_us;
        self.compose_us += timing.compose_us;
        self.max_draw_us = self.max_draw_us.max(timing.draw_us);
        self.dirty_px += timing.dirty_px;
        if timing.dirty_px > self.max_dirty_px {
            self.max_dirty_px = timing.dirty_px;
            self.max_dirty_box = timing.dirty_box;
        }
        if self.rendered.is_multiple_of(600) {
            self.report();
        }
    }

    fn report(&self) {
        let rendered = self.rendered.max(1);
        let max_dirty_box = format!("{:?}", self.max_dirty_box);
        tracing::debug!(
            "Frame timing: {frames} loop iterations, {rendered} rendered | avg draw {draw} us, avg compose {compose} us, avg vsync {vsync} us (blocking), avg blit {blit} us, max draw {max_draw} us | avg dirty {dirty} px, max dirty {max_dirty} px at {max_dirty_box}",
            frames = self.frames,
            rendered = self.rendered,
            draw = self.draw_us / rendered,
            compose = self.compose_us / rendered,
            vsync = self.vsync_us / rendered,
            blit = self.blit_us / rendered,
            max_draw = self.max_draw_us,
            dirty = self.dirty_px / rendered,
            max_dirty = self.max_dirty_px,
            max_dirty_box = max_dirty_box,
        );
    }
}

/// Runs the UI Shell lifecycle against an opened framebuffer backend.
#[cfg(feature = "ui")]
pub fn run_shell_on(
    fb: &mut Framebuffer,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
    marquee_fps: u32,
) {
    let config = legacy_config(theme, marquee_fps);
    run_shell_on_with_config(fb, preview_secs, tab, &config);
}

/// Runs the UI Shell lifecycle applying the full [`CoreConfig`] parameters.
#[cfg(feature = "ui")]
pub fn run_shell_on_with_config(
    fb: &mut Framebuffer,
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
) {
    let Some(window) = unblank_and_init_platform(fb) else {
        return;
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

    let ShellInit {
        shell,
        bt,
        commands,
    } = init_shell_with_config(main_window, !fb.is_hardware_panel(), tab, config);
    if let Err(e) = shell.window().show() {
        report_display(
            tracing::Level::ERROR,
            "Failed to show UI Shell MainWindow",
            &e,
        );
        return;
    }

    if !render_initial_frame(fb, &window, "UI Shell") {
        return;
    }

    run_render_loop(fb, &window, &shell, bt.as_ref(), &commands, preview_secs);
}

/// Executes the core appliance daemon with parsed options.
#[must_use]
pub fn run_app(opts: &CoreCliOptions) -> ExitCode {
    let temp_sensor = TempSensor::new();

    tracing::info!(
        target = "arm-cortex-a53",
        "DietPi Screen Manager Service running on Raspberry Pi 3 (aarch64)"
    );
    tracing::info!(
        thermal_sensor = %temp_sensor.gpio_info(),
        "Screen manager service started"
    );

    #[cfg(feature = "ui")]
    {
        let path = CoreConfig::resolve_path(opts.config.as_deref());
        match CoreConfig::load(&path) {
            Ok(cfg) => {
                if let Some(t) = cfg.theme {
                    tracing::info!("Configured startup theme: {t}");
                }
                run_ui_display_with_config(opts.screen, opts.preview_secs, opts.tab, &cfg);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load config {}", path.display());
                // Operator-facing rejection: keep it backtrace-free even under
                // RUST_BACKTRACE=1 (full Display would append the stack trace).
                return ExitCode::FAILURE;
            }
        }
    }

    #[cfg(not(feature = "ui"))]
    let _ = &opts;
    ExitCode::SUCCESS
}

/// CLI main entrypoint for the `core` binary.
///
/// Installs the global `tracing` subscriber before any other work so startup
/// events (config load, platform init) are captured; verbosity is controlled
/// by `RUST_LOG` (default `info`, see [`speaker_common::logging`]).
pub fn main() -> ExitCode {
    speaker_common::logging::init_daemon_logging();
    match resolve_cli_opts(
        parse_cli_args_from(env_args()),
        "screen-manager",
        print_usage,
    ) {
        Ok(opts) => run_app(&opts),
        Err(exit_code) => exit_code,
    }
}
