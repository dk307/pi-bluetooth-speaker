//! Integration tests for Touch Ingestion, Calibration, and Window Dispatching.
//!
//! Validates:
//! - [`TouchCalibration`] coordinate normalization and inversion matrices.
//! - [`TouchDecoder`] Linux `input_event` stream decoding.
//! - [`TouchInputDevice`] mock controller injection and fault handling.
//! - Golden viewport hit-testing with [`MainWindow`] and [`TransportCommandQueue`].
//! - Scrubber drag physics, preview, and seek-on-release.
//! - Play/pause rapid tap debouncing.

use screen_manager::ui::{
    AppShell, Framebuffer, MainWindow, TouchAction, TouchCalibration, TouchDecoder,
    TouchInputDevice,
};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};
use speaker_bluetooth::{TransportCommand, TransportCommandQueue};

#[test]
fn touch_calibration_mapping_and_clamping() {
    let cal = TouchCalibration {
        min_x: 100.0,
        max_x: 900.0,
        min_y: 50.0,
        max_y: 450.0,
        invert_x: false,
        invert_y: false,
        swap_xy: false,
    };

    // Exact min maps to 0.0, exact max maps to screen dimensions
    assert_eq!(cal.map_to_screen(100, 50, 800.0, 480.0), (0.0, 0.0));
    assert_eq!(cal.map_to_screen(900, 450, 800.0, 480.0), (800.0, 480.0));
    assert_eq!(cal.map_to_screen(500, 250, 800.0, 480.0), (400.0, 240.0));

    // Clamping on out-of-bounds readings
    assert_eq!(cal.map_to_screen(-50, -10, 800.0, 480.0), (0.0, 0.0));
    assert_eq!(cal.map_to_screen(2000, 1000, 800.0, 480.0), (800.0, 480.0));

    // Inverted 180° orientation
    let cal_inv = TouchCalibration {
        invert_x: true,
        invert_y: true,
        ..cal
    };
    assert_eq!(cal_inv.map_to_screen(100, 50, 800.0, 480.0), (800.0, 480.0));
    assert_eq!(cal_inv.map_to_screen(900, 450, 800.0, 480.0), (0.0, 0.0));

    // Swapped axes (portrait)
    let cal_swap = TouchCalibration {
        swap_xy: true,
        ..TouchCalibration::default()
    };
    assert_eq!(
        cal_swap.map_to_screen(240, 400, 800.0, 480.0),
        (400.0, 240.0)
    );
}

#[test]
fn touch_decoder_processes_linux_input_events() {
    fn make_ev(type_: u16, code: u16, value: i32) -> libc::input_event {
        libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        }
    }

    let mut decoder = TouchDecoder::new(TouchCalibration {
        min_x: 0.0,
        max_x: 800.0,
        min_y: 0.0,
        max_y: 480.0,
        ..Default::default()
    });

    // 1. Touch Down packet
    assert!(
        decoder
            .process_event(&make_ev(
                screen_manager::ui::input::EV_ABS,
                screen_manager::ui::input::ABS_X,
                200
            ))
            .is_none()
    );
    assert!(
        decoder
            .process_event(&make_ev(
                screen_manager::ui::input::EV_ABS,
                screen_manager::ui::input::ABS_Y,
                150
            ))
            .is_none()
    );
    assert!(
        decoder
            .process_event(&make_ev(
                screen_manager::ui::input::EV_KEY,
                screen_manager::ui::input::BTN_TOUCH,
                1
            ))
            .is_none()
    );
    let down_action = decoder.process_event(&make_ev(
        screen_manager::ui::input::EV_SYN,
        screen_manager::ui::input::SYN_REPORT,
        0,
    ));
    assert_eq!(down_action, Some(TouchAction::Down { x: 200.0, y: 150.0 }));

    // 2. Motion packet
    assert!(
        decoder
            .process_event(&make_ev(
                screen_manager::ui::input::EV_ABS,
                screen_manager::ui::input::ABS_X,
                220
            ))
            .is_none()
    );
    let move_action = decoder.process_event(&make_ev(
        screen_manager::ui::input::EV_SYN,
        screen_manager::ui::input::SYN_REPORT,
        0,
    ));
    assert_eq!(move_action, Some(TouchAction::Move { x: 220.0, y: 150.0 }));

    // 3. Touch Up packet
    assert!(
        decoder
            .process_event(&make_ev(
                screen_manager::ui::input::EV_KEY,
                screen_manager::ui::input::BTN_TOUCH,
                0
            ))
            .is_none()
    );
    let up_action = decoder.process_event(&make_ev(
        screen_manager::ui::input::EV_SYN,
        screen_manager::ui::input::SYN_REPORT,
        0,
    ));
    assert_eq!(up_action, Some(TouchAction::Up { x: 220.0, y: 150.0 }));
}

