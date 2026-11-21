//! Central appliance daemon and display orchestration service logic.
//!
//! Provides command-line options parsing, headless detection, display lifecycle
//! execution, and Slint framebuffer rendering coordination.

// `PathBuf` backs the unconditionally compiled `-c/--config` CLI field;
// `Path` is only referenced by the ui-gated DRM device probe.
#[cfg(feature = "ui")]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
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
use crate::ui::{
    AppShell, BootCheckLine, BootCheckStatus, BootCheckUpdate, BootProbe, BtDeviceRow,
    BtDevicesUpdate, MainWindow, SplashScreen, device_row,
};
#[cfg(feature = "ui")]
use slint::ComponentHandle;
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
    ///
    /// Tab 2 is retired (plan A6): `--tab bt|bluetooth|2` and `-B/--bt-devices`
    /// both resolve to opening the full-screen Bluetooth devices view.
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
    println!("  -B, --bt-devices            Open the Bluetooth devices screen on startup");
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
/// Tab `2` (`bt` / `bluetooth`) is retired as a dock tab: it resolves to
/// index 2, which [`init_shell_with_config`] maps to opening the full-screen
/// Bluetooth devices view (plan A6).
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
            // `-B` is sugar for the retired tab index 2: both open the
            // full-screen Bluetooth devices view (see `init_shell_with_config`).
            "-B" | "--bt-devices" => opts.tab = Some(2),
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

/// Runs the UI display loop with a config built from legacy CLI parameters.
#[cfg(feature = "ui")]
pub fn run_ui_display(
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    theme: Option<crate::ui::ThemePreset>,
    tab: Option<i32>,
    marquee_fps: u32,
) {
    let config = legacy_config(theme, marquee_fps);
    run_ui_display_with_config(screen, preview_secs, tab, &config, None);
}

/// Runs the requested screen applying the full [`CoreConfig`] parameters.
///
/// `config_path` is wired into the shell so the Screen Settings save action
/// persists the current state back to the active JSON configuration file.
#[cfg(feature = "ui")]
pub fn run_ui_display_with_config(
    screen: ScreenTarget,
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
    config_path: Option<&Path>,
) {
    if headless_requested() {
        tracing::info!("Headless mode requested via SPEAKER_CORE_HEADLESS=1");
        return;
    }
    if !Path::new("/dev/dri/card0").exists() {
        tracing::info!("Headless/simulated environment (no /dev/dri/card0 detected)");
        return;
    }
    crate::ui::runtime::run(crate::ui::runtime::RuntimeOptions {
        screen,
        preview_secs,
        tab,
        config,
        config_path,
        probe: BootProbe::real(),
        mocked: false,
    });
}

/// Returns `true` only when the headless override is explicitly set to `"1"`
/// (a value of `"0"` or any other content keeps the display enabled).
#[must_use]
pub fn headless_requested() -> bool {
    std::env::var_os("SPEAKER_CORE_HEADLESS").is_some_and(|v| v == "1")
}

/// Applies one boot-check update by invoking `set` with the rendered check line.
///
/// The setter abstraction lets both the standalone [`SplashScreen`] preview and
/// the [`MainWindow`] boot overlay consume the same update stream (the KMS
/// backend allows only one window per process).
#[cfg(feature = "ui")]
pub fn apply_check_update(set: &mut dyn FnMut(BootCheckLine), update: &BootCheckUpdate) {
    let line = BootCheckLine {
        text: slint::SharedString::from(&*update.text),
        running: update.status == BootCheckStatus::Running,
        ok: update.status == BootCheckStatus::Passed,
    };
    set(line);
}

/// Hard upper bound for the splash when no `preview_secs` window is given, so
/// a hung hardware probe (blocking D-Bus/I2C/sysfs call) can never stall the
/// appliance boot indefinitely.
pub const MAX_SPLASH_WAIT: Duration = Duration::from_secs(30);

/// Initializes and shows the [`SplashScreen`], applying an optional startup theme.
#[cfg(feature = "ui")]
#[must_use]
pub fn init_splash_screen(theme: Option<crate::ui::ThemePreset>) -> Option<SplashScreen> {
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

/// Drains all currently queued boot-check updates from `rx` through `set`,
/// returning `true` once the sender has disconnected (all checks settled).
#[cfg(feature = "ui")]
pub fn drain_boot_updates(
    set: &mut dyn FnMut(BootCheckLine),
    rx: &std::sync::mpsc::Receiver<BootCheckUpdate>,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(update) => apply_check_update(set, &update),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
        }
    }
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

