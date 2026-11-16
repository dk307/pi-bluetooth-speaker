#![cfg(feature = "ui")]
//! Integration tests for the Bluetooth details screen: status bar triggers, tab
//! toggling, live connection details, and marquee isolation from non-Player tabs.

use screen_manager::ui::{AppShell, BtLinkDetails, Framebuffer, MainWindow, MarqueeCompose};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};
use speaker_bluetooth::{BluetoothDevice, BtAddress};

fn click(window: &slint::platform::software_renderer::MinimalSoftwareWindow, x: f32, y: f32) {
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

fn shell_with_window() -> (
    AppShell,
    std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
) {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    shell.window().show().expect("show");
    (shell, window)
}

#[test]
fn toggle_returns_to_originating_tab() {
    let (shell, _window) = shell_with_window();

    shell.set_tab(1);
    shell.window().invoke_toggle_bluetooth_details();
    assert_eq!(shell.current_tab(), 2);
    assert_eq!(shell.window().get_previous_tab(), 1);

    shell.window().invoke_toggle_bluetooth_details();
    assert_eq!(shell.current_tab(), 1);
}

#[test]
fn toggle_from_dock_selected_details_falls_back_to_player() {
    let (shell, _window) = shell_with_window();

    // Reached via the dock: previous_tab was never set from another tab.
    shell.set_tab(2);
    shell.window().set_previous_tab(2);
    shell.window().invoke_toggle_bluetooth_details();
    assert_eq!(shell.current_tab(), 0);
}

#[test]
fn status_bar_logo_and_device_name_open_and_close_details() {
    let (shell, window) = shell_with_window();

    // Bluetooth logo (left).
    click(&window, 100.0, 30.0);
    assert_eq!(shell.current_tab(), 2, "logo tap opens details");

    // On the details screen the status bar is not mounted (the ScreenAppBar
    // owns the 60px top slot), so closing goes through the app-bar Back button.
    window.draw_if_needed(|_| {});
    click(&window, 100.0, 30.0);
    assert_eq!(
        shell.current_tab(),
        2,
        "status bar is not mounted on details screen"
    );

    // Device name (center) from the player tab.
    shell.set_tab(0);
    window.draw_if_needed(|_| {});
    click(&window, 400.0, 30.0);
    assert_eq!(shell.current_tab(), 2, "device name tap opens details");
}

#[test]
fn status_bar_codec_badge_does_not_open_details_from_player_tab() {
    let (shell, window) = shell_with_window();

    // Without a Bluetooth connection the codec badge is not rendered, so a tap on
    // the badge slot must not open the details screen (the codec information is
    // shown in its own overlay, see `top_bar_codec_integration.rs`).
    click(&window, 600.0, 30.0);
    assert_eq!(
        shell.current_tab(),
        0,
        "the hidden codec badge must not open details"
    );
    assert_eq!(shell.window().get_previous_tab(), 0);
}

#[test]
fn link_details_formatting_covers_non_streaming_rates_and_adapter_fallback() {
    let (shell, _window) = shell_with_window();
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let mut snapshot = bt.link_snapshot().expect("snapshot");

    // Non-streaming connected device without paired/trusted flags, empty adapter alias,
    // 48 kHz integer rate, 24-bit format, Mono channel, and no delay.
    snapshot.devices[0].paired = false;
    snapshot.devices[0].trusted = false;
    if let Some(ref mut adapter) = snapshot.adapter {
        adapter.alias = "".into();
        adapter.name = "hci0".into();
    }
    snapshot.pcms[0].running = false;
    snapshot.pcms[0].sampling_rate = 48_000;
    snapshot.pcms[0].format = "S24_LE".into();
    snapshot.pcms[0].channels = 1;
    snapshot.pcms[0].delay = None;

    let details = BtLinkDetails::select(&snapshot, None);
    shell.apply_bt_link_details(&details);
    // Applying the identical details a second time exercises the no-op change guards.
    shell.apply_bt_link_details(&details);

    let w = shell.window();
    assert_eq!(w.get_bt_link_text().as_str(), "Connected");
    assert_eq!(w.get_bt_stream_format().as_str(), "48 kHz • 24-bit • Mono");
    assert_eq!(w.get_bt_delay_text().as_str(), "—");
    assert_eq!(w.get_bt_adapter_name().as_str(), "hci0");

    // Multi-channel 32-bit stream format branch.
    snapshot.pcms[0].format = "S32_LE".into();
    snapshot.pcms[0].channels = 6;
    let multichannel = BtLinkDetails::select(&snapshot, None);
    shell.apply_bt_link_details(&multichannel);
    assert_eq!(w.get_bt_stream_format().as_str(), "48 kHz • 32-bit • 6 ch");
}

#[test]
fn volume_and_dim_controls_do_not_open_details() {
    let (shell, window) = shell_with_window();

    click(&window, 764.0, 30.0);
    assert_eq!(shell.current_tab(), 0, "DIM must not open details");

    click(&window, 690.0, 30.0);
    assert_eq!(shell.current_tab(), 0, "volume must not open details");
    assert!(
        shell.is_volume_hud_active(),
        "volume click must open volume HUD"
    );
}

#[test]
fn back_button_returns_to_previous_tab() {
    let (shell, window) = shell_with_window();
    shell.set_tab(1);
    shell.window().invoke_toggle_bluetooth_details();
    assert_eq!(shell.current_tab(), 2);

    // App-bar Back button (trailing icon, top-right of the 60px top bar slot:
    // 40px icon at x 744..784 with a 16px right margin → center ≈ (764, 30)).
    window.draw_if_needed(|_| {});
    click(&window, 764.0, 30.0);
    assert_eq!(shell.current_tab(), 1);
}

#[test]
fn link_details_apply_and_clear() {
    let (shell, _window) = shell_with_window();
    let (bt, mock) = BluetoothDevice::new_mocked();

    let snapshot = bt.link_snapshot().expect("snapshot");
    let details = BtLinkDetails::select(&snapshot, None);
    shell.apply_bt_link_details(&details);

    let w = shell.window();
    assert!(w.get_bt_connected());
    assert_eq!(w.get_bt_device_address().as_str(), "AA:BB:CC:DD:EE:FF");
    assert_eq!(
        w.get_bt_link_text().as_str(),
        "Streaming • Paired • Trusted"
    );
    assert_eq!(w.get_bt_rssi_text().as_str(), "-42 dBm");
    assert_eq!(w.get_bt_battery_text().as_str(), "85%");
    assert_eq!(
        w.get_bt_stream_format().as_str(),
        "44.1 kHz • 16-bit • Stereo"
    );
    assert_eq!(w.get_bt_delay_text().as_str(), "200.0 ms");
    assert_eq!(w.get_bt_adapter_name().as_str(), "RaspberryPi-DSP");
    assert_eq!(w.get_bt_adapter_address().as_str(), "DC:A6:32:11:22:33");

    mock.clear_devices();
    mock.clear_pcms();
    mock.set_adapter(None);
    let cleared = BtLinkDetails::select(&bt.link_snapshot().expect("snapshot"), None);
    shell.apply_bt_link_details(&cleared);
    assert!(!w.get_bt_connected());
    assert_eq!(w.get_bt_device_address().as_str(), "—");
    assert_eq!(w.get_bt_link_text().as_str(), "Waiting for connection");
    assert_eq!(w.get_bt_rssi_text().as_str(), "");
    assert_eq!(w.get_bt_battery_text().as_str(), "");
    assert_eq!(w.get_bt_stream_format().as_str(), "—");
    assert_eq!(w.get_bt_delay_text().as_str(), "—");
    assert_eq!(w.get_bt_adapter_name().as_str(), "—");
    assert_eq!(w.get_bt_adapter_address().as_str(), "—");
}

#[test]
fn select_prefers_requested_connected_device() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let mut second = bt.link_snapshot().expect("snapshot").devices[0].clone();
    second.address = BtAddress::new([1, 2, 3, 4, 5, 6]);
    mock.add_device(second);

    let snapshot = bt.link_snapshot().expect("snapshot");
    let preferred = BtLinkDetails::select(&snapshot, Some(BtAddress::new([1, 2, 3, 4, 5, 6])));
    assert_eq!(
        preferred.device.expect("device").address,
        BtAddress::new([1, 2, 3, 4, 5, 6])
    );
    assert!(preferred.pcm.is_none(), "no stream for the second device");

    let fallback = BtLinkDetails::select(&snapshot, Some(BtAddress::new([9; 6])));
    assert!(fallback.device.is_some(), "unknown preference falls back");
}

#[test]
fn marquee_stays_native_compose_off_the_player_tab() {
    let (shell, _window) = shell_with_window();
    shell.set_track_metadata(
        "A Deliberately Long Track Title That Overflows The Marquee Viewport Width",
        "Some Artist",
        "Audiophile DSP Sink",
    );
    // The Rust-side marquee composer was removed in the KMS migration: native
    // Slint composition must stay engaged (`compose_in_rust == false`) so no
    // stale row geometry can ever be painted over other tabs. The marquee
    // element itself is unmounted off tab 0 (`if root.active_tab == 0` in
    // viewport.slint), which kills its animation timer too.
    assert!(
        !shell
            .window()
            .global::<MarqueeCompose>()
            .get_compose_in_rust(),
        "Rust-side marquee composition must stay disabled"
    );

    shell.set_tab(2);
    assert!(
        !shell
            .window()
            .global::<MarqueeCompose>()
            .get_compose_in_rust(),
        "switching tabs must never re-enable Rust-side marquee composition"
    );
}
