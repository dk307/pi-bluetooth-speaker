//! Infineon MERUS MA12070P Class-D amplifier hardware driver with typed domain models and inherent mocking.
//!
//! Provides strongly-typed, non-blocking interfaces for the MA12070P multi-level
//! digital audio amplifier on the Raspberry Pi I2C-1 bus (`/dev/i2c-1`) at slave address `0x20`.
//!
//! # Hardware Configuration
//! - **Bus:** I2C-1 (`/dev/i2c-1`)
//! - **SDA:** BCM GPIO 2 (Physical Pin 3)
//! - **SCL:** BCM GPIO 3 (Physical Pin 5)
//! - **Power:** 3.3V DC (Physical Pin 1)
//! - **Ground:** GND (Physical Pin 9)
//! - **I2C Address:** `0x20` (default, AD0/AD1 grounded) or `0x21`..=`0x23`
//!
//! # Architecture
//! - [`AmpDevice`] is cheaply cloneable (`Arc`-backed handle) and `Send + Sync`,
//!   providing non-blocking asynchronous APIs.
//! - Companion [`AmpDeviceMock`] enables deterministic testing without physical hardware.
//! - Strongly typed domain models [`AmpErrorFlags`], [`AmpFault`], [`PowerModeProfile`],
//!   [`AudioInConfig`], [`DcuMonitor`], and [`AmpI2cAddress`].

use std::backtrace::Backtrace;
use std::fmt::{self, Display, Formatter};
use std::sync::{Arc, Mutex, RwLock};

use rppal::i2c::I2c;
use serde::{Deserialize, Serialize};

use crate::runtime::{FromJoinError, run_blocking};

use super::{BcmPin, GpioPinInfo, PhysicalPin};

/// Default 7-bit I2C slave address for the MA12070P amplifier (`0x20`).
pub const MA12070P_DEFAULT_I2C_ADDR: u8 = 0x20;

/// Minimum valid 7-bit I2C slave address for MA12070P (`0x20`).
pub const MA12070P_MIN_I2C_ADDR: u8 = 0x20;

/// Maximum valid 7-bit I2C slave address for MA12070P (`0x23`).
pub const MA12070P_MAX_I2C_ADDR: u8 = 0x23;

/// Default I2C bus device on Raspberry Pi 3.
pub const MA12070P_DEFAULT_I2C_BUS: u8 = 1;

/// MA12070P audio input format and clocking register address.
pub const REG_AUDIO_IN: u8 = 0x1B;

/// MA12070P error handling and latch clear register address.
pub const REG_EH_CLEAR: u8 = 0x2D;

/// MA12070P power mode profile register address.
pub const REG_POWER_MODE: u8 = 0x60;

/// MA12070P DCU monitor channel 0 register address.
pub const REG_DCU_MON0: u8 = 0x61;

/// MA12070P DCU monitor channel 1 register address.
pub const REG_DCU_MON1: u8 = 0x62;

/// MA12070P latched historical error accumulation register address.
pub const REG_ERROR_ACC: u8 = 0x6D;

/// MA12070P real-time live error status register address.
pub const REG_ERROR_LIVE: u8 = 0x7C;

/// Validated 7-bit I2C slave address for the MA12070P amplifier (`0x20..=0x23`).
///
/// # Examples
/// ```
/// use speaker_core::hardware::AmpI2cAddress;
///
/// let addr = AmpI2cAddress::new(0x20)?;
/// assert_eq!(addr.as_u8(), 0x20);
/// # Ok::<(), speaker_core::hardware::AmpError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AmpI2cAddress(u8);

impl AmpI2cAddress {
    /// Returns the default MA12070P I2C address (`0x20`).
    #[must_use]
    pub const fn default_address() -> Self {
        Self(MA12070P_DEFAULT_I2C_ADDR)
    }

    const fn is_valid(addr: u8) -> bool {
        addr >= MA12070P_MIN_I2C_ADDR && addr <= MA12070P_MAX_I2C_ADDR
    }

    /// Creates an [`AmpI2cAddress`] validating that the address matches MA12070P hardware specs.
    ///
    /// # Errors
    /// Returns [`AmpError`] if `addr` is not in `0x20..=0x23`.
    pub fn new(addr: u8) -> Result<Self, AmpError> {
        if Self::is_valid(addr) {
            Ok(Self(addr))
        } else {
            Err(AmpError::invalid_address(addr))
        }
    }

    /// Parses a hexadecimal string (`0x20`, `0X23`, `20`) into a validated [`AmpI2cAddress`].
    ///
    /// Rejects `+` or `-` signs (both before and after the optional `0x`/`0X` prefix) as
    /// non-standard in hardware address specifications, returning a soft user-facing error
    /// message without capturing a stack backtrace.
    ///
    /// # Errors
    /// Returns a descriptive user-facing error message if `s` is signed, non-hexadecimal,
    /// or outside the valid MA12070P address range (`0x20..=0x23`).
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
                "invalid MA12070P I2C address '{s}' (expected 0x{MA12070P_MIN_I2C_ADDR:02X}..=0x{MA12070P_MAX_I2C_ADDR:02X})"
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

impl TryFrom<u8> for AmpI2cAddress {
    type Error = AmpError;

    fn try_from(addr: u8) -> Result<Self, Self::Error> {
        Self::new(addr)
    }
}

impl Default for AmpI2cAddress {
    fn default() -> Self {
        Self::default_address()
    }
}

impl Display for AmpI2cAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02X}", self.0)
    }
}

/// Individual hardware fault condition tracked by MA12070P error registers.
///
/// # Examples
/// ```
/// use speaker_core::hardware::AmpFault;
///
/// let fault = AmpFault::OvercurrentProtect;
/// assert_eq!(fault.name(), "OVERCURRENT_PROTECT");
/// assert_eq!(fault.bit(), 1);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AmpFault {
    /// Flying capacitor voltage exceeded safe limit (bit 0).
    FlyingCapOvervoltage,
    /// Speaker overload or short circuit detected (bit 1).
    OvercurrentProtect,
    /// Audio clock missing or PLL lost sync (bit 2, normal on boot/idle).
    PllClockLoss,
    /// Main DC power supply dropped below 4V (bit 3).
    UndervoltagePvdd,
    /// Die temperature exceeded warning threshold (>125°C) (bit 4).
    HighTempWarning,
    /// Thermal overload shutdown triggered (>150°C) (bit 5).
    ThermalShutdown,
    /// Power stage output pin-to-pin short detected (bit 6).
    PinShortCircuit,
    /// DC offset voltage detected at speaker terminals (bit 7).
    DcOffsetProtect,
}

