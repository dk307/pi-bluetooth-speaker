//! Integration and hardware driver tests for the MA12070P audio amplifier.

use std::error::Error;
use std::time::Duration;

use speaker_core::hardware::{
    AmpDevice, AmpError, AmpErrorFlags, AmpFault, AmpHardwareInfo, AmpI2cAddress, AmpStatus,
    AudioInConfig, AudioInputFormat, ClockRatio, DcuChannelStatus, DcuMonitor, PowerModeProfile,
    amp_test_helpers,
};

#[test]
fn address_validation_and_parsing() {
    assert_eq!(AmpI2cAddress::default_address().as_u8(), 0x20);
    assert_eq!(AmpI2cAddress::default().as_u16(), 0x0020);
    assert_eq!(AmpI2cAddress::default().to_string(), "0x20");

    for addr in 0x20..=0x23 {
        let valid = AmpI2cAddress::new(addr).unwrap();
        assert_eq!(valid.as_u8(), addr);
        assert_eq!(AmpI2cAddress::try_from(addr).unwrap().as_u8(), addr);
    }

    let low_err = AmpI2cAddress::new(0x19).unwrap_err();
    assert!(low_err.is_invalid_address());
    assert!(!low_err.is_i2c());
    assert!(!low_err.is_cancelled());
    assert!(!low_err.is_mock_fault());
    assert!(low_err.to_string().contains("0x19"));
    assert!(low_err.source().is_none());

    let high_err = AmpI2cAddress::new(0x24).unwrap_err();
    assert!(high_err.is_invalid_address());

    assert_eq!(AmpI2cAddress::parse_hex("0x20").unwrap().as_u8(), 0x20);
    assert_eq!(AmpI2cAddress::parse_hex("0X21").unwrap().as_u8(), 0x21);
    assert_eq!(AmpI2cAddress::parse_hex("22").unwrap().as_u8(), 0x22);
    assert_eq!(AmpI2cAddress::parse_hex("  0x23  ").unwrap().as_u8(), 0x23);

    for signed in ["+0x20", "-0x20", "0x+20", "0x-20", "0X+21"] {
        let err = AmpI2cAddress::parse_hex(signed).unwrap_err();
        assert!(err.contains("sign characters are not allowed"));
    }

    assert!(
        AmpI2cAddress::parse_hex("not-hex")
            .unwrap_err()
            .contains("expected hexadecimal digits")
    );
    assert!(
        AmpI2cAddress::parse_hex("")
            .unwrap_err()
            .contains("expected hexadecimal digits")
    );
    assert!(
        AmpI2cAddress::parse_hex("0x00")
            .unwrap_err()
            .contains("invalid MA12070P I2C address")
    );
    assert!(
        AmpI2cAddress::parse_hex("0x25")
            .unwrap_err()
            .contains("invalid MA12070P I2C address")
    );
}

#[test]
fn fault_flags_and_descriptions() {
    for fault in AmpFault::ALL {
        let bit = fault.bit();
        assert_eq!(AmpFault::from_bit(bit), Some(fault));
        assert_eq!(fault.mask(), 1 << bit);
        assert_ne!(fault.name(), "");
        assert_ne!(fault.description(), "");
        assert_eq!(
            fault.to_string(),
            format!("{}: {}", fault.name(), fault.description())
        );
    }
    assert_eq!(AmpFault::from_bit(8), None);

    let none_flags = AmpErrorFlags::NONE;
    assert!(none_flags.is_clear());
    assert_eq!(none_flags.as_u8(), 0);
    assert_eq!(none_flags.to_string(), "0x00 (No faults)");
    assert_eq!(none_flags.faults(), Vec::<AmpFault>::new());

    let all_flags = AmpErrorFlags::from_bits(0xFF);
    assert!(!all_flags.is_clear());
    assert_eq!(all_flags.as_u8(), 0xFF);
    assert_eq!(all_flags.faults().len(), 8);
    assert!(all_flags.flying_cap_overvoltage());
    assert!(all_flags.overcurrent_protect());
    assert!(all_flags.pll_clock_loss());
    assert!(all_flags.undervoltage_pvdd());
    assert!(all_flags.high_temp_warning());
    assert!(all_flags.thermal_shutdown());
    assert!(all_flags.pin_short_circuit());
    assert!(all_flags.dc_offset_protect());
    assert!(all_flags.to_string().contains("FLYING_CAP_OVERVOLTAGE"));

    let single = AmpErrorFlags::from_bits(1 << 2);
    assert!(single.pll_clock_loss());
    assert!(!single.flying_cap_overvoltage());
    assert_eq!(single.faults(), vec![AmpFault::PllClockLoss]);
}

