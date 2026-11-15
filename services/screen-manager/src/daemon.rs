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
    AppShell, BootCheckLine, BootCheckStatus, BootCheckUpdate, BootProbe, BtDeviceRow,
    BtDevicesUpdate, FrameTiming, Framebuffer, FramebufferError, MainWindow, SplashScreen,
    device_row,
};
#[cfg(feature = "ui")]
use slint::ComponentHandle;
#[cfg(feature = "ui")]
use slint::platform::WindowAdapter;
#[cfg(feature = "ui")]
use slint::platform::software_renderer::MinimalSoftwareWindow;
#[cfg(feature = "ui")]
use speaker_bluetooth::{
    BtAddress, BtError, DeviceCommand, DeviceCommandQueue, PairingAgentHandle, PairingCode,
    PairingStatus, device_action_message,
};
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
    /// Queue bridging render-thread volume/mute actions to the `amp-volume` worker.
    pub amp_commands: crate::volume::AmpVolumeCommandQueue,
    /// Queue bridging render-thread Devices-screen actions to the `bt-monitor` worker.
    pub device_commands: speaker_bluetooth::DeviceCommandQueue,
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
    // Screen volume/mute always targets the amp, independent of Bluetooth
    // presence, so the queue is wired unconditionally.
    let amp_commands = crate::volume::AmpVolumeCommandQueue::new();
    shell.wire_amp_volume(&amp_commands);
    // The Devices screen is always present in the shell, so its command queue
    // is wired unconditionally (parity with the amp queue): navigation
    // callbacks stay functional even when the D-Bus connection failed at boot.
    let device_commands = speaker_bluetooth::DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&device_commands);
    let maybe_bt = if mocked_device {
        let (mock_bt, _) = speaker_bluetooth::BluetoothDevice::new_mocked();
        apply_initial_bt_state(&shell, &mock_bt);
        shell.wire_bluetooth_transport(&commands);
        Some(mock_bt)
    } else {
        match speaker_bluetooth::BluetoothDevice::new_blocking() {
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
        amp_commands,
        device_commands,
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
    /// Cover art delta: outer `Some` = cover state changed; inner`Some` = decoded
    /// pixel buffer to display, inner `None` = clear the displayed cover art.
    /// Carried as a `SharedPixelBuffer` (which is `Send + Sync`) rather than a
    /// `slint::Image` (which is not `Send`): the expensive decode/upscale happens
    /// on the worker, and the render thread only wraps the buffer in an image.
    // The three cover states (unchanged / clear / show buffer) are intentional.
    pub cover: Option<Option<CoverPixelBuffer>>,
    /// Authoritative amp level/mute snapshot, if it changed since the last
    /// publish (pushed by the dedicated `amp-volume` worker).
    pub amp_volume: Option<crate::volume::AmpVolumeState>,
    /// Bluetooth Devices screen delta (rows/discovery/pairing/toast), if any
    /// screen-visible state changed since the last publish.
    pub devices: Option<crate::ui::BtDevicesUpdate>,
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

/// Upper bound on one pairing attempt before the screen backs out to Add
/// Device and surfaces a "Pairing timed out" toast (spec: 30 s).
///
/// Unconditional production constant: tests inject short deadlines through
/// the parameterized [`BtMonitorState::poll_tick`] argument instead of
/// shrinking this value (AGENTS §5.3).
#[cfg(feature = "ui")]
pub const BT_PAIRING_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace window past [`BT_PAIRING_TIMEOUT`] before a pairing thread that
/// ignores cancellation is detached and its agent released.
#[cfg(feature = "ui")]
const BT_PAIRING_REAP_GRACE: Duration = Duration::from_secs(10);

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

/// In-flight pairing attempt owned by the `bt-monitor` worker (Devices screen).
///
/// `BlueZ`'s `Pair()` blocks for up to the full pairing window, so the call runs
/// on a dedicated transient thread while the worker keeps draining commands,
/// polls the agent's shared passkey slot, enforces the deadline, and honors
/// user cancellation. The agent is registered on its own bus connection
/// (plan A4) before the thread starts, and released when the session reaps.
#[cfg(feature = "ui")]
#[derive(Debug)]
struct PairingSession {
    /// Address of the device being paired.
    addr: BtAddress,
    /// Display name resolved when the attempt started.
    name: Box<str>,
    /// Passkey slot shared with the registered pairing agent.
    codes: Arc<Mutex<Option<PairingCode>>>,
    /// Registered agent; owns a dedicated connection until released.
    agent: Option<PairingAgentHandle>,
    /// Transient thread blocked inside `Pair()`; `None` once joined.
    handle: Option<std::thread::JoinHandle<()>>,
    /// Result slot written by the pairing thread when `Pair()` returns.
    result: Arc<Mutex<Option<Result<(), BtError>>>>,
    /// Instant after which the attempt is abandoned.
    deadline_at: Instant,
    /// User requested cancellation (`CancelPairing` / back navigation).
    cancel_requested: bool,
    /// Deadline fired; a "Pairing timed out" toast was staged.
    timed_out: bool,
    /// The agent's passkey for this device was already staged for display.
    code_published: bool,
    /// Terminal state once derived; drives reaping.
    terminal: Option<PairingStatus>,
}

/// Devices-screen dedup, adapter, and pairing session state embedded in
/// [`BtMonitorState`].
///
/// Rows/alias/scanning are deduplicated against the last published values;
/// `staged_pairing`/`staged_toast` are one-shot events that survive aborted
/// ticks ([`BtMonitorState::reset_dedup`] never clears them) so a terminal
/// pairing transition can never be lost to a mid-query command arrival.
#[cfg(feature = "ui")]
#[derive(Debug, Default)]
struct BtDevicesMonitorState {
    /// Last published paired/active rows.
    last_rows: Vec<BtDeviceRow>,
    /// Last published discovered rows.
    last_discovered: Vec<BtDeviceRow>,
    /// Last published discovery-spinner flag.
    last_scanning: bool,
    /// Last published adapter alias (subtitle source).
    last_alias: Option<Box<str>>,
    /// Cached `BlueZ` adapter id (`hciN`) for adapter-scoped admin calls.
    adapter_id: Option<Box<str>>,
    /// Discovery was started from this screen; stopped when the session ends.
    discovery_from_screen: bool,
    /// Active pairing attempt, if any.
    pairing: Option<PairingSession>,
    /// Pairing status transition awaiting publish.
    staged_pairing: Option<PairingStatus>,
    /// User-visible action-failure toast awaiting publish.
    staged_toast: Option<Box<str>>,
}

#[cfg(feature = "ui")]
impl BtDevicesMonitorState {
    /// Clears row/alias/scanning dedup so the next tick re-publishes the
    /// full screen state; staged events and the adapter-id cache survive.
    fn clear_row_dedup(&mut self) {
        self.last_rows.clear();
        self.last_discovered.clear();
        self.last_alias = None;
        self.last_scanning = false;
    }
}

/// Device rows rebuilt from one link snapshot, staged into the shared update.
#[cfg(feature = "ui")]
#[derive(Debug)]
struct BuiltDeviceRows {
    /// Paired or connected rows (main list).
    rows: Vec<BtDeviceRow>,
    /// Unpaired, unconnected rows (discovery list).
    discovered: Vec<BtDeviceRow>,
    /// Adapter's authoritative `Discovering` flag.
    scanning: bool,
    /// Adapter alias for the header subtitle, if an adapter is present.
    alias: Option<Box<str>>,
}

/// Host-testable state machine for the `bt-monitor` worker loop.
#[cfg(feature = "ui")]
#[derive(Debug, Default)]
pub struct BtMonitorState {
    last_pb: Option<speaker_bluetooth::BtPlaybackState>,
    last_pcms: Option<Box<[speaker_bluetooth::BlueAlsaPcm]>>,
    last_details: Option<crate::ui::BtLinkDetails>,
    cover: CoverState,
    settle: Option<SettleWindow>,
    devices: BtDevicesMonitorState,
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
    /// `TogglePlayPause` or `Seek` command.
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
        // Volume and mute commands are executed by the dedicated `amp-volume`
        // worker against `audio-manager`, never by the Bluetooth transport.
        if let Err(e) = match command {
            speaker_bluetooth::TransportCommand::TogglePlayPause => bt.player_toggle().map(|_| ()),
            speaker_bluetooth::TransportCommand::NextTrack => bt.player_next(),
            speaker_bluetooth::TransportCommand::PreviousTrack => bt.player_previous(),
            speaker_bluetooth::TransportCommand::Seek(pos) => bt.player_seek(pos),
        } {
            tracing::warn!(error = %e, "Bluetooth transport command failed");
        }
    }

    /// Queries one lightweight link snapshot and stages PCM and connection-detail
    /// changes into `update`.
    ///
    /// Returns the snapshot on success so the caller can rebuild Devices-screen
    /// rows from the same query, or `None` on a transient D-Bus error (the last
    /// published rows are kept; see [`BtMonitorState::stage_devices_update`]).
    pub fn publish_link_state(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        update: &mut BtSharedUpdate,
    ) -> Option<speaker_bluetooth::LinkSnapshot> {
        let snapshot = match bt.link_snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to query Bluetooth link snapshot");
                return None;
            }
        };
        if self.last_pcms.as_ref() != Some(&snapshot.pcms) {
            if let Some(active) = speaker_bluetooth::select_active_pcm(&snapshot.pcms) {
                update.pcm = Some(active.clone());
            }
            self.last_pcms = Some(snapshot.pcms.clone());
        }
        let preferred = self.last_pb.as_ref().and_then(|pb| pb.device_address);
        let details = crate::ui::BtLinkDetails::select(&snapshot, preferred);
        if self.last_details.as_ref() != Some(&details) {
            update.details = Some(details.clone());
            self.last_details = Some(details);
        }
        Some(snapshot)
    }

    /// Resets dedup state when a new command arrives mid-query so the next tick
    /// performs a full authoritative publish after executing the fresh command.
    pub fn reset_dedup(&mut self) {
        self.last_pb = None;
        self.cover.path = None;
        self.last_pcms = None;
        self.last_details = None;
        // The Devices screen re-publishes rows/alias/scanning after an aborted
        // tick, but staged pairing/toast events survive: they are one-shot
        // transitions that must not be lost to a command arriving mid-query.
        self.devices.clear_row_dedup();
    }

    /// Returns the cached `BlueZ` adapter id (`hciN`), resolving it lazily.
    ///
    /// A Devices-screen command can arrive before the first link snapshot has
    /// populated the cache (e.g. the screen opens and `+ Add Device` is tapped
    /// immediately), so adapter-scoped calls resolve it on demand.
    fn resolve_adapter_id(&mut self, bt: &speaker_bluetooth::BluetoothDevice) -> Option<Box<str>> {
        if let Some(id) = &self.devices.adapter_id {
            return Some(id.clone());
        }
        match bt.adapter_info() {
            Ok(Some(adapter)) => {
                self.devices.adapter_id = Some(adapter.id.clone());
                self.devices.adapter_id.clone()
            }
            Ok(None) => {
                tracing::debug!("No Bluetooth adapter available for device administration");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to resolve Bluetooth adapter id");
                None
            }
        }
    }

    /// Stages a user-visible toast for a failed device-admin action.
    fn stage_action_failure(&mut self, error: &BtError) {
        let message = device_action_message(error);
        tracing::warn!(error = %error, message, "Bluetooth device action failed");
        self.devices.staged_toast = Some(message.into());
    }

    /// Resolves a device's display name from the last published rows,
    /// falling back to the raw address for devices never seen on screen.
    fn device_name(&self, addr: BtAddress) -> Box<str> {
        self.devices
            .last_rows
            .iter()
            .chain(self.devices.last_discovered.iter())
            .find(|row| BtAddress::parse_str(row.address.as_str()).is_ok_and(|a| a == addr))
            .map_or_else(
                || addr.to_string().into_boxed_str(),
                |row| row.name.to_string().into_boxed_str(),
            )
    }

    /// Returns `true` if the address maps to a connected row on screen, so
    /// `Remove` can disconnect first (`BlueZ` rejects `RemoveDevice` while the
    /// device is connected — plan §12).
    fn device_row_connected(&self, addr: BtAddress) -> bool {
        self.devices.last_rows.iter().any(|row| {
            row.connected && BtAddress::parse_str(row.address.as_str()).is_ok_and(|a| a == addr)
        })
    }

    /// Executes one drained Devices-screen command against `bt`.
    ///
    /// Failures stage a toast (never a panic): the screen keeps working and
    /// the next authoritative snapshot reconciles the UI.
    fn execute_device_command(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        command: DeviceCommand,
        pairing_timeout: Duration,
    ) {
        // Refresh only invalidates dedup: the very next snapshot re-publishes
        // rows that are already on screen (forces a publish after e.g. a
        // failed action so stale optimistic state cannot linger).
        if matches!(command, DeviceCommand::Refresh) {
            self.devices.clear_row_dedup();
            return;
        }
        // Pairing control without an active attempt is a no-op: the UI can
        // fire late (double-tap) after a terminal transition already reaped
        // the session.
        if matches!(
            command,
            DeviceCommand::CancelPairing | DeviceCommand::ReleaseAgent
        ) && self.devices.pairing.is_none()
        {
            tracing::debug!(?command, "Ignoring pairing control with no active attempt");
            return;
        }
        let Some(adapter) = self.resolve_adapter_id(bt) else {
            self.devices.staged_toast = Some("Bluetooth adapter not available".into());
            return;
        };
        match command {
            // Handled above; the arm exists so new variants stay reviewed.
            DeviceCommand::Refresh => {}
            DeviceCommand::StartDiscovery => match bt.start_discovery(&adapter) {
                Ok(()) => self.devices.discovery_from_screen = true,
                Err(e) => self.stage_action_failure(&e),
            },
            DeviceCommand::StopDiscovery => match bt.stop_discovery(&adapter) {
                Ok(()) => self.devices.discovery_from_screen = false,
                Err(e) => self.stage_action_failure(&e),
            },
            DeviceCommand::Connect(addr) => {
                let name = self.device_name(addr);
                if let Err(e) = bt.connect_device(&adapter, addr) {
                    tracing::warn!(device = %name, error = %e, "Connect failed");
                    self.stage_action_failure(&e);
                }
            }
            DeviceCommand::Disconnect(addr) => {
                let name = self.device_name(addr);
                if let Err(e) = bt.disconnect_device(&adapter, addr) {
                    tracing::warn!(device = %name, error = %e, "Disconnect failed");
                    self.stage_action_failure(&e);
                }
            }
            DeviceCommand::Remove(addr) => {
                // BlueZ refuses RemoveDevice on a connected device, so
                // disconnect first (plan §12). Best-effort: the device may
                // already be gone, and Remove must still be attempted.
                if self.device_row_connected(addr)
                    && let Err(e) = bt.disconnect_device(&adapter, addr)
                {
                    tracing::warn!(error = %e, "Disconnect before remove failed; continuing");
                }
                let name = self.device_name(addr);
                if let Err(e) = bt.remove_device(&adapter, addr) {
                    tracing::warn!(device = %name, error = %e, "Remove failed");
                    self.stage_action_failure(&e);
                }
            }
            DeviceCommand::Pair(addr) => {
                self.begin_pairing(bt, &adapter, addr, pairing_timeout);
            }
            DeviceCommand::CancelPairing | DeviceCommand::ReleaseAgent => {
                if let Some(session) = self.devices.pairing.as_mut()
                    && !session.cancel_requested
                {
                    session.cancel_requested = true;
                    if let Err(e) = bt.cancel_pairing(&adapter, session.addr) {
                        tracing::warn!(error = %e, "Pairing cancellation failed");
                    }
                }
            }
        }
    }

    /// Starts a pairing attempt: registers the agent on its own connection
    /// **before** issuing `Pair()` (`BlueZ` dispatches the passkey to the
    /// registered default agent), then spawns a transient thread for the
    /// blocking call so the worker stays responsive.
    fn begin_pairing(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        adapter: &str,
        addr: BtAddress,
        pairing_timeout: Duration,
    ) {
        // Reap a finished attempt first so a retry right after a terminal
        // transition is not refused by a stale session.
        self.advance_pairing(bt);
        if self.devices.pairing.is_some() {
            tracing::debug!(device = %addr, "Pairing already in progress");
            self.devices.staged_toast = Some("Operation already in progress".into());
            return;
        }
        let codes = Arc::new(Mutex::new(None));
        let agent = match PairingAgentHandle::register(Arc::clone(&codes)) {
            Ok(agent) => agent,
            Err(e) => {
                self.stage_action_failure(&e);
                return;
            }
        };
        let name = self.device_name(addr);
        tracing::info!(device = %addr, name = %name, "Bluetooth pairing attempt started");
        let result_slot: Arc<Mutex<Option<Result<(), BtError>>>> = Arc::new(Mutex::new(None));
        let spawned = {
            let device = bt.clone();
            let adapter = adapter.to_owned().into_boxed_str();
            let slot = Arc::clone(&result_slot);
            std::thread::Builder::new()
                .name("bt-pairing".into())
                .spawn(move || {
                    let result = device.pair_device(&adapter, addr);
                    // A poisoned slot still carries the result: the panic only
                    // hit the report, never the pairing itself.
                    *slot.lock().unwrap_or_else(|p| {
                        tracing::warn!("Pairing result slot poisoned; recovering");
                        p.into_inner()
                    }) = Some(result);
                })
        };
        let handle = match spawned {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to spawn pairing thread");
                self.devices.staged_toast = Some("Pairing failed".into());
                // `agent` drops here and unregisters itself.
                return;
            }
        };
        self.devices.pairing = Some(PairingSession {
            addr,
            name: name.clone(),
            codes,
            agent: Some(agent),
            handle: Some(handle),
            result: result_slot,
            deadline_at: Instant::now() + pairing_timeout,
            cancel_requested: false,
            timed_out: false,
            code_published: false,
            terminal: None,
        });
        self.devices.staged_pairing = Some(PairingStatus::Connecting { name });
    }

    /// Returns `true` once the transient pairing thread has finished.
    fn pairing_finished(session: &PairingSession) -> bool {
        session
            .handle
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Joins (or detaches after the grace window), stops screen-started
    /// discovery, and releases the agent for a terminal session.
    fn reap_pairing(
        &mut self,
        session: &mut PairingSession,
        bt: &speaker_bluetooth::BluetoothDevice,
    ) {
        if let Some(handle) = session.handle.take() {
            if handle.is_finished() {
                if let Err(panic) = handle.join() {
                    std::panic::resume_unwind(panic);
                }
            } else {
                // Cancellation was issued but the thread is stuck; dropping
                // the handle detaches it and closing the agent's connection
                // releases the agent on the BlueZ side too.
                tracing::warn!(device = %session.addr, "Pairing thread stuck; detaching");
            }
        }
        if self.devices.discovery_from_screen {
            self.devices.discovery_from_screen = false;
            if let Some(adapter) = self.resolve_adapter_id(bt)
                && let Err(e) = bt.stop_discovery(&adapter)
            {
                tracing::warn!(error = %e, "StopDiscovery after pairing failed");
            }
        }
        if let Some(agent) = session.agent.take() {
            agent.unregister();
        }
        tracing::info!(
            device = %session.addr,
            status = ?session.terminal,
            "Bluetooth pairing session closed"
        );
    }

    /// Progresses the active pairing session: publishes the passkey, derives
    /// terminal states, enforces the deadline, and reaps finished attempts.
    fn advance_pairing(&mut self, bt: &speaker_bluetooth::BluetoothDevice) {
        let Some(mut session) = self.devices.pairing.take() else {
            return;
        };

        // Terminal already staged: reap once the thread finished or the grace
        // window past the deadline lapsed (an unstoppable thread is detached).
        if session.terminal.is_some() {
            if Self::pairing_finished(&session)
                || session.deadline_at + BT_PAIRING_REAP_GRACE <= Instant::now()
            {
                self.reap_pairing(&mut session, bt);
                return;
            }
            self.devices.pairing = Some(session);
            return;
        }

        // Publish the passkey exactly once, and only from a slot recorded for
        // THIS device: a stale value from a previous attempt must never show.
        if !session.code_published {
            let slot = session.codes.lock().unwrap_or_else(|p| {
                tracing::warn!("Pairing code slot poisoned; recovering");
                p.into_inner()
            });
            if let Some(code) = *slot
                && code.device == session.addr
            {
                session.code_published = true;
                self.devices.staged_pairing = Some(PairingStatus::AwaitingConfirmation {
                    name: session.name.clone(),
                    passkey: code.passkey,
                });
            }
        }

        // Terminal derive: thread completion outranks cancellation, which
        // outranks the deadline; a still-pending session is put back untouched.
        let terminal = if Self::pairing_finished(&session) {
            if let Some(handle) = session.handle.take()
                && let Err(panic) = handle.join()
            {
                std::panic::resume_unwind(panic);
            }
            let mut result = session.result.lock().unwrap_or_else(|p| {
                tracing::warn!("Pairing result slot poisoned; recovering");
                p.into_inner()
            });
            match result.take() {
                Some(Ok(())) => PairingStatus::Paired {
                    name: session.name.clone(),
                },
                Some(Err(e)) if session.cancel_requested => {
                    let _ = e; // cancel was user-initiated; surface Cancelled
                    PairingStatus::Cancelled
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "Bluetooth pairing attempt failed");
                    PairingStatus::Failed {
                        message: device_action_message(&e).into(),
                    }
                }
                None => PairingStatus::Failed {
                    message: "Pairing failed".into(),
                },
            }
        } else if session.cancel_requested {
            PairingStatus::Cancelled
        } else if Instant::now() >= session.deadline_at {
            session.timed_out = true;
            session.cancel_requested = true;
            if let Some(adapter) = self.resolve_adapter_id(bt)
                && let Err(e) = bt.cancel_pairing(&adapter, session.addr)
            {
                tracing::warn!(error = %e, "Timeout pairing cancellation failed");
            }
            self.devices.staged_toast = Some("Pairing timed out".into());
            PairingStatus::TimedOut
        } else {
            self.devices.pairing = Some(session);
            return;
        };

        session.terminal = Some(terminal.clone());
        self.devices.staged_pairing = Some(terminal);
        if Self::pairing_finished(&session) {
            // Completed this tick: reap immediately so the agent releases
            // without waiting out the grace window.
            self.reap_pairing(&mut session, bt);
        } else {
            self.devices.pairing = Some(session);
        }
    }

    /// Rebuilds the Devices-screen row model from a fresh snapshot, advancing
    /// adapter bookkeeping (id/alias/scanning) as a side effect.
    fn build_device_rows(&mut self, snapshot: &speaker_bluetooth::LinkSnapshot) -> BuiltDeviceRows {
        let (alias, scanning, adapter_id) = match snapshot.adapter.as_ref() {
            Some(adapter) => (
                Some(if adapter.alias.is_empty() {
                    adapter.name.clone()
                } else {
                    adapter.alias.clone()
                }),
                adapter.discovering,
                Some(adapter.id.clone()),
            ),
            // No adapter: clear the cached id so admin commands re-resolve
            // (and stage their own toast) instead of using a stale path.
            None => (None, false, None),
        };
        self.devices.adapter_id = adapter_id;
        // Active device = the same selection the details pane uses, so the
        // "Active" row badge always matches the playing connection.
        let active = self
            .last_details
            .as_ref()
            .and_then(|details| details.device.as_ref().map(|device| device.address));
        let rows = snapshot
            .devices
            .iter()
            .filter(|device| device.paired || device.connected)
            .map(|device| device_row(device, active))
            .collect();
        let discovered = snapshot
            .devices
            .iter()
            .filter(|device| !device.paired && !device.connected)
            .map(|device| device_row(device, active))
            .collect();
        BuiltDeviceRows {
            rows,
            discovered,
            scanning,
            alias,
        }
    }

    /// Folds built rows plus staged one-shot events into `update`, advancing
    /// dedup. `built == None` (snapshot failed) keeps the last published rows
    /// so a transient D-Bus hiccup never blanks the screen.
    fn stage_devices_update(
        &mut self,
        built: Option<BuiltDeviceRows>,
        update: &mut BtSharedUpdate,
    ) {
        let staged_pairing = self.devices.staged_pairing.take();
        let staged_toast = self.devices.staged_toast.take();
        let mut changed = false;
        if let Some(built) = built {
            if built.rows != self.devices.last_rows {
                self.devices.last_rows = built.rows;
                changed = true;
            }
            if built.discovered != self.devices.last_discovered {
                self.devices.last_discovered = built.discovered;
                changed = true;
            }
            if built.scanning != self.devices.last_scanning {
                self.devices.last_scanning = built.scanning;
                changed = true;
            }
            let staged_alias = match built.alias {
                Some(alias) => {
                    let alias_changed = self.devices.last_alias.as_deref() != Some(alias.as_ref());
                    if alias_changed {
                        self.devices.last_alias = Some(alias.clone());
                    }
                    alias_changed.then_some(alias)
                }
                None => None,
            };
            if changed
                || staged_alias.is_some()
                || staged_pairing.is_some()
                || staged_toast.is_some()
            {
                self.stage_devices_publish(update, staged_pairing, staged_toast, staged_alias);
            }
            return;
        }
        // Snapshot failed: only staged events publish; rows stay as-is.
        if staged_pairing.is_some() || staged_toast.is_some() {
            self.stage_devices_publish(update, staged_pairing, staged_toast, None);
        }
    }

    /// Publishes a full-state Devices update (apply replaces the row models
    /// wholesale, so the last-known rows always ride along).
    fn stage_devices_publish(
        &self,
        update: &mut BtSharedUpdate,
        pairing: Option<PairingStatus>,
        toast: Option<Box<str>>,
        adapter_alias: Option<Box<str>>,
    ) {
        update.devices = Some(BtDevicesUpdate {
            rows: self.devices.last_rows.clone(),
            discovered: self.devices.last_discovered.clone(),
            scanning: self.devices.last_scanning,
            pairing,
            toast,
            adapter_alias,
        });
    }

    /// Merges non-empty delta fields from `update` into `shared`, recovering from
    /// a poisoned mutex guard.
    pub fn commit_update(shared: &Mutex<BtSharedUpdate>, update: BtSharedUpdate) {
        if update.playback.is_some()
            || update.pcm.is_some()
            || update.details.is_some()
            || update.cover.is_some()
            || update.amp_volume.is_some()
            || update.devices.is_some()
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
            if update.amp_volume.is_some() {
                lock.amp_volume = update.amp_volume;
            }
            if update.devices.is_some() {
                lock.devices = update.devices;
            }
        }
    }

    /// Runs one command-drain + state-query + shared-publish cycle, returning
    /// `true` if a full snapshot poll completed or `false` if a newly queued
    /// command interrupted the tick and should be drained immediately.
    ///
    /// `pairing_timeout` is injected so tests can drive deadline transitions
    /// fast; production passes [`BT_PAIRING_TIMEOUT`] (unconditional).
    pub fn poll_tick(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        shared: &Mutex<BtSharedUpdate>,
        commands: &speaker_bluetooth::TransportCommandQueue,
        device_commands: &DeviceCommandQueue,
        pairing_timeout: Duration,
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

        // Devices-screen admin commands run before the snapshot so the very
        // next publish already reflects discovery/connect/pair/remove.
        for command in device_commands.drain() {
            self.execute_device_command(bt, command, pairing_timeout);
        }
        if !commands.is_empty() || !device_commands.is_empty() {
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
        let snapshot = self.publish_link_state(bt, &mut update);
        let built = snapshot
            .as_ref()
            .map(|snapshot| self.build_device_rows(snapshot));

        // Pairing progresses every tick regardless of snapshot health: the
        // passkey slot, deadline, and cancellation are independent of D-Bus
        // query success, so a flapping adapter can never wedge the screen.
        self.advance_pairing(bt);

        // A command that landed while the blocking snapshot query ran must not
        // merge a pre-command snapshot. The query already advanced the dedup
        // state, so it is reset here to force a full re-publish on the next
        // tick (after the fresh commands are executed).
        if !commands.is_empty() || !device_commands.is_empty() {
            self.reset_dedup();
            return false;
        }

        self.stage_devices_update(built, &mut update);
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
    device_commands: &DeviceCommandQueue,
    running: &AtomicBool,
) {
    let mut state = BtMonitorState::default();

    while running.load(Ordering::Relaxed) {
        if !state.poll_tick(bt, shared, commands, device_commands, BT_PAIRING_TIMEOUT) {
            continue;
        }

        let deadline = Instant::now() + state.keep_alive_interval();
        while running.load(Ordering::Relaxed) && Instant::now() < deadline {
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(BT_WAIT_SLICE);
            // A `true` return means a PropertiesChanged signal arrived: breaking
            // out re-polls immediately so track/status/position changes surface
            // without waiting for the remaining keep-alive window. Devices-screen
            // commands break the wait the same way transport commands do.
            if bt.wait_for_property_change(slice)
                || !commands.is_empty()
                || !device_commands.is_empty()
            {
                break;
            }
        }
    }
}

#[cfg(feature = "ui")]
#[expect(
    clippy::too_many_arguments,
    reason = "queues and peripherals are passed through to the devices render seam"
)]
pub fn run_render_loop(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    shell: &AppShell,
    maybe_bt: Option<&speaker_bluetooth::BluetoothDevice>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    device_commands: &speaker_bluetooth::DeviceCommandQueue,
    amp_commands: &crate::volume::AmpVolumeCommandQueue,
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
        device_commands,
        amp_commands,
        preview_secs,
        touch_dev,
        encoder_dev.as_ref(),
    );
}

