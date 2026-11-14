#![cfg(feature = "ui")]
//! Integration tests for the Now Playing top bar: codec badge visibility, the
//! codec information overlay, and the guard that keeps a long device name from
//! being drawn over the codec badge.

use std::rc::Rc;

use screen_manager::ui::{AppShell, Bgrx8888Pixel, BtLinkDetails, Framebuffer, MainWindow};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};
use speaker_bluetooth::BluetoothDevice;

/// Width of the appliance panel.
const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
/// Row through the vertical middle of the 60 px top bar (through the codec badge).
const BAR_MID_ROW: usize = 30;

fn click(window: &MinimalSoftwareWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);
}

fn shell_with_window() -> (AppShell, Rc<MinimalSoftwareWindow>, Framebuffer) {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    shell.window().show().expect("show");
    (shell, window, fb)
}

/// Connects (or disconnects) the mocked Bluetooth device.
fn set_connected(shell: &AppShell, connected: bool) {
    if connected {
        let (bt, _mock) = BluetoothDevice::new_mocked();
        let snapshot = bt.link_snapshot().expect("snapshot");
        let details = BtLinkDetails::select(&snapshot, None);
        shell.apply_bt_link_details(&details);
    } else {
        shell.apply_bt_link_details(&BtLinkDetails::default());
    }
    assert_eq!(
        shell.window().get_bt_connected(),
        connected,
        "the Bluetooth link state must follow the device"
    );
}

/// Renders the whole window and returns its pixels.
fn render(fb: &mut Framebuffer, window: &MinimalSoftwareWindow) -> Vec<Bgrx8888Pixel> {
    slint::platform::update_timers_and_animations();
    fb.render_frame_full_redraw(window).expect("render frame");
    fb.pixels().to_vec()
}

/// Returns the columns of `row` that differ between two renders.
fn differing_columns(a: &[Bgrx8888Pixel], b: &[Bgrx8888Pixel], row: usize) -> Vec<usize> {
    (0..WIDTH)
        .filter(|&x| a[row * WIDTH + x] != b[row * WIDTH + x])
        .collect()
}

/// Column range occupied by the codec badge, derived from the rendered pixels.
///
/// The badge is the only difference between a disconnected and a connected top
/// bar, so the differing columns localize it without hard-coding layout offsets.
/// The device name is cleared for both renders: it re-centers itself in the gap
/// between the logo and the controls, which would otherwise show up as a
/// difference of its own.
fn codec_badge_columns(
    fb: &mut Framebuffer,
    window: &MinimalSoftwareWindow,
    shell: &AppShell,
) -> (usize, usize) {
    shell.set_track_or_device("");
    set_connected(shell, false);
    let without_badge = render(fb, window);
    set_connected(shell, true);
    let with_badge = render(fb, window);

    let columns = differing_columns(&without_badge, &with_badge, BAR_MID_ROW);
    assert!(
        !columns.is_empty(),
        "the codec badge must be rendered once a device is connected"
    );
    (
        *columns.first().expect("badge start"),
        *columns.last().expect("badge end"),
    )
}

/// Advances the Slint timers and animations so an overlay fade-in completes.
fn settle() {
    for _ in 0..40 {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn codec_badge_is_only_rendered_while_a_device_is_connected() {
    let (shell, window, mut fb) = shell_with_window();
    shell.set_track_or_device("");

    set_connected(&shell, false);
    let disconnected = render(&mut fb, &window);
    set_connected(&shell, true);
    let connected = render(&mut fb, &window);

    let badge_columns = differing_columns(&disconnected, &connected, BAR_MID_ROW);
    assert!(
        !badge_columns.is_empty(),
        "the codec badge must appear once a device is connected"
    );
    assert!(
        badge_columns.iter().all(|&x| x > WIDTH / 2),
        "the codec badge belongs to the right-hand control group"
    );

    // Without a connection the badge slot stays empty: those columns still carry
    // the plain status bar background.
    let background = disconnected[BAR_MID_ROW * WIDTH];
    for &x in &badge_columns {
        assert_eq!(
            disconnected[BAR_MID_ROW * WIDTH + x],
            background,
            "no codec may be displayed without a Bluetooth connection (column {x})"
        );
    }
}

#[test]
fn codec_badge_tap_opens_the_codec_information_overlay() {
    let (shell, window, mut fb) = shell_with_window();
    // The device name is cleared so the tap lands on the codec badge and not on
    // the (re-centered) name text.
    shell.set_track_or_device("");
    set_connected(&shell, true);
    shell.set_stream_codec("LDAC");
    let (badge_first, badge_last) = codec_badge_columns(&mut fb, &window, &shell);
    let badge_center = (badge_first + badge_last) as f32 / 2.0;

    // Without a connection the badge slot must not react to touches.
    set_connected(&shell, false);
    window.draw_if_needed(|_| {});
    click(&window, badge_center, 30.0);
    assert!(
        !shell.is_codec_hud_visible(),
        "a tap on the hidden codec badge must not open the codec overlay"
    );
    assert_eq!(
        shell.current_tab(),
        0,
        "a tap on the hidden codec badge must not open the details screen"
    );

    // With a connection the badge opens the codec information overlay.
    set_connected(&shell, true);
    window.draw_if_needed(|_| {});
    click(&window, badge_center, 30.0);
    assert!(
        shell.is_codec_hud_visible(),
        "tapping the codec badge must open the codec overlay"
    );
    assert_eq!(
        shell.current_tab(),
        0,
        "the codec overlay must not switch to the Bluetooth details tab"
    );
}

#[test]
fn codec_overlay_reports_the_stream_information_and_dismisses() {
    let (shell, window, _fb) = shell_with_window();
    set_connected(&shell, true);
    shell.set_stream_codec("LDAC");
    shell.set_stream_sample_rate("96 kHz");
    shell.set_stream_bit_depth("24-bit");

    shell.show_codec_hud();
    settle();
    assert!(shell.is_codec_hud_visible());

    // The overlay reports the negotiated stream properties.
    let w = shell.window();
    assert_eq!(w.get_stream_codec(), "LDAC");
    assert_eq!(w.get_stream_sample_rate(), "96 kHz");
    assert_eq!(w.get_stream_bit_depth(), "24-bit");
    assert!(w.get_bt_connected());

    // Tapping the backdrop dismisses the overlay.
    click(&window, 400.0, 300.0);
    assert!(
        !shell.is_codec_hud_visible(),
        "a backdrop tap must dismiss the codec overlay"
    );

    // The close button in the overlay app bar dismisses it as well.
    shell.show_codec_hud();
    settle();
    click(&window, 764.0, 90.0);
    assert!(
        !shell.is_codec_hud_visible(),
        "the close button must dismiss the codec overlay"
    );
}

#[test]
fn long_device_name_is_elided_before_the_codec_badge() {
    let (shell, window, mut fb) = shell_with_window();
    set_connected(&shell, true);
    let (badge_first, _badge_last) = codec_badge_columns(&mut fb, &window, &shell);

    shell.set_track_or_device(
        "A Very Long Bluetooth Device Name That Would Otherwise Overwrite The Codec Badge",
    );
    let with_name = render(&mut fb, &window);
    shell.set_track_or_device("");
    let without_name = render(&mut fb, &window);

    let name_columns = differing_columns(&with_name, &without_name, BAR_MID_ROW);
    assert!(!name_columns.is_empty(), "the device name must be rendered");
    let name_last = *name_columns.last().expect("name end");
    assert!(
        name_last < badge_first,
        "the device name ends at column {name_last} and would overwrite the codec badge \
         starting at column {badge_first}"
    );
}
