//! Integration tests for device administration and the screen pairing agent.
//!
//! D-Bus paths run against a private `dbus-daemon` with a fake `org.bluez`
//! (per the repository's private-bus testing rule); mock paths exercise the
//! symmetric failure fidelity of `BluetoothDevice::new_mocked()`.

use std::sync::{Arc, Mutex};

use speaker_bluetooth::{
    AdminAction, BluetoothDevice, BtAddress, BtDevice, BtError, DeviceCommand, DeviceCommandQueue,
    PairingAgentHandle, PairingCode, SCREEN_AGENT_CAPABILITY, SCREEN_AGENT_PATH,
    device_action_message, device_address_from_path, device_object_path,
};
use speaker_common::diag::spawn_private_bus;
use zbus::blocking::Connection as BlockingConnection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const ADAPTER: &str = "hci0";
const DEVICE_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const PHONE: BtAddress = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
const PIXEL: BtAddress = BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

// ---------------------------------------------------------------------------
// Pure helpers: object paths, error messages, command queue
// ---------------------------------------------------------------------------

#[test]
fn device_object_path_builds_bluez_path() {
    let path = device_object_path(ADAPTER, PHONE).expect("valid device path");
    assert_eq!(path.as_str(), DEVICE_PATH);
    assert_eq!(device_address_from_path(path.as_str()), Some(PHONE));
    // Underscore is part of the D-Bus object-path segment alphabet
    // (`[A-Za-z0-9_]`, per zvariant's validator) and must be accepted.
    let underscored = device_object_path("h_ci0", PHONE).expect("underscored adapter id");
    assert_eq!(
        underscored.as_str(),
        "/org/bluez/h_ci0/dev_AA_BB_CC_DD_EE_FF"
    );
}

#[test]
fn device_object_path_rejects_malformed_adapter() {
    // Hyphen is NOT part of the D-Bus object-path segment alphabet (bus
    // *names* allow `-`, object-path elements do not) — rejected by both our
    // validator and `zvariant`.
    for adapter in [
        "",
        "hci0/x",
        "hci 0",
        "../x",
        "hci0..",
        "h\tci0",
        "bt-adapter",
    ] {
        let err =
            device_object_path(adapter, PHONE).expect_err("malformed adapter must be rejected");
        assert!(
            err.is_adapter_not_found(),
            "adapter {adapter:?} must map to AdapterNotFound, got {err}"
        );
        assert_eq!(
            device_action_message(&err),
            "Bluetooth adapter not available"
        );
    }
}

#[test]
fn device_address_from_path_variants() {
    assert_eq!(device_address_from_path(DEVICE_PATH), Some(PHONE));
    assert_eq!(
        device_address_from_path("/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source"),
        Some(PHONE)
    );
    assert_eq!(device_address_from_path("/invalid/path"), None);
    assert_eq!(
        device_address_from_path("/org/bluez/hci0/dev_11_22_33"),
        None
    );
    assert_eq!(device_address_from_path("/org/bluez/hci0/player0"), None);
}

