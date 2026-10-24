//! BH1750FVI digital ambient light sensor driver with typed domain models and inherent mocking.
//!
//! Provides strongly-typed, non-blocking interfaces for the ROHM BH1750FVI on the
//! Raspberry Pi I2C-1 bus (`/dev/i2c-1`).
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
//! - Strongly typed domain models [`Illuminance`], [`I2cAddress`], and [`MeasurementMode`].

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fmt::{self, Display, Formatter};
use std::io;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rppal::i2c::I2c;

use crate::runtime::{FromJoinError, run_blocking};

use super::{BcmPin, GpioPinInfo, PhysicalPin};

/// BH1750 optical measurement accuracy coefficient from datasheet: lux = raw / 1.2 at default `MTreg`.
pub const BH1750_ACCURACY_FACTOR: f32 = 1.2;

/// Default 7-bit I2C slave address for the BH1750 (ADDR pin pulled low / GND).
pub const BH1750_DEFAULT_I2C_ADDR: u8 = 0x23;

/// Alternate 7-bit I2C slave address for the BH1750 (ADDR pin tied to VCC).
pub const BH1750_ALT_I2C_ADDR: u8 = 0x5C;

/// Default I2C bus device on Raspberry Pi 3.
pub const BH1750_DEFAULT_I2C_BUS: u8 = 1;

/// BH1750 opcode: Power Down.
pub(crate) const CMD_POWER_DOWN: u8 = 0x00;

/// BH1750 opcode: Power On.
pub(crate) const CMD_POWER_ON: u8 = 0x01;

/// BH1750 opcode: Reset Data Register (only valid after Power On).
pub(crate) const CMD_RESET: u8 = 0x07;

/// Opcode: Continuous H-Resolution Mode (1 lx resolution, typ. 120 ms).
pub(crate) const CMD_CONTINUOUS_H_RES: u8 = 0x10;

/// Opcode: Continuous H-Resolution Mode 2 (0.5 lx resolution, typ. 120 ms).
pub(crate) const CMD_CONTINUOUS_H_RES2: u8 = 0x11;

/// Opcode: Continuous Low-Resolution Mode (4 lx resolution, typ. 16 ms).
pub(crate) const CMD_CONTINUOUS_L_RES: u8 = 0x13;

/// Opcode: One-Time H-Resolution Mode (sensor powers down after reading).
pub(crate) const CMD_ONETIME_H_RES: u8 = 0x20;

/// Opcode: One-Time H-Resolution Mode 2 (sensor powers down after reading).
pub(crate) const CMD_ONETIME_H_RES2: u8 = 0x21;

/// Opcode: One-Time Low-Resolution Mode (sensor powers down after reading).
pub(crate) const CMD_ONETIME_L_RES: u8 = 0x23;

/// Measurement operating mode of the BH1750 digital ambient light sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MeasurementMode {
    /// Continuous High-Resolution Mode: 1 lx resolution, ~120 ms conversion time.
    #[default]
    ContinuousHighRes,
    /// Continuous High-Resolution Mode 2: 0.5 lx resolution, ~120 ms conversion time.
    ContinuousHighRes2,
    /// Continuous Low-Resolution Mode: 4 lx resolution, ~16 ms conversion time.
    ContinuousLowRes,
    /// One-Time High-Resolution Mode: 1 lx resolution, automatically powers down.
    OneShotHighRes,
    /// One-Time High-Resolution Mode 2: 0.5 lx resolution, automatically powers down.
    OneShotHighRes2,
    /// One-Time Low-Resolution Mode: 4 lx resolution, automatically powers down.
    OneShotLowRes,
}

impl MeasurementMode {
    /// Returns the BH1750 command byte opcode for this measurement mode.
    #[must_use]
    pub const fn opcode(self) -> u8 {
        match self {
            Self::ContinuousHighRes => CMD_CONTINUOUS_H_RES,
            Self::ContinuousHighRes2 => CMD_CONTINUOUS_H_RES2,
            Self::ContinuousLowRes => CMD_CONTINUOUS_L_RES,
            Self::OneShotHighRes => CMD_ONETIME_H_RES,
            Self::OneShotHighRes2 => CMD_ONETIME_H_RES2,
            Self::OneShotLowRes => CMD_ONETIME_L_RES,
        }
    }

    /// Returns the nominal resolution in lux for this mode.
    #[must_use]
    pub const fn resolution_lux(self) -> f32 {
        match self {
            Self::ContinuousHighRes | Self::OneShotHighRes => 1.0,
            Self::ContinuousHighRes2 | Self::OneShotHighRes2 => 0.5,
            Self::ContinuousLowRes | Self::OneShotLowRes => 4.0,
        }
    }

    /// Returns the typical measurement integration time for this mode.
    #[must_use]
    pub const fn typical_measurement_time(self) -> Duration {
        match self {
            Self::ContinuousLowRes | Self::OneShotLowRes => Duration::from_millis(16),
            Self::ContinuousHighRes
            | Self::ContinuousHighRes2
            | Self::OneShotHighRes
            | Self::OneShotHighRes2 => Duration::from_millis(120),
        }
    }

    /// Returns the maximum measurement integration time guaranteed by the BH1750 datasheet.
    ///
    /// Low-resolution conversion is guaranteed within 24 ms (typ. 16 ms).
    /// High-resolution conversion is guaranteed within 180 ms (typ. 120 ms).
    #[must_use]
    pub const fn max_measurement_time(self) -> Duration {
        match self {
            Self::ContinuousLowRes | Self::OneShotLowRes => Duration::from_millis(24),
            Self::ContinuousHighRes
            | Self::ContinuousHighRes2
            | Self::OneShotHighRes
            | Self::OneShotHighRes2 => Duration::from_millis(180),
        }
    }

