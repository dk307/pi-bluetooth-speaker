//! Single-window, single-event-loop application runtime (KMS backend).
//!
//! The DRM/KMS backend allows exactly ONE window and ONE `run_event_loop()`
//! call for the process lifetime (see `docs/kms_femtovg_migration_plan.md`),
//! so all screen targets are expressed as phases driven by [`slint::Timer`]s
//! inside one event loop instead of the former sequential render loops:
//!
//! - [`ScreenTarget::Splash`]: standalone [`SplashScreen`]; boot checks drain
//!   on a 16 ms timer and the loop quits when they settle or the deadline
//!   (`preview_secs` / [`daemon::MAX_SPLASH_WAIT`]) expires.
//! - [`ScreenTarget::Shell`]: [`MainWindow`] only; Bluetooth/encoder/preview
//!   timers drive the session until quit.
//! - [`ScreenTarget::Appliance`]: [`MainWindow`] with the boot splash shown as
//!   an in-window overlay (`boot_splash_visible`) while boot checks run, then
//!   the overlay is hidden and the shell timers take over.
//!
//! Tests initialize the hermetic [`super::framebuffer::FramebufferPlatform`]
//! via [`super::framebuffer::Framebuffer::init_platform`] BEFORE calling
//! [`run`]; production relies on the lazily selected `backend-linuxkms`.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use crate::config::CoreConfig;
use crate::daemon::{self, BtSharedUpdate, ScreenTarget, ShellInit, report_display};
use crate::ui::theme::ThemePreset;
use crate::ui::{BootCheckLine, BootCheckUpdate, BootProbe, MainWindow};

/// Everything needed to start one UI session.
pub struct RuntimeOptions<'a> {
    /// Screen target selecting the runtime phase flow.
    pub screen: ScreenTarget,
    /// Optional preview duration in seconds bounding the whole session.
    pub preview_secs: Option<u64>,
    /// Navigation dock tab to open on startup (0 to 5).
    pub tab: Option<i32>,
    /// Fully resolved appliance configuration.
    pub config: &'a CoreConfig,
    /// JSON config path wired into the shell's Screen Settings save action.
    pub config_path: Option<&'a Path>,
    /// Boot-check probe (real hardware or scripted for tests).
    pub probe: BootProbe,
    /// `true` for hermetic host sessions: mocked Bluetooth, no system bus,
    /// no rotary encoder. Production always passes `false`.
    pub mocked: bool,
}

impl std::fmt::Debug for RuntimeOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOptions")
            .field("screen", &self.screen)
            .field("preview_secs", &self.preview_secs)
            .field("tab", &self.tab)
            .field("config", &self.config)
            .field("config_path", &self.config_path)
            .field("probe", &"<BootProbe>")
            .field("mocked", &self.mocked)
            .finish()
    }
}

/// Runs the UI for the requested screen target and returns once the event
/// loop exits (preview expiry, quit request, or last window closed), after
/// all background workers have been joined.
pub fn run(opts: RuntimeOptions<'_>) {
    let RuntimeOptions {
        screen,
        preview_secs,
        tab,
        config,
        config_path,
        probe,
        mocked,
    } = opts;
    tracing::info!(
        screen = ?screen,
        preview_secs = ?preview_secs,
        tab = ?tab,
        config_path = ?config_path,
        mocked,
        "UI runtime starting"
    );
    match screen {
        ScreenTarget::Splash => run_splash(probe, preview_secs, config.theme),
        ScreenTarget::Shell => {
            run_shell(preview_secs, tab, config, config_path, mocked);
        }
        ScreenTarget::Appliance => {
            run_appliance(probe, preview_secs, tab, config, config_path, mocked);
        }
    }
}

/// Requests the active event loop to exit, falling back to the test-harness
/// quit flag when the platform exposes no event-loop proxy.
fn request_quit() {
    if let Err(err) = slint::quit_event_loop() {
        tracing::debug!(
            error = %err,
            "quit_event_loop unavailable; signalling test loop directly"
        );
        super::framebuffer::signal_test_loop_quit();
    }
}

/// Runs the Slint event loop once, reporting any platform failure.
fn run_loop(label: &str) {
    if let Err(err) = slint::run_event_loop() {
        report_display(
            tracing::Level::ERROR,
            &format!("{label} event loop failed"),
            &err,
        );
    }
}