#[test]
fn touch_mock_device_dispatch_and_fault_injection() {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");

    let (mut touch_dev, mock_ctrl) = TouchInputDevice::new_mocked();
    assert_eq!(touch_dev.query_device_name(), "Mock Touchscreen Device");
    assert!(touch_dev.as_raw_fd().is_none());

    // Inject touch actions
    mock_ctrl.send_touch_down(400.0, 240.0);
    mock_ctrl.send_touch_move(420.0, 240.0);
    mock_ctrl.send_touch_up(420.0, 240.0);

    let (down, moved, up) = mock_ctrl.counters();
    assert_eq!((down, moved, up), (1, 1, 1));

    // Dispatch to window
    let dispatched = touch_dev
        .dispatch_to_window(&window)
        .expect("Dispatch should succeed");
    assert_eq!(dispatched, 3);

    // Subsequent poll is empty
    assert_eq!(
        touch_dev
            .dispatch_to_window(&window)
            .expect("Empty dispatch"),
        0
    );

    // Fault injection
    mock_ctrl.set_fault("Simulated I2C touch controller bus stall");
    let err = touch_dev.poll_actions().unwrap_err();
    assert!(err.is_mock_fault());
    assert!(format!("{err}").contains("bus stall"));

    mock_ctrl.clear_fault();
    let _ = touch_dev.poll_actions().unwrap();
}

#[test]
fn touch_viewport_golden_hit_testing_play_pause_and_skip() {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);
    shell.window().show().expect("Failed to show MainWindow");

    let commands = TransportCommandQueue::new();
    shell.wire_bluetooth_transport(&commands);

    // Configure active track so transport controls are enabled
    shell.set_track_metadata("Money for Nothing", "Dire Straits", "Brothers in Arms");
    shell.set_playback_state(false, 0, 180_000);

    // 1. Hit Play/Pause hero button center.
    // In the 800x480 layout:
    // Top status bar is 60px. Viewport is 60..480 (420px tall).
    // Cover art top at y=90 (30px margin from viewport top). Title starts at y=90.
    // Right Column: padding-top=30, metadata (title~40 + 16 + artist~28 = 84),
    // progress+transport VerticalLayout starts at y=30+84=114 from viewport top.
    // Equal stretch spacers distribute remaining space; probed transport center at y=337 window top.
    let test_y = 337.0;
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(588.0, test_y),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(588.0, test_y),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    let drained = commands.drain();
    assert_eq!(
        drained.len(),
        1,
        "Hero Play/Pause button should enqueue exactly 1 TogglePlayPause command"
    );
    assert_eq!(drained[0], TransportCommand::TogglePlayPause);
    assert!(
        shell.is_playing(),
        "UI should optimistically flip to playing"
    );

    // 2. Rapid tap debounce: tapping again immediately (<300ms) should be ignored
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(588.0, test_y),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(588.0, test_y),
        button: PointerEventButton::Left,
    });
    assert!(
        commands.drain().is_empty(),
        "Rapid tap within 300ms debounce interval must be ignored"
    );

    // 3. Hit Previous Track button (approx X: 473, Y: 337) - center 588 - 151 offset for 36px spacing
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(473.0, test_y),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(473.0, test_y),
        button: PointerEventButton::Left,
    });
    let drained_prev = commands.drain();
    assert_eq!(drained_prev.len(), 1);
    assert_eq!(drained_prev[0], TransportCommand::PreviousTrack);

    // 4. Hit Next Track button (approx X: 703, Y: 337) - center 588 + 151 offset for 36px spacing
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(703.0, test_y),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(703.0, test_y),
        button: PointerEventButton::Left,
    });
    let drained_next = commands.drain();
    assert_eq!(drained_next.len(), 1);
    assert_eq!(drained_next[0], TransportCommand::NextTrack);
}

