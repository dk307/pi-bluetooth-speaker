//! BH1750 ambient light sensor hardware abstraction and mock controller.
//!
//! Provides strongly-typed, non-blocking interfaces for the ROHM BH1750FVI
//! digital ambient light sensor on the Raspberry Pi I2C-1 bus (`/dev/i2c-1`).
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
//! Following the Microsoft Pragmatic Rust Guidelines:
//! - [`LightSensor`] is cheaply cloneable (`Arc`-backed per `M-SERVICES-CLONE`),
//!   `Send + Sync` (`M-TYPES-SEND`), with non-blocking async methods.
//! - Companion [`LightSensorMock`] enables deterministic testing without physical hardware.
//! - Strongly typed domain models [`Illuminance`], [`I2cAddress`], and [`MeasurementMode`].

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fmt::{self, Display, Formatter};
use std::io;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rppal::i2c::I2c;

use super::{BcmPin, GpioPinInfo, PhysicalPin};

/// Default 7-bit I2C slave address for the BH1750 (ADDR pin pulled low / GND).
pub const BH1750_DEFAULT_I2C_ADDR: u8 = 0x23;

/// Alternate 7-bit I2C slave address for the BH1750 (ADDR pin tied to VCC).
pub const BH1750_ALT_I2C_ADDR: u8 = 0x5C;

/// Default I2C bus device on Raspberry Pi 3.
pub const BH1750_DEFAULT_I2C_BUS: u8 = 1;

/// Foot-candles per lux conversion multiplier (1 lx ≈ 0.092903 fc).
pub const LUX_TO_FOOT_CANDLES: f32 = 0.092_903;

/// BH1750 opcode: Power Down.
pub const CMD_POWER_DOWN: u8 = 0x00;

/// BH1750 opcode: Power On.
pub const CMD_POWER_ON: u8 = 0x01;

/// BH1750 opcode: Reset Data Register (only valid after Power On).
pub const CMD_RESET: u8 = 0x07;

/// Opcode: Continuous H-Resolution Mode (1 lx resolution, typ. 120 ms).
pub const CMD_CONTINUOUS_H_RES: u8 = 0x10;

/// Opcode: Continuous H-Resolution Mode 2 (0.5 lx resolution, typ. 120 ms).
pub const CMD_CONTINUOUS_H_RES2: u8 = 0x11;

/// Opcode: Continuous Low-Resolution Mode (4 lx resolution, typ. 16 ms).
pub const CMD_CONTINUOUS_L_RES: u8 = 0x13;

/// Opcode: One-Time H-Resolution Mode (sensor powers down after reading).
pub const CMD_ONETIME_H_RES: u8 = 0x20;

/// Opcode: One-Time H-Resolution Mode 2 (sensor powers down after reading).
pub const CMD_ONETIME_H_RES2: u8 = 0x21;

