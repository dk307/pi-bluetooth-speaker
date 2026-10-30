//! Integration tests for the DS18B20 1-Wire temperature sensor driver.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use speaker_core::diag::ScopedTempDir;
use speaker_core::hardware::{
    DS18B20_BCM_GPIO_PIN, DS18B20_DTOVERLAY, DS18B20_PHYSICAL_PIN, DS18B20_SYSFS_BASE_PATH,
    PowerSupplyMode, SensorDiagnostics, SensorId, SensorReading, SocThermal, TempError, TempSensor,
    Temperature, parse_w1_slave, temp_test_helpers,
};

#[test]
fn sysfs_backend_discovers_and_reads_multiple_ds18b20_sensors() {
    let temp_dir = ScopedTempDir::new("w1_devices_test");

    // 1. Create two valid DS18B20 sensor directories
    let dev1 = temp_dir.path().join("28-00000aabbcc1");
    let dev2 = temp_dir.path().join("28-00000aabbcc2");
    // And a non-DS18B20 w1_bus_master1 directory that must be ignored
    let master = temp_dir.path().join("w1_bus_master1");

    fs::create_dir_all(&dev1).unwrap();
    fs::create_dir_all(&dev2).unwrap();
    fs::create_dir_all(&master).unwrap();

    fs::write(
        dev1.join("w1_slave"),
        "a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES\na7 01 4b 46 7f ff 0c 10 5c t=26437\n",
    )
    .unwrap();
    fs::write(
        dev2.join("w1_slave"),
        "b2 01 4b 46 7f ff 0c 10 11 : crc=11 YES\nb2 01 4b 46 7f ff 0c 10 11 t=30125\n",
    )
    .unwrap();

    let sensor = TempSensor::with_sysfs_path(temp_dir.path());

    // Verify GPIO metadata
    let gpio = sensor.gpio_info();
    assert_eq!(gpio.bcm_pin, DS18B20_BCM_GPIO_PIN);
    assert_eq!(gpio.physical_pin, DS18B20_PHYSICAL_PIN);
    assert_eq!(gpio.electrical_config, DS18B20_DTOVERLAY);
    assert!(gpio.to_string().contains("BCM GPIO 26"));
    assert!(gpio.to_string().contains("Physical Pin 37"));

    // Verify deterministic sorted discovery
    let ids = sensor.discover_sensors().unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0].as_str(), "28-00000aabbcc1");
    assert_eq!(ids[1].as_str(), "28-00000aabbcc2");

    // Verify readings
    let readings = sensor.read_all().unwrap();
    assert_eq!(readings.len(), 2);
    assert_eq!(readings[0].temperature.millidegrees_celsius(), 26_437);
    assert_eq!(readings[1].temperature.millidegrees_celsius(), 30_125);
    assert_eq!(readings[0].to_string(), "[28-00000aabbcc1]  26.44°C");

    // Verify primary sensor reading
    let primary = sensor.read_primary_temperature().unwrap();
    assert_eq!(primary.millidegrees_celsius(), 26_437);
}

