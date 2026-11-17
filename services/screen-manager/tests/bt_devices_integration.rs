#![cfg(feature = "ui")]
//! Integration tests for the Bluetooth Devices screen (plan §9).
//!
//! Pure row presentation, the shell controller, and the bt-monitor worker are
//! host-hermetic: the worker runs against `BluetoothDevice::new_mocked()` and
//! no test here reads or mutates the process environment. The pairing
//! state-machine tests that need a private `dbus-daemon` with a fake
//! `org.bluez` (and therefore mutate `DBUS_SYSTEM_BUS_ADDRESS`) live in the
//! dedicated single-test binary `pairing_agent_system_bus_integration.rs`
//! (repository private-bus testing rule).

mod common;

use std::rc::Rc;
use std::sync::Mutex;

use common::tick;
use screen_manager::daemon::{BT_PAIRING_TIMEOUT, BtMonitorState, BtSharedUpdate};
use screen_manager::ui::{
    AppShell, BtDeviceRow, BtDevicesController, BtDevicesUpdate, BtLinkDetails, Framebuffer,
    MainWindow, device_row, icon_slug, pairing_code_text, status_text, subtitle_text,
};
use slint::ComponentHandle;
use slint::Model;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};
use speaker_bluetooth::{
    AdminAction, BluetoothDevice, BtAddress, BtDevice, DeviceCommand, DeviceCommandQueue,
    PairingStatus, TransportCommandQueue,
};

const PHONE: BtAddress = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
const PIXEL: BtAddress = BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

fn shell_with_window() -> (AppShell, Rc<MinimalSoftwareWindow>) {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    shell.window().show().expect("show");
    (shell, window)
}

fn screen_row(address: &str, name: &str, connected: bool, paired: bool) -> BtDeviceRow {
    BtDeviceRow {
        address: address.into(),
        name: name.into(),
        subtitle: format!("Device · {address}").into(),
        status: if connected {
            "Active".into()
        } else if paired {
            "Paired".into()
        } else {
            "Available".into()
        },
        icon: "bluetooth".into(),
        connected,
        paired,
    }
}

fn sample_update() -> BtDevicesUpdate {
    BtDevicesUpdate {
        rows: vec![
            screen_row("AA:BB:CC:DD:EE:FF", "iPhone 15 Pro", true, true),
            screen_row("11:22:33:44:55:66", "Pixel 8", false, true),
        ],
        discovered: vec![screen_row("77:88:99:AA:BB:CC", "Echo Dot", false, false)],
        ..BtDevicesUpdate::default()
    }
}

