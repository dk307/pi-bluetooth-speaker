#![cfg(feature = "ui")]
//! Coverage tests for daemon branches the main `daemon_integration` suite
//! misses: render-loop error paths, splash loop pacing, cover publishing,
//! settle-window shapes, bt-monitor loop control flow, and the timed worker
//! races.
//!
//! Hermetic: mocks for framebuffer/touch/encoder/Bluetooth, temp files for
//! decode caches, and an isolated `DBUS_SYSTEM_BUS_ADDRESS` override so no
//! test ever talks to the host's system bus.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use screen_manager::daemon::{
    BT_COMMAND_SETTLE, BtMonitorState, BtSharedUpdate, CoverState, SettleKind, SettleWindow,
    decode_cover_buffer, idle_sleep, init_shell, publish_playback_state,
    run_render_loop_with_devices, run_render_loop_with_input, run_splash_on, run_ui_lifecycle,
};
use screen_manager::ui::{AppShell, BootProbe, Framebuffer, MainWindow, TouchInputDevice};
use slint::ComponentHandle;
use speaker_bluetooth::{
    BluetoothDevice, BtPlaybackState, TransportCommand, TransportCommandQueue,
};
use speaker_common::diag::ScopedTempDir;
use speaker_hardware::EncoderDevice;

const SCANOUT_LEN: usize = 800 * 480 * 4;

/// Serializes `DBUS_SYSTEM_BUS_ADDRESS` mutations within this test binary.
static DBUS_ENV_GUARD: Mutex<()> = Mutex::new(());

fn shell_setup() -> (
    Framebuffer,
    std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    AppShell,
) {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    (fb, window, shell)
}

fn sample_png(dir: &Path, name: &str, size: u32) -> std::path::PathBuf {
    let path = dir.join(name);
    image::DynamicImage::new_rgb8(size, size)
        .save_with_format(&path, image::ImageFormat::Png)
        .expect("png write");
    path
}

// ---------------------------------------------------------------------------
// Thin wrapper coverage: run_render_loop_with_input
// ---------------------------------------------------------------------------

#[test]
fn run_render_loop_with_input_wrapper_runs_zero_second_preview() {
    let (mut fb, window, shell) = shell_setup();
    let commands = TransportCommandQueue::new();
    run_render_loop_with_input(&mut fb, &window, &shell, None, &commands, Some(0), None);
}

// ---------------------------------------------------------------------------
// Splash & lifecycle render-loop failure injection
// ---------------------------------------------------------------------------

#[test]
fn splash_loop_pumps_frames_until_injected_render_failure() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    // Probes stay busy past the injected failure so `checks_done` cannot
    // terminate the pump first.
    let slow = BootProbe::from_probes(std::array::from_fn(
        |_| -> screen_manager::ui::boot::ProbeFn {
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(400));
                Ok("late probe".into())
            })
        },
    ));

    let injector = {
        let ctrl = ctrl.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            ctrl.set_render_error("late splash render failure");
        })
    };
    run_splash_on(&mut fb, slow, None, None);
    injector.join().expect("injector thread");
    ctrl.clear_render_error();

    assert!(
        fb.render_count() >= 2,
        "initial frame plus loop frames must render before the injected failure"
    );
}

#[test]
fn ui_lifecycle_loop_breaks_on_mid_run_render_failure() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");
    // A shown component keeps the window dirty so the initial frame renders
    // before the injected fault lands inside the preview loop.
    let main = MainWindow::new().expect("MainWindow");
    main.show().expect("show MainWindow");
    window.request_redraw();

    let injector = {
        let ctrl = ctrl.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            ctrl.set_render_error("mid-lifecycle render failure");
        })
    };
    run_ui_lifecycle(&mut fb, "Injected Lifecycle", Some(5), || Ok(()));
    injector.join().expect("injector thread");
    ctrl.clear_render_error();

    assert!(
        fb.render_count() >= 1,
        "initial frame must render before the loop breaks"
    );
}

