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

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use crate::config::CoreConfig;
use crate::daemon::{self, BtSharedUpdate, ScreenTarget, ShellInit, report_display};
use crate::ui::theme::ThemePreset;
use crate::ui::{AppShell, BootCheckLine, BootCheckUpdate, BootProbe, MainWindow};

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
        probe,
        mocked,
    } = opts;
    tracing::info!(
        screen = ?screen,
        preview_secs = ?preview_secs,
        tab = ?tab,
        mocked,
        "UI runtime starting"
    );
    match screen {
        ScreenTarget::Splash => run_splash(probe, preview_secs, config.theme),
        ScreenTarget::Shell => run_shell(preview_secs, tab, config, mocked),
        ScreenTarget::Appliance => run_appliance(probe, preview_secs, tab, config, mocked),
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

/// Standalone splash preview flow: boot checks drain into the single check
/// line and the loop quits on settle or deadline.
fn run_splash(probe: BootProbe, preview_secs: Option<u64>, theme: Option<ThemePreset>) {
    let Some(splash) = daemon::init_splash_screen(theme) else {
        return;
    };
    let weak_splash = splash.as_weak();
    let (rx, checks) = spawn_boot_checks(probe);
    let drain = Rc::new(RefCell::new(BootDrain {
        rx,
        checks_done: checks.is_none(),
        checks,
        started: Instant::now(),
        deadline: preview_secs.map_or(daemon::MAX_SPLASH_WAIT, Duration::from_secs),
    }));

    let drain_state = Rc::clone(&drain);
    let boot_timer = Timer::default();
    boot_timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let finished = drain_state.borrow_mut().poll(&mut |line| {
            if let Some(splash) = weak_splash.upgrade() {
                splash.set_current_check(line);
            }
        });
        if let Some((checks, checks_done)) = finished {
            daemon::finish_boot_worker(checks, checks_done);
            request_quit();
        }
    });
    let _boot_timer = boot_timer;

    tracing::info!(preview_secs = ?preview_secs, "Splash event loop starting");
    run_loop("Splash");
    drain.borrow_mut().abandon_leftover();
    tracing::info!("Splash event loop finished");
}

/// Delay before the deferred Bluetooth bootstrap runs: long enough for the
/// first frame to paint (60 Hz ≈ 16.7 ms/frame) before any D-Bus work.
const BT_BOOTSTRAP_DELAY: Duration = Duration::from_millis(50);

/// Schedules the deferred Bluetooth bootstrap on a one-shot timer.
///
/// The timer fires once the event loop is running (and has painted the
/// first frame), so the blocking `BlueZ` connect never delays window show.
/// Public so integration tests can drive the bootstrap with an injected
/// connection factory against the hermetic test platform (`M-TEST-UTIL`);
/// production passes `BluetoothDevice::new_blocking`.
///
/// The returned [`Timer`] must be kept alive: dropping a `Timer` cancels it.
#[must_use]
pub fn schedule_bt_bootstrap(
    connect: impl FnOnce() -> Result<speaker_bluetooth::BluetoothDevice, speaker_bluetooth::BtError>
    + 'static,
    shell: &Rc<AppShell>,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    commands: &speaker_bluetooth::TransportCommandQueue,
    running: &Arc<AtomicBool>,
    worker_slot: &Rc<RefCell<Option<std::io::Result<std::thread::JoinHandle<()>>>>>,
) -> Timer {
    let mut connect = Some(connect);
    let shell = Rc::clone(shell);
    let shared = Arc::clone(shared);
    let commands = commands.clone();
    let running = Arc::clone(running);
    let worker_slot = Rc::clone(worker_slot);
    let timer = Timer::default();
    timer.start(TimerMode::SingleShot, BT_BOOTSTRAP_DELAY, move || {
        if let Some(connect) = connect.take() {
            *worker_slot.borrow_mut() =
                daemon::bootstrap_bluetooth(connect, &shell, &shared, &commands, &running);
        }
    });
    timer
}

/// Background workers and periodic timers for one shell session.
///
/// Held alive across `run_event_loop()`; [`Session::shutdown`] stops the
/// workers after the loop returns.
struct Session {
    bg_running: Arc<AtomicBool>,
    amp_worker: std::io::Result<std::thread::JoinHandle<()>>,
    bt_worker: Rc<RefCell<Option<std::io::Result<std::thread::JoinHandle<()>>>>>,
    _bt_bootstrap_timer: Option<Timer>,
    _sync_timer: Timer,
    _encoder_timer: Option<Timer>,
}