fn unpaired_device(address: BtAddress) -> BtDevice {
    BtDevice {
        address,
        name: "Pixel 8".into(),
        alias: String::new().into(),
        connected: false,
        paired: false,
        trusted: false,
        blocked: false,
        rssi: Some(-60),
        battery_percentage: None,
        icon: Some("phone".into()),
        uuids: Vec::new().into_boxed_slice(),
    }
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Pure presentation functions
// ---------------------------------------------------------------------------

#[test]
fn icon_slug_maps_bluez_icon_categories() {
    assert_eq!(icon_slug(Some("phone")), "phone");
    assert_eq!(icon_slug(Some("telephone")), "phone");
    assert_eq!(icon_slug(Some("computer")), "laptop");
    assert_eq!(icon_slug(Some("laptop")), "laptop");
    assert_eq!(icon_slug(Some("desktop")), "laptop");
    assert_eq!(icon_slug(Some("tablet")), "tablet");
    assert_eq!(icon_slug(Some("audio-card")), "headset");
    assert_eq!(icon_slug(Some("headset")), "headset");
    assert_eq!(icon_slug(Some("headphones")), "headset");
    assert_eq!(icon_slug(Some("speaker")), "headset");
    assert_eq!(icon_slug(Some("watch")), "bluetooth");
    assert_eq!(icon_slug(None), "bluetooth");
}

#[test]
fn row_presentation_formats_subtitle_status_and_passkey() {
    let (bt, _mock) = BluetoothDevice::new_mocked();
    let snapshot = bt.link_snapshot().expect("snapshot");
    let phone = &snapshot.devices[0];

    assert_eq!(phone.display_name(), "iPhone 15 Pro");
    assert_eq!(subtitle_text(phone), "Phone · AA:BB:CC:DD:EE:FF");
    assert_eq!(
        status_text(phone, false),
        "Active",
        "connected reads Active"
    );
    assert_eq!(status_text(phone, true), "Active");

    let pixel = unpaired_device(PIXEL);
    assert_eq!(status_text(&pixel, true), "Active", "active override");
    assert_eq!(status_text(&pixel, false), "Available");
    let mut paired = pixel.clone();
    paired.paired = true;
    assert_eq!(status_text(&paired, false), "Paired");

    assert_eq!(pairing_code_text(482_910), "482910");
    assert_eq!(pairing_code_text(7), "000007", "passkeys are zero-padded");
}

#[test]
fn device_row_maps_fields_and_active_address() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.add_device(unpaired_device(PIXEL));
    let snapshot = bt.link_snapshot().expect("snapshot");
    let phone = &snapshot.devices[0];
    let pixel = snapshot
        .devices
        .iter()
        .find(|device| device.address == PIXEL)
        .expect("pixel in snapshot");

    let active = device_row(phone, Some(PHONE));
    assert_eq!(active.address.as_str(), "AA:BB:CC:DD:EE:FF");
    assert_eq!(active.name.as_str(), "iPhone 15 Pro");
    assert_eq!(active.subtitle.as_str(), "Phone · AA:BB:CC:DD:EE:FF");
    assert_eq!(active.status.as_str(), "Active");
    assert_eq!(active.icon.as_str(), "phone");
    assert!(active.connected && active.paired);

    let connected = device_row(phone, None);
    assert_eq!(
        connected.status.as_str(),
        "Active",
        "connected stays Active"
    );

    let available = device_row(pixel, None);
    assert_eq!(available.status.as_str(), "Available");
    assert_eq!(available.address.as_str(), "11:22:33:44:55:66");
    assert!(!available.connected && !available.paired);
}

#[test]
fn controller_accepts_only_changed_updates() {
    let mut controller = BtDevicesController::default();
    let update = sample_update();
    assert!(controller.accepts(&update));
    assert!(!controller.accepts(&update), "identical update is deduped");

    let mut changed = update.clone();
    changed.scanning = true;
    assert!(controller.accepts(&changed));

    // One-shot events bypass the dedup: a repeated identical refusal toast
    // (e.g. a second failed attempt after the first toast expired) must be
    // delivered again.
    let mut toasted = changed.clone();
    toasted.toast = Some("Operation failed".into());
    assert!(controller.accepts(&toasted));
    assert!(
        controller.accepts(&toasted),
        "repeated one-shot toasts are always delivered"
    );

    let mut toastless = toasted;
    toastless.toast = None;
    assert!(controller.accepts(&toastless));
    assert!(
        !controller.accepts(&toastless),
        "plain state updates still dedup"
    );
}

// ---------------------------------------------------------------------------
// Shell controller: property wiring, pairing transitions, callbacks
// ---------------------------------------------------------------------------

#[test]
fn apply_bt_devices_publishes_rows_subtitle_and_scanning() {
    let (shell, _window) = shell_with_window();
    let mut update = sample_update();
    update.adapter_alias = Some("Studio Amp".into());
    update.scanning = true;
    shell.apply_bt_devices(update);

    let w = shell.window();
    assert_eq!(
        w.get_bt_devices_subtitle().as_str(),
        r#"Visible as "Studio Amp""#
    );
    assert!(w.get_bt_scanning());

    let rows = w.get_bt_device_rows();
    assert_eq!(
        rows.row_data(0).expect("row 0").name.as_str(),
        "iPhone 15 Pro"
    );
    assert_eq!(rows.row_data(0).expect("row 0").status.as_str(), "Active");
    assert_eq!(
        rows.row_data(1).expect("row 1").address.as_str(),
        "11:22:33:44:55:66"
    );
    assert_eq!(rows.row_data(2), None);

    let discovered = w.get_bt_discovered_rows();
    assert_eq!(
        discovered.row_data(0).expect("discovered 0").name.as_str(),
        "Echo Dot"
    );
}