    /// Returns `true` if the mode continuously samples optical illuminance.
    #[must_use]
    pub const fn is_continuous(self) -> bool {
        matches!(
            self,
            Self::ContinuousHighRes | Self::ContinuousHighRes2 | Self::ContinuousLowRes
        )
    }

    /// Returns a human-readable identifier for the mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContinuousHighRes => "Continuous H-Resolution (1 lx, 120ms)",
            Self::ContinuousHighRes2 => "Continuous H-Resolution 2 (0.5 lx, 120ms)",
            Self::ContinuousLowRes => "Continuous Low-Resolution (4 lx, 16ms)",
            Self::OneShotHighRes => "One-Time H-Resolution (1 lx)",
            Self::OneShotHighRes2 => "One-Time H-Resolution 2 (0.5 lx)",
            Self::OneShotLowRes => "One-Time Low-Resolution (4 lx)",
        }
    }

    /// Returns a stable machine-readable slug for CLI flags and NDJSON telemetry.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::ContinuousHighRes => "high-res",
            Self::ContinuousHighRes2 => "high-res2",
            Self::ContinuousLowRes => "low-res",
            Self::OneShotHighRes => "oneshot-high-res",
            Self::OneShotHighRes2 => "oneshot-high-res2",
            Self::OneShotLowRes => "oneshot-low-res",
        }
    }
}

impl Display for MeasurementMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

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

/// Strongly-typed illuminance measurement in lux (lx).
///
/// Guards invariants: non-negative, non-NaN, and within valid sensor limits.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Illuminance {
    lux: f32,
}

impl Illuminance {
    /// Maximum valid illuminance reading supported by the BH1750FVI datasheet (120,000 lux).
    ///
    /// Bounds the full datasheet range including extended-`MTreg` sensitivity configurations;
    /// this driver pins the default `MTreg = 69`, so raw-derived readings peak at
    /// `65535 / 1.2 ≈ 54,612.5 lx`.
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

    /// Converts a raw 16-bit register word and measurement mode into [`Illuminance`].
    ///
    /// Note: primitive `raw as f32` is used here because `f32::from(u16)` is not `const fn` in stable Rust.
    ///
    /// # Panics
    /// Panics in debug builds if the converted lux value falls outside `0.0..=Self::MAX_LUX`.
    #[must_use]
    pub const fn from_raw_and_mode(raw: u16, mode: MeasurementMode) -> Self {
        let base_lux = raw as f32 / BH1750_ACCURACY_FACTOR;
        let lux = match mode {
            MeasurementMode::ContinuousHighRes2 | MeasurementMode::OneShotHighRes2 => {
                base_lux / 2.0
            }
            MeasurementMode::ContinuousHighRes
            | MeasurementMode::ContinuousLowRes
            | MeasurementMode::OneShotHighRes
            | MeasurementMode::OneShotLowRes => base_lux,
        };
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
#[derive(Debug, Clone)]
pub struct LightDiagnostics {
    /// Measured illuminance.
    pub illuminance: Illuminance,
    /// Active measurement mode.
    pub mode: MeasurementMode,
    /// Raw 16-bit ADC scratchpad value read over I2C.
    pub raw_word: u16,
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
        writeln!(f, "  • Operating Mode : {}", self.mode)?;
        writeln!(
            f,
            "  • Raw I2C Word   : 0x{:04X} ({})",
            self.raw_word, self.raw_word
        )?;
        write!(
            f,
            "  • Est. Res / Time: {:.1} lx / {:?}",
            self.mode.resolution_lux(),
            self.mode.typical_measurement_time()
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
    pub vcc_pin: &'static str,
    /// Hardware GND pin location on 40-pin header.
    pub gnd_pin: &'static str,
}

impl I2cHardwareInfo {
    /// Returns the standard Raspberry Pi 3 I2C-1 pinout for the BH1750 sensor.
    #[must_use]
    pub fn rpi3_default(address: I2cAddress) -> Self {
        Self::rpi3_with_bus_and_address(BH1750_DEFAULT_I2C_BUS, address)
    }

    /// Returns the Raspberry Pi 3 pinout for the specified I2C bus and address.
    ///
    /// # Panics
    /// Panics if `bus` is not [`BH1750_DEFAULT_I2C_BUS`] (`1`), as the modeled GPIO
    /// metadata corresponds strictly to Broadcom BSC1 controller pins (SDA BCM 2 / SCL BCM 3).
    #[must_use]
    pub fn rpi3_with_bus_and_address(bus: u8, address: I2cAddress) -> Self {
        const SDA_BCM: BcmPin = BcmPin::new(2);
        const SDA_PHYS: PhysicalPin = PhysicalPin::new(3);
        const SCL_BCM: BcmPin = BcmPin::new(3);
        const SCL_PHYS: PhysicalPin = PhysicalPin::new(5);

        assert!(
            bus == BH1750_DEFAULT_I2C_BUS,
            "GPIO metadata models Raspberry Pi 3 BSC1 (i2c-{BH1750_DEFAULT_I2C_BUS}) only, got bus {bus}"
        );

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
            vcc_pin: "3.3V Power (Physical Pin 1)",
            gnd_pin: "Ground (Physical Pin 9)",
        }
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
    Io { source: io::Error },
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

    pub(crate) fn from_io_err(source: io::Error) -> Self {
        Self::new(LightErrorKind::Io { source })
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

    /// Returns `true` if this error was caused by a generic I/O error.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        matches!(self.kind, LightErrorKind::Io { .. })
    }

    /// Returns `true` if this error occurred because the sensor is currently powered down.
    #[must_use]
    pub const fn is_powered_down(&self) -> bool {
        matches!(self.kind, LightErrorKind::PowerDown)
    }

    /// Returns `true` if this error was caused by task cancellation or runtime shutdown.
    ///
    /// Daemon services should inspect this predicate during graceful shutdown to avoid
    /// treating normal task cancellation or service termination as hardware faults.
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

    /// Returns a single-line error description without any captured backtrace.
    ///
    /// Safe for embedding inside machine-oriented `NDJSON` telemetry streams where
    /// multi-line stack traces when `RUST_BACKTRACE=1` would bloat or corrupt line records.
    #[must_use]
    pub fn summary(&self) -> String {
        struct Summary<'a>(&'a LightError);
        impl Display for Summary<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                self.0.fmt_kind(f)
            }
        }
        Summary(self).to_string()
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LightErrorKind::I2c(I2cDetail::Message(msg)) => write!(f, "I2C bus error: {msg}"),
            LightErrorKind::I2c(I2cDetail::Source(err)) => write!(f, "I2C bus error: {err}"),
            LightErrorKind::Io { source } => write!(f, "I/O error: {source}"),
            LightErrorKind::PowerDown => write!(
                f,
                "sensor is powered down; call set_mode() or reset() before reading"
            ),
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

impl Display for LightError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.fmt_kind(f)?;
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\nBacktrace:\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl std::error::Error for LightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            LightErrorKind::I2c(I2cDetail::Source(err)) => Some(err),
            LightErrorKind::Io { source } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for LightError {
    fn from(err: io::Error) -> Self {
        Self::from_io_err(err)
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

/// Simulated measurement source for [`LightSensorMock`].
#[derive(Debug, Clone, Copy)]
enum SimSource {
    /// Mode-adaptive illuminance in lux (`0.0..=Illuminance::MAX_LUX`).
    Lux(f32),
    /// Direct 16-bit raw register word override.
    Raw(u16),
}

/// Controllable in-memory mock for [`LightSensor`].
#[derive(Debug, Clone)]
pub struct LightSensorMock {
    state: Arc<RwLock<MockState>>,
}

#[derive(Debug)]
struct MockState {
    source: SimSource,
    mode: MeasurementMode,
    powered: bool,
    fault: Option<Box<str>>,
}

impl LightSensorMock {
    fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockState {
                source: SimSource::Lux(100.0),
                mode: MeasurementMode::ContinuousHighRes,
                powered: true,
                fault: None,
            })),
        }
    }

