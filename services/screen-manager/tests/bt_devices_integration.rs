#![cfg(feature = "ui")]
//! Integration tests for the Bluetooth Devices screen (plan §9).
//!
//! Pure row presentation and the shell controller are host-hermetic. The
//! bt-monitor worker runs against `BluetoothDevice::new_mocked()`, and the
//! pairing state machine is driven against a private `dbus-daemon` with a
//! fake `org.bluez` (repository private-bus testing rule). All D-Bus tests
//! serialize on `ENV_GUARD` because zbus reads `DBUS_SYSTEM_BUS_ADDRESS`
//! from the process environment on every system-bus connection.

use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    PairingStatus, SCREEN_AGENT_CAPABILITY, SCREEN_AGENT_PATH, TransportCommandQueue,
};
use speaker_common::diag::spawn_private_bus;
use zbus::blocking::Connection as BlockingConnection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const PHONE: BtAddress = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
const PIXEL: BtAddress = BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const DEVICE_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const ADAPTER_ADDRESS: &str = "DC:A6:32:11:22:33";
const ADAPTER_ALIAS: &str = "FakePi";

/// Serializes every reader of `DBUS_SYSTEM_BUS_ADDRESS` within this test binary.
static ENV_GUARD: Mutex<()> = Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn set_bus_env(address: &str) {
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // DBUS_SYSTEM_BUS_ADDRESS while this guard is held.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", address) };
}

fn clear_bus_env() {
    // SAFETY: same serialized-guard invariant as set_bus_env.
    unsafe { std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS") };
}

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

/// Drives one full worker tick and returns the staged screen update.
fn tick(
    state: &mut BtMonitorState,
    bt: &BluetoothDevice,
    shared: &Mutex<BtSharedUpdate>,
    transport: &TransportCommandQueue,
    commands: &DeviceCommandQueue,
    pairing_timeout: Duration,
) -> Option<BtDevicesUpdate> {
    assert!(
        state.poll_tick(bt, shared, transport, commands, pairing_timeout),
        "a tick only fails when a transport command interrupts it"
    );
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .devices
        .take()
}

/// In-memory `tracing` writer so failure diagnostics can show the raw error.
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log sink").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs(log: &Arc<Mutex<Vec<u8>>>) -> tracing::Dispatch {
    let sink = Arc::clone(log);
    tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(move || LogSink(Arc::clone(&sink)))
            .finish(),
    )
}

fn captured_log(log: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&log.lock().expect("log sink")).to_string()
}

/// Asserts the agent was registered on the fake before `Pair()` reached
/// `BlueZ`, with the exact registration payload.
fn assert_agent_registered_before_pair(calls: &Arc<Mutex<FakeCalls>>) {
    let guard = calls.lock().expect("fake lock");
    let seq = &guard.seq;
    let registered = seq
        .iter()
        .position(|entry| entry == "register_agent")
        .expect("agent registers first");
    let paired = seq
        .iter()
        .position(|entry| entry == "pair")
        .expect("Pair reaches BlueZ");
    assert!(registered < paired, "agent before Pair: {seq:?}");
    assert_eq!(
        guard.register_agent,
        vec![(
            SCREEN_AGENT_PATH.to_string(),
            SCREEN_AGENT_CAPABILITY.to_string()
        )]
    );
    assert_eq!(guard.request_default_agent, 1);
}

// ---------------------------------------------------------------------------
// Fake `org.bluez` for the live pairing state machine
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeCalls {
    seq: Vec<String>,
    register_agent: Vec<(String, String)>,
    request_default_agent: u32,
    unregister_agent: u32,
    pair_entered: u32,
    cancel_pairing: u32,
}

struct AdapterProps {
    address: &'static str,
    name: &'static str,
    alias: &'static str,
    powered: bool,
    pairable: bool,
    discovering: bool,
}

const ADAPTER_PROPS: AdapterProps = AdapterProps {
    address: ADAPTER_ADDRESS,
    name: "hci0",
    alias: ADAPTER_ALIAS,
    powered: true,
    pairable: true,
    discovering: false,
};

struct FakeAdapter1 {
    props: &'static AdapterProps,
}

#[zbus::interface(name = "org.bluez.Adapter1")]
impl FakeAdapter1 {
    #[zbus(property)]
    fn address(&self) -> String {
        self.props.address.to_string()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.props.name.to_string()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.props.alias.to_string()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        self.props.powered
    }

