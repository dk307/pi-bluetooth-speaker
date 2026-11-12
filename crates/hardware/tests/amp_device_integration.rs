//! Integration and hardware driver tests for the MA12070P audio amplifier.

use std::error::Error;
use std::time::Duration;

use speaker_hardware::{
    AmpDevice, AmpError, AmpErrorFlags, AmpFault, AmpHardwareInfo, AmpI2cAddress, AmpSettings,
    AmpStatus, DcuChannelStatus, DcuMonitor, LimiterSlew, Pm2Config, PowerModeMon,
    PowerModeProfile, amp_test_helpers,
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
    // Table-driven: register 0x1D bits 2:0 -> preset (0-5).
    let profile_table = [
        (0, PowerModeProfile::Pmp0),
        (1, PowerModeProfile::Pmp1),
        (2, PowerModeProfile::Pmp2),
        (3, PowerModeProfile::Pmp3),
        (4, PowerModeProfile::Pmp4),
        (5, PowerModeProfile::Pmp5Custom),
        (0xFF, PowerModeProfile::Pmp5Custom),
    ];
    for (raw, expected) in profile_table {
        assert_eq!(PowerModeProfile::from_raw(raw), expected, "raw {raw:#x}");
        assert_eq!(expected.as_u8(), expected.as_u8());
    }
    for (idx, profile) in [
        PowerModeProfile::Pmp0,
        PowerModeProfile::Pmp1,
        PowerModeProfile::Pmp2,
        PowerModeProfile::Pmp3,
        PowerModeProfile::Pmp4,
        PowerModeProfile::Pmp5Custom,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(profile.as_u8(), idx as u8);
    }

    // Read-only live monitor register 0x60 bits 1:0 -> PM0-PM3.
    let mon_table = [
        (0, PowerModeMon::Pm0),
        (1, PowerModeMon::Pm1),
        (2, PowerModeMon::Pm2),
        (3, PowerModeMon::Pm3),
        (0xFF, PowerModeMon::Pm3),
    ];
    for (raw, expected) in mon_table {
        assert_eq!(PowerModeMon::from_raw(raw), expected, "raw {raw:#x}");
    }
}

