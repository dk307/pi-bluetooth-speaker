//! BH1750FVI digital ambient light sensor driver with typed domain models and inherent mocking.
//!
//! Provides strongly-typed, non-blocking interfaces for the ROHM BH1750FVI on the
//! Raspberry Pi I2C-1 bus (`/dev/i2c-1`) in continuous high-resolution mode (1 lx resolution).
//!
//! # Hardware Configuration
//! - **Bus:** I2C-1 (`/dev/i2c-1`)
//! - **SDA:** BCM GPIO 2 (Physical Pin 3)
//! - **SCL:** BCM GPIO 3 (Physical Pin 5)
//! - **Power:** 3.3V DC (Physical Pin 1)
//! - **Ground:** GND (Physical Pin 9)
//! - **I2C Address:** `0x23` (ADDR grounded / default) or `0x5C` (ADDR to 3.3V)
//!
//! # Architecture
//! - [`LightSensor`] is cheaply cloneable (`Arc`-backed handle) and `Send + Sync`,
//!   providing non-blocking asynchronous APIs.
//! - Companion [`LightSensorMock`] enables deterministic testing without physical hardware.
//! - Strongly typed domain models [`Illuminance`] and [`I2cAddress`].

use std::backtrace::Backtrace;
use std::fmt::{self, Display, Formatter};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use bh1750_embedded::{Address as BhAddress, Bh1750, Error as BhError, Resolution as BhResolution};
use rppal::i2c::I2c;

use crate::runtime::{FromJoinError, run_blocking};

use super::{BcmPin, GpioPinInfo, PhysicalPin};

/// BH1750 optical measurement accuracy coefficient from datasheet: lux = raw / 1.2.
pub const BH1750_ACCURACY_FACTOR: f32 = 1.2;

/// Default 7-bit I2C slave address for the BH1750 (ADDR pin pulled low / GND).
pub const BH1750_DEFAULT_I2C_ADDR: u8 = 0x23;

/// Alternate 7-bit I2C slave address for the BH1750 (ADDR pin tied to VCC).
pub const BH1750_ALT_I2C_ADDR: u8 = 0x5C;

/// Default I2C bus device on Raspberry Pi 3.
pub const BH1750_DEFAULT_I2C_BUS: u8 = 1;

/// Validated 7-bit I2C address for the BH1750 ambient light sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct I2cAddress(u8);

impl I2cAddress {
    /// Returns the default BH1750 I2C address (`0x23`, ADDR pin grounded).
    #[must_use]
    pub const fn default_address() -> Self {
        Self(BH1750_DEFAULT_I2C_ADDR)
    }

    /// Returns the alternate BH1750 I2C address (`0x5C`, ADDR pin tied to VCC).
    #[must_use]
    pub const fn alternate_address() -> Self {
        Self(BH1750_ALT_I2C_ADDR)
    }

    const fn is_valid(addr: u8) -> bool {
        addr == BH1750_DEFAULT_I2C_ADDR || addr == BH1750_ALT_I2C_ADDR
    }

    /// Creates an [`I2cAddress`] validating that the address matches BH1750 hardware specs.
    ///
    /// # Errors
    /// Returns [`LightError`] if `addr` is neither `0x23` nor `0x5C`.
    pub fn new(addr: u8) -> Result<Self, LightError> {
        if Self::is_valid(addr) {
            Ok(Self(addr))
        } else {
            Err(LightError::invalid_address(addr))
        }
    }

