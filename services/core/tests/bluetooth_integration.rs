//! Integration tests for the Bluetooth and `BlueALSA` hardware driver.

use speaker_core::hardware::{
    BluetoothDevice, BluetoothHardwareInfo, BtAddress, BtDevice, BtError,
};

#[test]
fn address_validation_and_hardware_metadata() {
    let addr = BtAddress::parse("DC:A6:32:00:11:22").unwrap();
    assert_eq!(addr.to_string(), "DC:A6:32:00:11:22");
    assert_eq!(addr.as_bytes(), &[0xDC, 0xA6, 0x32, 0x00, 0x11, 0x22]);

    for invalid in [
        "+DC:A6:32:00:11:22",
        "-DC:A6:32:00:11:22",
        "DC:+A6:32:00:11:22",
        "DC:A6:32:00:11",
        "DC:A6:32:00:11:22:33",
        "DC:ZZ:32:00:11:22",
    ] {
        BtAddress::parse_str(invalid).unwrap_err();
    }

    let hw = BluetoothHardwareInfo::rpi3();
    assert_eq!(&*hw.chip, "Broadcom BCM43438 (Bluetooth 4.1 + BLE)");
    assert_eq!(&*hw.bus, "UART (PL011 / /dev/ttyAMA0)");
    assert_eq!(hw.baud_rate, 921_600);
    assert_eq!(&*hw.default_controller, "hci0");
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

#[tokio::test]
async fn runtime_task_cancellation_mapping() {
    let err: BtError = speaker_core::runtime::abort_and_map("bluetooth integration task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("bluetooth integration task"));
}

#[test]
fn bluetooth_status_and_predicates_integration() {
    let (bt, mock) = BluetoothDevice::new_mocked();
    let status = bt.status().unwrap();
    assert!(status.is_operational());
    assert_eq!(status.connected_count(), 1);
    assert!(status.is_streaming());
    assert!(status.active_pcm().is_some());
    assert_eq!(status.connected_devices().len(), 1);
    assert_eq!(status.paired_devices().len(), 1);

    mock.set_powered(false);
    let off_status = bt.status().unwrap();
    assert!(!off_status.is_operational());

    mock.clear_devices();
    assert_eq!(bt.status().unwrap().connected_count(), 0);

    mock.clear_pcms();
    assert!(!bt.status().unwrap().is_streaming());

    mock.set_fault(Some("dbus connection dropped".into()));
    let fault_err = bt.status().unwrap_err();
    assert!(fault_err.is_mock_fault());
    assert!(fault_err.to_string().contains("dbus connection dropped"));
}