#[test]
fn render_loop_breaks_on_frame_error() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    let commands = TransportCommandQueue::new();
    let injector = {
        let ctrl = ctrl.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            ctrl.set_render_error("render loop frame failure");
        })
    };
    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        None,
        &commands,
        Some(2),
        None,
        None,
    );
    injector.join().expect("injector thread");
    ctrl.clear_render_error();
}

#[test]
fn render_loop_quiet_frames_idle_sleep_and_timing_report() {
    let temp = ScopedTempDir::try_new("fb_quiet_loop").expect("temp dir");
    let dev = temp.path().join("quiet_fb");
    std::fs::write(&dev, vec![0_u8; SCANOUT_LEN]).expect("write fb file");

    let mut fb = Framebuffer::open(&dev, None::<&Path>).expect("open fb");
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    shell.window().show().expect("show shell");
    let commands = TransportCommandQueue::new();

    // The per-frame timing summary is emitted through `tracing::debug!`, whose
    // multi-line argument expressions only execute while a DEBUG subscriber is
    // active. A process-global subscriber (installed once; `try_init` ignores
    // repeats) also rebuilds callsite interest so parallel subscriber-less
    // tests in this binary can never cache the callsite as disabled.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        None,
        &commands,
        Some(1),
        None,
        None,
    );

    assert!(
        commands.drain().is_empty(),
        "no transport commands may fire without user input"
    );
}

#[test]
fn render_loop_reports_touch_and_encoder_faults() {
    let (mut fb, window, shell) = shell_setup();
    let commands = TransportCommandQueue::new();

    let (touch, touch_ctrl) = TouchInputDevice::new_mocked();
    touch_ctrl.send_touch_down(100.0, 100.0);
    touch_ctrl.send_touch_move(120.0, 100.0);
    touch_ctrl.send_touch_up(120.0, 100.0);

    // The encoder fails from the first poll: first failure warns and latches
    // `encoder_warned`, subsequent failures downgrade to debug.
    let (encoder, encoder_ctrl) = EncoderDevice::new_mocked();
    encoder_ctrl.set_fault(Some("encoder bus fault".into()));

    let injector = {
        let touch_ctrl = touch_ctrl.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            touch_ctrl.set_fault("touch dispatch fault");
        })
    };

    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        None,
        &commands,
        Some(1),
        Some(touch),
        Some(&encoder),
    );
    injector.join().expect("injector thread");

    // Injected actions were dispatched once; the fault only affects polling.
    assert_eq!(touch_ctrl.counters(), (1, 1, 1));
}

// ---------------------------------------------------------------------------
// bt-monitor worker loop control flow
// ---------------------------------------------------------------------------

#[test]
fn bt_monitor_skips_waiting_while_commands_stream_in() {
    let (mut fb, window, shell) = shell_setup();
    let (bt, _bt_mock) = BluetoothDevice::new_mocked();
    let commands = TransportCommandQueue::new();

    // Keep the queue hot so `poll_tick` observes a fresh command after its
    // drain and short-circuits (the `continue` path of the monitor loop).
    let stop = Arc::new(AtomicBool::new(false));
    let pusher = {
        let stop = Arc::clone(&stop);
        let commands = commands.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                commands.push(TransportCommand::NextTrack);
                let until = Instant::now() + Duration::from_micros(2);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
        })
    };

    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        Some(&bt),
        &commands,
        Some(1),
        None,
        None,
    );
    stop.store(true, Ordering::Relaxed);
    pusher.join().expect("pusher thread");
}

#[test]
fn bt_monitor_breaks_keep_alive_wait_on_queued_command() {
    let (mut fb, window, shell) = shell_setup();
    let (bt, _bt_mock) = BluetoothDevice::new_mocked();
    let commands = TransportCommandQueue::new();

    // The first poll completes (queue empty), the worker then parks in its
    // keep-alive slice; a command queued mid-slice must break the wait.
    let delayed = {
        let commands = commands.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            commands.push(TransportCommand::Seek(5_000));
        })
    };

    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        Some(&bt),
        &commands,
        Some(1),
        None,
        None,
    );
    delayed.join().expect("delayed pusher thread");
}