/// Opcode: One-Time Low-Resolution Mode (sensor powers down after reading).
pub const CMD_ONETIME_L_RES: u8 = 0x23;

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

    /// Creates an [`I2cAddress`] validating that the address matches BH1750 hardware specs.
    ///
    /// # Errors
    /// Returns [`LightError`] if `addr` is neither `0x23` nor `0x5C`.
    pub fn new(addr: u8) -> Result<Self, LightError> {
        if addr == BH1750_DEFAULT_I2C_ADDR || addr == BH1750_ALT_I2C_ADDR {
            Ok(Self(addr))
        } else {
            Err(LightError::invalid_address(addr))
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
    /// Maximum valid illuminance reading supported by the BH1750 sensor (120,000 lux).
    pub const MAX_LUX: f32 = 120_000.0;

    /// Creates an [`Illuminance`] from a raw float in lux.
    ///
    /// # Errors
    /// Returns [`LightError`] if `lux` is negative, NaN, infinite, or exceeds [`MAX_LUX`].
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
    #[must_use]
    pub fn from_raw_and_mode(raw: u16, mode: MeasurementMode) -> Self {
        let base_lux = f32::from(raw) / 1.2;
        let lux = match mode {
            MeasurementMode::ContinuousHighRes2 | MeasurementMode::OneShotHighRes2 => {
                base_lux / 2.0
            }
            MeasurementMode::ContinuousHighRes
            | MeasurementMode::ContinuousLowRes
            | MeasurementMode::OneShotHighRes
            | MeasurementMode::OneShotLowRes => base_lux,
        };
        let clamped = lux.clamp(0.0, Self::MAX_LUX);
        Self { lux: clamped }
    }

    /// Returns illuminance in lux (lx).
    #[must_use]
    pub const fn lux(self) -> f32 {
        self.lux
    }

    /// Returns illuminance in millilux (mlx).
    #[must_use]
    pub fn millilux(self) -> u32 {
        let mlx = (self.lux * 1000.0).round();
        if mlx <= 0.0 {
            0
        } else if mlx >= u32::MAX as f32 {
            u32::MAX
        } else {
            #[expect(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "bounds checked 0..=u32::MAX"
            )]
            let val = mlx as u32;
            val
        }
    }

    /// Returns illuminance converted to foot-candles (fc).
    #[must_use]
    pub fn foot_candles(self) -> f32 {
        self.lux * LUX_TO_FOOT_CANDLES
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
    /// Measured illuminance in foot-candles.
    pub foot_candles: f32,
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
            self.illuminance, self.foot_candles
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
    pub device_path: &'static str,
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
    ///
    /// # Panics
    /// Panics if static header pin constants are outside the 40-pin header bounds.
    #[must_use]
    pub fn rpi3_default(address: I2cAddress) -> Self {
        let sda_bcm = BcmPin::from_u8(2).expect("valid BCM 2");
        let sda_phys = PhysicalPin::from_u8(3).expect("valid Pin 3");
        let scl_bcm = BcmPin::from_u8(3).expect("valid BCM 3");
        let scl_phys = PhysicalPin::from_u8(5).expect("valid Pin 5");

        Self {
            bus: BH1750_DEFAULT_I2C_BUS,
            address,
            device_path: "/dev/i2c-1",
            sda_pin: GpioPinInfo {
                bcm_pin: sda_bcm,
                physical_pin: sda_phys,
                signal_name: "I2C-1 Data (SDA)",
                bus_protocol: "I2C (i2c-dev / Broadcom BSC1)",
                electrical_config: "Internal 1.8k pull-up to 3.3V",
            },
            scl_pin: GpioPinInfo {
                bcm_pin: scl_bcm,
                physical_pin: scl_phys,
                signal_name: "I2C-1 Clock (SCL)",
                bus_protocol: "I2C (i2c-dev / Broadcom BSC1)",
                electrical_config: "Internal 1.8k pull-up to 3.3V",
            },
            vcc_pin: "3.3V Power (Physical Pin 1)",
            gnd_pin: "Ground (Physical Pin 9)",
        }
    }
}

/// Categorized failure kinds for ambient light sensor operations.
#[derive(Debug)]
pub(crate) enum LightErrorKind {
    I2c {
        message: String,
        source: Option<rppal::i2c::Error>,
    },
    Io {
        message: String,
        source: io::Error,
    },
    InvalidAddress {
        address: u8,
    },
    InvalidIlluminance {
        message: String,
    },
    MockFault {
        message: String,
    },
}

/// Canonical error returned by ambient light sensor drivers and utilities.
#[derive(Debug)]
pub struct LightError {
    kind: LightErrorKind,
    backtrace: Backtrace,
}

