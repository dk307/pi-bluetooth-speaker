//! Integration and hardware driver tests for the Bluetooth & `BlueALSA` subsystem.

use std::collections::HashMap;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use speaker_bluetooth::bluetooth_test_helpers::{DbusClient, ManagedObjects};
use speaker_bluetooth::{
    BlueAlsaPcm, BluetoothControllerInfo, BluetoothDevice, BluetoothHardwareInfo,
    BluetoothServices, BtAddress, BtDevice, BtError, BtPlaybackState, bluetooth_test_helpers,
    decode_pcm_format, format_duration_ms, seek_offset_us, validate_bip_handle,
};
use speaker_common::systemd::ServiceState;

#[test]
fn address_validation_and_parsing() {
    let valid = "AA:BB:CC:DD:EE:FF";
    let parsed = BtAddress::parse(valid).unwrap();
    assert_eq!(parsed.to_string(), "AA:BB:CC:DD:EE:FF");
    assert_eq!(parsed.as_bytes(), &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    let hyphens = "aa-bb-cc-dd-ee-ff";
    let parsed_h = BtAddress::parse_str(hyphens).unwrap();
    assert_eq!(parsed_h.to_string(), "AA:BB:CC:DD:EE:FF");

    BtAddress::parse_str("+AA:BB:CC:DD:EE:FF").unwrap_err();
    BtAddress::parse_str("AA:+BB:CC:DD:EE:FF").unwrap_err();
    BtAddress::parse_str("-AA:BB:CC:DD:EE:FF").unwrap_err();
    BtAddress::parse_str("AA:BB:CC:DD:EE").unwrap_err();
    BtAddress::parse_str("AA:BB:CC:DD:EE:FF:00").unwrap_err();
    BtAddress::parse_str("AA:GG:CC:DD:EE:FF").unwrap_err();
    BtAddress::parse_str("no delimiters").unwrap_err();

    BtAddress::try_from(valid).unwrap();
    valid.parse::<BtAddress>().unwrap();
}

#[test]
fn service_state_predicates_and_display() {
    assert!(ServiceState::Active.is_active());
    assert!(!ServiceState::Inactive.is_active());
    assert!(!ServiceState::Failed.is_active());
    assert!(!ServiceState::NotFound.is_active());

    assert_eq!(ServiceState::Active.description(), "Running");
    assert_eq!(ServiceState::Inactive.to_string(), "INACTIVE / STOPPED");
    assert_eq!(ServiceState::Failed.to_string(), "FAILED");
    assert_eq!(
        ServiceState::NotFound.to_string(),
        "NOT INSTALLED / NOT FOUND"
    );
}

#[test]
fn bluetooth_services_predicates() {
    let all_ok = BluetoothServices {
        bluetoothd: ServiceState::Active,
        bluealsa: ServiceState::Active,
        bluealsa_aplay: ServiceState::Active,
        bt_agent: ServiceState::Active,
    };
    assert!(all_ok.is_operational());
    assert!(all_ok.all_active());

    let aplay_only = BluetoothServices {
        bluetoothd: ServiceState::Active,
        bluealsa: ServiceState::Inactive,
        bluealsa_aplay: ServiceState::Active,
        bt_agent: ServiceState::Active,
    };
    assert!(aplay_only.is_operational());
    assert!(!aplay_only.all_active());

    let bt_down = BluetoothServices {
        bluetoothd: ServiceState::Inactive,
        bluealsa: ServiceState::Active,
        bluealsa_aplay: ServiceState::Active,
        bt_agent: ServiceState::Active,
    };
    assert!(!bt_down.is_operational());
}

#[test]
fn bluetooth_status_query_methods() {
    let (device, mock) = BluetoothDevice::new_mocked();
    let mut status = device.status().unwrap();

    assert!(status.is_operational());
    assert_eq!(status.connected_count(), 1);
    assert!(status.is_streaming());
    assert!(status.active_pcm().is_some());
    assert_eq!(status.connected_devices().len(), 1);
    assert_eq!(status.paired_devices().len(), 1);
    assert!(status.is_playing());
    assert!(status.playback_state().is_some());

    // Toggle playback to paused and verify status.is_playing()
    mock.set_player_status("paused");
    let paused_status = device.status().unwrap();
    assert!(!paused_status.is_playing());

    // Clear playback and verify None
    mock.set_playback(None);
    let no_pb_status = device.status().unwrap();
    assert!(!no_pb_status.is_playing());
    assert!(no_pb_status.playback_state().is_none());

    // Disconnect adapter power
    mock.set_powered(false);
    assert!(!device.status().unwrap().is_operational());

    // No operational services
    status.services.bluetoothd = ServiceState::Inactive;
    assert!(!status.is_operational());
}

#[test]
fn bt_device_helpers() {
    let addr = BtAddress::new([1, 2, 3, 4, 5, 6]);
    let dev1 = BtDevice {
        address: addr,
        name: "Original".into(),
        alias: "Custom Alias".into(),
        connected: true,
        paired: true,
        trusted: true,
        blocked: false,
        rssi: Some(-50),
        battery_percentage: Some(90),
        icon: Some("phone".into()),
        uuids: Box::default(),
    };
    assert_eq!(dev1.display_name(), "Custom Alias");

    let dev2 = BtDevice {
        address: addr,
        name: "Fallback Name".into(),
        alias: "Unknown".into(),
        connected: false,
        paired: false,
        trusted: false,
        blocked: false,
        rssi: None,
        battery_percentage: None,
        icon: None,
        uuids: Box::default(),
    };
    assert_eq!(dev2.display_name(), "Fallback Name");

    let dev3 = BtDevice {
        address: addr,
        name: "".into(),
        alias: "".into(),
        connected: false,
        paired: false,
        trusted: false,
        blocked: false,
        rssi: None,
        battery_percentage: None,
        icon: None,
        uuids: Box::default(),
    };
    assert_eq!(dev3.display_name(), "Unknown Device");
}

#[test]
fn bluealsa_pcm_stream_state_description() {
    let pcm = BlueAlsaPcm {
        pcm_path: "/org/bluealsa/hci0/dev_00_11_22_33_44_55/a2dpsnk/source".into(),
        device_address: BtAddress::new([0, 0x11, 0x22, 0x33, 0x44, 0x55]),
        resolved_name: "Pixel 8".into(),
        transport: "A2DP-sink".into(),
        running: true,
        selected_codec: "AAC".into(),
        available_codecs: vec!["SBC".into(), "AAC".into()].into_boxed_slice(),
        sampling_rate: 44_100,
        channels: 2,
        format: "S16_LE".into(),
        volume: Some("100%".into()),
        volume_percent: Some(100),
        muted: false,
        delay: None,
    };
    assert_eq!(pcm.stream_state_description(), "STREAMING (Active)");

    let mut paused = pcm.clone();
    paused.running = false;
    assert_eq!(
        paused.stream_state_description(),
        "STANDBY / PAUSED (Connected)"
    );
}

#[test]
fn hardware_info_rpi3_constants() {
    let hw = BluetoothHardwareInfo::rpi3();
    assert_eq!(&*hw.chip, "Broadcom BCM43438 (Bluetooth 4.1 + BLE)");
    assert_eq!(&*hw.bus, "UART (PL011 / /dev/ttyAMA0)");
    assert_eq!(hw.baud_rate, 921_600);
    assert_eq!(&*hw.firmware, "/lib/firmware/brcm/BCM43430A1.hcd");
    assert_eq!(&*hw.driver, "hci_uart (bcm)");
    assert_eq!(&*hw.default_controller, "hci0");
    assert!(!hw.controller.is_known());
    assert_eq!(hw.controller, BluetoothControllerInfo::unknown());
}

/// Real `hciconfig hci0 version` output from the Pi (TP-Link UB500, RTL8761B).
const HCICONFIG_RTL8761B_OUTPUT: &str = "\
hci0:	Type: Primary  Bus: USB
	BD Address: 20:E1:5D:B2:16:BF  ACL MTU: 1021:6  SCO MTU: 255:12
	HCI Version: 5.4 (0xd)  Revision: 0xddd5
	LMP Version: 5.4 (0xd)  Subversion: 0xc761
	Manufacturer: Realtek Semiconductor Corporation (93)
";

#[test]
fn parse_hciconfig_version_rtl8761b() {
    let info = BluetoothControllerInfo::from_hciconfig_output(HCICONFIG_RTL8761B_OUTPUT)
        .expect("real hciconfig output must parse");
    assert_eq!(info.adapter.as_deref(), Some("hci0"));
    assert_eq!(info.bus_type.as_deref(), Some("USB"));
    assert_eq!(info.bd_address.as_deref(), Some("20:E1:5D:B2:16:BF"));
    assert_eq!(info.hci_version.as_deref(), Some("5.4"));
    assert_eq!(info.hci_revision.as_deref(), Some("0xddd5"));
    assert_eq!(info.lmp_version.as_deref(), Some("5.4"));
    assert_eq!(info.lmp_subversion.as_deref(), Some("0xc761"));
    assert_eq!(
        info.manufacturer.as_deref(),
        Some("Realtek Semiconductor Corporation")
    );
    assert_eq!(info.firmware_version.as_deref(), Some("0xddd5c761"));
    assert!(info.is_known());
}

#[test]
fn parse_hciconfig_version_bcm_uart() {
    let output = "\
hci0:	Type: Primary  Bus: UART
	BD Address: B8:27:EB:12:34:56  ACL MTU: 1021:8  SCO MTU: 255:12
	HCI Version: 4.1 (0x7)  Revision: 0x1000
	LMP Version: 4.1 (0x7)  Subversion: 0x2207
	Manufacturer: Broadcom Corporation (15)
";
    let info =
        BluetoothControllerInfo::from_hciconfig_output(output).expect("BCM output must parse");
    assert_eq!(info.bus_type.as_deref(), Some("UART"));
    assert_eq!(info.bd_address.as_deref(), Some("B8:27:EB:12:34:56"));
    assert_eq!(info.hci_version.as_deref(), Some("4.1"));
    assert_eq!(info.hci_revision.as_deref(), Some("0x1000"));
    assert_eq!(info.lmp_version.as_deref(), Some("4.1"));
    assert_eq!(info.lmp_subversion.as_deref(), Some("0x2207"));
    assert_eq!(info.manufacturer.as_deref(), Some("Broadcom Corporation"));
    assert_eq!(info.firmware_version.as_deref(), Some("0x10002207"));
}

#[test]
fn parse_hciconfig_version_tolerates_missing_fields() {
    let output = "hci0:\tHCI Version: 5.4 (0xd)  Revision: 0xddd5\n";
    let info = BluetoothControllerInfo::from_hciconfig_output(output)
        .expect("version-only output still parses");
    assert_eq!(info.adapter.as_deref(), Some("hci0"));
    assert_eq!(info.bus_type, None);
    assert_eq!(info.bd_address, None);
    assert_eq!(info.hci_version.as_deref(), Some("5.4"));
    assert_eq!(info.hci_revision.as_deref(), Some("0xddd5"));
    assert_eq!(info.firmware_version, None);
}

#[test]
fn parse_hciconfig_version_lmp_only_is_known() {
    let output = "hci0:\tRevision: 0x0555  LMP Version: 5.4 (0xd)  Subversion: 0xc761\n";
    let info = BluetoothControllerInfo::from_hciconfig_output(output)
        .expect("LMP-only output still parses");
    assert_eq!(info.hci_version, None);
    assert_eq!(info.hci_revision.as_deref(), Some("0x0555"));
    assert_eq!(info.lmp_version.as_deref(), Some("5.4"));
    assert_eq!(info.lmp_subversion.as_deref(), Some("0xc761"));
    assert_eq!(info.firmware_version.as_deref(), Some("0x555c761"));
    assert!(info.is_known());
}

#[test]
fn parse_hciconfig_version_bad_hex_keeps_other_fields() {
    let output = "hci0:\tType: Primary  Bus: USB\nBD Address: 20:E1:5D:B2:16:BF\n\t\
            HCI Version: 5.4 (0xd)\n\tRevision: unknown\n\t\
            LMP Version: 5.4 (0xd)  Subversion: broken\n\t\
            Manufacturer: Realtek Semiconductor Corporation (93)\n";
    let info = BluetoothControllerInfo::from_hciconfig_output(output)
        .expect("bad hex values must not abort the whole parse");
    assert_eq!(info.bus_type.as_deref(), Some("USB"));
    assert_eq!(info.bd_address.as_deref(), Some("20:E1:5D:B2:16:BF"));
    assert_eq!(info.hci_version.as_deref(), Some("5.4"));
    assert_eq!(info.lmp_version.as_deref(), Some("5.4"));
    assert_eq!(
        info.manufacturer.as_deref(),
        Some("Realtek Semiconductor Corporation")
    );
    assert_eq!(info.adapter.as_deref(), Some("hci0"));
    assert_eq!(info.firmware_version, None);
}

#[test]
fn parse_hciconfig_version_first_line_only_captures_adapter() {
    let output =
        "hci9:\tType: Primary  Bus: USB\nBD Address: 20:E1:5D:B2:16:BF\n\tLMP Version: 5.4 (0xd)\n";
    let info = BluetoothControllerInfo::from_hciconfig_output(output).expect("output parses");
    assert_eq!(info.adapter.as_deref(), Some("hci9"));
}

#[test]
fn parse_hciconfig_version_empty_is_none() {
    assert!(BluetoothControllerInfo::from_hciconfig_output("").is_none());
    assert!(BluetoothControllerInfo::from_hciconfig_output("garbage text\n").is_none());
}

#[test]
fn bt_error_predicates_and_conversions() {
    let dbus_err = bluetooth_test_helpers::make_dbus_error("connection refused");
    assert!(dbus_err.is_dbus());
    assert!(!dbus_err.is_adapter_not_found());
    assert!(!dbus_err.is_invalid_address());
    assert!(!dbus_err.is_cancelled());
    assert!(!dbus_err.is_mock_fault());
    assert_eq!(
        dbus_err.summary(),
        "D-Bus communication error: connection refused"
    );

    let adapter_err = bluetooth_test_helpers::make_adapter_not_found_error("hci1");
    assert!(adapter_err.is_adapter_not_found());
    assert_eq!(
        adapter_err.summary(),
        "Bluetooth adapter 'hci1' not detected"
    );

    let inv_err = bluetooth_test_helpers::make_invalid_address_error("abc", "too short");
    assert!(inv_err.is_invalid_address());
    assert_eq!(
        inv_err.summary(),
        "invalid Bluetooth address 'abc': too short"
    );

    let cancel_err = bluetooth_test_helpers::make_cancelled_error("aborted");
    assert!(cancel_err.is_cancelled());
    assert_eq!(cancel_err.summary(), "task cancelled: aborted");

    let player_err = bluetooth_test_helpers::make_no_active_player_error("player disconnected");
    assert!(player_err.is_no_active_player());
    assert!(!player_err.is_dbus());
    assert!(!player_err.is_cancelled());
    assert_eq!(
        player_err.summary(),
        "no active Bluetooth media player: player disconnected"
    );

    let mock_err = bluetooth_test_helpers::make_mock_fault_error("simulated error");
    assert!(mock_err.is_mock_fault());
    assert_eq!(mock_err.summary(), "injected mock fault: simulated error");

    let from_join = bluetooth_test_helpers::make_from_cancelled_error("timeout");
    assert!(from_join.is_cancelled());

    let _bt = dbus_err.backtrace();
}

#[test]
fn mock_device_operations_and_fault_injection() {
    let (device, mock) = BluetoothDevice::new_mocked();

    // Initial happy path
    let status = device.status().unwrap();
    assert!(status.is_operational());
    assert_eq!(status.connected_count(), 1);
    assert_eq!(status.connected_devices().len(), 1);
    assert_eq!(status.paired_devices().len(), 1);
    assert!(status.is_streaming());
    assert!(status.active_pcm().is_some());

    // Test clear devices and add device
    mock.clear_devices();
    assert_eq!(device.status().unwrap().connected_count(), 0);

    let new_dev = BtDevice {
        address: BtAddress::new([1, 2, 3, 4, 5, 6]),
        name: "Headphones".into(),
        alias: "WH-1000XM4".into(),
        connected: true,
        paired: true,
        trusted: true,
        blocked: false,
        rssi: Some(-65),
        battery_percentage: Some(60),
        icon: Some("audio-headphones".into()),
        uuids: Box::default(),
    };
    mock.add_device(new_dev.clone());
    mock.add_device(new_dev); // updates existing
    assert_eq!(device.status().unwrap().connected_count(), 1);

    // Test PCMs
    mock.clear_pcms();
    assert!(!device.status().unwrap().is_streaming());
    assert!(device.status().unwrap().active_pcm().is_none());

    let new_pcm = BlueAlsaPcm {
        pcm_path: "/org/bluealsa/hci0/dev_01_02_03_04_05_06/a2dpsnk/source".into(),
        device_address: BtAddress::new([1, 2, 3, 4, 5, 6]),
        resolved_name: "WH-1000XM4".into(),
        transport: "A2DP-sink".into(),
        running: true,
        selected_codec: "LDAC".into(),
        available_codecs: vec!["SBC".into(), "LDAC".into()].into_boxed_slice(),
        sampling_rate: 96_000,
        channels: 2,
        format: "S32_LE".into(),
        volume: Some("80".into()),
        volume_percent: Some(63),
        muted: false,
        delay: None,
    };
    mock.add_pcm(new_pcm);
    assert!(device.status().unwrap().is_streaming());

    // Test adapter controls on mock
    device.set_powered(false).unwrap();
    device.set_powered_for_adapter(Some("hci0"), true).unwrap();
    device
        .set_powered_for_adapter(Some("hci99"), true)
        .unwrap_err();

    device.set_discoverable(true).unwrap();
    device
        .set_discoverable_for_adapter(Some("hci0"), false)
        .unwrap();
    device
        .set_discoverable_for_adapter(Some("hci99"), false)
        .unwrap_err();

    device.set_pairable(false).unwrap();
    device.set_pairable_for_adapter(Some("hci0"), true).unwrap();
    device
        .set_pairable_for_adapter(Some("hci99"), true)
        .unwrap_err();

    // Target adapter missing on mock when adapter is cleared
    mock.set_adapter(None);
    let err_p = device.set_powered(true).unwrap_err();
    assert!(err_p.is_adapter_not_found());
    let err_d = device.set_discoverable(true).unwrap_err();
    assert!(err_d.is_adapter_not_found());
    let err_pair = device.set_pairable(true).unwrap_err();
    assert!(err_pair.is_adapter_not_found());

    // Fault injection
    mock.set_fault(Some("D-Bus daemon stopped".into()));
    let err = device.status().unwrap_err();
    assert!(err.is_mock_fault());
    assert!(err.to_string().contains("D-Bus daemon stopped"));
}

#[tokio::test]
async fn async_methods_read_from_mock_device() {
    let (device, _) = BluetoothDevice::new_mocked();
    let status = device.status_async().await.unwrap();
    assert!(status.is_operational());
}

#[tokio::test]
async fn status_for_adapter_queries() {
    let (device, mock) = BluetoothDevice::new_mocked();
    let status = device.status_for_adapter(Some("hci0")).unwrap();
    assert_eq!(status.adapter.as_ref().unwrap().id.as_ref(), "hci0");

    let err = device.status_for_adapter(Some("hci1")).unwrap_err();
    assert!(err.is_adapter_not_found());
    assert!(err.to_string().contains("hci1"));

    let async_status = device
        .status_for_adapter_async(Some("hci0".into()))
        .await
        .unwrap();
    assert_eq!(async_status.adapter.as_ref().unwrap().id.as_ref(), "hci0");

    let async_err = device
        .status_for_adapter_async(Some("hci1".into()))
        .await
        .unwrap_err();
    assert!(async_err.is_adapter_not_found());

    assert!(device.adapter_info_for(Some("hci0")).unwrap().is_some());
    device.adapter_info_for(Some("hci1")).unwrap_err();
    mock.read_status().unwrap();

    mock.set_adapter(None);
    let none_err = device.status_for_adapter(Some("hci0")).unwrap_err();
    assert!(none_err.is_adapter_not_found());
}

#[test]
fn bluetooth_device_convenience_methods_and_debug() {
    let (device, mock) = BluetoothDevice::new_mocked();
    assert!(format!("{device:?}").contains("BluetoothDevice::Mock"));

    let zerr = BtError::from(zbus::Error::Address("test address".into()));
    assert!(zerr.is_dbus());
    assert_eq!(
        zerr.summary(),
        "D-Bus communication error: address error: test address"
    );

    device.services_status().unwrap();
    device.adapter_info().unwrap();
    device.connected_devices().unwrap();
    device.audio_pcms().unwrap();

    let new_services = BluetoothServices {
        bluetoothd: ServiceState::Active,
        bluealsa: ServiceState::Active,
        bluealsa_aplay: ServiceState::Inactive,
        bt_agent: ServiceState::Inactive,
    };
    mock.set_services(new_services.clone());
    assert_eq!(device.services_status().unwrap(), new_services);
}

#[test]
fn parse_systemd_state_all_combinations() {
    assert_eq!(
        DbusClient::parse_systemd_state("active", "loaded"),
        ServiceState::Active
    );
    assert_eq!(
        DbusClient::parse_systemd_state("inactive", "not-found"),
        ServiceState::NotFound
    );
    assert_eq!(
        DbusClient::parse_systemd_state("not-found", "loaded"),
        ServiceState::NotFound
    );
    assert_eq!(
        DbusClient::parse_systemd_state("failed", "loaded"),
        ServiceState::Failed
    );
    assert_eq!(
        DbusClient::parse_systemd_state("inactive", "loaded"),
        ServiceState::Inactive
    );
    assert_eq!(
        DbusClient::parse_systemd_state("deactivating", "loaded"),
        ServiceState::Inactive
    );
}

fn to_val<T: Into<Value<'static>>>(v: T) -> OwnedValue {
    OwnedValue::try_from(v.into()).unwrap()
}

