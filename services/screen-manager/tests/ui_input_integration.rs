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

    // 2. Touch and drag the big-circle slider in the Volume HUD (centered at Y: ~348, X: 112..688)
    // Tap near the left end of the track (X: 112 -> 0%) and drag to center (X: 400 -> 50%) and right (X: 688 -> 100%)
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(112.0, 348.0),
        button: PointerEventButton::Left,
    });
    assert_eq!(shell.volume(), 0.0);

    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(400.0, 348.0),
    });
    assert_eq!(shell.volume(), 50.0);

    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(688.0, 348.0),
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(688.0, 348.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);
    assert_eq!(shell.volume(), 100.0);

    // Consecutive SetVolume commands coalesce to the latest value (100%)
    let drained = commands.drain();
    assert_eq!(drained, vec![TransportCommand::SetVolume(100)]);

    // 3. Click the dedicated MUTE / UNMUTE button at the bottom of the Volume HUD (X: 400, Y: 439)
    assert!(!shell.is_muted());
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(400.0, 439.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(400.0, 439.0),
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
        position: LogicalPosition::new(400.0, 439.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(400.0, 439.0),
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

    // 2. Click "Back" button on Settings screen (top-right header, approx X: 733, Y: 92)
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(733.0, 92.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(733.0, 92.0),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    assert_eq!(
        shell.current_tab(),
        0,
        "Clicking Back button on Settings screen must return to Now Playing"
    );
}