    /// Parses a hexadecimal string (`0x23`, `0X5C`, `23`) into a validated [`I2cAddress`].
    ///
    /// Rejects `+` or `-` signs (both before and after the optional `0x`/`0X` prefix) as
    /// non-standard in hardware address specifications, returning a soft user-facing error
    /// message without capturing a stack backtrace.
    ///
    /// # Errors
    /// Returns a descriptive user-facing error message if `s` is signed, non-hexadecimal,
    /// or does not match a valid BH1750 slave address (`0x23` or `0x5C`).
    pub fn parse_hex(s: &str) -> Result<Self, String> {
        let clean = s.trim();
        let digits = clean
            .strip_prefix("0x")
            .or_else(|| clean.strip_prefix("0X"))
            .unwrap_or(clean);
        if clean.starts_with(['+', '-']) || digits.starts_with(['+', '-']) {
            return Err(format!(
                "invalid hex address '{s}': sign characters are not allowed"
            ));
        }
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "invalid hex address '{s}': expected hexadecimal digits"
            ));
        }
        let addr = u8::from_str_radix(digits, 16)
            .map_err(|e| format!("invalid hex address '{s}': {e}"))?;
        if Self::is_valid(addr) {
            Ok(Self(addr))
        } else {
            Err(format!(
                "invalid BH1750 I2C address '{s}' (expected 0x{BH1750_DEFAULT_I2C_ADDR:02X} or 0x{BH1750_ALT_I2C_ADDR:02X})"
            ))
        }
    }

    /// Returns the raw 7-bit slave address as `u8`.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }

    /// Returns the raw 7-bit slave address as `u16` for I2C driver APIs.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0 as u16
    }
}

impl TryFrom<u8> for I2cAddress {
    type Error = LightError;

    fn try_from(addr: u8) -> Result<Self, Self::Error> {
        Self::new(addr)
    }
}

impl Default for I2cAddress {
    fn default() -> Self {
        Self::default_address()
    }
}

impl Display for I2cAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02X}", self.0)
    }
}

impl From<I2cAddress> for BhAddress {
    fn from(addr: I2cAddress) -> Self {
        match addr.as_u8() {
            BH1750_DEFAULT_I2C_ADDR => Self::Low,
            BH1750_ALT_I2C_ADDR => Self::High,
            other => Self::Custom(other),
        }
    }
}

/// Strongly-typed illuminance measurement in lux (lx).
///
/// Guards invariants: non-negative, non-NaN, and within valid sensor limits.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Illuminance {
    lux: f32,
}

impl Illuminance {
    /// Maximum valid illuminance reading supported by the BH1750FVI datasheet (120,000 lux).
    pub const MAX_LUX: f32 = 120_000.0;

    /// Creates an [`Illuminance`] from a raw float in lux.
    ///
    /// Accepts values across the full BH1750FVI datasheet range (`0.0..=Self::MAX_LUX`).
    ///
    /// # Errors
    /// Returns [`LightError`] if `lux` is negative, NaN, infinite, or exceeds [`Self::MAX_LUX`].
    pub fn from_lux(lux: f32) -> Result<Self, LightError> {
        if !lux.is_finite() || lux < 0.0 || lux > Self::MAX_LUX {
            return Err(LightError::invalid_illuminance(format!(
                "lux value must be finite and between 0.0 and {}, got {lux}",
                Self::MAX_LUX
            )));
        }
        Ok(Self { lux })
    }

    /// Converts a raw 16-bit register word into [`Illuminance`] using the standard BH1750 accuracy factor.
    ///
    /// Note: primitive `raw as f32` is used here because `f32::from(u16)` is not `const fn` in stable Rust.
    ///
    /// # Panics
    /// Panics in debug builds if the converted lux value falls outside `0.0..=Self::MAX_LUX`.
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        let lux = (raw as f32) / BH1750_ACCURACY_FACTOR;
        debug_assert!(
            lux >= 0.0 && lux <= Self::MAX_LUX,
            "raw conversion breached Illuminance invariant"
        );
        Self { lux }
    }

    /// Returns illuminance in lux (lx).
    #[must_use]
    pub const fn lux(self) -> f32 {
        self.lux
    }

    /// Returns illuminance in millilux (mlx, 1 lux = 1,000 millilux).
    ///
    /// Since [`Illuminance`] guarantees `0.0 <= lux <= Self::MAX_LUX` (`120_000.0`),
    /// the maximum millilux value is `120_000_000`, which safely fits in `u32`
    /// without loss or overflow.
    #[must_use]
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "lux is guaranteed 0.0..=120_000.0, so mlx <= 120_000_000 which fits in u32"
    )]
    pub fn millilux(self) -> u32 {
        ((self.lux * 1000.0).round()) as u32
    }

    /// Returns illuminance converted to foot-candles (fc).
    #[must_use]
    pub const fn foot_candles(self) -> f32 {
        crate::diag::foot_candles(self.lux)
    }
}

impl Display for Illuminance {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2} lx", self.lux)
    }
}