    #[zbus(property)]
    fn pairable(&self) -> bool {
        self.props.pairable
    }

    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.props.discovering
    }
}

struct DeviceProps {
    address: &'static str,
    name: &'static str,
    alias: &'static str,
    connected: bool,
    paired: bool,
    trusted: bool,
}

const DEVICE_PROPS: DeviceProps = DeviceProps {
    address: "AA:BB:CC:DD:EE:FF",
    name: "iPhone",
    alias: "iPhone 15 Pro",
    connected: false,
    paired: true,
    trusted: true,
};

struct FakeDevice1 {
    calls: Arc<Mutex<FakeCalls>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    props: &'static DeviceProps,
}

/// Waits until pairing is cancelled (condvar, spurious-wakeup safe) or the
/// 5s safety window elapses; returns `true` when cancelled. Runs on tokio's
/// blocking pool so the zbus executor stays free for concurrent calls.
fn wait_released(release: &(Mutex<bool>, Condvar)) -> bool {
    let (lock, cv) = release;
    let mut released = lock.lock().expect("release lock");
    while !*released {
        let (guard, timeout) = cv
            .wait_timeout(released, Duration::from_secs(5))
            .expect("release condvar");
        released = guard;
        if timeout.timed_out() {
            break;
        }
    }
    let cancelled = *released;
    *released = false; // re-arm the next pairing session
    cancelled
}

#[zbus::interface(name = "org.bluez.Device1")]
impl FakeDevice1 {
    #[zbus(property)]
    fn address(&self) -> String {
        self.props.address.to_string()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.props.name.to_string()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.props.alias.to_string()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.props.connected
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        self.props.paired
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.props.trusted
    }

    /// Blocks like `BlueZ`'s `Pair()` until the daemon cancels it (or the 5s
    /// safety fallback elapses) and always completes with an authentication
    /// error, mimicking a cancelled pairing window.
    ///
    /// The condvar wait runs on tokio's blocking pool: a zbus interface
    /// method future drives the connection's dispatch path while it runs, so
    /// parking that executor stalls every other call on the fake (notably the
    /// worker's `GetManagedObjects` snapshot, deadlocking the test).
    async fn pair(&self) -> zbus::fdo::Result<()> {
        self.record_pair_entry();
        let release = Arc::clone(&self.release);
        let cancelled = tokio::task::spawn_blocking(move || wait_released(&release))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("pair wait join: {e}")))?;
        Err(zbus::fdo::Error::Failed(if cancelled {
            "org.bluez.Error.AuthenticationCanceled".to_string()
        } else {
            "pair wait expired".to_string()
        }))
    }

    /// Records `Pair()` entry on the fake; sync so the guard never leaks
    /// into the async `pair()` future (Send across await).
    fn record_pair_entry(&self) {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("pair".to_string());
        calls.pair_entered += 1;
    }

    fn cancel_pairing(&self) {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("cancel_pairing".to_string());
        calls.cancel_pairing += 1;
        drop(calls);
        let (lock, cv) = &*self.release;
        *lock.lock().expect("release lock") = true;
        cv.notify_all();
    }
}

struct FakeAgentManager {
    calls: Arc<Mutex<FakeCalls>>,
}

#[zbus::interface(name = "org.bluez.AgentManager1")]
#[expect(
    clippy::needless_pass_by_value,
    reason = "D-Bus interface method signatures are fixed by the wire contract"
)]
impl FakeAgentManager {
    fn register_agent(&self, agent: ObjectPath<'_>, capability: String) {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("register_agent".to_string());
        calls.register_agent.push((agent.to_string(), capability));
    }

    fn request_default_agent(&self, agent: ObjectPath<'_>) {
        let _ = agent;
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("request_default_agent".to_string());
        calls.request_default_agent += 1;
    }

    fn unregister_agent(&self, agent: ObjectPath<'_>) {
        let _ = agent;
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("unregister_agent".to_string());
        calls.unregister_agent += 1;
    }
}

#[zbus::proxy(
    interface = "org.bluez.Agent1",
    gen_async = false,
    blocking_name = "AgentClientProxyBlocking"
)]
trait AgentClient {
    fn request_confirmation(&self, device: &ObjectPath<'_>, passkey: u32) -> zbus::Result<()>;
}

fn blocking_conn(address: &str) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .build()
        .expect("blocking connection")
}

