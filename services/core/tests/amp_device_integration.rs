//! Integration tests for the MA12070P amplifier hardware driver.

use std::time::Duration;

use speaker_core::hardware::{
    AmpDevice, AmpError, AmpErrorFlags, AmpFault, AmpI2cAddress, AudioInConfig, AudioInputFormat,
    ClockRatio, DcuChannelStatus, DcuMonitor, PowerModeProfile,
};

#[test]
fn address_validation_and_hardware_metadata() {
    let addr = AmpI2cAddress::new(0x20).unwrap();
    let (amp, _) = AmpDevice::new_mocked();
    let hw = amp.hardware_info();

    assert_eq!(addr.as_u8(), 0x20);
    assert_eq!(addr.as_u16(), 0x0020);
    assert_eq!(AmpI2cAddress::parse_hex("0x22").unwrap().as_u8(), 0x22);

    for signed in ["+0x20", "-0x20", "0x+20", "0x-20", "0X+21"] {
        assert!(
            AmpI2cAddress::parse_hex(signed)
                .unwrap_err()
                .contains("sign characters")
        );
    }

    assert_eq!(hw.bus, 1);
    assert_eq!(hw.address.as_u8(), 0x20);
    assert_eq!(hw.sda_pin.bcm_pin.as_u8(), 2);
    assert_eq!(hw.scl_pin.bcm_pin.as_u8(), 3);
    assert_eq!(hw.vcc_pin.as_u8(), 1);
    assert_eq!(hw.gnd_pin.as_u8(), 9);

    let bus_err = AmpDevice::with_bus_and_address(99, addr).unwrap_err();
    assert!(bus_err.is_unsupported_bus());
    assert!(!bus_err.is_i2c());
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_mock_updates() {
    let (amp, mock) = AmpDevice::new_mocked();
    let amp_clone = amp.clone();

    // Verify initial simulated reading
    let initial = amp.read_status_async().await.unwrap();
    assert_eq!(initial.power_mode, PowerModeProfile::Pmp2);
    assert!(initial.live_errors.is_clear());

    // Update in mock controller
    mock.set_power_mode(PowerModeProfile::Pmp0);
    mock.set_live_errors(AmpErrorFlags::from_bits(1 << 1));
    mock.set_audio_in(AudioInConfig {
        format: AudioInputFormat::StandardI2s,
        ratio: ClockRatio::AutoDetect,
    });
    mock.set_dcu_monitor(DcuMonitor {
        ch0: DcuChannelStatus::Active,
        ch1: DcuChannelStatus::Active,
    });

    // Observe update in separate async task
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        amp_clone.read_status_async().await
    });

    let observed = handle.await.unwrap().unwrap();
    assert_eq!(observed.power_mode, PowerModeProfile::Pmp0);
    assert!(observed.live_errors.overcurrent_protect());
    assert_eq!(
        observed.live_errors.faults(),
        vec![AmpFault::OvercurrentProtect]
    );
}

#[test]
fn diagnostics_and_fault_clearing() {
    let (amp, mock) = AmpDevice::new_mocked();

    mock.set_latched_errors(AmpErrorFlags::from_bits((1 << 2) | (1 << 3)));
    let status = amp.read_status().unwrap();
    assert!(status.latched_errors.pll_clock_loss());
    assert!(status.latched_errors.undervoltage_pvdd());
    assert_eq!(
        status.latched_errors.names_joined(),
        "PLL_CLOCK_LOSS, UNDERVOLTAGE_PVDD"
    );

    amp.clear_latched_errors().unwrap();
    let reset_status = amp.read_status().unwrap();
    assert!(reset_status.latched_errors.is_clear());
    assert_eq!(reset_status.latched_errors.names_joined(), "None");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_device_clones_read_and_clear() {
    let (amp, _mock) = AmpDevice::new_mocked();
    let a1 = amp.clone();
    let a2 = amp.clone();

    let h1 = tokio::spawn(async move {
        for _ in 0..100 {
            let res = a1.read_status_async().await;
            let _ = res.unwrap();
        }
    });

    let h2 = tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            a2.clear_latched_errors()
                .expect("clear_latched_errors failed during concurrent cycling");
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();
    let _ = amp.read_status().unwrap();
}

#[tokio::test]
async fn runtime_task_cancellation_mapping() {
    let err: AmpError = speaker_core::runtime::abort_and_map("amplifier integration task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("amplifier integration task"));
}