#[test]
fn touch_viewport_progress_bar_is_read_only() {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);
    shell.window().show().expect("Failed to show MainWindow");

    let commands = TransportCommandQueue::new();
    shell.wire_bluetooth_transport(&commands);

    // Track length = 200 seconds (200,000 ms)
    shell.set_track_metadata("Sultans of Swing", "Dire Straits", "Dire Straits");
    shell.set_playback_state(true, 10_000, 200_000);

    // Progress bar center is approx Y: 210 in the right column (X spans ~440 to ~720)
    // 1. Pointer Down at 50% along the progress track (X: 580, Y: 210)
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(580.0, 210.0),
        button: PointerEventButton::Left,
    });

    // 2. Drag finger along the progress track to 75% position
    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(650.0, 210.0),
    });

    // 3. Pointer Up (Release)
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(650.0, 210.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    // Progress bar is read-only: NO seek command must be enqueued!
    let drained = commands.drain();
    assert!(
        drained.is_empty(),
        "Read-only progress bar must not enqueue any seek commands on touch/drag/release"
    );
}

#[test]
fn touch_top_bar_volume_opens_hud_and_slider_and_mute_work() {
    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);
    shell.window().show().expect("Failed to show MainWindow");
    let _ = fb.render_frame(&window).expect("Initial frame render");

    let commands = TransportCommandQueue::new();
    shell.wire_bluetooth_transport(&commands);

    assert!(!shell.is_volume_hud_visible());

    // 1. Click the volume control in TopStatusBar (top-right area before DIM button: X: 690, Y: 30)
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(690.0, 30.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(690.0, 30.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    assert!(
        shell.is_volume_hud_visible(),
        "Clicking top status bar volume control must open the Volume HUD screen"
    );
    let _ = fb.render_frame(&window).expect("Render HUD open");
    // Let the 200 ms HUD fade-in complete; mid-fade presses do not reach the HUD.
    for _ in 0..30 {
        slint::platform::update_timers_and_animations();
        let _ = fb.render_frame(&window).expect("Render HUD fade");
    }
    assert_eq!(shell.volume_hud_opacity(), 1.0, "HUD fade must complete");

    // 2. Touch and drag the big-circle slider in the Volume HUD (centered at Y: ~382, X: 112..688)
    // Tap near the left end of the track (X: 112 -> 0%) and drag to center (X: 400 -> 50%) and right (X: 688 -> 100%)
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(112.0, 382.0),
        button: PointerEventButton::Left,
    });
    assert_eq!(shell.volume(), 0.0);

    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(400.0, 382.0),
    });
    assert_eq!(shell.volume(), 50.0);

    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(688.0, 382.0),
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(688.0, 382.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);
    assert_eq!(shell.volume(), 100.0);

    // Consecutive SetVolume commands coalesce to the latest value (100%)
    let drained = commands.drain();
    assert_eq!(drained, vec![TransportCommand::SetVolume(100)]);

    // 3. Click the dedicated MUTE / UNMUTE button at the bottom of the Volume HUD (X: 400, Y: 438)
    assert!(!shell.is_muted());
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(400.0, 438.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(400.0, 438.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    assert!(
        shell.is_muted(),
        "Clicking dedicated MUTE button inside Volume HUD must mute audio"
    );
    assert_eq!(commands.drain(), vec![TransportCommand::SetMute(true)]);

    // Click UNMUTE button again
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(400.0, 438.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(400.0, 438.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);
    assert!(!shell.is_muted());
    assert_eq!(commands.drain(), vec![TransportCommand::SetMute(false)]);

    // 4. Tap backdrop outside controls (e.g., X: 40, Y: 100) to dismiss HUD immediately
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(40.0, 100.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(40.0, 100.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);
    assert!(
        !shell.is_volume_hud_visible(),
        "Tapping backdrop outside controls must dismiss Volume HUD"
    );
}

#[test]
fn evdev_open_and_poll_raw_hardware_events() {
    use screen_manager::ui::input::{
        ABS_X, ABS_Y, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, EvdevSyscall, MockDeviceConfig,
        MockEvdevSyscall, SYN_REPORT, TouchAction, TouchInputDevice, input_event_to_bytes,
        make_input_event,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    let mock = Arc::new(MockEvdevSyscall::new());
    let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;
    let device_path = PathBuf::from("/dev/input/event0");
    mock.register_path(&device_path, MockDeviceConfig::default());

    let mut dev =
        TouchInputDevice::open_with(&device_path, Arc::clone(&syscall)).expect("open_with failed");
    assert_eq!(dev.query_device_name(), "raspberrypi-ts");
    let fd = dev.as_raw_fd().expect("expected raw fd");
    assert!(fd >= 100);

    // Push Down, Move, Up events
    let mut event_bytes = Vec::new();
    // Down (ABS_X, ABS_Y, BTN_TOUCH 1, SYN)
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(EV_ABS, ABS_X, 100)));
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(EV_ABS, ABS_Y, 120)));
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_KEY, BTN_TOUCH, 1,
    )));
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_SYN, SYN_REPORT, 0,
    )));

    // Move (ABS_X, SYN)
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(EV_ABS, ABS_X, 150)));
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_SYN, SYN_REPORT, 0,
    )));

    // Up (BTN_TOUCH 0, SYN)
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_KEY, BTN_TOUCH, 0,
    )));
    event_bytes.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_SYN, SYN_REPORT, 0,
    )));

    mock.push_raw_events(fd, &event_bytes);

    let actions = dev.poll_actions().expect("poll_actions failed");
    assert_eq!(actions.len(), 3);
    assert_eq!(actions[0], TouchAction::Down { x: 100.0, y: 120.0 });
    assert_eq!(actions[1], TouchAction::Move { x: 150.0, y: 120.0 });
    assert_eq!(actions[2], TouchAction::Up { x: 150.0, y: 120.0 });

    // Second poll encounters WouldBlock and returns empty
    assert_eq!(
        dev.poll_actions().expect("poll empty"),
        Vec::<TouchAction>::new()
    );

    // Drop dev and verify descriptor closed
    drop(dev);
    assert!(mock.is_closed(fd));
}

