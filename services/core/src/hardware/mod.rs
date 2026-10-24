//! Hardware abstractions for Raspberry Pi peripherals, sensors, and actuators.
//!
//! This module provides strongly-typed, mockable interfaces for the physical
//! hardware attached to the Raspberry Pi 3 Model B 40-pin GPIO header and
//! peripheral buses (1-Wire, I2C-1, PWM, and GPIO interrupts).
//!
//! Every hardware driver supports three execution modes:
//! 1. **Core Service Runtime:** Asynchronous, non-blocking operations integrated
//!    with the Tokio runtime (`spawn_blocking` for slow sysfs/bus reads).
//! 2. **Unit & Integration Testing:** Inherent mocking via `new_mocked()`
//!    controllers and configurable sysfs paths so tests run on any host without
//!    physical Raspberry Pi hardware.
//! 3. **Diagnostic CLI Utilities:** Direct synchronous inspection and hardware
//!    pinout reporting for standalone diagnostic binaries.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fmt::{self, Display, Formatter};

macro_rules! impl_canonical_error_display {
    ($err:ident) => {
        impl $err {
            /// Returns a single-line error description without any captured backtrace.
            ///
            /// Safe for embedding inside machine-oriented `NDJSON` telemetry streams where
            /// multi-line stack traces when `RUST_BACKTRACE=1` would bloat or corrupt line records.
            #[must_use]
            pub fn summary(&self) -> String {
                struct Summary<'a>(&'a $err);
                impl std::fmt::Display for Summary<'_> {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        self.0.fmt_kind(f)
                    }
                }
                Summary(self).to_string()
            }
        }

        impl std::fmt::Display for $err {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.fmt_kind(f)?;
                if self.backtrace.status() == std::backtrace::BacktraceStatus::Captured {
                    write!(f, "\nBacktrace:\n{}", self.backtrace)?;
                }
                Ok(())
            }
        }
    };
}
pub(crate) use impl_canonical_error_display;

pub(crate) mod light;
pub(crate) mod temp;

#[doc(inline)]
pub use light::{
    BH1750_ACCURACY_FACTOR, BH1750_ALT_I2C_ADDR, BH1750_DEFAULT_I2C_ADDR, BH1750_DEFAULT_I2C_BUS,
    I2cAddress, I2cHardwareInfo, Illuminance, LightDiagnostics, LightError, LightSensor,
    LightSensorMock, MeasurementMode,
};

#[doc(inline)]
pub use temp::{
    DS18B20_BCM_GPIO_PIN, DS18B20_DTOVERLAY, DS18B20_PHYSICAL_PIN, DS18B20_SYSFS_BASE_PATH,
    PowerSupplyMode, SensorDiagnostics, SensorId, SensorReading, SocThermal, TempError, TempSensor,
    TempSensorMock, Temperature, parse_w1_slave,
};

/// Maximum valid BCM GPIO number on the Raspberry Pi 40-pin header.
const MAX_BCM_PIN: u8 = 27;

/// Minimum physical pin on the 40-pin header.
const MIN_PHYSICAL_PIN: u8 = 1;

/// Maximum physical pin on the 40-pin header.
const MAX_PHYSICAL_PIN: u8 = 40;

#[derive(Debug)]
pub(crate) enum PinErrorKind {
    InvalidBcmPin { pin: u8 },
    InvalidPhysicalPin { pin: u8 },
}

/// Error returned when a GPIO or board header pin number fails range validation.
#[derive(Debug)]
pub struct PinError {
    kind: PinErrorKind,
    backtrace: Backtrace,
}

