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

use embedded_hal::blocking::i2c::{Read, Write, WriteRead};
use rppal::i2c::I2c;
use serde::{Deserialize, Serialize};

use speaker_common::runtime::{FromJoinError, run_blocking};

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

/// MA12070P DCU channel 0 power-mode monitor register address
/// (`pm_mon` bits 1:0, `freqmode_mon` bits 6:4, `pps_passed` bit 7).
pub const REG_POWER_MODE: u8 = 0x60;

/// MA12070P DCU channel 0 status register address
/// (ocp / `vcfly1` / `vcfly2` / `pvdd_ok` / `vdd_ok` bit 4 / mute monitor).
pub const REG_DCU_MON0: u8 = 0x61;

/// MA12070P DCU channel 1 status register address (same layout as 0x61).
///
/// Phase 0 correction: the driver header places `dcu_mon1` status at 0x65
/// (0x62 is dcu0 `m_mon`, 0x63/0x64 dcu1 `pm_mon`/`freqmode`).
pub const REG_DCU_MON1: u8 = 0x65;

/// MA12070P latched historical error accumulation register address.
pub const REG_ERROR_ACC: u8 = 0x6D;

/// MA12070P real-time live error status register address.
pub const REG_ERROR_LIVE: u8 = 0x7C;

/// MA12070P power mode control register address (0x00).
/// Bit 6: manualPM (1 = manual power mode control)
/// Bits 5:4: `PM_man` (0-3 = Power Mode 0-3 selection)
pub const REG_PM_CTRL: u8 = 0x00;

/// MA12070P power mode profile register address (0x1D).
/// Bits 2:0: `PMprofile` (0-4 = PMP0-PMP4 selection)
pub const REG_PM_PROFILE: u8 = 0x1D;

/// MA12070P audio in mode register address (0x25).
/// Bits 6:5: gain select (0 = 20dB, 1 = 26dB, 2 = 32dB, 3 = 36dB)
pub const REG_AUDIO_IN_MODE: u8 = 0x25;

/// MA12070P I2S format register address (0x35).
/// Bits 2:0: I2S format (0 = Standard I2S, 1 = Left-Justified, 2 = RJ-16, 3 = RJ-20, 4 = RJ-24)
pub const REG_I2S_FORMAT: u8 = 0x35;

/// MA12070P audio processor control register address (0x36).
/// Bit 7: `audio_proc_mute` (1 = mute, 0 = play)
pub const REG_AUDIO_PROC: u8 = 0x36;

/// MA12070P master volume integer dB register address (0x40).
/// Raw value 0..144 maps to -144..0 dB (raw = -dB).
pub const REG_VOL_DB_MASTER: u8 = 0x40;

/// MA12070P master volume fractional 0.25 dB register address (0x41).
/// Bits 1:0: fractional steps (0, 0.25, 0.5, 0.75 dB)
pub const REG_VOL_LSB_MASTER: u8 = 0x41;

/// MA12070P channel 0 volume integer dB register address (0x42).
/// Channels 0-3 at 0x42..=0x45. Raw 0..144 maps to +24..-120 dB (raw = 24 - dB).
pub const REG_VOL_DB_CH0: u8 = 0x42;

/// MA12070P channel volume fractional 0.25 dB register address (0x46).
/// Bits 1:0 per channel (0, 0.25, 0.5, 0.75 dB).
pub const REG_VOL_LSB_CH: u8 = 0x46;

/// MA12070P channel 0 DRC limiter threshold integer dB register address (0x47).
/// Channels 0-3 at 0x47..=0x4A. Raw 0..144 maps to +24..-120 dB (raw = 24 - dB).
pub const REG_THR_DB_CH0: u8 = 0x47;

/// MA12070P channel limiter threshold fractional 0.25 dB register address (0x4B).
/// Bits 1:0 per channel (0, 0.25, 0.5, 0.75 dB).
pub const REG_THR_LSB_CH: u8 = 0x4B;

/// VLP register value for 0 dB gain (datasheet Table 8-9: `reg = 24 − dB`;
/// user-scale raw = −dB maps `0x40` to `168 − raw`, range 0x18..=0xA8).
pub const VLP_ZERO_DB_REG: u16 = 168;

/// `0x35` bit 3 `audio_proc_enable` (VLP bypass mux; 1 = VLP active).
pub const AUDIO_PROC_ENABLE_MASK: u8 = 0x08;

/// `0x36` bit 6 `audio_proc_limiterEnable` (output limiter bypass mux).
pub const LIMITER_ENABLE_MASK: u8 = 0x40;

/// Pure helper: apply or clear the serial mute bit (0x36 bit 7) in a register value.
#[must_use]
pub fn apply_mute_bit(reg: u8, muted: bool) -> u8 {
    if muted {
        reg | (1 << 7)
    } else {
        reg & !(1 << 7)
    }
}

/// Pure helper: extract mute state from register 0x36 bit 7.
#[must_use]
pub const fn is_muted_bit(reg: u8) -> bool {
    reg & (1 << 7) != 0
}