#[test]
fn apply_bt_devices_maps_pairing_transitions() {
    let (shell, _window) = shell_with_window();
    let w = shell.window();

    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::Connecting {
            name: "iPhone 15 Pro".into(),
        }),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 2);
    assert_eq!(w.get_bt_pair_device_name().as_str(), "iPhone 15 Pro");
    assert!(w.get_bt_pair_spinning());
    assert_eq!(w.get_bt_pair_code().as_str(), "");

    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::AwaitingConfirmation {
            name: "iPhone 15 Pro".into(),
            passkey: 482_910,
        }),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 2);
    assert_eq!(w.get_bt_pair_code().as_str(), "482910");

    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::Paired {
            name: "iPhone 15 Pro".into(),
        }),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 0);
    assert!(!w.get_bt_pair_spinning());
    assert_eq!(
        w.get_bt_pair_code().as_str(),
        "",
        "terminals clear stale codes"
    );

    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::Failed {
            message: "Pairing failed".into(),
        }),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 1);

    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::TimedOut),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 1);
    assert!(!w.get_bt_pair_spinning());

    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::Cancelled),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 1);
}

#[test]
fn apply_bt_devices_shows_toast_and_dedupes_identical_publishes() {
    let (shell, _window) = shell_with_window();
    let w = shell.window();

    shell.apply_bt_devices(BtDevicesUpdate {
        toast: Some("Pairing timed out".into()),
        ..BtDevicesUpdate::default()
    });
    assert!(w.get_bt_toast_visible());
    assert_eq!(w.get_bt_toast_text().as_str(), "Pairing timed out");

    // The sentinel proves the second identical publish never reached Slint.
    w.set_bt_devices_mode(99);
    shell.apply_bt_devices(BtDevicesUpdate {
        toast: Some("Pairing timed out".into()),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 99, "identical update is deduped");
}

#[test]
fn row_delete_keys_confirmation_dialog_by_address() {
    let (shell, _window) = shell_with_window();
    shell.apply_bt_devices(sample_update());

    let w = shell.window();
    w.invoke_bt_row_delete(1);
    assert!(w.get_bt_delete_open());
    assert_eq!(w.get_bt_delete_address().as_str(), "11:22:33:44:55:66");
    assert_eq!(w.get_bt_delete_device_name().as_str(), "Pixel 8");

    w.invoke_bt_delete_cancel_clicked();
    assert!(!w.get_bt_delete_open());
}

#[test]
fn delete_confirm_pushes_remove_for_dialog_address() {
    let (shell, _window) = shell_with_window();
    let queue = DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&queue);
    shell.apply_bt_devices(sample_update());

    let w = shell.window();
    w.invoke_bt_row_delete(0);
    assert_eq!(w.get_bt_delete_address().as_str(), "AA:BB:CC:DD:EE:FF");
    w.invoke_bt_delete_confirm_clicked();
    assert!(!w.get_bt_delete_open());
    assert_eq!(queue.drain(), vec![DeviceCommand::Remove(PHONE)]);
    assert!(queue.is_empty());
}

#[test]
fn row_action_ignores_invalid_and_missing_rows() {
    let (shell, _window) = shell_with_window();
    let queue = DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&queue);
    shell.apply_bt_devices(BtDevicesUpdate {
        rows: vec![screen_row("not-an-address", "Broken", false, true)],
        ..BtDevicesUpdate::default()
    });

    let w = shell.window();
    w.invoke_bt_row_action(0, false);
    assert!(queue.is_empty(), "unparseable address must not push");

    w.invoke_bt_row_action(9, false);
    assert!(queue.is_empty(), "out-of-range index must not push");

    w.set_bt_delete_address("bogus".into());
    w.invoke_bt_delete_confirm_clicked();
    assert!(queue.is_empty(), "invalid dialog address must not push");
    assert!(!w.get_bt_delete_open());
}