#[test]
fn sysfs_backend_reports_error_when_directory_has_no_ds18b20_devices() {
    let temp_dir = ScopedTempDir::new("w1_empty_test");
    fs::create_dir_all(temp_dir.path().join("w1_bus_master1")).unwrap();

    let sensor = TempSensor::with_sysfs_path(temp_dir.path());
    let err = sensor.discover_sensors().unwrap_err();
    assert!(err.is_no_sensors());
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_live_mock_updates() {
    let (sensor, mock) = TempSensor::new_mocked();
    let id = SensorId::parse("28-00000feedbee").unwrap();
    mock.set_temperature(id.clone(), Temperature::from_celsius(25.0).unwrap());

    // Clone sensor handle for a background consumer task (M-SERVICES-CLONE)
    let sensor_clone = sensor.clone();
    let t1 = sensor_clone.read_primary_temperature_async().await.unwrap();
    assert_eq!(t1.millidegrees_celsius(), 25_000);

    // Update temperature via mock controller and verify clone sees updated reading
    mock.set_temperature(id, Temperature::from_celsius(42.5).unwrap());
    let t2 = sensor_clone.read_primary_temperature_async().await.unwrap();
    assert_eq!(t2.millidegrees_celsius(), 42_500);
}

#[test]
fn sysfs_backend_reads_full_diagnostics_and_various_power_modes() {
    let temp_dir = ScopedTempDir::new("w1_diag_test");

    let dev1 = temp_dir.path().join("28-00000d1a0001");
    let dev2 = temp_dir.path().join("28-00000d1a0002");
    let dev3 = temp_dir.path().join("28-00000d1a0003");

    fs::create_dir_all(&dev1).unwrap();
    fs::create_dir_all(&dev2).unwrap();
    fs::create_dir_all(&dev3).unwrap();

    // Dev 1: External power (1), 12-bit, 750ms, alarms 0 75
    fs::write(
        dev1.join("w1_slave"),
        "a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES\na7 01 4b 46 7f ff 0c 10 5c t=26437\n",
    )
    .unwrap();
    fs::write(dev1.join("resolution"), "12\n").unwrap();
    fs::write(dev1.join("ext_power"), "1\n").unwrap();
    fs::write(dev1.join("conv_time"), "750\n").unwrap();
    fs::write(dev1.join("alarms"), "0 75\n").unwrap();

    // Dev 2: Parasite power (0)
    fs::write(
        dev2.join("w1_slave"),
        "b2 01 4b 46 7f ff 0c 10 11 : crc=11 YES\nb2 01 4b 46 7f ff 0c 10 11 t=30125\n",
    )
    .unwrap();
    fs::write(dev2.join("ext_power"), "0\n").unwrap();

    // Dev 3: Unknown power mode (99)
    fs::write(
        dev3.join("w1_slave"),
        "b2 01 4b 46 7f ff 0c 10 11 : crc=11 YES\nb2 01 4b 46 7f ff 0c 10 11 t=30125\n",
    )
    .unwrap();
    fs::write(dev3.join("ext_power"), "99\n").unwrap();

    let sensor = TempSensor::with_sysfs_path(temp_dir.path());
    let id1 = SensorId::parse("28-00000d1a0001").unwrap();
    let diag1 = sensor.read_diagnostics(&id1).unwrap();
    assert_eq!(diag1.resolution_bits, Some(12));
    assert_eq!(diag1.conversion_time_ms, Some(750));
    assert_eq!(diag1.alarms, Some((0, 75)));
    assert_eq!(diag1.power_mode, Some(PowerSupplyMode::External));

    let id2 = SensorId::parse("28-00000d1a0002").unwrap();
    let diag2 = sensor.read_diagnostics(&id2).unwrap();
    assert_eq!(diag2.power_mode, Some(PowerSupplyMode::Parasite));

    let id3 = SensorId::parse("28-00000d1a0003").unwrap();
    let diag3 = sensor.read_diagnostics(&id3).unwrap();
    assert_eq!(diag3.power_mode, Some(PowerSupplyMode::Unknown(99)));
}

#[tokio::test]
async fn runtime_task_cancellation_mapping() {
    let err: speaker_core::hardware::TempError =
        speaker_core::runtime::abort_and_map("temperature integration task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("temperature integration task"));
}

#[test]
fn temperature_conversions_and_bounds() {
    let t = Temperature::from_millidegrees_celsius(26_437).unwrap();
    assert_eq!(t.millidegrees_celsius(), 26_437);
    assert!((t.celsius() - 26.437).abs() < 1e-4);
    assert_eq!(t.to_string(), "26.44°C");

    // Negative temperature
    let neg = Temperature::from_millidegrees_celsius(-5_125).unwrap();
    assert!((neg.celsius() - (-5.125)).abs() < 1e-4);

    // Boundary check: below -55 °C and above +125 °C must fail
    let low_err = Temperature::from_millidegrees_celsius(-55_001).unwrap_err();
    assert!(low_err.is_out_of_range());

    let high_err = Temperature::from_millidegrees_celsius(125_001).unwrap_err();
    assert!(high_err.is_out_of_range());

    let nan_err = Temperature::from_celsius(f32::NAN).unwrap_err();
    assert!(nan_err.is_out_of_range());
}

#[test]
fn sensor_id_validates_family_prefix_and_suffix() {
    let valid = SensorId::parse("28-00000abcdef0").unwrap();
    assert_eq!(valid.as_str(), "28-00000abcdef0");

    assert!(
        SensorId::parse("10-00000abcdef0")
            .unwrap_err()
            .is_invalid_sensor_id()
    );
    assert!(SensorId::parse("28-").unwrap_err().is_invalid_sensor_id());
    assert!(
        SensorId::parse("28-abc/def")
            .unwrap_err()
            .is_invalid_sensor_id()
    );
    // Non-hexadecimal characters ('g', 'z') must be rejected per 1-Wire DS18B20 specification
    assert!(
        SensorId::parse("28-00000diag001")
            .unwrap_err()
            .is_invalid_sensor_id()
    );
    assert!(
        SensorId::parse("28-0000000xyz01")
            .unwrap_err()
            .is_invalid_sensor_id()
    );
}

#[test]
fn parse_w1_slave_handles_valid_crc_and_rejects_corrupt_frames() {
    let valid_payload = "\
a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES
a7 01 4b 46 7f ff 0c 10 5c t=26437
";
    let temp = parse_w1_slave(valid_payload).unwrap();
    assert_eq!(temp.millidegrees_celsius(), 26_437);

    let crc_fail_payload = "\
a7 01 4b 46 7f ff 0c 10 5c : crc=5c NO
a7 01 4b 46 7f ff 0c 10 5c t=26437
";
    let err = parse_w1_slave(crc_fail_payload).unwrap_err();
    assert!(err.is_crc_mismatch());

    let missing_t_payload = "\
a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES
a7 01 4b 46 7f ff 0c 10 5c
";
    let err = parse_w1_slave(missing_t_payload).unwrap_err();
    assert!(err.is_parse_error());

    // Empty payload
    assert!(parse_w1_slave("").unwrap_err().is_parse_error());

    // Single line with YES but missing second line
    assert!(
        parse_w1_slave("a7 01 4b 46 : crc=5c YES\n")
            .unwrap_err()
            .is_parse_error()
    );

    // Missing colon on line 1 without YES: the 1-Wire kernel driver signals hardware
    // cyclic redundancy check failures with "NO" on line 1; thus payloads lacking "YES"
    // classify as CRC mismatches.
    let no_colon = "a7 01 4b 46 7f ff 0c 10 5c crc=5c NO\na7 01 4b 46 7f ff 0c 10 5c t=26437\n";
    assert!(parse_w1_slave(no_colon).unwrap_err().is_crc_mismatch());

    // Invalid integer in t=
    let bad_int =
        "a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES\na7 01 4b 46 7f ff 0c 10 5c t=not_an_int\n";
    assert!(parse_w1_slave(bad_int).unwrap_err().is_parse_error());
}

#[test]
fn mock_sensor_supports_readings_and_fault_injection() {
    let (sensor, mock) = TempSensor::new_mocked();

    // Initially empty -> NoSensorsDetected
    assert!(sensor.discover_sensors().unwrap_err().is_no_sensors());

    let id1 = SensorId::parse("28-000000000001").unwrap();
    let id2 = SensorId::parse("28-000000000002").unwrap();
    mock.set_temperature(id1.clone(), Temperature::from_celsius(26.44).unwrap());
    mock.set_raw_w1_slave(
        id2.clone(),
        "ff 07 4b 46 7f ff 0c 10 5c : crc=5c YES\nff 07 4b 46 7f ff 0c 10 5c t=31250\n",
    );

    let all = sensor.read_all().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].sensor_id, id1);
    assert_eq!(all[0].temperature.millidegrees_celsius(), 26_440);
    assert_eq!(all[1].sensor_id, id2);
    assert_eq!(all[1].temperature.millidegrees_celsius(), 31_250);

    // Extended diagnostics
    let diag = sensor.read_diagnostics(&id1).unwrap();
    assert_eq!(diag.resolution_bits, Some(12));
    assert_eq!(diag.power_mode, Some(PowerSupplyMode::External));

    // Diagnostics for sensor with RawPayload
    let diag_raw = sensor.read_diagnostics(&id2).unwrap();
    assert!(diag_raw.raw_payload.contains("ff 07 4b 46"));

    // Inject CRC error on id2
    mock.set_raw_w1_slave(id2.clone(), "00 : crc=00 NO\n00 t=31250\n");
    assert!(sensor.read_sensor(&id2).unwrap_err().is_crc_mismatch());

    // Inject bus I/O error
    mock.set_io_error("1-Wire bus disconnected");
    assert!(sensor.read_primary_temperature().unwrap_err().is_io());

    mock.clear_io_error();
    assert_eq!(
        sensor
            .read_primary_temperature()
            .unwrap()
            .millidegrees_celsius(),
        26_440
    );
}