#[test]
fn device_action_message_maps_bluez_error_names() {
    // More specific names must win over generic prefixes; table-driven so a
    // renamed BlueZ error fails loudly here instead of in the UI.
    let cases: &[(&str, &str)] = &[
        (
            "org.bluez.Error.AuthenticationCanceled: gone",
            "Pairing cancelled",
        ),
        (
            "org.bluez.Error.AuthenticationFailed: nope",
            "Pairing failed",
        ),
        (
            "org.bluez.Error.AuthenticationRejected: nope",
            "Pairing rejected",
        ),
        (
            "org.bluez.Error.AuthenticationTimeout: slow",
            "Pairing timed out",
        ),
        (
            "org.bluez.Error.AlreadyConnected: yes",
            "Device already connected",
        ),
        (
            "org.bluez.Error.ConnectionAttemptFailed: down",
            "Could not connect to device",
        ),
        ("org.bluez.Error.DoesNotExist: gone", "Device not available"),
        (
            "org.freedesktop.DBus.Error.UnknownObject: nope",
            "Device not available",
        ),
        (
            "org.bluez.Error.NotReady: off",
            "Bluetooth adapter not ready",
        ),
        ("org.bluez.Error.NotConnected: down", "Device not connected"),
        (
            "org.bluez.Error.NotSupported: no",
            "Operation not supported",
        ),
        (
            "org.bluez.Error.InProgress: busy",
            "Operation already in progress",
        ),
        ("org.bluez.Error.NotAuthorized: no", "Not authorized"),
        ("org.bluez.Error.NotAvailable: no", "Device unavailable"),
        (
            "org.bluez.Error.AlreadyExists: bonded",
            "Device already paired",
        ),
        (
            "org.bluez.Error.InvalidArguments: bad",
            "Invalid device address",
        ),
        // Generic failure and unknown names fall through to the fallback.
        (
            "org.bluez.Error.Failed: generic",
            "Bluetooth operation failed",
        ),
        (
            "something entirely unexpected",
            "Bluetooth operation failed",
        ),
    ];
    for (details, expected) in cases {
        let err = BtError::dbus(*details);
        assert_eq!(device_action_message(&err), *expected, "details: {details}");
    }

    // Domain kinds short-circuit before the name table.
    assert_eq!(
        device_action_message(&BtError::adapter_not_found("hci9")),
        "Bluetooth adapter not available"
    );
    assert_eq!(
        device_action_message(&BtError::invalid_address("+1", "signed")),
        "Invalid device address"
    );
    assert_eq!(
        device_action_message(&BtError::cancelled("task stopped")),
        "Operation cancelled"
    );
    assert_eq!(
        device_action_message(&BtError::mock_fault("boom")),
        "Operation failed"
    );
}

#[test]
fn device_command_queue_drains_fifo_and_bounds_backlog() {
    let queue = DeviceCommandQueue::new();
    assert!(queue.is_empty());

    queue.push(DeviceCommand::Refresh);
    queue.push(DeviceCommand::Connect(PHONE));
    queue.push(DeviceCommand::StartDiscovery);
    assert!(!queue.is_empty());
    assert_eq!(
        queue.drain(),
        vec![
            DeviceCommand::Refresh,
            DeviceCommand::Connect(PHONE),
            DeviceCommand::StartDiscovery
        ]
    );
    assert!(queue.is_empty());

    // Overflow drops the newest command beyond the 32-entry backlog.
    for _ in 0..33 {
        queue.push(DeviceCommand::Remove(PHONE));
    }
    assert_eq!(queue.drain().len(), 32);
}

// ---------------------------------------------------------------------------
// Mock backend: symmetric failure fidelity and observable effects
// ---------------------------------------------------------------------------

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

#[test]
fn mock_discovery_toggles_adapter_and_records_action() {
    let (device, mock) = BluetoothDevice::new_mocked();
    assert!(!mock.discovering());

    device.start_discovery(ADAPTER).expect("start discovery");
    assert!(mock.discovering());
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StartDiscovery));
    // The status snapshot reflects Discovering exactly like parsed BlueZ props.
    let status = device.status().expect("status");
    assert!(status.adapter.expect("adapter").discovering);

    device.stop_discovery(ADAPTER).expect("stop discovery");
    assert!(!mock.discovering());
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StopDiscovery));
}

#[test]
fn mock_adapter_absent_fidelity() {
    let (device, mock) = BluetoothDevice::new_mocked();
    mock.set_adapter(None);

    let err = device
        .start_discovery(ADAPTER)
        .expect_err("discovery without adapter must fail");
    assert!(err.is_adapter_not_found());
    assert_eq!(
        device_action_message(&err),
        "Bluetooth adapter not available"
    );
    assert!(
        device
            .remove_device(ADAPTER, PHONE)
            .expect_err("removal without adapter must fail")
            .is_adapter_not_found()
    );
}