impl LightError {
    #[cfg(test)]
    pub(crate) fn i2c(message: impl Into<String>) -> Self {
        Self {
            kind: LightErrorKind::I2c {
                message: message.into(),
                source: None,
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn from_i2c_err(err: rppal::i2c::Error) -> Self {
        Self {
            kind: LightErrorKind::I2c {
                message: err.to_string(),
                source: Some(err),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn from_io_err(err: io::Error) -> Self {
        Self {
            kind: LightErrorKind::Io {
                message: err.to_string(),
                source: err,
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn invalid_address(address: u8) -> Self {
        Self {
            kind: LightErrorKind::InvalidAddress { address },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn invalid_illuminance(message: impl Into<String>) -> Self {
        Self {
            kind: LightErrorKind::InvalidIlluminance {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn mock_fault(message: impl Into<String>) -> Self {
        Self {
            kind: LightErrorKind::MockFault {
                message: message.into(),
            },
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns `true` if this error was caused by an I2C communication or bus failure.
    #[must_use]
    pub const fn is_i2c(&self) -> bool {
        matches!(
            self.kind,
            LightErrorKind::I2c { .. } | LightErrorKind::Io { .. }
        )
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
}

impl Display for LightError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LightErrorKind::I2c { message, .. } => write!(f, "I2C bus error: {message}"),
            LightErrorKind::Io { message, .. } => write!(f, "I/O error: {message}"),
            LightErrorKind::InvalidAddress { address } => {
                write!(
                    f,
                    "invalid BH1750 I2C address: 0x{address:02X} (expected 0x23 or 0x5C)"
                )
            }
            LightErrorKind::InvalidIlluminance { message } => {
                write!(f, "invalid illuminance: {message}")
            }
            LightErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
        }?;
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\nBacktrace:\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl std::error::Error for LightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            LightErrorKind::I2c { source, .. } => source
                .as_ref()
                .map(|s| s as &(dyn std::error::Error + 'static)),
            LightErrorKind::Io { source, .. } => Some(source),
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

/// Controllable in-memory mock for [`LightSensor`].
#[derive(Debug, Clone)]
pub struct LightSensorMock {
    state: Arc<RwLock<MockState>>,
}

#[derive(Debug)]
struct MockState {
    raw_word: u16,
    mode: MeasurementMode,
    fault: Option<String>,
}

impl LightSensorMock {
    fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockState {
                raw_word: 120, // 100 lx (120 / 1.2)
                mode: MeasurementMode::ContinuousHighRes,
                fault: None,
            })),
        }
    }

    /// Sets the simulated illuminance in lux.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_lux(&self, lux: f32) {
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "clamped to 0..=u16::MAX"
        )]
        let raw = (lux * 1.2).round().clamp(0.0, f32::from(u16::MAX)) as u16;
        let mut state = self.state.write().expect("mock lock poisoned");
        state.raw_word = raw;
    }

    /// Sets the raw 16-bit register word.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_raw_word(&self, raw: u16) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.raw_word = raw;
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<String>) {
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

    fn read_raw(&self) -> Result<(u16, MeasurementMode), LightError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        Ok((state.raw_word, state.mode))
    }

    fn set_mode(&self, mode: MeasurementMode) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        state.mode = mode;
        Ok(())
    }

    fn power_down(&self) -> Result<(), LightError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        Ok(())
    }

    fn reset(&self) -> Result<(), LightError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(LightError::mock_fault(fault.clone()));
        }
        state.raw_word = 0;
        Ok(())
    }
}

enum Backend {
    Hardware {
        i2c: Mutex<I2c>,
        bus: u8,
        address: I2cAddress,
        mode: Mutex<MeasurementMode>,
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
    /// Returns [`LightError`] if the I2C bus cannot be opened.
    pub fn with_bus_and_address(bus: u8, address: I2cAddress) -> Result<Self, LightError> {
        let mut i2c = I2c::with_bus(bus).map_err(LightError::from)?;
        i2c.set_slave_address(address.as_u16())
            .map_err(LightError::from)?;

        // Power on sensor and enter continuous high-res mode
        i2c.write(&[CMD_POWER_ON]).map_err(LightError::from)?;
        i2c.write(&[CMD_CONTINUOUS_H_RES])
            .map_err(LightError::from)?;

        Ok(Self {
            inner: Arc::new(Backend::Hardware {
                i2c: Mutex::new(i2c),
                bus,
                address,
                mode: Mutex::new(MeasurementMode::ContinuousHighRes),
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
        I2cHardwareInfo::rpi3_default(self.i2c_address())
    }

    /// Powers down the sensor to enter sleep mode.
    ///
    /// # Errors
    /// Returns [`LightError`] if writing the command opcode fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn power_down(&self) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware { i2c, .. } => {
                let mut guard = i2c.lock().expect("I2C lock poisoned");
                guard.write(&[CMD_POWER_DOWN]).map_err(LightError::from)?;
                Ok(())
            }
            Backend::Mock(mock) => mock.power_down(),
        }
    }

    /// Resets the sensor measurement data register.
    ///
    /// # Errors
    /// Returns [`LightError`] if writing the command opcode fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn reset(&self) -> Result<(), LightError> {
        match self.inner.as_ref() {
            Backend::Hardware { i2c, .. } => {
                let mut guard = i2c.lock().expect("I2C lock poisoned");
                guard.write(&[CMD_POWER_ON]).map_err(LightError::from)?;
                guard.write(&[CMD_RESET]).map_err(LightError::from)?;
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
            Backend::Hardware { i2c, mode, .. } => {
                let mut guard = i2c.lock().expect("I2C lock poisoned");
                guard
                    .write(&[new_mode.opcode()])
                    .map_err(LightError::from)?;
                let mut cur_mode = mode.lock().expect("mode lock poisoned");
                *cur_mode = new_mode;
                Ok(())
            }
            Backend::Mock(mock) => mock.set_mode(new_mode),
        }
    }

    /// Reads the current ambient illuminance from the sensor.
    ///
    /// # Errors
    /// Returns [`LightError`] if I2C bus communication fails or returns invalid data.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_lux(&self) -> Result<Illuminance, LightError> {
        match self.inner.as_ref() {
            Backend::Hardware { i2c, mode, .. } => {
                let mut guard = i2c.lock().expect("I2C lock poisoned");
                let mut buf = [0u8; 2];
                guard.read(&mut buf).map_err(LightError::from)?;
                let raw = (u16::from(buf[0]) << 8) | u16::from(buf[1]);
                let cur_mode = *mode.lock().expect("mode lock poisoned");
                Ok(Illuminance::from_raw_and_mode(raw, cur_mode))
            }
            Backend::Mock(mock) => {
                let (raw, cur_mode) = mock.read_raw()?;
                Ok(Illuminance::from_raw_and_mode(raw, cur_mode))
            }
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
        match self.inner.as_ref() {
            Backend::Hardware {
                i2c,
                bus,
                address,
                mode,
            } => {
                let mut guard = i2c.lock().expect("I2C lock poisoned");
                let mut buf = [0u8; 2];
                guard.read(&mut buf).map_err(LightError::from)?;
                let raw_word = (u16::from(buf[0]) << 8) | u16::from(buf[1]);
                let cur_mode = *mode.lock().expect("mode lock poisoned");
                let illuminance = Illuminance::from_raw_and_mode(raw_word, cur_mode);
                Ok(LightDiagnostics {
                    illuminance,
                    foot_candles: illuminance.foot_candles(),
                    mode: cur_mode,
                    raw_word,
                    address: *address,
                    bus: *bus,
                })
            }
            Backend::Mock(mock) => {
                let (raw_word, cur_mode) = mock.read_raw()?;
                let illuminance = Illuminance::from_raw_and_mode(raw_word, cur_mode);
                Ok(LightDiagnostics {
                    illuminance,
                    foot_candles: illuminance.foot_candles(),
                    mode: cur_mode,
                    raw_word,
                    address: I2cAddress::default(),
                    bus: BH1750_DEFAULT_I2C_BUS,
                })
            }
        }
    }

    /// Asynchronously reads illuminance from the sensor without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`LightError`] if reading fails.
    pub async fn read_lux_async(&self) -> Result<Illuminance, LightError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_lux())
            .await
            .map_err(|join_err| LightError::from(io::Error::other(join_err)))?
    }

    /// Asynchronously reads extended diagnostics from the sensor.
    ///
    /// # Errors
    /// Returns [`LightError`] if reading fails.
    pub async fn read_diagnostics_async(&self) -> Result<LightDiagnostics, LightError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_diagnostics())
            .await
            .map_err(|join_err| LightError::from(io::Error::other(join_err)))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(I2cAddress::new(0x20).unwrap_err().is_invalid_address());
        assert!(I2cAddress::new(0x77).unwrap_err().is_invalid_address());
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

    #[tokio::test]
    async fn async_methods_read_from_mock_sensor() {
        let (sensor, mock) = LightSensor::new_mocked();
        mock.set_lux(250.0);

        let reading = sensor.read_lux_async().await.unwrap();
        assert!((reading.lux() - 250.0).abs() < 1e-1);

        let diag = sensor.read_diagnostics_async().await.unwrap();
        assert!((diag.illuminance.lux() - 250.0).abs() < 1e-1);
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
            assert_eq!(mode.to_string(), mode.as_str());
            assert!(mode.typical_measurement_time() > Duration::ZERO);
        }
    }

    #[test]
    fn illuminance_bounds_and_millilux_edge_cases() {
        let zero = Illuminance::from_lux(0.0).unwrap();
        assert_eq!(zero.lux(), 0.0);
        assert_eq!(zero.millilux(), 0);
        assert_eq!(zero.to_string(), "0.00 lx");

        // Clamping on negative lux inside struct
        let neg_clamped = Illuminance { lux: -10.0 };
        assert_eq!(neg_clamped.millilux(), 0);

        // Clamping on very large lux inside struct
        let huge = Illuminance { lux: 1e12 };
        assert_eq!(huge.millilux(), u32::MAX);

        let max = Illuminance::from_lux(Illuminance::MAX_LUX).unwrap();
        assert_eq!(max.lux(), Illuminance::MAX_LUX);
        assert!(max.millilux() > 0);

        let over = Illuminance::from_lux(Illuminance::MAX_LUX + 1.0).unwrap_err();
        assert!(over.to_string().contains("invalid illuminance"));

        // Raw conversions for remaining modes
        let raw_low = Illuminance::from_raw_and_mode(120, MeasurementMode::ContinuousLowRes);
        assert!((raw_low.lux() - 100.0).abs() < 1e-3);

        let raw_oneshot_h = Illuminance::from_raw_and_mode(120, MeasurementMode::OneShotHighRes);
        assert!((raw_oneshot_h.lux() - 100.0).abs() < 1e-3);

        let raw_oneshot_h2 = Illuminance::from_raw_and_mode(120, MeasurementMode::OneShotHighRes2);
        assert!((raw_oneshot_h2.lux() - 50.0).abs() < 1e-3);

        let raw_oneshot_l = Illuminance::from_raw_and_mode(120, MeasurementMode::OneShotLowRes);
        assert!((raw_oneshot_l.lux() - 100.0).abs() < 1e-3);
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
        assert!(formatted.contains("[BH1750 Light Diagnostics: Bus 1, Addr 0x23]"));
        assert!(formatted.contains("Illuminance"));
        assert!(formatted.contains("Operating Mode"));
        assert!(formatted.contains("Raw I2C Word"));

        let hw = sensor.hardware_info();
        assert_eq!(hw.bus, 1);
        assert_eq!(hw.address.as_u8(), 0x23);
        assert_eq!(hw.device_path, "/dev/i2c-1");
        assert_eq!(hw.vcc_pin, "3.3V Power (Physical Pin 1)");
        assert_eq!(hw.gnd_pin, "Ground (Physical Pin 9)");
        assert_eq!(hw.sda_pin.signal_name, "I2C-1 Data (SDA)");
        assert_eq!(hw.scl_pin.signal_name, "I2C-1 Clock (SCL)");
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

        // std::error::Error trait
        assert!(i2c_err.source().is_none());
        assert!(fault_err.source().is_none());
        assert!(illum_err.source().is_none());
        assert!(addr_err.source().is_none());

        // From conversions and error cause preservation
        let io_err = io::Error::new(io::ErrorKind::NotFound, "device missing");
        let from_io = LightError::from(io_err);
        assert!(from_io.is_i2c());
        assert!(from_io.source().is_some());
        assert!(from_io.to_string().contains("I/O error: device missing"));

        let rppal_err = rppal::i2c::Error::Io(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        let from_rppal = LightError::from(rppal_err);
        assert!(from_rppal.is_i2c());
        assert!(from_rppal.source().is_some());
        assert!(from_rppal.to_string().contains("timed out"));
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

        sensor.reset().unwrap();
        assert_eq!(sensor.read_lux().unwrap().lux(), 0.0);

        sensor.power_down().unwrap();

        sensor.set_mode(MeasurementMode::ContinuousLowRes).unwrap();
        assert_eq!(mock.current_mode(), MeasurementMode::ContinuousLowRes);

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

        // Hardware error path test on non-existent bus
        let _ = LightSensor::with_bus_and_address(99, I2cAddress::alternate_address());
    }
}
