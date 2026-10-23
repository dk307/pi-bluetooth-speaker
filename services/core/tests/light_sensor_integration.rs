//! Integration tests for the BH1750 ambient light sensor.

use std::time::Duration;

use speaker_core::hardware::{I2cAddress, LightSensor, MeasurementMode};

#[test]
fn address_validation_and_hardware_metadata() {
    let addr = I2cAddress::new(0x23).unwrap();
    let (sensor, _) = LightSensor::new_mocked();
    let hw = sensor.hardware_info();

    assert_eq!(addr.as_u8(), 0x23);
    assert_eq!(addr.as_u16(), 0x23);
    assert_eq!(hw.bus, 1);
    assert_eq!(hw.address.as_u8(), 0x23);
    assert_eq!(hw.sda_pin.bcm_pin.as_u8(), 2);
    assert_eq!(hw.scl_pin.bcm_pin.as_u8(), 3);
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_mock_updates() {
    let (sensor, mock) = LightSensor::new_mocked();
    let sensor_clone = sensor.clone();

    // Verify initial simulated reading (100 lx)
    let initial = sensor.read_lux_async().await.unwrap();
    assert!((initial.lux() - 100.0).abs() < 1e-3);

    // Update in mock controller
    mock.set_lux(1250.5);

    // Observe update in separate async task
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        sensor_clone.read_lux_async().await
    });

    let observed = handle.await.unwrap().unwrap();
    assert!((observed.lux() - 1250.5).abs() < 1.0);
    assert!((observed.foot_candles() - (1250.5 * 0.092_903)).abs() < 0.5);
}

#[test]
fn measurement_mode_switching_and_diagnostics() {
    let (sensor, mock) = LightSensor::new_mocked();

    sensor
        .set_mode(MeasurementMode::ContinuousHighRes2)
        .unwrap();
    assert_eq!(mock.current_mode(), MeasurementMode::ContinuousHighRes2);

    mock.set_raw_word(240); // 240 / 1.2 / 2.0 = 100 lx in HighRes2 mode
    let diag = sensor.read_diagnostics().unwrap();
    assert_eq!(diag.mode, MeasurementMode::ContinuousHighRes2);
    assert_eq!(diag.raw_word, 240);
    assert!((diag.illuminance.lux() - 100.0).abs() < 1e-2);
}