#[tokio::test]
async fn async_methods_read_from_sensor_without_blocking() {
    fn assert_send<T: Send>(_: T) {}
    let (sensor, mock) = TempSensor::new_mocked();
    let id = SensorId::parse("28-00000abcdef1").unwrap();
    assert_send(sensor.discover_sensors_async());
    assert_send(sensor.read_primary_temperature_async());
    assert_send(sensor.read_all_async());
    assert_send(sensor.read_sensor_async(&id));
    assert_send(sensor.read_diagnostics_async(&id));

    mock.set_temperature(
        id.clone(),
        Temperature::from_millidegrees_celsius(28_500).unwrap(),
    );

    let discovered = sensor.discover_sensors_async().await.unwrap();
    assert_eq!(discovered, vec![id.clone()]);

    let primary = sensor.read_primary_temperature_async().await.unwrap();
    assert_eq!(primary.millidegrees_celsius(), 28_500);

    let reading = sensor.read_sensor_async(&id).await.unwrap();
    assert_eq!(reading.temperature.millidegrees_celsius(), 28_500);

    let diag = sensor.read_diagnostics_async(&id).await.unwrap();
    assert_eq!(diag.temperature.millidegrees_celsius(), 28_500);
}

#[tokio::test]
async fn cancelled_blocking_task_maps_to_temp_error_cancelled() {
    let err: TempError = speaker_core::runtime::abort_and_map("1-Wire sensor read").await;
    assert!(err.is_cancelled());
    assert!(!err.is_io());
    assert!(
        err.to_string()
            .contains("1-Wire sensor read task was cancelled")
    );
}