#[test]
fn power_mode_profile_and_decoding() {
    assert_eq!(PowerModeProfile::from_raw(0), PowerModeProfile::Pmp0);
    assert_eq!(PowerModeProfile::from_raw(1), PowerModeProfile::Pmp1);
    assert_eq!(PowerModeProfile::from_raw(2), PowerModeProfile::Pmp2);
    assert_eq!(PowerModeProfile::from_raw(3), PowerModeProfile::Pmp3);
    assert_eq!(PowerModeProfile::from_raw(0xFF), PowerModeProfile::Pmp3);

    assert_eq!(PowerModeProfile::Pmp0.as_u8(), 0);
    assert_eq!(PowerModeProfile::Pmp1.as_u8(), 1);
    assert_eq!(PowerModeProfile::Pmp2.as_u8(), 2);
    assert_eq!(PowerModeProfile::Pmp3.as_u8(), 3);

    assert_eq!(
        PowerModeProfile::Pmp0.to_string(),
        "PMP0 (High Audio Performance / Class D)"
    );
    assert_eq!(
        PowerModeProfile::Pmp1.to_string(),
        "PMP1 (Low Idle Power Optimization)"
    );
    assert_eq!(
        PowerModeProfile::Pmp2.to_string(),
        "PMP2 (Default Balanced / Dynamic Power Mode)"
    );
    assert_eq!(PowerModeProfile::Pmp3.to_string(), "PMP3 (Custom Profile)");
}

#[test]
fn audio_in_and_dcu_decoding() {
    assert_eq!(
        AudioInputFormat::from_raw(0b000),
        AudioInputFormat::StandardI2s
    );
    assert_eq!(
        AudioInputFormat::from_raw(0b001),
        AudioInputFormat::LeftJustified
    );
    assert_eq!(
        AudioInputFormat::from_raw(0b010),
        AudioInputFormat::RightJustified16
    );
    assert_eq!(
        AudioInputFormat::from_raw(0b011),
        AudioInputFormat::RightJustified20
    );
    assert_eq!(
        AudioInputFormat::from_raw(0b100),
        AudioInputFormat::RightJustified24
    );
    assert_eq!(
        AudioInputFormat::from_raw(0b101),
        AudioInputFormat::Custom(5)
    );
    assert_eq!(
        AudioInputFormat::StandardI2s.to_string(),
        "Standard I2S (32-bit slot)"
    );
    assert_eq!(
        AudioInputFormat::LeftJustified.to_string(),
        "Left-Justified (LJ)"
    );
    assert_eq!(
        AudioInputFormat::RightJustified16.to_string(),
        "Right-Justified 16-bit (RJ-16)"
    );
    assert_eq!(
        AudioInputFormat::RightJustified20.to_string(),
        "Right-Justified 20-bit (RJ-20)"
    );
    assert_eq!(
        AudioInputFormat::RightJustified24.to_string(),
        "Right-Justified 24-bit (RJ-24)"
    );
    assert_eq!(AudioInputFormat::Custom(5).to_string(), "Custom Format");

    assert_eq!(ClockRatio::from_raw(0b00 << 3), ClockRatio::AutoDetect);
    assert_eq!(ClockRatio::from_raw(0b01 << 3), ClockRatio::Manual64x);
    assert_eq!(ClockRatio::from_raw(0b10 << 3), ClockRatio::Manual128x);
    assert_eq!(ClockRatio::from_raw(0b11 << 3), ClockRatio::Manual256x);
    assert_eq!(
        ClockRatio::AutoDetect.to_string(),
        "Auto-detect clock ratio"
    );
    assert_eq!(ClockRatio::Manual64x.to_string(), "Manual Ratio (64x fs)");
    assert_eq!(ClockRatio::Manual128x.to_string(), "Manual Ratio (128x fs)");
    assert_eq!(ClockRatio::Manual256x.to_string(), "Manual Ratio (256x fs)");

    let cfg = AudioInConfig::from_raw(0b0000_0000);
    assert_eq!(cfg.format, AudioInputFormat::StandardI2s);
    assert_eq!(cfg.ratio, ClockRatio::AutoDetect);
    assert_eq!(
        cfg.to_string(),
        "Standard I2S (32-bit slot), Auto-detect clock ratio"
    );

    assert_eq!(DcuChannelStatus::from_raw(0x10), DcuChannelStatus::Active);
    assert_eq!(DcuChannelStatus::from_raw(0x00), DcuChannelStatus::Standby);
    assert!(DcuChannelStatus::Active.is_active());
    assert!(!DcuChannelStatus::Standby.is_active());
    assert_eq!(DcuChannelStatus::Active.to_string(), "Active / Regulated");
    assert_eq!(DcuChannelStatus::Standby.to_string(), "Standby / Idle");

    let dcu = DcuMonitor::from_raw(0x10, 0x00);
    assert_eq!(dcu.ch0, DcuChannelStatus::Active);
    assert_eq!(dcu.ch1, DcuChannelStatus::Standby);
    assert_eq!(
        dcu.to_string(),
        "Ch0: Active / Regulated | Ch1: Standby / Idle"
    );
}