/// Extended diagnostic report for the BH1750 ambient light sensor.
///
/// A report is only produced for an active sensor; a powered-down sensor yields
/// a [`LightError`] with [`LightError::is_powered_down`] instead.
#[derive(Debug, Clone)]
pub struct LightDiagnostics {
    /// Measured illuminance.
    pub illuminance: Illuminance,
    /// I2C slave address.
    pub address: I2cAddress,
    /// I2C bus number.
    pub bus: u8,
}

impl Display for LightDiagnostics {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[BH1750 Light Diagnostics: Bus {}, Addr {}]",
            self.bus, self.address
        )?;
        writeln!(
            f,
            "  • Illuminance    : {} ({:.2} fc)",
            self.illuminance,
            self.illuminance.foot_candles()
        )?;
        write!(
            f,
            "  • Operating Mode : Continuous High-Resolution (1 lx, ~120 ms)"
        )
    }
}

/// Hardware connection and GPIO pinout metadata for the I2C-1 bus.
#[derive(Debug, Clone)]
pub struct I2cHardwareInfo {
    /// Linux I2C bus number.
    pub bus: u8,
    /// I2C slave device address.
    pub address: I2cAddress,
    /// Kernel device path.
    pub device_path: Box<str>,
    /// SDA data line GPIO descriptor.
    pub sda_pin: GpioPinInfo,
    /// SCL clock line GPIO descriptor.
    pub scl_pin: GpioPinInfo,
    /// Hardware VCC pin location on 40-pin header.
    pub vcc_pin: PhysicalPin,
    /// Hardware GND pin location on 40-pin header.
    pub gnd_pin: PhysicalPin,
}

/// Validates that `bus` corresponds to the Raspberry Pi 3 BSC1 controller (I2C-1).
fn validate_rpi3_bus(bus: u8) -> Result<(), LightError> {
    if bus != BH1750_DEFAULT_I2C_BUS {
        return Err(LightError::i2c(format!(
            "unsupported I2C bus {bus}: hardware driver models Raspberry Pi 3 BSC1 (i2c-{BH1750_DEFAULT_I2C_BUS}) only"
        )));
    }
    Ok(())
}

impl I2cHardwareInfo {
    fn new_rpi3_unchecked(bus: u8, address: I2cAddress) -> Self {
        const SDA_BCM: BcmPin = BcmPin::new(2);
        const SDA_PHYS: PhysicalPin = PhysicalPin::new(3);
        const SCL_BCM: BcmPin = BcmPin::new(3);
        const SCL_PHYS: PhysicalPin = PhysicalPin::new(5);
        const VCC_PHYS: PhysicalPin = PhysicalPin::new(1);
        const GND_PHYS: PhysicalPin = PhysicalPin::new(9);

        let device_path = format!("/dev/i2c-{bus}").into_boxed_str();

        Self {
            bus,
            address,
            device_path,
            sda_pin: GpioPinInfo {
                bcm_pin: SDA_BCM,
                physical_pin: SDA_PHYS,
                signal_name: "I2C-1 Data (SDA)",
                bus_protocol: "I2C (i2c-dev / Broadcom BSC1)",
                electrical_config: "Internal 1.8k pull-up to 3.3V",
            },
            scl_pin: GpioPinInfo {
                bcm_pin: SCL_BCM,
                physical_pin: SCL_PHYS,
                signal_name: "I2C-1 Clock (SCL)",
                bus_protocol: "I2C (i2c-dev / Broadcom BSC1)",
                electrical_config: "Internal 1.8k pull-up to 3.3V",
            },
            vcc_pin: VCC_PHYS,
            gnd_pin: GND_PHYS,
        }
    }

    /// Returns the standard Raspberry Pi 3 I2C-1 pinout for the BH1750 sensor.
    #[must_use]
    pub fn rpi3_default(address: I2cAddress) -> Self {
        Self::new_rpi3_unchecked(BH1750_DEFAULT_I2C_BUS, address)
    }

    /// Returns Raspberry Pi 3 I2C pinout metadata for `bus` and `address`.
    ///
    /// # Errors
    /// Returns [`LightError`] if `bus` is not [`BH1750_DEFAULT_I2C_BUS`] (`1`).
    pub fn try_rpi3_with_bus_and_address(bus: u8, address: I2cAddress) -> Result<Self, LightError> {
        validate_rpi3_bus(bus)?;
        Ok(Self::new_rpi3_unchecked(bus, address))
    }
}

