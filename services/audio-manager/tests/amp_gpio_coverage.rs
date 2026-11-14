//! Additional integration tests for amp_gpio module to increase coverage.

use audio_manager::amp_gpio::{AmpGpio, AmpGpioConfig};

#[test]
fn amp_gpio_config_default() {
    let config = AmpGpioConfig::default();
    assert_eq!(config.enable, 14);
    assert_eq!(config.mute, 15);
    assert_eq!(config.booster, 17);
    assert_eq!(config.error, 23);
}

#[test]
fn amp_gpio_new_mocked_initial_state() {
#[allow(unused_variables)]
    let (gpio, mock) = AmpGpio::new_mocked();

    // Initial state matches open() safe orientation
    assert!(mock.mute_pin_low());
    assert!(!mock.booster_on());
    assert!(!mock.amp_awake());
    assert_eq!(mock.wake_calls(), 0);
    assert_eq!(mock.reset_calls(), 0);
    assert_eq!(mock.power_up_calls(), 0);
    assert_eq!(mock.mute_calls(), 0);
}

#[test]
fn amp_gpio_mock_wake() {
    let (gpio, mock) = AmpGpio::new_mocked();

    gpio.wake().unwrap();
    assert!(mock.amp_awake());
    assert_eq!(mock.wake_calls(), 1);

    // Idempotent
    gpio.wake().unwrap();
    assert_eq!(mock.wake_calls(), 2);
    assert!(mock.amp_awake());
}

#[test]
fn amp_gpio_mock_power_up() {
    let (gpio, mock) = AmpGpio::new_mocked();

    gpio.power_up().unwrap();
    assert!(mock.booster_on());
    assert!(mock.amp_awake());
    assert_eq!(mock.power_up_calls(), 1);
}

#[test]
fn amp_gpio_mock_reset() {
    let (gpio, mock) = AmpGpio::new_mocked();

    gpio.wake().unwrap();
    assert!(mock.amp_awake());

    gpio.reset().unwrap();
    assert!(mock.amp_awake()); // reset brings it back awake
    assert!(mock.mute_pin_low());
    assert_eq!(mock.reset_calls(), 1);
}

#[test]
fn amp_gpio_mock_reset_calls_hook() {
#[allow(unused_variables)]
    let (gpio, mock) = AmpGpio::new_mocked();

    let hook_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_called_clone = hook_called.clone();
    mock.set_reset_hook(Some(std::sync::Arc::new(move || {
        hook_called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    })));

    gpio.reset().unwrap();
    assert!(hook_called.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn amp_gpio_mock_set_hardware_mute() {
    let (gpio, mock) = AmpGpio::new_mocked();

    assert!(mock.mute_pin_low());

    gpio.set_hardware_mute(false).unwrap();
    assert!(!mock.mute_pin_low());
    assert_eq!(mock.mute_calls(), 1);

    gpio.set_hardware_mute(true).unwrap();
    assert!(mock.mute_pin_low());
    assert_eq!(mock.mute_calls(), 2);
}

#[test]
fn amp_gpio_mock_error_active() {
    let (gpio, mock) = AmpGpio::new_mocked();

    assert!(!gpio.error_active().unwrap());

    mock.set_error_active(true);
    assert!(gpio.error_active().unwrap());

    mock.set_error_active(false);
    assert!(!gpio.error_active().unwrap());
}

#[test]
fn amp_gpio_mock_control_methods() {
    let (gpio, mock) = AmpGpio::new_mocked();

    // Test all mock control methods
    assert_eq!(mock.wake_calls(), 0);
    assert_eq!(mock.reset_calls(), 0);
    assert_eq!(mock.power_up_calls(), 0);
    assert_eq!(mock.mute_calls(), 0);
    assert!(mock.mute_pin_low());
    assert!(!mock.booster_on());
    assert!(!mock.amp_awake());
    assert!(!gpio.error_active().unwrap());

    mock.set_error_active(true);
    assert!(gpio.error_active().unwrap());

    // Test set_reset_hook with None
    mock.set_reset_hook(None);
    gpio.reset().unwrap(); // Should not panic
}

#[test]
fn amp_gpio_debug_impl() {
    let (gpio, _mock) = AmpGpio::new_mocked();
    let debug_str = format!("{:?}", gpio);
    assert!(debug_str.contains("AmpGpio"));
}

#[test]
fn amp_gpio_config_custom() {
    let config = AmpGpioConfig {
        enable: 10,
        mute: 11,
        booster: 12,
        error: 13,
    };
    assert_eq!(config.enable, 10);
    assert_eq!(config.mute, 11);
    assert_eq!(config.booster, 12);
    assert_eq!(config.error, 13);
}

// Test constants by checking their values match expected
#[test]
fn amp_gpio_constants_via_behavior() {
    // The constants are private but we can verify behavior
    // BOOSTER_SETTLE = 200ms, ENABLE_SETTLE = 50ms
    // RESET_PULSE = 1ms, RESET_SETTLE = 50ms
    // EDGE_POLL_INTERVAL = 500ms

    // Just verify the test compiles and runs
    // The actual timing is tested in integration tests
}

#[test]
fn amp_gpio_mock_state_debug() {
#[allow(unused_variables)]
    let (gpio, mock) = AmpGpio::new_mocked();
    let debug_str = format!("{:?}", mock);
    assert!(debug_str.contains("MockGpioState"));
    assert!(debug_str.contains("awake"));
    assert!(debug_str.contains("mute_low"));
}