fn iface(name: &'static str) -> OwnedInterfaceName {
    OwnedInterfaceName::try_from(name).unwrap()
}

#[test]
fn parse_bluez_objects_adapter_and_devices() {
    let mut objects: ManagedObjects = HashMap::new();

    // 1. Adapter object
    let adapter_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let mut adapter_ifaces = HashMap::new();
    let mut adapter_props = HashMap::new();
    adapter_props.insert("Address".to_string(), to_val("DC:A6:32:11:22:33"));
    adapter_props.insert("Name".to_string(), to_val("RaspberryPi"));
    adapter_props.insert("Alias".to_string(), to_val("Speaker-Pi"));
    adapter_props.insert("Powered".to_string(), to_val(true));
    adapter_props.insert("Discoverable".to_string(), to_val(false));
    adapter_props.insert("Pairable".to_string(), to_val(true));
    adapter_props.insert("Discovering".to_string(), to_val(false));
    adapter_props.insert("Class".to_string(), to_val(0x0024_0404_u32));
    adapter_props.insert("Modalias".to_string(), to_val("usb:v1D6Bp0246"));
    adapter_ifaces.insert(iface("org.bluez.Adapter1"), adapter_props);
    objects.insert(adapter_path, adapter_ifaces);

    // 2. Device object with Battery1
    let dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap();
    let mut dev_ifaces = HashMap::new();
    let mut dev_props = HashMap::new();
    dev_props.insert("Address".to_string(), to_val("AA:BB:CC:DD:EE:FF"));
    dev_props.insert("Name".to_string(), to_val("Pixel 8"));
    dev_props.insert("Alias".to_string(), to_val("Deepak's Phone"));
    dev_props.insert("Connected".to_string(), to_val(true));
    dev_props.insert("Paired".to_string(), to_val(true));
    dev_props.insert("Trusted".to_string(), to_val(true));
    dev_props.insert("Blocked".to_string(), to_val(false));
    dev_props.insert("RSSI".to_string(), to_val(-48i16));
    dev_props.insert("Icon".to_string(), to_val("phone"));
    dev_props.insert(
        "UUIDs".to_string(),
        to_val(vec!["0000110b-0000-1000-8000-00805f9b34fb"]),
    );
    dev_ifaces.insert(iface("org.bluez.Device1"), dev_props);

    let mut bat_props = HashMap::new();
    bat_props.insert("Percentage".to_string(), to_val(88u8));
    dev_ifaces.insert(iface("org.bluez.Battery1"), bat_props);
    objects.insert(dev_path, dev_ifaces);

    // 3. Invalid adapter without Address and with invalid Address
    let bad_adapter_path = OwnedObjectPath::try_from("/org/bluez/bad").unwrap();
    let mut bad_adapter_props = HashMap::new();
    bad_adapter_props.insert("Name".to_string(), to_val("BadAdapter"));
    let mut bad_adapter_ifaces = HashMap::new();
    bad_adapter_ifaces.insert(iface("org.bluez.Adapter1"), bad_adapter_props);
    objects.insert(bad_adapter_path, bad_adapter_ifaces);

    let bad_addr_adapter_path = OwnedObjectPath::try_from("/org/bluez/bad2").unwrap();
    let mut bad_addr_props = HashMap::new();
    bad_addr_props.insert("Address".to_string(), to_val("not-an-address"));
    let mut bad_addr_ifaces = HashMap::new();
    bad_addr_ifaces.insert(iface("org.bluez.Adapter1"), bad_addr_props);
    objects.insert(bad_addr_adapter_path, bad_addr_ifaces);

    // 4. Invalid device without Address and with invalid Address
    let bad_dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/bad_dev").unwrap();
    let mut bad_dev_props = HashMap::new();
    bad_dev_props.insert("Name".to_string(), to_val("NoAddressDev"));
    let mut bad_dev_ifaces = HashMap::new();
    bad_dev_ifaces.insert(iface("org.bluez.Device1"), bad_dev_props);
    objects.insert(bad_dev_path, bad_dev_ifaces);

    let bad_addr_dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/bad_dev2").unwrap();
    let mut bad_addr_dev_props = HashMap::new();
    bad_addr_dev_props.insert("Address".to_string(), to_val("bad-mac"));
    let mut bad_addr_dev_ifaces = HashMap::new();
    bad_addr_dev_ifaces.insert(iface("org.bluez.Device1"), bad_addr_dev_props);
    objects.insert(bad_addr_dev_path, bad_addr_dev_ifaces);

    // Parse with default adapter
    let (adapter, devices) = DbusClient::parse_bluez_objects(&objects, None).unwrap();
    let adapter = adapter.unwrap();
    assert_eq!(adapter.id.as_ref(), "hci0");
    assert_eq!(adapter.name.as_ref(), "RaspberryPi");
    assert_eq!(adapter.alias.as_ref(), "Speaker-Pi");
    assert!(adapter.powered);
    assert_eq!(adapter.class, Some(0x0024_0404));

    assert_eq!(devices.len(), 1);
    let dev = &devices[0];
    assert_eq!(dev.display_name(), "Deepak's Phone");
    assert!(dev.connected);
    assert_eq!(dev.battery_percentage, Some(88));
    assert_eq!(dev.rssi, Some(-48));
    assert_eq!(dev.uuids.len(), 1);

    // Parse with target adapter
    let (targeted, _) = DbusClient::parse_bluez_objects(&objects, Some("hci0")).unwrap();
    assert_eq!(targeted.unwrap().id.as_ref(), "hci0");

    // Parse with non-existent target adapter
    let not_found = DbusClient::parse_bluez_objects(&objects, Some("hci99")).unwrap_err();
    assert!(not_found.is_adapter_not_found());
}