/// Underlying detail for an I2C bus failure.
#[derive(Debug)]
pub(crate) enum I2cDetail {
    Message(Box<str>),
    Source(rppal::i2c::Error),
}

/// Categorized failure kinds for ambient light sensor operations.
#[derive(Debug)]
pub(crate) enum LightErrorKind {
    I2c(I2cDetail),
    PowerDown,
    Cancelled { message: Box<str> },
    InvalidAddress { address: u8 },
    InvalidIlluminance { message: Box<str> },
    MockFault { message: Box<str> },
}

/// Canonical error returned by ambient light sensor drivers and utilities.
#[derive(Debug)]
pub struct LightError {
    kind: LightErrorKind,
    backtrace: Backtrace,
}

impl LightError {
    pub(crate) fn new(kind: LightErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn i2c(message: impl Into<Box<str>>) -> Self {
        Self::new(LightErrorKind::I2c(I2cDetail::Message(message.into())))
    }

    pub(crate) fn from_i2c_err(err: rppal::i2c::Error) -> Self {
        Self::new(LightErrorKind::I2c(I2cDetail::Source(err)))
    }

    pub(crate) fn from_bh_err(err: BhError<rppal::i2c::Error>) -> Self {
        match err {
            BhError::I2c(i2c_err) => Self::from_i2c_err(i2c_err),
            BhError::MeasurementTimeOutOfRange => Self::i2c("measurement time out of range"),
        }
    }

    pub(crate) fn powered_down() -> Self {
        Self::new(LightErrorKind::PowerDown)
    }

    pub(crate) fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(LightErrorKind::Cancelled {
            message: message.into(),
        })
    }

    pub(crate) fn invalid_address(address: u8) -> Self {
        Self::new(LightErrorKind::InvalidAddress { address })
    }

    pub(crate) fn invalid_illuminance(message: impl Into<Box<str>>) -> Self {
        Self::new(LightErrorKind::InvalidIlluminance {
            message: message.into(),
        })
    }

    pub(crate) fn mock_fault(message: impl Into<Box<str>>) -> Self {
        Self::new(LightErrorKind::MockFault {
            message: message.into(),
        })
    }

    /// Returns `true` if this error was caused by an I2C communication or bus failure.
    #[must_use]
    pub const fn is_i2c(&self) -> bool {
        matches!(self.kind, LightErrorKind::I2c(_))
    }

    /// Returns `true` if this error occurred because the sensor is currently powered down.
    #[must_use]
    pub const fn is_powered_down(&self) -> bool {
        matches!(self.kind, LightErrorKind::PowerDown)
    }

    /// Returns `true` if this error was caused by task cancellation or runtime shutdown.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, LightErrorKind::Cancelled { .. })
    }

    /// Returns `true` if this error was caused by an invalid I2C address.
    #[must_use]
    pub const fn is_invalid_address(&self) -> bool {
        matches!(self.kind, LightErrorKind::InvalidAddress { .. })
    }

    /// Returns `true` if this error was caused by an invalid illuminance calculation or value.
    #[must_use]
    pub const fn is_invalid_illuminance(&self) -> bool {
        matches!(self.kind, LightErrorKind::InvalidIlluminance { .. })
    }

    /// Returns `true` if this error was caused by an injected mock fault.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, LightErrorKind::MockFault { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LightErrorKind::I2c(I2cDetail::Message(msg)) => write!(f, "I2C bus error: {msg}"),
            LightErrorKind::I2c(I2cDetail::Source(err)) => write!(f, "I2C bus error: {err}"),
            LightErrorKind::PowerDown => {
                write!(f, "sensor is powered down; call reset() before reading")
            }
            LightErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
            LightErrorKind::InvalidAddress { address } => {
                write!(
                    f,
                    "invalid BH1750 I2C address: 0x{address:02X} (expected 0x{BH1750_DEFAULT_I2C_ADDR:02X} or 0x{BH1750_ALT_I2C_ADDR:02X})"
                )
            }
            LightErrorKind::InvalidIlluminance { message } => {
                write!(f, "invalid illuminance: {message}")
            }
            LightErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
        }
    }
}