#[test]
fn evdev_partial_chunk_buffering() {
    use screen_manager::ui::input::{
        ABS_X, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, EvdevSyscall, MockDeviceConfig, MockEvdevSyscall,
        SYN_REPORT, TouchAction, TouchInputDevice, input_event_to_bytes, make_input_event,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    let mock = Arc::new(MockEvdevSyscall::new());
    let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;
    let device_path = PathBuf::from("/dev/input/event1");
    mock.register_path(&device_path, MockDeviceConfig::default());

    let mut dev =
        TouchInputDevice::open_with(&device_path, Arc::clone(&syscall)).expect("open_with failed");
    let fd = dev.as_raw_fd().unwrap();

    let ev1 = make_input_event(EV_ABS, ABS_X, 200);
    let ev1_bytes = input_event_to_bytes(&ev1);
    let split_idx = 10;
    assert!(split_idx < ev1_bytes.len());

    // Push partial bytes
    mock.push_raw_events(fd, &ev1_bytes[..split_idx]);
    assert_eq!(
        dev.poll_actions().expect("poll partial"),
        Vec::<TouchAction>::new()
    );

    // Push remaining bytes + syn
    let mut rest = Vec::new();
    rest.extend_from_slice(&ev1_bytes[split_idx..]);
    rest.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_KEY, BTN_TOUCH, 1,
    )));
    rest.extend_from_slice(&input_event_to_bytes(&make_input_event(
        EV_SYN, SYN_REPORT, 0,
    )));

    mock.push_raw_events(fd, &rest);
    let actions = dev.poll_actions().expect("poll assembled");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0], TouchAction::Down { x: 200.0, y: 0.0 });
}