#[test]
fn parse_bluez_objects_filters_devices_by_target_adapter() {
    let mut objects: ManagedObjects = HashMap::new();

    let adapter0_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let mut adapter0_ifaces = HashMap::new();
    let mut adapter0_props = HashMap::new();
    adapter0_props.insert("Address".to_string(), to_val("DC:A6:32:00:00:00"));
    adapter0_ifaces.insert(iface("org.bluez.Adapter1"), adapter0_props);
    objects.insert(adapter0_path, adapter0_ifaces);

    let dev0_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_00_00_00_00_00").unwrap();
    let mut dev0_ifaces = HashMap::new();
    let mut dev0_props = HashMap::new();
    dev0_props.insert("Address".to_string(), to_val("AA:00:00:00:00:00"));
    dev0_ifaces.insert(iface("org.bluez.Device1"), dev0_props);
    objects.insert(dev0_path, dev0_ifaces);

    let adapter1_path = OwnedObjectPath::try_from("/org/bluez/hci1").unwrap();
    let mut adapter1_ifaces = HashMap::new();
    let mut adapter1_props = HashMap::new();
    adapter1_props.insert("Address".to_string(), to_val("DC:A6:32:11:11:11"));
    adapter1_ifaces.insert(iface("org.bluez.Adapter1"), adapter1_props);
    objects.insert(adapter1_path, adapter1_ifaces);

    let dev1_path = OwnedObjectPath::try_from("/org/bluez/hci1/dev_BB_11_11_11_11_11").unwrap();
    let mut dev1_ifaces = HashMap::new();
    let mut dev1_props = HashMap::new();
    dev1_props.insert("Address".to_string(), to_val("BB:11:11:11:11:11"));
    dev1_ifaces.insert(iface("org.bluez.Device1"), dev1_props);
    objects.insert(dev1_path, dev1_ifaces);

    let (a0, d0) = DbusClient::parse_bluez_objects(&objects, Some("hci0")).unwrap();
    assert_eq!(a0.unwrap().id.as_ref(), "hci0");
    assert_eq!(d0.len(), 1);
    assert_eq!(d0[0].address, BtAddress::new([0xAA, 0, 0, 0, 0, 0]));

    let (a1, d1) = DbusClient::parse_bluez_objects(&objects, Some("hci1")).unwrap();
    assert_eq!(a1.unwrap().id.as_ref(), "hci1");
    assert_eq!(d1.len(), 1);
    assert_eq!(
        d1[0].address,
        BtAddress::new([0xBB, 0x11, 0x11, 0x11, 0x11, 0x11])
    );
}

#[test]
fn parse_bluealsa_objects_streams() {
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
    pcm_props.insert("Codec".to_string(), to_val("AAC"));
    pcm_props.insert("Sampling".to_string(), to_val(48000u32));
    pcm_props.insert("Channels".to_string(), to_val(2u8));
    pcm_props.insert("Format".to_string(), to_val(0x8210u16));
    pcm_props.insert("Volume".to_string(), to_val(vec![0x7Fu8, 0x7Fu8]));
    pcm_props.insert("Delay".to_string(), to_val(1500u16));

    pcm_ifaces.insert(iface("org.bluealsa.PCM1"), pcm_props);
    objects.insert(pcm_path, pcm_ifaces);

    // Stream 2 with string codecs, Rate, u16 channels, u16 volume, Muted, string delay
    let pcm_path2 =
        OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_AA_BB_CC_DD_EE_FF/a2dpsnk/sink").unwrap();
    let mut pcm_ifaces2 = HashMap::new();
    let mut pcm_props2 = HashMap::new();
    pcm_props2.insert(
        "Device".to_string(),
        to_val("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
    );
    pcm_props2.insert("Transport".to_string(), to_val("A2DP-sink"));
    pcm_props2.insert("Running".to_string(), to_val(false));
    pcm_props2.insert("Codec".to_string(), to_val("SBC"));
    pcm_props2.insert("Rate".to_string(), to_val(44100u32));
    pcm_props2.insert("Format".to_string(), to_val(0x8210u16));
    pcm_props2.insert("Channels".to_string(), to_val(2u16));
    pcm_props2.insert(
        "ChannelMap".to_string(),
        to_val(vec!["L".to_string(), "R".to_string()]),
    );
    pcm_props2.insert("Volume".to_string(), to_val(vec![0xFFu8, 0x7Fu8]));
    pcm_props2.insert("Delay".to_string(), to_val(1200u16));
    pcm_ifaces2.insert(iface("org.bluealsa.PCM1"), pcm_props2);
    objects.insert(pcm_path2, pcm_ifaces2);

    // Stream 3 with [ Unknown ] codecs and no Device (skipped)
    let pcm_path3 = OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_unknown/sco/source").unwrap();
    let mut pcm_ifaces3 = HashMap::new();
    let mut pcm_props3 = HashMap::new();
    pcm_props3.insert("Codecs".to_string(), to_val("[ Unknown ]"));
    pcm_ifaces3.insert(iface("org.bluealsa.PCM1"), pcm_props3);
    objects.insert(pcm_path3, pcm_ifaces3);

    let devices = vec![BtDevice {
        address: BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        name: "Phone".into(),
        alias: "Pixel Phone".into(),
        connected: true,
        paired: true,
        trusted: true,
        blocked: false,
        rssi: None,
        battery_percentage: None,
        icon: None,
        uuids: Box::default(),
    }];

    let pcms = DbusClient::parse_bluealsa_objects(&objects, &devices);
    assert_eq!(pcms.len(), 2);

    // Sorted by pcm_path: .../sink comes before .../source
    let sink_pcm = &pcms[0];
    assert_eq!(sink_pcm.selected_codec.as_ref(), "SBC");
    assert!(sink_pcm.available_codecs.is_empty());
    assert_eq!(sink_pcm.sampling_rate, 44100);
    assert_eq!(sink_pcm.channels, 2);
    assert!(!sink_pcm.running);
    assert!(sink_pcm.muted);
    assert_eq!(sink_pcm.volume.as_deref(), Some("L: 127 R: 127"));
    assert_eq!(sink_pcm.delay.as_deref(), Some("120.0 ms"));
    assert_eq!(sink_pcm.format.as_ref(), "S16_2LE");

    let source_pcm = &pcms[1];
    assert_eq!(source_pcm.resolved_name.as_ref(), "Pixel Phone");
    assert_eq!(source_pcm.selected_codec.as_ref(), "AAC");
    assert!(source_pcm.available_codecs.is_empty());
    assert_eq!(source_pcm.sampling_rate, 48000);
    assert_eq!(source_pcm.channels, 2);
    assert!(source_pcm.running);
    assert!(!source_pcm.muted);
    assert_eq!(source_pcm.volume.as_deref(), Some("CH0: 127 CH1: 127"));
    assert_eq!(source_pcm.delay.as_deref(), Some("150.0 ms"));
}

#[test]
fn decode_pcm_format_identifiers() {
    // Signed/unsigned prefix, width, endian; no byte suffix when bytes*8 == width.
    assert_eq!(decode_pcm_format(0x8210).as_ref(), "S16_2LE");
    assert_eq!(decode_pcm_format(0x0108).as_ref(), "U8LE");
    assert_eq!(decode_pcm_format(0x8420).as_ref(), "S32_4LE");
    // Physical bytes larger than width: underscore byte-count suffix.
    assert_eq!(decode_pcm_format(0x8318).as_ref(), "S24_3LE");
    assert_eq!(decode_pcm_format(0x8418).as_ref(), "S24_4LE");
    // Doc examples: unsigned 16-bit 2 bytes big-endian.
    assert_eq!(decode_pcm_format(0x4210).as_ref(), "U16_2BE");
    // Invalid: zero id, zero width, zero bytes, width larger than bytes*8.
    assert_eq!(decode_pcm_format(0).as_ref(), "Invalid");
    assert_eq!(decode_pcm_format(0x8100).as_ref(), "Invalid");
    assert_eq!(decode_pcm_format(0x8000).as_ref(), "Invalid");
    assert_eq!(decode_pcm_format(0x8020).as_ref(), "Invalid");
}

#[test]
fn format_volume_bytes_channels() {
    // Empty input: no volume, not muted, no percent.
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[], None),
        (Option::<Box<str>>::None, false, None)
    );
    // Stereo, unmuted, generic channel names. (127 + 64) / 2 = 95.5 -> 95.5 / 127 = 75.2% -> 75%
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0x7F, 0x40], None),
        (Some("CH0: 127 CH1: 64".into()), false, Some(75))
    );
    // Any muted high bit → muted true; levels masked to 7 bits. 127 / 127 = 100%
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0xFF, 0x7F], None),
        (Some("CH0: 127 CH1: 127".into()), true, Some(100))
    );
    // ChannelMap names override generic indices. (0 + 32) / 2 = 16 -> 16 / 127 = 12.6% -> 13%
    let names: Box<[Box<str>]> = vec!["L".into(), "R".into()].into_boxed_slice();
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0x80, 0x20], Some(&names)),
        (Some("L: 0 R: 32".into()), true, Some(13))
    );
    // ChannelMap shorter than volume bytes: falls back to CH{i}. (16 + 32) / 2 = 24 -> 24 / 127 = 18.9% -> 19%
    let one_name: Box<[Box<str>]> = vec!["L".into()].into_boxed_slice();
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0x10, 0x20], Some(&one_name)),
        (Some("L: 16 CH1: 32".into()), false, Some(19))
    );
}