#[test]
fn pm2_config_and_dcu_decoding() {
    // Table-driven: register 0x1B -> PM2 modulation configuration.
    let cfg_table = [
        (0b0000_0000, false, 0, 0),
        (0b0100_0000, true, 0, 0),
        (0b0001_1000, false, 3, 0),
        (0b0100_1111, true, 1, 7),
        (0xFF, true, 7, 7),
    ];
    for (raw, usespread, dtsteps, baltype) in cfg_table {
        let cfg = Pm2Config::from_raw(raw);
        assert_eq!(cfg.usespread, usespread, "raw {raw:#x} usespread");
        assert_eq!(cfg.dtsteps, dtsteps, "raw {raw:#x} dtsteps");
        assert_eq!(cfg.baltype, baltype, "raw {raw:#x} baltype");
    }

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
        pm_mon: PowerModeMon::Pm2,
        pm2_config: Pm2Config {
            usespread: false,
            dtsteps: 3,
            baltype: 1,
        },
        dcu_monitor: DcuMonitor {
            ch0: DcuChannelStatus::Active,
            ch1: DcuChannelStatus::Active,
        },
        live_errors: AmpErrorFlags::NONE,
        latched_errors: AmpErrorFlags::NONE,
    };

    let formatted = status_ok.to_string();
    assert!(formatted.contains("PM Monitor (live)     : PM2"));
    assert!(formatted.contains("PM2 Modulation Config : spread: off | dt: 3 steps | baltype: 1"));
    assert!(formatted.contains("Ch0: Active / Regulated | Ch1: Active / Regulated"));
    assert!(formatted.contains("[OK] Normal operation (No faults detected)"));
    assert!(formatted.contains("[OK] No errors logged since boot"));

    let status_fault = AmpStatus {
        pm_mon: PowerModeMon::Pm0,
        pm2_config: Pm2Config {
            usespread: true,
            dtsteps: 1,
            baltype: 2,
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
    assert_eq!(status.pm_mon, PowerModeMon::Pm2);
    assert!(status.live_errors.is_clear());
    assert!(status.latched_errors.is_clear());

    mock.set_live_errors(AmpErrorFlags::from_bits(1 << 0));
    mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 2));
    mock.set_pm_mon(PowerModeMon::Pm0);
    mock.set_pm2_config(Pm2Config {
        usespread: true,
        dtsteps: 5,
        baltype: 2,
    });
    mock.set_dcu_monitor(DcuMonitor {
        ch0: DcuChannelStatus::Standby,
        ch1: DcuChannelStatus::Active,
    });

    let updated = amp.read_status().unwrap();
    assert_eq!(updated.pm_mon, PowerModeMon::Pm0);
    assert_eq!(
        updated.pm2_config,
        Pm2Config {
            usespread: true,
            dtsteps: 5,
            baltype: 2,
        }
    );
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
    assert_eq!(status.pm_mon, PowerModeMon::Pm2);

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
    let err: AmpError = speaker_common::runtime::abort_and_map("MA12070P test task").await;
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

    let decoded = AmpStatus::from_raw_registers([0x02, 0x08, 0x02, 0x05, 0x05, 0x08]);
    assert_eq!(decoded.pm_mon, PowerModeMon::Pm2);
    assert!(decoded.live_errors.overcurrent_protect());
    assert!(decoded.latched_errors.undervoltage_pvdd());
    assert_eq!(
        decoded.pm2_config,
        Pm2Config {
            usespread: false,
            dtsteps: 1,
            baltype: 0
        }
    );
    assert_eq!(decoded.dcu_monitor.ch0, DcuChannelStatus::Standby);

    assert_eq!(AmpStatus::eh_clear_pulse_sequence(0xFF), [0xFB, 0xFF, 0xFB]);
    assert_eq!(AmpStatus::eh_clear_pulse_sequence(0x00), [0x00, 0x04, 0x00]);
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_mock_updates() {
    let (amp, mock) = AmpDevice::new_mocked();
    let amp_clone = amp.clone();

    // Verify initial simulated reading
    let initial = amp.read_status_async().await.unwrap();
    assert_eq!(initial.pm_mon, PowerModeMon::Pm2);
    assert!(initial.live_errors.is_clear());

    // Update in mock controller
    mock.set_pm_mon(PowerModeMon::Pm0);
    mock.set_live_errors(AmpErrorFlags::from_bits(1 << 1));
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
    assert_eq!(observed.pm_mon, PowerModeMon::Pm0);
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

// === Phase 1.5 Write-path tests ===

#[test]
fn mock_write_master_volume_and_read() {
    let (amp, mock) = AmpDevice::new_mocked();
    // Default mock volume is 116 (50%)
    assert_eq!(mock.mock_master_volume_raw(), 116);

    // Write new volume
    amp.write_master_volume(144).unwrap(); // 0 dB
    assert_eq!(mock.mock_master_volume_raw(), 144);

    amp.write_master_volume(0).unwrap(); // -144 dB (mute)
    assert_eq!(mock.mock_master_volume_raw(), 0);

    amp.write_master_volume(72).unwrap(); // mid-range
    assert_eq!(mock.mock_master_volume_raw(), 72);
}

#[test]
fn mock_write_serial_mute_and_read() {
    let (amp, mock) = AmpDevice::new_mocked();
    assert!(!mock.mock_serial_muted());

    amp.write_serial_mute(true).unwrap();
    assert!(mock.mock_serial_muted());

    amp.write_serial_mute(false).unwrap();
    assert!(!mock.mock_serial_muted());
}

#[test]
fn mock_write_power_mode_ctrl_and_read() {
    let (amp, mock) = AmpDevice::new_mocked();
    // Default: manualPM=1 (bit6), PM_man=2 (bits5:4) -> decoded PM_man = 2
    assert_eq!(mock.mock_pm_ctrl(), 2);

    amp.write_power_mode_ctrl(0).unwrap(); // PMP0
    assert_eq!(mock.mock_pm_ctrl(), 0);

    amp.write_power_mode_ctrl(3).unwrap(); // PMP3
    assert_eq!(mock.mock_pm_ctrl(), 3);
}

#[test]
fn mock_write_power_mode_profile_and_read() {
    let (amp, mock) = AmpDevice::new_mocked();
    // Default PMP2
    assert_eq!(mock.mock_pm_profile(), 2);

    amp.write_power_mode_profile(0).unwrap(); // PMP0
    assert_eq!(mock.mock_pm_profile(), 0);

    amp.write_power_mode_profile(4).unwrap(); // PMP4
    assert_eq!(mock.mock_pm_profile(), 4);
}

#[test]
fn mock_write_i2s_format_and_read() {
    let (amp, mock) = AmpDevice::new_mocked();
    // Default: Standard I2S = 0
    assert_eq!(mock.mock_i2s_format(), 0);

    amp.write_i2s_format(1).unwrap(); // Left-Justified
    assert_eq!(mock.mock_i2s_format(), 1);

    amp.write_i2s_format(4).unwrap(); // Right-Justified 24-bit
    assert_eq!(mock.mock_i2s_format(), 4);
}

#[test]
fn mock_write_audio_in_mode_and_read() {
    let (amp, mock) = AmpDevice::new_mocked();
    // Default: 20dB gain = 0
    assert_eq!(mock.mock_audio_in_mode(), 0);

    amp.write_audio_in_mode(1).unwrap(); // 26dB
    assert_eq!(mock.mock_audio_in_mode(), 1);

    amp.write_audio_in_mode(3).unwrap(); // 36dB
    assert_eq!(mock.mock_audio_in_mode(), 3);
}

#[tokio::test]
async fn async_write_methods_work_on_mock() {
    let (amp, mock) = AmpDevice::new_mocked();

    // Test async write_master_volume
    amp.write_master_volume_async(100).await.unwrap();
    assert_eq!(mock.mock_master_volume_raw(), 100);

    // Test async write_serial_mute
    amp.write_serial_mute_async(true).await.unwrap();
    assert!(mock.mock_serial_muted());

    // Test async write_power_mode_ctrl
    amp.write_power_mode_ctrl_async(1).await.unwrap();
    assert_eq!(mock.mock_pm_ctrl(), 1);

    // Test async write_power_mode_profile
    amp.write_power_mode_profile_async(3).await.unwrap();
    assert_eq!(mock.mock_pm_profile(), 3);

    // Test async write_i2s_format
    amp.write_i2s_format_async(4).await.unwrap();
    assert_eq!(mock.mock_i2s_format(), 4);

    // Test async clear_latched_errors (aliased as clear_error_handler_async was removed)
    amp.clear_latched_errors_async().await.unwrap();
    // Just verify it doesn't error
}

#[test]
fn mock_fault_injection_on_write_paths() {
    let (amp, mock) = AmpDevice::new_mocked();

    // Inject fault
    mock.set_fault(Some("injected write fault".into()));

    // All write methods should return the fault
    assert!(amp.write_master_volume(100).unwrap_err().is_mock_fault());
    assert!(amp.write_serial_mute(true).unwrap_err().is_mock_fault());
    assert!(amp.write_power_mode_ctrl(0).unwrap_err().is_mock_fault());
    assert!(amp.write_power_mode_profile(0).unwrap_err().is_mock_fault());
    assert!(amp.write_i2s_format(0).unwrap_err().is_mock_fault());
    assert!(amp.write_audio_in_mode(0).unwrap_err().is_mock_fault());

    // Clear fault
    mock.set_fault(None);
    amp.write_master_volume(100).unwrap();
}

// === Embedded-hal mock bus tests (Phase 1.5) ===
// These tests exercise the HardwareDevice implementation directly using
// an embedded-hal mock bus, covering register-level logic that the mock backend doesn't.

mod eh_mock_bus {
    use embedded_hal::blocking::i2c::{Read, Write, WriteRead};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// Simple in-memory I2C register map for testing `HardwareDevice` logic.
    /// Uses shared state (Rc<`RefCell`<...>>) so clones observe the same register values.
    #[derive(Debug, Default, Clone)]
    pub struct MockBus {
        registers: Rc<RefCell<HashMap<u8, u8>>>,
        /// Registers that are stuck at a specific value (simulates hardware
        /// that accepts the transaction but ignores writes — read-back returns
        /// the stuck value, exercising write verification paths).
        stuck_registers: Rc<RefCell<HashMap<u8, u8>>>,
    }

    impl MockBus {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_register(self, reg: u8, val: u8) -> Self {
            self.registers.borrow_mut().insert(reg, val);
            self
        }

        /// Make a register stuck at a specific value (simulates hardware that ignores writes)
        pub fn with_stuck_register(self, reg: u8, val: u8) -> Self {
            self.stuck_registers.borrow_mut().insert(reg, val);
            self.registers.borrow_mut().insert(reg, val);
            self
        }

        /// Read the effective register value (stuck value wins over written value).
        pub fn get_register(&self, reg: u8) -> Option<u8> {
            if let Some(val) = self.stuck_registers.borrow().get(&reg) {
                return Some(*val);
            }
            self.registers.borrow().get(&reg).copied()
        }

        fn write_register(&self, reg: u8, val: u8) {
            // Stuck registers silently ignore writes.
            if !self.stuck_registers.borrow().contains_key(&reg) {
                self.registers.borrow_mut().insert(reg, val);
            }
        }
    }

    impl Write for MockBus {
        type Error = &'static str;

        fn write(&mut self, _address: u8, bytes: &[u8]) -> Result<(), Self::Error> {
            // Register write transaction: [reg, val]. Empty writes are probes.
            if bytes.len() >= 2 {
                self.write_register(bytes[0], bytes[1]);
            }
            Ok(())
        }
    }

    // Standard embedded-hal 0.2.7 Read trait: read(address, buffer)
    impl Read for MockBus {
        type Error = &'static str;

        fn read(&mut self, _address: u8, _buffer: &mut [u8]) -> Result<(), Self::Error> {
            // Not used by `HardwareDevice`; register reads go through `write_read`.
            Ok(())
        }
    }

    // Standard embedded-hal 0.2.7 WriteRead trait (write-read to specific address)
    impl WriteRead for MockBus {
        type Error = &'static str;

        fn write_read(
            &mut self,
            _address: u8,
            bytes: &[u8],
            buffer: &mut [u8],
        ) -> Result<(), Self::Error> {
            // Register read transaction: write the register pointer, read back one byte.
            if bytes.len() == 1 && buffer.len() == 1 {
                let reg = bytes[0];
                buffer[0] = self.get_register(reg).unwrap_or(0);
            }
            Ok(())
        }
    }
}

mod hardware_device_tests {
    use super::eh_mock_bus::MockBus;
    use speaker_hardware::amp::{
        AmpI2cAddress, LimiterSlew, REG_AUDIO_IN_MODE, REG_AUDIO_PROC, REG_I2S_FORMAT, REG_PM_CTRL,
        REG_PM_PROFILE, REG_THR_DB_CH0, REG_VOL_DB_MASTER, REG_VOL_LSB_MASTER,
    };
    use speaker_hardware::amp_test_helpers::HardwareDevice;

    fn addr() -> AmpI2cAddress {
        AmpI2cAddress::default()
    }

    #[test]
    fn hardware_device_write_master_volume_encodes_correctly() {
        let bus = MockBus::new();
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        // Write raw = 100 (−100 dB) -> VLP reg = 168 − 100 = 68 (0x44), frac = 0
        dev.write_master_volume(100).unwrap();

        assert_eq!(bus.get_register(REG_VOL_DB_MASTER), Some(68));
        assert_eq!(bus.get_register(REG_VOL_LSB_MASTER), Some(0));
    }

    #[test]
    fn hardware_device_write_master_volume_rejects_out_of_range() {
        let bus = MockBus::new();
        let mut dev = HardwareDevice::new(bus, addr());

        let err = dev.write_master_volume(145).unwrap_err();
        assert!(err.is_invalid_argument());
        assert!(err.summary().contains("master_volume_raw"));
    }

    #[test]
    fn hardware_device_write_serial_mute_rmw_and_verify() {
        let bus = MockBus::new().with_register(REG_AUDIO_PROC, 0x00);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        // Mute: RMW bit 7
        dev.write_serial_mute(true).unwrap();
        assert_eq!(bus.get_register(REG_AUDIO_PROC), Some(0x80));

        // Unmute: RMW bit 7 clear
        dev.write_serial_mute(false).unwrap();
        assert_eq!(bus.get_register(REG_AUDIO_PROC), Some(0x00));
    }

    #[test]
    fn hardware_device_write_serial_mute_verify_fails_on_mismatch() {
        // Simulate hardware that ignores the write (stuck at 0)
        let bus = MockBus::new().with_stuck_register(REG_AUDIO_PROC, 0x00);
        let mut dev = HardwareDevice::new(bus, addr());

        // This will write 0x80, but read-back returns 0x00 -> verification fails
        let err = dev.write_serial_mute(true).unwrap_err();
        assert!(err.is_verification());
        assert!(err.summary().contains("0x36"));
        assert!(err.summary().contains("0x80"));
        assert!(err.summary().contains("0x00"));
    }

    #[test]
    fn hardware_device_write_power_mode_ctrl_encodes_pm_man() {
        let bus = MockBus::new().with_register(REG_PM_CTRL, 0x00);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        // Write PM_man = 2 (PMP2) -> should encode as 0x40 | (2 << 4) = 0x60
        dev.write_power_mode_ctrl(2).unwrap();
        assert_eq!(bus.get_register(REG_PM_CTRL), Some(0x60));

        // Transition PMP2 -> PMP0 should clear old PM_man bits
        dev.write_power_mode_ctrl(0).unwrap();
        assert_eq!(bus.get_register(REG_PM_CTRL), Some(0x40)); // manualPM=1, PM_man=0
    }

    #[test]
    fn hardware_device_write_power_mode_profile_validates_range() {
        let bus = MockBus::new().with_register(REG_PM_PROFILE, 0x00);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.write_power_mode_profile(4).unwrap();
        assert_eq!(bus.get_register(REG_PM_PROFILE), Some(4));

        let err = dev.write_power_mode_profile(6).unwrap_err();
        assert!(err.is_invalid_argument());
    }

    #[test]
    fn hardware_device_write_i2s_format_validates_range() {
        let bus = MockBus::new().with_register(REG_I2S_FORMAT, 0x00);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.write_i2s_format(7).unwrap();
        assert_eq!(bus.get_register(REG_I2S_FORMAT), Some(7));

        // Reserved encodings 2 and 3 must be rejected.
        let err2 = dev.write_i2s_format(2).unwrap_err();
        assert!(err2.is_invalid_argument());
        let err3 = dev.write_i2s_format(3).unwrap_err();
        assert!(err3.is_invalid_argument());
    }

    #[test]
    fn hardware_device_write_audio_in_mode_validates_range() {
        // Register 0x25 typically has bit 7 set at reset (0x80), preserved by mask 0x9F
        let bus = MockBus::new().with_register(REG_AUDIO_IN_MODE, 0x80);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.write_audio_in_mode(5).unwrap();
        // Mode 5 -> bits 7:5 = 101; lower bits preserved -> 0x80 | 0xA0 = 0xA0... 0x80&0x1F=0, so 0xA0
        assert_eq!(bus.get_register(REG_AUDIO_IN_MODE), Some(0xA0));

        let err = dev.write_audio_in_mode(6).unwrap_err();
        assert!(err.is_invalid_argument());
    }

    #[test]
    fn hardware_device_write_limiter_thresholds_all_channels() {
        // Driver-default −15 dBFS: VLP reg = 24 − (−15) = 39 (0x27)
        let bus = MockBus::new();
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.write_limiter_thresholds(-15).unwrap();
        for ch in 0..4u8 {
            assert_eq!(bus.get_register(REG_THR_DB_CH0 + ch), Some(0x27), "ch{ch}");
        }

        let err = dev.write_limiter_thresholds(1).unwrap_err();
        assert!(err.is_invalid_argument());
        let err = dev.write_limiter_thresholds(-145).unwrap_err();
        assert!(err.is_invalid_argument());
    }

    #[test]
    fn hardware_device_enable_audio_processor_and_limiter_rmw() {
        // Live-reset-typical values: 0x35 = 0x00 (VLP bypassed), 0x36 = 0x01
        let bus = MockBus::new()
            .with_register(REG_I2S_FORMAT, 0x00)
            .with_register(REG_AUDIO_PROC, 0x01);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.enable_audio_processor().unwrap();
        assert_eq!(bus.get_register(REG_I2S_FORMAT), Some(0x08));

        dev.enable_limiter().unwrap();
        assert_eq!(bus.get_register(REG_AUDIO_PROC), Some(0x41));
    }

    #[test]
    fn hardware_device_enable_audio_processor_preserves_other_bits() {
        // Bits 7:3 (attack/release/VLP) and bits 2:0 (format) must survive.
        let bus = MockBus::new().with_register(REG_I2S_FORMAT, 0x83);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.enable_audio_processor().unwrap();
        assert_eq!(bus.get_register(REG_I2S_FORMAT), Some(0x8B));
    }

    #[test]
    fn hardware_device_write_i2s_format_preserves_vlp_and_limiter_bits() {
        // Phase 0 regression guard: the 0x35 mask must touch bits 2:0 only.
        let bus = MockBus::new().with_register(REG_I2S_FORMAT, 0x88);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.write_i2s_format(1).unwrap();
        assert_eq!(bus.get_register(REG_I2S_FORMAT), Some(0x89));
    }

    #[test]
    fn hardware_device_read_settings_decodes_registers() {
        // Reset-typical snapshot: PM_CTRL manualPM|PM_man=2, PM_PROFILE=2,
        // AUDIO_IN_MODE bit7 set (gain 0), I2S format 3, muted (0x36 bit 7),
        // VLP volume reg 116 (raw = 168 − 116 = 52, ≈ −52 dB) with
        // fractional 0.25 dB steps value 2.
        let bus = MockBus::new()
            .with_register(REG_PM_CTRL, 0x60)
            .with_register(REG_PM_PROFILE, 0x02)
            .with_register(REG_AUDIO_IN_MODE, 0x80)
            .with_register(REG_I2S_FORMAT, 0x03)
            .with_register(REG_AUDIO_PROC, 0x80)
            .with_register(REG_VOL_DB_MASTER, 116)
            .with_register(REG_VOL_LSB_MASTER, 0x02);
        let mut dev = HardwareDevice::new(bus, addr());

        let s = dev.read_settings().unwrap();
        assert_eq!(s.master_volume_raw, 52);
        assert_eq!(s.master_volume_frac, 2);
        assert!(s.serial_muted);
        assert_eq!(s.pm_ctrl_pm, 2);
        assert_eq!(s.pm_profile, 2);
        assert_eq!(s.i2s_format, 3);
        assert_eq!(s.audio_in_mode, 4);
    }

    #[test]
    fn hardware_device_settings_round_trip_after_writes() {
        let bus = MockBus::new()
            .with_register(REG_PM_CTRL, 0x00)
            .with_register(REG_PM_PROFILE, 0x00)
            .with_register(REG_AUDIO_IN_MODE, 0x00)
            .with_register(REG_I2S_FORMAT, 0x00)
            .with_register(REG_AUDIO_PROC, 0x00)
            .with_register(REG_VOL_DB_MASTER, 0)
            .with_register(REG_VOL_LSB_MASTER, 0);
        let mut dev = HardwareDevice::new(bus.clone(), addr());

        dev.write_master_volume(144).unwrap();
        dev.write_serial_mute(true).unwrap();
        dev.write_power_mode_ctrl(3).unwrap();
        dev.write_power_mode_profile(4).unwrap();
        dev.write_i2s_format(4).unwrap();
        dev.write_audio_in_mode(3).unwrap();
        dev.configure_limiter_slew(LimiterSlew::Fast, LimiterSlew::Slow)
            .unwrap();

        let s = dev.read_settings().unwrap();
        assert_eq!(s.master_volume_raw, 144);
        assert_eq!(s.master_volume_frac, 0);
        assert!(s.serial_muted);
        assert_eq!(s.pm_ctrl_pm, 3);
        assert_eq!(s.pm_profile, 4);
        assert_eq!(s.i2s_format, 4);
        assert_eq!(s.audio_in_mode, 3);
        // Fast attack (0x80) + slow release (0x00) in the 0x35 register.
        assert_eq!(bus.get_register(REG_I2S_FORMAT), Some((4 & 0x07) | 0x80));
    }
}

// === Phase 2 (Stage A) VLP / limiter mock-path tests ===

#[test]
fn mock_enable_vlp_and_limiter_round_trip() {
    let (amp, mock) = AmpDevice::new_mocked();
    assert!(!mock.mock_vlp_enabled());
    assert!(!mock.mock_limiter_enabled());
    assert_eq!(mock.mock_limiter_thresholds(), -15);

    amp.enable_audio_processor().unwrap();
    amp.enable_limiter().unwrap();
    amp.write_limiter_thresholds(-15).unwrap();

    assert!(mock.mock_vlp_enabled());
    assert!(mock.mock_limiter_enabled());
    assert_eq!(mock.mock_limiter_thresholds(), -15);

    assert!(
        amp.write_limiter_thresholds(1)
            .unwrap_err()
            .is_invalid_argument()
    );
    assert!(
        amp.write_limiter_thresholds(-145)
            .unwrap_err()
            .is_invalid_argument()
    );

    // Fault injection covers the error path.
    mock.set_fault(Some("simulated bus failure".into()));
    assert!(amp.enable_audio_processor().unwrap_err().is_mock_fault());
    assert!(amp.enable_limiter().unwrap_err().is_mock_fault());
    assert!(
        amp.write_limiter_thresholds(-15)
            .unwrap_err()
            .is_mock_fault()
    );
}

#[tokio::test]
async fn mock_async_vlp_and_limiter_writes() {
    let (amp, mock) = AmpDevice::new_mocked();
    amp.enable_audio_processor_async().await.unwrap();
    amp.enable_limiter_async().await.unwrap();
    amp.write_limiter_thresholds_async(-15).await.unwrap();
    assert!(mock.mock_vlp_enabled());
    assert!(mock.mock_limiter_enabled());
    assert_eq!(mock.mock_limiter_thresholds(), -15);
}

// === Phase 1.5 settings read-path tests ===

#[test]
fn decode_vlp_raw_boundaries() {
    use speaker_hardware::amp::decode_vlp_raw;
    // 0x18 (24) = 0 dB -> raw 144; 0xA8 (168) = −144 dB -> raw 0.
    assert_eq!(decode_vlp_raw(24), 144);
    assert_eq!(decode_vlp_raw(168), 0);
    // Mid-scale: reg 68 (−44 dBFS offset) -> raw 100.
    assert_eq!(decode_vlp_raw(68), 100);
    // Boost range (reg < 24) clamps to full volume; > 0xA8 clamps to mute.
    assert_eq!(decode_vlp_raw(0), 144);
    assert_eq!(decode_vlp_raw(200), 0);
}

#[test]
fn mock_read_settings_defaults() {
    let (amp, _mock) = AmpDevice::new_mocked();
    let s = amp.read_settings().unwrap();
    assert_eq!(s.master_volume_raw, 116);
    assert_eq!(s.master_volume_frac, 0);
    assert!(!s.serial_muted);
    assert_eq!(s.pm_ctrl_pm, 2);
    assert_eq!(s.pm_profile, 2);
    assert_eq!(s.i2s_format, 0);
    assert_eq!(s.audio_in_mode, 0);
}

#[test]
fn mock_read_settings_reflects_writes() {
    let (amp, _mock) = AmpDevice::new_mocked();
    amp.write_master_volume(100).unwrap();
    amp.write_serial_mute(true).unwrap();
    amp.write_power_mode_ctrl(3).unwrap();
    amp.write_power_mode_profile(4).unwrap();
    amp.write_i2s_format(4).unwrap();
    amp.write_audio_in_mode(2).unwrap();

    let s = amp.read_settings().unwrap();
    assert_eq!(s.master_volume_raw, 100);
    assert!(s.serial_muted);
    assert_eq!(s.pm_ctrl_pm, 3);
    assert_eq!(s.pm_profile, 4);
    assert_eq!(s.i2s_format, 4);
    assert_eq!(s.audio_in_mode, 2);
}

#[test]
fn mock_read_settings_fault_injection() {
    let (amp, mock) = AmpDevice::new_mocked();
    mock.set_fault(Some("simulated bus failure".into()));
    let err = amp.read_settings().unwrap_err();
    assert!(err.is_mock_fault());
    assert!(err.summary().contains("simulated bus failure"));
}

#[test]
fn settings_from_raw_registers_decodes() {
    // VLP volume reg 116 decodes to user raw = 168 − 116 = 52.
    let s = AmpSettings::from_raw_registers([0x60, 0x02, 0x80, 0x03, 0x80, 116, 0x02]);
    assert_eq!(s.master_volume_raw, 52);
    assert_eq!(s.master_volume_frac, 2);
    assert!(s.serial_muted);
    assert_eq!(s.pm_ctrl_pm, 2);
    assert_eq!(s.pm_profile, 2);
    assert_eq!(s.i2s_format, 3);
    assert_eq!(s.audio_in_mode, 4);

    // Unmuted; VLP reg 0 is the +24 dB boost rail and clamps to full volume
    // (raw 144); out-of-spec register values (0x07 profile, mode 7) still decode.
    let s = AmpSettings::from_raw_registers([0x40, 0x07, 0x7F, 0x07, 0x00, 0, 0x00]);
    assert_eq!(s.master_volume_raw, 144);
    assert_eq!(s.master_volume_frac, 0);
    assert!(!s.serial_muted);
    assert_eq!(s.pm_ctrl_pm, 0);
    assert_eq!(s.pm_profile, 7);
    assert_eq!(s.i2s_format, 7);
    assert_eq!(s.audio_in_mode, 3);

    // Display renders all six decoded settings.
    let text = s.to_string();
    assert!(text.contains("Master Volume"));
    assert!(text.contains("Serial Mute"));
    assert!(text.contains("Manual Power Mode"));
    assert!(text.contains("Power Mode Profile"));
    assert!(text.contains("I2S Format Select"));
    assert!(text.contains("Audio Input Mode"));
    // dB label direction (dB = raw − 144): raw 144 = 0 dB, NOT -144 dB.
    assert!(
        text.contains("Master Volume         : 0 dB (raw 144"),
        "full volume must display as 0 dB, got: {text}"
    );
}

#[tokio::test]
async fn async_settings_read_and_audio_in_mode_write() {
    let (amp, mock) = AmpDevice::new_mocked();

    let s = amp.read_settings_async().await.unwrap();
    assert_eq!(s.pm_ctrl_pm, 2);

    amp.write_audio_in_mode_async(1).await.unwrap();
    assert_eq!(mock.mock_audio_in_mode(), 1);
    assert_eq!(amp.read_settings_async().await.unwrap().audio_in_mode, 1);
}

#[test]
fn shared_register_0x35_writers_preserve_each_others_bits() {
    // Audit Bug D: `0x35` is ONE physical byte shared by the I2S format
    // (bits 2:0), VLP enable (bit 3), and limiter slew (bits 7:4) writers.
    // The mock must model the same read-modify-write semantics as
    // `HardwareDevice` so mask-preservation interactions are exercised.
    let (amp, mock) = AmpDevice::new_mocked();

    amp.write_i2s_format(4).unwrap();
    amp.enable_audio_processor().unwrap();
    amp.configure_limiter_slew(LimiterSlew::Fast, LimiterSlew::Normal)
        .unwrap();

    // Each later writer preserved the earlier writers' bits.
    assert_eq!(mock.mock_i2s_format(), 4, "format preserved across RMWs");
    assert!(mock.mock_vlp_enabled(), "VLP bit preserved");
    let s = amp.read_settings().unwrap();
    assert_eq!(s.i2s_format, 4, "read-back decodes the shared register");
}

#[test]
fn shared_register_0x36_writers_preserve_each_others_bits() {
    // `0x36` is ONE physical byte: serial mute (bit 7) and limiter enable
    // (bit 6) must not clobber each other.
    let (amp, mock) = AmpDevice::new_mocked();

    amp.enable_limiter().unwrap();
    amp.write_serial_mute(true).unwrap();
    assert!(mock.mock_serial_muted());
    assert!(mock.mock_limiter_enabled(), "limiter bit preserved by mute");

    amp.write_serial_mute(false).unwrap();
    assert!(!mock.mock_serial_muted());
    assert!(
        mock.mock_limiter_enabled(),
        "limiter bit survives the unmute"
    );
    assert!(!amp.read_settings().unwrap().serial_muted);
}