super::impl_canonical_error_display!(LightError);

impl std::error::Error for LightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            LightErrorKind::I2c(I2cDetail::Source(err)) => Some(err),
            _ => None,
        }
    }
}

impl From<rppal::i2c::Error> for LightError {
    fn from(err: rppal::i2c::Error) -> Self {
        Self::from_i2c_err(err)
    }
}

impl FromJoinError for LightError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

/// Controllable in-memory mock for [`LightSensor`].
#[derive(Debug, Clone)]
pub struct LightSensorMock {
    state: Arc<RwLock<MockState>>,
}

#[derive(Debug)]
struct MockState {
    lux: f32,
    powered: bool,
    fault: Option<Box<str>>,
}

impl LightSensorMock {
    fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockState {
                lux: 100.0,
                powered: true,
                fault: None,
            })),
        }
    }

    /// Sets the simulated illuminance in lux.
    ///
    /// # Panics
    /// Panics if `lux` is negative or not finite (`NaN` or infinite), or if the mock state lock is poisoned.
    pub fn set_lux(&self, lux: f32) {
        assert!(
            lux.is_finite() && lux >= 0.0,
            "mock lux must be finite and non-negative, got {lux}"
        );
        let mut state = self.state.write().expect("mock lock poisoned");
        state.lux = lux.min(Illuminance::MAX_LUX);
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.fault = fault;
    }

    /// Returns `true` if the mock sensor is currently powered on.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn is_powered(&self) -> bool {
        let state = self.state.read().expect("mock lock poisoned");
        state.powered
    }

    fn read_lux(&self) -> Result<Illuminance, LightError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        if !state.powered {
            return Err(LightError::powered_down());
        }
        Illuminance::from_lux(state.lux)
    }

    fn power_down(&self) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        state.powered = false;
        Ok(())
    }

    fn reset(&self) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        // Mirrors hardware: reset re-arms continuous conversion, and HardwareDevice::read_lux()
        // enforces any remaining settle time via HwState before reading the bus. In mock mode,
        // subsequent reads return the current ambient level immediately without synthetic host sleep.
        state.powered = true;
        Ok(())
    }
}

#[cfg(feature = "test-util")]
pub use hw_state::HwState;

#[cfg(not(feature = "test-util"))]
use hw_state::HwState;

mod hw_state {
    use std::time::{Duration, Instant};

    use super::LightError;

    /// Host-testable hardware state machine tracking power state and integration settle times.
    #[derive(Debug)]
    pub struct HwState {
        pub(crate) powered: bool,
        pub(crate) last_transition: Instant,
    }

    impl HwState {
        /// Maximum continuous high-resolution conversion time guaranteed by the BH1750 datasheet (180 ms).
        pub const SETTLE_DURATION: Duration = Duration::from_millis(180);

        #[must_use]
        pub const fn new(now: Instant) -> Self {
            Self {
                powered: true,
                last_transition: now,
            }
        }

        pub fn record_power_down(&mut self) {
            self.powered = false;
        }

        pub fn record_wake_or_reset(&mut self, now: Instant) {
            self.powered = true;
            self.last_transition = now;
        }

        /// Prepares for a reading by checking the integration settle window.
        ///
        /// # Errors
        ///
        /// Returns [`LightError`] if the sensor is powered down.
        pub fn prepare_read(&self, now: Instant) -> Result<Option<Duration>, LightError> {
            if !self.powered {
                return Err(LightError::powered_down());
            }
            let elapsed = now.saturating_duration_since(self.last_transition);
            Ok(Self::SETTLE_DURATION.checked_sub(elapsed))
        }
    }
}

/// Test utilities exposed for integration test suites under `tests/`.
#[cfg(feature = "test-util")]
pub mod test_helpers {
    use bh1750_embedded::Error as BhError;

    use super::LightError;

    #[must_use]
    pub fn make_i2c_error(message: impl Into<Box<str>>) -> LightError {
        LightError::i2c(message)
    }

    #[must_use]
    pub fn make_powered_down_error() -> LightError {
        LightError::powered_down()
    }

    #[must_use]
    pub fn make_cancelled_error(message: impl Into<Box<str>>) -> LightError {
        LightError::cancelled(message)
    }