#[test]
fn mock_connect_disconnect_pair_trust_remove_effects() {
    let (device, mock) = BluetoothDevice::new_mocked();

    device
        .disconnect_device(ADAPTER, PHONE)
        .expect("disconnect");
    let status = device.status().expect("status");
    let phone = status
        .devices
        .iter()
        .find(|d| d.address == PHONE)
        .expect("phone present");
    assert!(!phone.connected);

    device.connect_device(ADAPTER, PHONE).expect("connect");
    let status = device.status().expect("status");
    assert!(
        status
            .devices
            .iter()
            .find(|d| d.address == PHONE)
            .expect("phone present")
            .connected
    );
    assert_eq!(mock.last_admin_action(), Some(AdminAction::Connect));

    // Pairing flips only `paired`; trust is an explicit follow-up call.
    mock.add_device(unpaired_device(PIXEL));
    device.pair_device(ADAPTER, PIXEL).expect("pair");
    let status = device.status().expect("status");
    let pixel = status
        .devices
        .iter()
        .find(|d| d.address == PIXEL)
        .expect("pixel present");
    assert!(pixel.paired);
    assert!(!pixel.trusted);

    device.set_trusted(ADAPTER, PIXEL, true).expect("trust");
    let status = device.status().expect("status");
    assert!(
        status
            .devices
            .iter()
            .find(|d| d.address == PIXEL)
            .expect("pixel present")
            .trusted
    );
    assert_eq!(mock.last_admin_action(), Some(AdminAction::SetTrusted));

    device.remove_device(ADAPTER, PIXEL).expect("remove");
    let status = device.status().expect("status");
    assert!(status.devices.iter().all(|d| d.address != PIXEL));
    assert_eq!(mock.last_admin_action(), Some(AdminAction::Remove));
}

#[test]
fn mock_absent_device_error_matches_bluez_shape() {
    let (device, _mock) = BluetoothDevice::new_mocked();
    let ghost = BtAddress::new([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);

    let results = [
        device.connect_device(ADAPTER, ghost),
        device.disconnect_device(ADAPTER, ghost),
        device.pair_device(ADAPTER, ghost),
        device.cancel_pairing(ADAPTER, ghost),
        device.remove_device(ADAPTER, ghost),
        device.set_trusted(ADAPTER, ghost, true),
    ];
    for result in results {
        let err = result.expect_err("unknown device must fail");
        assert!(err.is_dbus(), "expected D-Bus-shaped error, got {err}");
        assert!(
            err.summary().contains("org.bluez.Error.DoesNotExist"),
            "summary must carry the BlueZ error name, got {}",
            err.summary()
        );
        assert_eq!(device_action_message(&err), "Device not available");
    }
}

#[test]
fn mock_cancel_pairing_records_action_without_state_change() {
    let (device, mock) = BluetoothDevice::new_mocked();
    device.cancel_pairing(ADAPTER, PHONE).expect("cancel");
    assert_eq!(mock.last_admin_action(), Some(AdminAction::CancelPairing));
    let status = device.status().expect("status");
    let phone = status
        .devices
        .iter()
        .find(|d| d.address == PHONE)
        .expect("phone present");
    assert!(phone.connected, "cancel must not alter connection state");
}

#[test]
fn mock_fault_injects_into_every_admin_call() {
    let (device, mock) = BluetoothDevice::new_mocked();
    mock.set_fault(Some("admin fault injected".into()));

    let results = [
        device.start_discovery(ADAPTER),
        device.stop_discovery(ADAPTER),
        device.connect_device(ADAPTER, PHONE),
        device.disconnect_device(ADAPTER, PHONE),
        device.pair_device(ADAPTER, PHONE),
        device.cancel_pairing(ADAPTER, PHONE),
        device.remove_device(ADAPTER, PHONE),
        device.set_trusted(ADAPTER, PHONE, false),
    ];
    for result in results {
        let err = result.expect_err("injected fault must fail every admin call");
        assert!(err.is_mock_fault(), "expected mock fault, got {err}");
        assert_eq!(device_action_message(&err), "Operation failed");
    }
}

// ---------------------------------------------------------------------------
// Live D-Bus: fake org.bluez on a private bus
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FakeCalls {
    start_discovery: u32,
    stop_discovery: u32,
    removed: Vec<String>,
    connect: u32,
    disconnect: u32,
    pair: u32,
    cancel_pairing: u32,
    trusted: Option<bool>,
    register_agent: Vec<(String, String)>,
    request_default_agent: u32,
    unregister_agent: u32,
    fail_adapter: bool,
    fail_device: bool,
    fail_agent: bool,
}

struct FakeAdapter1 {
    calls: Arc<Mutex<FakeCalls>>,
}

#[zbus::interface(name = "org.bluez.Adapter1")]
#[expect(
    clippy::needless_pass_by_value,
    reason = "D-Bus interface method signatures are fixed by the wire contract"
)]
impl FakeAdapter1 {
    fn start_discovery(&self) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_adapter {
            return Err(zbus::fdo::Error::Failed(
                "adapter failure injected".to_string(),
            ));
        }
        calls.start_discovery += 1;
        Ok(())
    }

    fn stop_discovery(&self) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_adapter {
            return Err(zbus::fdo::Error::Failed(
                "adapter failure injected".to_string(),
            ));
        }
        calls.stop_discovery += 1;
        Ok(())
    }

    fn remove_device(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_adapter {
            return Err(zbus::fdo::Error::Failed(
                "adapter failure injected".to_string(),
            ));
        }
        calls.removed.push(device.to_string());
        Ok(())
    }
}

