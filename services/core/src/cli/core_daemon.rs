//! Central appliance daemon and display orchestration service logic.
//!
//! Provides command-line options parsing, headless detection, display lifecycle
//! execution, and Slint framebuffer rendering coordination.

// `PathBuf` backs the unconditionally compiled `-c/--config` CLI field;
// `Path` is only referenced by the ui-gated framebuffer paths.
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
use crate::diag::{CliParse, DEFAULT_CONFIG_PATH, env_args, next_arg_value, resolve_cli_opts};
use crate::hardware::TempSensor;
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
    println!("DietPi Bluetooth Speaker & DSP Core Appliance Service");
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

/// Runs the UI display loop on the default `/dev/fb0` device path.
#[cfg(feature = "ui")]
pub fn run_ui_display(
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
    marquee_fps: u32,
) {
    if headless_requested() {
        tracing::info!("Headless mode requested via SPEAKER_CORE_HEADLESS=1");
        return;
    }
    run_ui_display_on_path(
        Path::new(Framebuffer::DEFAULT_FB_PATH),
        screen,
        preview_secs,
        theme,
        tab,
        marquee_fps,
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
            run_splash_on(&mut fb, BootProbe::real(), preview_secs, theme);
            let remaining_preview =
                preview_secs.map(|p| p.saturating_sub(splash_start.elapsed().as_secs()));
            run_shell_on(&mut fb, remaining_preview, theme, tab, marquee_fps);
        }
        ScreenTarget::Splash => {
            run_splash_on(&mut fb, BootProbe::real(), preview_secs, theme);
        }
        ScreenTarget::Shell => {
            run_shell_on(&mut fb, preview_secs, theme, tab, marquee_fps);
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
        }
        Ok(false) => {
            tracing::debug!("Screen buffer already current");
        }
        Err(e) => {
            report_display(tracing::Level::ERROR, "Failed to render frame", &e);
            return;
        }
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

    let splash = match SplashScreen::new() {
        Ok(splash) => splash,
        Err(e) => {
            report_display(
                tracing::Level::ERROR,
                "Failed to initialize Boot splash screen",
                &e,
            );
            return;
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
        return;
    }

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
    let mut checks_done = checks.is_none();

    'render: {
        match fb.render_frame(&window) {
            Ok(true) => {
                tracing::info!("Boot splash screen rendered to /dev/fb0 successfully");
            }
            Ok(false) => {
                tracing::debug!("Screen buffer already current");
            }
            Err(e) => {
                report_display(tracing::Level::ERROR, "Failed to render frame", &e);
                break 'render;
            }
        }

        let start = Instant::now();
        // Hard upper bound: without a preview window the splash may still not
        // wait forever on a hung hardware probe (blocking D-Bus/I2C calls).
        let deadline = preview_secs.map_or(MAX_SPLASH_WAIT, Duration::from_secs);
        tracing::info!("Running UI render loop");

        loop {
            loop {
                match rx.try_recv() {
                    Ok(update) => apply_check_update(&splash, &update),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        checks_done = true;
                        break;
                    }
                }
            }

            slint::platform::update_timers_and_animations();
            if let Err(e) = fb.render_frame(&window) {
                report_display(tracing::Level::ERROR, "Render loop frame failed", &e);
                break;
            }

            if checks_done || start.elapsed() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    if checks_done {
        if let Some(handle) = checks {
            if let Err(err) = handle.join() {
                std::panic::resume_unwind(err);
            }
        }
    } else {
        tracing::warn!("Boot checks did not finish within the splash window; abandoning worker");
    }

    tracing::info!("Splash render loop finished");
}

/// Fully-initialized shell plus its Bluetooth transport wiring.
#[cfg(feature = "ui")]
#[derive(Debug)]
pub struct ShellInit {
    /// Application shell bound to the Slint window.
    pub shell: AppShell,
    /// Connected Bluetooth device handle, if D-Bus was reachable.
    pub bt: Option<crate::hardware::BluetoothDevice>,
    /// Queue bridging render-thread transport callbacks to the `bt-monitor` worker.
    pub commands: crate::hardware::TransportCommandQueue,
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
    let shell = AppShell::new(main_window);
    shell.set_marquee_fps(marquee_fps);
    if let Some(t) = theme {
        shell.apply_theme(t);
    }
    let commands = crate::hardware::TransportCommandQueue::new();
    let maybe_bt = if mocked_device {
        let (mock_bt, _) = crate::hardware::BluetoothDevice::new_mocked();
        if let Ok(Some(ref pb)) = mock_bt.playback_state() {
            shell.apply_bluetooth_playback(pb);
        }
        shell.wire_bluetooth_transport(&commands);
        Some(mock_bt)
    } else {
        match crate::hardware::BluetoothDevice::new() {
            Ok(bt) => {
                if let Ok(Some(ref pb)) = bt.playback_state() {
                    shell.apply_bluetooth_playback(pb);
                }
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

/// Worker-side snapshot of Bluetooth state published to the render thread.
///
/// Each field is an `Option` delta: `None` means "no change this tick", while the
/// inner value carries the authoritative latest state. Cover art is decoded and
/// upscaled here (off the render thread) and published as a pre-rendered image.
#[cfg(feature = "ui")]
#[derive(Debug, Default)]
pub struct BtSharedUpdate {
    /// Latest playback snapshot, if it changed since the last publish.
    pub playback: Option<crate::hardware::BtPlaybackState>,
    /// Active PCM stream, if it changed since the last publish.
    pub pcm: Option<crate::hardware::BlueAlsaPcm>,
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
    pub fn suppresses(&self, pb: &crate::hardware::BtPlaybackState) -> bool {
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
    bt: &crate::hardware::BluetoothDevice,
    update: &mut BtSharedUpdate,
    last_pb: &mut Option<crate::hardware::BtPlaybackState>,
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
            if cover.path != cover_path || retry_due {
                if let Some(path) = cover_path.as_ref() {
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
                } else {
                    update.cover = Some(None);
                    *cover = CoverState::default();
                }
            }
        }
        Ok(None) => {
            let placeholder = crate::hardware::BtPlaybackState::default();
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

/// Runs on the dedicated `bt-monitor` thread until `running` is cleared.
/// Panics here are re-raised by the render loop's `join()` so a dead monitor
/// fails the daemon instead of leaving the playback UI silently stale.
#[cfg(feature = "ui")]
fn bt_monitor_loop(
    bt: &crate::hardware::BluetoothDevice,
    shared: &Mutex<BtSharedUpdate>,
    commands: &crate::hardware::TransportCommandQueue,
    running: &AtomicBool,
) {
    // Signal-driven wait with a keep-alive fallback: PropertiesChanged
    // signals wake the loop immediately (via the shared ChangeFlag); the
    // deadline guarantees periodic re-poll even if the D-Bus listener
    // thread dies. Slices cap command latency while waiting.
    const BT_KEEP_ALIVE: Duration = Duration::from_secs(1);
    /// Refresh cadence while playing, matching the old adaptive poll interval.
    const BT_PLAYING_POLL: Duration = Duration::from_millis(250);
    const BT_WAIT_SLICE: Duration = Duration::from_millis(100);
    let mut last_pb: Option<crate::hardware::BtPlaybackState> = None;
    let mut last_pcms: Option<Box<[crate::hardware::BlueAlsaPcm]>> = None;
    let mut cover = CoverState::default();
    let mut settle: Option<SettleWindow> = None;

    while running.load(Ordering::Relaxed) {
        let drained = commands.drain();
        // Play/pause flips the shell's status indicator and seek scrubs the
        // progress bar, both optimistically ahead of peer confirmation. A
        // snapshot fetched immediately after either command can still carry
        // the pre-command state (AVRCP lags by hundreds of milliseconds), so
        // conflicting snapshots are suppressed until the peer catches up. The
        // window is only re-armed on a fresh optimistic command so an
        // in-flight window survives unrelated command batches.
        let toggle = drained
            .iter()
            .any(|c| matches!(c, crate::hardware::TransportCommand::TogglePlayPause));
        let seek = drained.iter().find_map(|c| match c {
            crate::hardware::TransportCommand::Seek(pos) => Some(*pos),
            _ => None,
        });
        if toggle || seek.is_some() {
            // A toggle arm wins when both land in one batch: the status flip is
            // the more visible artifact, and a same-batch seek is rare.
            settle = last_pb.as_ref().map(|pre| SettleWindow {
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
        for command in drained {
            // A failed command is logged (never silently dropped) so operators can
            // diagnose why the authoritative state failed to reconcile the
            // optimistic UI flip; the next authoritative publish corrects the UI.
            if let Err(e) = match command {
                crate::hardware::TransportCommand::TogglePlayPause => {
                    bt.player_toggle().map(|_| ())
                }
                crate::hardware::TransportCommand::NextTrack => bt.player_next(),
                crate::hardware::TransportCommand::PreviousTrack => bt.player_previous(),
                crate::hardware::TransportCommand::Seek(pos) => bt.player_seek(pos),
            } {
                tracing::warn!(error = %e, "Bluetooth transport command failed");
            }
        }

        // A command queued between the drain and the snapshot query would make
        // the query execute a stale pre-command publish; skip the query so the
        // fresh command is executed first and the next tick reads post-command
        // state.
        if !commands.is_empty() {
            continue;
        }

        let mut update = BtSharedUpdate::default();

        // Publish the authoritative state; transient D-Bus errors keep the last
        // published state instead of flashing the placeholder over live playback.
        publish_playback_state(bt, &mut update, &mut last_pb, &mut cover, &mut settle);
        if let Ok(ref pcms) = bt.audio_pcms() {
            if last_pcms.as_ref() != Some(pcms) {
                if let Some(active) = pcms.iter().find(|p| p.running).or_else(|| pcms.first()) {
                    update.pcm = Some(active.clone());
                }
                last_pcms = Some(pcms.clone());
            }
        }

        // A command that landed while the blocking snapshot query ran must not
        // merge a pre-command snapshot. The query already advanced the dedup
        // state, so it is reset here to force a full re-publish on the next
        // tick (after the fresh commands are executed).
        if !commands.is_empty() {
            last_pb = None;
            cover.path = None;
            last_pcms = None;
            continue;
        }

        if update.playback.is_some() || update.pcm.is_some() || update.cover.is_some() {
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
            if update.cover.is_some() {
                lock.cover = update.cover;
            }
        }

        // BlueZ does not reliably re-emit AVRCP Position during playback, so the
        // keep-alive doubles as the progress refresh: shorten it while playing
        // so the position bar advances in ~250 ms steps instead of 1 s jumps.
        let keep_alive = if last_pb
            .as_ref()
            .is_some_and(crate::hardware::BtPlaybackState::is_playing)
        {
            BT_PLAYING_POLL
        } else {
            BT_KEEP_ALIVE
        };
        let deadline = Instant::now() + keep_alive;
        while running.load(Ordering::Relaxed) && Instant::now() < deadline {
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(BT_WAIT_SLICE);
            // A `true` return means a PropertiesChanged signal arrived: breaking
            // out re-polls immediately so track/status/position changes surface
            // without waiting for the remaining keep-alive window.
            let signaled = bt.wait_for_property_change(slice);
            if signaled || !commands.is_empty() {
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
    maybe_bt: Option<&crate::hardware::BluetoothDevice>,
    commands: &crate::hardware::TransportCommandQueue,
    preview_secs: Option<u64>,
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
    while duration.is_none_or(|d| start.elapsed() < d) {
        let frame_start = Instant::now();

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
            idle_sleep();
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
#[cfg(feature = "ui")]
fn idle_sleep() {
    let timer_duration = slint::platform::duration_until_next_timer_update();
    let sleep_duration = match timer_duration {
        Some(d) => d.min(Duration::from_millis(50)),
        None => Duration::from_millis(50),
    };
    std::thread::sleep(sleep_duration.max(Duration::from_millis(10)));
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
        if self.rendered % 600 == 0 {
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

    let ShellInit {
        shell,
        bt,
        commands,
    } = init_shell(
        main_window,
        !fb.is_hardware_panel(),
        theme,
        tab,
        marquee_fps,
    );
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

    run_render_loop(fb, &window, &shell, bt.as_ref(), &commands, preview_secs);
}

/// Executes the core appliance daemon with parsed options.
#[must_use]
pub fn run_app(opts: &CoreCliOptions) -> ExitCode {
    let temp_sensor = TempSensor::new();

    tracing::info!(
        target = "arm-cortex-a53",
        "Speaker Core Service running on Raspberry Pi 3 (aarch64)"
    );
    tracing::info!(
        thermal_sensor = %temp_sensor.gpio_info(),
        "Core service started"
    );

    #[cfg(feature = "ui")]
    {
        let path = CoreConfig::resolve_path(opts.config.as_deref());
        match CoreConfig::load(&path) {
            Ok(cfg) => {
                if let Some(t) = cfg.theme {
                    tracing::info!("Configured startup theme: {t}");
                }
                run_ui_display(
                    opts.screen,
                    opts.preview_secs,
                    cfg.theme,
                    opts.tab,
                    cfg.marquee_fps.value(),
                );
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
/// by `RUST_LOG` (default `info`, see [`crate::logging`]).
pub fn main() -> ExitCode {
    crate::logging::init_daemon_logging();
    match resolve_cli_opts(parse_cli_args_from(env_args()), "core", print_usage) {
        Ok(opts) => run_app(&opts),
        Err(exit_code) => exit_code,
    }
}