/// Spawns the `boot-checks` worker, degrading gracefully when the thread
/// cannot be spawned (M-PANIC-IS-STOP): boot checks are skipped with a
/// warning and the appliance still boots.
fn spawn_boot_checks(
    probe: BootProbe,
) -> (
    std::sync::mpsc::Receiver<BootCheckUpdate>,
    Option<std::thread::JoinHandle<()>>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<BootCheckUpdate>();
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
    (rx, checks)
}

/// Shared state of the boot-check drain timer.
struct BootDrain {
    rx: std::sync::mpsc::Receiver<BootCheckUpdate>,
    checks: Option<std::thread::JoinHandle<()>>,
    checks_done: bool,
    started: Instant,
    deadline: Duration,
}

impl BootDrain {
    /// Drains pending boot-check updates through `set`; returns
    /// `Some((checks, checks_done))` once the boot phase is over (all checks
    /// settled or the deadline expired), taking the worker handle so the
    /// finish path runs exactly once.
    fn poll(
        &mut self,
        set: &mut dyn FnMut(BootCheckLine),
    ) -> Option<(Option<std::thread::JoinHandle<()>>, bool)> {
        self.checks_done |= daemon::drain_boot_updates(set, &self.rx);
        (self.checks_done || self.started.elapsed() >= self.deadline)
            .then(|| (self.checks.take(), self.checks_done))
    }

    /// Abandons a still-running worker after an early loop exit (preview
    /// expiry) so the canonical warning is emitted instead of leaking it.
    fn abandon_leftover(&mut self) {
        if self.checks.is_some() {
            daemon::finish_boot_worker(self.checks.take(), false);
        }
    }
}

/// Starts the boot-check drain timer.
///
/// The returned timer handle must be kept alive (dropping a [`Timer`] cancels
/// it) and the returned drain state must be released with
/// [`BootDrain::abandon_leftover`] when the event loop ends, so a still
/// running boot-check worker is abandoned with a warning instead of leaking.
///
/// `apply` publishes each new check line; `settled` runs once when the checks
/// settle (or the deadline expires). The timer stops itself at that point, so
/// the 16 ms drain cadence never outlives the boot phase.
fn start_boot_phase(
    probe: BootProbe,
    deadline: Duration,
    apply: impl Fn(BootCheckLine) + 'static,
    settled: impl FnOnce() + 'static,
) -> (Rc<Timer>, Rc<RefCell<BootDrain>>) {
    let (rx, checks) = spawn_boot_checks(probe);
    let drain = Rc::new(RefCell::new(BootDrain {
        rx,
        checks_done: checks.is_none(),
        checks,
        started: Instant::now(),
        deadline,
    }));

    let drain_state = Rc::clone(&drain);
    let mut settled = Some(settled);
    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let finished = drain_state.borrow_mut().poll(&mut |line| apply(line));
        if let Some((checks, checks_done)) = finished {
            daemon::finish_boot_worker(checks, checks_done);
            if let Some(timer) = timer_weak.upgrade() {
                timer.stop();
            }
            if let Some(settled) = settled.take() {
                settled();
            }
        }
    });
    (timer, drain)
}

/// Standalone splash preview flow: boot checks drain into the single check
/// line and the loop quits on settle or deadline.
fn run_splash(probe: BootProbe, preview_secs: Option<u64>, theme: Option<ThemePreset>) {
    let Some(splash) = daemon::init_splash_screen(theme) else {
        return;
    };
    let weak_splash = splash.as_weak();
    let deadline = preview_secs.map_or(daemon::MAX_SPLASH_WAIT, Duration::from_secs);
    let (timer, drain) = start_boot_phase(
        probe,
        deadline,
        move |line| {
            if let Some(splash) = weak_splash.upgrade() {
                splash.set_current_check(line);
            }
        },
        request_quit,
    );
    let _timer = timer;

    tracing::info!(preview_secs = ?preview_secs, "Splash event loop starting");
    run_loop("Splash");
    drain.borrow_mut().abandon_leftover();
    tracing::info!("Splash event loop finished");
}

/// Background workers and periodic timers for one shell session.
///
/// Held alive across `run_event_loop()`; [`Session::shutdown`] stops the
/// workers after the loop returns.
struct Session {
    bg_running: Arc<AtomicBool>,
    amp_worker: std::io::Result<std::thread::JoinHandle<()>>,
    brightness_worker: std::io::Result<std::thread::JoinHandle<()>>,
    bt_worker: Option<std::io::Result<std::thread::JoinHandle<()>>>,
    _sync_timer: Timer,
    _encoder_timer: Option<Timer>,
}