    #[must_use]
    pub fn make_invalid_address_error(address: u8) -> LightError {
        LightError::invalid_address(address)
    }

    #[must_use]
    pub fn make_invalid_illuminance_error(message: impl Into<Box<str>>) -> LightError {
        LightError::invalid_illuminance(message)
    }

    #[must_use]
    pub fn make_mock_fault_error(message: impl Into<Box<str>>) -> LightError {
        LightError::mock_fault(message)
    }

    #[must_use]
    pub fn from_bh_err(err: BhError<rppal::i2c::Error>) -> LightError {
        LightError::from_bh_err(err)
    }
}

/// Internal hardware peripheral device wrapping the [`bh1750_embedded::Bh1750`] driver.
struct HardwareDevice {
    driver: Bh1750<I2c, rppal::hal::Delay>,
    state: HwState,
}

impl HardwareDevice {
    fn new(bus: u8, address: I2cAddress) -> Result<Self, LightError> {
        let i2c = I2c::with_bus(bus)?;
        let mut driver = Bh1750::new(i2c, rppal::hal::Delay::new(), address.into());
        driver.power_on().map_err(LightError::from_bh_err)?;
        driver
            .start_continuous_measurement(BhResolution::High)
            .map_err(LightError::from_bh_err)?;
        Ok(Self {
            driver,
            state: HwState::new(Instant::now()),
        })
    }

    fn power_down(&mut self) -> Result<(), LightError> {
        self.driver.power_down().map_err(LightError::from_bh_err)?;
        self.state.record_power_down();
        Ok(())
    }

    fn reset(&mut self) -> Result<(), LightError> {
        self.driver.power_on().map_err(LightError::from_bh_err)?;
        self.driver.reset().map_err(LightError::from_bh_err)?;
        self.driver
            .start_continuous_measurement(BhResolution::High)
            .map_err(LightError::from_bh_err)?;
        self.state.record_wake_or_reset(Instant::now());
        Ok(())
    }

    fn read_lux(&mut self) -> Result<Illuminance, LightError> {
        let sleep_dur = self.state.prepare_read(Instant::now())?;
        if let Some(dur) = sleep_dur {
            std::thread::sleep(dur);
        }
        let lux = self
            .driver
            .current_measurement_lux(BhResolution::High)
            .map_err(LightError::from_bh_err)?;
        Illuminance::from_lux(lux)
    }
}

/// Internal I2C / mock backend dispatch for [`LightSensor`].
enum Backend {
    Hardware {
        device: Mutex<HardwareDevice>,
        bus: u8,
        address: I2cAddress,
    },
    Mock(LightSensorMock),
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hardware { bus, address, .. } => f
                .debug_struct("Hardware")
                .field("bus", bus)
                .field("address", address)
                .finish_non_exhaustive(),
            Self::Mock(mock) => f.debug_tuple("Mock").field(mock).finish(),
        }
    }
}

/// Cheaply cloneable abstraction for the BH1750 ambient light sensor.
#[derive(Clone, Debug)]
pub struct LightSensor {
    inner: Arc<Backend>,
}

impl LightSensor {
    /// Creates a new [`LightSensor`] using the default Raspberry Pi I2C-1 bus and address `0x23`.
    ///
    /// # Errors
    /// Returns [`LightError`] if the I2C device cannot be opened or initialized.
    pub fn new() -> Result<Self, LightError> {
        Self::with_bus_and_address(BH1750_DEFAULT_I2C_BUS, I2cAddress::default())
    }

    /// Creates a new [`LightSensor`] with a specific I2C bus and 7-bit address.
    ///
    /// # Errors
    /// Returns [`LightError`] if `bus` is not [`BH1750_DEFAULT_I2C_BUS`] (`1`) or if the I2C bus cannot be opened.
    pub fn with_bus_and_address(bus: u8, address: I2cAddress) -> Result<Self, LightError> {
        validate_rpi3_bus(bus)?;
        let device = HardwareDevice::new(bus, address)?;
        Ok(Self {
            inner: Arc::new(Backend::Hardware {
                device: Mutex::new(device),
                bus,
                address,
            }),
        })
    }

