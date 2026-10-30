//! Integration and hardware driver tests for the EC12 rotary encoder.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use speaker_core::hardware::{
    EncoderDevice, EncoderError, EncoderHardwareInfo, EncoderStatus, RgbColor, RotationDirection,
    encoder_test_helpers,
};

#[test]
fn rotation_direction_slug_roundtrip() {
    assert_eq!(RotationDirection::Clockwise.slug(), "cw");
    assert_eq!(RotationDirection::CounterClockwise.slug(), "ccw");
    assert_eq!(
        RotationDirection::parse_slug("cw").unwrap(),
        RotationDirection::Clockwise
    );
    assert_eq!(
        RotationDirection::parse_slug("CCW").unwrap(),
        RotationDirection::CounterClockwise
    );
    assert_eq!(
        RotationDirection::parse_slug(" clockwise ").unwrap(),
        RotationDirection::Clockwise
    );
    assert_eq!(
        RotationDirection::parse_slug("counter-clockwise").unwrap(),
        RotationDirection::CounterClockwise
    );
    let err = RotationDirection::parse_slug("sideways").unwrap_err();
    assert!(err.contains("invalid rotation direction 'sideways'"));
    assert_eq!(RotationDirection::Clockwise.to_string(), "cw");
}

#[test]
fn rgb_color_names_and_display() {
    assert_eq!(RgbColor::OFF.name(), "off");
    assert_eq!(RgbColor::RED.name(), "red");
    assert_eq!(RgbColor::GREEN.name(), "green");
    assert_eq!(RgbColor::BLUE.name(), "blue");
    assert_eq!(RgbColor::YELLOW.name(), "yellow");
    assert_eq!(RgbColor::CYAN.name(), "cyan");
    assert_eq!(RgbColor::MAGENTA.name(), "magenta");
    assert_eq!(RgbColor::WHITE.name(), "white");
    assert_eq!(RgbColor::YELLOW.to_string(), "yellow");
    assert_eq!(RgbColor::default(), RgbColor::OFF);
}

#[test]
fn rgb_color_parse_name_accepts_case_and_whitespace() {
    assert_eq!(RgbColor::parse_name("WHITE").unwrap(), RgbColor::WHITE);
    assert_eq!(
        RgbColor::parse_name(" Magenta ").unwrap(),
        RgbColor::MAGENTA
    );
    assert_eq!(RgbColor::parse_name("off").unwrap(), RgbColor::OFF);
    let err = RgbColor::parse_name("chartreuse").unwrap_err();
    assert!(err.contains("invalid LED color 'chartreuse'"));
}

#[test]
fn rgb_color_maps_to_led_crate_colors_and_back() {
    for (rgb, led) in [
        (RgbColor::OFF, encoder_test_helpers::LedColor::Black),
        (RgbColor::RED, encoder_test_helpers::LedColor::Red),
        (RgbColor::GREEN, encoder_test_helpers::LedColor::Green),
        (RgbColor::BLUE, encoder_test_helpers::LedColor::Blue),
        (RgbColor::YELLOW, encoder_test_helpers::LedColor::Yellow),
        (RgbColor::CYAN, encoder_test_helpers::LedColor::Cyan),
        (RgbColor::MAGENTA, encoder_test_helpers::LedColor::Magenta),
        (RgbColor::WHITE, encoder_test_helpers::LedColor::White),
    ] {
        assert_eq!(rgb.to_led_color(), led);
        assert_eq!(RgbColor::from_led_color(led), rgb);
    }
}

#[derive(Clone, Default)]
struct SharedFakeChannel(Arc<Mutex<bool>>);

impl encoder_test_helpers::ChannelPin for SharedFakeChannel {
    fn set_channel(&mut self, on: bool) {
        *self.0.lock().expect("test channel lock poisoned") = on;
    }
}

fn channel_lit(channel: &SharedFakeChannel) -> bool {
    *channel.0.lock().expect("test channel lock poisoned")
}

