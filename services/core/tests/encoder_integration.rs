//! Integration tests for the EC12 rotary encoder driver.

use std::time::Duration;

use speaker_core::hardware::{
    ENCODER_LED_BLUE_BCM_GPIO_PIN, ENCODER_LED_GREEN_BCM_GPIO_PIN, ENCODER_LED_RED_BCM_GPIO_PIN,
    ENCODER_PHASE_A_BCM_GPIO_PIN, ENCODER_PHASE_B_BCM_GPIO_PIN, ENCODER_PUSH_SW_BCM_GPIO_PIN,
    EncoderDevice, RgbColor, RotationDirection,
};

#[test]
fn hardware_metadata_and_domain_models() {
    assert_eq!(ENCODER_PHASE_A_BCM_GPIO_PIN, 25);
    assert_eq!(ENCODER_PHASE_B_BCM_GPIO_PIN, 24);
    assert_eq!(ENCODER_PUSH_SW_BCM_GPIO_PIN, 22);
    assert_eq!(ENCODER_LED_RED_BCM_GPIO_PIN, 20);
    assert_eq!(ENCODER_LED_GREEN_BCM_GPIO_PIN, 16);
    assert_eq!(ENCODER_LED_BLUE_BCM_GPIO_PIN, 12);

    let (device, _) = EncoderDevice::new_mocked();
    let hw = device.hardware_info();
    assert_eq!(hw.phase_a.bcm_pin.as_u8(), ENCODER_PHASE_A_BCM_GPIO_PIN);
    assert_eq!(hw.phase_a.physical_pin.as_u8(), 22);
    assert_eq!(hw.phase_b.bcm_pin.as_u8(), ENCODER_PHASE_B_BCM_GPIO_PIN);
    assert_eq!(hw.phase_b.physical_pin.as_u8(), 18);
    assert_eq!(hw.push_switch.bcm_pin.as_u8(), ENCODER_PUSH_SW_BCM_GPIO_PIN);
    assert_eq!(hw.push_switch.physical_pin.as_u8(), 15);
    assert_eq!(hw.led_red.physical_pin.as_u8(), 38);
    assert_eq!(hw.led_green.physical_pin.as_u8(), 36);
    assert_eq!(hw.led_blue.physical_pin.as_u8(), 32);
    assert!(hw.phase_a.electrical_config.contains("pull-up"));
    assert!(hw.push_switch.electrical_config.contains("pull-down"));

    assert_eq!(
        RotationDirection::parse_slug("cw").unwrap(),
        RotationDirection::Clockwise
    );
    assert_eq!(
        RotationDirection::parse_slug("ccw").unwrap(),
        RotationDirection::CounterClockwise
    );
    assert_eq!(RgbColor::parse_name("WHITE").unwrap(), RgbColor::WHITE);
    assert!(
        RotationDirection::parse_slug("diagonal")
            .unwrap_err()
            .contains("invalid rotation direction")
    );
    assert!(
        RgbColor::parse_name("nope")
            .unwrap_err()
            .contains("invalid LED color")
    );
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_mock_updates() {
    let (device, mock) = EncoderDevice::new_mocked();
    let device_clone = device.clone();

    let initial = device.read_status_async().await.unwrap();
    assert!(!initial.switch_pressed);
    assert_eq!(initial.led_color, RgbColor::OFF);

    mock.set_switch_pressed(true);
    mock.set_leds(RgbColor::GREEN);

    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        device_clone.read_status_async().await
    });

    let observed = handle.await.unwrap().unwrap();
    assert!(observed.switch_pressed);
    assert_eq!(observed.press_count, 1);
    assert_eq!(observed.led_color, RgbColor::GREEN);
}

#[test]
fn quadrature_rotation_and_switch_event_streaming() {
    let (device, mock) = EncoderDevice::new_mocked();

    // Baseline state 0b11, then one full CCW detent cycle (11 -> 01 -> 00 -> 10 -> 11).
    mock.set_phase_a(false);
    mock.set_phase_b(false);
    mock.set_phase_a(true);
    mock.set_phase_b(true);

    let status = device.read_status().unwrap();
    assert_eq!(status.quadrature_state(), 0b11);
    assert_eq!(status.cw_count, 0);
    assert_eq!(status.ccw_count, 2);

    // One full CW detent cycle (11 -> 10 -> 00 -> 01 -> 11).
    mock.set_phase_b(false);
    mock.set_phase_a(false);
    mock.set_phase_b(true);
    mock.set_phase_a(true);

    let status = device.read_status().unwrap();
    assert_eq!(status.cw_count, 2);
    assert_eq!(status.ccw_count, 2);

    // Press and release the push switch.
    mock.set_switch_pressed(true);
    let status = device.read_status().unwrap();
    assert!(status.switch_pressed);
    assert_eq!(status.press_count, 1);
    assert_eq!(status.release_count, 0);

    mock.set_switch_pressed(false);
    let status = device.read_status().unwrap();
    assert_eq!(status.release_count, 1);
}

#[test]
fn led_writes_and_fault_injection() {
    let (device, mock) = EncoderDevice::new_mocked();

    for color in [
        RgbColor::RED,
        RgbColor::GREEN,
        RgbColor::BLUE,
        RgbColor::WHITE,
        RgbColor::OFF,
    ] {
        device.set_leds(color).unwrap();
        assert_eq!(mock.led_color(), color);
    }

    mock.set_fault(Some("simulated GPIO failure".into()));
    let err = device.read_status().unwrap_err();
    assert!(err.is_mock_fault());
    let err = device.set_leds(RgbColor::RED).unwrap_err();
    assert!(err.is_mock_fault());
}

#[tokio::test]
async fn runtime_task_cancellation_mapping() {
    let err: speaker_core::hardware::EncoderError =
        speaker_core::runtime::abort_and_map("EC12 integration task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("EC12 integration task"));
}

#[test]
fn concurrent_multi_thread_status_reads() {
    let (device, mock) = EncoderDevice::new_mocked();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let d = device.clone();
        handles.push(std::thread::spawn(move || d.read_status().unwrap()));
    }
    mock.set_switch_pressed(true);
    for handle in handles {
        let _ = handle.join().unwrap();
    }
    let status = device.read_status().unwrap();
    assert_eq!(status.press_count, 1);
}