#[test]
fn extract_mac_from_pcm_path_validation() {
    assert_eq!(
        DbusClient::extract_mac_from_pcm_path(
            "/org/bluealsa/hci0/dev_11_22_33_44_55_66/a2dpsnk/source"
        )
        .as_deref(),
        Some("11:22:33:44:55:66")
    );
    assert_eq!(
        DbusClient::extract_mac_from_pcm_path(
            "/org/bluealsa/hci0/dev_GG_GG_GG_GG_GG_GG/a2dpsnk/source"
        ),
        None
    );
    assert_eq!(
        DbusClient::extract_mac_from_pcm_path("/org/bluealsa/hci0/dev_11_22_33/a2dpsnk/source"),
        None
    );
    assert_eq!(DbusClient::extract_mac_from_pcm_path("/invalid/path"), None);
}

#[test]
fn unit_not_found_error_matching() {
    use zbus::fdo;
    use zbus::names::OwnedErrorName;

    let dummy_msg = zbus::message::Message::method_call("/org/test", "Test")
        .unwrap()
        .build(&())
        .unwrap();

    let err_no_such = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.systemd1.NoSuchUnit").unwrap(),
        Some("Unit not loaded".into()),
        dummy_msg.clone(),
    );
    assert!(bluetooth_test_helpers::is_no_such_unit_error(&err_no_such));

    let err_unknown_unit = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.systemd1.UnknownUnit").unwrap(),
        None,
        dummy_msg.clone(),
    );
    assert!(bluetooth_test_helpers::is_no_such_unit_error(
        &err_unknown_unit
    ));

    let err_not_found = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.NotFound").unwrap(),
        None,
        dummy_msg.clone(),
    );
    assert!(!bluetooth_test_helpers::is_no_such_unit_error(
        &err_not_found
    ));

    let err_denied = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.AccessDenied").unwrap(),
        None,
        dummy_msg.clone(),
    );
    assert!(!bluetooth_test_helpers::is_no_such_unit_error(&err_denied));

    let err_fdo_unknown = zbus::Error::FDO(Box::new(fdo::Error::UnknownObject("test".to_string())));
    assert!(bluetooth_test_helpers::is_no_such_unit_error(
        &err_fdo_unknown
    ));

    let err_fdo_failed = zbus::Error::FDO(Box::new(fdo::Error::Failed("failed".to_string())));
    assert!(!bluetooth_test_helpers::is_no_such_unit_error(
        &err_fdo_failed
    ));

    let err_string_fallback =
        zbus::Error::Failure("org.freedesktop.systemd1.NoSuchUnit".to_string());
    assert!(bluetooth_test_helpers::is_no_such_unit_error(
        &err_string_fallback
    ));

    let err_io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "refused",
    )));
    assert!(!bluetooth_test_helpers::is_no_such_unit_error(&err_io));
}

#[test]
fn service_absent_error_matching() {
    use zbus::fdo;
    use zbus::names::OwnedErrorName;

    let dummy_msg = zbus::message::Message::method_call("/org/test", "Test")
        .unwrap()
        .build(&())
        .unwrap();

    let err_svc_unknown = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.ServiceUnknown").unwrap(),
        None,
        dummy_msg.clone(),
    );
    assert!(bluetooth_test_helpers::is_service_absent_error(
        &err_svc_unknown
    ));

    let err_no_owner = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.NameHasNoOwner").unwrap(),
        None,
        dummy_msg.clone(),
    );
    assert!(bluetooth_test_helpers::is_service_absent_error(
        &err_no_owner
    ));

    let err_fdo_iface =
        zbus::Error::FDO(Box::new(fdo::Error::UnknownInterface("test".to_string())));
    assert!(bluetooth_test_helpers::is_service_absent_error(
        &err_fdo_iface
    ));

    let err_fdo_obj = zbus::Error::FDO(Box::new(fdo::Error::UnknownObject("test".to_string())));
    assert!(bluetooth_test_helpers::is_service_absent_error(
        &err_fdo_obj
    ));

    assert!(bluetooth_test_helpers::is_service_absent_fdo_error(
        &fdo::Error::ServiceUnknown("not found".into())
    ));
    assert!(bluetooth_test_helpers::is_service_absent_fdo_error(
        &fdo::Error::NameHasNoOwner("no owner".into())
    ));
    assert!(bluetooth_test_helpers::is_service_absent_fdo_error(
        &fdo::Error::UnknownInterface("unknown".into())
    ));
    assert!(bluetooth_test_helpers::is_service_absent_fdo_error(
        &fdo::Error::UnknownObject("unknown".into())
    ));
    assert!(!bluetooth_test_helpers::is_service_absent_fdo_error(
        &fdo::Error::AccessDenied("denied".into())
    ));

    let err_denied = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.AccessDenied").unwrap(),
        None,
        dummy_msg,
    );
    assert!(!bluetooth_test_helpers::is_service_absent_error(
        &err_denied
    ));

    let err_string_fallback =
        zbus::Error::Failure("The name org.bluealsa was not provided (ServiceUnknown)".into());
    assert!(bluetooth_test_helpers::is_service_absent_error(
        &err_string_fallback
    ));

    let err_io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "reset",
    )));
    assert!(!bluetooth_test_helpers::is_service_absent_error(&err_io));
}