/// Full fake `org.bluez`: `ObjectManager` at `/` (production-accurate and
/// required for `GetManagedObjects`), adapter, device, and agent manager.
fn fake_bluez(
    address: &str,
    calls: &Arc<Mutex<FakeCalls>>,
    release: &Arc<(Mutex<bool>, Condvar)>,
) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at("/", zbus::fdo::ObjectManager)
        .expect("serve object manager")
        .serve_at(
            ADAPTER_PATH,
            FakeAdapter1 {
                props: &ADAPTER_PROPS,
            },
        )
        .expect("serve adapter")
        .serve_at(
            DEVICE_PATH,
            FakeDevice1 {
                calls: Arc::clone(calls),
                release: Arc::clone(release),
                props: &DEVICE_PROPS,
            },
        )
        .expect("serve device")
        .serve_at(
            "/org/bluez",
            FakeAgentManager {
                calls: Arc::clone(calls),
            },
        )
        .expect("serve agent manager")
        .build()
        .expect("fake org.bluez connection")
}

/// Minimal fake `org.bluez`: only the agent manager (mock-backend pairing
/// tests never query `GetManagedObjects`).
fn fake_agent_manager(address: &str, calls: &Arc<Mutex<FakeCalls>>) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at(
            "/org/bluez",
            FakeAgentManager {
                calls: Arc::clone(calls),
            },
        )
        .expect("serve agent manager")
        .build()
        .expect("fake org.bluez connection")
}

fn unique_names(probe: &BlockingConnection) -> Vec<String> {
    zbus::blocking::fdo::DBusProxy::new(probe)
        .expect("bus proxy")
        .list_names()
        .expect("ListNames")
        .iter()
        .map(std::string::ToString::to_string)
        .filter(|name| name.starts_with(':'))
        .collect()
}

/// Plays `BlueZ`: confirms the pairing passkey to the screen's agent, found
/// as the only connection that appeared after pairing began.
fn confirm_passkey(probe: &BlockingConnection, baseline: &[String]) {
    let mut fresh: Vec<String> = unique_names(probe)
        .into_iter()
        .filter(|name| !baseline.contains(name))
        .collect();
    let agent = fresh
        .pop()
        .expect("the pairing agent must own a bus connection");
    assert!(fresh.is_empty(), "only the agent may be new: {fresh:?}");

    let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
    let proxy = AgentClientProxyBlocking::builder(probe)
        .destination(agent.as_str())
        .expect("agent destination")
        .path(SCREEN_AGENT_PATH)
        .expect("agent path")
        .build()
        .expect("agent client");
    proxy
        .request_confirmation(&device, 482_910)
        .expect("confirmation reaches the agent");
}