/// Fully-initialized shell plus its Bluetooth transport wiring.
#[cfg(feature = "ui")]
#[derive(Debug)]
pub struct ShellInit {
    /// Application shell bound to the Slint window.
    pub shell: AppShell,
    /// Connected Bluetooth device handle: always `Some` for mocked
    /// sessions; `None` for live sessions, whose connect runs on the
    /// `bt-bootstrap` worker ([`spawn_bt_bootstrap_worker`]).
    pub bt: Option<speaker_bluetooth::BluetoothDevice>,
    /// Queue bridging render-thread transport callbacks to the `bt-monitor` worker.
    pub commands: speaker_bluetooth::TransportCommandQueue,
    /// Queue bridging render-thread volume/mute actions to the `amp-volume` worker.
    pub amp_commands: crate::volume::AmpVolumeCommandQueue,
    /// Queue bridging render-thread brightness actions to the `brightness` worker.
    pub brightness_commands: crate::screen_brightness::BrightnessCommandQueue,
    /// Initial auto-mode preference from the appliance configuration.
    pub brightness_auto_enabled: bool,
    /// Initial manual percent used when auto mode is disabled at boot.
    pub brightness_manual_percent: u8,
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
///
/// Never blocks on D-Bus: live sessions defer the `BlueZ` connect and the
/// initial playback/link snapshots to [`spawn_bt_bootstrap_worker`], which
/// runs on a worker thread so first paint is never delayed.
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
    // Brightness is controlled by the embedded engine on the brightness
    // worker (single sysfs writer), so its queue is wired unconditionally,
    // like the amp queue.
    let brightness_commands = crate::screen_brightness::BrightnessCommandQueue::new();
    shell.wire_brightness(&brightness_commands);
    // The Devices screen is always present in the shell, so its command queue
    // is wired unconditionally (parity with the amp queue): navigation
    // callbacks stay functional even when the D-Bus connection failed at boot.
    let device_commands = speaker_bluetooth::DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&device_commands);
    // Mocked sessions initialize eagerly against an in-memory device (no
    // system bus involved). Live sessions never connect here: the blocking
    // `BlueZ` connect and the initial snapshots run on the `bt-bootstrap`
    // worker thread (see [`spawn_bt_bootstrap_worker`]), so first paint is
    // never delayed by a slow system bus.
    let maybe_bt = mocked_device.then(|| {
        let (mock_bt, _) = speaker_bluetooth::BluetoothDevice::new_mocked();
        apply_initial_bt_state(&shell, &mock_bt);
        shell.wire_bluetooth_transport(&commands);
        mock_bt
    });
    match tab {
        // Tab 2 is retired (plan A6): `--tab bt|bluetooth|2` and
        // `-B/--bt-devices` open the full-screen devices view instead of a
        // dock tab, so `set_tab(2)` must never be applied. Live sessions
        // bootstrap lazily, so availability is unknowable at init: the
        // screen opens unconditionally and renders its empty/no-adapter
        // state until the worker publishes rows.
        Some(2) => shell.window().invoke_open_bt_devices(),
        Some(tb) => shell.set_tab(tb),
        None => {}
    }
    ShellInit {
        shell,
        bt: maybe_bt,
        commands,
        amp_commands,
        brightness_commands,
        brightness_auto_enabled: config.auto_brightness_enabled,
        brightness_manual_percent: config.brightness_manual_percent,
        device_commands,
    }
}

#[cfg(feature = "ui")]
/// Applies the initial playback/link snapshots to `shell` (best-effort).
///
/// Query failures are logged at WARN and leave the shell at its defaults —
/// never a panic or a blank flash (called from [`init_shell`] for mocked
/// sessions; tests inject mocked or faulted devices directly).
pub fn apply_initial_bt_state(shell: &AppShell, bt: &speaker_bluetooth::BluetoothDevice) {
    let active = bt.active_device();
    let preferred = match bt.playback_state() {
        Ok(Some(pb)) => {
            shell.apply_bluetooth_playback(&pb);
            active.or(pb.device_address)
        }
        Ok(None) => active,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query initial Bluetooth playback state");
            active
        }
    };
    match bt.link_snapshot() {
        Ok(ref snapshot) => {
            let details = crate::ui::BtLinkDetails::select(snapshot, preferred);
            if let Some(ref pcm) = details.pcm {
                shell.apply_pcm_stream(pcm);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query initial Bluetooth link snapshot");
        }
    }
}