    /// Sets the simulated illuminance in lux across operating modes.
    ///
    /// The mock calculates the mode-appropriate raw register word on read,
    /// accounting for High-Res 2 scaling (0.5 lx resolution). Values above
    /// the 16-bit register word ceiling (~54,612.5 lx at default `MTreg`)
    /// are saturated to `u16::MAX` when encoded into the raw register.
    ///
    /// # Panics
    /// Panics if `lux` is negative or not finite (`NaN` or infinite), or if the mock state lock is poisoned.
    pub fn set_lux(&self, lux: f32) {
        assert!(
            lux.is_finite() && lux >= 0.0,
            "mock lux must be finite and non-negative, got {lux}"
        );
        let mut state = self.state.write().expect("mock lock poisoned");
        state.source = SimSource::Lux(lux.min(Illuminance::MAX_LUX));
    }

    /// Sets the raw 16-bit register word directly.
    ///
    /// Clears any mode-adaptive lux setting.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_raw_word(&self, raw: u16) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.source = SimSource::Raw(raw);
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.fault = fault;
    }

    /// Returns the active measurement mode set on the sensor.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn current_mode(&self) -> MeasurementMode {
        let state = self.state.read().expect("mock lock poisoned");
        state.mode
    }

    /// Reads the simulated raw 16-bit word and active mode.
    ///
    /// Note: on physical BH1750 hardware, one-shot modes automatically enter power-down
    /// after completing a conversion (and [`LightSensor::read_raw_word`] re-sends `CMD_POWER_ON`
    /// before each one-shot trigger). The mock retains `powered = true` across one-shot reads
    /// unless [`power_down()`](Self::power_down) is explicitly called.
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "raw word fits in u16"
    )]
    fn read_raw(&self) -> Result<(u16, MeasurementMode), LightError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        if !state.powered {
            return Err(LightError::powered_down());
        }
        let raw = match state.source {
            SimSource::Lux(lux) => {
                let mult = match state.mode {
                    MeasurementMode::ContinuousHighRes2 | MeasurementMode::OneShotHighRes2 => 2.0,
                    _ => 1.0,
                };
                ((lux * mult * BH1750_ACCURACY_FACTOR).round()).clamp(0.0, f32::from(u16::MAX))
                    as u16
            }
            SimSource::Raw(raw) => raw,
        };
        Ok((raw, state.mode))
    }

    fn set_mode(&self, mode: MeasurementMode) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        state.mode = mode;
        state.powered = true;
        Ok(())
    }

    fn power_down(&self) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        state.powered = false;
        Ok(())
    }

    /// Resets the mock measurement register.
    ///
    /// Note: unlike physical hardware in continuous mode (which immediately starts a
    /// new conversion after `CMD_RESET`), the mock zeroes the simulated reading
    /// until the next [`set_lux()`](Self::set_lux) or [`set_raw_word()`](Self::set_raw_word) call
    /// so tests can deterministically verify that a reset occurred.
    fn reset(&self) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        state.source = SimSource::Lux(0.0);
        state.powered = true;
        Ok(())
    }
}