#[test]
fn bluetooth_device_control_methods_and_mock_coverage() {
    let (device, mock) = BluetoothDevice::new_mocked();

    // Inherent device power/pairable/discoverable controls
    device.set_powered(true).unwrap();
    device.set_powered(false).unwrap();
    device.set_discoverable(true).unwrap();
    device.set_discoverable(false).unwrap();
    device.set_pairable(true).unwrap();
    device.set_pairable(false).unwrap();

    // Adapter-targeted controls
    device.set_powered_for_adapter(Some("hci0"), true).unwrap();
    device
        .set_discoverable_for_adapter(Some("hci0"), true)
        .unwrap();
    device.set_pairable_for_adapter(Some("hci0"), true).unwrap();

    // Unknown adapter errors
    let err_pow = device
        .set_powered_for_adapter(Some("unknown_adapter"), true)
        .unwrap_err();
    assert!(err_pow.is_adapter_not_found());

    let err_disc = device
        .set_discoverable_for_adapter(Some("unknown_adapter"), true)
        .unwrap_err();
    assert!(err_disc.is_adapter_not_found());

    let err_pair = device
        .set_pairable_for_adapter(Some("unknown_adapter"), true)
        .unwrap_err();
    assert!(err_pair.is_adapter_not_found());

    // Mock controller setters
    mock.set_powered(true);
    mock.set_discoverable(true);
    mock.set_pairable(true);
    mock.clear_devices();
    assert!(device.connected_devices().unwrap().is_empty());
    mock.clear_pcms();
    assert!(device.audio_pcms().unwrap().is_empty());

    // Controller info probe
    let probe_info = speaker_bluetooth::BluetoothControllerInfo::probe("nonexistent_hci99");
    assert!(!probe_info.is_known());

    // FDO error conversion
    let fdo_err = BtError::from(zbus::fdo::Error::UnknownObject("test obj".into()));
    assert!(fdo_err.is_dbus());
    assert!(fdo_err.summary().contains("test obj"));
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_mock_updates() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let bt_clone = bt.clone();

    // Verify initial simulated status
    let initial = bt.status_async().await.unwrap();
    assert!(initial.is_operational());
    assert_eq!(initial.connected_count(), 1);
    assert!(initial.is_streaming());

    // Update state via mock controller
    mock.set_powered(false);
    mock.set_discoverable(true);
    mock.set_pairable(false);

    let dev2 = BtDevice {
        address: BtAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]),
        name: "Tablet".into(),
        alias: "Kitchen Tablet".into(),
        connected: false,
        paired: true,
        trusted: true,
        blocked: false,
        rssi: None,
        battery_percentage: None,
        icon: Some("tablet".into()),
        uuids: Box::default(),
    };
    mock.add_device(dev2);

    let updated = bt_clone.status_async().await.unwrap();
    assert!(!updated.is_operational());
    let adapter = updated.adapter.as_ref().unwrap();
    assert!(!adapter.powered);
    assert!(adapter.discoverable);
    assert!(!adapter.pairable);

    assert_eq!(updated.devices.len(), 2);
    assert_eq!(updated.paired_devices().len(), 2);
    assert_eq!(updated.connected_devices().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_device_clones_read_and_controls() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let b1 = bt.clone();
    let b2 = bt.clone();

    let h1 = tokio::spawn(async move {
        for _ in 0..100 {
            let res = b1.status_async().await;
            res.unwrap();
        }
    });

    let h2 = tokio::spawn(async move {
        for i in 0..100 {
            let powered = i % 2 == 0;
            mock.set_powered(powered);
            b2.set_discoverable(!powered).unwrap();
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();
    let _ = bt.status().unwrap();
}

#[test]
fn playback_state_and_duration_formatting() {
    assert_eq!(format_duration_ms(0), "00:00");
    assert_eq!(format_duration_ms(500), "00:00");
    assert_eq!(format_duration_ms(59_000), "00:59");
    assert_eq!(format_duration_ms(60_000), "01:00");
    assert_eq!(format_duration_ms(65_000), "01:05");
    assert_eq!(format_duration_ms(3_599_000), "59:59");
    assert_eq!(format_duration_ms(3_600_000), "01:00:00");
    assert_eq!(format_duration_ms(3_661_000), "01:01:01");
    assert_eq!(format_duration_ms(36_000_000), "10:00:00");

    let def = BtPlaybackState::default();
    assert_eq!(def.device_address, None);
    assert_eq!(def.device_name.as_ref(), "Bluetooth Audio Device");
    assert_eq!(def.status.as_ref(), "stopped");
    assert!(!def.is_playing());
    assert_eq!(def.title.as_ref(), "Waiting for Bluetooth Connection...");
    assert_eq!(def.artist.as_ref(), "");
    assert_eq!(def.album.as_ref(), "");
    assert_eq!(def.position_ms, 0);
    assert_eq!(def.duration_ms, 0);
    assert_eq!(def.player_path, None);
    assert_eq!(def.obex_port, None);
    assert_eq!(def.image_handle, None);
    assert_eq!(def.cover_art_path, None);
    assert_eq!(def.progress_ratio(), 0.0);
    assert_eq!(def.format_position(), "00:00");
    assert_eq!(def.format_duration(), "00:00");

    let mut state = BtPlaybackState {
        device_address: Some(BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])),
        device_name: "Pixel 9".into(),
        status: "playing".into(),
        title: "Brothers in Arms".into(),
        artist: "Dire Straits".into(),
        album: "Brothers in Arms".into(),
        position_ms: 125_000,
        duration_ms: 415_000,
        player_path: Some("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0".into()),
        obex_port: Some(4097),
        image_handle: Some("img001".into()),
        cover_art_path: Some("/tmp/test.jpg".into()),
    };
    assert!(state.is_playing());
    assert_eq!(state.format_position(), "02:05");
    assert_eq!(state.format_duration(), "06:55");
    assert!((state.progress_ratio() - (125.0 / 415.0)).abs() < 1e-4);

    state.status = "Playing".into();
    assert!(state.is_playing());
    state.status = "PLAYING".into();
    assert!(state.is_playing());
    state.status = "paused".into();
    assert!(!state.is_playing());
    state.status = "stopped".into();
    assert!(!state.is_playing());

    // Boundary progress ratio checks
    state.position_ms = 0;
    assert_eq!(state.progress_ratio(), 0.0);
    state.position_ms = 415_000;
    assert_eq!(state.progress_ratio(), 1.0);
    state.position_ms = 500_000;
    assert_eq!(state.progress_ratio(), 1.0);
    state.duration_ms = 0;
    assert_eq!(state.progress_ratio(), 0.0);
}

#[test]
fn bluetooth_mock_playback_controls() {
    let (device, _) = BluetoothDevice::new_mocked();

    // Verify initial playback state
    let initial = device.status().unwrap();
    let initial_playback = initial.playback.as_ref().unwrap();
    assert!(initial_playback.is_playing());
    assert_eq!(initial_playback.title.as_ref(), "Money for Nothing");
    assert_eq!(initial_playback.artist.as_ref(), "Dire Straits");
    assert_eq!(initial_playback.album.as_ref(), "Brothers in Arms");
    assert_eq!(initial_playback.position_ms, 105_000);
    assert_eq!(initial_playback.duration_ms, 230_000);

    // Pause
    device.player_pause().unwrap();
    let s = device.status().unwrap();
    assert_eq!(s.playback.as_ref().unwrap().status.as_ref(), "paused");
    assert!(!s.playback.as_ref().unwrap().is_playing());

    // Play
    device.player_play().unwrap();
    let s = device.status().unwrap();
    assert_eq!(s.playback.as_ref().unwrap().status.as_ref(), "playing");
    assert!(s.playback.as_ref().unwrap().is_playing());

    // Toggle
    let is_playing = device.player_toggle().unwrap();
    assert!(!is_playing);
    assert_eq!(
        device
            .playback_state()
            .unwrap()
            .as_ref()
            .unwrap()
            .status
            .as_ref(),
        "paused"
    );
    let is_playing = device.player_toggle().unwrap();
    assert!(is_playing);
    assert_eq!(
        device
            .playback_state()
            .unwrap()
            .as_ref()
            .unwrap()
            .status
            .as_ref(),
        "playing"
    );

    // Seek
    device.player_seek(150_000).unwrap();
    assert_eq!(
        device
            .status()
            .unwrap()
            .playback
            .as_ref()
            .unwrap()
            .position_ms,
        150_000
    );

    // Next / Previous
    device.player_next().unwrap();
    device.player_previous().unwrap();
}

#[test]
fn bluetooth_mock_playback_state_mutation_and_faults() {
    let (device, mock) = BluetoothDevice::new_mocked();

    // Mock controller setters
    mock.set_player_status("stopped");
    assert_eq!(
        device
            .status()
            .unwrap()
            .playback
            .as_ref()
            .unwrap()
            .status
            .as_ref(),
        "stopped"
    );

    mock.set_player_track("Tunnel of Love", "Dire Straits", "Making Movies", 490_000);
    let s = device.status().unwrap();
    let p = s.playback.as_ref().unwrap();
    assert_eq!(p.title.as_ref(), "Tunnel of Love");
    assert_eq!(p.artist.as_ref(), "Dire Straits");
    assert_eq!(p.album.as_ref(), "Making Movies");
    assert_eq!(p.duration_ms, 490_000);

    mock.set_player_position(245_000);
    assert_eq!(
        device
            .status()
            .unwrap()
            .playback
            .as_ref()
            .unwrap()
            .position_ms,
        245_000
    );

    // No active player behavior when playback is None
    mock.set_playback(None);
    assert!(device.status().unwrap().playback.is_none());

    let err_play = device.player_play().unwrap_err();
    assert!(err_play.is_no_active_player());
    assert_eq!(
        err_play.summary(),
        "no active Bluetooth media player: no active mock player"
    );
    assert!(device.player_pause().unwrap_err().is_no_active_player());
    assert!(device.player_toggle().unwrap_err().is_no_active_player());
    assert!(device.player_next().unwrap_err().is_no_active_player());
    assert!(device.player_previous().unwrap_err().is_no_active_player());
    assert!(
        device
            .player_seek(10_000)
            .unwrap_err()
            .is_no_active_player()
    );

    // Mock fault injection
    mock.set_player_status("playing");
    mock.set_fault(Some("simulated player error".into()));
    let err_fault = device.player_play().unwrap_err();
    assert!(err_fault.is_mock_fault());
    assert!(device.player_seek(1000).unwrap_err().is_mock_fault());
}

#[test]
fn parse_bluez_all_with_media_player() {
    let mut objects: ManagedObjects = HashMap::new();

    // 1. Adapter object
    let adapter_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let mut adapter_ifaces = HashMap::new();
    let mut adapter_props = HashMap::new();
    adapter_props.insert("Address".to_string(), to_val("DC:A6:32:11:22:33"));
    adapter_props.insert("Name".to_string(), to_val("RaspberryPi"));
    adapter_props.insert("Alias".to_string(), to_val("Speaker-Pi"));
    adapter_props.insert("Powered".to_string(), to_val(true));
    adapter_ifaces.insert(iface("org.bluez.Adapter1"), adapter_props);
    objects.insert(adapter_path, adapter_ifaces);

    // 2. Device object
    let dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap();
    let mut dev_ifaces = HashMap::new();
    let mut dev_props = HashMap::new();
    dev_props.insert("Address".to_string(), to_val("AA:BB:CC:DD:EE:FF"));
    dev_props.insert("Name".to_string(), to_val("Pixel 8 Pro"));
    dev_props.insert("Alias".to_string(), to_val("Deepak's Phone"));
    dev_props.insert("Connected".to_string(), to_val(true));
    dev_props.insert("Paired".to_string(), to_val(true));
    dev_ifaces.insert(iface("org.bluez.Device1"), dev_props);
    objects.insert(dev_path, dev_ifaces);

    // 3. MediaPlayer1 object
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

    let sig_s = zbus::zvariant::Signature::try_from("s").unwrap();
    let sig_v = zbus::zvariant::Signature::try_from("v").unwrap();
    let mut track_dict = zbus::zvariant::Dict::new(&sig_s, &sig_v);
    track_dict
        .append(
            Value::from("Title"),
            Value::Value(Box::new(Value::from("So Far Away"))),
        )
        .unwrap();
    track_dict
        .append(
            Value::from("Artist"),
            Value::Value(Box::new(Value::from("Dire Straits"))),
        )
        .unwrap();
    track_dict
        .append(
            Value::from("Album"),
            Value::Value(Box::new(Value::from("Brothers in Arms"))),
        )
        .unwrap();
    track_dict
        .append(
            Value::from("Duration"),
            Value::Value(Box::new(Value::from(312_000u32))),
        )
        .unwrap();
    track_dict
        .append(
            Value::from("ImgHandle"),
            Value::Value(Box::new(Value::from("12345"))),
        )
        .unwrap();
    player_props.insert("Track".to_string(), to_val(Value::Dict(track_dict)));
    player_props.insert("ObexPort".to_string(), to_val(Value::from(4097u16)));

    player_ifaces.insert(iface("org.bluez.MediaPlayer1"), player_props);
    objects.insert(player_path, player_ifaces);

    // Query BlueZ with parse_bluez_all
    let (adapter, devices, playback) = DbusClient::parse_bluez_all(&objects, Some("hci0")).unwrap();

    let adapter = adapter.expect("adapter should be parsed");
    assert_eq!(adapter.id.as_ref(), "hci0");

    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].display_name(), "Deepak's Phone");

    let pb = playback.expect("playback should be parsed");
    assert!(pb.is_playing());
    assert_eq!(pb.device_name.as_ref(), "Deepak's Phone");
    assert_eq!(
        pb.device_address,
        Some(BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]))
    );
    assert_eq!(pb.title.as_ref(), "So Far Away");
    assert_eq!(pb.artist.as_ref(), "Dire Straits");
    assert_eq!(pb.album.as_ref(), "Brothers in Arms");
    assert_eq!(pb.position_ms, 45_000);
    assert_eq!(pb.duration_ms, 312_000);
    assert_eq!(pb.format_position(), "00:45");
    assert_eq!(pb.format_duration(), "05:12");
    assert_eq!(pb.obex_port, Some(4097));
    assert_eq!(pb.image_handle.as_deref(), Some("12345"));
    assert_eq!(
        pb.player_path.as_deref(),
        Some("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0")
    );
}