#[test]
fn soc_thermal_parses_content() {
    let temp_dir = ScopedTempDir::new("soc_thermal_test");
    let path = temp_dir.path().join("temp");
    fs::write(&path, "41856\n").unwrap();

    let soc = SocThermal::read_from_path(&path).unwrap();
    assert_eq!(soc.temperature().millidegrees_celsius(), 41_856);
    assert!((soc.temperature().celsius() - 41.856).abs() < 1e-3);
    assert_eq!(soc.to_string(), "SoC: 41.86°C");

    let chassis = Temperature::from_millidegrees_celsius(24_000).unwrap();
    assert!((soc.delta_from_chassis(chassis) - 17.856).abs() < 1e-2);

    // Parse error
    let bad_path = temp_dir.path().join("bad_temp");
    fs::write(&bad_path, "not-a-number\n").unwrap();
    let err = SocThermal::read_from_path(&bad_path).unwrap_err();
    assert!(err.is_parse_error());

    // Missing file
    let missing_err = SocThermal::read_from_path(temp_dir.path().join("nonexistent")).unwrap_err();
    assert!(missing_err.is_io());
}

#[test]
fn temperature_constructors_and_try_from() {
    let t = Temperature::new_millidegrees_celsius(25_000);
    assert_eq!(t.millidegrees_celsius(), 25_000);

    let from_try: Temperature = 30_000.try_into().unwrap();
    assert_eq!(from_try.millidegrees_celsius(), 30_000);

    let err_try: Result<Temperature, _> = 200_000.try_into();
    let _ = err_try.unwrap_err();

    assert!(
        Temperature::from_celsius(-60.0)
            .unwrap_err()
            .is_out_of_range()
    );
    assert!(
        Temperature::from_celsius(130.0)
            .unwrap_err()
            .is_out_of_range()
    );
    assert!(
        Temperature::from_celsius(f32::INFINITY)
            .unwrap_err()
            .is_out_of_range()
    );
    assert!(
        Temperature::from_celsius(f32::NEG_INFINITY)
            .unwrap_err()
            .is_out_of_range()
    );
    assert!(
        Temperature::from_celsius(f32::NAN)
            .unwrap_err()
            .is_out_of_range()
    );
}

#[test]
#[should_panic(expected = "temperature millidegrees_c must be within DS18B20 range")]
fn temperature_new_panics_on_low_overflow() {
    let _ = Temperature::new_millidegrees_celsius(-55_001);
}

