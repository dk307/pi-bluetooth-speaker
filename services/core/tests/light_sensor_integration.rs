//! Integration and unit tests for the BH1750 ambient light sensor.
//!
//! Verifies hardware state transitions, I2C address parsing, illuminance conversions,
//! error mapping, diagnostic telemetry, and concurrent async mocking outside the source tree.

use std::error::Error;
use std::time::{Duration, Instant};

use bh1750_embedded::{Address as BhAddress, Error as BhError};
use speaker_core::diag::LUX_TO_FOOT_CANDLES;
use speaker_core::hardware::{
    BH1750_DEFAULT_I2C_ADDR, BH1750_DEFAULT_I2C_BUS, I2cAddress, I2cHardwareInfo, Illuminance,
    LightError, LightHwState, LightSensor, light_test_helpers,
};

#[test]
fn address_validation_and_hardware_metadata() {
    let addr = I2cAddress::new(0x23).unwrap();
    let (sensor, _) = LightSensor::new_mocked();
    let hw = sensor.hardware_info();

    assert_eq!(addr.as_u8(), 0x23);
    assert_eq!(addr.as_u16(), 0x0023);
    assert_eq!(I2cAddress::parse_hex("0x5C").unwrap().as_u8(), 0x5C);
    for signed in ["+0x23", "-0x23", "0x+23", "0x-23", "0X+5C"] {
        assert!(
            I2cAddress::parse_hex(signed)
                .unwrap_err()
                .contains("sign characters")
        );
    }
    assert_eq!(hw.bus, 1);
    assert_eq!(hw.address.as_u8(), 0x23);
    assert_eq!(hw.sda_pin.bcm_pin.as_u8(), 2);
    assert_eq!(hw.scl_pin.bcm_pin.as_u8(), 3);

    let illum = Illuminance::from_lux(250.0).unwrap();
    assert_eq!(illum.millilux(), 250_000);
    assert!((speaker_core::diag::foot_candles(250.0) - illum.foot_candles()).abs() < 1e-4);
}

#[test]
fn illuminance_conversions_and_bounds() {
    let illum = Illuminance::from_lux(100.0).unwrap();
    assert_eq!(illum.lux(), 100.0);
    assert_eq!(illum.millilux(), 100_000);
    assert!((illum.foot_candles() - 9.2903).abs() < 1e-3);

    // Raw conversion (120 raw -> 100 lx)
    let converted = Illuminance::from_raw(120);
    assert!((converted.lux() - 100.0).abs() < 1e-3);

    // Validation rejects negative and NaN
    let _ = Illuminance::from_lux(-1.0).unwrap_err();
    let _ = Illuminance::from_lux(f32::NAN).unwrap_err();
    let _ = Illuminance::from_lux(f32::INFINITY).unwrap_err();
}

#[test]
fn i2c_address_validates_known_addresses() {
    assert_eq!(I2cAddress::new(0x23).unwrap().as_u8(), 0x23);
    assert_eq!(I2cAddress::new(0x5C).unwrap().as_u8(), 0x5C);
    assert_eq!(I2cAddress::try_from(0x23).unwrap().as_u8(), 0x23);
    assert_eq!(I2cAddress::try_from(0x5C).unwrap().as_u8(), 0x5C);
    assert!(I2cAddress::try_from(0x42).unwrap_err().is_invalid_address());
    assert!(I2cAddress::new(0x00).unwrap_err().is_invalid_address());
    assert!(I2cAddress::new(0x20).unwrap_err().is_invalid_address());
    assert!(I2cAddress::new(0x77).unwrap_err().is_invalid_address());
    assert!(I2cAddress::new(0x80).unwrap_err().is_invalid_address());

    assert_eq!(I2cAddress::parse_hex("0x23").unwrap().as_u8(), 0x23);
    assert_eq!(I2cAddress::parse_hex(" 0X5C ").unwrap().as_u8(), 0x5C);
    assert_eq!(I2cAddress::parse_hex("23").unwrap().as_u8(), 0x23);
    for signed in ["+0x23", "-0x23", "0x+23", "0x-23", "0X+5C", "+23", "-23"] {
        assert!(
            I2cAddress::parse_hex(signed)
                .unwrap_err()
                .contains("sign characters are not allowed"),
            "failed to reject signed input {signed}"
        );
    }
    for invalid in ["", "0x", "0X", "not-hex", "0xzz"] {
        assert!(
            I2cAddress::parse_hex(invalid)
                .unwrap_err()
                .contains("invalid hex address"),
            "failed to reject invalid hex {invalid}"
        );
    }
    assert_eq!(
        I2cAddress::parse_hex("0x77").unwrap_err(),
        "invalid BH1750 I2C address '0x77' (expected 0x23 or 0x5C)"
    );
}