#[derive(Debug)]
struct HwState {
    mode: MeasurementMode,
    /// Tracks whether the sensor is awakened (`true`) or explicitly placed into sleep mode
    /// via [`LightSensor::power_down()`] (`false`).
    ///
    /// Note: in `OneShot*` modes, the BH1750 hardware automatically enters power-down after
    /// each conversion, and [`LightSensor::read_raw_word()`] transparently re-sends `CMD_POWER_ON`
    /// plus the one-shot opcode on each read as long as `powered` is `true`.
    powered: bool,
    last_transition: Instant,
}

impl HwState {
    const fn new(now: Instant) -> Self {
        Self {
            mode: MeasurementMode::ContinuousHighRes,
            powered: true,
            last_transition: now,
        }
    }

    fn record_power_down(&mut self) {
        self.powered = false;
    }

    fn record_wake_or_reset(&mut self, now: Instant) {
        self.powered = true;
        self.last_transition = now;
    }

    fn record_mode_change(&mut self, new_mode: MeasurementMode, now: Instant) {
        self.mode = new_mode;
        self.powered = true;
        self.last_transition = now;
    }

    /// Evaluates whether the sensor is powered and computes any remaining settle/conversion sleep.
    fn prepare_read(
        &self,
        now: Instant,
    ) -> Result<(MeasurementMode, Option<Duration>), LightError> {
        if !self.powered {
            return Err(LightError::powered_down());
        }
        let sleep_dur = if self.mode.is_continuous() {
            let elapsed = now.saturating_duration_since(self.last_transition);
            self.mode.max_measurement_time().checked_sub(elapsed)
        } else {
            Some(self.mode.max_measurement_time())
        };
        Ok((self.mode, sleep_dur))
    }
}

const fn decode_raw_be(buf: [u8; 2]) -> u16 {
    u16::from_be_bytes(buf)
}