#[test]
fn parse_bluez_all_adapter_filtering_and_fallback() {
    let mut objects: ManagedObjects = HashMap::new();

    // Adapter hci0
    let adapter_path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let mut adapter_ifaces = HashMap::new();
    let mut adapter_props = HashMap::new();
    adapter_props.insert("Address".to_string(), to_val("DC:A6:32:11:22:33"));
    adapter_ifaces.insert(iface("org.bluez.Adapter1"), adapter_props);
    objects.insert(adapter_path, adapter_ifaces);

    // Player on hci0
    let player_path =
        OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0").unwrap();
    let mut player_ifaces = HashMap::new();
    let mut player_props = HashMap::new();
    player_props.insert("Status".to_string(), to_val("playing"));
    player_ifaces.insert(iface("org.bluez.MediaPlayer1"), player_props);
    objects.insert(player_path, player_ifaces);

    // Player on hci1 (different adapter)
    let other_player_path =
        OwnedObjectPath::try_from("/org/bluez/hci1/dev_11_22_33_44_55_66/player0").unwrap();
    let mut other_ifaces = HashMap::new();
    let mut other_props = HashMap::new();
    other_props.insert("Status".to_string(), to_val("paused"));
    other_ifaces.insert(iface("org.bluez.MediaPlayer1"), other_props);
    objects.insert(other_player_path, other_ifaces);

    let (_, _, filtered_pb) = DbusClient::parse_bluez_all(&objects, Some("hci0")).unwrap();
    assert_eq!(
        filtered_pb.unwrap().player_path.as_deref(),
        Some("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/player0")
    );

    // Test fallback defaults when Track dictionary is omitted
    let mut minimal_objects = HashMap::new();
    let mut min_dev_ifaces = HashMap::new();
    min_dev_ifaces.insert(iface("org.bluez.Device1"), HashMap::new());
    minimal_objects.insert(
        OwnedObjectPath::try_from("/org/bluez/hci0/dev_minimal").unwrap(),
        min_dev_ifaces,
    );

    let mut min_player_ifaces = HashMap::new();
    let mut min_player_props = HashMap::new();
    min_player_props.insert("Status".to_string(), to_val("paused"));
    min_player_ifaces.insert(iface("org.bluez.MediaPlayer1"), min_player_props);
    minimal_objects.insert(
        OwnedObjectPath::try_from("/org/bluez/hci0/player_min").unwrap(),
        min_player_ifaces,
    );

    let (_, _, min_pb) = DbusClient::parse_bluez_all(&minimal_objects, None).unwrap();
    let min_pb = min_pb.unwrap();
    assert!(!min_pb.is_playing());
    assert_eq!(min_pb.device_name.as_ref(), "Bluetooth Audio Device");
    assert_eq!(min_pb.title.as_ref(), "Waiting for Bluetooth Connection...");
    assert_eq!(min_pb.artist.as_ref(), "");
    assert_eq!(min_pb.album.as_ref(), "");
    assert_eq!(min_pb.position_ms, 0);
    assert_eq!(min_pb.duration_ms, 0);
}

#[test]
fn clean_cover_art_pruning_logic() {
    use speaker_bluetooth::{clean_cover_art_in_dir, clean_old_cover_art};
    use speaker_common::diag::ScopedTempDir;

    let temp = ScopedTempDir::new("cover_art_clean_test");
    let dir = temp.path();

    let mac = "AA_BB_CC_DD_EE_FF";
    let old1 = dir.join(format!("speaker-cover-{mac}-old1.jpg"));
    let old2 = dir.join(format!("speaker-cover-{mac}-old2.jpg"));
    let current = dir.join(format!("speaker-cover-{mac}-current.jpg"));
    let other_mac = dir.join("speaker-cover-11_22_33_44_55_66-handle.jpg");
    let non_cover = dir.join(format!("speaker-cover-{mac}-non_image.txt"));

    std::fs::write(&old1, b"old1").unwrap();
    std::fs::write(&old2, b"old2").unwrap();
    std::fs::write(&current, b"current").unwrap();
    std::fs::write(&other_mac, b"other").unwrap();
    std::fs::write(&non_cover, b"txt").unwrap();

    clean_cover_art_in_dir(dir, mac, "current");

    assert!(!old1.exists());
    assert!(!old2.exists());
    assert!(current.exists());
    assert!(other_mac.exists());
    assert!(non_cover.exists());

    // Also call clean_old_cover_art
    clean_old_cover_art("00_00_00_00_00_00", "none");
}

#[test]
fn cover_art_path_and_cache_validation() {
    use speaker_bluetooth::{
        BtAddress, COVER_ART_CACHE_DIR, cover_art_cached_path, cover_art_cached_path_in,
        cover_art_path_for, cover_art_path_for_in,
    };
    use speaker_common::diag::ScopedTempDir;

    let addr = BtAddress::new([0x30, 0xE0, 0x44, 0x67, 0x38, 0xA2]);

    // Empty handle returns None
    assert!(cover_art_path_for(addr, "").is_none());
    assert!(cover_art_cached_path(addr, "").is_none());
    assert!(cover_art_path_for(addr, "!@#$%^&*()").is_none());

    // Valid handle generates the production path (pure path computation, no I/O).
    let (cache_path, mac, handle) = cover_art_path_for(addr, "1690001").unwrap();
    assert_eq!(mac, "30_E0_44_67_38_A2");
    assert_eq!(handle, "1690001");
    assert_eq!(
        cache_path,
        format!("{COVER_ART_CACHE_DIR}/speaker-cover-30_E0_44_67_38_A2-1690001.jpg")
    );

    // Filesystem-backed cache validation runs in a hermetic temporary directory
    // (the production cache directory requires root privileges to create).
    let dir = ScopedTempDir::new("cover-cache-test");
    let (tmp_path, tmp_mac, tmp_handle) =
        cover_art_path_for_in(dir.path(), addr, "1690001").unwrap();
    assert_eq!(tmp_mac, mac);
    assert_eq!(tmp_handle, handle);
    assert!(std::path::Path::new(&tmp_path).starts_with(dir.path()));

    let mut valid_jpeg = vec![0u8; 120];
    valid_jpeg[0] = 0xFF;
    valid_jpeg[1] = 0xD8;
    valid_jpeg[118] = 0xFF;
    valid_jpeg[119] = 0xD9;
    std::fs::write(&tmp_path, &valid_jpeg).unwrap();
    let res = cover_art_cached_path_in(dir.path(), addr, "1690001");
    assert_eq!(res.as_deref(), Some(tmp_path.as_str()));
    let _ = std::fs::remove_file(&tmp_path);
}

#[test]
fn is_complete_jpeg_thorough_validation() {
    use speaker_bluetooth::is_complete_jpeg;
    use speaker_common::diag::ScopedTempDir;

    let dir = ScopedTempDir::new("complete-jpeg-test");
    let non_existent = dir.path().join("missing.jpg");
    assert!(!is_complete_jpeg(&non_existent));

    // File with < 100 bytes
    let small_file = dir.path().join("small.jpg");
    std::fs::write(&small_file, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
    assert!(!is_complete_jpeg(&small_file));

    // File with exactly 99 bytes even with valid markers
    let mut ninety_nine = vec![0u8; 99];
    ninety_nine[0] = 0xFF;
    ninety_nine[1] = 0xD8;
    ninety_nine[97] = 0xFF;
    ninety_nine[98] = 0xD9;
    let ninety_nine_file = dir.path().join("99.jpg");
    std::fs::write(&ninety_nine_file, &ninety_nine).unwrap();
    assert!(!is_complete_jpeg(&ninety_nine_file));

    // File with 100 bytes but invalid SOI header
    let mut bad_header = vec![0u8; 100];
    bad_header[0] = 0x00;
    bad_header[1] = 0x00;
    bad_header[98] = 0xFF;
    bad_header[99] = 0xD9;
    let bad_header_file = dir.path().join("bad_header.jpg");
    std::fs::write(&bad_header_file, &bad_header).unwrap();
    assert!(!is_complete_jpeg(&bad_header_file));

    // File with 100 bytes with valid SOI header but invalid EOI footer
    let mut bad_footer = vec![0u8; 100];
    bad_footer[0] = 0xFF;
    bad_footer[1] = 0xD8;
    bad_footer[98] = 0x00;
    bad_footer[99] = 0x00;
    let bad_footer_file = dir.path().join("bad_footer.jpg");
    std::fs::write(&bad_footer_file, &bad_footer).unwrap();
    assert!(!is_complete_jpeg(&bad_footer_file));

    // Valid 100-byte JPEG file
    let mut valid = vec![0u8; 100];
    valid[0] = 0xFF;
    valid[1] = 0xD8;
    valid[98] = 0xFF;
    valid[99] = 0xD9;
    let valid_file = dir.path().join("valid.jpg");
    std::fs::write(&valid_file, &valid).unwrap();
    assert!(is_complete_jpeg(&valid_file));
}

#[test]
fn mock_cover_art_resolution_and_control() {
    use speaker_bluetooth::{BluetoothDevice, BtAddress, cover_art_path_for_in};
    use speaker_common::diag::ScopedTempDir;

    let dir = ScopedTempDir::new("mock-cover-cache-test");
    let (device, mock) = BluetoothDevice::new_mocked();
    // Point the mock at a harness-owned cache dir so the test never touches the
    // root-only production cache (`/var/cache/speaker-covers`).
    mock.set_cover_art_cache_dir(dir.path());
    let addr = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    // Initial mock state has no cover art path (both the async query and the
    // blocking diagnostic variant agree on the mock backend).
    let pb = device.playback_state().unwrap().unwrap();
    assert!(pb.cover_art_path.is_none());
    let pb_blocking = device.playback_state_with_cover().unwrap().unwrap();
    assert!(pb_blocking.cover_art_path.is_none());

    // Explicitly set mock cover art path
    mock.set_player_cover_art(Some("/tmp/custom-cover.jpg".into()));
    let pb = device.playback_state().unwrap().unwrap();
    assert_eq!(pb.cover_art_path.as_deref(), Some("/tmp/custom-cover.jpg"));

    // Reset mock cover art to None and test automatic resolution via cache
    mock.set_player_cover_art(None);
    mock.set_player_image_handle(Some("1690002".into()), Some(4097));

    let (cache_path, tmp_mac, tmp_handle) =
        cover_art_path_for_in(dir.path(), addr, "1690002").unwrap();
    assert_eq!(tmp_mac, "AA_BB_CC_DD_EE_FF");
    assert_eq!(tmp_handle, "1690002");
    let mut valid_bytes = vec![0u8; 128];
    valid_bytes[0] = 0xFF;
    valid_bytes[1] = 0xD8;
    valid_bytes[126] = 0xFF;
    valid_bytes[127] = 0xD9;
    std::fs::write(&cache_path, &valid_bytes).unwrap();

    let pb = device.playback_state().unwrap().unwrap();
    assert_eq!(pb.cover_art_path.as_deref(), Some(cache_path.as_str()));

    // Verify status reading also derives cover art
    let status = device.status().unwrap();
    assert_eq!(
        status
            .playback
            .as_ref()
            .and_then(|p| p.cover_art_path.as_deref()),
        Some(cache_path.as_str())
    );

    let _ = std::fs::remove_file(&cache_path);
}

#[test]
fn parse_bluez_all_deterministic_player_selection() {
    fn with_adapter() -> ManagedObjects {
        let mut objects: ManagedObjects = HashMap::new();
        let mut adapter_ifaces = HashMap::new();
        let mut adapter_props = HashMap::new();
        adapter_props.insert("Address".to_string(), to_val("DC:A6:32:11:22:33"));
        adapter_props.insert("Name".to_string(), to_val("RaspberryPi"));
        adapter_props.insert("Alias".to_string(), to_val("Speaker-Pi"));
        adapter_props.insert("Powered".to_string(), to_val(true));
        adapter_ifaces.insert(iface("org.bluez.Adapter1"), adapter_props);
        objects.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0").unwrap(),
            adapter_ifaces,
        );
        objects
    }

    // Two paused players: the lexicographically smallest object path must win
    // regardless of `HashMap` iteration order (insertion is reversed on
    // purpose so the tie-break cannot depend on it).
    let mut objects = with_adapter();
    for path in [
        "/org/bluez/hci0/dev_22/player1",
        "/org/bluez/hci0/dev_11/player9",
    ] {
        let mut ifaces = HashMap::new();
        let mut props = HashMap::new();
        props.insert("Status".to_string(), to_val("paused"));
        ifaces.insert(iface("org.bluez.MediaPlayer1"), props);
        objects.insert(OwnedObjectPath::try_from(path).unwrap(), ifaces);
    }
    let (_, _, playback) = DbusClient::parse_bluez_all(&objects, Some("hci0")).unwrap();
    assert_eq!(
        playback.unwrap().player_path.as_deref(),
        Some("/org/bluez/hci0/dev_11/player9")
    );

    // A playing player beats a paused one even when its path sorts later.
    let mut objects = with_adapter();
    for (path, status) in [
        ("/org/bluez/hci0/dev_11/player1", "paused"),
        ("/org/bluez/hci0/dev_22/player9", "playing"),
    ] {
        let mut ifaces = HashMap::new();
        let mut props = HashMap::new();
        props.insert("Status".to_string(), to_val(status));
        ifaces.insert(iface("org.bluez.MediaPlayer1"), props);
        objects.insert(OwnedObjectPath::try_from(path).unwrap(), ifaces);
    }
    let (_, _, playback) = DbusClient::parse_bluez_all(&objects, Some("hci0")).unwrap();
    assert_eq!(
        playback.unwrap().player_path.as_deref(),
        Some("/org/bluez/hci0/dev_22/player9")
    );
}