#[test]
fn mock_sensor_supports_readings_and_fault_injection() {
    let (sensor, mock) = LightSensor::new_mocked();
    assert!(mock.is_powered());

    // Default mock value: 100 lx
    let reading = sensor.read_lux().unwrap();
    assert!((reading.lux() - 100.0).abs() < 1e-3);

    // Update mock illuminance
    mock.set_lux(500.0);
    let reading = sensor.read_lux().unwrap();
    assert!((reading.lux() - 500.0).abs() < 1e-1);

    // Diagnostics
    let diag = sensor.read_diagnostics().unwrap();
    assert_eq!(diag.bus, BH1750_DEFAULT_I2C_BUS);
    assert_eq!(diag.address.as_u8(), BH1750_DEFAULT_I2C_ADDR);
    assert!(diag.to_string().contains("Continuous High-Resolution"));

    // Power down
    sensor.power_down().unwrap();
    assert!(!mock.is_powered());
    assert!(sensor.read_lux().unwrap_err().is_powered_down());
    assert!(sensor.read_diagnostics().unwrap_err().is_powered_down());

    // Reset re-awakens and preserves the simulated ambient level
    sensor.reset().unwrap();
    assert!(mock.is_powered());
    assert!((sensor.read_lux().unwrap().lux() - 500.0).abs() < 1e-1);

    // Fault injection
    mock.set_fault(Some("bus arbitration lost".into()));
    let err = sensor.read_lux().unwrap_err();
    assert!(err.is_mock_fault());
    assert!(sensor.read_diagnostics().unwrap_err().is_mock_fault());
    assert!(sensor.power_down().unwrap_err().is_mock_fault());
    assert!(sensor.reset().unwrap_err().is_mock_fault());

    // Clear fault
    mock.set_fault(None);
    mock.set_lux(150.0);
    assert_eq!(sensor.read_lux().unwrap().lux(), 150.0);
}

#[test]
#[should_panic(expected = "mock lux must be finite and non-negative")]
fn mock_set_lux_panics_on_nan() {
    let (_, mock) = LightSensor::new_mocked();
    mock.set_lux(f32::NAN);
}

#[test]
#[should_panic(expected = "mock lux must be finite and non-negative")]
fn mock_set_lux_panics_on_negative() {
    let (_, mock) = LightSensor::new_mocked();
    mock.set_lux(-1.0);
}

#[test]
fn hw_state_settle_math_and_transitions() {
    let now = Instant::now();
    let mut state = LightHwState::new(now);

    // Within 180ms window, settle delay is required
    let delay = state.prepare_read(now).unwrap();
    assert!(delay.is_some());
    assert!(delay.unwrap() <= LightHwState::SETTLE_DURATION);

    // After 180ms window, settle delay is None
    let later = now + Duration::from_millis(200);
    assert_eq!(state.prepare_read(later).unwrap(), None);

    // Power down
    state.record_power_down();
    assert!(state.prepare_read(later).unwrap_err().is_powered_down());

    // Reset re-arms and records new transition
    state.record_wake_or_reset(later);
    let reset_delay = state.prepare_read(later).unwrap();
    assert!(reset_delay.is_some());
    assert!(reset_delay.unwrap() <= LightHwState::SETTLE_DURATION);
}

#[test]
fn light_diagnostics_formatting_and_hardware_info() {
    let (sensor, _) = LightSensor::new_mocked();
    let diag = sensor.read_diagnostics().unwrap();
    let diag_str = diag.to_string();
    assert!(diag_str.contains("BH1750 Light Diagnostics: Bus 1, Addr 0x23"));
    assert!(diag_str.contains("Illuminance"));

    let hw = sensor.hardware_info();
    assert_eq!(hw.bus, 1);
    assert_eq!(hw.address.as_u8(), 0x23);
    assert_eq!(hw.device_path.as_ref(), "/dev/i2c-1");
    assert_eq!(hw.sda_pin.physical_pin.as_u8(), 3);
    assert_eq!(hw.scl_pin.physical_pin.as_u8(), 5);
    assert_eq!(hw.vcc_pin.as_u8(), 1);
    assert_eq!(hw.gnd_pin.as_u8(), 9);
}

