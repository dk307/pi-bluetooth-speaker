//! Integration tests for the DS18B20 1-Wire temperature sensor driver.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use speaker_core::hardware::{
    DS18B20_BCM_GPIO_PIN, DS18B20_DTOVERLAY, DS18B20_PHYSICAL_PIN, SensorId, TempSensor,
    Temperature,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Scoped temporary directory that removes itself on drop.
struct ScopedTempDir {
    path: PathBuf,
}

impl ScopedTempDir {
    fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("{prefix}_{pid}_{id}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create temporary test directory");
        Self { path }
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn sysfs_backend_discovers_and_reads_multiple_ds18b20_sensors() {
    let temp_dir = ScopedTempDir::new("w1_devices_test");

    // 1. Create two valid DS18B20 sensor directories
    let dev1 = temp_dir.path.join("28-00000aabbcc1");
    let dev2 = temp_dir.path.join("28-00000aabbcc2");
    // And a non-DS18B20 w1_bus_master1 directory that must be ignored
    let master = temp_dir.path.join("w1_bus_master1");

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

    let sensor = TempSensor::with_sysfs_path(&temp_dir.path);

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
    assert_eq!(
        readings[0].to_string(),
        "[28-00000aabbcc1]  26.44°C  |   79.59°F"
    );

    // Verify primary sensor reading
    let primary = sensor.read_primary_temperature().unwrap();
    assert_eq!(primary.millidegrees_celsius(), 26_437);
}

#[test]
fn sysfs_backend_reports_error_when_directory_has_no_ds18b20_devices() {
    let temp_dir = ScopedTempDir::new("w1_empty_test");
    fs::create_dir_all(temp_dir.path.join("w1_bus_master1")).unwrap();

    let sensor = TempSensor::with_sysfs_path(&temp_dir.path);
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