/// Ticks until the worker reaps the pairing session (agent unregistered).
fn tick_until_reaped(
    calls: &Arc<Mutex<FakeCalls>>,
    state: &mut BtMonitorState,
    bt: &BluetoothDevice,
    shared: &Mutex<BtSharedUpdate>,
    transport: &TransportCommandQueue,
    commands: &DeviceCommandQueue,
) {
    for _ in 0..50 {
        let reaped = calls.lock().expect("fake lock").unregister_agent;
        if reaped >= 1 {
            return;
        }
        tick(state, bt, shared, transport, commands, BT_PAIRING_TIMEOUT);
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the pairing agent must be unregistered when the session reaps");
}

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

    let mut changed = update;
    changed.scanning = true;
    assert!(controller.accepts(&changed));
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

    w.set_bt_devices_mode(42);
    shell.apply_bt_devices(BtDevicesUpdate {
        pairing: Some(PairingStatus::Idle),
        ..BtDevicesUpdate::default()
    });
    assert_eq!(w.get_bt_devices_mode(), 42, "Idle is a no-op transition");
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
    assert_eq!(queue.drain(), vec![DeviceCommand::StartDiscovery]);

    w.invoke_bt_cancel_scan_clicked();
    assert_eq!(
        queue.drain(),
        vec![DeviceCommand::StopDiscovery, DeviceCommand::Refresh],
        "Cancel stops discovery before requesting a refresh"
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

    w.set_bt_devices_mode(2);
    assert!(w.get_is_animating(), "pairing ring animates");

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
    assert_eq!(update.discovered.len(), 1, "unpaired devices are discovery");
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

#[test]
fn pairing_registers_agent_before_pair_and_fails_without_a_bus() {
    let _guard = lock_env();
    set_bus_env("unix:path=/nx");

    let (bt, mock) = BluetoothDevice::new_mocked();
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

    commands.push(DeviceCommand::Pair(PHONE));
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("registration failure stages a toast");
    assert!(update.pairing.is_none(), "no session without an agent");
    assert!(update.toast.is_some());
    assert_eq!(
        mock.last_admin_action(),
        None,
        "Pair must not be issued before the agent registers"
    );

    // A second attempt retries registration; an earlier session would have
    // staged "Operation already in progress" instead of another failure.
    commands.push(DeviceCommand::Pair(PHONE));
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("second failure stages a toast");
    assert!(update.toast.is_some());
    assert_ne!(
        update.toast.as_deref(),
        Some("Operation already in progress")
    );
    assert_eq!(mock.last_admin_action(), None);

    clear_bus_env();
}

#[test]
fn mock_pairing_success_reaps_agent_and_stops_discovery() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_agent_manager(&address, &calls);
    let (bt, mock) = BluetoothDevice::new_mocked();
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
    commands.push(DeviceCommand::Pair(PHONE));
    let mut terminal = None;
    for _ in 0..50 {
        let update = tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        );
        if let Some(update) = update
            && let Some(status) = update.pairing
            && !matches!(
                status,
                PairingStatus::Connecting { .. } | PairingStatus::AwaitingConfirmation { .. }
            )
        {
            terminal = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(terminal, Some(PairingStatus::Paired { .. })),
        "mock pairing must complete, got {terminal:?}"
    );

    let guard = calls.lock().expect("fake lock");
    assert_eq!(guard.register_agent.len(), 1, "one agent registered");
    assert_eq!(guard.request_default_agent, 1);
    assert_eq!(guard.unregister_agent, 1, "terminal reaps the agent");
    drop(guard);
    assert!(!mock.discovering(), "reap stops screen-started discovery");
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StopDiscovery));

    // The reap stopped discovery, so one tick still publishes the cleared
    // scanning flag; only after that settles is CancelPairing a pure no-op.
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    commands.push(DeviceCommand::CancelPairing);
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
        "a reaped session turns CancelPairing into a no-op"
    );
    clear_bus_env();
}

#[test]
fn mock_pairing_fault_publishes_failed_terminal() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_agent_manager(&address, &calls);
    let (bt, mock) = BluetoothDevice::new_mocked();
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

    mock.set_fault(Some("pair fault".into()));
    commands.push(DeviceCommand::Pair(PHONE));
    let mut terminal = None;
    for _ in 0..50 {
        let update = tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        );
        if let Some(update) = update
            && let Some(status) = update.pairing
            && !matches!(status, PairingStatus::Connecting { .. })
        {
            terminal = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match terminal {
        Some(PairingStatus::Failed { message }) => {
            assert_eq!(message.as_ref(), "Operation failed");
        }
        other => panic!("faulted pairing must fail, got {other:?}"),
    }

    let guard = calls.lock().expect("fake lock");
    assert_eq!(guard.unregister_agent, 1, "failure also reaps the agent");
    drop(guard);
    clear_bus_env();
}