#[test]
fn light_error_variants_predicates_and_conversions() {
    let err_i2c = light_test_helpers::make_i2c_error("arbitration lost");
    assert!(err_i2c.is_i2c());
    assert!(!err_i2c.is_powered_down());
    assert_eq!(err_i2c.summary(), "I2C bus error: arbitration lost");
    assert!(err_i2c.to_string().contains("arbitration lost"));
    assert!(err_i2c.source().is_none());

    let err_down = light_test_helpers::make_powered_down_error();
    assert!(err_down.is_powered_down());
    assert_eq!(
        err_down.summary(),
        "sensor is powered down; call reset() before reading"
    );

    let err_cancel = light_test_helpers::make_cancelled_error("aborted");
    assert!(err_cancel.is_cancelled());
    assert_eq!(err_cancel.summary(), "task cancelled: aborted");

    let err_addr = light_test_helpers::make_invalid_address_error(0x42);
    assert!(err_addr.is_invalid_address());
    assert!(err_addr.summary().contains("0x42"));

    let err_illum = light_test_helpers::make_invalid_illuminance_error("negative");
    assert!(err_illum.is_invalid_illuminance());
    assert_eq!(err_illum.summary(), "invalid illuminance: negative");

    let err_mock = light_test_helpers::make_mock_fault_error("test fault");
    assert!(err_mock.is_mock_fault());
    assert_eq!(err_mock.summary(), "injected mock fault: test fault");

    let err_bh = light_test_helpers::from_bh_err(BhError::MeasurementTimeOutOfRange);
    assert!(err_bh.is_i2c());
    assert_eq!(
        err_bh.summary(),
        "I2C bus error: measurement time out of range"
    );
}