struct FakeDevice1 {
    calls: Arc<Mutex<FakeCalls>>,
}

#[zbus::interface(name = "org.bluez.Device1")]
impl FakeDevice1 {
    fn connect(&self) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_device {
            return Err(zbus::fdo::Error::Failed(
                "device failure injected".to_string(),
            ));
        }
        calls.connect += 1;
        Ok(())
    }

    fn disconnect(&self) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_device {
            return Err(zbus::fdo::Error::Failed(
                "device failure injected".to_string(),
            ));
        }
        calls.disconnect += 1;
        Ok(())
    }

    fn pair(&self) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_device {
            return Err(zbus::fdo::Error::Failed(
                "device failure injected".to_string(),
            ));
        }
        calls.pair += 1;
        Ok(())
    }

    fn cancel_pairing(&self) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_device {
            return Err(zbus::fdo::Error::Failed(
                "device failure injected".to_string(),
            ));
        }
        calls.cancel_pairing += 1;
        Ok(())
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.calls
            .lock()
            .expect("fake lock")
            .trusted
            .unwrap_or(false)
    }

    #[zbus(property)]
    fn set_trusted(&mut self, value: bool) {
        self.calls.lock().expect("fake lock").trusted = Some(value);
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
    fn register_agent(&self, agent: ObjectPath<'_>, capability: String) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.register_agent.push((agent.to_string(), capability));
        if calls.fail_agent {
            return Err(zbus::fdo::Error::Failed(
                "agent registration injected".to_string(),
            ));
        }
        Ok(())
    }

    fn request_default_agent(&self, agent: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        let _ = agent;
        let mut calls = self.calls.lock().expect("fake lock");
        if calls.fail_agent {
            return Err(zbus::fdo::Error::Failed(
                "agent registration injected".to_string(),
            ));
        }
        calls.request_default_agent += 1;
        Ok(())
    }

    fn unregister_agent(&self, agent: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        let _ = agent;
        let mut calls = self.calls.lock().expect("fake lock");
        calls.unregister_agent += 1;
        if calls.fail_agent {
            return Err(zbus::fdo::Error::Failed(
                "agent release injected".to_string(),
            ));
        }
        Ok(())
    }
}