#[test]
fn live_pairing_publishes_passkey_then_times_out() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let _fake = fake_bluez(&address, &calls, &release);
    let probe = blocking_conn(&address);
    let bt = BluetoothDevice::with_connection(blocking_conn(&address));
    let baseline = unique_names(&probe);
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let first = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("first tick publishes rows from the fake adapter");
    assert_eq!(first.rows.len(), 1);
    assert_eq!(first.adapter_alias.as_deref(), Some(ADAPTER_ALIAS));

    // Short injected deadline: agent registration and the passkey must be
    // observed well inside it (production passes BT_PAIRING_TIMEOUT).
    let deadline = Duration::from_millis(800);
    commands.push(DeviceCommand::Pair(PHONE));
    let log = Arc::new(Mutex::new(Vec::new()));
    let dispatch = capture_logs(&log);
    let update = tracing::dispatcher::with_default(&dispatch, || {
        tick(&mut state, &bt, &shared, &transport, &commands, deadline)
    })
    .expect("connecting publishes");
    assert!(
        matches!(update.pairing, Some(PairingStatus::Connecting { .. })),
        "expected Connecting, got {:?} (toast: {:?}) | log: {} | seq: {:?}",
        update.pairing,
        update.toast,
        captured_log(&log),
        calls.lock().expect("fake lock").seq
    );
    assert_agent_registered_before_pair(&calls);

    confirm_passkey(&probe, &baseline);
    let update =
        tick(&mut state, &bt, &shared, &transport, &commands, deadline).expect("passkey publishes");
    match update.pairing {
        Some(PairingStatus::AwaitingConfirmation { name, passkey, .. }) => {
            assert_eq!(passkey, 482_910);
            assert_eq!(name.as_ref(), "iPhone 15 Pro");
        }
        other => panic!("expected passkey confirmation, got {other:?}"),
    }

    // A second Pair while one is in flight must not restart the attempt.
    commands.push(DeviceCommand::Pair(PHONE));
    let update = tick(&mut state, &bt, &shared, &transport, &commands, deadline)
        .expect("in-progress toast publishes");
    assert_eq!(
        update.toast.as_deref(),
        Some("Operation already in progress")
    );
    assert_eq!(
        calls.lock().expect("fake lock").pair_entered,
        1,
        "second Pair must not reach BlueZ"
    );

    std::thread::sleep(Duration::from_millis(900));
    let update =
        tick(&mut state, &bt, &shared, &transport, &commands, deadline).expect("timeout publishes");
    assert!(matches!(update.pairing, Some(PairingStatus::TimedOut)));
    assert_eq!(update.toast.as_deref(), Some("Pairing timed out"));
    assert_eq!(
        calls.lock().expect("fake lock").cancel_pairing,
        1,
        "the deadline cancels the BlueZ pair"
    );

    tick_until_reaped(&calls, &mut state, &bt, &shared, &transport, &commands);
    commands.push(DeviceCommand::CancelPairing);
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
        "a reaped session turns CancelPairing into a no-op"
    );
    clear_bus_env();
}

#[test]
fn live_pairing_user_cancellation_publishes_cancelled() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let _fake = fake_bluez(&address, &calls, &release);
    let bt = BluetoothDevice::with_connection(blocking_conn(&address));
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

    commands.push(DeviceCommand::Pair(PHONE));
    let log = Arc::new(Mutex::new(Vec::new()));
    let dispatch = capture_logs(&log);
    let update = tracing::dispatcher::with_default(&dispatch, || {
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
    })
    .expect("connecting publishes");
    assert!(
        matches!(update.pairing, Some(PairingStatus::Connecting { .. })),
        "expected Connecting, got {:?} (toast: {:?}) | log: {} | seq: {:?}",
        update.pairing,
        update.toast,
        captured_log(&log),
        calls.lock().expect("fake lock").seq
    );

    commands.push(DeviceCommand::CancelPairing);
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("cancellation publishes");
    assert!(matches!(update.pairing, Some(PairingStatus::Cancelled)));
    assert!(update.toast.is_none(), "cancellation is not an error toast");
    assert_eq!(calls.lock().expect("fake lock").cancel_pairing, 1);

    tick_until_reaped(&calls, &mut state, &bt, &shared, &transport, &commands);
    let guard = calls.lock().expect("fake lock");
    assert_eq!(guard.unregister_agent, 1);
    assert_eq!(guard.pair_entered, 1);
    assert!(guard.seq.contains(&"request_default_agent".to_string()));
    drop(guard);
    clear_bus_env();
}

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

    // Codec badge (right): the fixed-spec codec shows only the badge pill.
    click(&window, 600.0, 30.0);
    assert!(
        shell.window().get_bt_devices_open(),
        "codec badge tap opens the screen"
    );
    shell.window().invoke_close_bt_devices();
    window.draw_if_needed(|_| {});

    // Variable-spec codec: sample-rate/bit-depth pills shift the tap target.
    shell.set_stream_codec("AAC");
    shell.set_stream_sample_rate("48 kHz");
    shell.set_stream_bit_depth("24-bit");
    window.draw_if_needed(|_| {});
    click(&window, 590.0, 30.0);
    assert!(
        shell.window().get_bt_devices_open(),
        "bit-depth badge tap opens the screen"
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

#[test]
fn marquee_rows_are_empty_off_the_player_tab() {
    let (shell, _window) = shell_with_window();
    shell.set_track_metadata(
        "A Deliberately Long Track Title That Overflows The Marquee Viewport Width",
        "Some Artist",
        "Audiophile DSP Sink",
    );
    shell.set_marquee_compose_in_rust(true);

    // The old tab-2 details screen is retired; any non-player tab must refuse
    // to compose stale marquee geometry.
    shell.set_tab(4);
    assert!(
        shell.marquee_row_specs().is_empty(),
        "stale marquee geometry must not be composed over other tabs"
    );
}