#[test]
fn discovery_callbacks_push_commands_in_order() {
    let (shell, _window) = shell_with_window();
    let queue = DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&queue);
    shell.apply_bt_devices(sample_update());

    let w = shell.window();
    w.invoke_bt_add_device_clicked();
    assert_eq!(
        w.get_bt_devices_mode(),
        1,
        "Add Device opens the discovery view (plan §5)"
    );
    assert_eq!(queue.drain(), vec![DeviceCommand::StartDiscovery]);

    w.invoke_bt_cancel_scan_clicked();
    assert_eq!(
        w.get_bt_devices_mode(),
        0,
        "Cancel returns to the paired list"
    );
    assert_eq!(
        queue.drain(),
        vec![DeviceCommand::StopDiscovery, DeviceCommand::Refresh],
        "Cancel stops discovery before requesting a refresh"
    );
}

#[test]
fn toast_without_session_converges_optimistic_pairing_flip() {
    let (shell, _window) = shell_with_window();
    let w = shell.window();
    // The optimistic flip exactly as on_bt_row_action performs it for a
    // discovery row (mode 2 + spinner, before the worker has seen the command).
    w.set_bt_devices_open(true);
    w.set_bt_devices_mode(2);
    w.set_bt_pair_device_name("Echo Dot".into());
    w.set_bt_pair_spinning(true);

    // Pre-session refusal (agent registration failure, no adapter, spawn
    // failure) stages only a toast — it must still converge the view.
    shell.apply_bt_devices(BtDevicesUpdate {
        toast: Some("Operation failed".into()),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 1, "refusal returns to Add Device");
    assert!(!w.get_bt_pair_spinning(), "dead spinner must be cleared");
    assert_eq!(w.get_bt_pair_code().as_str(), "");
    assert!(w.get_bt_toast_visible(), "refusal is surfaced as a toast");

    // A toast published alongside a LIVE session status must not kick the
    // user out of the pairing view — the terminal transition owns the mode.
    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::Connecting {
            name: "Echo Dot".into(),
        }),
        toast: Some("Adapter not ready".into()),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 2, "live session wins over toast");
}

#[test]
fn close_stops_screen_started_scan() {
    let (shell, _window) = shell_with_window();
    let queue = DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&queue);
    let w = shell.window();

    // Idle close: nothing to stop, no commands pushed.
    w.set_bt_devices_open(true);
    w.invoke_close_bt_devices();
    assert!(!w.get_bt_devices_open());
    assert!(
        queue.drain().is_empty(),
        "idle close must not touch discovery"
    );

    // Close while scanning: StopDiscovery then Refresh (same FIFO as Cancel).
    w.set_bt_devices_open(true);
    w.set_bt_scanning(true);
    w.invoke_close_bt_devices();
    assert!(!w.get_bt_devices_open());
    assert_eq!(
        queue.drain(),
        vec![DeviceCommand::StopDiscovery, DeviceCommand::Refresh],
        "closing mid-scan must stop the screen-started scan"
    );
}

#[test]
fn row_action_dispatches_connect_disconnect_and_pair() {
    let (shell, _window) = shell_with_window();
    let queue = DeviceCommandQueue::new();
    shell.wire_bluetooth_devices(&queue);
    shell.apply_bt_devices(sample_update());

    let w = shell.window();
    w.invoke_bt_row_action(0, false);
    assert_eq!(queue.drain(), vec![DeviceCommand::Disconnect(PHONE)]);
    w.invoke_bt_row_action(1, false);
    assert_eq!(queue.drain(), vec![DeviceCommand::Connect(PIXEL)]);

    // Discovery row: Pair with the optimistic pairing chrome.
    w.set_bt_devices_mode(0);
    w.invoke_bt_row_action(0, true);
    let echo = BtAddress::new([0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC]);
    assert_eq!(queue.drain(), vec![DeviceCommand::Pair(echo)]);
    assert_eq!(w.get_bt_devices_mode(), 2, "optimistic pairing view");
    assert_eq!(w.get_bt_pair_device_name().as_str(), "Echo Dot");
    assert!(w.get_bt_pair_spinning());

    w.invoke_bt_pair_cancel_clicked();
    assert_eq!(queue.drain(), vec![DeviceCommand::CancelPairing]);
    assert_eq!(w.get_bt_devices_mode(), 1);
}