#[test]
#[should_panic(expected = "temperature millidegrees_c must be within DS18B20 range")]
fn temperature_new_panics_on_high_overflow() {
    let _ = Temperature::new_millidegrees_celsius(125_001);
}

#[test]
fn sensor_id_traits_and_conversions() {
    let id_str = "28-01201b7d5396";
    let id: SensorId = id_str.parse().unwrap();
    assert_eq!(id.as_ref(), id_str);
    assert_eq!(id.to_string(), id_str);

    let id_from_str: SensorId = id_str.try_into().unwrap();
    assert_eq!(id_from_str, id);

    let id_from_string: SensorId = id_str.to_string().try_into().unwrap();
    assert_eq!(id_from_string, id);
}

#[test]
fn sensor_reading_and_diagnostics_display_formatting() {
    let sensor_id = SensorId::parse("28-01201b7d5396").unwrap();
    let temperature = Temperature::from_millidegrees_celsius(22_500).unwrap();
    let reading = SensorReading {
        sensor_id: sensor_id.clone(),
        temperature,
    };
    let formatted = reading.to_string();
    assert!(formatted.contains("[28-01201b7d5396]"));
    assert!(formatted.contains("22.50°C"));

    // PowerSupplyMode Display
    assert_eq!(
        PowerSupplyMode::External.to_string(),
        "External 3.3V (Vdd pin)"
    );
    assert_eq!(
        PowerSupplyMode::Parasite.to_string(),
        "Parasite power (data line)"
    );
    assert_eq!(PowerSupplyMode::Unknown(42).to_string(), "Unknown (42)");

    // Diagnostics with all resolutions
    for (res, prec) in [
        (9, "0.5°C"),
        (10, "0.25°C"),
        (11, "0.125°C"),
        (12, "0.0625°C"),
        (8, "variable"),
    ] {
        let diag = SensorDiagnostics {
            sensor_id: sensor_id.clone(),
            temperature,
            resolution_bits: Some(res),
            power_mode: Some(PowerSupplyMode::External),
            conversion_time_ms: Some(750),
            alarms: Some((0, 70)),
            raw_payload: "raw frame\n".into(),
        };
        let text = diag.to_string();
        assert!(text.contains(prec));
        assert!(text.contains("External 3.3V"));
        assert!(text.contains("750 ms"));
        assert!(text.contains("Low: 0°C | High: 70°C"));
    }

    // Minimal diagnostics with None fields
    let minimal_diag = SensorDiagnostics {
        sensor_id,
        temperature,
        resolution_bits: None,
        power_mode: None,
        conversion_time_ms: None,
        alarms: None,
        raw_payload: "raw\n".into(),
    };
    let min_text = minimal_diag.to_string();
    assert_ne!(min_text, "");
    assert!(!min_text.contains("Resolution     :"));
}