impl Session {
    /// Spawns the Bluetooth/amp workers and starts the state-sync and
    /// rotary-encoder timers. The `AppShell` stays alive through the `Rc`
    /// clones captured by those timers.
    ///
    /// Mocked sessions spawn the `bt-monitor` worker eagerly from the mock
    /// device; live sessions spawn the `bt-bootstrap` worker, which performs
    /// the blocking `BlueZ` connect and the initial snapshot reads off the
    /// render thread and publishes the results through the shared update
    /// channel.
    fn start(init: ShellInit, mocked: bool) -> Self {
        let ShellInit {
            shell,
            bt,
            commands,
            amp_commands,
            brightness_commands,
            brightness_auto_enabled,
            brightness_manual_percent,
            device_commands,
        } = init;
        let shell = Rc::new(shell);
        let bg_running = Arc::new(AtomicBool::new(true));
        let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));

        let amp_worker =
            daemon::spawn_amp_volume_worker(&shared, &bg_running, &amp_commands, mocked);
        let brightness_worker = daemon::spawn_brightness_worker(
            &shared,
            &bg_running,
            &brightness_commands,
            mocked,
            brightness_auto_enabled,
            brightness_manual_percent,
        );
        // Raised by the bootstrap worker once the `BlueZ` connect succeeded.
        // The transport callbacks are wired on the render thread (the shell is
        // not `Send`), so the sync timer watches this flag.
        let bt_connected = Arc::new(AtomicBool::new(false));
        let bt_worker = Some(bt.as_ref().map_or_else(
            || {
                daemon::spawn_bt_bootstrap_worker(
                    speaker_bluetooth::BluetoothDevice::new_blocking,
                    &shared,
                    &commands,
                    &device_commands,
                    &bg_running,
                    &bt_connected,
                )
            },
            |device| {
                daemon::spawn_bt_monitor_worker(
                    device,
                    &shared,
                    &commands,
                    &device_commands,
                    &bg_running,
                )
            },
        ));

        // Periodic reconciliation of worker-published Bluetooth/amp state.
        let sync_timer = Timer::default();
        let shell_sync = Rc::clone(&shell);
        let commands_sync = commands.clone();
        let connected_sync = Arc::clone(&bt_connected);
        let transport_wired = Cell::new(false);
        sync_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            // Wire the transport once the live connect succeeded: before that
            // a transport tap has no worker to execute it, so it must stay a
            // no-op instead of flipping the optimistic UI state.
            if !transport_wired.get() && connected_sync.load(Ordering::Acquire) {
                transport_wired.set(true);
                shell_sync.wire_bluetooth_transport(&commands_sync);
            }
            daemon::apply_shared_bt_state(&shell_sync, &shared);
            // Release the frozen backdrop once both overlays (and their fade-out)
            // have finished closing so the next open captures a fresh snapshot.
            if shell_sync.window().get_overlay_backdrop_available()
                && !shell_sync.is_volume_hud_active()
                && !shell_sync.is_codec_hud_active()
                && !shell_sync.is_brightness_hud_active()
            {
                shell_sync.set_overlay_backdrop(slint::Image::default());
                tracing::debug!("Cleared overlay backdrop");
            }
        });

        // Physical EC12 encoder only exists on the live appliance (mocked
        // sessions must never open GPIO).
        let encoder_timer = (!mocked).then(|| {
            let timer = Timer::default();
            match speaker_hardware::EncoderDevice::new() {
                Ok(encoder) => {
                    let shell = Rc::clone(&shell);
                    let mut tracker = crate::ui::EncoderVolumeTracker::new();
                    let mut warned = false;
                    let encoder = Some(encoder);
                    timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
                        daemon::poll_encoder_input(
                            encoder.as_ref(),
                            &mut tracker,
                            &shell,
                            &mut warned,
                        );
                    });
                }
                Err(err) => {
                    report_display(
                        tracing::Level::WARN,
                        "Failed to initialize EC12 rotary encoder for volume control",
                        &err,
                    );
                }
            }
            timer
        });

        Self {
            bg_running,
            amp_worker,
            brightness_worker,
            bt_worker,
            _sync_timer: sync_timer,
            _encoder_timer: encoder_timer,
        }
    }

    /// Stops the background workers after the event loop returned, re-raising
    /// any worker panic (M-PANIC-ON-BUG) instead of swallowing it.
    fn shutdown(self) {
        self.bg_running.store(false, Ordering::Relaxed);
        daemon::join_amp_volume_worker(self.amp_worker);
        daemon::join_brightness_worker(self.brightness_worker);
        match self.bt_worker {
            Some(Ok(handle)) => {
                if let Err(panic) = handle.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Some(Err(err)) => {
                report_display(
                    tracing::Level::WARN,
                    "bt-monitor thread could not be started",
                    &err,
                );
            }
            // The Bluetooth worker was never spawned (mocked session without a
            // device): no worker to join.
            None => {}
        }
        tracing::info!("UI session workers stopped");
    }
}