#[tokio::test]
async fn async_methods_read_from_mock_sensor() {
    let (sensor, mock) = LightSensor::new_mocked();
    mock.set_lux(350.0);

    let illum = sensor.read_lux_async().await.unwrap();
    assert!((illum.lux() - 350.0).abs() < 1e-1);

    let diag = sensor.read_diagnostics_async().await.unwrap();
    assert!((diag.illuminance.lux() - 350.0).abs() < 1e-1);
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
fn diagnostics_and_power_management() {
    let (sensor, mock) = LightSensor::new_mocked();

    mock.set_lux(240.0);
    let diag = sensor.read_diagnostics().unwrap();
    assert!((diag.illuminance.lux() - 240.0).abs() < 1e-2);

    sensor.power_down().unwrap();
    assert!(sensor.read_diagnostics().unwrap_err().is_powered_down());
    assert!(sensor.read_lux().unwrap_err().is_powered_down());

    sensor.reset().unwrap();
    let reset_diag = sensor.read_diagnostics().unwrap();
    assert!((reset_diag.illuminance.lux() - 240.0).abs() < 1e-2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_sensor_clones_power_and_mode_operations() {
    let (sensor, _mock) = LightSensor::new_mocked();
    let s1 = sensor.clone();
    let s2 = sensor.clone();

    // Async reader on the runtime vs. blocking power cycler on the blocking pool,
    // so both genuinely run in parallel on a multi-threaded runtime.
    let h1 = tokio::spawn(async move {
        for _ in 0..100 {
            if let Err(err) = s1.read_lux_async().await {
                assert!(err.is_powered_down(), "unexpected error: {err}");
            }
        }
    });

    let h2 = tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            s2.power_down()
                .expect("power_down failed during concurrent cycling");
            s2.reset().expect("reset failed during concurrent cycling");
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();
    assert!(
        sensor.read_lux().is_ok(),
        "sensor must end powered after reset"
    );
}

#[tokio::test]
async fn runtime_task_cancellation_mapping() {
    let err: LightError =
        speaker_core::runtime::abort_and_map("ambient light integration task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("ambient light integration task"));
}

#[test]
fn sensor_bus_validation_and_debug_formatting() {
    // Hermetic: invalid bus is rejected before any `/dev/i2c-*` device is opened.
    let err = LightSensor::with_bus_and_address(99, I2cAddress::default()).unwrap_err();
    assert!(err.summary().contains("unsupported I2C bus 99"));
    let (sensor, _) = LightSensor::new_mocked();
    let debug = format!("{sensor:?}");
    assert!(debug.contains("Mock"));
}

#[test]
fn i2c_address_methods_and_formatting() {
    let addr = I2cAddress::default_address();
    assert_eq!(addr.as_u8(), 0x23);
    assert_eq!(addr.as_u16(), 0x0023);
    assert_eq!(addr.to_string(), "0x23");

    let alt = I2cAddress::alternate_address();
    assert_eq!(alt.as_u8(), 0x5C);
    assert_eq!(alt.as_u16(), 0x005C);
    assert_eq!(alt.to_string(), "0x5C");

    assert_eq!(BhAddress::from(addr), BhAddress::Low);
    assert_eq!(BhAddress::from(alt), BhAddress::High);

    let default_hw = I2cHardwareInfo::rpi3_default(addr);
    assert_eq!(default_hw.bus, 1);
    assert_eq!(default_hw.address, addr);
}

#[test]
fn illuminance_bounds_and_millilux_edge_cases() {
    let zero = Illuminance::from_lux(0.0).unwrap();
    assert_eq!(zero.millilux(), 0);
    assert_eq!(zero.foot_candles(), 0.0);
    assert_eq!(zero.to_string(), "0.00 lx");

    let max = Illuminance::from_lux(Illuminance::MAX_LUX).unwrap();
    assert_eq!(max.millilux(), 120_000_000);

    let err_over = Illuminance::from_lux(Illuminance::MAX_LUX + 1.0).unwrap_err();
    assert!(err_over.is_invalid_illuminance());
}

#[test]
fn light_error_backtrace_and_predicates() {
    let err = light_test_helpers::make_i2c_error("test");
    let _ = err.backtrace();
    assert!(err.is_i2c());
    assert!(!err.is_powered_down());
    assert!(!err.is_cancelled());
    assert!(!err.is_invalid_address());
    assert!(!err.is_invalid_illuminance());
    assert!(!err.is_mock_fault());

    let mock_err = light_test_helpers::make_mock_fault_error("bus fault");
    assert!(mock_err.is_mock_fault());
    assert_eq!(mock_err.summary(), "injected mock fault: bus fault");

    let addr_err = light_test_helpers::make_invalid_address_error(0x77);
    assert!(addr_err.is_invalid_address());
    assert!(addr_err.summary().contains("0x77"));

    let illum_err = light_test_helpers::make_invalid_illuminance_error("overflow");
    assert!(illum_err.is_invalid_illuminance());
    assert!(illum_err.summary().contains("overflow"));

    let pwr_err = light_test_helpers::make_powered_down_error();
    assert!(pwr_err.is_powered_down());
    assert!(pwr_err.summary().contains("powered down"));

    let cancel_err = light_test_helpers::make_cancelled_error("aborted");
    assert!(cancel_err.is_cancelled());
    assert!(cancel_err.summary().contains("aborted"));
}

#[tokio::test]
async fn light_sensor_async_diagnostics() {
    let (sensor, _) = LightSensor::new_mocked();
    let diag = sensor.read_diagnostics_async().await.unwrap();
    assert_eq!(diag.bus, BH1750_DEFAULT_I2C_BUS);
    assert_eq!(diag.address, I2cAddress::default_address());
}

#[test]
fn try_rpi3_hardware_info_validates_bus() {
    let err = I2cHardwareInfo::try_rpi3_with_bus_and_address(2, I2cAddress::default()).unwrap_err();
    assert!(err.is_i2c());
    assert!(err.summary().contains("unsupported I2C bus 2"));
}

#[test]
fn mock_sensor_bus_matches_rpi3_default() {
    let (sensor, _) = LightSensor::new_mocked();
    assert_eq!(sensor.i2c_bus(), BH1750_DEFAULT_I2C_BUS);
    let info = sensor.hardware_info();
    assert_eq!(info.bus, BH1750_DEFAULT_I2C_BUS);
}

#[test]
fn sensor_power_down_and_reset() {
    let (sensor, _) = LightSensor::new_mocked();
    sensor.power_down().unwrap();
    assert!(sensor.read_lux().unwrap_err().is_powered_down());
    sensor.reset().unwrap();
    let _ = sensor.read_lux().unwrap();
}