impl Session {
    /// Spawns the Bluetooth/amp workers and starts the state-sync and
    /// rotary-encoder timers. The `AppShell` stays alive through the `Rc`
    /// clones captured by those timers.
    ///
    /// Mocked sessions spawn the `bt-monitor` worker eagerly from the mock
    /// device; live sessions schedule [`daemon::bootstrap_bluetooth`] on a
    /// one-shot timer so the blocking `BlueZ` connect runs only after the
    /// event loop has painted the first frame.
    fn start(init: ShellInit, mocked: bool) -> Self {
        let ShellInit {
            shell,
            bt,
            commands,
            amp_commands,
        } = init;
        let shell = Rc::new(shell);
        let bg_running = Arc::new(AtomicBool::new(true));
        let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));

        let amp_worker =
            daemon::spawn_amp_volume_worker(&shared, &bg_running, &amp_commands, mocked);
        let bt_worker = Rc::new(RefCell::new(None));
        let bt_bootstrap_timer = if let Some(device) = bt.as_ref() {
            *bt_worker.borrow_mut() = Some(daemon::spawn_bt_monitor_worker(
                device,
                &shared,
                &commands,
                &bg_running,
            ));
            None
        } else if !mocked {
            Some(schedule_bt_bootstrap(
                speaker_bluetooth::BluetoothDevice::new_blocking,
                &shell,
                &shared,
                &commands,
                &bg_running,
                &bt_worker,
            ))
        } else {
            None
        };

        // Periodic reconciliation of worker-published Bluetooth/amp state.
        let sync_timer = Timer::default();
        let shell_sync = Rc::clone(&shell);
        sync_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            daemon::apply_shared_bt_state(&shell_sync, &shared);
        });

        // The KMS/FemtoVG backend exposes no CPU-readable scanout, so blurred
        // overlay backdrops cannot be captured here: overlays degrade to the
        // softened scrim. `daemon::sync_overlay_backdrop` stays harness-only.
        tracing::info!(
            "Overlay backdrop capture unavailable on the KMS backend; overlays use scrim dimming only"
        );

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
            bt_worker,
            _bt_bootstrap_timer: bt_bootstrap_timer,
            _sync_timer: sync_timer,
            _encoder_timer: encoder_timer,
        }
    }

    /// Stops the background workers after the event loop returned, re-raising
    /// any worker panic (M-PANIC-ON-BUG) instead of swallowing it.
    fn shutdown(self) {
        self.bg_running.store(false, Ordering::Relaxed);
        daemon::join_amp_volume_worker(self.amp_worker);
        let bt_worker = self.bt_worker.borrow_mut().take();
        match bt_worker {
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
            // Bootstrap never ran (preview quit before its one-shot timer
            // fired, or the BlueZ connect failed): no worker to join.
            None => {}
        }
        tracing::info!("UI session workers stopped");
    }
}

/// Shows the shell window, erroring out before any worker is spawned.
fn show_shell_window(init: &ShellInit) -> bool {
    if let Err(err) = init.shell.window().show() {
        report_display(
            tracing::Level::ERROR,
            "Failed to show UI Shell MainWindow",
            &err,
        );
        return false;
    }
    true
}

/// Shell-only flow: main window, background workers, sync/encoder timers,
/// optional preview quit.
fn run_shell(preview_secs: Option<u64>, tab: Option<i32>, config: &CoreConfig, mocked: bool) {
    let main_window = match MainWindow::new() {
        Ok(win) => win,
        Err(err) => {
            report_display(
                tracing::Level::ERROR,
                "Failed to initialize UI Shell MainWindow",
                &err,
            );
            return;
        }
    };
    let init = daemon::init_shell_with_config(main_window, mocked, tab, config);
    if !show_shell_window(&init) {
        return;
    }
    let session = Session::start(init, mocked);
    if let Some(secs) = preview_secs {
        Timer::single_shot(Duration::from_secs(secs), request_quit);
    }

    tracing::info!(preview_secs = ?preview_secs, "Shell event loop starting");
    run_loop("Shell");
    session.shutdown();
    tracing::info!("Shell event loop finished");
}

/// Appliance flow: one `MainWindow` whose boot checks render as an in-window
/// overlay (`boot_splash_visible`) until they settle, then the shell session
/// takes over for the rest of the process lifetime.
fn run_appliance(
    probe: BootProbe,
    preview_secs: Option<u64>,
    tab: Option<i32>,
    config: &CoreConfig,
    mocked: bool,
) {
    let main_window = match MainWindow::new() {
        Ok(win) => win,
        Err(err) => {
            report_display(
                tracing::Level::ERROR,
                "Failed to initialize UI Shell MainWindow",
                &err,
            );
            return;
        }
    };
    main_window.set_boot_splash_visible(true);
    let weak_window = main_window.as_weak();

    let init = daemon::init_shell_with_config(main_window, mocked, tab, config);
    if !show_shell_window(&init) {
        return;
    }

    let (rx, checks) = spawn_boot_checks(probe);
    let drain = Rc::new(RefCell::new(BootDrain {
        rx,
        checks_done: checks.is_none(),
        checks,
        started: Instant::now(),
        deadline: preview_secs.map_or(daemon::MAX_SPLASH_WAIT, Duration::from_secs),
    }));

    let drain_state = Rc::clone(&drain);
    let boot_timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&boot_timer);
    boot_timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let finished = drain_state.borrow_mut().poll(&mut |line| {
            if let Some(window) = weak_window.upgrade() {
                window.set_boot_current_check(line);
            }
        });
        if let Some((checks, checks_done)) = finished {
            if let Some(window) = weak_window.upgrade() {
                window.set_boot_splash_visible(false);
            }
            daemon::finish_boot_worker(checks, checks_done);
            if let Some(timer) = timer_weak.upgrade() {
                timer.stop();
            }
            tracing::info!("Boot checks settled; splash overlay hidden");
        }
    });
    let _boot_timer = boot_timer;

    let session = Session::start(init, mocked);
    if let Some(secs) = preview_secs {
        Timer::single_shot(Duration::from_secs(secs), request_quit);
    }

    tracing::info!(preview_secs = ?preview_secs, "Appliance event loop starting");
    run_loop("Appliance");
    drain.borrow_mut().abandon_leftover();
    session.shutdown();
    tracing::info!("Appliance event loop finished");
}
