#![cfg(feature = "ui")]
//! Integration tests for the Bluetooth details screen: status bar triggers, tab
//! toggling, live connection details, and marquee isolation from non-Player tabs.

use screen_manager::ui::{AppShell, BtLinkDetails, Framebuffer, MainWindow};
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
fn status_bar_clicks_are_intercepted_off_the_player_tab() {
    let (shell, window) = shell_with_window();

    // On tab 1 the status bar is hidden behind the status_bar_placeholder overlay:
    // a click on the codec badge strip (X: 600, Y: 30) must NOT fall through to the
    // hidden status bar's TouchAreas (opacity-0 elements still hit-test in Slint).
    shell.set_tab(1);
    click(&window, 600.0, 30.0);
    click(&window, 100.0, 30.0);
    click(&window, 690.0, 30.0);
    assert_eq!(
        shell.current_tab(),
        1,
        "Top-strip clicks on non-player tabs must be intercepted by the placeholder"
    );
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

    // Bluetooth logo (left) — only works on tab 0 (Now Playing).
    click(&window, 100.0, 30.0);
    assert_eq!(shell.current_tab(), 2, "logo tap opens details");
    // Back to Now Playing: invoke ScreenAppBar back action.
    shell.window().invoke_app_bar_back();
    assert_eq!(
        shell.current_tab(),
        0,
        "back from details returns to player"
    );

    // Device name (center) — only works on tab 0.
    click(&window, 400.0, 30.0);
    assert_eq!(shell.current_tab(), 2, "device name tap opens details");
    // Back via ScreenAppBar back action.
    shell.window().invoke_app_bar_back();
    assert_eq!(shell.current_tab(), 0);
}

#[test]
fn status_bar_codec_badge_opens_details_from_now_playing_tab() {
    let (shell, window) = shell_with_window();

    // Fixed-spec codec ("aptX-HD"): sample-rate and bit-depth pills are hidden.
    // Codec badge only works on tab 0 (Now Playing) where the status bar is visible.
    click(&window, 600.0, 30.0);
    assert_eq!(shell.current_tab(), 2, "codec badge tap opens details");
    assert_eq!(shell.window().get_previous_tab(), 0);

    // Return to Now Playing to test the AAC badge path (status bar only visible on tab 0)
    shell.window().invoke_app_bar_back();
    assert_eq!(shell.current_tab(), 0);

    // Variable-spec codec ("AAC"): sample-rate and bit-depth pills are visible
    // and tapping them also opens/toggles Bluetooth details.
    shell.set_stream_codec("AAC");
    shell.set_stream_sample_rate("48 kHz");
    shell.set_stream_bit_depth("24-bit");
    click(&window, 590.0, 30.0);
    assert_eq!(shell.current_tab(), 2, "bit-depth badge tap opens details");
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
    let (shell, _window) = shell_with_window();
    shell.set_tab(1);
    shell.window().invoke_toggle_bluetooth_details();
    assert_eq!(shell.current_tab(), 2);

    // Invoke the ScreenAppBar back action directly (tests the wiring, not click coords).
    shell.window().invoke_app_bar_back();
    assert_eq!(shell.current_tab(), 1);
}

#[test]
fn back_button_on_themes_closes_themes_view() {
    let (shell, _window) = shell_with_window();

    shell.set_tab(5);
    shell.window().set_show_themes(true);
    shell.window().invoke_app_bar_back();
    assert!(
        !shell.window().get_show_themes(),
        "Back on the Themes view must close the themes list"
    );
    assert_eq!(
        shell.current_tab(),
        5,
        "Back on the Themes view must stay on the Settings screen"
    );

    // Second back leaves the Settings screen entirely.
    shell.window().invoke_app_bar_back();
    assert_eq!(shell.current_tab(), 0);
}

#[test]
fn back_button_falls_back_to_player_when_previous_tab_is_stale() {
    let (shell, _window) = shell_with_window();

    // previous_tab collapsed to the active tab (multi-hop history): back must
    // fall back to Now Playing instead of staying on the same screen.
    shell.set_tab(3);
    shell.window().set_previous_tab(3);
    shell.window().invoke_app_bar_back();
    assert_eq!(
        shell.current_tab(),
        0,
        "Back with stale previous_tab must return to Now Playing"
    );

    // Back on Now Playing is a no-op.
    shell.window().invoke_app_bar_back();
    assert_eq!(shell.current_tab(), 0);
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
fn marquee_rows_are_empty_off_the_player_tab() {
    let (shell, _window) = shell_with_window();
    shell.set_track_metadata(
        "A Deliberately Long Track Title That Overflows The Marquee Viewport Width",
        "Some Artist",
        "Audiophile DSP Sink",
    );
    shell.set_marquee_compose_in_rust(true);

    shell.set_tab(2);
    assert!(
        shell.marquee_row_specs().is_empty(),
        "stale marquee geometry must not be composed over other tabs"
    );
}
