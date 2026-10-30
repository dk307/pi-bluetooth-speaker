//! Integration tests for BCM and physical pin validation and metadata structures.

use std::error::Error;

use speaker_core::hardware::{BcmPin, GpioPinInfo, PhysicalPin};

#[test]
fn bcm_pin_guards_valid_range() {
    assert_eq!(BcmPin::from_u8(0).unwrap().as_u8(), 0);
    assert_eq!(BcmPin::from_u8(26).unwrap().as_u8(), 26);
    assert_eq!(BcmPin::from_u8(27).unwrap().as_u8(), 27);
    let err = BcmPin::from_u8(28).unwrap_err();
    assert!(err.is_invalid_bcm_pin());
    assert!(!err.is_invalid_physical_pin());
    assert!(err.to_string().contains("invalid BCM GPIO pin 28"));
    let _ = err.backtrace();
    assert!(err.source().is_none());

    assert_eq!(BcmPin::try_from(26).unwrap().as_u8(), 26);
    let _ = BcmPin::try_from(29).unwrap_err();
    assert_eq!(BcmPin::new(26).to_string(), "BCM GPIO 26");
}

#[test]
#[should_panic(expected = "BCM GPIO pin must be in range 0..=27")]
fn bcm_pin_new_panics_on_invalid() {
    let _ = BcmPin::new(28);
}

#[test]
fn physical_pin_guards_valid_range() {
    let low_err = PhysicalPin::from_u8(0).unwrap_err();
    assert!(low_err.is_invalid_physical_pin());
    assert!(!low_err.is_invalid_bcm_pin());
    assert!(
        low_err
            .to_string()
            .contains("invalid physical header pin 0")
    );

    assert_eq!(PhysicalPin::from_u8(1).unwrap().as_u8(), 1);
    assert_eq!(PhysicalPin::from_u8(37).unwrap().as_u8(), 37);
    assert_eq!(PhysicalPin::from_u8(40).unwrap().as_u8(), 40);

    let high_err = PhysicalPin::from_u8(41).unwrap_err();
    assert!(high_err.is_invalid_physical_pin());
    assert_eq!(PhysicalPin::new(37).to_string(), "Physical Pin 37");

    assert_eq!(PhysicalPin::try_from(37).unwrap().as_u8(), 37);
    let _ = PhysicalPin::try_from(0).unwrap_err();
    let _ = PhysicalPin::try_from(42).unwrap_err();
}

#[test]
#[should_panic(expected = "Physical header pin must be in range 1..=40")]
fn physical_pin_new_panics_on_invalid_low() {
    let _ = PhysicalPin::new(0);
}

#[test]
#[should_panic(expected = "Physical header pin must be in range 1..=40")]
fn physical_pin_new_panics_on_invalid_high() {
    let _ = PhysicalPin::new(41);
}

#[test]
fn gpio_pin_info_display_formatting() {
    let info = GpioPinInfo {
        signal_name: "1-Wire Data",
        bcm_pin: BcmPin::from_u8(4).unwrap(),
        physical_pin: PhysicalPin::from_u8(7).unwrap(),
        bus_protocol: "1-Wire (w1-gpio)",
        electrical_config: "dtoverlay=w1-gpio,gpiopin=4",
    };
    let formatted = info.to_string();
    assert!(formatted.contains("1-Wire Data"));
    assert!(formatted.contains("BCM GPIO 4"));
    assert!(formatted.contains("Physical Pin 7"));
    assert!(formatted.contains("1-Wire (w1-gpio)"));
}