/// Spawns the Bluetooth bootstrap worker for live sessions.
///
/// The blocking `BlueZ` connect AND the initial playback/link snapshot reads
/// happen on this worker thread — never on the render thread — so a slow or
/// unresponsive system bus can no longer stall the first paint, the splash
/// updates, the input handling or the preview deadline. The worker publishes
/// the resulting state through the shared update channel; the render thread
/// applies it in [`apply_shared_bt_state`].
///
/// `connected` is raised once the connect succeeded so the render thread can
/// wire the transport callbacks (the shell is not `Send`, so that step must
/// happen on the render thread).
///
/// The `connect` factory is injectable so integration tests exercise the
/// worker against `new_mocked()` devices without touching the system bus
/// ("Ensure Hermetic Tests").
///
/// A failed connect is retried with [`BT_BOOTSTRAP_RETRY`] backoff (see
/// [`spawn_bt_bootstrap_worker_with_retry`]) rather than abandoning the
/// `device_commands` queue forever.
///
/// # Errors
/// Returns the underlying thread-spawn I/O error when the OS refuses to
/// create the worker thread.
#[cfg(feature = "ui")]
pub fn spawn_bt_bootstrap_worker(
    connect: impl Fn() -> Result<speaker_bluetooth::BluetoothDevice, speaker_bluetooth::BtError>
    + Send
    + 'static,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    device_commands: &speaker_bluetooth::DeviceCommandQueue,
    running: &Arc<AtomicBool>,
    connected: &Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_bt_bootstrap_worker_with_retry(
        connect,
        shared,
        commands,
        device_commands,
        running,
        connected,
        BT_BOOTSTRAP_RETRY,
    )
}

/// Production delay between failed Bluetooth connect attempts by the
/// `bt-bootstrap` worker.
///
/// A transient system-bus/D-Bus failure at boot (service started before
/// `dbus`/`bluetoothd` finished coming up) must not permanently orphan the
/// devices command queue: the worker keeps retrying with this backoff and
/// publishes a toast per attempt so the screen can surface the failure
/// instead of silently doing nothing.
#[cfg(feature = "ui")]
pub const BT_BOOTSTRAP_RETRY: Duration = Duration::from_secs(3);