#[test]
fn evdev_open_and_ioctl_fault_injections() {
    use screen_manager::ui::input::{
        EvdevSyscall, MockDeviceConfig, MockEvdevSyscall, TouchInputDevice,
    };
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

    let mock = Arc::new(MockEvdevSyscall::new());
    let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;

    // 1. Open failure
    let p_not_found = PathBuf::from("/dev/input/missing");
    let err = TouchInputDevice::open_with(&p_not_found, Arc::clone(&syscall)).unwrap_err();
    assert!(err.is_io());

    let p_denied = PathBuf::from("/dev/input/denied");
    mock.register_path(
        &p_denied,
        MockDeviceConfig {
            fail_open: Some(io::ErrorKind::PermissionDenied),
            ..Default::default()
        },
    );
    let err = TouchInputDevice::open_with(&p_denied, Arc::clone(&syscall)).unwrap_err();
    assert!(err.is_io());

    // 2. Nonblocking fcntl failure
    let p_nb_fail = PathBuf::from("/dev/input/nb_fail");
    mock.register_path(
        &p_nb_fail,
        MockDeviceConfig {
            fail_nonblocking: true,
            ..Default::default()
        },
    );
    let err = TouchInputDevice::open_with(&p_nb_fail, Arc::clone(&syscall)).unwrap_err();
    assert!(err.is_io());

    // 3. ABS ioctl complete failure
    let p_abs_fail = PathBuf::from("/dev/input/abs_fail");
    mock.register_path(
        &p_abs_fail,
        MockDeviceConfig {
            fail_abs: true,
            ..Default::default()
        },
    );
    let err = TouchInputDevice::open_with(&p_abs_fail, Arc::clone(&syscall)).unwrap_err();
    assert!(err.is_io());

    // 4. Invalid calibration bounds (min >= max)
    let p_bad_bounds = PathBuf::from("/dev/input/bad_bounds");
    mock.register_path(
        &p_bad_bounds,
        MockDeviceConfig {
            abs_x: libc::input_absinfo {
                value: 0,
                minimum: 800,
                maximum: 800,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
            ..Default::default()
        },
    );
    let err = TouchInputDevice::open_with(&p_bad_bounds, Arc::clone(&syscall)).unwrap_err();
    assert!(format!("{err}").contains("Invalid touch axis limits"));

    // 5. ST abs fails, MT abs succeeds (fallback branch)
    let p_mt_fallback = PathBuf::from("/dev/input/mt_fallback");
    mock.register_path(
        &p_mt_fallback,
        MockDeviceConfig {
            fail_st_abs_only: true,
            ..Default::default()
        },
    );
    let dev = TouchInputDevice::open_with(&p_mt_fallback, Arc::clone(&syscall))
        .expect("MT fallback should succeed");
    assert_eq!(dev.calibration().max_x, 800.0);

    // 6. query_name failure fallback
    let p_bad_name = PathBuf::from("/dev/input/bad_name");
    mock.register_path(
        &p_bad_name,
        MockDeviceConfig {
            fail_name: true,
            ..Default::default()
        },
    );
    let dev_bad_name = TouchInputDevice::open_with(&p_bad_name, Arc::clone(&syscall)).unwrap();
    assert_eq!(dev_bad_name.query_device_name(), "Unknown Evdev Device");

    // 7. read error handling
    let p_read_err = PathBuf::from("/dev/input/read_err");
    mock.register_path(
        &p_read_err,
        MockDeviceConfig {
            fail_read: Some(io::ErrorKind::BrokenPipe),
            ..Default::default()
        },
    );
    let mut dev_read_err = TouchInputDevice::open_with(&p_read_err, Arc::clone(&syscall)).unwrap();
    let err = dev_read_err.poll_actions().unwrap_err();
    assert!(err.is_io());
}

#[test]
fn evdev_auto_discover_scenarios() {
    use screen_manager::ui::input::{
        EvdevSyscall, MockDeviceConfig, MockEvdevSyscall, TouchInputDevice,
    };
    use std::sync::Arc;

    let temp = tempfile::tempdir().expect("tempdir");
    let temp_path = temp.path();

    let mock = Arc::new(MockEvdevSyscall::new());
    let syscall: Arc<dyn EvdevSyscall> = Arc::clone(&mock) as _;

    // Scenario 1: Empty directory -> NotFound
    let err = TouchInputDevice::auto_discover_in(temp_path, &syscall).unwrap_err();
    assert!(err.is_device_not_found());

    // Scenario 2: event0 is a Keyboard, event1 is TouchScreen
    let ev0 = temp_path.join("event0");
    let ev1 = temp_path.join("event1");
    std::fs::write(&ev0, b"").unwrap();
    std::fs::write(&ev1, b"").unwrap();

    mock.register_path(
        &ev0,
        MockDeviceConfig {
            name: "Standard Keyboard".to_string(),
            ..Default::default()
        },
    );
    mock.register_path(
        &ev1,
        MockDeviceConfig {
            name: "Freenove TouchScreen Device".to_string(),
            ..Default::default()
        },
    );

    let dev = TouchInputDevice::auto_discover_in(temp_path, &syscall)
        .expect("auto_discover should find event1");
    assert_eq!(dev.query_device_name(), "Freenove TouchScreen Device");

    // Scenario 3: event0 is directly a touchscreen
    mock.register_path(
        &ev0,
        MockDeviceConfig {
            name: "raspberrypi-ts".to_string(),
            ..Default::default()
        },
    );
    let dev0 = TouchInputDevice::auto_discover_in(temp_path, &syscall)
        .expect("auto_discover should find event0 directly");
    assert_eq!(dev0.query_device_name(), "raspberrypi-ts");
}

#[test]
fn native_evdev_syscall_with_pipes() {
    use screen_manager::ui::input::{EvdevSyscall, NativeEvdevSyscall, is_touchscreen_name};
    use std::io;

    let native = NativeEvdevSyscall;
    let mut fds = [0; 2];
    // SAFETY: pipe initializes the 2-element c_int array with valid file descriptors.
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(ret, 0);
    let [rfd, wfd] = fds;

    // Test set_nonblocking on pipe
    native
        .set_nonblocking(rfd)
        .expect("set_nonblocking on pipe");

    // Read on empty nonblocking pipe -> WouldBlock
    let mut buf = [0u8; 16];
    let err = native.read(rfd, &mut buf).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

    // Write to pipe and read
    let data = b"touch-data";
    // SAFETY: writing into a valid OS pipe descriptor with accurate length.
    let written = unsafe { libc::write(wfd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
    assert_eq!(written, 10);

    let n = native.read(rfd, &mut buf).expect("read from pipe");
    assert_eq!(&buf[..n], data);

    // abs_code > u8::MAX returns InvalidInput without unsafe ioctl call
    let err = native.query_abs(rfd, 1000).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    // Name match helper tests including ft5406 digitizers
    assert!(is_touchscreen_name("ft5406"));
    assert!(is_touchscreen_name("rpi-ft5406"));
    assert!(is_touchscreen_name("FT5406 memory based driver"));
    assert!(is_touchscreen_name("raspberrypi-ts"));
    assert!(is_touchscreen_name("Goodix Capacitive TouchScreen"));
    assert!(!is_touchscreen_name("keyboard"));
    assert!(!is_touchscreen_name("mouse"));

    // Close pipe fds
    native.close(rfd).expect("close rfd");
    native.close(wfd).expect("close wfd");
}

#[test]
fn touch_error_comprehensive() {
    use screen_manager::ui::input::TouchError;
    use std::error::Error;
    use std::io;

    let err_io = TouchError::io(
        "/dev/input/event0",
        io::Error::new(io::ErrorKind::NotFound, "not found"),
    );
    assert!(err_io.is_io());
    assert!(!err_io.is_device_not_found());
    assert!(!err_io.is_mock_fault());
    let _ = err_io.backtrace();
    assert!(format!("{err_io}").contains("Touch I/O error"));
    assert!(err_io.source().is_some());

    let err_dnf = TouchError::device_not_found("No input found");
    assert!(err_dnf.is_device_not_found());
    assert!(format!("{err_dnf}").contains("Touch device not found"));
    assert!(err_dnf.source().is_none());

    let err_cfg = TouchError::invalid_config("Zero span axis");
    assert!(format!("{err_cfg}").contains("Invalid touch configuration"));
    assert!(err_cfg.source().is_none());
}

#[test]
fn multi_touch_protocol_b_tracking_id() {
    use screen_manager::ui::input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID, EV_ABS, EV_SYN, SYN_REPORT,
        TouchAction, TouchCalibration, TouchDecoder, make_input_event,
    };

    let mut decoder = TouchDecoder::new(TouchCalibration::default());

    // MT Tracking ID >= 0 indicates down
    decoder.process_event(&make_input_event(EV_ABS, ABS_MT_POSITION_X, 300));
    decoder.process_event(&make_input_event(EV_ABS, ABS_MT_POSITION_Y, 200));
    decoder.process_event(&make_input_event(EV_ABS, ABS_MT_TRACKING_ID, 42));
    let down = decoder.process_event(&make_input_event(EV_SYN, SYN_REPORT, 0));
    assert_eq!(down, Some(TouchAction::Down { x: 300.0, y: 200.0 }));

    // MT Tracking ID -1 indicates release (Up)
    decoder.process_event(&make_input_event(EV_ABS, ABS_MT_TRACKING_ID, -1));
    let up = decoder.process_event(&make_input_event(EV_SYN, SYN_REPORT, 0));
    assert_eq!(up, Some(TouchAction::Up { x: 300.0, y: 200.0 }));
}

#[test]
fn production_wrappers_and_calibration_setters() {
    use screen_manager::ui::input::{
        ABS_X, EvdevSyscall, NativeEvdevSyscall, TouchCalibration, TouchInputDevice,
    };

    // Test production open on nonexistent path
    let err = TouchInputDevice::open("/dev/input/nonexistent-test-node").unwrap_err();
    assert!(err.is_io());

    // Test set_calibration on device
    let (mut dev, _) = TouchInputDevice::new_mocked();
    dev.set_calibration(TouchCalibration {
        invert_x: true,
        invert_y: true,
        swap_xy: true,
        ..Default::default()
    })
    .expect("valid calibration should be accepted");
    assert!(dev.calibration().swap_xy);

    // Test NaN and inverted/zero span calibration rejection
    let nan_cal = TouchCalibration {
        min_x: f32::NAN,
        ..Default::default()
    };
    assert!(!nan_cal.is_valid());
    assert_eq!(nan_cal.map_to_screen(100, 100, 800.0, 480.0), (0.0, 0.0));
    let err_nan = dev.set_calibration(nan_cal).unwrap_err();
    assert!(format!("{err_nan}").contains("Invalid touch calibration"));
    assert!(dev.calibration().is_valid());

    let zero_span_cal = TouchCalibration {
        min_x: 100.0,
        max_x: 100.0,
        ..Default::default()
    };
    assert!(!zero_span_cal.is_valid());
    assert_eq!(
        zero_span_cal.map_to_screen(100, 100, 800.0, 480.0),
        (0.0, 0.0)
    );
    let err_zero = dev.set_calibration(zero_span_cal).unwrap_err();
    assert!(format!("{err_zero}").contains("Invalid touch calibration"));
    assert!(dev.calibration().is_valid());

    // NativeEvdevSyscall error paths with invalid fd (-1)
    let native = NativeEvdevSyscall;
    let _ = native.set_nonblocking(-1).unwrap_err();
    let _ = native.read(-1, &mut [0u8; 8]).unwrap_err();
    let _ = native.query_abs(-1, ABS_X).unwrap_err();
    let _ = native.query_name(-1).unwrap_err();
    let _ = native.close(-1).unwrap_err();
}

#[test]
fn touch_viewport_settings_button_opens_settings_screen_and_navigates() {
    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");
    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);
    shell.window().show().expect("Failed to show MainWindow");

    assert_eq!(shell.current_tab(), 0, "Default screen must be Now Playing");
    let _ = fb.render_frame(&window).expect("Render initial frame");

    // 1. Click right-bottom Settings button on Now Playing (X: 758, Y: 440)
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(758.0, 440.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(758.0, 440.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    assert_eq!(
        shell.current_tab(),
        5,
        "Clicking right-bottom Settings button must navigate to Settings screen"
    );
    let _ = fb.render_frame(&window).expect("Render Settings screen");

    // 2. Click "Back" button — the screen app bar puts Back on the trailing
    // (right-most) edge of the 60px top bar slot: 40px icon at x 744..784 with
    // a 16px right margin → center ≈ (764, 30). The status bar is not mounted
    // on the Settings screen.
    let _ = fb.render_frame(&window).expect("Render before back click");
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(764.0, 30.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(764.0, 30.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    assert_eq!(
        shell.current_tab(),
        0,
        "Clicking Back button on Settings screen must return to Now Playing"
    );
}