#[test]
fn common_anode_led_drives_channels_per_color() {
    let (red, green, blue) = (
        SharedFakeChannel::default(),
        SharedFakeChannel::default(),
        SharedFakeChannel::default(),
    );
    let mut led = encoder_test_helpers::CommonAnodeLED::new(
        encoder_test_helpers::HalOutputPinAdapter(red.clone()),
        encoder_test_helpers::HalOutputPinAdapter(green.clone()),
        encoder_test_helpers::HalOutputPinAdapter(blue.clone()),
    );

    for (color, r, g, b) in [
        (encoder_test_helpers::LedColor::Red, true, false, false),
        (encoder_test_helpers::LedColor::Green, false, true, false),
        (encoder_test_helpers::LedColor::Blue, false, false, true),
        (encoder_test_helpers::LedColor::Yellow, true, true, false),
        (encoder_test_helpers::LedColor::Cyan, false, true, true),
        (encoder_test_helpers::LedColor::Magenta, true, false, true),
        (encoder_test_helpers::LedColor::White, true, true, true),
        (encoder_test_helpers::LedColor::Black, false, false, false),
    ] {
        led.set(color).unwrap();
        assert_eq!(channel_lit(&red), r, "red channel for {color:?}");
        assert_eq!(channel_lit(&green), g, "green channel for {color:?}");
        assert_eq!(channel_lit(&blue), b, "blue channel for {color:?}");
    }
}

#[test]
fn apply_rotation_counts_detents_per_direction() {
    let shared = encoder_test_helpers::SharedEncoderState::default();
    encoder_test_helpers::apply_rotation(
        &shared,
        encoder_test_helpers::DecoderDirection::Clockwise,
    );
    encoder_test_helpers::apply_rotation(
        &shared,
        encoder_test_helpers::DecoderDirection::Clockwise,
    );
    encoder_test_helpers::apply_rotation(
        &shared,
        encoder_test_helpers::DecoderDirection::CounterClockwise,
    );
    encoder_test_helpers::apply_rotation(&shared, encoder_test_helpers::DecoderDirection::None);

    let status = shared.snapshot(RgbColor::GREEN);
    assert_eq!(status.cw_count, 2);
    assert_eq!(status.ccw_count, 1);
    assert_eq!(status.led_color, RgbColor::GREEN);
}

#[test]
fn rotation_interrupt_callback_without_installed_state_is_noop() {
    encoder_test_helpers::rotation_interrupt_callback(
        "ec12",
        encoder_test_helpers::DecoderDirection::Clockwise,
    );
    encoder_test_helpers::rotation_interrupt_callback(
        "ec12",
        encoder_test_helpers::DecoderDirection::None,
    );
}

#[test]
fn install_or_reuse_shared_keeps_the_first_installation() {
    let first = encoder_test_helpers::install_or_reuse_shared(Arc::new(
        encoder_test_helpers::SharedEncoderState::default(),
    ));
    let second = encoder_test_helpers::install_or_reuse_shared(Arc::new(
        encoder_test_helpers::SharedEncoderState::default(),
    ));
    assert!(
        Arc::ptr_eq(&first, &second),
        "second installation must reuse the process-global counters"
    );
}

#[test]
fn hardware_claim_is_exclusive_and_released_on_drop() {
    {
        let _claim =
            encoder_test_helpers::HardwareClaim::acquire().expect("first claim must succeed");
        let err =
            encoder_test_helpers::HardwareClaim::acquire().expect_err("second claim must fail");
        assert!(err.is_encoder_init());
        assert!(
            err.summary().contains("already claimed"),
            "unexpected message: {}",
            err.summary()
        );
    };
    let _reacquired = encoder_test_helpers::HardwareClaim::acquire()
        .expect("dropping the holder must release the claim");
}