impl AmpFault {
    /// Returns the register bit index (`0..=7`) for this fault.
    #[must_use]
    pub const fn bit(self) -> u8 {
        match self {
            Self::FlyingCapOvervoltage => 0,
            Self::OvercurrentProtect => 1,
            Self::PllClockLoss => 2,
            Self::UndervoltagePvdd => 3,
            Self::HighTempWarning => 4,
            Self::ThermalShutdown => 5,
            Self::PinShortCircuit => 6,
            Self::DcOffsetProtect => 7,
        }
    }

    /// Returns the bitmask (`1 << bit`) for this fault.
    #[must_use]
    pub const fn mask(self) -> u8 {
        1 << self.bit()
    }

    /// Returns the canonical constant name string for this fault.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FlyingCapOvervoltage => "FLYING_CAP_OVERVOLTAGE",
            Self::OvercurrentProtect => "OVERCURRENT_PROTECT",
            Self::PllClockLoss => "PLL_CLOCK_LOSS",
            Self::UndervoltagePvdd => "UNDERVOLTAGE_PVDD",
            Self::HighTempWarning => "HIGH_TEMP_WARNING",
            Self::ThermalShutdown => "THERMAL_SHUTDOWN",
            Self::PinShortCircuit => "PIN_SHORT_CIRCUIT",
            Self::DcOffsetProtect => "DC_OFFSET_PROTECT",
        }
    }

    /// Returns a human-readable description of this fault condition.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::FlyingCapOvervoltage => "Flying capacitor voltage exceeded safe limit",
            Self::OvercurrentProtect => "Speaker overload / short circuit detected",
            Self::PllClockLoss => "Audio clock missing or PLL lost sync (normal on boot/idle)",
            Self::UndervoltagePvdd => "Main DC power supply dropped below 4V",
            Self::HighTempWarning => "Die temperature high (>125°C) - cooling needed",
            Self::ThermalShutdown => "Thermal overload (>150°C) - amplifier auto-shutoff",
            Self::PinShortCircuit => "Power stage output pin-to-pin short detected",
            Self::DcOffsetProtect => "DC voltage detected at speaker terminals",
        }
    }

    /// Returns the fault corresponding to a bit index (`0..=7`), or `None` if `bit > 7`.
    #[must_use]
    pub const fn from_bit(bit: u8) -> Option<Self> {
        match bit {
            0 => Some(Self::FlyingCapOvervoltage),
            1 => Some(Self::OvercurrentProtect),
            2 => Some(Self::PllClockLoss),
            3 => Some(Self::UndervoltagePvdd),
            4 => Some(Self::HighTempWarning),
            5 => Some(Self::ThermalShutdown),
            6 => Some(Self::PinShortCircuit),
            7 => Some(Self::DcOffsetProtect),
            _ => None,
        }
    }

    /// All known fault conditions ordered by bit position.
    pub const ALL: [Self; 8] = [
        Self::FlyingCapOvervoltage,
        Self::OvercurrentProtect,
        Self::PllClockLoss,
        Self::UndervoltagePvdd,
        Self::HighTempWarning,
        Self::ThermalShutdown,
        Self::PinShortCircuit,
        Self::DcOffsetProtect,
    ];
}

impl Display for AmpFault {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name(), self.description())
    }
}

/// Bitflags representation of the MA12070P error status registers (0x6D and 0x7C).
///
/// # Examples
/// ```
/// use speaker_core::hardware::{AmpErrorFlags, AmpFault};
///
/// let flags = AmpErrorFlags::from_bits(0x02);
/// assert!(flags.has_fault(AmpFault::OvercurrentProtect));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
pub struct AmpErrorFlags(u8);

impl AmpErrorFlags {
    /// No error flags set (all bits zero).
    pub const NONE: Self = Self(0);

    /// Creates an [`AmpErrorFlags`] from raw register bits.
    #[must_use]
    pub const fn from_bits(raw: u8) -> Self {
        Self(raw)
    }

    /// Returns the raw 8-bit register value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }

