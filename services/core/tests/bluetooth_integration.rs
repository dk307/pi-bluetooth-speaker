//! Integration and hardware driver tests for the Bluetooth & `BlueALSA` subsystem.

use std::collections::HashMap;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use speaker_core::hardware::bluetooth_test_helpers::{DbusClient, ManagedObjects};
use speaker_core::hardware::{
    BlueAlsaPcm, BluetoothControllerInfo, BluetoothDevice, BluetoothHardwareInfo,
    BluetoothServices, BtAddress, BtDevice, BtError, ServiceState, bluetooth_test_helpers,
    decode_pcm_format,
};

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
    // Empty input: no volume, not muted.
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[], None),
        (Option::<Box<str>>::None, false)
    );
    // Stereo, unmuted, generic channel names.
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0x7F, 0x40], None),
        (Some("CH0: 127 CH1: 64".into()) as Option<Box<str>>, false)
    );
    // Any muted high bit → muted true; levels masked to 7 bits.
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0xFF, 0x7F], None),
        (Some("CH0: 127 CH1: 127".into()) as Option<Box<str>>, true)
    );
    // ChannelMap names override generic indices.
    let names: Box<[Box<str>]> = vec!["L".into(), "R".into()].into_boxed_slice();
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0x80, 0x20], Some(&names)),
        (Some("L: 0 R: 32".into()) as Option<Box<str>>, true)
    );
    // ChannelMap shorter than volume bytes: falls back to CH{i}.
    let one_name: Box<[Box<str>]> = vec!["L".into()].into_boxed_slice();
    assert_eq!(
        bluetooth_test_helpers::format_volume_bytes(&[0x10, 0x20], Some(&one_name)),
        (Some("L: 16 CH1: 32".into()) as Option<Box<str>>, false)
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
    let probe_info = speaker_core::hardware::BluetoothControllerInfo::probe("nonexistent_hci99");
    assert!(!probe_info.is_known());

    // FDO error conversion
    let fdo_err = BtError::from(zbus::fdo::Error::UnknownObject("test obj".into()));
    assert!(fdo_err.is_dbus());
    assert!(fdo_err.summary().contains("test obj"));

    // BluetoothDevice::new invocation
    let _ = BluetoothDevice::new();
}