#[test]
fn temp_error_all_variants_predicates_and_display() {
    let no_sensors = temp_test_helpers::make_no_sensors_error(PathBuf::from("/sys/bus/w1/devices"));
    assert!(no_sensors.is_no_sensors());
    assert!(
        no_sensors
            .to_string()
            .contains("no DS18B20 sensors (28-*) detected")
    );
    assert!(no_sensors.source().is_none());
    let _ = no_sensors.backtrace();

    let crc_with_id = temp_test_helpers::make_crc_mismatch_error(Some("28-001".into()));
    assert!(crc_with_id.is_crc_mismatch());
    assert!(
        crc_with_id
            .to_string()
            .contains("1-Wire CRC check failed for sensor 28-001")
    );

    let crc_no_id = temp_test_helpers::make_crc_mismatch_error(None);
    assert!(crc_no_id.is_crc_mismatch());
    assert!(crc_no_id.to_string().contains("1-Wire CRC check failed"));

    let parse_err = temp_test_helpers::make_parse_error("missing t=");
    assert!(parse_err.is_parse_error());
    assert!(parse_err.to_string().contains("failed to parse w1_slave"));

    let range_err = temp_test_helpers::make_out_of_range_error(150_000);
    assert!(range_err.is_out_of_range());
    assert!(
        range_err
            .to_string()
            .contains("temperature 150000 m°C is outside")
    );

    let invalid_sensor_err = temp_test_helpers::make_invalid_sensor_id_error("bad-id");
    assert!(invalid_sensor_err.is_invalid_sensor_id());
    assert!(
        invalid_sensor_err
            .to_string()
            .contains("invalid DS18B20 sensor ID 'bad-id'")
    );

    let temp_io_err = TempError::from(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
    assert!(temp_io_err.is_io());
    assert!(temp_io_err.to_string().contains("1-Wire sysfs I/O error"));
    assert!(temp_io_err.source().is_some());

    let cancel_err = temp_test_helpers::make_cancelled_error("worker cancelled");
    assert!(cancel_err.is_cancelled());
    assert!(!cancel_err.is_io());
    assert!(!cancel_err.is_no_sensors());
    assert!(matches!(
        cancel_err.backtrace().status(),
        std::backtrace::BacktraceStatus::Captured | std::backtrace::BacktraceStatus::Disabled
    ));
    assert!(
        cancel_err
            .to_string()
            .contains("task cancelled: worker cancelled")
    );
    assert!(cancel_err.source().is_none());

    // Machine-oriented single-line summary omits captured backtrace
    for err in [
        &no_sensors,
        &crc_with_id,
        &crc_no_id,
        &parse_err,
        &range_err,
        &invalid_sensor_err,
        &temp_io_err,
        &cancel_err,
    ] {
        let s = err.summary();
        assert!(!s.contains('\n'), "summary contained newline: {s}");
        assert!(
            !s.contains("Backtrace:"),
            "summary contained backtrace: {s}"
        );
        assert!(err.to_string().starts_with(&s));
    }
}

#[tokio::test]
async fn temp_sensor_read_all_async_and_metadata() {
    let (sensor, mock) = TempSensor::new_mocked();
    assert_eq!(sensor.sysfs_base_path(), Path::new(DS18B20_SYSFS_BASE_PATH));

    let gpio = sensor.gpio_info();
    assert_eq!(gpio.bcm_pin.as_u8(), 26);
    assert_eq!(gpio.physical_pin.as_u8(), 37);

    let id = SensorId::parse("28-000000000042").unwrap();
    mock.set_temperature(id.clone(), Temperature::from_celsius(20.0).unwrap());

    let all = sensor.read_all_async().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].sensor_id, id);
}

#[test]
fn temp_sensor_mock_diagnostics_and_clear() {
    let (sensor, mock) = TempSensor::new_mocked();
    let id = SensorId::parse("28-000000000042").unwrap();
    mock.set_temperature(id.clone(), Temperature::from_celsius(20.0).unwrap());

    mock.set_diagnostics(
        id.clone(),
        Some(10),
        Some(PowerSupplyMode::Parasite),
        Some(188),
        Some((-10, 50)),
    );
    let diag = sensor.read_diagnostics(&id).unwrap();
    assert_eq!(diag.resolution_bits, Some(10));
    assert_eq!(diag.power_mode, Some(PowerSupplyMode::Parasite));
    assert_eq!(diag.conversion_time_ms, Some(188));
    assert_eq!(diag.alarms, Some((-10, 50)));

    // Test sensor not found
    let missing_id = SensorId::parse("28-000000000099").unwrap();
    let _ = sensor.read_sensor(&missing_id).unwrap_err();

    // Test clear_sensors
    mock.clear_sensors();
    assert!(sensor.discover_sensors().unwrap_err().is_no_sensors());
}

#[test]
fn temp_sensor_sysfs_path_defaults_and_errors() {
    // Hermeticity invariant: TempSensor::new() and with_sysfs_path() perform zero
    // filesystem syscalls at construction time, succeeding even when the path does not exist.
    let default_sensor = TempSensor::new();
    assert_eq!(
        default_sensor.sysfs_base_path(),
        Path::new(DS18B20_SYSFS_BASE_PATH)
    );
    let nonexistent = TempSensor::with_sysfs_path("/nonexistent/hermetic/w1/path");
    assert_eq!(
        nonexistent.sysfs_base_path(),
        Path::new("/nonexistent/hermetic/w1/path")
    );

    // Test sysfs path that is a file (triggers Err(NotADirectory) in discover_sysfs_sensors)
    let temp_dir = ScopedTempDir::new("ds18b20_file_test");
    let temp_file = temp_dir.path().join("not_a_dir");
    fs::write(&temp_file, "not a directory").unwrap();
    let file_sensor = TempSensor::with_sysfs_path(&temp_file);
    let res = file_sensor.discover_sensors();
    assert!(res.unwrap_err().is_io());
}