#[test]
fn poll_tick_aborts_when_commands_race_the_snapshot_query() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let commands = TransportCommandQueue::new();
    let mut state = BtMonitorState::default();

    // Seed the dedup cache with a clean full tick.
    assert!(state.poll_tick(&bt, &shared, &commands));
    assert!(state.has_playback());

    // Stream commands from a second thread with a ~2 µs cadence so both race
    // windows are sampled: after the drain (first early-out) and during the
    // blocking snapshot query (dedup reset early-out).
    let stop = Arc::new(AtomicBool::new(false));
    let pusher = {
        let stop = Arc::clone(&stop);
        let commands = commands.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                commands.push(TransportCommand::TogglePlayPause);
                let until = Instant::now() + Duration::from_micros(2);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut saw_drain_race, mut saw_query_race) = (false, false);
    while !(saw_drain_race && saw_query_race) && Instant::now() < deadline {
        if !state.poll_tick(&bt, &shared, &commands) {
            // The query-race path resets dedup; the drain-race path cannot.
            if state.has_playback() {
                saw_drain_race = true;
            } else {
                saw_query_race = true;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    pusher.join().expect("pusher thread");

    assert!(
        saw_drain_race,
        "a command arriving between drain and query must abort the tick"
    );
    assert!(
        saw_query_race,
        "a command arriving during the query must reset dedup state"
    );
}

// ---------------------------------------------------------------------------
// Playback publish, cover decoding, and settle-window shapes
// ---------------------------------------------------------------------------

#[test]
fn publish_playback_state_decodes_and_stages_cover() {
    let dir = ScopedTempDir::try_new("cover_publish").expect("temp dir");
    let png = sample_png(dir.path(), "art.png", 64);

    let (bt, mock) = BluetoothDevice::new_mocked();
    let path: Box<str> = png.to_string_lossy().into_owned().into_boxed_str();
    mock.set_playback(Some(BtPlaybackState {
        cover_art_path: Some(path.clone()),
        status: "playing".into(),
        ..BtPlaybackState::default()
    }));

    let mut update = BtSharedUpdate::default();
    let mut last_pb: Option<BtPlaybackState> = None;
    let mut cover = CoverState::default();
    let mut settle = None;
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);

    assert!(
        update.cover.is_some_and(|buffer| buffer.is_some()),
        "a decodable cover must publish its pixel buffer"
    );
    assert_eq!(cover.path.as_deref(), Some(path.as_ref()));
    assert!(cover.failed_since.is_none());
    assert!(update.playback.is_some());
}

#[test]
fn publish_playback_state_device_gone_branch_shapes() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.set_playback(None);

    // Outer guard passes, inner cover guard fails (no path was ever staged).
    let mut last_pb = Some(BtPlaybackState {
        status: "playing".into(),
        ..BtPlaybackState::default()
    });
    let mut cover = CoverState::default();
    let mut update = BtSharedUpdate::default();
    let mut settle = None;
    publish_playback_state(&bt, &mut update, &mut last_pb, &mut cover, &mut settle);
    assert!(update.playback.is_some(), "placeholder must publish");
    assert!(update.cover.is_none(), "nothing to clear without a path");

    // Second tick: `last_pb` already holds the placeholder, so the outer
    // publish guard short-circuits entirely.
    let mut update2 = BtSharedUpdate::default();
    publish_playback_state(&bt, &mut update2, &mut last_pb, &mut cover, &mut settle);
    assert!(
        update2.playback.is_none(),
        "unchanged placeholder must be deduplicated"
    );
}