/// Internal I2C / mock backend dispatch for [`LightSensor`].
///
/// Coverage note: `Backend::Hardware` interacts directly with the Linux kernel I2C device
/// (`/dev/i2c-1` via `rppal::i2c::I2c` ioctl syscalls) and is verified on physical
/// Raspberry Pi 3 hardware (`root@192.168.2.110`), while `Backend::Mock` exercises all
/// domain invariants, scaling calculations, error predicates, and CLI telemetry on host/CI.
enum Backend {
    Hardware {
        i2c: Mutex<I2c>,
        bus: u8,
        address: I2cAddress,
        state: Mutex<HwState>,
        op_lock: Mutex<()>,
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
        if bus != BH1750_DEFAULT_I2C_BUS {
            return Err(LightError::i2c(format!(
                "unsupported I2C bus {bus}: hardware driver models Raspberry Pi 3 BSC1 (i2c-{BH1750_DEFAULT_I2C_BUS}) only"
            )));
        }
        let mut i2c = I2c::with_bus(bus)?;
        i2c.set_slave_address(address.as_u16())?;

        // Power on sensor and enter continuous high-res mode
        i2c.write(&[CMD_POWER_ON])?;
        i2c.write(&[CMD_CONTINUOUS_H_RES])?;

        Ok(Self {
            inner: Arc::new(Backend::Hardware {
                i2c: Mutex::new(i2c),
                bus,
                address,
                state: Mutex::new(HwState::new(Instant::now())),
                op_lock: Mutex::new(()),
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
    #[must_use]
    pub fn hardware_info(&self) -> I2cHardwareInfo {
        I2cHardwareInfo::rpi3_with_bus_and_address(self.i2c_bus(), self.i2c_address())
    }

    /// Powers down the sensor to enter sleep mode.
    ///
    /// In sleep mode, current consumption drops to <1 µA. Conversions cease until
    /// the sensor is re-awakened via [`set_mode()`](Self::set_mode) or [`reset()`](Self::reset).
    ///
    /// # Errors
    /// Returns [`LightError`] if writing the command opcode fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn power_down(&self) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware {
                i2c,
                state,
                op_lock,
                ..
            } => {
                let _op_guard = op_lock.lock().expect("op lock poisoned");
                i2c.lock()
                    .expect("I2C lock poisoned")
                    .write(&[CMD_POWER_DOWN])?;
                state
                    .lock()
                    .expect("state lock poisoned")
                    .record_power_down();
                Ok(())
            }
            Backend::Mock(mock) => mock.power_down(),
        }
    }

    fn write_wake_opcode(i2c: &Mutex<I2c>, opcode: u8) -> Result<(), LightError> {
        let mut guard = i2c.lock().expect("I2C lock poisoned");
        guard.write(&[CMD_POWER_ON])?;
        guard.write(&[opcode])?;
        Ok(())
    }

    /// Resets the sensor measurement data register and awakens the sensor if powered down.
    ///
    /// In mock mode, `reset()` zeroes the simulated illuminance and raw register word
    /// until the next [`LightSensorMock::set_lux()`] or [`LightSensorMock::set_raw_word()`] call.
    ///
    /// # Errors
    /// Returns [`LightError`] if writing the command opcode fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn reset(&self) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware {
                i2c,
                state,
                op_lock,
                ..
            } => {
                let _op_guard = op_lock.lock().expect("op lock poisoned");
                Self::write_wake_opcode(i2c, CMD_RESET)?;
                state
                    .lock()
                    .expect("state lock poisoned")
                    .record_wake_or_reset(Instant::now());
                Ok(())
            }
            Backend::Mock(mock) => mock.reset(),
        }
    }

    /// Configures the measurement operating mode on the sensor.
    ///
    /// # Errors
    /// Returns [`LightError`] if writing the command opcode fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn set_mode(&self, new_mode: MeasurementMode) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware {
                i2c,
                state,
                op_lock,
                ..
            } => {
                let _op_guard = op_lock.lock().expect("op lock poisoned");
                Self::write_wake_opcode(i2c, new_mode.opcode())?;
                state
                    .lock()
                    .expect("state lock poisoned")
                    .record_mode_change(new_mode, Instant::now());
                Ok(())
            }
            Backend::Mock(mock) => mock.set_mode(new_mode),
        }
    }

    fn read_raw_word(&self) -> Result<(u16, MeasurementMode), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware {
                i2c,
                state,
                op_lock,
                ..
            } => {
                let _op_guard = op_lock.lock().expect("op lock poisoned");
                let (cur_mode, sleep_dur) = state
                    .lock()
                    .expect("state lock poisoned")
                    .prepare_read(Instant::now())?;
                if !cur_mode.is_continuous() {
                    Self::write_wake_opcode(i2c, cur_mode.opcode())?;
                }
                if let Some(dur) = sleep_dur {
                    std::thread::sleep(dur);
                }
                let mut guard = i2c.lock().expect("I2C lock poisoned");
                let mut buf = [0u8; 2];
                guard.read(&mut buf)?;
                Ok((decode_raw_be(buf), cur_mode))
            }
            Backend::Mock(mock) => mock.read_raw(),
        }
    }

    /// Reads the current ambient illuminance from the sensor.
    ///
    /// # Power State & Settle Semantics
    /// If [`power_down()`](Self::power_down) was previously invoked, conversions
    /// cease and this method returns an error with [`LightError::is_powered_down()`]
    /// until the sensor is re-awakened via [`set_mode()`](Self::set_mode) or
    /// [`reset()`](Self::reset).
    ///
    /// In continuous modes (`Continuous*`), the first read after construction, [`set_mode()`](Self::set_mode),
    /// or [`reset()`](Self::reset) waits until the initial [`max_measurement_time()`](MeasurementMode::max_measurement_time)
    /// settle window has elapsed; subsequent reads return the most recently completed conversion
    /// latched in the BH1750 data register without blocking per read. In `OneShot*` modes (while
    /// not explicitly powered down), each call automatically issues `CMD_POWER_ON` and the one-shot
    /// trigger opcode and waits the full [`max_measurement_time()`](MeasurementMode::max_measurement_time)
    /// before reading the converted word.
    ///
    /// # Errors
    /// Returns [`LightError`] if the sensor is powered down or I2C communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_lux(&self) -> Result<Illuminance, LightError> {
        let (raw, mode) = self.read_raw_word()?;
        Ok(Illuminance::from_raw_and_mode(raw, mode))
    }

    /// Reads extended diagnostics from the sensor over I2C.
    ///
    /// # Errors
    /// Returns [`LightError`] if reading from the sensor fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_diagnostics(&self) -> Result<LightDiagnostics, LightError> {
        let (raw_word, mode) = self.read_raw_word()?;
        let illuminance = Illuminance::from_raw_and_mode(raw_word, mode);
        Ok(LightDiagnostics {
            illuminance,
            mode,
            raw_word,
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
    // Note: primitive `as` casts (`u32::MAX as u128`, `u16::MAX as f32`) are required here
    // because `From::from` trait methods cannot be invoked inside a `const` block in stable Rust.
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "compile-time constant invariant proof; From::from is not const fn"
    )]
    let max_mlx = (Illuminance::MAX_LUX * 1000.0) as u128;
    assert!(max_mlx <= u32::MAX as u128);

    // Compile-time invariant proof: maximum raw register u16::MAX / 1.2 (54,612.5 lx) <= MAX_LUX (120,000.0 lx)
    let max_from_raw = Illuminance::from_raw_and_mode(u16::MAX, MeasurementMode::ContinuousHighRes);
    assert!(max_from_raw.lux() <= Illuminance::MAX_LUX);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::abort_and_map;
    use std::error::Error;

    #[test]
    fn illuminance_conversions_and_bounds() {
        let illum = Illuminance::from_lux(100.0).unwrap();
        assert_eq!(illum.lux(), 100.0);
        assert_eq!(illum.millilux(), 100_000);
        assert!((illum.foot_candles() - 9.2903).abs() < 1e-3);

        // Raw conversion for H-Res 1 (120 raw -> 100 lx)
        let converted = Illuminance::from_raw_and_mode(120, MeasurementMode::ContinuousHighRes);
        assert!((converted.lux() - 100.0).abs() < 1e-3);

        // Raw conversion for H-Res 2 (120 raw -> 50 lx)
        let converted2 = Illuminance::from_raw_and_mode(120, MeasurementMode::ContinuousHighRes2);
        assert!((converted2.lux() - 50.0).abs() < 1e-3);

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

        // Fault injection
        mock.set_fault(Some("bus arbitration lost".into()));
        let err = sensor.read_lux().unwrap_err();
        assert!(err.is_mock_fault());
        assert!(sensor.read_diagnostics().unwrap_err().is_mock_fault());

        // Clear fault
        mock.set_fault(None);
        let _ = sensor.read_lux().unwrap();
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
    fn raw_and_mode_u16_max_boundary_and_mock_saturation() {
        let max_hres = Illuminance::from_raw_and_mode(u16::MAX, MeasurementMode::ContinuousHighRes);
        assert!((max_hres.lux() - (65535.0 / 1.2)).abs() < 1e-2);
        assert!(max_hres.lux() <= Illuminance::MAX_LUX);

        let max_hres2 =
            Illuminance::from_raw_and_mode(u16::MAX, MeasurementMode::ContinuousHighRes2);
        assert!((max_hres2.lux() - (65535.0 / 2.4)).abs() < 1e-2);
        assert!(max_hres2.lux() <= Illuminance::MAX_LUX);

        let (sensor, mock) = LightSensor::new_mocked();
        mock.set_lux(54_612.0);
        let reading = sensor.read_lux().unwrap();
        assert!((reading.lux() - 54_612.0).abs() < 1.0);

        mock.set_lux(Illuminance::MAX_LUX);
        let reading_saturated = sensor.read_lux().unwrap();
        assert_eq!(reading_saturated.lux(), 65535.0 / 1.2);
    }

    #[tokio::test]
    async fn async_methods_read_from_mock_sensor() {
        fn assert_send<T: Send>(_: T) {}
        let (sensor, mock) = LightSensor::new_mocked();
        assert_send(sensor.read_lux_async());
        assert_send(sensor.read_diagnostics_async());

        mock.set_lux(250.0);

        let reading = sensor.read_lux_async().await.unwrap();
        assert!((reading.lux() - 250.0).abs() < 1e-1);

        let diag = sensor.read_diagnostics_async().await.unwrap();
        assert!((diag.illuminance.lux() - 250.0).abs() < 1e-1);
    }

    #[tokio::test]
    async fn cancelled_blocking_task_maps_to_light_error_cancelled() {
        let err: LightError = abort_and_map("BH1750 illuminance read").await;
        assert!(err.is_cancelled());
        assert!(!err.is_i2c());
        assert!(
            err.to_string()
                .contains("BH1750 illuminance read task was cancelled")
        );
    }

    #[test]
    fn measurement_modes_and_opcodes_all_variants() {
        let modes = [
            (
                MeasurementMode::ContinuousHighRes,
                CMD_CONTINUOUS_H_RES,
                1.0,
                true,
            ),
            (
                MeasurementMode::ContinuousHighRes2,
                CMD_CONTINUOUS_H_RES2,
                0.5,
                true,
            ),
            (
                MeasurementMode::ContinuousLowRes,
                CMD_CONTINUOUS_L_RES,
                4.0,
                true,
            ),
            (
                MeasurementMode::OneShotHighRes,
                CMD_ONETIME_H_RES,
                1.0,
                false,
            ),
            (
                MeasurementMode::OneShotHighRes2,
                CMD_ONETIME_H_RES2,
                0.5,
                false,
            ),
            (
                MeasurementMode::OneShotLowRes,
                CMD_ONETIME_L_RES,
                4.0,
                false,
            ),
        ];

        for (mode, opcode, res, is_cont) in modes {
            assert_eq!(mode.opcode(), opcode);
            assert_eq!(mode.resolution_lux(), res);
            assert_eq!(mode.is_continuous(), is_cont);
            assert_ne!(mode.as_str(), "");
            assert_ne!(mode.slug(), "");
            assert_eq!(mode.to_string(), mode.as_str());
            assert!(mode.typical_measurement_time() > Duration::ZERO);
            assert!(mode.max_measurement_time() >= mode.typical_measurement_time());
        }
    }

    #[test]
    fn illuminance_bounds_and_millilux_edge_cases() {
        let zero = Illuminance::from_lux(0.0).unwrap();
        assert_eq!(zero.lux(), 0.0);
        assert_eq!(zero.millilux(), 0);
        assert_eq!(zero.to_string(), "0.00 lx");

        let small = Illuminance::from_lux(0.001).unwrap();
        assert_eq!(small.millilux(), 1);

        let max = Illuminance::from_lux(Illuminance::MAX_LUX).unwrap();
        assert_eq!(max.lux(), Illuminance::MAX_LUX);
        assert_eq!(max.millilux(), 120_000_000);

        let over = Illuminance::from_lux(Illuminance::MAX_LUX + 1.0).unwrap_err();
        assert!(over.to_string().contains("invalid illuminance"));

        let neg = Illuminance::from_lux(-1.0).unwrap_err();
        assert!(neg.to_string().contains("invalid illuminance"));

        let nan_err = Illuminance::from_lux(f32::NAN).unwrap_err();
        assert!(nan_err.to_string().contains("invalid illuminance"));

        let inf_err = Illuminance::from_lux(f32::INFINITY).unwrap_err();
        assert!(inf_err.to_string().contains("invalid illuminance"));

        // Raw conversions for remaining modes (tolerance accounts for BH1750 1.2 divisor precision)
        let raw_low = Illuminance::from_raw_and_mode(120, MeasurementMode::ContinuousLowRes);
        assert!((raw_low.lux() - 100.0).abs() < 1e-2);

        let raw_oneshot_h = Illuminance::from_raw_and_mode(120, MeasurementMode::OneShotHighRes);
        assert!((raw_oneshot_h.lux() - 100.0).abs() < 1e-2);

        let raw_oneshot_h2 = Illuminance::from_raw_and_mode(120, MeasurementMode::OneShotHighRes2);
        assert!((raw_oneshot_h2.lux() - 50.0).abs() < 1e-2);

        let raw_oneshot_l = Illuminance::from_raw_and_mode(120, MeasurementMode::OneShotLowRes);
        assert!((raw_oneshot_l.lux() - 100.0).abs() < 1e-2);
    }

    #[test]
    fn i2c_address_methods_and_formatting() {
        let def = I2cAddress::default();
        assert_eq!(def, I2cAddress::default_address());
        assert_eq!(def.as_u8(), 0x23);
        assert_eq!(def.as_u16(), 0x0023);
        assert_eq!(def.to_string(), "0x23");

        let alt = I2cAddress::alternate_address();
        assert_eq!(alt.as_u8(), 0x5C);
        assert_eq!(alt.as_u16(), 0x005C);
        assert_eq!(alt.to_string(), "0x5C");
    }

    #[test]
    fn light_diagnostics_formatting_and_hardware_info() {
        let (sensor, _) = LightSensor::new_mocked();
        let diag = sensor.read_diagnostics().unwrap();
        let formatted = diag.to_string();
        assert!(formatted.contains("BH1750 Light Diagnostics"));
        assert!(formatted.contains("Bus 1"));
        assert!(formatted.contains("Addr 0x23"));
        assert!(formatted.contains("Illuminance"));
        assert!(formatted.contains("Operating Mode"));
        assert!(formatted.contains("Raw I2C Word"));

        let hw = sensor.hardware_info();
        assert_eq!(hw.bus, 1);
        assert_eq!(hw.address.as_u8(), 0x23);
        assert_eq!(hw.device_path.as_ref(), "/dev/i2c-1");
        assert_eq!(hw.vcc_pin, "3.3V Power (Physical Pin 1)");
        assert_eq!(hw.gnd_pin, "Ground (Physical Pin 9)");
        assert_eq!(hw.sda_pin.signal_name, "I2C-1 Data (SDA)");
        assert_eq!(hw.scl_pin.signal_name, "I2C-1 Clock (SCL)");

        let hw_alt = I2cHardwareInfo::rpi3_default(I2cAddress::alternate_address());
        assert_eq!(hw_alt.bus, 1);
        assert_eq!(hw_alt.device_path.as_ref(), "/dev/i2c-1");
        assert_eq!(hw_alt.address.as_u8(), 0x5C);
    }

    #[test]
    #[should_panic(expected = "GPIO metadata models Raspberry Pi 3 BSC1")]
    fn rpi3_hardware_info_panics_on_non_default_bus() {
        let _ = I2cHardwareInfo::rpi3_with_bus_and_address(2, I2cAddress::default());
    }

    #[test]
    fn light_error_variants_predicates_and_conversions() {
        let i2c_err = LightError::i2c("bus busy");
        assert!(i2c_err.is_i2c());
        assert!(!i2c_err.is_invalid_address());
        assert!(!i2c_err.is_mock_fault());
        assert!(i2c_err.to_string().contains("I2C bus error: bus busy"));

        let addr_err = LightError::invalid_address(0x42);
        assert!(addr_err.is_invalid_address());
        assert!(!addr_err.is_i2c());
        assert!(
            addr_err
                .to_string()
                .contains("invalid BH1750 I2C address: 0x42")
        );

        let fault_err = LightError::mock_fault("simulated bus drop");
        assert!(fault_err.is_mock_fault());
        assert!(
            fault_err
                .to_string()
                .contains("injected mock fault: simulated bus drop")
        );

        let illum_err = LightError::invalid_illuminance("NaN lux");
        assert!(illum_err.is_invalid_illuminance());
        assert!(!illum_err.is_i2c());
        assert!(
            illum_err
                .to_string()
                .contains("invalid illuminance: NaN lux")
        );

        let cancel_err = LightError::cancelled("operation aborted");
        assert!(cancel_err.is_cancelled());
        assert!(!cancel_err.is_i2c());
        assert!(!cancel_err.is_io());
        assert!(!cancel_err.is_powered_down());
        assert!(matches!(
            cancel_err.backtrace().status(),
            BacktraceStatus::Captured | BacktraceStatus::Disabled
        ));
        assert!(
            cancel_err
                .to_string()
                .contains("task cancelled: operation aborted")
        );

        let pwr_err = LightError::powered_down();
        assert!(pwr_err.is_powered_down());
        assert!(!pwr_err.is_i2c());
        assert!(pwr_err.to_string().contains("sensor is powered down"));
        assert!(pwr_err.source().is_none());

        // std::error::Error trait
        assert!(i2c_err.source().is_none());
        assert!(fault_err.source().is_none());
        assert!(illum_err.source().is_none());
        assert!(addr_err.source().is_none());
        assert!(cancel_err.source().is_none());

        // From conversions and error cause preservation
        let io_err = io::Error::new(io::ErrorKind::NotFound, "device missing");
        let from_io = LightError::from(io_err);
        assert!(from_io.is_io());
        assert!(!from_io.is_i2c());
        assert!(from_io.source().is_some());
        assert!(from_io.to_string().contains("I/O error: device missing"));

        let rppal_err = rppal::i2c::Error::Io(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        let from_rppal = LightError::from(rppal_err);
        assert!(from_rppal.is_i2c());
        assert!(from_rppal.source().is_some());
        assert!(from_rppal.to_string().contains("timed out"));

        // Machine-oriented single-line summary omits captured backtrace
        for err in [
            &i2c_err,
            &addr_err,
            &fault_err,
            &illum_err,
            &cancel_err,
            &pwr_err,
            &from_io,
            &from_rppal,
        ] {
            let s = err.summary();
            assert!(!s.contains('\n'), "summary contained newline: {s}");
            assert!(
                !s.contains("Backtrace:"),
                "summary contained backtrace: {s}"
            );
            assert!(err.to_string().starts_with(&s));
        }
    }

    #[test]
    fn hw_state_settle_math_and_transitions() {
        let t0 = Instant::now();
        let mut hw = HwState::new(t0);
        assert_eq!(decode_raw_be([0x01, 0x2C]), 300);

        // Immediately after transition in ContinuousHighRes (180 ms max), full settle sleep required
        let (mode, sleep0) = hw.prepare_read(t0).unwrap();
        assert_eq!(mode, MeasurementMode::ContinuousHighRes);
        assert_eq!(sleep0, Some(Duration::from_millis(180)));

        // After 100 ms elapsed, 80 ms remaining
        let (_, sleep_partial) = hw.prepare_read(t0 + Duration::from_millis(100)).unwrap();
        assert_eq!(sleep_partial, Some(Duration::from_millis(80)));

        // After 200 ms elapsed (>= 180 ms), no sleep required
        let (_, sleep_settled) = hw.prepare_read(t0 + Duration::from_millis(200)).unwrap();
        assert_eq!(sleep_settled, None);

        // Power down rejects reads until wake/reset or mode change
        hw.record_power_down();
        assert!(
            hw.prepare_read(t0 + Duration::from_millis(300))
                .unwrap_err()
                .is_powered_down()
        );

        // Reset awakens sensor and resets transition timestamp
        let t1 = t0 + Duration::from_millis(500);
        hw.record_wake_or_reset(t1);
        assert_eq!(
            hw.prepare_read(t1).unwrap().1,
            Some(Duration::from_millis(180))
        );

        // One-shot mode always requires full max_measurement_time regardless of elapsed time
        let t2 = t1 + Duration::from_secs(5);
        hw.record_mode_change(MeasurementMode::OneShotLowRes, t1);
        let (os_mode, os_sleep) = hw.prepare_read(t2).unwrap();
        assert_eq!(os_mode, MeasurementMode::OneShotLowRes);
        assert_eq!(os_sleep, Some(Duration::from_millis(24)));
    }

    #[test]
    fn sensor_mock_controls_and_error_paths() {
        let (sensor, mock) = LightSensor::new_mocked();
        assert_eq!(sensor.i2c_bus(), 1);
        assert_eq!(sensor.i2c_address(), I2cAddress::default());

        // Test mock raw word and reset
        mock.set_raw_word(600);
        let reading = sensor.read_lux().unwrap();
        assert!((reading.lux() - 500.0).abs() < 1e-1);

        // Test mode-adaptive set_lux across modes
        mock.set_lux(1200.0);
        let reading_h1 = sensor.read_lux().unwrap();
        assert!((reading_h1.lux() - 1200.0).abs() < 1e-1);

        sensor
            .set_mode(MeasurementMode::ContinuousHighRes2)
            .unwrap();
        let reading_h2 = sensor.read_lux().unwrap();
        assert!((reading_h2.lux() - 1200.0).abs() < 1e-1);

        sensor.reset().unwrap();
        assert_eq!(sensor.read_lux().unwrap().lux(), 0.0);

        sensor.power_down().unwrap();
        let pwr_err = sensor.read_lux().unwrap_err();
        assert!(pwr_err.is_powered_down());
        assert!(sensor.read_diagnostics().unwrap_err().is_powered_down());

        // set_mode re-awakens sensor after power_down
        mock.set_lux(300.0);
        sensor.set_mode(MeasurementMode::ContinuousLowRes).unwrap();
        assert_eq!(mock.current_mode(), MeasurementMode::ContinuousLowRes);
        assert!((sensor.read_lux().unwrap().lux() - 300.0).abs() < 1.0);

        // reset also re-awakens sensor after power_down
        sensor.power_down().unwrap();
        assert!(sensor.read_lux().unwrap_err().is_powered_down());
        sensor.reset().unwrap();
        assert_eq!(sensor.read_lux().unwrap().lux(), 0.0);

        // Debug output
        let debug_str = format!("{sensor:?}");
        assert!(debug_str.contains("LightSensor"));

        // Injected fault in sensor controls
        mock.set_fault(Some("I2C NACK".into()));
        assert!(sensor.power_down().unwrap_err().is_mock_fault());
        assert!(sensor.reset().unwrap_err().is_mock_fault());
        assert!(
            sensor
                .set_mode(MeasurementMode::ContinuousHighRes)
                .unwrap_err()
                .is_mock_fault()
        );

        // Hardware constructor error path is covered hermetically by the bus-validation check;
        // do NOT invoke `LightSensor::new()` in unit tests as it opens `/dev/i2c-1` on the host.
        let bus_err =
            LightSensor::with_bus_and_address(99, I2cAddress::alternate_address()).unwrap_err();
        assert!(bus_err.is_i2c());
        assert!(bus_err.to_string().contains("unsupported I2C bus 99"));
    }
}