#[test]
fn apply_switch_level_counts_transitions_only() {
    let shared = encoder_test_helpers::SharedEncoderState::default();
    encoder_test_helpers::apply_switch_level(&shared, true);
    encoder_test_helpers::apply_switch_level(&shared, true);
    encoder_test_helpers::apply_switch_level(&shared, false);
    encoder_test_helpers::apply_switch_level(&shared, false);
    encoder_test_helpers::apply_switch_level(&shared, true);

    let status = shared.snapshot(RgbColor::OFF);
    assert!(status.switch_pressed);
    assert_eq!(status.press_count, 2);
    assert_eq!(status.release_count, 1);
}

#[test]
fn encoder_status_display() {
    let status = EncoderStatus {
        switch_pressed: true,
        cw_count: 3,
        ccw_count: 2,
        press_count: 1,
        release_count: 0,
        led_color: RgbColor::GREEN,
    };
    let text = status.to_string();
    assert!(text.contains("CW 3 | CCW 2"));
    assert!(text.contains("Press 1 / Release 0"));
    assert!(text.contains("pressed"));
    assert!(text.contains("RGB Indicator  : green"));
}

#[test]
fn hardware_info_pin_metadata_matches_baseline() {
    let info = EncoderHardwareInfo::rpi3_default();
    assert_eq!(info.phase_a.bcm_pin.as_u8(), 25);
    assert_eq!(info.phase_a.physical_pin.as_u8(), 22);
    assert!(info.phase_a.electrical_config.contains("pull-up"));
    assert_eq!(info.phase_b.bcm_pin.as_u8(), 24);
    assert_eq!(info.phase_b.physical_pin.as_u8(), 18);
    assert_eq!(info.push_switch.bcm_pin.as_u8(), 22);
    assert_eq!(info.push_switch.physical_pin.as_u8(), 15);
    assert!(info.push_switch.electrical_config.contains("pull-down"));
    assert_eq!(info.led_red.bcm_pin.as_u8(), 20);
    assert_eq!(info.led_red.physical_pin.as_u8(), 38);
    assert_eq!(info.led_green.bcm_pin.as_u8(), 16);
    assert_eq!(info.led_green.physical_pin.as_u8(), 36);
    assert_eq!(info.led_blue.bcm_pin.as_u8(), 12);
    assert_eq!(info.led_blue.physical_pin.as_u8(), 32);
    assert!(info.led_red.electrical_config.contains("330Ω"));
    assert!(info.led_green.electrical_config.contains("180Ω"));
    assert!(info.led_blue.electrical_config.contains("180Ω"));
}

#[test]
fn error_predicates_and_summary() {
    let gpio_err = EncoderError::from(rppal::gpio::Error::PinUsed(25));
    assert!(gpio_err.is_gpio());
    assert!(!gpio_err.is_encoder_init());
    assert!(!gpio_err.is_cancelled());
    assert!(!gpio_err.is_mock_fault());
    assert!(gpio_err.summary().contains("GPIO error:"));

    let init_err = EncoderError::from(anyhow::Error::msg("decoder wiring failed"));
    assert!(init_err.is_encoder_init());
    assert!(
        init_err
            .summary()
            .contains("encoder initialization failed: decoder wiring failed")
    );

    let cancelled = encoder_test_helpers::make_cancelled_error("task went away");
    assert!(cancelled.is_cancelled());
    assert!(
        cancelled
            .summary()
            .contains("task cancelled: task went away")
    );

    let fault = encoder_test_helpers::make_mock_fault_error("injected");
    assert!(fault.is_mock_fault());
    assert!(fault.summary().contains("injected mock fault: injected"));
}

#[test]
fn gpio_error_source_and_conversion() {
    let err = EncoderError::from(rppal::gpio::Error::PinNotAvailable(99));
    assert!(err.is_gpio());
    let source =
        std::error::Error::source(&err).and_then(|s| s.downcast_ref::<rppal::gpio::Error>());
    assert!(matches!(
        source,
        Some(rppal::gpio::Error::PinNotAvailable(99))
    ));
}

#[test]
fn encoder_init_error_flattens_anyhow_message() {
    let err = EncoderError::from(anyhow::Error::msg("decoder init boom"));
    assert!(err.is_encoder_init());
    assert!(
        err.summary()
            .contains("encoder initialization failed: decoder init boom")
    );
    assert!(std::error::Error::source(&err).is_none());
}

