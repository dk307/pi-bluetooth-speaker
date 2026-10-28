//! Integration tests for the DS18B20 1-Wire temperature sensor driver.

use std::fs;

use speaker_core::diag::ScopedTempDir;
use speaker_core::hardware::{
    DS18B20_BCM_GPIO_PIN, DS18B20_DTOVERLAY, DS18B20_PHYSICAL_PIN, PowerSupplyMode, SensorId,
    TempSensor, Temperature,
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