/// Decodes a VLP `0x40` register value (`24 − dB` encoding) to the appliance
/// user-scale raw (raw = −dB, 0..=144).
///
/// Values below the 0 dB register (boost) clamp to 144; values above the
/// −144 dB register clamp to 0.
#[must_use]
pub const fn decode_vlp_raw(reg: u16) -> u16 {
    if reg >= VLP_ZERO_DB_REG {
        0
    } else {
        let diff = VLP_ZERO_DB_REG - reg;
        if diff > 144 { 144 } else { diff }
    }
}

/// Validated 7-bit I2C slave address for the MA12070P amplifier (`0x20..=0x23`).
///
/// # Examples
/// ```
/// use speaker_hardware::AmpI2cAddress;
///
/// let addr = AmpI2cAddress::new(0x20)?;
/// assert_eq!(addr.as_u8(), 0x20);
/// # Ok::<(), speaker_hardware::AmpError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// use speaker_hardware::AmpFault;
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
/// use speaker_hardware::{AmpErrorFlags, AmpFault};
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

    /// Returns comma-separated active fault names, or `"None"` if no faults are present.
    #[must_use]
    pub fn names_joined(self) -> String {
        let faults = self.faults();
        if faults.is_empty() {
            "None".to_string()
        } else {
            faults
                .iter()
                .map(|f| f.name())
                .collect::<Vec<_>>()
                .join(", ")
        }
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

impl AmpStatus {
    /// Decodes a 6-byte register snapshot read from
    /// `[REG_ERROR_LIVE, REG_ERROR_ACC, REG_POWER_MODE, REG_DCU_MON0, REG_DCU_MON1, REG_AUDIO_IN]`.
    #[must_use]
    pub const fn from_raw_registers(regs: [u8; 6]) -> Self {
        let [live_err, acc_err, pwr_mode, dcu0, dcu1, audio_in] = regs;
        Self {
            power_mode: PowerModeProfile::from_raw(pwr_mode),
            audio_in: AudioInConfig::from_raw(audio_in),
            dcu_monitor: DcuMonitor::from_raw(dcu0, dcu1),
            live_errors: AmpErrorFlags::from_bits(live_err),
            latched_errors: AmpErrorFlags::from_bits(acc_err),
        }
    }

    /// Computes the three-step rising-edge register value sequence (`low -> high -> low`)
    /// on bit 2 (`0x04`) of `REG_EH_CLEAR` (register `0x2D`) required to clear latched faults.
    #[must_use]
    pub const fn eh_clear_pulse_sequence(cur: u8) -> [u8; 3] {
        [cur & !0x04, cur | 0x04, cur & !0x04]
    }
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

/// Phase 1.5 configuration values readable and writable on the MA12070P.
///
/// Groups the user-configurable amplifier settings: master volume, serial mute,
/// manual power mode, power mode profile, I2S format, and audio input gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmpSettings {
    /// Master volume raw attenuation (raw = -dB, 0..=144; 0 is full mute, 144 is 0 dB).
    pub master_volume_raw: u16,
    /// Master volume fractional register value (0.25 dB steps, bits 1:0 of 0x41).
    pub master_volume_frac: u8,
    /// Serial mute state (register 0x36 bit 7).
    pub serial_muted: bool,
    /// Decoded manual power mode `PM_man` (bits 5:4 of register 0x00, 0-3).
    pub pm_ctrl_pm: u8,
    /// Power mode profile selection (bits 2:0 of register 0x1D, 0-4).
    pub pm_profile: u8,
    /// I2S format select (bits 2:0 of register 0x35, 0-4).
    pub i2s_format: u8,
    /// Audio input gain select (bits 6:5 of register 0x25, 0-3).
    pub audio_in_gain: u8,
}

impl AmpSettings {
    /// Decodes a 7-byte register snapshot read from
    /// `[REG_PM_CTRL, REG_PM_PROFILE, REG_AUDIO_IN_MODE, REG_I2S_FORMAT, REG_AUDIO_PROC, REG_VOL_DB_MASTER, REG_VOL_LSB_MASTER]`.
    #[must_use]
    pub const fn from_raw_registers(regs: [u8; 7]) -> Self {
        let [
            pm_ctrl,
            pm_profile,
            audio_in_mode,
            i2s_format,
            audio_proc,
            vol_int,
            vol_frac,
        ] = regs;
        Self {
            // `as` casts required inside const blocks (From is not const fn);
            // u8 -> u16 is lossless. VLP register decode: raw = 168 − reg,
            // clamped to the user scale 0..=144 (reg < 24 = boost).
            master_volume_raw: if vol_int >= VLP_ZERO_DB_REG as u8 {
                0
            } else {
                let diff = VLP_ZERO_DB_REG - vol_int as u16;
                if diff > 144 { 144 } else { diff }
            },
            master_volume_frac: vol_frac & 0x03,
            serial_muted: is_muted_bit(audio_proc),
            pm_ctrl_pm: (pm_ctrl >> 4) & 0x03,
            pm_profile: pm_profile & 0x07,
            i2s_format: i2s_format & 0x07,
            audio_in_gain: (audio_in_mode >> 5) & 0x03,
        }
    }
}

impl Display for AmpSettings {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Master Volume         : -{} dB (raw {}, frac {})",
            self.master_volume_raw, self.master_volume_raw, self.master_volume_frac
        )?;
        writeln!(
            f,
            "Serial Mute           : {}",
            if self.serial_muted {
                "Muted"
            } else {
                "Unmuted"
            }
        )?;
        writeln!(
            f,
            "Manual Power Mode     : PM_man={} (manualPM set)",
            self.pm_ctrl_pm
        )?;
        writeln!(f, "Power Mode Profile    : {}", self.pm_profile)?;
        writeln!(f, "I2S Format Select     : {}", self.i2s_format)?;
        write!(f, "Audio Input Gain      : {}", self.audio_in_gain)?;
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
        return Err(AmpError::unsupported_bus(bus));
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

/// Categorized failure kinds for amplifier hardware operations.
#[derive(Debug)]
pub(crate) enum AmpErrorKind {
    I2c(rppal::i2c::Error),
    Cancelled {
        message: Box<str>,
    },
    InvalidAddress {
        address: u8,
    },
    UnsupportedBus {
        bus: u8,
    },
    MockFault {
        message: Box<str>,
    },
    /// Register write verification failed (read-back mismatch).
    Verification {
        register: u8,
        wrote: u8,
        read: u8,
    },
    /// Invalid argument value for a domain operation.
    InvalidArgument {
        param: &'static str,
        value: u16,
    },
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

    pub(crate) fn i2c(err: rppal::i2c::Error) -> Self {
        Self::new(AmpErrorKind::I2c(err))
    }

    pub(crate) fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(AmpErrorKind::Cancelled {
            message: message.into(),
        })
    }

    pub(crate) fn invalid_address(address: u8) -> Self {
        Self::new(AmpErrorKind::InvalidAddress { address })
    }

    pub(crate) fn unsupported_bus(bus: u8) -> Self {
        Self::new(AmpErrorKind::UnsupportedBus { bus })
    }

    pub(crate) fn mock_fault(message: impl Into<Box<str>>) -> Self {
        Self::new(AmpErrorKind::MockFault {
            message: message.into(),
        })
    }

    pub(crate) fn verification(register: u8, wrote: u8, read: u8) -> Self {
        Self::new(AmpErrorKind::Verification {
            register,
            wrote,
            read,
        })
    }

    pub(crate) fn invalid_argument(param: &'static str, value: u16) -> Self {
        Self::new(AmpErrorKind::InvalidArgument { param, value })
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

    /// Returns `true` if this error was caused by an unsupported I2C bus number.
    #[must_use]
    pub const fn is_unsupported_bus(&self) -> bool {
        matches!(self.kind, AmpErrorKind::UnsupportedBus { .. })
    }

    /// Returns `true` if this error was caused by an injected mock fault.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, AmpErrorKind::MockFault { .. })
    }

    /// Returns `true` if this error was caused by a register write verification mismatch.
    #[must_use]
    pub const fn is_verification(&self) -> bool {
        matches!(self.kind, AmpErrorKind::Verification { .. })
    }

    /// Returns `true` if this error was caused by an invalid argument value.
    #[must_use]
    pub const fn is_invalid_argument(&self) -> bool {
        matches!(self.kind, AmpErrorKind::InvalidArgument { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AmpErrorKind::I2c(err) => write!(f, "I2C bus error: {err}"),
            AmpErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
            AmpErrorKind::InvalidAddress { address } => {
                write!(
                    f,
                    "invalid MA12070P I2C address: 0x{address:02X} (expected 0x{MA12070P_MIN_I2C_ADDR:02X}..=0x{MA12070P_MAX_I2C_ADDR:02X})"
                )
            }
            AmpErrorKind::UnsupportedBus { bus } => {
                write!(
                    f,
                    "unsupported I2C bus {bus}: hardware driver models Raspberry Pi 3 BSC1 (i2c-{MA12070P_DEFAULT_I2C_BUS}) only"
                )
            }
            AmpErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
            AmpErrorKind::Verification {
                register,
                wrote,
                read,
            } => write!(
                f,
                "register 0x{register:02X} verification failed: wrote 0x{wrote:02X}, read back 0x{read:02X}"
            ),
            AmpErrorKind::InvalidArgument { param, value } => {
                write!(f, "invalid argument {param}: value {value} out of range")
            }
        }
    }
}

speaker_common::impl_canonical_error_display!(AmpError);

impl std::error::Error for AmpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AmpErrorKind::I2c(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rppal::i2c::Error> for AmpError {
    fn from(err: rppal::i2c::Error) -> Self {
        Self::i2c(err)
    }
}

/// Test-bus seam: lets `embedded-hal` mock buses with `&'static str` errors
/// satisfy the `AmpError: From<E>` bound of [`HardwareDevice`] in integration
/// tests. Bus errors from such mocks are surfaced as mock faults.
impl From<&'static str> for AmpError {
    fn from(err: &'static str) -> Self {
        Self::mock_fault(err)
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
    // Write-path mock state (Phase 1.5). Stores the hardware `0x40` VLP
    // register encoding (24 − dB) to mirror `HardwareDevice` fidelity.
    mock_master_volume_reg: u16,
    mock_serial_muted: bool,
    mock_pm_ctrl: u8,
    mock_pm_profile: u8,
    mock_i2s_format: u8,
    mock_audio_in_mode: u8,
    // Phase 2 (Stage A): full-register mock fidelity for the VLP/limiter
    // enable bits — stores the complete `0x35` / `0x36` register values.
    mock_i2s_format_reg: u8,
    mock_audio_proc_reg: u8,
    mock_limiter_thresholds_dbfs: i16,
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
                // Write-path defaults
                mock_master_volume_reg: 168 - 116, // ≈ -28 dB (raw 116 ⇒ reg 52)
                mock_serial_muted: false,
                mock_pm_ctrl: 0x60, // manualPM=1 (bit6), PM_man=2 (bits5:4) -> PMP2
                mock_pm_profile: 2, // PMP2
                mock_i2s_format: 0, // Standard I2S
                mock_audio_in_mode: 0, // 20dB gain
                mock_i2s_format_reg: 0x00, // VLP bypassed (reset default)
                mock_audio_proc_reg: 0x01, // bit0 set (live reset default)
                mock_limiter_thresholds_dbfs: -15, // driver probe default
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

    // === Write-path mock methods (Phase 1.5) ===

    fn read_settings(&self) -> Result<AmpSettings, AmpError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        Ok(AmpSettings {
            master_volume_raw: decode_vlp_raw(state.mock_master_volume_reg),
            master_volume_frac: 0,
            serial_muted: state.mock_serial_muted,
            pm_ctrl_pm: (state.mock_pm_ctrl >> 4) & 0x03,
            pm_profile: state.mock_pm_profile & 0x07,
            i2s_format: state.mock_i2s_format & 0x07,
            audio_in_gain: (state.mock_audio_in_mode >> 5) & 0x03,
        })
    }

    fn write_master_volume(&self, raw: u16) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        if raw > 144 {
            return Err(AmpError::invalid_argument("master_volume_raw", raw));
        }
        // Store the same encoded register value as hardware: int = 168 − raw, frac = 0
        state.mock_master_volume_reg = 168 - raw;
        Ok(())
    }

    fn write_serial_mute(&self, muted: bool) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        // Models the hardware RMW outcome on register 0x36 bit 7 (audio_proc_mute);
        // verify-by-re-read always succeeds in the mock unless a fault is injected.
        state.mock_serial_muted = muted;
        Ok(())
    }

    fn write_power_mode_ctrl(&self, pm: u8) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        // Store the same encoded register value as hardware: (pm & 0x03) << 4 | 0x40
        state.mock_pm_ctrl = 0x40 | ((pm & 0x03) << 4);
        Ok(())
    }

    fn write_power_mode_profile(&self, profile: u8) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        if profile > 4 {
            return Err(AmpError::invalid_argument("pm_profile", u16::from(profile)));
        }
        // Store encoded value: profile & 0x07
        state.mock_pm_profile = profile & 0x07;
        Ok(())
    }

    fn write_i2s_format(&self, format: u8) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        if format > 4 {
            return Err(AmpError::invalid_argument("i2s_format", u16::from(format)));
        }
        // Store encoded value: format & 0x07
        state.mock_i2s_format = format & 0x07;
        Ok(())
    }

    fn write_audio_in_mode(&self, gain: u8) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        if gain > 3 {
            return Err(AmpError::invalid_argument("audio_in_gain", u16::from(gain)));
        }
        // Store encoded value: (gain & 0x03) << 5
        state.mock_audio_in_mode = (gain & 0x03) << 5;
        Ok(())
    }

    fn enable_audio_processor(&self) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        state.mock_i2s_format_reg |= AUDIO_PROC_ENABLE_MASK;
        Ok(())
    }

    fn enable_limiter(&self) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        state.mock_audio_proc_reg |= LIMITER_ENABLE_MASK;
        Ok(())
    }

    fn write_limiter_thresholds(&self, dbfs: i16) -> Result<(), AmpError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(AmpError::mock_fault(fault.clone()));
        }
        if !(-144..=0).contains(&dbfs) {
            return Err(AmpError::invalid_argument(
                "limiter_threshold_dbfs",
                dbfs.unsigned_abs(),
            ));
        }
        state.mock_limiter_thresholds_dbfs = dbfs;
        Ok(())
    }

    // === Mock state setters for test inspection (Phase 1.5) ===

    /// Returns `true` when the mocked VLP is enabled (`0x35` bit 3).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_vlp_enabled(&self) -> bool {
        self.state
            .read()
            .expect("mock lock poisoned")
            .mock_i2s_format_reg
            & AUDIO_PROC_ENABLE_MASK
            != 0
    }

    /// Returns `true` when the mocked limiter is enabled (`0x36` bit 6).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_limiter_enabled(&self) -> bool {
        self.state
            .read()
            .expect("mock lock poisoned")
            .mock_audio_proc_reg
            & LIMITER_ENABLE_MASK
            != 0
    }

    /// Gets the mocked limiter threshold (dBFS).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_limiter_thresholds(&self) -> i16 {
        self.state
            .read()
            .expect("mock lock poisoned")
            .mock_limiter_thresholds_dbfs
    }

    /// Get the mocked master volume raw value.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_master_volume_raw(&self) -> u16 {
        decode_vlp_raw(
            self.state
                .read()
                .expect("mock lock poisoned")
                .mock_master_volume_reg,
        )
    }

    /// Get the mocked serial mute state.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_serial_muted(&self) -> bool {
        self.state
            .read()
            .expect("mock lock poisoned")
            .mock_serial_muted
    }

    /// Get the mocked `PM_CTRL` decoded `PM_man` value (0-3).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_pm_ctrl(&self) -> u8 {
        let encoded = self.state.read().expect("mock lock poisoned").mock_pm_ctrl;
        (encoded >> 4) & 0x03
    }

    /// Get the mocked `PM_PROFILE` value (0-4).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_pm_profile(&self) -> u8 {
        self.state
            .read()
            .expect("mock lock poisoned")
            .mock_pm_profile
    }

    /// Get the mocked I2S format value (0-4).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_i2s_format(&self) -> u8 {
        self.state
            .read()
            .expect("mock lock poisoned")
            .mock_i2s_format
    }

    /// Get the mocked audio in mode gain value (0-3).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn mock_audio_in_mode(&self) -> u8 {
        let encoded = self
            .state
            .read()
            .expect("mock lock poisoned")
            .mock_audio_in_mode;
        (encoded >> 5) & 0x03
    }

    /// Set the mocked master volume raw value directly (for test setup).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_mock_master_volume_raw(&self, raw: u16) {
        self.state
            .write()
            .expect("mock lock poisoned")
            .mock_master_volume_reg = 168 - raw;
    }

    /// Set the mocked serial mute state directly (for test setup).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_mock_serial_muted(&self, muted: bool) {
        self.state
            .write()
            .expect("mock lock poisoned")
            .mock_serial_muted = muted;
    }

    /// Set the mocked `PM_CTRL` value directly (for test setup).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_mock_pm_ctrl(&self, pm: u8) {
        self.state.write().expect("mock lock poisoned").mock_pm_ctrl = pm;
    }

    /// Set the mocked `PM_PROFILE` value directly (for test setup).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_mock_pm_profile(&self, profile: u8) {
        self.state
            .write()
            .expect("mock lock poisoned")
            .mock_pm_profile = profile;
    }

    /// Set the mocked I2S format value directly (for test setup).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_mock_i2s_format(&self, format: u8) {
        self.state
            .write()
            .expect("mock lock poisoned")
            .mock_i2s_format = format;
    }

    /// Set the mocked audio in mode value directly (for test setup).
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_mock_audio_in_mode(&self, gain: u8) {
        self.state
            .write()
            .expect("mock lock poisoned")
            .mock_audio_in_mode = gain;
    }
}