#[test]
fn encoder_init_error_preserves_anyhow_causal_chain() {
    let chained = anyhow::Error::msg("line request failed").context("decoder init");
    let err = EncoderError::from(chained);
    assert!(
        err.summary()
            .contains("encoder initialization failed: decoder init: line request failed")
    );
}

#[test]
fn mock_simulate_rotation_counts_detents() {
    let (device, mock) = EncoderDevice::new_mocked();

    let status = device.read_status().unwrap();
    assert!(!status.switch_pressed);
    assert_eq!(status.cw_count, 0);
    assert_eq!(status.ccw_count, 0);
    assert_eq!(status.led_color, RgbColor::OFF);

    mock.simulate_rotation(RotationDirection::CounterClockwise, 3);
    mock.simulate_rotation(RotationDirection::Clockwise, 2);
    let status = device.read_status().unwrap();
    assert_eq!(status.cw_count, 2);
    assert_eq!(status.ccw_count, 3);
}

#[test]
fn mock_switch_press_release_counts() {
    let (device, mock) = EncoderDevice::new_mocked();
    mock.set_switch_pressed(true);
    mock.set_switch_pressed(true);
    mock.set_switch_pressed(false);
    mock.set_switch_pressed(false);
    mock.set_switch_pressed(true);

    let status = device.read_status().unwrap();
    assert!(status.switch_pressed);
    assert_eq!(status.press_count, 2);
    assert_eq!(status.release_count, 1);
}

#[test]
fn mock_set_leds_latches_color() {
    let mock_leds = {
        let (device, mock) = EncoderDevice::new_mocked();
        mock.set_leds(RgbColor::CYAN);
        device.set_leds(RgbColor::MAGENTA).unwrap();
        (device.read_status().unwrap().led_color, mock)
    };
    assert_eq!(mock_leds.0, RgbColor::MAGENTA);
    assert_eq!(mock_leds.1.led_color(), RgbColor::MAGENTA);
}

#[test]
fn mock_fault_fails_all_operations() {
    let (device, mock) = EncoderDevice::new_mocked();
    mock.set_fault(Some("simulated GPIO failure".into()));

    let status_err = device.read_status().unwrap_err();
    assert!(status_err.is_mock_fault());
    assert!(status_err.summary().contains("simulated GPIO failure"));

    let leds_err = device.set_leds(RgbColor::WHITE).unwrap_err();
    assert!(leds_err.is_mock_fault());
    assert!(leds_err.summary().contains("simulated GPIO failure"));

    mock.set_fault(None);
    device.read_status().unwrap();
    device.set_leds(RgbColor::OFF).unwrap();
}

#[test]
fn mock_led_color_accessor_tracks_latest_write() {
    let (_, mock) = EncoderDevice::new_mocked();
    assert_eq!(mock.led_color(), RgbColor::OFF);
    mock.set_leds(RgbColor::YELLOW);
    assert_eq!(mock.led_color(), RgbColor::YELLOW);
}

#[tokio::test]
async fn async_methods_read_from_mock_device() {
    let (device, mock) = EncoderDevice::new_mocked();
    mock.set_switch_pressed(true);

    let status = device.read_status_async().await.unwrap();
    assert!(status.switch_pressed);
    assert_eq!(status.press_count, 1);

    device.set_leds_async(RgbColor::BLUE).await.unwrap();
    assert_eq!(mock.led_color(), RgbColor::BLUE);
}

#[tokio::test]
async fn cancelled_blocking_task_maps_to_encoder_error_cancelled() {
    let err: EncoderError = speaker_core::runtime::abort_and_map("EC12 test task").await;
    assert!(err.is_cancelled());
    assert!(err.summary().contains("EC12 test task"));
}

#[test]
fn debug_formatting_reports_mock_backend() {
    let (device, _mock) = EncoderDevice::new_mocked();
    let debug = format!("{device:?}");
    assert!(debug.contains("Mock"));
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