#[test]
fn settle_window_shapes_suppress_pcm_only_when_armed() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let pcm = bt.audio_pcms().expect("audio pcms")[0].clone();
    let playing = BtPlaybackState {
        status: "playing".into(),
        ..BtPlaybackState::default()
    };

    // Playback snapshots are never suppressed by a volume settle window.
    let volume = SettleWindow {
        kind: SettleKind::Volume {
            target_pct: Some(50),
            target_muted: false,
        },
        started: Instant::now(),
    };
    assert!(!volume.suppresses(&playing));

    // An expired window suppresses nothing (immediate reconciliation).
    let expired = SettleWindow {
        kind: SettleKind::Volume {
            target_pct: Some(50),
            target_muted: false,
        },
        started: Instant::now()
            .checked_sub(BT_COMMAND_SETTLE + Duration::from_secs(1))
            .expect("monotonic instant before start"),
    };
    assert!(!expired.suppresses_pcm(&pcm));

    // Toggle/seek windows only guard playback snapshots, never PCM levels.
    let toggle = SettleWindow {
        kind: SettleKind::Toggle { pre_playing: true },
        started: Instant::now(),
    };
    assert!(!toggle.suppresses_pcm(&pcm));
    let seek = SettleWindow {
        kind: SettleKind::Seek { target_ms: 1_000 },
        started: Instant::now(),
    };
    assert!(!seek.suppresses_pcm(&pcm));
}

#[test]
fn pcm_settle_window_suppresses_then_expires() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let commands = TransportCommandQueue::new();
    let mut state = BtMonitorState::default();

    assert!(state.poll_tick(&bt, &shared, &commands));
    commands.push(TransportCommand::SetVolume(64));
    assert!(state.poll_tick(&bt, &shared, &commands));

    // Diverge the authoritative level while the settle window is active: the
    // stale snapshot must be withheld from the shell.
    mock.set_pcm_volume_state(30, false);
    let mut withheld = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut withheld);
    assert!(
        withheld.pcm.is_none(),
        "stale level during an armed settle window must be suppressed"
    );

    // Re-arm and wait out the settle window: expiry clears dedup so an
    // otherwise unchanged snapshot republishes.
    commands.push(TransportCommand::SetVolume(66));
    assert!(state.poll_tick(&bt, &shared, &commands));
    std::thread::sleep(Duration::from_millis(800));
    let mut reconciled = BtSharedUpdate::default();
    state.publish_link_state(&bt, &mut reconciled);
    assert!(
        reconciled.pcm.is_some(),
        "settle expiry must force a fresh authoritative publish"
    );
}

#[test]
fn render_loop_applies_decoded_cover_buffer() {
    let dir = ScopedTempDir::try_new("cover_apply").expect("temp dir");
    let png = sample_png(dir.path(), "art.png", 64);
    // Warm the upscale cache so the worker's decode is cheap regardless of
    // host speed; a failed decode here would make the assertions meaningless.
    assert!(
        decode_cover_buffer(png.to_str().expect("utf8")).is_some(),
        "cover decode must succeed on this host"
    );

    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.set_playback(Some(BtPlaybackState {
        cover_art_path: Some(png.to_string_lossy().into_owned().into_boxed_str()),
        status: "playing".into(),
        ..BtPlaybackState::default()
    }));

    let commands = TransportCommandQueue::new();
    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        Some(&bt),
        &commands,
        Some(1),
        None,
        None,
    );

    assert!(
        shell.window().get_has_cover_art(),
        "the worker's decoded cover buffer must reach the shell"
    );
}

#[test]
fn render_loop_applies_disconnect_cover_clear() {
    let dir = ScopedTempDir::try_new("cover_clear").expect("temp dir");
    let png = sample_png(dir.path(), "art.png", 64);
    // Warm the upscale cache (see the buffer-apply test).
    assert!(
        decode_cover_buffer(png.to_str().expect("utf8")).is_some(),
        "cover decode must succeed on this host"
    );

    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.set_playback(Some(BtPlaybackState {
        cover_art_path: Some(png.to_string_lossy().into_owned().into_boxed_str()),
        status: "playing".into(),
        ..BtPlaybackState::default()
    }));

    let commands = TransportCommandQueue::new();
    // Disconnect mid-run (after the buffer has certainly been applied): the
    // worker publishes the clear delta, which the render loop applies.
    let flipper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        mock.set_playback(None);
    });

    run_render_loop_with_devices(
        &mut fb,
        &window,
        &shell,
        Some(&bt),
        &commands,
        Some(2),
        None,
        None,
    );
    flipper.join().expect("flipper thread");

    assert!(
        !shell.window().get_has_cover_art(),
        "the disconnect delta must clear displayed cover art"
    );
}