#[test]
fn amp_status_display_and_formatting() {
    let status_ok = AmpStatus {
        power_mode: PowerModeProfile::Pmp2,
        audio_in: AudioInConfig {
            format: AudioInputFormat::StandardI2s,
            ratio: ClockRatio::AutoDetect,
        },
        dcu_monitor: DcuMonitor {
            ch0: DcuChannelStatus::Active,
            ch1: DcuChannelStatus::Active,
        },
        live_errors: AmpErrorFlags::NONE,
        latched_errors: AmpErrorFlags::NONE,
    };

    let formatted = status_ok.to_string();
    assert!(formatted.contains("Power Mode Profile    : PMP2"));
    assert!(formatted.contains("Standard I2S (32-bit slot)"));
    assert!(formatted.contains("Ch0: Active / Regulated | Ch1: Active / Regulated"));
    assert!(formatted.contains("[OK] Normal operation (No faults detected)"));
    assert!(formatted.contains("[OK] No errors logged since boot"));

    let status_fault = AmpStatus {
        power_mode: PowerModeProfile::Pmp0,
        audio_in: AudioInConfig {
            format: AudioInputFormat::LeftJustified,
            ratio: ClockRatio::Manual64x,
        },
        dcu_monitor: DcuMonitor {
            ch0: DcuChannelStatus::Standby,
            ch1: DcuChannelStatus::Standby,
        },
        live_errors: AmpErrorFlags::from_bits(1 << 1),
        latched_errors: AmpErrorFlags::from_bits((1 << 2) | (1 << 4)),
    };

    let formatted_fault = status_fault.to_string();
    assert!(formatted_fault.contains("[CRITICAL FAULT] OVERCURRENT_PROTECT"));
    assert!(formatted_fault.contains("[HISTORICAL EVENT] PLL_CLOCK_LOSS"));
    assert!(formatted_fault.contains("[HISTORICAL EVENT] HIGH_TEMP_WARNING"));
}

