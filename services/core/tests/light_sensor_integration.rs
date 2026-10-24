//! Integration tests for the BH1750 ambient light sensor.

use std::time::Duration;

use speaker_core::hardware::{I2cAddress, LUX_TO_FOOT_CANDLES, LightSensor, MeasurementMode};

#[test]
fn address_validation_and_hardware_metadata() {
    let addr = I2cAddress::new(0x23).unwrap();
    let (sensor, _) = LightSensor::new_mocked();
    let hw = sensor.hardware_info();

    assert_eq!(addr.as_u8(), 0x23);
    assert_eq!(addr.as_u16(), 0x0023);
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

    // Update in mock controller (exact multiple of 1.2 resolution divisor)
    mock.set_lux(1200.0);

    // Observe update in separate async task
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        sensor_clone.read_lux_async().await
    });

    let observed = handle.await.unwrap().unwrap();
    assert!((observed.lux() - 1200.0).abs() < 1e-3);
    assert!((observed.foot_candles() - (1200.0 * LUX_TO_FOOT_CANDLES)).abs() < 1e-4);
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

#[tokio::test]
async fn concurrent_sensor_clones_power_and_mode_operations() {
    let (sensor, _mock) = LightSensor::new_mocked();
    let s1 = sensor.clone();
    let s2 = sensor.clone();

    let h1 = tokio::spawn(async move {
        for _ in 0..10 {
            s1.set_mode(MeasurementMode::ContinuousLowRes).unwrap();
            if let Err(err) = s1.read_lux_async().await {
                assert!(err.is_powered_down(), "unexpected error: {err}");
            }
        }
    });

    let h2 = tokio::spawn(async move {
        for _ in 0..10 {
            s2.power_down().unwrap();
            s2.reset().unwrap();
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();
}