#[test]
fn is_animating_tracks_pairing_and_toast() {
    let (shell, _window) = shell_with_window();
    shell.set_tab(1);
    let w = shell.window();
    w.set_bt_devices_open(true);
    assert!(!w.get_is_animating(), "idle screen must not animate");

    // Mode 2 without a live ring must not pin the render loop: the ring's
    // frame timer only runs while bt_pair_spinning is set.
    w.set_bt_devices_mode(2);
    assert!(
        !w.get_is_animating(),
        "pairing view without a spinning ring idles"
    );
    w.set_bt_pair_spinning(true);
    assert!(w.get_is_animating(), "pairing ring animates");
    w.set_bt_pair_spinning(false);

    w.set_bt_devices_mode(0);
    w.set_bt_toast_visible(true);
    assert!(w.get_is_animating(), "toast is animated state");

    w.set_bt_toast_visible(false);
    assert!(!w.get_is_animating());

    // A16: the indeterminate scanning spinner (mode 1) is a live animation
    // source too, so `is_animating` must cover it.
    w.set_bt_devices_mode(1);
    assert!(
        !w.get_is_animating(),
        "idle Add Device screen does not animate"
    );
    w.set_bt_scanning(true);
    assert!(w.get_is_animating(), "scanning spinner animates");
    w.set_bt_scanning(false);
    assert!(!w.get_is_animating());
}

#[test]
fn devices_screen_slint_avoids_perpetual_animation_tick() {
    let source = screen_source();
    assert!(
        !source.contains("animation-tick("),
        "M-NO-PERPETUAL-ANIMATION-DRIVER: animation-tick() must not drive the screen"
    );
}

fn screen_source() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/bluetooth_devices.slint"),
    )
    .expect("bluetooth_devices.slint must exist")
}

#[test]
fn devices_screen_toast_timer_restarts_per_toast() {
    let source = screen_source();
    assert!(
        source.contains("changed bt_toast_text"),
        "a new toast must restart the auto-hide timer (phase truncation fix)"
    );
    assert!(
        !source.contains("running: root.bt_toast_visible"),
        "a bound running timer keeps its phase across new toasts"
    );
}

#[test]
fn devices_screen_cancel_row_uses_layout_padding() {
    let source = screen_source();
    assert!(
        !source.contains("y: 24px"),
        "hardcoded pixel offsets are forbidden (M-SLINT-MATH-LAYOUT); use layout padding"
    );
}

#[test]
fn devices_screen_scrollbar_hides_after_last_scroll() {
    let source = screen_source();
    assert!(
        !source.contains("if !hide_timer.running"),
        "hide timer must restart on every scroll event so the thumb never \
         fades mid-flick; hiding happens 1500 ms after the LAST movement"
    );
    assert!(
        source.contains("hide_timer.running = false;"),
        "scroll handler must reset the hide timer"
    );
}

#[test]
fn devices_screen_list_disables_panning_while_delete_dialog_open() {
    let source = screen_source();
    assert!(
        source.contains("interactive: !root.delete_open"),
        "modal delete dialog must disable list panning"
    );
}

#[test]
fn devices_screen_overflow_cue_gated_on_content_height() {
    let source = screen_source();
    let cue = source
        .find("visible: flick.content_height > flick.height;")
        .expect("fade cue must be gated on content overflow (audit A13)");
    let scrollbar = source
        .find("Rendered after the cue")
        .expect("scrollbar must be rendered after the cue (audit A10)");
    assert!(
        scrollbar > cue,
        "scrollbar must render above the fade cue (audit A10)"
    );
}

// ---------------------------------------------------------------------------
// bt-monitor worker state machine (mocked BluetoothDevice)
// ---------------------------------------------------------------------------