#[test]
fn bt_error_preserves_dbus_source() {
    use std::error::Error as _;

    let err = BtError::from(zbus::Error::Address("dbus unavailable".to_string()));
    assert!(err.is_dbus());
    let source = err.source().expect("typed source must be preserved");
    let dbus_source = source
        .downcast_ref::<zbus::Error>()
        .expect("source must remain a typed zbus::Error");
    assert!(matches!(dbus_source, zbus::Error::Address(_)));

    let fdo_err = BtError::from(zbus::fdo::Error::Failed("unknown object".into()));
    assert!(fdo_err.is_dbus());
    assert!(fdo_err.source().is_some());
}

#[test]
fn bip_handle_validation_matches_aosp_contract() {
    // Valid: exactly 7 ASCII digits (what AOSP phones advertise in Track.ImgHandle).
    for (raw, expected) in [
        ("1669286", Some("1669286")),
        ("0000000", Some("0000000")),
        // Invalid: wrong length, non-digits, whitespace, empty.
        ("166928", None),
        ("16692861", None),
        ("h11_00_00_00_00_01", None),
        ("16692a6", None),
        ("1669 86", None),
        ("", None),
    ] {
        assert_eq!(
            validate_bip_handle(raw).as_deref(),
            expected,
            "unexpected validation outcome for {raw:?}"
        );
    }
}

#[test]
fn seek_offset_us_converts_absolute_ms_to_relative_us() {
    // BlueZ Seek is relative; 60 s target with 45 s elapsed = +15 s.
    assert_eq!(seek_offset_us(60_000, 45_000), 15_000_000);
    assert_eq!(seek_offset_us(45_000, 60_000), -15_000_000);
    assert_eq!(seek_offset_us(30_000, 30_000), 0);
}

#[test]
fn dbus_unknown_method_and_bluealsa_helpers_coverage() {
    use speaker_bluetooth::bluetooth_test_helpers::is_unknown_method_error;
    use zbus::names::OwnedErrorName;

    let method_unk = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.UnknownMethod").unwrap(),
        Some("unknown".into()),
        zbus::Message::method_call("/org/bluealsa", "GetCodecs")
            .unwrap()
            .build(&())
            .unwrap(),
    );
    assert!(is_unknown_method_error(&method_unk));

    let method_obj = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.UnknownObject").unwrap(),
        None,
        zbus::Message::method_call("/org/bluealsa", "GetCodecs")
            .unwrap()
            .build(&())
            .unwrap(),
    );
    assert!(is_unknown_method_error(&method_obj));

    let method_other = zbus::Error::MethodError(
        OwnedErrorName::try_from("org.freedesktop.DBus.Error.AccessDenied").unwrap(),
        None,
        zbus::Message::method_call("/org/bluealsa", "GetCodecs")
            .unwrap()
            .build(&())
            .unwrap(),
    );
    assert!(!is_unknown_method_error(&method_other));

    let fdo_unk = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownMethod("m".into())));
    assert!(is_unknown_method_error(&fdo_unk));
    let fdo_obj = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject("o".into())));
    assert!(is_unknown_method_error(&fdo_obj));
    let fdo_other = zbus::Error::FDO(Box::new(zbus::fdo::Error::Failed("f".into())));
    assert!(!is_unknown_method_error(&fdo_other));
    assert!(!is_unknown_method_error(&zbus::Error::Address(
        "addr".into()
    )));

    assert!(
        DbusClient::map_bluealsa_objects_result(Ok(ManagedObjects::new()))
            .unwrap()
            .is_empty()
    );
    assert!(
        DbusClient::map_bluealsa_objects_result(Err(zbus::fdo::Error::ServiceUnknown(
            "org.bluealsa".into()
        )))
        .unwrap()
        .is_empty()
    );
    assert!(
        DbusClient::map_bluealsa_objects_result(Err(zbus::fdo::Error::Failed("boom".into())))
            .unwrap_err()
            .is_dbus()
    );

    assert!(
        DbusClient::map_get_codecs_error("/pcm0", fdo_unk)
            .unwrap()
            .is_empty()
    );
    assert!(
        DbusClient::map_get_codecs_error("/pcm0", fdo_other)
            .unwrap_err()
            .is_dbus()
    );

    let mut codec_map: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();
    codec_map.insert("ldac".into(), HashMap::new());
    codec_map.insert("SBC".into(), HashMap::new());
    codec_map.insert("aptX".into(), HashMap::new());
    let sorted = DbusClient::decode_pcm_codecs_map("/pcm0", Ok(codec_map)).unwrap();
    assert_eq!(
        sorted.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
        vec!["SBC", "aptX", "ldac"]
    );
    assert!(
        DbusClient::decode_pcm_codecs_map("/pcm0", Err(zbus::Error::Address("bad".into())))
            .unwrap_err()
            .is_dbus()
    );
}

#[test]
fn dbus_player_path_toggle_and_seek_helpers_coverage() {
    assert!(
        DbusClient::extract_player_path(None)
            .unwrap_err()
            .is_no_active_player()
    );
    let mut pb = BtPlaybackState::default();
    assert!(
        DbusClient::extract_player_path(Some(pb.clone()))
            .unwrap_err()
            .is_no_active_player()
    );
    assert!(
        DbusClient::resolve_toggle_target(None)
            .unwrap_err()
            .is_no_active_player()
    );
    assert!(
        DbusClient::resolve_toggle_target(Some(pb.clone()))
            .unwrap_err()
            .is_no_active_player()
    );
    assert!(
        DbusClient::resolve_seek_target(None, 5_000)
            .unwrap_err()
            .is_no_active_player()
    );
    assert!(
        DbusClient::resolve_seek_target(Some(pb.clone()), 5_000)
            .unwrap_err()
            .is_no_active_player()
    );

    pb.player_path = Some("/org/bluez/hci0/dev_AA/player0".into());
    pb.status = "playing".into();
    pb.position_ms = 2_000;
    assert_eq!(
        DbusClient::extract_player_path(Some(pb.clone()))
            .unwrap()
            .as_ref(),
        "/org/bluez/hci0/dev_AA/player0"
    );
    let (path, method, next_playing) = DbusClient::resolve_toggle_target(Some(pb.clone())).unwrap();
    assert_eq!(path.as_ref(), "/org/bluez/hci0/dev_AA/player0");
    assert_eq!(method, "Pause");
    assert!(!next_playing);

    pb.status = "paused".into();
    let (_, method, next_playing) = DbusClient::resolve_toggle_target(Some(pb.clone())).unwrap();
    assert_eq!(method, "Play");
    assert!(next_playing);

    let (seek_path, cur_ms, offset_us) = DbusClient::resolve_seek_target(Some(pb), 5_500).unwrap();
    assert_eq!(seek_path.as_ref(), "/org/bluez/hci0/dev_AA/player0");
    assert_eq!(cur_ms, 2_000);
    assert_eq!(offset_us, 3_500_000);
}

#[test]
fn reconciler_types_default_and_debug() {
    use speaker_bluetooth::reconciler::{DeviceWatchState, DisconnectReason, ReconcileAction};

    let state = DeviceWatchState::default();
    assert_eq!(state.divergence_ticks, 0);
    assert_eq!(state.hard_resets_attempted, 0);

    let action = ReconcileAction::CycleConnection {
        device_path: "/org/bluez/hci1/dev_test".into(),
        reason: DisconnectReason::OrphanedBluezTransport,
    };
    assert_eq!(action, action.clone());
}