#[test]
fn hardware_info_and_bus_validation() {
    let hw = AmpHardwareInfo::rpi3_default(AmpI2cAddress::default());
    assert_eq!(hw.bus, 1);
    assert_eq!(hw.address.as_u8(), 0x20);
    assert_eq!(hw.device_path.as_ref(), "/dev/i2c-1");
    assert_eq!(hw.sda_pin.bcm_pin.as_u8(), 2);
    assert_eq!(hw.sda_pin.physical_pin.as_u8(), 3);
    assert_eq!(hw.scl_pin.bcm_pin.as_u8(), 3);
    assert_eq!(hw.scl_pin.physical_pin.as_u8(), 5);
    assert_eq!(hw.vcc_pin.as_u8(), 1);
    assert_eq!(hw.gnd_pin.as_u8(), 9);

    let valid = AmpHardwareInfo::try_rpi3_with_bus_and_address(1, AmpI2cAddress::default());
    let _ = valid.unwrap();

    let invalid_bus = AmpHardwareInfo::try_rpi3_with_bus_and_address(2, AmpI2cAddress::default());
    assert!(invalid_bus.is_err());
    assert!(invalid_bus.unwrap_err().is_unsupported_bus());
}

#[test]
fn amp_error_predicates_and_conversions() {
    let io_err = rppal::i2c::Error::Io(std::io::Error::other("arbitration lost"));
    let i2c_err = AmpError::from(io_err);
    assert!(i2c_err.is_i2c());
    assert!(!i2c_err.is_cancelled());
    assert!(!i2c_err.is_invalid_address());
    assert!(!i2c_err.is_unsupported_bus());
    assert!(!i2c_err.is_mock_fault());
    assert!(i2c_err.summary().contains("I2C bus error"));
    assert!(i2c_err.to_string().contains("I2C bus error"));
    assert!(std::error::Error::source(&i2c_err).is_some());
    let _ = i2c_err.backtrace();

    let canc_err = amp_test_helpers::make_cancelled_error("runtime task aborted");
    assert!(canc_err.is_cancelled());
    assert_eq!(canc_err.summary(), "task cancelled: runtime task aborted");

    let addr_err = amp_test_helpers::make_invalid_address_error(0x50);
    assert!(addr_err.is_invalid_address());
    assert!(addr_err.summary().contains("0x50"));

    let bus_err = amp_test_helpers::make_unsupported_bus_error(5);
    assert!(bus_err.is_unsupported_bus());
    assert!(!bus_err.is_i2c());
    assert_eq!(
        bus_err.summary(),
        "unsupported I2C bus 5: hardware driver models Raspberry Pi 3 BSC1 (i2c-1) only"
    );

    let mock_err = amp_test_helpers::make_mock_fault_error("simulated bus hang");
    assert!(mock_err.is_mock_fault());
    assert_eq!(
        mock_err.summary(),
        "injected mock fault: simulated bus hang"
    );
}

#[test]
fn mock_device_operations_and_fault_injection() {
    let (amp, mock) = AmpDevice::new_mocked();
    assert_eq!(amp.i2c_bus(), 1);
    assert_eq!(amp.i2c_address(), AmpI2cAddress::default());
    let _ = format!("{amp:?}");

    let status = amp.read_status().unwrap();
    assert_eq!(status.power_mode, PowerModeProfile::Pmp2);
    assert!(status.live_errors.is_clear());
    assert!(status.latched_errors.is_clear());

    mock.set_live_errors(AmpErrorFlags::from_bits(1 << 0));
    mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 2));
    mock.set_power_mode(PowerModeProfile::Pmp0);
    mock.set_audio_in(AudioInConfig {
        format: AudioInputFormat::RightJustified24,
        ratio: ClockRatio::Manual128x,
    });
    mock.set_dcu_monitor(DcuMonitor {
        ch0: DcuChannelStatus::Standby,
        ch1: DcuChannelStatus::Active,
    });

    let updated = amp.read_status().unwrap();
    assert_eq!(updated.power_mode, PowerModeProfile::Pmp0);
    assert_eq!(updated.audio_in.format, AudioInputFormat::RightJustified24);
    assert_eq!(updated.audio_in.ratio, ClockRatio::Manual128x);
    assert_eq!(updated.dcu_monitor.ch0, DcuChannelStatus::Standby);
    assert!(updated.live_errors.flying_cap_overvoltage());
    assert!(updated.latched_errors.pll_clock_loss());

    assert_eq!(
        amp.read_live_errors().unwrap(),
        AmpErrorFlags::from_bits(1 << 0)
    );
    assert_eq!(
        amp.read_latched_errors().unwrap(),
        AmpErrorFlags::from_bits(1 << 2)
    );

    amp.clear_latched_errors().unwrap();
    assert!(amp.read_latched_errors().unwrap().is_clear());
    let status_cleared = amp.read_status().unwrap();
    assert!(status_cleared.latched_errors.is_clear());

    // Fault injection
    mock.set_fault(Some("bus communication error".into()));
    assert!(amp.read_status().unwrap_err().is_mock_fault());
    assert!(amp.clear_latched_errors().unwrap_err().is_mock_fault());
    assert!(amp.read_live_errors().unwrap_err().is_mock_fault());
    assert!(amp.read_latched_errors().unwrap_err().is_mock_fault());

    mock.set_fault(None);
    let _ = amp.read_status().unwrap();
}