#[test]
fn worker_publishes_rows_and_dedupes_unchanged_ticks() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.add_device(unpaired_device(PIXEL));
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();

    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("first tick publishes rows");
    assert_eq!(update.rows.len(), 1, "paired/connected devices only");
    // Phone-style discovery (plan §9): the discovery list shows ALL devices
    // the adapter sees — paired devices in range included — so a phone that
    // is already bonded never makes "Add Device" look like a stuck scan.
    assert_eq!(update.discovered.len(), 2, "all devices are discovery rows");
    assert!(
        update
            .discovered
            .iter()
            .any(|row| row.paired && row.address == PHONE.to_string()),
        "the paired phone appears in discovery with its paired status"
    );
    assert!(
        update
            .discovered
            .iter()
            .any(|row| row.address == PIXEL.to_string()),
        "the unpaired device appears in discovery"
    );
    assert!(!update.scanning);
    assert_eq!(update.adapter_alias.as_deref(), Some("RaspberryPi-DSP"));

    let unchanged = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );
    assert!(unchanged.is_none(), "identical state is deduped");

    commands.push(DeviceCommand::Refresh);
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some(),
        "Refresh clears the dedup baseline"
    );

    commands.push(DeviceCommand::StartDiscovery);
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("discovery start publishes");
    assert!(update.scanning);
    assert!(mock.discovering());
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StartDiscovery));

    commands.push(DeviceCommand::StopDiscovery);
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("discovery stop publishes");
    assert!(!update.scanning);
    assert!(!mock.discovering());
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StopDiscovery));
}

#[test]
fn stop_discovery_is_scoped_to_screen_started_scans() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();

    // Warm tick caches the adapter id so the stop below never probes lazily.
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some()
    );

    // Stop without a screen-started scan must not touch the adapter (the
    // screen may close a scan it never started — e.g. close after reap).
    commands.push(DeviceCommand::StopDiscovery);
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_none(),
        "skipped stop publishes nothing"
    );
    assert_eq!(
        mock.last_admin_action(),
        None,
        "adapter must not be called for a never-started scan"
    );

    // Start → stop still executes as before.
    commands.push(DeviceCommand::StartDiscovery);
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some()
    );
    assert!(mock.discovering());
    commands.push(DeviceCommand::StopDiscovery);
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some()
    );
    assert!(!mock.discovering());
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StopDiscovery));
}

#[test]
fn action_failure_stages_toast_and_keeps_rows() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some()
    );

    mock.set_fault(Some("admin fault".into()));
    commands.push(DeviceCommand::Connect(PHONE));
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("failure toast publishes");
    assert_eq!(update.toast.as_deref(), Some("Operation failed"));
    assert_eq!(update.rows.len(), 1, "rows are kept through the failure");
    assert_eq!(mock.last_admin_action(), None, "fault blocked the write");
    mock.set_fault(None);
}

#[test]
fn missing_adapter_stages_unavailable_toast() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    mock.set_adapter(None);
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    commands.push(DeviceCommand::StartDiscovery);
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("adapter failure publishes a toast");
    assert_eq!(
        update.toast.as_deref(),
        Some("Bluetooth adapter not available")
    );
    assert_eq!(mock.last_admin_action(), None);
}

#[test]
fn transient_snapshot_error_keeps_last_rows() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some()
    );

    mock.set_fault(Some("query hiccup".into()));
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_none(),
        "a failed snapshot must never blank the screen"
    );

    mock.set_fault(None);
    commands.push(DeviceCommand::Refresh);
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("refresh republishes");
    assert_eq!(update.rows.len(), 1, "rows survived the transient error");
}

#[test]
fn remove_disconnects_before_deleting_the_row() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_some()
    );

    commands.push(DeviceCommand::Remove(PHONE));
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("removal republishes rows");
    assert!(update.rows.is_empty(), "the device row is gone");
    assert_eq!(mock.last_admin_action(), Some(AdminAction::Remove));
}

// ---------------------------------------------------------------------------
// Pairing state machine
// ---------------------------------------------------------------------------