#[test]
fn reconciler_build_snapshots_comprehensive_matrix() {
    use speaker_bluetooth::reconciler::{UUID_A2DP_SOURCE, build_snapshots};

    let mut bluez_objs = ManagedObjects::new();
    let mut bluealsa_objs = ManagedObjects::new();

    // Dev 1: Healthy A2DP session
    let dev1_path = "/org/bluez/hci0/dev_11_11_11_11_11_11";
    let mut dev1_props = HashMap::new();
    dev1_props.insert("Connected".to_string(), to_val(true));
    dev1_props.insert("Paired".to_string(), to_val(true));
    dev1_props.insert("Bonded".to_string(), to_val(true));
    dev1_props.insert("ServicesResolved".to_string(), to_val(true));
    dev1_props.insert("UUIDs".to_string(), to_val(vec![UUID_A2DP_SOURCE]));
    let mut dev1_ifaces = HashMap::new();
    dev1_ifaces.insert(iface("org.bluez.Device1"), dev1_props);
    bluez_objs.insert(OwnedObjectPath::try_from(dev1_path).unwrap(), dev1_ifaces);

    let mut fd1_ifaces = HashMap::new();
    fd1_ifaces.insert(iface("org.bluez.MediaTransport1"), HashMap::new());
    bluez_objs.insert(
        OwnedObjectPath::try_from(format!("{dev1_path}/fd0")).unwrap(),
        fd1_ifaces,
    );

    let mut pcm1_props = HashMap::new();
    pcm1_props.insert(
        "Device".to_string(),
        to_val(zbus::zvariant::ObjectPath::try_from(dev1_path).unwrap()),
    );
    pcm1_props.insert("Transport".to_string(), to_val("A2DP-sink"));
    let mut pcm1_ifaces = HashMap::new();
    pcm1_ifaces.insert(iface("org.bluealsa.PCM1"), pcm1_props);
    bluealsa_objs.insert(
        OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_11_11_11_11_11_11/a2dpsnk/source")
            .unwrap(),
        pcm1_ifaces,
    );

    // Dev 2: In voice call (HFP active)
    let dev2_path = "/org/bluez/hci0/dev_22_22_22_22_22_22";
    let mut dev2_props = HashMap::new();
    dev2_props.insert("Connected".to_string(), to_val(true));
    dev2_props.insert("Paired".to_string(), to_val(true));
    dev2_props.insert("Bonded".to_string(), to_val(true));
    dev2_props.insert("ServicesResolved".to_string(), to_val(true));
    dev2_props.insert("UUIDs".to_string(), to_val(vec![UUID_A2DP_SOURCE]));
    let mut dev2_ifaces = HashMap::new();
    dev2_ifaces.insert(iface("org.bluez.Device1"), dev2_props);
    bluez_objs.insert(OwnedObjectPath::try_from(dev2_path).unwrap(), dev2_ifaces);

    let mut pcm2_props = HashMap::new();
    pcm2_props.insert("Transport".to_string(), to_val("HFP-AG"));
    let mut pcm2_ifaces = HashMap::new();
    pcm2_ifaces.insert(iface("org.bluealsa.PCM1"), pcm2_props);
    bluealsa_objs.insert(
        OwnedObjectPath::try_from("/org/bluealsa/hci0/dev_22_22_22_22_22_22/hfphf/source").unwrap(),
        pcm2_ifaces,
    );

    let snapshots = build_snapshots(&bluez_objs, &bluealsa_objs);
    assert_eq!(snapshots.len(), 2);

    let s1 = &snapshots[0];
    assert_eq!(s1.path.as_ref(), dev1_path);
    assert!(s1.has_bluez_transport);
    assert!(s1.has_bluealsa_pcm);
    assert!(!s1.has_voice_call_pcm);

    let s2 = &snapshots[1];
    assert_eq!(s2.path.as_ref(), dev2_path);
    assert!(!s2.has_bluez_transport);
    assert!(!s2.has_bluealsa_pcm);
    assert!(s2.has_voice_call_pcm);
}

#[tokio::test]
async fn reconciler_session_cap_and_recovery_flow() {
    use speaker_bluetooth::reconciler::{
        BluezDeviceSnapshot, DisconnectReason, MockReconcilerDbus, ReconcileAction,
        execute_reconcile_actions_with_settle, reconcile_snapshots,
    };
    use std::time::Duration;

    let (mock, _ctrl) = MockReconcilerDbus::new_mocked();
    let dev_path: Box<str> = "/org/bluez/hci1/dev_repeat_fail".into();
    let mut tracked = HashMap::new();

    let divergent_snapshot = BluezDeviceSnapshot {
        path: dev_path.clone(),
        connected: true,
        paired: true,
        bonded: true,
        services_resolved: true,
        supports_a2dp_source: true,
        has_bluez_transport: true,
        has_bluealsa_pcm: false,
        has_voice_call_pcm: false,
    };

    // Cycle 1: Tick 1 (divergence 1) -> Tick 2 (Hard reset 1)
    let acts1 = reconcile_snapshots(&mut tracked, std::slice::from_ref(&divergent_snapshot));
    assert_eq!(acts1, []);
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 1);

    let acts2 = reconcile_snapshots(&mut tracked, std::slice::from_ref(&divergent_snapshot));
    assert_eq!(acts2.len(), 1);
    assert_eq!(
        acts2[0],
        ReconcileAction::CycleConnection {
            device_path: dev_path.clone(),
            reason: DisconnectReason::OrphanedBluezTransport,
        }
    );
    execute_reconcile_actions_with_settle(&mut tracked, &mock, &acts2, Duration::from_millis(1))
        .await;
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 1);

    // Cycle 2: Tick 3 (divergence 1) -> Tick 4 (Hard reset 2)
    let acts3 = reconcile_snapshots(&mut tracked, std::slice::from_ref(&divergent_snapshot));
    assert_eq!(acts3, []);

    let acts4 = reconcile_snapshots(&mut tracked, std::slice::from_ref(&divergent_snapshot));
    assert_eq!(acts4.len(), 1);
    execute_reconcile_actions_with_settle(&mut tracked, &mock, &acts4, Duration::from_millis(1))
        .await;
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 2);

    // After MAX_HARD_RESETS_PER_SESSION (2): Reconciler must back off
    let acts5 = reconcile_snapshots(&mut tracked, std::slice::from_ref(&divergent_snapshot));
    assert_eq!(acts5, []);

    // Once healthy stream establishes, resets count clears back to 0
    let healthy_snapshot = BluezDeviceSnapshot {
        has_bluealsa_pcm: true,
        ..divergent_snapshot
    };
    let acts6 = reconcile_snapshots(&mut tracked, std::slice::from_ref(&healthy_snapshot));
    assert_eq!(acts6, []);
    assert_eq!(tracked.get(&dev_path).unwrap().hard_resets_attempted, 0);
    assert_eq!(tracked.get(&dev_path).unwrap().divergence_ticks, 0);
}

#[tokio::test]
async fn reconciler_di_spawn_and_lifecycle_integration() {
    use speaker_bluetooth::reconciler::{
        MockReconcilerDbus, ReconcileAction, TickOutcome, cycle_device_connection_with_settle,
        execute_reconcile_actions_with_settle, reconciler_tick,
    };
    use speaker_bluetooth::spawn_bt_transport_reconciler_with;
    use std::time::Duration;

    let (mock, ctrl) = MockReconcilerDbus::new_mocked();
    let dev_path: Box<str> = "/org/bluez/hci1/dev_di_test".into();
    let mut tracked = std::collections::HashMap::new();

    // 1. Test execute_reconcile_actions_with_settle and cycle_device_connection_with_settle
    let actions = vec![ReconcileAction::ConnectA2dpProfile {
        device_path: dev_path.clone(),
    }];
    execute_reconcile_actions_with_settle(&mut tracked, &mock, &actions, Duration::from_millis(1))
        .await;
    assert_eq!(ctrl.connect_profile_calls().len(), 1);

    assert!(cycle_device_connection_with_settle(&mock, &dev_path, Duration::from_millis(1)).await);
    assert_eq!(ctrl.disconnect_calls(), vec![dev_path.clone()]);
    assert_eq!(ctrl.connect_calls(), vec![dev_path]);

    // 2. Test tick outcome with empty state
    let outcome = reconciler_tick(&mock, &mut tracked, Duration::from_millis(1)).await;
    assert_eq!(outcome, TickOutcome::ActionsExecuted(0));

    // 3. Test background task spawn and graceful abort
    let handle = spawn_bt_transport_reconciler_with(
        mock.clone(),
        Duration::from_millis(10),
        Duration::from_millis(1),
    );
    tokio::time::sleep(Duration::from_millis(25)).await;
    handle.abort();
    let join_res = handle.await;
    assert!(join_res.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn reconciler_real_dbus_wire_and_bluealsa_fallback_integration() {
    use speaker_bluetooth::reconciler::{ReconcilerDbus, UUID_A2DP_SOURCE, fetch_bluealsa_objects};
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[derive(Clone, Default)]
    struct FakeDeviceCalls {
        profiles: Arc<Mutex<Vec<String>>>,
        disconnected: Arc<Mutex<u32>>,
        connected: Arc<Mutex<u32>>,
    }

    struct FakeDevice1 {
        calls: FakeDeviceCalls,
    }

    #[zbus::interface(name = "org.bluez.Device1")]
    impl FakeDevice1 {
        fn connect_profile(&self, uuid: &str) {
            self.calls.profiles.lock().unwrap().push(uuid.to_string());
        }
        fn disconnect(&self) {
            *self.calls.disconnected.lock().unwrap() += 1;
        }
        fn connect(&self) {
            *self.calls.connected.lock().unwrap() += 1;
        }
    }

    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().expect("daemon stdout"))
        .read_line(&mut address)
        .expect("read bus address");
    let address = address.trim().to_string();
    let _guard = KillOnDrop(daemon);

    let calls = FakeDeviceCalls::default();
    let dev_path = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

    // 1. Export org.bluez with ObjectManager at "/" and FakeDevice1 at dev_path
    let _bluez_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .serve_at("/", zbus::fdo::ObjectManager)
        .unwrap()
        .serve_at(
            dev_path,
            FakeDevice1 {
                calls: calls.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    // 2. Export org.bluealsa with ObjectManager ONLY at "/" (NOT at "/org/bluealsa")
    // to exercise the /org/bluealsa -> UnknownObject -> "/" fallback path!
    let bluealsa_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluealsa")
        .unwrap()
        .serve_at("/", zbus::fdo::ObjectManager)
        .unwrap()
        .build()
        .await
        .unwrap();

    let client_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();

    // Exercise ReconcilerDbus for zbus::Connection:
    // fetch_bluez_objects
    let bluez_objs = client_conn.fetch_bluez_objects().await.unwrap();
    assert!(bluez_objs.contains_key(&OwnedObjectPath::try_from(dev_path).unwrap()));

    // fetch_bluealsa_objects fallback from /org/bluealsa to /
    let bluealsa_objs = client_conn.fetch_bluealsa_objects().await.unwrap();
    assert!(bluealsa_objs.is_empty());
    let direct_objs = fetch_bluealsa_objects(&client_conn).await.unwrap();
    assert!(direct_objs.is_empty());

    // Now also mount ObjectManager at /org/bluealsa and verify primary path succeeds
    bluealsa_conn
        .object_server()
        .at("/org/bluealsa", zbus::fdo::ObjectManager)
        .await
        .unwrap();
    let primary_objs = client_conn.fetch_bluealsa_objects().await.unwrap();
    assert!(primary_objs.is_empty());

    // Exercise connect_profile, disconnect, connect over the wire
    client_conn
        .connect_profile(dev_path, UUID_A2DP_SOURCE)
        .await
        .unwrap();
    client_conn.disconnect(dev_path).await.unwrap();
    client_conn.connect(dev_path).await.unwrap();

    assert_eq!(
        *calls.profiles.lock().unwrap(),
        vec![UUID_A2DP_SOURCE.to_string()]
    );
    assert_eq!(*calls.disconnected.lock().unwrap(), 1);
    assert_eq!(*calls.connected.lock().unwrap(), 1);
}
