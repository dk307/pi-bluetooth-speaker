//! Integration tests for the Bluetooth and `BlueALSA` hardware driver.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use speaker_core::hardware::{
    BluetoothDevice, BluetoothHardwareInfo, BtAddress, BtDevice, BtError, CommandOutput,
    CommandRunner, ServiceState,
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

struct MockRunner {
    responses: HashMap<(String, String), CommandOutput>,
}

impl CommandRunner for MockRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let key = (program.to_string(), args.join(" "));
        if let Some(out) = self.responses.get(&key) {
            Ok(out.clone())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("command not mocked: {program} {args:?}"),
            ))
        }
    }
}

#[test]
fn system_inspector_with_custom_runner() {
    let mut responses = HashMap::new();
    responses.insert(
        ("systemctl".into(), "is-active bluetooth".into()),
        CommandOutput {
            stdout: "active".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("systemctl".into(), "is-active bluealsa".into()),
        CommandOutput {
            stdout: "active".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("systemctl".into(), "is-active bluealsa-aplay".into()),
        CommandOutput {
            stdout: "active".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("systemctl".into(), "is-active bt-agent".into()),
        CommandOutput {
            stdout: "active".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("bluetoothctl".into(), "show".into()),
        CommandOutput {
            stdout: "Controller AA:BB:CC:DD:EE:00 (public)\nName: Speaker-Pi\nPowered: yes\nPairable: yes\nDiscoverable: yes\n".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("bluetoothctl".into(), "devices Connected".into()),
        CommandOutput {
            stdout: "Device 11:22:33:44:55:66 Phone\n".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("bluetoothctl".into(), "devices".into()),
        CommandOutput {
            stdout: "Device 11:22:33:44:55:66 Phone\n".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("bluetoothctl".into(), "info 11:22:33:44:55:66".into()),
        CommandOutput {
            stdout: "Device 11:22:33:44:55:66\nName: Phone\nConnected: yes\nPaired: yes\nTrusted: yes\nRSSI: -55 dBm\nBattery Percentage: 92%\nUUID: Audio Sink (0000110b-0000-1000-8000-00805f9b34fb)\n".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("bluealsa-cli".into(), "list-pcms".into()),
        CommandOutput {
            stdout: "/org/bluealsa/hci0/dev_11_22_33_44_55_66/a2dpsnk/source\n".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );
    responses.insert(
        ("bluealsa-cli".into(), "info /org/bluealsa/hci0/dev_11_22_33_44_55_66/a2dpsnk/source".into()),
        CommandOutput {
            stdout: "Transport: A2DP-sink\nRunning: true\nSelected codec: LDAC\nSampling: 96000\nChannels: 2\nFormat: S32_LE\nVolume: L: 90 R: 90\nDelay: 1500\n".into(),
            stderr: String::new(),
            exit_code: 0,
        },
    );

    let runner = MockRunner { responses };
    let bt = BluetoothDevice::with_runner(Arc::new(runner));
    let status = bt.status().unwrap();

    assert_eq!(status.services.bluetoothd, ServiceState::Active);
    assert!(status.services.all_active());
    let adapter = status.adapter.unwrap();
    assert_eq!(adapter.name.as_ref(), "Speaker-Pi");
    assert!(adapter.powered);

    assert_eq!(status.devices.len(), 1);
    let dev = &status.devices[0];
    assert_eq!(dev.name.as_ref(), "Phone");
    assert_eq!(dev.rssi, Some(-55));
    assert_eq!(dev.battery_percentage, Some(92));

    assert_eq!(status.pcms.len(), 1);
    let pcm = &status.pcms[0];
    assert_eq!(pcm.selected_codec.as_ref(), "LDAC");
    assert_eq!(pcm.sampling_rate, 96_000);
    assert_eq!(pcm.format.as_ref(), "S32_LE");
    assert!(pcm.running);
}
