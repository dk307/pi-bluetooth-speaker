//! Live D-Bus integration tests for the real `DbusClient` transport paths
//! (`player_seek`, `player_toggle`, `query_bluealsa`, codec querying and
//! service-absent mapping) against a fake BlueZ/BlueALSA stack served on a
//! private `dbus-daemon` session bus — no system bus or hardware required.
//!
//! The mock-device suite (`bluetooth_integration.rs`) covers the `Backend::Mock`
//! layer; this file exercises the actual `Backend::Dbus` methods that the CRAP
//! report flagged as 0% covered, over a real D-Bus transport.

use std::collections::HashMap;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use speaker_bluetooth::bluetooth_test_helpers::{self, DbusClient, ManagedObjects};
use speaker_bluetooth::{BluetoothDevice, BtAddress, BtDevice};

fn to_val<T: Into<Value<'static>>>(v: T) -> OwnedValue {
    OwnedValue::try_from(v.into()).unwrap()
}

fn iface(name: &'static str) -> OwnedInterfaceName {
    OwnedInterfaceName::try_from(name).unwrap()
}

/// Kills the private `dbus-daemon` when the test ends.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns a private session bus and returns its `KillOnDrop` guard plus the
/// bus address. Fails loudly when `dbus-daemon` is missing.
fn spawn_private_bus() -> (KillOnDrop, String) {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed to run D-Bus live integration tests");
    let mut address = String::new();
    std::io::BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    (KillOnDrop(daemon), address)
}

/// Fake `org.freedesktop.DBus.ObjectManager` serving a static snapshot,
/// exactly like real BlueZ/BlueALSA `GetManagedObjects`.
struct FakeObjectManager(ManagedObjects);

#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl FakeObjectManager {
    fn get_managed_objects(&self) -> ManagedObjects {
        self.0.clone()
    }
}

/// Records every player method call issued by the client under test.
#[derive(Default)]
struct PlayerCalls {
    methods: Vec<String>,
    seeks: Vec<i64>,
}

type SharedCalls = Arc<Mutex<PlayerCalls>>;

/// Fake `org.bluez.MediaPlayer1` exposing `Play`, `Pause` and `Seek`.
struct FakeMediaPlayer {
    calls: SharedCalls,
}

#[zbus::interface(name = "org.bluez.MediaPlayer1")]
impl FakeMediaPlayer {
    fn play(&self) {
        self.calls.lock().unwrap().methods.push("Play".into());
    }

    fn pause(&self) {
        self.calls.lock().unwrap().methods.push("Pause".into());
    }

    fn seek(&self, offset: i64) {
        let mut calls = self.calls.lock().unwrap();
        calls.methods.push("Seek".into());
        calls.seeks.push(offset);
    }
}

/// Fake `org.bluealsa.PCM1` answering `GetCodecs`.
struct FakePcm {
    codecs: HashMap<String, HashMap<String, OwnedValue>>,
    fail: bool,
}

#[zbus::interface(name = "org.bluealsa.PCM1")]
impl FakePcm {
    fn get_codecs(&self) -> zbus::fdo::Result<HashMap<String, HashMap<String, OwnedValue>>> {
        if self.fail {
            return Err(zbus::fdo::Error::AccessDenied("denied".into()));
        }
        Ok(self.codecs.clone())
    }
}

/// Builds the `BlueZ` managed-objects fixture: one adapter, one connected
/// device, and one actively playing `MediaPlayer1` at `Position` 45 s.
fn bluez_fixture() -> ManagedObjects {
    let mut objects: ManagedObjects = HashMap::new();

    let adapter_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let mut adapter_ifaces = HashMap::new();
    let mut adapter_props = HashMap::new();
    adapter_props.insert("Address".to_string(), to_val("DC:A6:32:11:22:33"));
    adapter_props.insert("Name".to_string(), to_val("RaspberryPi"));
    adapter_props.insert("Powered".to_string(), to_val(true));
    adapter_ifaces.insert(iface("org.bluez.Adapter1"), adapter_props);
    objects.insert(adapter_path, adapter_ifaces);

    let dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap();
    let mut dev_ifaces = HashMap::new();
    let mut dev_props = HashMap::new();
    dev_props.insert("Address".to_string(), to_val("AA:BB:CC:DD:EE:FF"));
    dev_props.insert("Name".to_string(), to_val("Pixel 8 Pro"));
    dev_props.insert("Connected".to_string(), to_val(true));
    dev_ifaces.insert(iface("org.bluez.Device1"), dev_props);
    objects.insert(dev_path, dev_ifaces);

    let player_path =
        OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0").unwrap();
    let mut player_ifaces = HashMap::new();
    let mut player_props = HashMap::new();
    player_props.insert("Status".to_string(), to_val("playing"));
    player_props.insert("Position".to_string(), to_val(45_000u32));
    player_props.insert(
        "Device".to_string(),
        to_val(OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap()),
    );
    player_ifaces.insert(iface("org.bluez.MediaPlayer1"), player_props);
    objects.insert(player_path, player_ifaces);

    objects
}