// ---------------------------------------------------------------------------
// Bluetooth D-Bus connection failure reporting
// ---------------------------------------------------------------------------

#[test]
fn init_shell_reports_bluetooth_connection_failure() {
    let _guard = DBUS_ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("platform init");

    // Point the system bus at a nonexistent socket so `new_blocking` fails
    // without touching the host's real D-Bus.
    // SAFETY: serialized by `DBUS_ENV_GUARD`; no other test in this binary
    // reads `DBUS_SYSTEM_BUS_ADDRESS`, and the value is restored below.
    unsafe {
        std::env::set_var(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/nonexistent/no-such-dbus-socket",
        );
    };
    let init = init_shell(
        MainWindow::new().expect("MainWindow"),
        false,
        None,
        None,
        60,
    );
    // SAFETY: same serialized-guard invariant as above.
    unsafe { std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS") };

    assert!(
        init.bt.is_none(),
        "an unreachable system bus must degrade to a shell without Bluetooth"
    );
}

// ---------------------------------------------------------------------------
// idle_sleep signal-interrupt handling
// ---------------------------------------------------------------------------

extern "C" fn noop_signal_handler(_signal: libc::c_int) {}

#[test]
fn idle_sleep_retries_poll_on_eintr() {
    // SAFETY: installing a no-op SIGUSR1 handler; restored to SIG_DFL before
    // the test returns so later tests observe default disposition.
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            noop_signal_handler as *const () as libc::sighandler_t,
        )
    };
    // SAFETY: `pthread_self` only reads the calling thread's identity.
    let this_thread = unsafe { libc::pthread_self() };

    let stop = Arc::new(AtomicBool::new(false));
    let interrupter = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: valid thread handle and signal number; the handler
                // is a no-op installed above.
                unsafe { libc::pthread_kill(this_thread, libc::SIGUSR1) };
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    let mut fds = [0; 2];
    // SAFETY: pipe initializes the 2-element c_int array with valid descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let [rfd, wfd] = fds;

    // Interrupted polls must be retried promptly rather than spin or crash.
    for _ in 0..40 {
        idle_sleep(Some(rfd));
    }

    stop.store(true, Ordering::Relaxed);
    interrupter.join().expect("interrupter thread");
    // SAFETY: restoring the default disposition after the test.
    unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
    // SAFETY: closing test pipe descriptors.
    unsafe {
        libc::close(rfd);
        libc::close(wfd)
    };
}

// ---------------------------------------------------------------------------
// Cover decode edge cases
// ---------------------------------------------------------------------------

#[test]
fn decode_cover_buffer_uses_large_sources_verbatim() {
    let dir = ScopedTempDir::try_new("cover_large").expect("temp dir");
    let src = sample_png(dir.path(), "large.png", 600);

    let buffer = decode_cover_buffer(src.to_str().expect("utf8")).expect("decoded");
    assert_eq!(buffer.width(), 600);
    assert_eq!(buffer.height(), 600);
    assert!(
        !dir.path().join("large.png.480.jpg").exists(),
        "sources at panel size or larger bypass the upscale cache"
    );
}

#[test]
fn decode_cover_buffer_survives_unwritable_cache_path() {
    let dir = ScopedTempDir::try_new("cover_unwritable").expect("temp dir");
    let src = sample_png(dir.path(), "art.png", 64);

    // Occupy the cache slot with a directory: decoding proceeds from the
    // source, the stale entry is not removed (not a regular file), and the
    // upscale cache write fails without discarding the in-memory buffer.
    let cache = dir.path().join("art.png.480.jpg");
    std::fs::create_dir(&cache).expect("cache dir");

    let buffer =
        decode_cover_buffer(src.to_str().expect("utf8")).expect("decode must proceed in memory");
    assert_eq!(buffer.width(), 480);
    assert!(cache.is_dir(), "existing directory must not be removed");
}