    /// Creates a mocked [`LightSensor`] and returns its companion [`LightSensorMock`].
    #[must_use]
    pub fn new_mocked() -> (Self, LightSensorMock) {
        let mock = LightSensorMock::new();
        let sensor = Self {
            inner: Arc::new(Backend::Mock(mock.clone())),
        };
        (sensor, mock)
    }

    /// Returns the active I2C bus number.
    #[must_use]
    pub fn i2c_bus(&self) -> u8 {
        match self.inner.as_ref() {
            Backend::Hardware { bus, .. } => *bus,
            Backend::Mock(_) => BH1750_DEFAULT_I2C_BUS,
        }
    }

    /// Returns the active I2C 7-bit slave address.
    #[must_use]
    pub fn i2c_address(&self) -> I2cAddress {
        match self.inner.as_ref() {
            Backend::Hardware { address, .. } => *address,
            Backend::Mock(_) => I2cAddress::default(),
        }
    }

    /// Returns hardware and GPIO pinout metadata for this sensor.
    ///
    /// # Invariant
    /// The sensor bus is guaranteed to be validated as Raspberry Pi 3 BSC1 (I2C-1)
    /// at sensor construction time (`with_bus_and_address` or `new_mocked`).
    #[must_use]
    pub fn hardware_info(&self) -> I2cHardwareInfo {
        I2cHardwareInfo::new_rpi3_unchecked(self.i2c_bus(), self.i2c_address())
    }

    /// Powers down the sensor to enter sleep mode (<1 µA current consumption).
    ///
    /// Conversions cease until the sensor is re-awakened via [`reset()`](Self::reset).
    ///
    /// # Errors
    /// Returns [`LightError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn power_down(&self) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .power_down(),
            Backend::Mock(mock) => mock.power_down(),
        }
    }

    /// Resets the sensor measurement data register, awakens the sensor if sleeping,
    /// and restarts continuous high-resolution measurement.
    ///
    /// On hardware, `reset()` re-arms continuous conversion and records the transition
    /// in [`HwState`], guaranteeing that subsequent `read_lux()` calls transparently
    /// enforce any remaining datasheet settle duration (~180 ms) before reading the register.
    /// In mock mode, `reset()` re-powers the simulated sensor and preserves the configured
    /// illuminance without introducing synthetic wall-clock sleeps into test suites.
    ///
    /// # Errors
    /// Returns [`LightError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn reset(&self) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .reset(),
            Backend::Mock(mock) => mock.reset(),
        }
    }

    /// Reads the current ambient illuminance from the sensor.
    ///
    /// # Errors
    /// Returns [`LightError`] if the sensor is powered down or I2C communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_lux(&self) -> Result<Illuminance, LightError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .read_lux(),
            Backend::Mock(mock) => mock.read_lux(),
        }
    }

    /// Reads extended diagnostics from the sensor over I2C.
    ///
    /// # Errors
    /// Returns [`LightError`] if reading from the sensor fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_diagnostics(&self) -> Result<LightDiagnostics, LightError> {
        Ok(LightDiagnostics {
            illuminance: self.read_lux()?,
            address: self.i2c_address(),
            bus: self.i2c_bus(),
        })
    }

    /// Asynchronously reads illuminance from the sensor without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`LightError`] if reading fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_lux_async(&self) -> Result<Illuminance, LightError> {
        let this = self.clone();
        run_blocking("BH1750 illuminance read", move || this.read_lux()).await
    }

    /// Asynchronously reads extended diagnostics from the sensor.
    ///
    /// # Errors
    /// Returns [`LightError`] if reading fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_diagnostics_async(&self) -> Result<LightDiagnostics, LightError> {
        let this = self.clone();
        run_blocking("BH1750 diagnostics read", move || this.read_diagnostics()).await
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LightSensor>();
    assert_send_sync::<LightSensorMock>();

    // Compile-time invariant proofs:
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "compile-time constant invariant proof; From::from is not const fn"
    )]
    let max_mlx = (Illuminance::MAX_LUX * 1000.0) as u128;
    assert!(max_mlx <= u32::MAX as u128);

    // Compile-time invariant proof: maximum raw register u16::MAX / 1.2 (54,612.5 lx) <= MAX_LUX (120,000.0 lx)
    let max_from_raw = Illuminance::from_raw(u16::MAX);
    assert!(max_from_raw.lux() <= Illuminance::MAX_LUX);
};