#[tokio::test]
async fn async_methods_read_from_mock_device() {
    let (amp, mock) = AmpDevice::new_mocked();

    let status = amp.read_status_async().await.unwrap();
    assert_eq!(status.power_mode, PowerModeProfile::Pmp2);

    mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 3));
    assert!(
        amp.read_latched_errors_async()
            .await
            .unwrap()
            .undervoltage_pvdd()
    );

    mock.set_live_errors(AmpErrorFlags::from_bits(1 << 4));
    assert!(
        amp.read_live_errors_async()
            .await
            .unwrap()
            .high_temp_warning()
    );

    amp.clear_latched_errors_async().await.unwrap();
    assert!(amp.read_latched_errors_async().await.unwrap().is_clear());
}

#[tokio::test]
async fn cancelled_blocking_task_maps_to_amp_error_cancelled() {
    let err: AmpError = speaker_core::runtime::abort_and_map("MA12070P test task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("MA12070P test task"));
}

#[test]
fn amp_device_with_bus_validates_rpi3() {
    let err = AmpDevice::with_bus_and_address(99, AmpI2cAddress::default()).unwrap_err();
    assert!(err.is_unsupported_bus());
    assert!(!err.is_i2c());
    assert!(err.summary().contains("unsupported I2C bus 99"));
}

#[test]
fn amp_error_flags_all_predicates() {
    let empty_flags = AmpErrorFlags::NONE;
    assert_eq!(empty_flags.names_joined(), "None");
    let multi_flags = AmpErrorFlags::from_bits((1 << 0) | (1 << 2));
    assert_eq!(
        multi_flags.names_joined(),
        "FLYING_CAP_OVERVOLTAGE, PLL_CLOCK_LOSS"
    );
    for fault in AmpFault::ALL {
        let flags = AmpErrorFlags::from_bits(fault.mask());
        assert_eq!(
            flags.flying_cap_overvoltage(),
            fault == AmpFault::FlyingCapOvervoltage
        );
        assert_eq!(
            flags.overcurrent_protect(),
            fault == AmpFault::OvercurrentProtect
        );
        assert_eq!(flags.pll_clock_loss(), fault == AmpFault::PllClockLoss);
        assert_eq!(
            flags.undervoltage_pvdd(),
            fault == AmpFault::UndervoltagePvdd
        );
        assert_eq!(
            flags.high_temp_warning(),
            fault == AmpFault::HighTempWarning
        );
        assert_eq!(flags.thermal_shutdown(), fault == AmpFault::ThermalShutdown);
        assert_eq!(
            flags.pin_short_circuit(),
            fault == AmpFault::PinShortCircuit
        );
        assert_eq!(
            flags.dc_offset_protect(),
            fault == AmpFault::DcOffsetProtect
        );
    }
}

#[test]
fn amp_device_live_and_latched_error_methods() {
    let (device, _) = AmpDevice::new_mocked();
    assert!(device.read_live_errors().unwrap().is_clear());
    assert!(device.read_latched_errors().unwrap().is_clear());
    device.clear_latched_errors().unwrap();
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