/// [`spawn_bt_bootstrap_worker`] with a test-injectable retry delay.
///
/// Production code keeps the unconditional [`BT_BOOTSTRAP_RETRY`] constant;
/// integration tests pass a short real-time delay so retry, queue-drain, and
/// shutdown behavior is exercised without slow tests (repo rule:
/// "Unconditional Production Constants; Tests Inject Deadlines").
///
/// # Errors
/// Returns the underlying thread-spawn I/O error when the OS refuses to
/// create the worker thread.
#[cfg(feature = "ui")]
pub fn spawn_bt_bootstrap_worker_with_retry(
    connect: impl Fn() -> Result<speaker_bluetooth::BluetoothDevice, speaker_bluetooth::BtError>
    + Send
    + 'static,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    device_commands: &speaker_bluetooth::DeviceCommandQueue,
    running: &Arc<AtomicBool>,
    connected: &Arc<AtomicBool>,
    retry: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let shared = Arc::clone(shared);
    let commands = commands.clone();
    let device_commands = device_commands.clone();
    let running = Arc::clone(running);
    let connected = Arc::clone(connected);
    std::thread::Builder::new()
        .name("bt-bootstrap".into())
        .spawn(move || {
            loop {
                match connect() {
                    Ok(bt) => {
                        connected.store(true, Ordering::Release);
                        // The monitor's first tick publishes the initial
                        // playback/link snapshots into the shared update channel;
                        // the render thread applies them — no D-Bus round trip
                        // ever runs on the UI thread.
                        bt_monitor_loop(&bt, &shared, &commands, &device_commands, &running);
                        return;
                    }
                    Err(e) => {
                        report_display(
                            tracing::Level::WARN,
                            "Bluetooth connect failed; scheduling a retry",
                            &e,
                        );
                        // Commands queued by the devices screen while the bus is
                        // down are drained and logged rather than silently
                        // orphaned: without a worker they would never execute.
                        let dropped = device_commands.drain();
                        if !dropped.is_empty() {
                            tracing::warn!(
                                dropped = dropped.len(),
                                "dropped device commands: Bluetooth connection unavailable"
                            );
                        }
                        // Surface the failure on the devices screen: a toast the
                        // shell applies like any worker-published delta.
                        let update = BtSharedUpdate {
                            devices: Some(crate::ui::BtDevicesUpdate {
                                toast: Some(
                                    format!(
                                        "Bluetooth unavailable ({}); retrying…",
                                        speaker_bluetooth::device_action_message(&e)
                                    )
                                    .into(),
                                ),
                                ..crate::ui::BtDevicesUpdate::default()
                            }),
                            ..BtSharedUpdate::default()
                        };
                        BtMonitorState::commit_update(&shared, update);
                        // Backoff in slices so a shutdown request
                        // (`running = false`) terminates the worker promptly
                        // instead of after the full delay.
                        let mut remaining = retry;
                        while !remaining.is_zero() && running.load(Ordering::Acquire) {
                            let slice = remaining.min(Duration::from_millis(100));
                            std::thread::sleep(slice);
                            remaining = remaining.saturating_sub(slice);
                        }
                        if !running.load(Ordering::Acquire) {
                            return;
                        }
                    }
                }
            }
        })
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
    /// Authoritative display brightness snapshot, published by the dedicated
    /// `brightness` worker (never read from D-Bus on the render thread).
    pub brightness: Option<crate::screen_brightness::BrightnessState>,
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
    let fallback_addr = last_pb.as_ref().and_then(|p| p.device_address);
    match bt.playback_state_for(fallback_addr) {
        Ok(Some(ref pb)) => {
            // Compute device_changed before settle-expiry resets `last_pb` to
            // `None`, otherwise the force-publish tick on the same device
            // would look like a device switch and clear prior cover art.
            let device_changed =
                last_pb.as_ref().and_then(|p| p.device_address) != pb.device_address;
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
            if cover_path.is_none() && device_changed && cover.path.is_some() {
                // Switched to a device without cover art (e.g. a laptop with
                // no BIP service): clear the previous device's artwork so the
                // old phone's cover never lingers on top of the new track.
                update.cover = Some(None);
                *cover = CoverState::default();
            } else if (cover.path != cover_path || retry_due)
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
    /// Last published discovery-spinner flag (`None` = invalidated by
    /// `clear_row_dedup` so the next successful snapshot re-publishes rows).
    last_scanning: Option<bool>,
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
    /// One-shot flag set when a Connect/Disconnect/Remove command finishes.
    staged_action_settled: bool,
}

#[cfg(feature = "ui")]
impl BtDevicesMonitorState {
    /// Marks row/alias/scanning dedup dirty so the next successful snapshot
    /// re-publishes full screen state without erasing `last_rows` /
    /// `last_discovered` (a transient snapshot failure right after reset still
    /// preserves the last known rows).
    fn clear_row_dedup(&mut self) {
        self.last_alias = None;
        self.last_scanning = None;
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
    last_active_pcm: Option<speaker_bluetooth::BlueAlsaPcm>,
    last_active_device: Option<BtAddress>,
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

    /// Queries one lightweight link snapshot, stages PCM changes into `update`,
    /// and refreshes the cached link details used to pick the active device row.
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
        let active_addr = bt.active_device();
        self.last_active_device = active_addr;
        let active_pcm =
            speaker_bluetooth::select_active_pcm_for(&snapshot.pcms, active_addr).cloned();
        if self.last_pcms.as_ref() != Some(&snapshot.pcms) || self.last_active_pcm != active_pcm {
            if let Some(ref active) = active_pcm {
                update.pcm = Some(active.clone());
            }
            self.last_pcms = Some(snapshot.pcms.clone());
            self.last_active_pcm = active_pcm;
        }
        let preferred =
            active_addr.or_else(|| self.last_pb.as_ref().and_then(|pb| pb.device_address));
        self.last_details = Some(crate::ui::BtLinkDetails::select(&snapshot, preferred));
        Some(snapshot)
    }

    /// Resets dedup state when a new command arrives mid-query so the next tick
    /// performs a full authoritative publish after executing the fresh command.
    pub fn reset_dedup(&mut self) {
        self.last_pb = None;
        self.cover.path = None;
        self.last_pcms = None;
        self.last_active_pcm = None;
        self.last_active_device = None;
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

    /// Returns `true` if `row` refers to `addr` (address-keyed lookup across
    /// publishes; row indices are never trusted — plan A2).
    fn row_matches(row: &crate::ui::BtDeviceRow, addr: BtAddress) -> bool {
        BtAddress::parse_str(row.address.as_str()).is_ok_and(|a| a == addr)
    }

    /// Resolves a device's display name from the last published rows,
    /// falling back to the raw address for devices never seen on screen.
    fn device_name(&self, addr: BtAddress) -> Box<str> {
        self.devices
            .last_rows
            .iter()
            .chain(self.devices.last_discovered.iter())
            .find(|row| Self::row_matches(row, addr))
            .map_or_else(
                || addr.to_string().into_boxed_str(),
                |row| row.name.to_string().into_boxed_str(),
            )
    }

    /// Returns `true` if the address maps to a connected row on screen, so
    /// `Remove` can disconnect first (`BlueZ` rejects `RemoveDevice` while the
    /// device is connected — plan §12).
    fn device_row_connected(&self, addr: BtAddress) -> bool {
        self.devices
            .last_rows
            .iter()
            .any(|row| row.connected && Self::row_matches(row, addr))
    }

    /// Executes one drained Devices-screen command against `bt`.
    ///
    /// Failures stage a toast (never a panic): the screen keeps working and
    /// the next authoritative snapshot reconciles the UI. Each variant's
    /// behavior lives in its own small helper so this dispatcher stays a flat
    /// guard + resolve + match (cargo-crap: complexity budget).
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
        if matches!(command, DeviceCommand::CancelPairing) && self.devices.pairing.is_none() {
            tracing::debug!(?command, "Ignoring pairing control with no active attempt");
            return;
        }
        let Some(adapter) = self.resolve_adapter_id(bt) else {
            self.devices.staged_toast = Some("Bluetooth adapter not available".into());
            self.devices.staged_action_settled = true;
            return;
        };
        match command {
            // Handled above; the arm exists so new variants stay reviewed.
            DeviceCommand::Refresh => {}
            DeviceCommand::StartDiscovery => self.start_screen_discovery(bt, &adapter),
            DeviceCommand::StopDiscovery => self.stop_screen_discovery(bt, &adapter),
            DeviceCommand::Connect(addr) => self.connect_device_row(bt, &adapter, addr),
            DeviceCommand::Disconnect(addr) => self.disconnect_device_row(bt, &adapter, addr),
            DeviceCommand::Remove(addr) => self.remove_device_row(bt, &adapter, addr),
            DeviceCommand::Pair(addr) => {
                self.begin_pairing(bt, &adapter, addr, pairing_timeout);
            }
            DeviceCommand::CancelPairing => self.cancel_pairing_session(bt, &adapter),
        }
    }

    /// Starts adapter discovery for the Add Device screen.
    fn start_screen_discovery(&mut self, bt: &speaker_bluetooth::BluetoothDevice, adapter: &str) {
        match bt.start_discovery(adapter) {
            Ok(()) => self.devices.discovery_from_screen = true,
            Err(e) => self.stage_action_failure(&e),
        }
    }

    /// Stops adapter discovery, but only scans this screen started: scopes RF
    /// scanning to the screen (pairing reap and explicit Cancel also clear the
    /// flag), and avoids error toasts for never-started scans.
    fn stop_screen_discovery(&mut self, bt: &speaker_bluetooth::BluetoothDevice, adapter: &str) {
        if !self.devices.discovery_from_screen {
            tracing::debug!("stop discovery skipped: not screen-started");
            return;
        }
        match bt.stop_discovery(adapter) {
            Ok(()) => self.devices.discovery_from_screen = false,
            Err(e) => self.stage_action_failure(&e),
        }
    }

    /// Connects a paired/discovered device row.
    fn connect_device_row(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        adapter: &str,
        addr: BtAddress,
    ) {
        let name = self.device_name(addr);
        if let Err(e) = bt.connect_device(adapter, addr) {
            tracing::warn!(device = %name, error = %e, "Connect failed");
            self.stage_action_failure(&e);
        }
        self.devices.staged_action_settled = true;
    }

    /// Disconnects the active device row.
    fn disconnect_device_row(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        adapter: &str,
        addr: BtAddress,
    ) {
        let name = self.device_name(addr);
        if let Err(e) = bt.disconnect_device(adapter, addr) {
            tracing::warn!(device = %name, error = %e, "Disconnect failed");
            self.stage_action_failure(&e);
        }
        self.devices.staged_action_settled = true;
    }

    /// Removes a paired device (`BlueZ` refuses connected devices, so a
    /// best-effort disconnect happens first — plan §12).
    fn remove_device_row(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        adapter: &str,
        addr: BtAddress,
    ) {
        if self.device_row_connected(addr)
            && let Err(e) = bt.disconnect_device(adapter, addr)
        {
            tracing::warn!(error = %e, "Disconnect before remove failed; continuing");
        }
        let name = self.device_name(addr);
        if let Err(e) = bt.remove_device(adapter, addr) {
            tracing::warn!(device = %name, error = %e, "Remove failed");
            self.stage_action_failure(&e);
        }
        self.devices.staged_action_settled = true;
    }

    /// Cancels an in-flight pairing attempt on the adapter.
    fn cancel_pairing_session(&mut self, bt: &speaker_bluetooth::BluetoothDevice, adapter: &str) {
        if let Some(session) = self.devices.pairing.as_mut()
            && !session.cancel_requested
        {
            session.cancel_requested = true;
            if let Err(e) = bt.cancel_pairing(adapter, session.addr) {
                tracing::warn!(error = %e, "Pairing cancellation failed");
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
        if let Some(session) = &self.devices.pairing {
            tracing::debug!(device = %addr, "Pairing already in progress");
            // Ride the live session's status along with the refusal toast: a
            // bare toast-only publish (pairing: None) would let the shell's
            // refusal convergence release the pairing view while this attempt
            // is still running (review: pairing-view desync on retry).
            let ride_along = session.terminal.clone().unwrap_or_else(|| {
                if session.cancel_requested {
                    PairingStatus::Cancelled
                } else {
                    PairingStatus::Connecting {
                        name: session.name.clone(),
                    }
                }
            });
            self.devices.staged_toast = Some("Operation already in progress".into());
            if self.devices.staged_pairing.is_none() {
                self.devices.staged_pairing = Some(ride_along);
            }
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

        self.publish_pairing_code(&mut session);

        // Terminal derive: thread completion outranks cancellation, which
        // outranks the deadline; a still-pending session is put back untouched.
        let Some(terminal) = self.derive_pairing_terminal(bt, &mut session) else {
            self.devices.pairing = Some(session);
            return;
        };
        self.stage_pairing_terminal(bt, session, &terminal);
    }

    /// Publishes the passkey exactly once, and only from a slot recorded for
    /// THIS device: a stale value from a previous attempt must never show.
    fn publish_pairing_code(&mut self, session: &mut PairingSession) {
        if session.code_published {
            return;
        }
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

    /// Derives the terminal status for this tick: thread completion outranks
    /// cancellation, which outranks the deadline. `None` = still pending (the
    /// session goes back untouched).
    fn derive_pairing_terminal(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        session: &mut PairingSession,
    ) -> Option<PairingStatus> {
        if Self::pairing_finished(session) {
            if let Some(handle) = session.handle.take()
                && let Err(panic) = handle.join()
            {
                std::panic::resume_unwind(panic);
            }
            let mut result = session.result.lock().unwrap_or_else(|p| {
                tracing::warn!("Pairing result slot poisoned; recovering");
                p.into_inner()
            });
            return Some(match result.take() {
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
            });
        }
        if session.cancel_requested {
            return Some(PairingStatus::Cancelled);
        }
        if Instant::now() >= session.deadline_at {
            session.cancel_requested = true;
            if let Some(adapter) = self.resolve_adapter_id(bt)
                && let Err(e) = bt.cancel_pairing(&adapter, session.addr)
            {
                tracing::warn!(error = %e, "Timeout pairing cancellation failed");
            }
            self.devices.staged_toast = Some("Pairing timed out".into());
            return Some(PairingStatus::TimedOut);
        }
        None
    }

    /// Records a terminal status on the session, publishes the pairing
    /// transition, and surfaces pairing failures as a toast (the pairing
    /// view silently popping back with no explanation reads as "still
    /// pending" and invites blind retries).
    ///
    /// A session completed this tick is reaped immediately so the agent
    /// releases without waiting out the grace window; otherwise it goes back
    /// with its terminal recorded.
    fn stage_pairing_terminal(
        &mut self,
        bt: &speaker_bluetooth::BluetoothDevice,
        mut session: PairingSession,
        terminal: &PairingStatus,
    ) {
        session.terminal = Some(terminal.clone());
        self.devices.staged_pairing = Some(terminal.clone());
        if matches!(terminal, PairingStatus::Paired { .. })
            && let Some(adapter) = self.resolve_adapter_id(bt)
            && let Err(e) = bt.set_trusted(&adapter, session.addr, true)
        {
            tracing::warn!(device = %session.addr, error = %e, "Failed to mark paired device Trusted");
        }
        if let PairingStatus::Failed { message } = terminal {
            self.devices.staged_toast = Some(message.clone());
        }
        if Self::pairing_finished(&session) {
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
        // Active device = arbitrated `ActiveDevice` (falling back to the active
        // playback player) so only the playing source reads "Active" while
        // other connected standby devices read "Connected".
        let active = self
            .last_active_device
            .or_else(|| self.last_pb.as_ref().and_then(|pb| pb.device_address));
        // Paired/active list: only bonded (or connected) devices.
        let rows = snapshot
            .devices
            .iter()
            .filter(|device| device.paired || device.connected)
            .map(|device| device_row(device, active))
            .collect();
        // Discovery list: only unpaired, unconnected devices so "Add Device"
        // presents a dedicated screen of new devices to pair rather than
        // repeating the paired/connected list.
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
        let staged_action_settled = std::mem::take(&mut self.devices.staged_action_settled);
        let mut changed = false;
        if let Some(built) = built {
            let row_count = built.rows.len();
            let discovered_count = built.discovered.len();
            if built.rows != self.devices.last_rows {
                self.devices.last_rows = built.rows;
                changed = true;
            }
            if built.discovered != self.devices.last_discovered {
                tracing::debug!(
                    discovered = discovered_count,
                    paired_rows = row_count,
                    "Bluetooth discovery list changed"
                );
                self.devices.last_discovered = built.discovered;
                changed = true;
            }
            if self.devices.last_scanning != Some(built.scanning) {
                self.devices.last_scanning = Some(built.scanning);
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
                || staged_action_settled
            {
                self.stage_devices_publish(
                    update,
                    staged_pairing,
                    staged_toast,
                    staged_alias,
                    staged_action_settled,
                );
            }
            return;
        }
        // Snapshot failed: only staged events publish; rows stay as-is.
        if staged_pairing.is_some() || staged_toast.is_some() || staged_action_settled {
            self.stage_devices_publish(
                update,
                staged_pairing,
                staged_toast,
                None,
                staged_action_settled,
            );
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
        action_settled: bool,
    ) {
        update.devices = Some(BtDevicesUpdate {
            rows: self.devices.last_rows.clone(),
            discovered: self.devices.last_discovered.clone(),
            scanning: self.devices.last_scanning.unwrap_or(false),
            pairing,
            toast,
            adapter_alias,
            action_settled,
        });
    }

    /// Merges non-empty delta fields from `update` into `shared`, recovering from
    /// a poisoned mutex guard.
    pub fn commit_update(shared: &Mutex<BtSharedUpdate>, update: BtSharedUpdate) {
        if update.playback.is_some()
            || update.pcm.is_some()
            || update.cover.is_some()
            || update.amp_volume.is_some()
            || update.brightness.is_some()
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
            if update.cover.is_some() {
                lock.cover = update.cover;
            }
            if update.amp_volume.is_some() {
                lock.amp_volume = update.amp_volume;
            }
            if update.brightness.is_some() {
                lock.brightness = update.brightness;
            }
            if let Some(mut next) = update.devices {
                if let Some(prev) = lock.devices.take() {
                    next.pairing = next.pairing.or(prev.pairing);
                    next.toast = next.toast.or(prev.toast);
                    next.adapter_alias = next.adapter_alias.or(prev.adapter_alias);
                    next.action_settled |= prev.action_settled;
                }
                lock.devices = Some(next);
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

/// Polls the EC12 rotary encoder and applies volume/mute deltas to the shell,
/// requesting a redraw when the encoder changed UI state.
#[cfg(feature = "ui")]
pub fn poll_encoder_input(
    encoder_dev: Option<&speaker_hardware::EncoderDevice>,
    encoder_tracker: &mut crate::ui::EncoderVolumeTracker,
    shell: &AppShell,
    encoder_warned: &mut bool,
) {
    let Some(enc) = encoder_dev else { return };
    match encoder_tracker.poll_and_apply(enc, shell) {
        Ok(changed) => {
            *encoder_warned = false;
            if changed {
                shell.window().window().request_redraw();
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
pub fn join_amp_volume_worker(amp_worker: std::io::Result<std::thread::JoinHandle<()>>) {
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

/// Joins the `brightness` worker, re-raising a worker panic (fail-fast).
#[cfg(feature = "ui")]
pub fn join_brightness_worker(brightness_worker: std::io::Result<std::thread::JoinHandle<()>>) {
    match brightness_worker {
        Ok(handle) => {
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(e) => tracing::error!(
            error = %e,
            "Failed to spawn brightness worker thread; screen brightness disabled"
        ),
    }
}

/// Spawns the embedded brightness control-loop worker thread.
///
/// The worker owns the BH1750 sensor and the backlight sysfs handle, so no
/// separate daemon or D-Bus service is involved. Mocked sessions use in-memory
/// hardware so host tests never touch I²C or sysfs.
///
/// # Errors
/// Returns the underlying thread-spawn I/O error.
#[cfg(feature = "ui")]
pub fn spawn_brightness_worker(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &Arc<AtomicBool>,
    brightness_commands: &crate::screen_brightness::BrightnessCommandQueue,
    mocked: bool,
    auto_enabled: bool,
    manual_percent: u8,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let shared = Arc::clone(shared);
    let running = Arc::clone(running);
    let brightness_commands = brightness_commands.clone();
    let policy = crate::screen_brightness::default_policy();
    std::thread::Builder::new()
        .name("brightness".into())
        .spawn(move || {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            let (sensor, backlight) = if mocked {
                let (sensor, _sensor_ctrl) = speaker_hardware::LightSensor::new_mocked();
                let (backlight, _backlight_ctrl) = speaker_hardware::Backlight::new_mocked();
                (sensor, backlight)
            } else {
                let sensor = match speaker_hardware::LightSensor::new() {
                    Ok(sensor) => sensor,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to open BH1750 sensor; brightness disabled");
                        return;
                    }
                };
                let (backlight, device) =
                    match speaker_hardware::Backlight::with_appliance_default() {
                        Ok(handle) => handle,
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to open backlight sysfs device; brightness disabled");
                            return;
                        }
                    };
                tracing::info!(device = %device, "Backlight sysfs device selected");
                (sensor, backlight)
            };
            let mut engine = match crate::screen_brightness::BrightnessEngine::try_new(
                sensor,
                backlight,
                policy,
                auto_enabled,
            ) {
                Ok(engine) => engine,
                Err(e) => {
                    tracing::error!(error = %e, "Brightness hardware bootstrap failed; brightness disabled");
                    return;
                }
            };
            if !auto_enabled {
                let command = crate::screen_brightness::BrightnessCommand::SetPercent(f32::from(
                    manual_percent,
                ));
                let _ = engine.tick(
                    std::time::Instant::now(),
                    0.0,
                    &[command],
                    crate::screen_brightness::BRIGHTNESS_IDLE_RESET,
                );
            }
            crate::screen_brightness::run_worker(
                &mut engine,
                &shared,
                &brightness_commands,
                &running,
                crate::screen_brightness::BRIGHTNESS_IDLE_RESET,
            );
        })
}

/// Spawns the `bt-monitor` worker thread for a live Bluetooth device.
///
/// # Errors
/// Returns the underlying thread-spawn I/O error when the OS refuses to
/// create the `bt-monitor` worker thread.
#[cfg(feature = "ui")]
pub fn spawn_bt_monitor_worker(
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
/// the runtime's `mocked` flag, not on Bluetooth availability: a live
/// appliance whose Bluetooth D-Bus connection failed at startup must still
/// drive the real amplifier, never a silent mock.
///
/// # Errors
/// Returns the underlying thread-spawn I/O error when the OS refuses to
/// create the `amp-volume` worker thread.
#[cfg(feature = "ui")]
pub fn spawn_amp_volume_worker(
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

/// Applies queued Bluetooth playback/PCM/cover updates from the worker to the shell.
#[cfg(feature = "ui")]
pub fn apply_shared_bt_state(shell: &AppShell, shared_state: &Arc<Mutex<BtSharedUpdate>>) {
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
        if let Some(brightness) = lock.brightness.take() {
            shell.apply_brightness(&brightness);
        }
    }
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
                run_ui_display_with_config(
                    opts.screen,
                    opts.preview_secs,
                    opts.tab,
                    &cfg,
                    Some(path.as_path()),
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