/// Internal hardware peripheral device wrapping direct I2C bus transactions.
///
/// Generic over `embedded_hal::blocking::i2c` traits to enable testing with embedded-hal mocks.
/// Production code instantiates with `rppal::hal::I2c` (enabled by `rppal` `hal` feature).
#[allow(
    missing_debug_implementations,
    reason = "Generic over I2C which may not implement Debug"
)]
pub struct HardwareDevice<I2C> {
    i2c: I2C,
    address: AmpI2cAddress,
}

impl<I2C, E> HardwareDevice<I2C>
where
    I2C: Write<Error = E> + Read<Error = E> + WriteRead<Error = E>,
    AmpError: From<E>,
{
    pub fn new(i2c: I2C, address: AmpI2cAddress) -> Self {
        // Probe is done by caller (with_bus_and_address uses set_slave_address)
        // For embedded-hal mocks, no probe needed
        Self { i2c, address }
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, AmpError> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address.as_u8(), &[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), AmpError> {
        // Plain write transaction: `write_read` would clock a dummy byte out of
        // the device via repeated-start read, which is not part of the
        // MA12070P register-write protocol.
        self.i2c.write(self.address.as_u8(), &[reg, val])?;
        Ok(())
    }

    fn write_regs(&mut self, regs: &[(u8, u8)]) -> Result<(), AmpError> {
        for (reg, val) in regs {
            self.write_reg(*reg, *val)?;
        }
        Ok(())
    }

    fn read_regs<const N: usize>(&mut self, regs: [u8; N]) -> Result<[u8; N], AmpError> {
        let mut out = [0u8; N];
        for (slot, reg) in out.iter_mut().zip(regs) {
            *slot = self.read_reg(reg)?;
        }
        Ok(out)
    }

    fn read_status(&mut self) -> Result<AmpStatus, AmpError> {
        const STATUS_REGISTERS: [u8; 6] = [
            REG_ERROR_LIVE,
            REG_ERROR_ACC,
            REG_POWER_MODE,
            REG_DCU_MON0,
            REG_DCU_MON1,
            REG_AUDIO_IN,
        ];
        Ok(AmpStatus::from_raw_registers(
            self.read_regs(STATUS_REGISTERS)?,
        ))
    }

    fn clear_latched_errors(&mut self) -> Result<(), AmpError> {
        let cur = self.read_reg(REG_EH_CLEAR)?;
        // Drive clear bit low first, then high, then low to guarantee a rising edge transition
        // regardless of initial bit state.
        for val in AmpStatus::eh_clear_pulse_sequence(cur) {
            self.write_reg(REG_EH_CLEAR, val)?;
        }
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

    /// Reads the Phase 1.5 configuration registers as a decoded [`AmpSettings`].
    ///
    /// Public so host tests (embedded-hal mock buses) can exercise the read path,
    /// mirroring the public Phase 1.5 write methods.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    pub fn read_settings(&mut self) -> Result<AmpSettings, AmpError> {
        const SETTINGS_REGISTERS: [u8; 7] = [
            REG_PM_CTRL,
            REG_PM_PROFILE,
            REG_AUDIO_IN_MODE,
            REG_I2S_FORMAT,
            REG_AUDIO_PROC,
            REG_VOL_DB_MASTER,
            REG_VOL_LSB_MASTER,
        ];
        Ok(AmpSettings::from_raw_registers(
            self.read_regs(SETTINGS_REGISTERS)?,
        ))
    }

    // === Write operations (Phase 1.5) ===

    /// Write master volume (user-scale raw = −dB, 0 = −144 dB mute, 144 = 0 dB).
    ///
    /// The MA12070P VLP encodes gain as `register = 24 − dB` (datasheet
    /// Table 8-9: 0x18 = 0.00 dB, 0xA8 = −144.00 dB, boost below 0x18), so the
    /// appliance user-scale raw (raw = −dB) maps to **`reg = 168 − raw`**.
    /// Live-verified on the Pi (2026-11-11): `0xA8` cuts playback, `0x30`
    /// (−24 dB) audibly attenuates. The fractional register 0x41 stays 0
    /// (`amp_curve` produces integer dB).
    ///
    /// Requires the VLP to be enabled (`0x35` bit 3 `audio_proc_enable`,
    /// see [`Self::enable_audio_processor`]) — writes have no audible effect
    /// while the VLP is bypassed.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `raw > 144`.
    pub fn write_master_volume(&mut self, raw: u16) -> Result<(), AmpError> {
        const RAW_MAX: u16 = 144;
        if raw > RAW_MAX {
            return Err(AmpError::invalid_argument("master_volume_raw", raw));
        }
        let int_reg = (VLP_ZERO_DB_REG - raw) as u8;
        let frac_reg = 0; // amp_curve produces integer dB; frac reg reserved for 0.25 dB steps
        self.write_regs(&[(REG_VOL_DB_MASTER, int_reg), (REG_VOL_LSB_MASTER, frac_reg)])
    }

    /// Enable the VLP (Volume & Limiter Processor): RMW set `0x35` bit 3
    /// (`audio_proc_enable`). The VLP must be enabled for master/channel
    /// volume and limiter settings to reach the audio path; the original
    /// kernel driver `probe()` always set this bit.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    pub fn enable_audio_processor(&mut self) -> Result<(), AmpError> {
        let reg = self.read_reg(REG_I2S_FORMAT)?;
        self.write_reg(REG_I2S_FORMAT, reg | AUDIO_PROC_ENABLE_MASK)
    }

    /// Enable the VLP output limiter: RMW set `0x36` bit 6
    /// (`audio_proc_limiterEnable`). Reduces gain when the output approaches
    /// the programmed threshold (protects the loudspeakers from clipping).
    /// The original kernel driver enabled it with −15 dBFS thresholds.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    pub fn enable_limiter(&mut self) -> Result<(), AmpError> {
        let reg = self.read_reg(REG_AUDIO_PROC)?;
        self.write_reg(REG_AUDIO_PROC, reg | LIMITER_ENABLE_MASK)
    }

    /// Write the limiter threshold for all four channels (0x47..0x4A).
    ///
    /// `dbfs` is the limiter threshold in dBFS (0 ..= −144); the register
    /// uses the same `24 − dB` VLP mapping (driver default −15 dBFS ⇒ 0x27).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `dbfs` is out of range.
    pub fn write_limiter_thresholds(&mut self, dbfs: i16) -> Result<(), AmpError> {
        if !(-144..=0).contains(&dbfs) {
            return Err(AmpError::invalid_argument(
                "limiter_threshold_dbfs",
                dbfs.unsigned_abs(),
            ));
        }
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "range-checked above: 24 - dbfs is 24..=168, fits u8"
        )]
        let reg = (24 - dbfs) as u8;
        self.write_regs(&[
            (REG_THR_DB_CH0, reg),
            (REG_THR_DB_CH0 + 1, reg),
            (REG_THR_DB_CH0 + 2, reg),
            (REG_THR_DB_CH0 + 3, reg),
        ])
    }

    /// RMW of serial mute register 0x36 bit 7 with verify-by-re-read.
    ///
    /// The serial bit is the only software mute the appliance relies on:
    /// `/MUTE` (GPIO15) is boot-time wiring only, the kernel-era `ma120x0p`
    /// driver never rewrote `0x36`, and live verification on the Pi
    /// (2026-11) confirmed a read-modify-write of bit 7 mutes/restores
    /// playback mid-stream and persists. The bit resets to the unmuted
    /// power-on default on an amp power cycle; the next write re-applies
    /// it. The RMW preserves limiter bit 6 and all other `0x36` bits.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or read-back verification fails.
    pub fn write_serial_mute(&mut self, muted: bool) -> Result<(), AmpError> {
        let reg = self.read_reg(REG_AUDIO_PROC)?;
        let desired = apply_mute_bit(reg, muted);
        self.write_reg(REG_AUDIO_PROC, desired)?;
        let verify = self.read_reg(REG_AUDIO_PROC)?;
        if is_muted_bit(verify) != muted {
            return Err(AmpError::verification(REG_AUDIO_PROC, desired, verify));
        }
        Ok(())
    }

    /// RMW of `PM_CTRL` (0x00): set manualPM=1, `PM_man` from `PowerMode` (0-3).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    pub fn write_power_mode_ctrl(&mut self, pm: u8) -> Result<(), AmpError> {
        let reg = self.read_reg(REG_PM_CTRL)?;
        let desired = (reg & 0x0F) | 0x40 | ((pm & 0x03) << 4);
        self.write_reg(REG_PM_CTRL, desired)
    }

    /// Write `PM_PROFILE` (0x1D) bits 2:0 (0-4).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `profile > 4`.
    pub fn write_power_mode_profile(&mut self, profile: u8) -> Result<(), AmpError> {
        if profile > 4 {
            return Err(AmpError::invalid_argument("pm_profile", u16::from(profile)));
        }
        let reg = self.read_reg(REG_PM_PROFILE)?;
        let desired = (reg & 0xF8) | (profile & 0x07);
        self.write_reg(REG_PM_PROFILE, desired)
    }

    /// RMW of I2S format register 0x35 **bits 2:0 only**.
    ///
    /// Bits 7:3 of `0x35` hold limiter attack (7:6), limiter release (5:4),
    /// and `audio_proc_enable` (bit 3, the VLP bypass mux) — clearing any of
    /// them silently bypasses volume control and misconfigures the limiter.
    /// (Phase 0 live finding: the original `0xF8` mask disabled the VLP.)
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `format > 4`.
    pub fn write_i2s_format(&mut self, format: u8) -> Result<(), AmpError> {
        if format > 4 {
            return Err(AmpError::invalid_argument("i2s_format", u16::from(format)));
        }
        let reg = self.read_reg(REG_I2S_FORMAT)?;
        let desired = (reg & 0xF8) | (format & 0x07);
        self.write_reg(REG_I2S_FORMAT, desired)
    }

    /// RMW of audio in mode register 0x25 bits 6:5.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `gain > 3`.
    pub fn write_audio_in_mode(&mut self, gain: u8) -> Result<(), AmpError> {
        if gain > 3 {
            return Err(AmpError::invalid_argument("audio_in_gain", u16::from(gain)));
        }
        let reg = self.read_reg(REG_AUDIO_IN_MODE)?;
        let desired = (reg & 0x9F) | ((gain & 0x03) << 5);
        self.write_reg(REG_AUDIO_IN_MODE, desired)
    }
}