/// Creates the shell main window, reporting a failure to the display log.
fn create_main_window() -> Option<MainWindow> {
    match MainWindow::new() {
        Ok(win) => Some(win),
        Err(err) => {
            report_display(
                tracing::Level::ERROR,
                "Failed to initialize UI Shell MainWindow",
                &err,
            );
            None
        }
    }
}

/// Initializes the shell for `main_window` and shows it.
///
/// Returns `None` when the window could not be shown: the caller must not
/// spawn any worker in that case.
fn init_and_show_shell(
    main_window: MainWindow,
    mocked: bool,
    tab: Option<i32>,
    config: &CoreConfig,
    config_path: Option<&Path>,
) -> Option<ShellInit> {
    let init = daemon::init_shell_with_config(main_window, mocked, tab, config);
    if let Some(path) = config_path {
        init.shell.wire_config(path);
    }
    if let Err(err) = init.shell.window().show() {
        report_display(
            tracing::Level::ERROR,
            "Failed to show UI Shell MainWindow",
            &err,
        );
        return None;
    }
    Some(init)
}

/// Runs the shell session (background workers + event loop) until the preview
/// deadline expires or a quit is requested.
///
/// `after_loop` runs between the event loop returning and the workers being
/// stopped, so a still-running boot-check worker can be abandoned first.
fn run_shell_session(
    init: ShellInit,
    mocked: bool,
    preview_secs: Option<u64>,
    phase: &'static str,
    after_loop: impl FnOnce(),
) {
    let session = Session::start(init, mocked);
    if let Some(secs) = preview_secs {
        Timer::single_shot(Duration::from_secs(secs), request_quit);
    }

    tracing::info!(preview_secs = ?preview_secs, phase, "event loop starting");
    run_loop(phase);
    after_loop();
    session.shutdown();
    tracing::info!(phase, "event loop finished");
}

/// Shell-only flow: main window, background workers, sync/encoder timers,
/// optional preview quit.
fn run_shell(
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
    config_path: Option<&Path>,
    mocked: bool,
) {
    let Some(main_window) = create_main_window() else {
        return;
    };
    let Some(init) = init_and_show_shell(main_window, mocked, tab, config, config_path) else {
        return;
    };
    run_shell_session(init, mocked, preview_secs, "Shell", || {});
}

/// Appliance flow: one `MainWindow` whose boot checks render as an in-window
/// overlay (`boot_splash_visible`) until they settle, then the shell session
/// takes over for the rest of the process lifetime.
fn run_appliance(
    probe: BootProbe,
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
    config_path: Option<&Path>,
    mocked: bool,
) {
    let Some(main_window) = create_main_window() else {
        return;
    };
    main_window.set_boot_splash_visible(true);
    let weak_window = main_window.as_weak();

    let Some(init) = init_and_show_shell(main_window, mocked, tab, config, config_path) else {
        return;
    };

    let deadline = preview_secs.map_or(daemon::MAX_SPLASH_WAIT, Duration::from_secs);
    let (apply_window, settle_window) = (weak_window.clone(), weak_window.clone());
    let (_boot_timer, drain) = start_boot_phase(
        probe,
        deadline,
        move |line| {
            if let Some(window) = apply_window.upgrade() {
                window.set_boot_current_check(line);
            }
        },
        move || {
            if let Some(window) = settle_window.upgrade() {
                window.set_boot_splash_visible(false);
            }
            tracing::info!("Boot checks settled; splash overlay hidden");
        },
    );

    run_shell_session(init, mocked, preview_secs, "Appliance", || {
        drain.borrow_mut().abandon_leftover();
    });
}