/// Dispatches a left-click at a logical window position so status-bar entry
/// points are hit-tested through the real pointer pipeline.
fn click(window: &MinimalSoftwareWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
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

#[test]
fn open_bt_devices_captures_originating_tab_and_closes() {
    let (shell, _window) = shell_with_window();
    shell.set_tab(1);
    shell.window().invoke_open_bt_devices();
    assert!(shell.window().get_bt_devices_open());
    assert_eq!(shell.window().get_previous_tab(), 1);
    shell.window().invoke_close_bt_devices();
    assert!(!shell.window().get_bt_devices_open());
    assert_eq!(
        shell.window().get_previous_tab(),
        1,
        "close keeps the return tab"
    );
}

#[test]
fn status_bar_taps_open_the_devices_screen() {
    let (shell, window) = shell_with_window();

    // Bluetooth logo (left).
    click(&window, 100.0, 30.0);
    assert!(
        shell.window().get_bt_devices_open(),
        "logo tap opens the screen"
    );
    assert_eq!(shell.window().get_previous_tab(), 0);
    shell.window().invoke_close_bt_devices();
    window.draw_if_needed(|_| {});

    // Device name (center) from the player tab.
    click(&window, 400.0, 30.0);
    assert!(
        shell.window().get_bt_devices_open(),
        "device name tap opens the screen"
    );
    shell.window().invoke_close_bt_devices();
    window.draw_if_needed(|_| {});

    // Codec badge (right): on the KMS mainline the badge keeps its codec
    // information overlay (#62) — it must open the codec HUD, not the
    // devices screen. The badge renders only while a device is connected,
    // so publish a connected row before the badge taps.
    shell.apply_bt_devices(BtDevicesUpdate {
        rows: vec![screen_row("AA:BB:CC:DD:EE:FF", "iPhone 15 Pro", true, true)],
        ..BtDevicesUpdate::default()
    });
    window.draw_if_needed(|_| {});
    click(&window, 600.0, 30.0);
    assert!(
        shell.is_codec_hud_visible(),
        "codec badge tap opens the codec overlay"
    );
    assert!(
        !shell.window().get_bt_devices_open(),
        "codec badge tap must not open the devices screen"
    );
    shell.dismiss_codec_hud();
    window.draw_if_needed(|_| {});

    // Variable-spec codec: sample-rate/bit-depth pills shift the tap target;
    // the tap still lands on the badge and opens the codec HUD.
    shell.set_stream_codec("AAC");
    shell.set_stream_sample_rate("48 kHz");
    shell.set_stream_bit_depth("24-bit");
    window.draw_if_needed(|_| {});
    click(&window, 590.0, 30.0);
    assert!(
        shell.is_codec_hud_visible(),
        "bit-depth badge tap opens the codec overlay"
    );
}

#[test]
fn volume_and_dim_controls_do_not_open_devices_screen() {
    let (shell, window) = shell_with_window();

    click(&window, 764.0, 30.0);
    assert!(
        !shell.window().get_bt_devices_open(),
        "DIM must not open the screen"
    );

    click(&window, 690.0, 30.0);
    assert!(
        !shell.window().get_bt_devices_open(),
        "volume must not open the screen"
    );
    assert!(
        shell.is_volume_hud_active(),
        "volume click must open volume HUD"
    );
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
fn select_covers_active_device_and_adapter_name_fallbacks() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let mut snapshot = bt.link_snapshot().expect("snapshot");

    // Default mock: connected phone plus aliased adapter.
    let details = BtLinkDetails::select(&snapshot, None);
    assert!(details.device.is_some());
    assert!(details.pcm.is_some());
    assert_eq!(details.adapter_name_text().as_str(), "RaspberryPi-DSP");

    // An empty alias falls back to the adapter model name.
    if let Some(ref mut adapter) = snapshot.adapter {
        adapter.alias = "".into();
        adapter.name = "hci0".into();
    }
    let no_alias = BtLinkDetails::select(&snapshot, None);
    assert_eq!(no_alias.adapter_name_text().as_str(), "hci0");

    // A disconnected snapshot clears the selection but keeps the adapter.
    mock.clear_devices();
    mock.clear_pcms();
    let cleared = BtLinkDetails::select(&bt.link_snapshot().expect("snapshot"), None);
    assert!(cleared.device.is_none());
    assert!(cleared.pcm.is_none());
    assert_eq!(cleared.adapter_name_text().as_str(), "RaspberryPi-DSP");

    // A missing adapter renders the placeholder.
    mock.set_adapter(None);
    let absent = BtLinkDetails::select(&bt.link_snapshot().expect("snapshot"), None);
    assert!(absent.adapter.is_none());
    assert_eq!(absent.adapter_name_text().as_str(), "—");
}