/// Client proxy used to play the role of `BlueZ` calling the screen agent.
#[zbus::proxy(
    interface = "org.bluez.Agent1",
    gen_async = false,
    blocking_name = "AgentClientProxyBlocking"
)]
trait AgentClient {
    fn release(&self) -> zbus::Result<()>;
    fn request_pin_code(&self, device: &ObjectPath<'_>) -> zbus::Result<String>;
    fn display_pin_code(&self, device: &ObjectPath<'_>, pincode: &str) -> zbus::Result<()>;
    fn request_passkey(&self, device: &ObjectPath<'_>) -> zbus::Result<u32>;
    fn display_passkey(
        &self,
        device: &ObjectPath<'_>,
        passkey: u32,
        entered: u16,
    ) -> zbus::Result<()>;
    fn request_confirmation(&self, device: &ObjectPath<'_>, passkey: u32) -> zbus::Result<()>;
    fn request_authorization(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
    fn authorize_service(&self, device: &ObjectPath<'_>, uuid: &str) -> zbus::Result<()>;
    fn cancel(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
}

fn blocking_conn(address: &str) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .build()
        .expect("blocking connection")
}

/// Serves a fake `org.bluez` with adapter, device, and agent manager objects.
fn fake_bluez(address: &str, calls: &Arc<Mutex<FakeCalls>>) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at(
            "/org/bluez/hci0",
            FakeAdapter1 {
                calls: Arc::clone(calls),
            },
        )
        .expect("serve adapter")
        .serve_at(
            DEVICE_PATH,
            FakeDevice1 {
                calls: Arc::clone(calls),
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

fn device_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from(DEVICE_PATH).expect("constant device path")
}

#[test]
fn live_discovery_and_removal_calls_reach_bluez() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_bluez(&address, &calls);
    let device = BluetoothDevice::with_connection(blocking_conn(&address));

    device.start_discovery(ADAPTER).expect("start discovery");
    device.stop_discovery(ADAPTER).expect("stop discovery");
    device.remove_device(ADAPTER, PHONE).expect("remove device");

    let calls = calls.lock().expect("fake lock");
    assert_eq!(calls.start_discovery, 1);
    assert_eq!(calls.stop_discovery, 1);
    assert_eq!(calls.removed, vec![DEVICE_PATH.to_string()]);
}

#[test]
fn live_absent_adapter_maps_to_adapter_not_found() {
    let (_guard, address) = spawn_private_bus();
    // Owns `org.bluez` with the production `ObjectManager` at `/` but serves no
    // adapter or device objects: BlueZ replies `UnknownObject` for them.
    let _fake = zbus::blocking::connection::Builder::address(address.as_str())
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at("/", zbus::fdo::ObjectManager)
        .expect("serve object manager")
        .build()
        .expect("fake org.bluez connection");
    let device = BluetoothDevice::with_connection(blocking_conn(&address));

    for result in [
        device.start_discovery(ADAPTER),
        device.stop_discovery(ADAPTER),
        device.remove_device(ADAPTER, PHONE),
    ] {
        let err = result.expect_err("missing adapter object must fail");
        assert!(
            err.is_adapter_not_found(),
            "expected AdapterNotFound, got {err}"
        );
    }
}

#[test]
fn live_connect_pair_cancel_trust_reach_device() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_bluez(&address, &calls);
    let device = BluetoothDevice::with_connection(blocking_conn(&address));

    device.connect_device(ADAPTER, PHONE).expect("connect");
    device.pair_device(ADAPTER, PHONE).expect("pair");
    device.cancel_pairing(ADAPTER, PHONE).expect("cancel");
    device.set_trusted(ADAPTER, PHONE, true).expect("trust");
    device
        .disconnect_device(ADAPTER, PHONE)
        .expect("disconnect");

    let calls = calls.lock().expect("fake lock");
    assert_eq!(calls.connect, 1);
    assert_eq!(calls.pair, 1);
    assert_eq!(calls.cancel_pairing, 1);
    assert_eq!(calls.trusted, Some(true));
    assert_eq!(calls.disconnect, 1);
}

#[test]
fn live_device_failure_is_not_masked_as_adapter_error() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let mut guard = calls.lock().expect("fake lock");
    guard.fail_device = true;
    guard.fail_adapter = true;
    // Release before the fake serves the mutex: the fake's method handlers lock
    // it, and the client below blocks waiting for their replies.
    drop(guard);
    let _fake = fake_bluez(&address, &calls);
    let device = BluetoothDevice::with_connection(blocking_conn(&address));

    let device_err = device
        .connect_device(ADAPTER, PHONE)
        .expect_err("injected device failure");
    assert!(device_err.is_dbus(), "got {device_err}");
    assert!(device_err.summary().contains("Failed"));
    assert_eq!(
        device_action_message(&device_err),
        "Bluetooth operation failed"
    );

    // An adapter failure must stay a D-Bus error, never become AdapterNotFound.
    let adapter_err = device
        .start_discovery(ADAPTER)
        .expect_err("injected adapter failure");
    assert!(adapter_err.is_dbus(), "got {adapter_err}");
    assert!(!adapter_err.is_adapter_not_found());
}

#[test]
fn pairing_agent_registers_records_and_releases() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let fake = fake_bluez(&address, &calls);
    let agent_conn = blocking_conn(&address);
    let agent_unique = agent_conn.unique_name().expect("unique name").to_string();
    let codes = Arc::new(Mutex::new(None));
    let handle =
        PairingAgentHandle::register_on(agent_conn, Arc::clone(&codes)).expect("register agent");

    let state = calls.lock().expect("fake lock");
    assert_eq!(
        state.register_agent,
        vec![(
            SCREEN_AGENT_PATH.to_string(),
            SCREEN_AGENT_CAPABILITY.to_string()
        )]
    );
    assert_eq!(state.request_default_agent, 1);
    drop(state);

    // Play `BlueZ`: invoke the agent's methods at its unique name.
    let proxy = AgentClientProxyBlocking::builder(&fake)
        .destination(agent_unique.as_str())
        .expect("agent destination")
        .path(SCREEN_AGENT_PATH)
        .expect("agent path")
        .build()
        .expect("agent client");

    proxy
        .request_confirmation(&device_path(), 482_910)
        .expect("confirmation");
    assert_eq!(
        handle.current_code(),
        Some(PairingCode {
            device: PHONE,
            passkey: 482_910
        })
    );

    proxy
        .display_passkey(&device_path(), 482_910, 3)
        .expect("display passkey");
    assert_eq!(handle.current_code().map(|c| c.passkey), Some(482_910));

    // Legacy PIN paths report fixed values without recording a comparison.
    assert_eq!(
        proxy.request_pin_code(&device_path()).expect("pin code"),
        "0000"
    );
    assert_eq!(proxy.request_passkey(&device_path()).expect("passkey"), 0);
    proxy
        .display_pin_code(&device_path(), "1234")
        .expect("display pin");
    proxy
        .request_authorization(&device_path())
        .expect("authorization");
    proxy
        .authorize_service(&device_path(), "0000110b-0000-1000-8000-00805f9b34fb")
        .expect("authorize service");
    proxy.release().expect("release");

    // An unparseable device path is logged and never recorded.
    let odd_path = OwnedObjectPath::try_from("/org/bluez/hci0/player0").expect("player path");
    proxy
        .request_confirmation(&odd_path, 111_222)
        .expect("unparseable path still confirms");
    assert_eq!(handle.current_code(), None);

    proxy.cancel(&device_path()).expect("cancel");
    assert_eq!(handle.current_code(), None);

    // Explicit unregister is idempotent with Drop: exactly one release.
    handle.unregister();
    assert_eq!(calls.lock().expect("fake lock").unregister_agent, 1);
}

#[test]
fn pairing_agent_drop_releases_default_role() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_bluez(&address, &calls);
    let codes = Arc::new(Mutex::new(None));

    let handle = PairingAgentHandle::register_on(blocking_conn(&address), codes).expect("register");
    assert_eq!(calls.lock().expect("fake lock").unregister_agent, 0);
    drop(handle);
    assert_eq!(calls.lock().expect("fake lock").unregister_agent, 1);
}

#[test]
fn pairing_agent_registration_failure_returns_error() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let mut guard = calls.lock().expect("fake lock");
    guard.fail_agent = true;
    drop(guard);
    let _fake = fake_bluez(&address, &calls);
    let codes = Arc::new(Mutex::new(None));

    let err = PairingAgentHandle::register_on(blocking_conn(&address), codes)
        .expect_err("injected registration failure");
    assert!(err.is_dbus(), "got {err}");
}

#[test]
fn pairing_agent_unregister_failure_is_swallowed_with_log() {
    let (_guard, address) = spawn_private_bus();
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_bluez(&address, &calls);
    let codes = Arc::new(Mutex::new(None));
    let handle =
        PairingAgentHandle::register_on(blocking_conn(&address), codes).expect("register agent");

    let mut guard = calls.lock().expect("fake lock");
    guard.fail_agent = true;
    drop(guard);
    // Must not panic or retry: the failure is logged and the handle drops.
    handle.unregister();
    assert_eq!(calls.lock().expect("fake lock").unregister_agent, 1);
}

#[test]
fn pairing_status_and_mock_defaults_are_equality_ready() {
    use speaker_bluetooth::PairingStatus;

    // The worker dedups published updates by equality; every variant must be
    // Clone + PartialEq with stable payloads.
    let variants = [
        PairingStatus::Idle,
        PairingStatus::Connecting {
            name: "Phone".into(),
        },
        PairingStatus::AwaitingConfirmation {
            name: "Phone".into(),
            passkey: 482_910,
        },
        PairingStatus::Paired {
            name: "Phone".into(),
        },
        PairingStatus::Failed {
            message: "Pairing failed".into(),
        },
        PairingStatus::TimedOut,
        PairingStatus::Cancelled,
    ];
    for variant in variants {
        assert_eq!(variant.clone(), variant);
    }
    assert_ne!(
        PairingStatus::TimedOut,
        PairingStatus::Failed {
            message: "Pairing failed".into()
        }
    );

    // A fresh mock has recorded no admin action yet.
    let (_device, mock) = BluetoothDevice::new_mocked();
    assert_eq!(mock.last_admin_action(), None);
}