impl PinError {
    pub(crate) fn new(kind: PinErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns `true` if a BCM GPIO number was outside `0..=27`.
    #[must_use]
    pub const fn is_invalid_bcm_pin(&self) -> bool {
        matches!(self.kind, PinErrorKind::InvalidBcmPin { .. })
    }

    /// Returns `true` if a physical board pin number was outside `1..=40`.
    #[must_use]
    pub const fn is_invalid_physical_pin(&self) -> bool {
        matches!(self.kind, PinErrorKind::InvalidPhysicalPin { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl Display for PinError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            PinErrorKind::InvalidBcmPin { pin } => {
                write!(
                    f,
                    "invalid BCM GPIO pin {pin}: must be in range 0..={MAX_BCM_PIN}"
                )?;
            }
            PinErrorKind::InvalidPhysicalPin { pin } => {
                write!(
                    f,
                    "invalid physical header pin {pin}: must be in range {MIN_PHYSICAL_PIN}..={MAX_PHYSICAL_PIN}"
                )?;
            }
        }
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\nBacktrace:\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl std::error::Error for PinError {}

/// Broadcom SOC channel (BCM) GPIO pin number (`0..=27`).
///
/// Guards the invariant that the pin number corresponds to a valid user-accessible
/// BCM GPIO line on the Raspberry Pi 40-pin header.
///
/// # Examples
/// ```
/// use speaker_core::hardware::BcmPin;
///
/// let pin = BcmPin::from_u8(26)?;
/// assert_eq!(pin.as_u8(), 26);
/// # Ok::<(), speaker_core::hardware::PinError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BcmPin(u8);

impl BcmPin {
    /// Creates a `BcmPin` from a raw pin number in `0..=27`.
    ///
    /// # Panics
    /// Panics if `pin > 27`. Prefer [`BcmPin::from_u8`] for fallible construction.
    #[must_use]
    pub const fn new(pin: u8) -> Self {
        assert!(pin <= MAX_BCM_PIN, "BCM GPIO pin must be in range 0..=27");
        Self(pin)
    }

    /// Fallibly creates a `BcmPin` from a raw pin number in `0..=27`.
    ///
    /// # Errors
    /// Returns [`PinError`] with [`PinError::is_invalid_bcm_pin`] if `pin > 27`.
    pub fn from_u8(pin: u8) -> Result<Self, PinError> {
        if pin <= MAX_BCM_PIN {
            Ok(Self(pin))
        } else {
            Err(PinError::new(PinErrorKind::InvalidBcmPin { pin }))
        }
    }

    /// Returns the underlying BCM GPIO number.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for BcmPin {
    type Error = PinError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value)
    }
}

impl Display for BcmPin {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "BCM GPIO {}", self.0)
    }
}

/// Physical board header pin number on the 40-pin Raspberry Pi header (`1..=40`).
///
/// # Examples
/// ```
/// use speaker_core::hardware::PhysicalPin;
///
/// let pin = PhysicalPin::from_u8(37)?;
/// assert_eq!(pin.as_u8(), 37);
/// # Ok::<(), speaker_core::hardware::PinError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalPin(u8);

impl PhysicalPin {
    /// Creates a `PhysicalPin` from a raw board pin number in `1..=40`.
    ///
    /// # Panics
    /// Panics if `pin` is outside `1..=40`. Prefer [`PhysicalPin::from_u8`] for fallible construction.
    #[must_use]
    pub const fn new(pin: u8) -> Self {
        assert!(
            pin >= MIN_PHYSICAL_PIN && pin <= MAX_PHYSICAL_PIN,
            "Physical header pin must be in range 1..=40"
        );
        Self(pin)
    }

    /// Fallibly creates a `PhysicalPin` from a raw pin number in `1..=40`.
    ///
    /// # Errors
    /// Returns [`PinError`] with [`PinError::is_invalid_physical_pin`] if `pin` is outside `1..=40`.
    pub fn from_u8(pin: u8) -> Result<Self, PinError> {
        if (MIN_PHYSICAL_PIN..=MAX_PHYSICAL_PIN).contains(&pin) {
            Ok(Self(pin))
        } else {
            Err(PinError::new(PinErrorKind::InvalidPhysicalPin { pin }))
        }
    }

    /// Returns the underlying 1-based physical header pin number.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for PhysicalPin {
    type Error = PinError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value)
    }
}

impl Display for PhysicalPin {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Physical Pin {}", self.0)
    }
}

/// Describes a GPIO header pin assignment and electrical configuration for a peripheral.
///
/// Used by both the core service diagnostics screen and standalone CLI utilities to
/// report exact hardware wiring details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpioPinInfo {
    /// Functional name of the signal or peripheral line.
    pub signal_name: &'static str,
    /// Broadcom SOC channel (BCM) GPIO number.
    pub bcm_pin: BcmPin,
    /// Physical 40-pin header pin number.
    pub physical_pin: PhysicalPin,
    /// Hardware bus or protocol name (e.g., `"1-Wire (w1)"`, `"I2C-1"`).
    pub bus_protocol: &'static str,
    /// Electrical configuration and boot overlay details.
    pub electrical_config: &'static str,
}

impl Display for GpioPinInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} ({}) | Bus: {} | Config: {}",
            self.signal_name,
            self.bcm_pin,
            self.physical_pin,
            self.bus_protocol,
            self.electrical_config
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

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
}