/// Runs the UI render loop with explicit touch input device injection.
#[cfg(feature = "ui")]
#[expect(
    clippy::too_many_arguments,
    reason = "explicit peripheral injection seam for hermetic touch integration testing"
)]
pub fn run_render_loop_with_input(
    fb: &mut Framebuffer,
    window: &Rc<MinimalSoftwareWindow>,
    shell: &AppShell,
    maybe_bt: Option<&speaker_bluetooth::BluetoothDevice>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    device_commands: &speaker_bluetooth::DeviceCommandQueue,
    amp_commands: &crate::volume::AmpVolumeCommandQueue,
    preview_secs: Option<u64>,
    touch_dev: Option<crate::ui::TouchInputDevice>,
) {
    run_render_loop_with_devices(
        fb,
        window,
        shell,
        maybe_bt,
        commands,
        device_commands,
        amp_commands,
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

/// Joins the `amp-volume` worker, re-raising a worker panic (fail-fast).
#[cfg(feature = "ui")]
fn join_amp_volume_worker(amp_worker: std::io::Result<std::thread::JoinHandle<()>>) {
    match amp_worker {
        Ok(handle) => {
            // Propagate a worker panic to this thread: a crashed amp-volume
            // worker must fail the daemon (fail-fast) rather than leave screen
            // volume silently unresponsive.
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "Failed to spawn amp-volume worker thread; screen volume disabled"
            );
        }
    }
}

/// Spawns the `bt-monitor` worker thread for a live Bluetooth device.
#[cfg(feature = "ui")]
fn spawn_bt_monitor_worker(
    bt: &speaker_bluetooth::BluetoothDevice,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    device_commands: &speaker_bluetooth::DeviceCommandQueue,
    running: &Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let bt = bt.clone();
    let shared = Arc::clone(shared);
    let commands = commands.clone();
    let device_commands = device_commands.clone();
    let running = Arc::clone(running);
    std::thread::Builder::new()
        .name("bt-monitor".into())
        .spawn(move || bt_monitor_loop(&bt, &shared, &commands, &device_commands, &running))
}

/// Spawns the dedicated `amp-volume` worker thread.
///
/// The worker always runs: in mocked sessions (host tests, splash previews)
/// against a mock client so nothing touches the host system bus; in live
/// sessions against the real `audio-manager` D-Bus API. The choice is keyed on
/// the framebuffer backend, not on Bluetooth availability: a live appliance
/// whose Bluetooth D-Bus connection failed at startup must still drive the real
/// amplifier, never a silent mock.
#[cfg(feature = "ui")]
fn spawn_amp_volume_worker(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &Arc<AtomicBool>,
    amp_commands: &crate::volume::AmpVolumeCommandQueue,
    mocked: bool,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let shared = Arc::clone(shared);
    let running = Arc::clone(running);
    let amp_commands = amp_commands.clone();
    std::thread::Builder::new()
        .name("amp-volume".into())
        .spawn(move || {
            if mocked {
                // Mocked sessions (host tests, splash previews) get a mock
                // client seeded at 50 % unmuted; no system-bus call.
                let (client, _ctrl) = crate::volume::AmpVolumeClient::new_mocked(
                    crate::volume::AmpVolumeState::new(50, false),
                );
                crate::volume::run_worker(&client, &shared, &amp_commands, &running);
            } else {
                // Retry connection establishment with bounded backoff while
                // the session runs: a temporarily unreachable system bus at
                // startup must not kill screen volume control permanently.
                let mut backoff = Duration::from_secs(1);
                loop {
                    if !running.load(Ordering::Relaxed) {
                        return;
                    }
                    match crate::volume::AmpVolumeClient::new_blocking() {
                        Ok(client) => {
                            crate::volume::run_worker(&client, &shared, &amp_commands, &running);
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                backoff_ms = backoff.as_millis() as u64,
                                "System D-Bus unavailable; retrying amp-volume connection"
                            );
                            // Sleep in slices so shutdown is never delayed by
                            // a long backoff.
                            let mut remaining = backoff;
                            while !remaining.is_zero() && running.load(Ordering::Relaxed) {
                                let slice = remaining.min(Duration::from_millis(200));
                                std::thread::sleep(slice);
                                remaining = remaining.saturating_sub(slice);
                            }
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                        }
                    }
                }
            }
        })
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
    device_commands: &speaker_bluetooth::DeviceCommandQueue,
    amp_commands: &crate::volume::AmpVolumeCommandQueue,
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
    let device_commands_worker = device_commands.clone();

    let amp_worker = spawn_amp_volume_worker(
        &shared_state,
        &bg_running,
        amp_commands,
        !fb.is_hardware_panel(),
    );

    let bt_worker = maybe_bt.map(|bt| {
        spawn_bt_monitor_worker(
            bt,
            &shared_state_worker,
            &commands_worker,
            &device_commands_worker,
            &bg_running_worker,
        )
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
    join_amp_volume_worker(amp_worker);
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
        if let Some(devices) = lock.devices.take() {
            shell.apply_bt_devices(devices);
        }
        if let Some(cover) = lock.cover.take() {
            match cover {
                // Cheap on the render thread: just wraps the pre-decoded buffer
                // into an image handle; the expensive decode happened on the worker.
                Some(buffer) => shell.set_cover_image(slint::Image::from_rgba8(buffer)),
                None => shell.set_has_cover_art(false),
            }
        }
        if let Some(amp) = lock.amp_volume.take() {
            shell.apply_amp_volume(&amp);
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
        amp_commands,
        device_commands,
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

    run_render_loop(
        fb,
        &window,
        &shell,
        bt.as_ref(),
        &commands,
        &device_commands,
        &amp_commands,
        preview_secs,
    );
}

/// Executes the core appliance daemon with parsed options.
#[must_use = "the process exit code must be propagated to the caller"]
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