    /// Returns `true` if no fault bits are set.
    #[must_use]
    pub const fn is_clear(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` if the specified fault bit is set.
    #[must_use]
    pub const fn has_fault(self, fault: AmpFault) -> bool {
        (self.0 & fault.mask()) != 0
    }

    /// Returns `true` if flying capacitor overvoltage is flagged.
    #[must_use]
    pub const fn flying_cap_overvoltage(self) -> bool {
        self.has_fault(AmpFault::FlyingCapOvervoltage)
    }

    /// Returns `true` if speaker overload or short circuit protection is flagged.
    #[must_use]
    pub const fn overcurrent_protect(self) -> bool {
        self.has_fault(AmpFault::OvercurrentProtect)
    }

    /// Returns `true` if audio clock missing or PLL lock loss is flagged.
    #[must_use]
    pub const fn pll_clock_loss(self) -> bool {
        self.has_fault(AmpFault::PllClockLoss)
    }

    /// Returns `true` if main PVDD supply undervoltage (<4V) is flagged.
    #[must_use]
    pub const fn undervoltage_pvdd(self) -> bool {
        self.has_fault(AmpFault::UndervoltagePvdd)
    }

    /// Returns `true` if die high temperature warning (>125°C) is flagged.
    #[must_use]
    pub const fn high_temp_warning(self) -> bool {
        self.has_fault(AmpFault::HighTempWarning)
    }

    /// Returns `true` if thermal overload shutdown (>150°C) is flagged.
    #[must_use]
    pub const fn thermal_shutdown(self) -> bool {
        self.has_fault(AmpFault::ThermalShutdown)
    }

    /// Returns `true` if pin-to-pin short circuit is flagged.
    #[must_use]
    pub const fn pin_short_circuit(self) -> bool {
        self.has_fault(AmpFault::PinShortCircuit)
    }

    /// Returns `true` if DC offset protection at speaker terminals is flagged.
    #[must_use]
    pub const fn dc_offset_protect(self) -> bool {
        self.has_fault(AmpFault::DcOffsetProtect)
    }

    /// Returns a list of active faults represented by these flags.
    #[must_use]
    pub fn faults(self) -> Vec<AmpFault> {
        AmpFault::ALL
            .iter()
            .copied()
            .filter(|&f| self.has_fault(f))
            .collect()
    }
}

impl Display for AmpErrorFlags {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.is_clear() {
            write!(f, "0x00 (No faults)")
        } else {
            write!(f, "0x{:02X} (", self.0)?;
            let active = self.faults();
            for (idx, fault) in active.iter().enumerate() {
                if idx > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", fault.name())?;
            }
            write!(f, ")")
        }
    }
}

/// MA12070P power mode profile (PMP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PowerModeProfile {
    /// High audio performance / unconstrained Class D (PMP0).
    Pmp0,
    /// Low idle power optimization (PMP1).
    Pmp1,
    /// Default balanced / dynamic power mode (PMP2).
    Pmp2,
    /// Custom power mode profile (PMP3).
    Pmp3,
}

impl PowerModeProfile {
    /// Decodes a power mode profile from register 0x60 bits [1:0].
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 0x03 {
            0 => Self::Pmp0,
            1 => Self::Pmp1,
            2 => Self::Pmp2,
            _ => Self::Pmp3,
        }
    }

    /// Returns the raw 2-bit register value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Pmp0 => 0,
            Self::Pmp1 => 1,
            Self::Pmp2 => 2,
            Self::Pmp3 => 3,
        }
    }

    /// Returns a human-readable description of this power profile.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Pmp0 => "PMP0 (High Audio Performance / Class D)",
            Self::Pmp1 => "PMP1 (Low Idle Power Optimization)",
            Self::Pmp2 => "PMP2 (Default Balanced / Dynamic Power Mode)",
            Self::Pmp3 => "PMP3 (Custom Profile)",
        }
    }
}

impl Display for PowerModeProfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Digital audio input format configured in MA12070P register 0x1B bits [2:0].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioInputFormat {
    /// Standard I2S format with 32-bit slot width.
    StandardI2s,
    /// Left-justified audio format.
    LeftJustified,
    /// Right-justified 16-bit audio format.
    RightJustified16,
    /// Right-justified 20-bit audio format.
    RightJustified20,
    /// Right-justified 24-bit audio format.
    RightJustified24,
    /// Custom or reserved format.
    Custom(u8),
}

impl AudioInputFormat {
    /// Decodes audio format from register 0x1B bits [2:0].
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 0x07 {
            0b000 => Self::StandardI2s,
            0b001 => Self::LeftJustified,
            0b010 => Self::RightJustified16,
            0b011 => Self::RightJustified20,
            0b100 => Self::RightJustified24,
            other => Self::Custom(other),
        }
    }

    /// Returns a human-readable description of this audio format.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::StandardI2s => "Standard I2S (32-bit slot)",
            Self::LeftJustified => "Left-Justified (LJ)",
            Self::RightJustified16 => "Right-Justified 16-bit (RJ-16)",
            Self::RightJustified20 => "Right-Justified 20-bit (RJ-20)",
            Self::RightJustified24 => "Right-Justified 24-bit (RJ-24)",
            Self::Custom(_) => "Custom Format",
        }
    }
}

impl Display for AudioInputFormat {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Sample rate and clock ratio mode configured in MA12070P register 0x1B bits [4:3].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClockRatio {
    /// Auto-detect clock ratio from incoming bit clock and word clock.
    AutoDetect,
    /// Manual ratio of 64x fs.
    Manual64x,
    /// Manual ratio of 128x fs.
    Manual128x,
    /// Manual ratio of 256x fs.
    Manual256x,
}

impl ClockRatio {
    /// Decodes sample rate clock ratio from register 0x1B bits [4:3].
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match (raw >> 3) & 0x03 {
            0b00 => Self::AutoDetect,
            0b01 => Self::Manual64x,
            0b10 => Self::Manual128x,
            _ => Self::Manual256x,
        }
    }

    /// Returns a human-readable description of this clock ratio setting.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::AutoDetect => "Auto-detect clock ratio",
            Self::Manual64x => "Manual Ratio (64x fs)",
            Self::Manual128x => "Manual Ratio (128x fs)",
            Self::Manual256x => "Manual Ratio (256x fs)",
        }
    }
}

impl Display for ClockRatio {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Decoded audio input configuration from register 0x1B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioInConfig {
    /// Input digital audio data format.
    pub format: AudioInputFormat,
    /// Clock ratio mode.
    pub ratio: ClockRatio,
}

impl AudioInConfig {
    /// Decodes complete audio input configuration from register 0x1B.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        Self {
            format: AudioInputFormat::from_raw(raw),
            ratio: ClockRatio::from_raw(raw),
        }
    }
}

impl Display for AudioInConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}, {}",
            self.format.description(),
            self.ratio.description()
        )
    }
}

/// Operational status of an internal DCU charge pump channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DcuChannelStatus {
    /// Charge pump is active and regulated (bit 4 set).
    Active,
    /// Charge pump is in standby or idle (bit 4 clear).
    Standby,
}

impl DcuChannelStatus {
    /// Decodes DCU channel status from register bits (bit 4: 0x10).
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        if (raw & 0x10) != 0 {
            Self::Active
        } else {
            Self::Standby
        }
    }

    /// Returns `true` if the channel charge pump is active and regulated.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns a human-readable status description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Active => "Active / Regulated",
            Self::Standby => "Standby / Idle",
        }
    }
}

impl Display for DcuChannelStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Decoded status of both internal DCU charge pump channels (registers 0x61 and 0x62).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DcuMonitor {
    /// Channel 0 charge pump operational status.
    pub ch0: DcuChannelStatus,
    /// Channel 1 charge pump operational status.
    pub ch1: DcuChannelStatus,
}

impl DcuMonitor {
    /// Decodes DCU monitor registers 0x61 and 0x62.
    #[must_use]
    pub const fn from_raw(dcu0: u8, dcu1: u8) -> Self {
        Self {
            ch0: DcuChannelStatus::from_raw(dcu0),
            ch1: DcuChannelStatus::from_raw(dcu1),
        }
    }
}

impl Display for DcuMonitor {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Ch0: {} | Ch1: {}", self.ch0, self.ch1)
    }
}

/// Comprehensive register status of the MA12070P amplifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmpStatus {
    /// Power mode profile.
    pub power_mode: PowerModeProfile,
    /// I2S input audio format and clocking ratio.
    pub audio_in: AudioInConfig,
    /// Internal DCU charge pump status.
    pub dcu_monitor: DcuMonitor,
    /// Real-time live error flags (register 0x7C).
    pub live_errors: AmpErrorFlags,
    /// Latched historical error accumulation flags (register 0x6D).
    pub latched_errors: AmpErrorFlags,
}

impl Display for AmpStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "Power Mode Profile    : {}", self.power_mode)?;
        writeln!(f, "I2S Format & Timing   : {}", self.audio_in)?;
        writeln!(f, "Internal Charge Pumps : {}", self.dcu_monitor)?;
        writeln!(
            f,
            "---------------------------------------------------------"
        )?;
        writeln!(f, "Real-Time Amplifier Status:")?;
        let live_flags = self.live_errors.faults();
        if live_flags.is_empty() {
            writeln!(f, "  [OK] Normal operation (No faults detected)")?;
        } else {
            for fault in &live_flags {
                writeln!(
                    f,
                    "  [CRITICAL FAULT] {}: {}",
                    fault.name(),
                    fault.description()
                )?;
            }
        }
        writeln!(
            f,
            "---------------------------------------------------------"
        )?;
        writeln!(f, "Historical Event History (Latched Since Boot):")?;
        let latched_flags = self.latched_errors.faults();
        if latched_flags.is_empty() {
            write!(f, "  [OK] No errors logged since boot")?;
        } else {
            for (idx, fault) in latched_flags.iter().enumerate() {
                if idx > 0 {
                    writeln!(f)?;
                }
                writeln!(f, "  [HISTORICAL EVENT] {}", fault.name())?;
                write!(f, "                     ↳ Detail: {}", fault.description())?;
            }
        }
        Ok(())
    }
}

/// Hardware connection and GPIO pinout metadata for the MA12070P amplifier.
#[derive(Debug, Clone)]
pub struct AmpHardwareInfo {
    /// Linux I2C bus number.
    pub bus: u8,
    /// I2C slave device address.
    pub address: AmpI2cAddress,
    /// Kernel device path.
    pub device_path: Box<str>,
    /// SDA data line GPIO descriptor.
    pub sda_pin: GpioPinInfo,
    /// SCL clock line GPIO descriptor.
    pub scl_pin: GpioPinInfo,
    /// Hardware 3.3V pin location on 40-pin header.
    pub vcc_pin: PhysicalPin,
    /// Hardware GND pin location on 40-pin header.
    pub gnd_pin: PhysicalPin,
}

/// Validates that `bus` corresponds to the Raspberry Pi 3 BSC1 controller (I2C-1).
fn validate_rpi3_bus(bus: u8) -> Result<(), AmpError> {
    if bus != MA12070P_DEFAULT_I2C_BUS {
        return Err(AmpError::i2c(format!(
            "unsupported I2C bus {bus}: hardware driver models Raspberry Pi 3 BSC1 (i2c-{MA12070P_DEFAULT_I2C_BUS}) only"
        )));
    }
    Ok(())
}

impl AmpHardwareInfo {
    fn new_rpi3_unchecked(bus: u8, address: AmpI2cAddress) -> Self {
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

    /// Returns the standard Raspberry Pi 3 I2C-1 pinout for the MA12070P amplifier.
    #[must_use]
    pub fn rpi3_default(address: AmpI2cAddress) -> Self {
        Self::new_rpi3_unchecked(MA12070P_DEFAULT_I2C_BUS, address)
    }

    /// Returns Raspberry Pi 3 I2C pinout metadata for `bus` and `address`.
    ///
    /// # Errors
    /// Returns [`AmpError`] if `bus` is not [`MA12070P_DEFAULT_I2C_BUS`] (`1`).
    pub fn try_rpi3_with_bus_and_address(
        bus: u8,
        address: AmpI2cAddress,
    ) -> Result<Self, AmpError> {
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

/// Categorized failure kinds for amplifier hardware operations.
#[derive(Debug)]
pub(crate) enum AmpErrorKind {
    I2c(I2cDetail),
    Cancelled { message: Box<str> },
    InvalidAddress { address: u8 },
    MockFault { message: Box<str> },
}

/// Canonical error returned by MA12070P amplifier drivers and utilities.
#[derive(Debug)]
pub struct AmpError {
    kind: AmpErrorKind,
    backtrace: Backtrace,
}

impl AmpError {
    pub(crate) fn new(kind: AmpErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn i2c(message: impl Into<Box<str>>) -> Self {
        Self::new(AmpErrorKind::I2c(I2cDetail::Message(message.into())))
    }

    pub(crate) fn from_i2c_err(err: rppal::i2c::Error) -> Self {
        Self::new(AmpErrorKind::I2c(I2cDetail::Source(err)))
    }

    pub(crate) fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(AmpErrorKind::Cancelled {
            message: message.into(),
        })
    }

    pub(crate) fn invalid_address(address: u8) -> Self {
        Self::new(AmpErrorKind::InvalidAddress { address })
    }

    pub(crate) fn mock_fault(message: impl Into<Box<str>>) -> Self {
        Self::new(AmpErrorKind::MockFault {
            message: message.into(),
        })
    }

    /// Returns `true` if this error was caused by an I2C communication or bus failure.
    #[must_use]
    pub const fn is_i2c(&self) -> bool {
        matches!(self.kind, AmpErrorKind::I2c(_))
    }

    /// Returns `true` if this error was caused by task cancellation or runtime shutdown.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, AmpErrorKind::Cancelled { .. })
    }

    /// Returns `true` if this error was caused by an invalid I2C address.
    #[must_use]
    pub const fn is_invalid_address(&self) -> bool {
        matches!(self.kind, AmpErrorKind::InvalidAddress { .. })
    }

    /// Returns `true` if this error was caused by an injected mock fault.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, AmpErrorKind::MockFault { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AmpErrorKind::I2c(I2cDetail::Message(msg)) => write!(f, "I2C bus error: {msg}"),
            AmpErrorKind::I2c(I2cDetail::Source(err)) => write!(f, "I2C bus error: {err}"),
            AmpErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
            AmpErrorKind::InvalidAddress { address } => {
                write!(
                    f,
                    "invalid MA12070P I2C address: 0x{address:02X} (expected 0x{MA12070P_MIN_I2C_ADDR:02X}..=0x{MA12070P_MAX_I2C_ADDR:02X})"
                )
            }
            AmpErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
        }
    }
}

super::impl_canonical_error_display!(AmpError);

impl std::error::Error for AmpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AmpErrorKind::I2c(I2cDetail::Source(err)) => Some(err),
            _ => None,
        }
    }
}

impl From<rppal::i2c::Error> for AmpError {
    fn from(err: rppal::i2c::Error) -> Self {
        Self::from_i2c_err(err)
    }
}

impl FromJoinError for AmpError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

/// Controllable in-memory mock for [`AmpDevice`].
#[derive(Debug, Clone)]
pub struct AmpDeviceMock {
    state: Arc<RwLock<MockState>>,
}

#[derive(Debug)]
struct MockState {
    power_mode: PowerModeProfile,
    audio_in: AudioInConfig,
    dcu_monitor: DcuMonitor,
    live_errors: AmpErrorFlags,
    latched_errors: AmpErrorFlags,
    fault: Option<Box<str>>,
}

impl AmpDeviceMock {
    fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockState {
                power_mode: PowerModeProfile::Pmp2,
                audio_in: AudioInConfig {
                    format: AudioInputFormat::StandardI2s,
                    ratio: ClockRatio::AutoDetect,
                },
                dcu_monitor: DcuMonitor {
                    ch0: DcuChannelStatus::Active,
                    ch1: DcuChannelStatus::Active,
                },
                live_errors: AmpErrorFlags::NONE,
                latched_errors: AmpErrorFlags::NONE,
                fault: None,
            })),
        }
    }

    /// Sets the simulated live error flags.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_live_errors(&self, flags: AmpErrorFlags) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.live_errors = flags;
    }

    /// Sets the simulated latched historical error flags.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_latched_errors(&self, flags: AmpErrorFlags) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.latched_errors = flags;
    }

    /// Sets the simulated power mode profile.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_power_mode(&self, mode: PowerModeProfile) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.power_mode = mode;
    }

    /// Sets the simulated audio input configuration.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_audio_in(&self, config: AudioInConfig) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.audio_in = config;
    }

    /// Sets the simulated DCU charge pump monitor status.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_dcu_monitor(&self, dcu: DcuMonitor) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.dcu_monitor = dcu;
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.fault = fault;
    }

    fn read_status(&self) -> Result<AmpStatus, AmpError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        Ok(AmpStatus {
            power_mode: state.power_mode,
            audio_in: state.audio_in,
            dcu_monitor: state.dcu_monitor,
            live_errors: state.live_errors,
            latched_errors: state.latched_errors,
        })
    }

    fn clear_latched_errors(&self) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        state.latched_errors = AmpErrorFlags::NONE;
        Ok(())
    }

    fn read_live_errors(&self) -> Result<AmpErrorFlags, AmpError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        Ok(state.live_errors)
    }

    fn read_latched_errors(&self) -> Result<AmpErrorFlags, AmpError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        Ok(state.latched_errors)
    }
}

/// Internal hardware peripheral device wrapping direct I2C bus transactions.
struct HardwareDevice {
    i2c: I2c,
}

impl HardwareDevice {
    fn new(bus: u8, address: AmpI2cAddress) -> Result<Self, AmpError> {
        let mut i2c = I2c::with_bus(bus)?;
        i2c.set_slave_address(address.as_u16())?;
        Ok(Self { i2c })
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, AmpError> {
        self.i2c.write(&[reg])?;
        let mut buf = [0u8; 1];
        self.i2c.read(&mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), AmpError> {
        self.i2c.write(&[reg, val])?;
        Ok(())
    }

    fn read_status(&mut self) -> Result<AmpStatus, AmpError> {
        let live_err = self.read_reg(REG_ERROR_LIVE)?;
        let acc_err = self.read_reg(REG_ERROR_ACC)?;
        let pwr_mode = self.read_reg(REG_POWER_MODE)?;
        let dcu0 = self.read_reg(REG_DCU_MON0)?;
        let dcu1 = self.read_reg(REG_DCU_MON1)?;
        let audio_in = self.read_reg(REG_AUDIO_IN)?;

        Ok(AmpStatus {
            power_mode: PowerModeProfile::from_raw(pwr_mode),
            audio_in: AudioInConfig::from_raw(audio_in),
            dcu_monitor: DcuMonitor::from_raw(dcu0, dcu1),
            live_errors: AmpErrorFlags::from_bits(live_err),
            latched_errors: AmpErrorFlags::from_bits(acc_err),
        })
    }

    fn clear_latched_errors(&mut self) -> Result<(), AmpError> {
        let cur = self.read_reg(REG_EH_CLEAR)?;
        self.write_reg(REG_EH_CLEAR, cur | 0x04)?;
        self.write_reg(REG_EH_CLEAR, cur & !0x04)?;
        Ok(())
    }

    fn read_live_errors(&mut self) -> Result<AmpErrorFlags, AmpError> {
        let live_err = self.read_reg(REG_ERROR_LIVE)?;
        Ok(AmpErrorFlags::from_bits(live_err))
    }

    fn read_latched_errors(&mut self) -> Result<AmpErrorFlags, AmpError> {
        let acc_err = self.read_reg(REG_ERROR_ACC)?;
        Ok(AmpErrorFlags::from_bits(acc_err))
    }
}

/// Internal I2C / mock backend dispatch for [`AmpDevice`].
enum Backend {
    Hardware {
        device: Mutex<HardwareDevice>,
        bus: u8,
        address: AmpI2cAddress,
    },
    Mock(AmpDeviceMock),
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

/// Cheaply cloneable abstraction for the MA12070P amplifier.
///
/// # Examples
/// ```
/// use speaker_core::hardware::AmpDevice;
///
/// let (amp, mock) = AmpDevice::new_mocked();
/// let status = amp.read_status()?;
/// assert!(status.live_errors.is_clear());
/// # Ok::<(), speaker_core::hardware::AmpError>(())
/// ```
#[derive(Clone, Debug)]
pub struct AmpDevice {
    inner: Arc<Backend>,
}

/// Alias for [`AmpDevice`] matching the MA12070P IC designation.
pub type Ma12070p = AmpDevice;

/// Alias for [`AmpDeviceMock`] matching the MA12070P IC designation.
pub type Ma12070pMock = AmpDeviceMock;

impl AmpDevice {
    /// Creates a new [`AmpDevice`] using the default Raspberry Pi I2C-1 bus and address `0x20`.
    ///
    /// # Errors
    /// Returns [`AmpError`] if the I2C device cannot be opened or initialized.
    pub fn new() -> Result<Self, AmpError> {
        Self::with_bus_and_address(MA12070P_DEFAULT_I2C_BUS, AmpI2cAddress::default())
    }

    /// Creates a new [`AmpDevice`] with a specific I2C bus and 7-bit slave address.
    ///
    /// # Errors
    /// Returns [`AmpError`] if `bus` is not [`MA12070P_DEFAULT_I2C_BUS`] (`1`) or if the I2C bus cannot be opened.
    pub fn with_bus_and_address(bus: u8, address: AmpI2cAddress) -> Result<Self, AmpError> {
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

    /// Creates a mocked [`AmpDevice`] and returns its companion [`AmpDeviceMock`].
    #[must_use]
    pub fn new_mocked() -> (Self, AmpDeviceMock) {
        let mock = AmpDeviceMock::new();
        let amp = Self {
            inner: Arc::new(Backend::Mock(mock.clone())),
        };
        (amp, mock)
    }

    /// Returns the active I2C bus number.
    #[must_use]
    pub fn i2c_bus(&self) -> u8 {
        match self.inner.as_ref() {
            Backend::Hardware { bus, .. } => *bus,
            Backend::Mock(_) => MA12070P_DEFAULT_I2C_BUS,
        }
    }

    /// Returns the active I2C 7-bit slave address.
    #[must_use]
    pub fn i2c_address(&self) -> AmpI2cAddress {
        match self.inner.as_ref() {
            Backend::Hardware { address, .. } => *address,
            Backend::Mock(_) => AmpI2cAddress::default(),
        }
    }

    /// Returns hardware and GPIO pinout metadata for this amplifier.
    #[must_use]
    pub fn hardware_info(&self) -> AmpHardwareInfo {
        AmpHardwareInfo::new_rpi3_unchecked(self.i2c_bus(), self.i2c_address())
    }

    /// Reads full register status from the amplifier.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_status(&self) -> Result<AmpStatus, AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .read_status(),
            Backend::Mock(mock) => mock.read_status(),
        }
    }

    /// Clears historical latched error logs in register 0x2D.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn clear_latched_errors(&self) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .clear_latched_errors(),
            Backend::Mock(mock) => mock.clear_latched_errors(),
        }
    }

    /// Reads live real-time error flags from register 0x7C.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_live_errors(&self) -> Result<AmpErrorFlags, AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .read_live_errors(),
            Backend::Mock(mock) => mock.read_live_errors(),
        }
    }

    /// Reads latched historical error flags from register 0x6D.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_latched_errors(&self) -> Result<AmpErrorFlags, AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .read_latched_errors(),
            Backend::Mock(mock) => mock.read_latched_errors(),
        }
    }

    /// Asynchronously reads full status from the amplifier without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if reading fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn read_status_async(&self) -> Result<AmpStatus, AmpError> {
        let this = self.clone();
        run_blocking("MA12070P status read", move || this.read_status()).await
    }

    /// Asynchronously clears historical latched error logs without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if clearing fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn clear_latched_errors_async(&self) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P clear latches", move || {
            this.clear_latched_errors()
        })
        .await
    }

    /// Asynchronously reads live real-time error flags without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if reading fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn read_live_errors_async(&self) -> Result<AmpErrorFlags, AmpError> {
        let this = self.clone();
        run_blocking("MA12070P live errors read", move || this.read_live_errors()).await
    }

    /// Asynchronously reads latched historical error flags without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if reading fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn read_latched_errors_async(&self) -> Result<AmpErrorFlags, AmpError> {
        let this = self.clone();
        run_blocking("MA12070P latched errors read", move || {
            this.read_latched_errors()
        })
        .await
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AmpDevice>();
    assert_send_sync::<AmpDeviceMock>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn address_validation_and_parsing() {
        assert_eq!(AmpI2cAddress::default_address().as_u8(), 0x20);
        assert_eq!(AmpI2cAddress::default().as_u16(), 0x0020);
        assert_eq!(AmpI2cAddress::default().to_string(), "0x20");

        for addr in 0x20..=0x23 {
            let valid = AmpI2cAddress::new(addr).unwrap();
            assert_eq!(valid.as_u8(), addr);
            assert_eq!(AmpI2cAddress::try_from(addr).unwrap().as_u8(), addr);
        }

        let low_err = AmpI2cAddress::new(0x19).unwrap_err();
        assert!(low_err.is_invalid_address());
        assert!(!low_err.is_i2c());
        assert!(!low_err.is_cancelled());
        assert!(!low_err.is_mock_fault());
        assert!(low_err.to_string().contains("0x19"));
        assert!(low_err.source().is_none());

        let high_err = AmpI2cAddress::new(0x24).unwrap_err();
        assert!(high_err.is_invalid_address());

        assert_eq!(AmpI2cAddress::parse_hex("0x20").unwrap().as_u8(), 0x20);
        assert_eq!(AmpI2cAddress::parse_hex("0X21").unwrap().as_u8(), 0x21);
        assert_eq!(AmpI2cAddress::parse_hex("22").unwrap().as_u8(), 0x22);
        assert_eq!(AmpI2cAddress::parse_hex("  0x23  ").unwrap().as_u8(), 0x23);

        for signed in ["+0x20", "-0x20", "0x+20", "0x-20", "0X+21"] {
            let err = AmpI2cAddress::parse_hex(signed).unwrap_err();
            assert!(err.contains("sign characters are not allowed"));
        }

        assert!(
            AmpI2cAddress::parse_hex("not-hex")
                .unwrap_err()
                .contains("expected hexadecimal digits")
        );
        assert!(
            AmpI2cAddress::parse_hex("")
                .unwrap_err()
                .contains("expected hexadecimal digits")
        );
        assert!(
            AmpI2cAddress::parse_hex("0x00")
                .unwrap_err()
                .contains("invalid MA12070P I2C address")
        );
        assert!(
            AmpI2cAddress::parse_hex("0x25")
                .unwrap_err()
                .contains("invalid MA12070P I2C address")
        );
    }

    #[test]
    fn fault_flags_and_descriptions() {
        for fault in AmpFault::ALL {
            let bit = fault.bit();
            assert_eq!(AmpFault::from_bit(bit), Some(fault));
            assert_eq!(fault.mask(), 1 << bit);
            assert_ne!(fault.name(), "");
            assert_ne!(fault.description(), "");
            assert_eq!(
                fault.to_string(),
                format!("{}: {}", fault.name(), fault.description())
            );
        }
        assert_eq!(AmpFault::from_bit(8), None);

        let none_flags = AmpErrorFlags::NONE;
        assert!(none_flags.is_clear());
        assert_eq!(none_flags.as_u8(), 0);
        assert_eq!(none_flags.to_string(), "0x00 (No faults)");
        assert_eq!(none_flags.faults(), Vec::<AmpFault>::new());

        let all_flags = AmpErrorFlags::from_bits(0xFF);
        assert!(!all_flags.is_clear());
        assert_eq!(all_flags.as_u8(), 0xFF);
        assert_eq!(all_flags.faults().len(), 8);
        assert!(all_flags.flying_cap_overvoltage());
        assert!(all_flags.overcurrent_protect());
        assert!(all_flags.pll_clock_loss());
        assert!(all_flags.undervoltage_pvdd());
        assert!(all_flags.high_temp_warning());
        assert!(all_flags.thermal_shutdown());
        assert!(all_flags.pin_short_circuit());
        assert!(all_flags.dc_offset_protect());
        assert!(all_flags.to_string().contains("FLYING_CAP_OVERVOLTAGE"));

        let single = AmpErrorFlags::from_bits(1 << 2);
        assert!(single.pll_clock_loss());
        assert!(!single.flying_cap_overvoltage());
        assert_eq!(single.faults(), vec![AmpFault::PllClockLoss]);
    }

    #[test]
    fn power_mode_profile_and_decoding() {
        assert_eq!(PowerModeProfile::from_raw(0), PowerModeProfile::Pmp0);
        assert_eq!(PowerModeProfile::from_raw(1), PowerModeProfile::Pmp1);
        assert_eq!(PowerModeProfile::from_raw(2), PowerModeProfile::Pmp2);
        assert_eq!(PowerModeProfile::from_raw(3), PowerModeProfile::Pmp3);
        assert_eq!(PowerModeProfile::from_raw(0xFF), PowerModeProfile::Pmp3);

        assert_eq!(PowerModeProfile::Pmp0.as_u8(), 0);
        assert_eq!(PowerModeProfile::Pmp1.as_u8(), 1);
        assert_eq!(PowerModeProfile::Pmp2.as_u8(), 2);
        assert_eq!(PowerModeProfile::Pmp3.as_u8(), 3);

        assert_eq!(
            PowerModeProfile::Pmp0.to_string(),
            "PMP0 (High Audio Performance / Class D)"
        );
        assert_eq!(
            PowerModeProfile::Pmp1.to_string(),
            "PMP1 (Low Idle Power Optimization)"
        );
        assert_eq!(
            PowerModeProfile::Pmp2.to_string(),
            "PMP2 (Default Balanced / Dynamic Power Mode)"
        );
        assert_eq!(PowerModeProfile::Pmp3.to_string(), "PMP3 (Custom Profile)");
    }

    #[test]
    fn audio_in_and_dcu_decoding() {
        assert_eq!(
            AudioInputFormat::from_raw(0b000),
            AudioInputFormat::StandardI2s
        );
        assert_eq!(
            AudioInputFormat::from_raw(0b001),
            AudioInputFormat::LeftJustified
        );
        assert_eq!(
            AudioInputFormat::from_raw(0b010),
            AudioInputFormat::RightJustified16
        );
        assert_eq!(
            AudioInputFormat::from_raw(0b011),
            AudioInputFormat::RightJustified20
        );
        assert_eq!(
            AudioInputFormat::from_raw(0b100),
            AudioInputFormat::RightJustified24
        );
        assert_eq!(
            AudioInputFormat::from_raw(0b101),
            AudioInputFormat::Custom(5)
        );
        assert_eq!(
            AudioInputFormat::StandardI2s.to_string(),
            "Standard I2S (32-bit slot)"
        );
        assert_eq!(
            AudioInputFormat::LeftJustified.to_string(),
            "Left-Justified (LJ)"
        );
        assert_eq!(
            AudioInputFormat::RightJustified16.to_string(),
            "Right-Justified 16-bit (RJ-16)"
        );
        assert_eq!(
            AudioInputFormat::RightJustified20.to_string(),
            "Right-Justified 20-bit (RJ-20)"
        );
        assert_eq!(
            AudioInputFormat::RightJustified24.to_string(),
            "Right-Justified 24-bit (RJ-24)"
        );
        assert_eq!(AudioInputFormat::Custom(5).to_string(), "Custom Format");

        assert_eq!(ClockRatio::from_raw(0b00 << 3), ClockRatio::AutoDetect);
        assert_eq!(ClockRatio::from_raw(0b01 << 3), ClockRatio::Manual64x);
        assert_eq!(ClockRatio::from_raw(0b10 << 3), ClockRatio::Manual128x);
        assert_eq!(ClockRatio::from_raw(0b11 << 3), ClockRatio::Manual256x);
        assert_eq!(
            ClockRatio::AutoDetect.to_string(),
            "Auto-detect clock ratio"
        );
        assert_eq!(ClockRatio::Manual64x.to_string(), "Manual Ratio (64x fs)");
        assert_eq!(ClockRatio::Manual128x.to_string(), "Manual Ratio (128x fs)");
        assert_eq!(ClockRatio::Manual256x.to_string(), "Manual Ratio (256x fs)");

        let cfg = AudioInConfig::from_raw(0b0000_0000);
        assert_eq!(cfg.format, AudioInputFormat::StandardI2s);
        assert_eq!(cfg.ratio, ClockRatio::AutoDetect);
        assert_eq!(
            cfg.to_string(),
            "Standard I2S (32-bit slot), Auto-detect clock ratio"
        );

        assert_eq!(DcuChannelStatus::from_raw(0x10), DcuChannelStatus::Active);
        assert_eq!(DcuChannelStatus::from_raw(0x00), DcuChannelStatus::Standby);
        assert!(DcuChannelStatus::Active.is_active());
        assert!(!DcuChannelStatus::Standby.is_active());
        assert_eq!(DcuChannelStatus::Active.to_string(), "Active / Regulated");
        assert_eq!(DcuChannelStatus::Standby.to_string(), "Standby / Idle");

        let dcu = DcuMonitor::from_raw(0x10, 0x00);
        assert_eq!(dcu.ch0, DcuChannelStatus::Active);
        assert_eq!(dcu.ch1, DcuChannelStatus::Standby);
        assert_eq!(
            dcu.to_string(),
            "Ch0: Active / Regulated | Ch1: Standby / Idle"
        );
    }

    #[test]
    fn amp_status_display_and_formatting() {
        let status_ok = AmpStatus {
            power_mode: PowerModeProfile::Pmp2,
            audio_in: AudioInConfig {
                format: AudioInputFormat::StandardI2s,
                ratio: ClockRatio::AutoDetect,
            },
            dcu_monitor: DcuMonitor {
                ch0: DcuChannelStatus::Active,
                ch1: DcuChannelStatus::Active,
            },
            live_errors: AmpErrorFlags::NONE,
            latched_errors: AmpErrorFlags::NONE,
        };

        let formatted = status_ok.to_string();
        assert!(formatted.contains("Power Mode Profile    : PMP2"));
        assert!(formatted.contains("Standard I2S (32-bit slot)"));
        assert!(formatted.contains("Ch0: Active / Regulated | Ch1: Active / Regulated"));
        assert!(formatted.contains("[OK] Normal operation (No faults detected)"));
        assert!(formatted.contains("[OK] No errors logged since boot"));

        let status_fault = AmpStatus {
            power_mode: PowerModeProfile::Pmp0,
            audio_in: AudioInConfig {
                format: AudioInputFormat::LeftJustified,
                ratio: ClockRatio::Manual64x,
            },
            dcu_monitor: DcuMonitor {
                ch0: DcuChannelStatus::Standby,
                ch1: DcuChannelStatus::Standby,
            },
            live_errors: AmpErrorFlags::from_bits(1 << 1),
            latched_errors: AmpErrorFlags::from_bits((1 << 2) | (1 << 4)),
        };

        let formatted_fault = status_fault.to_string();
        assert!(formatted_fault.contains("[CRITICAL FAULT] OVERCURRENT_PROTECT"));
        assert!(formatted_fault.contains("[HISTORICAL EVENT] PLL_CLOCK_LOSS"));
        assert!(formatted_fault.contains("[HISTORICAL EVENT] HIGH_TEMP_WARNING"));
    }

    #[test]
    fn hardware_info_and_bus_validation() {
        let hw = AmpHardwareInfo::rpi3_default(AmpI2cAddress::default());
        assert_eq!(hw.bus, 1);
        assert_eq!(hw.address.as_u8(), 0x20);
        assert_eq!(hw.device_path.as_ref(), "/dev/i2c-1");
        assert_eq!(hw.sda_pin.bcm_pin.as_u8(), 2);
        assert_eq!(hw.sda_pin.physical_pin.as_u8(), 3);
        assert_eq!(hw.scl_pin.bcm_pin.as_u8(), 3);
        assert_eq!(hw.scl_pin.physical_pin.as_u8(), 5);
        assert_eq!(hw.vcc_pin.as_u8(), 1);
        assert_eq!(hw.gnd_pin.as_u8(), 9);

        let valid = AmpHardwareInfo::try_rpi3_with_bus_and_address(1, AmpI2cAddress::default());
        let _ = valid.unwrap();

        let invalid_bus =
            AmpHardwareInfo::try_rpi3_with_bus_and_address(2, AmpI2cAddress::default());
        assert!(invalid_bus.is_err());
        assert!(invalid_bus.unwrap_err().is_i2c());
    }

    #[test]
    fn amp_error_predicates_and_conversions() {
        let i2c_err = AmpError::i2c("failed to write register");
        assert!(i2c_err.is_i2c());
        assert!(!i2c_err.is_cancelled());
        assert!(!i2c_err.is_invalid_address());
        assert!(!i2c_err.is_mock_fault());
        assert_eq!(i2c_err.summary(), "I2C bus error: failed to write register");
        assert!(i2c_err.to_string().contains("I2C bus error"));
        let _ = i2c_err.backtrace();

        let canc_err = AmpError::cancelled("runtime task aborted");
        assert!(canc_err.is_cancelled());
        assert_eq!(canc_err.summary(), "task cancelled: runtime task aborted");

        let addr_err = AmpError::invalid_address(0x50);
        assert!(addr_err.is_invalid_address());
        assert!(addr_err.summary().contains("0x50"));

        let mock_err = AmpError::mock_fault("simulated bus hang");
        assert!(mock_err.is_mock_fault());
        assert_eq!(
            mock_err.summary(),
            "injected mock fault: simulated bus hang"
        );
    }

    #[test]
    fn mock_device_operations_and_fault_injection() {
        let (amp, mock) = AmpDevice::new_mocked();
        assert_eq!(amp.i2c_bus(), 1);
        assert_eq!(amp.i2c_address(), AmpI2cAddress::default());
        let _ = format!("{amp:?}");

        let status = amp.read_status().unwrap();
        assert_eq!(status.power_mode, PowerModeProfile::Pmp2);
        assert!(status.live_errors.is_clear());
        assert!(status.latched_errors.is_clear());

        mock.set_live_errors(AmpErrorFlags::from_bits(1 << 0));
        mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 2));
        mock.set_power_mode(PowerModeProfile::Pmp0);
        mock.set_audio_in(AudioInConfig {
            format: AudioInputFormat::RightJustified24,
            ratio: ClockRatio::Manual128x,
        });
        mock.set_dcu_monitor(DcuMonitor {
            ch0: DcuChannelStatus::Standby,
            ch1: DcuChannelStatus::Active,
        });

        let updated = amp.read_status().unwrap();
        assert_eq!(updated.power_mode, PowerModeProfile::Pmp0);
        assert_eq!(updated.audio_in.format, AudioInputFormat::RightJustified24);
        assert_eq!(updated.audio_in.ratio, ClockRatio::Manual128x);
        assert_eq!(updated.dcu_monitor.ch0, DcuChannelStatus::Standby);
        assert!(updated.live_errors.flying_cap_overvoltage());
        assert!(updated.latched_errors.pll_clock_loss());

        assert_eq!(
            amp.read_live_errors().unwrap(),
            AmpErrorFlags::from_bits(1 << 0)
        );
        assert_eq!(
            amp.read_latched_errors().unwrap(),
            AmpErrorFlags::from_bits(1 << 2)
        );

        amp.clear_latched_errors().unwrap();
        assert!(amp.read_latched_errors().unwrap().is_clear());
        let status_cleared = amp.read_status().unwrap();
        assert!(status_cleared.latched_errors.is_clear());

        // Fault injection
        mock.set_fault(Some("bus communication error".into()));
        assert!(amp.read_status().unwrap_err().is_mock_fault());
        assert!(amp.clear_latched_errors().unwrap_err().is_mock_fault());
        assert!(amp.read_live_errors().unwrap_err().is_mock_fault());
        assert!(amp.read_latched_errors().unwrap_err().is_mock_fault());

        mock.set_fault(None);
        let _ = amp.read_status().unwrap();
    }

    #[tokio::test]
    async fn async_methods_read_from_mock_device() {
        let (amp, mock) = AmpDevice::new_mocked();

        let status = amp.read_status_async().await.unwrap();
        assert_eq!(status.power_mode, PowerModeProfile::Pmp2);

        mock.set_latched_errors(AmpErrorFlags::from_bits(1 << 3));
        assert!(
            amp.read_latched_errors_async()
                .await
                .unwrap()
                .undervoltage_pvdd()
        );

        mock.set_live_errors(AmpErrorFlags::from_bits(1 << 4));
        assert!(
            amp.read_live_errors_async()
                .await
                .unwrap()
                .high_temp_warning()
        );

        amp.clear_latched_errors_async().await.unwrap();
        assert!(amp.read_latched_errors_async().await.unwrap().is_clear());
    }

    #[tokio::test]
    async fn cancelled_blocking_task_maps_to_amp_error_cancelled() {
        let err: AmpError = crate::runtime::abort_and_map("MA12070P test task").await;
        assert!(err.is_cancelled());
        assert!(err.summary().contains("MA12070P test task"));
    }

    #[test]
    fn amp_device_with_bus_validates_rpi3() {
        let err = AmpDevice::with_bus_and_address(99, AmpI2cAddress::default()).unwrap_err();
        assert!(err.is_i2c());
        assert!(err.summary().contains("unsupported I2C bus 99"));
    }

    #[test]
    fn amp_error_flags_all_predicates() {
        for fault in AmpFault::ALL {
            let flags = AmpErrorFlags::from_bits(fault.mask());
            assert_eq!(
                flags.flying_cap_overvoltage(),
                fault == AmpFault::FlyingCapOvervoltage
            );
            assert_eq!(
                flags.overcurrent_protect(),
                fault == AmpFault::OvercurrentProtect
            );
            assert_eq!(flags.pll_clock_loss(), fault == AmpFault::PllClockLoss);
            assert_eq!(
                flags.undervoltage_pvdd(),
                fault == AmpFault::UndervoltagePvdd
            );
            assert_eq!(
                flags.high_temp_warning(),
                fault == AmpFault::HighTempWarning
            );
            assert_eq!(flags.thermal_shutdown(), fault == AmpFault::ThermalShutdown);
            assert_eq!(
                flags.pin_short_circuit(),
                fault == AmpFault::PinShortCircuit
            );
            assert_eq!(
                flags.dc_offset_protect(),
                fault == AmpFault::DcOffsetProtect
            );
        }
    }
}