/// Internal I2C / mock backend dispatch for [`AmpDevice`].
enum Backend {
    Hardware {
        device: Mutex<HardwareDevice<I2c>>,
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
/// use speaker_hardware::AmpDevice;
///
/// let (amp, mock) = AmpDevice::new_mocked();
/// let status = amp.read_status()?;
/// assert!(status.live_errors.is_clear());
/// # Ok::<(), speaker_hardware::AmpError>(())
/// ```
#[derive(Clone, Debug)]
pub struct AmpDevice {
    inner: Arc<Backend>,
}

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
        let mut i2c = I2c::with_bus(bus)?;
        i2c.set_slave_address(address.as_u16())?;
        let device = HardwareDevice::new(i2c, address);
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

    /// Reads the Phase 1.5 configuration values (volume, mute, power mode, I2S format, gain).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_settings(&self) -> Result<AmpSettings, AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .read_settings(),
            Backend::Mock(mock) => mock.read_settings(),
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

    /// Asynchronously reads the Phase 1.5 configuration values without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if reading fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn read_settings_async(&self) -> Result<AmpSettings, AmpError> {
        let this = self.clone();
        run_blocking("MA12070P settings read", move || this.read_settings()).await
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

    // === Write operations (Phase 1.5) ===

    /// Write master volume (0x40 integer dB, 0x41 fractional 0.25 dB).
    /// Raw = -dB (0 = -144 dB mute, 144 = 0 dB). Caller maps percent→raw via `amp_curve::PERCENT_TO_RAW`.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `raw > 144`.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_master_volume(&self, raw: u16) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_master_volume(raw),
            Backend::Mock(mock) => mock.write_master_volume(raw),
        }
    }

    /// Write serial mute (0x36 bit 7) with verify-by-re-read.
    /// Live-verified implementation.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or verification mismatches.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_serial_mute(&self, muted: bool) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_serial_mute(muted),
            Backend::Mock(mock) => mock.write_serial_mute(muted),
        }
    }

    /// Write `PM_CTRL` (0x00): set manualPM=1, `PM_man` from `PowerMode` (0-3).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_power_mode_ctrl(&self, pm: u8) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_power_mode_ctrl(pm),
            Backend::Mock(mock) => mock.write_power_mode_ctrl(pm),
        }
    }

    /// Write `PM_PROFILE` (0x1D) bits 2:0 (0-4).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_power_mode_profile(&self, profile: u8) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_power_mode_profile(profile),
            Backend::Mock(mock) => mock.write_power_mode_profile(profile),
        }
    }

    /// Write I2S format (0x35) bits 2:0.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_i2s_format(&self, format: u8) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_i2s_format(format),
            Backend::Mock(mock) => mock.write_i2s_format(format),
        }
    }

    /// Write audio in mode (0x25) bits 6:5 gain select.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_audio_in_mode(&self, gain: u8) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_audio_in_mode(gain),
            Backend::Mock(mock) => mock.write_audio_in_mode(gain),
        }
    }

    // === Async write wrappers ===

    /// Asynchronously write master volume without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_master_volume_async(&self, raw: u16) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write master volume", move || {
            this.write_master_volume(raw)
        })
        .await
    }

    /// Asynchronously write serial mute without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or verification mismatches.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_serial_mute_async(&self, muted: bool) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write serial mute", move || {
            this.write_serial_mute(muted)
        })
        .await
    }

    /// Asynchronously write `PM_CTRL` without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_power_mode_ctrl_async(&self, pm: u8) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write PM_CTRL", move || {
            this.write_power_mode_ctrl(pm)
        })
        .await
    }

    /// Asynchronously write `PM_PROFILE` without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_power_mode_profile_async(&self, profile: u8) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write PM_PROFILE", move || {
            this.write_power_mode_profile(profile)
        })
        .await
    }

    /// Asynchronously write I2S format without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_i2s_format_async(&self, format: u8) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write I2S format", move || {
            this.write_i2s_format(format)
        })
        .await
    }

    /// Asynchronously write audio in mode without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_audio_in_mode_async(&self, gain: u8) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write audio in mode", move || {
            this.write_audio_in_mode(gain)
        })
        .await
    }

    /// Asynchronously enable the VLP (`0x35` bit 3 `audio_proc_enable`).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn enable_audio_processor_async(&self) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P enable audio processor", move || {
            this.enable_audio_processor()
        })
        .await
    }

    /// Asynchronously enable the VLP output limiter (`0x36` bit 6).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn enable_limiter_async(&self) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P enable limiter", move || this.enable_limiter()).await
    }

    /// Asynchronously write limiter thresholds (all four channels).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `dbfs` is out of range.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics.
    pub async fn write_limiter_thresholds_async(&self, dbfs: i16) -> Result<(), AmpError> {
        let this = self.clone();
        run_blocking("MA12070P write limiter thresholds", move || {
            this.write_limiter_thresholds(dbfs)
        })
        .await
    }

    /// Enable the VLP (`0x35` bit 3 `audio_proc_enable`).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn enable_audio_processor(&self) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .enable_audio_processor(),
            Backend::Mock(mock) => mock.enable_audio_processor(),
        }
    }

    /// Enable the VLP output limiter (`0x36` bit 6).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn enable_limiter(&self) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .enable_limiter(),
            Backend::Mock(mock) => mock.enable_limiter(),
        }
    }

    /// Write limiter thresholds (all four channels).
    ///
    /// # Errors
    /// Returns [`AmpError`] if communication fails or `dbfs` is out of range.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn write_limiter_thresholds(&self, dbfs: i16) -> Result<(), AmpError> {
        match self.inner.as_ref() {
            Backend::Hardware { device, .. } => device
                .lock()
                .expect("hardware device lock poisoned")
                .write_limiter_thresholds(dbfs),
            Backend::Mock(mock) => mock.write_limiter_thresholds(dbfs),
        }
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AmpDevice>();
    assert_send_sync::<AmpDeviceMock>();
};

/// Test utilities for integration tests and unit tests.
pub mod test_helpers {
    use super::AmpError;

    /// Creates an `AmpError::invalid_address` error for testing.
    #[must_use]
    pub fn make_invalid_address_error(address: u8) -> AmpError {
        AmpError::invalid_address(address)
    }

    /// Creates an `AmpError::unsupported_bus` error for testing.
    #[must_use]
    pub fn make_unsupported_bus_error(bus: u8) -> AmpError {
        AmpError::unsupported_bus(bus)
    }

    /// Creates an `AmpError::cancelled` error for testing.
    #[must_use]
    pub fn make_cancelled_error(task: &str) -> AmpError {
        AmpError::cancelled(task)
    }

    /// Creates an `AmpError::mock_fault` error for testing.
    #[must_use]
    pub fn make_mock_fault_error(detail: &str) -> AmpError {
        AmpError::mock_fault(detail)
    }

    /// Re-export `HardwareDevice` for embedded-hal mock bus testing.
    pub use super::HardwareDevice;
}