/// Builds the `BlueALSA` managed-objects fixture: one A2DP source PCM.
fn bluealsa_fixture() -> ManagedObjects {
    let mut objects: ManagedObjects = HashMap::new();
    let pcm_path =
        OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source")
            .unwrap();
    let mut pcm_ifaces = HashMap::new();
    let mut pcm_props = HashMap::new();
    pcm_props.insert(
        "Device".to_string(),
        to_val("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
    );
    pcm_props.insert("Transport".to_string(), to_val("A2DP-sink"));
    pcm_props.insert("Running".to_string(), to_val(true));
    pcm_props.insert("Codec".to_string(), to_val("SBC"));
    pcm_props.insert("Sampling".to_string(), to_val(48000u32));
    pcm_props.insert("Channels".to_string(), to_val(2u8));
    pcm_props.insert("Format".to_string(), to_val(0x8210u16));
    pcm_ifaces.insert(iface("org.bluealsa.PCM1"), pcm_props);
    objects.insert(pcm_path, pcm_ifaces);
    objects
}

fn test_device() -> BtDevice {
    BtDevice {
        address: BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        name: "Pixel 8 Pro".into(),
        alias: "Pixel 8 Pro".into(),
        connected: true,
        paired: true,
        trusted: true,
        blocked: false,
        rssi: Some(-50),
        battery_percentage: None,
        icon: Some("phone".into()),
        uuids: Box::default(),
    }
}

/// Connects a device client to the private bus addressed by `address`.
fn connect_device(address: &str) -> BluetoothDevice {
    let conn = zbus::blocking::connection::Builder::address(address)
        .unwrap()
        .build()
        .unwrap();
    BluetoothDevice::with_connection(conn)
}

/// Connects a raw `DbusClient` to the private bus addressed by `address`.
fn connect_client(address: &str) -> DbusClient {
    let conn = zbus::blocking::connection::Builder::address(address)
        .unwrap()
        .build()
        .unwrap();
    DbusClient::new(conn)
}

#[test]
fn player_seek_translates_absolute_position_into_relative_offset() {
    let (_kill, address) = spawn_private_bus();
    let calls: SharedCalls = Arc::default();

    let _server = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .serve_at("/", FakeObjectManager(bluez_fixture()))
        .unwrap()
        .serve_at(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0",
            FakeMediaPlayer {
                calls: Arc::clone(&calls),
            },
        )
        .unwrap()
        .build()
        .unwrap();

    let client = connect_device(&address);
    // Current position 45_000 ms; seeking to 150_000 ms must issue the
    // relative offset (150_000 - 45_000) * 1_000 µs.
    client.player_seek(150_000).unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.methods, ["Seek"]);
    assert_eq!(calls.seeks, [105_000_000]);
}

#[test]
fn player_toggle_pauses_playing_player_and_reports_next_state() {
    let (_kill, address) = spawn_private_bus();
    let calls: SharedCalls = Arc::default();

    let _server = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .serve_at("/", FakeObjectManager(bluez_fixture()))
        .unwrap()
        .serve_at(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0",
            FakeMediaPlayer {
                calls: Arc::clone(&calls),
            },
        )
        .unwrap()
        .build()
        .unwrap();

    let client = connect_device(&address);
    // The fixture player is "playing": toggling must Pause and report false.
    let next_playing = client.player_toggle().unwrap();
    assert!(!next_playing);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.methods, ["Pause"]);
}

#[test]
fn query_bluealsa_parses_pcms_and_available_codecs() {
    let (_kill, address) = spawn_private_bus();

    let mut codecs = HashMap::new();
    codecs.insert("sbc".to_string(), HashMap::new());
    codecs.insert("aac".to_string(), HashMap::new());

    let _server = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluealsa")
        .unwrap()
        .serve_at("/org/bluealsa", FakeObjectManager(bluealsa_fixture()))
        .unwrap()
        .serve_at(
            "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source",
            FakePcm {
                codecs,
                fail: false,
            },
        )
        .unwrap()
        .build()
        .unwrap();

    let client = connect_client(&address);
    let pcms = bluetooth_test_helpers::query_bluealsa(&client, &[test_device()]).unwrap();
    assert_eq!(pcms.len(), 1);

    let pcm = &pcms[0];
    assert_eq!(pcm.selected_codec.as_ref(), "SBC");
    assert!(pcm.running);
    assert_eq!(pcm.resolved_name.as_ref(), "Pixel 8 Pro");
    assert_eq!(pcm.sampling_rate, 48_000);
    assert_eq!(pcm.channels, 2);
    let codec_names: Vec<&str> = pcm.available_codecs.iter().map(AsRef::<str>::as_ref).collect();
    assert_eq!(
        codec_names,
        ["aac", "sbc"],
        "GetCodecs names must be sorted"
    );
}

#[test]
fn query_bluealsa_propagates_codec_query_faults() {
    let (_kill, address) = spawn_private_bus();

    let _server = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluealsa")
        .unwrap()
        .serve_at("/org/bluealsa", FakeObjectManager(bluealsa_fixture()))
        .unwrap()
        .serve_at(
            "/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/source",
            FakePcm {
                codecs: HashMap::new(),
                fail: true,
            },
        )
        .unwrap()
        .build()
        .unwrap();

    let client = connect_client(&address);
    let err = bluetooth_test_helpers::query_bluealsa(&client, &[test_device()])
        .expect_err("a denied GetCodecs must propagate as a BtError");
    assert!(
        err.is_dbus() && err.to_string().contains("GetCodecs"),
        "AccessDenied is a real transport fault, not a legacy-daemon absence: {err}"
    );
}

#[test]
fn fetch_bluealsa_objects_absent_service_maps_to_empty_snapshot() {
    let (_kill, address) = spawn_private_bus();
    // No server owns `org.bluealsa` on this bus: the name lookup fails with
    // ServiceUnknown, which must map to an empty snapshot, not an error.

    let client = connect_client(&address);
    let pcms = bluetooth_test_helpers::query_bluealsa(&client, &[test_device()]).unwrap();
    assert!(
        pcms.is_empty(),
        "absent bluealsa service must yield no PCM streams"
    );
}
