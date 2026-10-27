//! TOP-UP EC12 quadrature rotary encoder driver with typed domain models and inherent mocking.
//!
//! Provides strongly-typed, non-blocking interfaces for the EC12 rotary encoder, push
//! switch, and common-anode RGB status LED ring on the Raspberry Pi 3 GPIO header.
//!
//! # Hardware Configuration
//! - **Phase A:** BCM GPIO 25 (Physical Pin 22), active-LOW with internal pull-up
//! - **Phase B:** BCM GPIO 24 (Physical Pin 18), active-LOW with internal pull-up
//! - **Push Switch:** BCM GPIO 22 (Physical Pin 15), active-HIGH to 3.3V with internal pull-down
//! - **LED Red:** BCM GPIO 20 (Physical Pin 38, 330Ω series resistor)
//! - **LED Green:** BCM GPIO 16 (Physical Pin 36, 180Ω series resistor)
//! - **LED Blue:** BCM GPIO 12 (Physical Pin 32, 180Ω series resistor)
//! - **LED Common:** Anode tied to 3.3V; channels are active-LOW (`set_low` = ON)
//!
//! # Architecture
//! - [`EncoderDevice`] is cheaply cloneable (`Arc`-backed handle) and `Send + Sync`,
//!   providing non-blocking asynchronous APIs.
//! - Companion [`EncoderDeviceMock`] enables deterministic testing without physical hardware.
//! - Strongly typed domain models [`RotationDirection`], [`RgbColor`], and [`EncoderStatus`].

use std::backtrace::Backtrace;
use std::fmt::{self, Display, Formatter};
use std::sync::{Arc, Mutex, RwLock};

use rppal::gpio::{Gpio, InputPin, OutputPin};

use crate::runtime::{FromJoinError, run_blocking};

use super::{BcmPin, GpioPinInfo, PhysicalPin};

/// BCM GPIO pin for the EC12 encoder quadrature Phase A output.
pub const ENCODER_PHASE_A_BCM_GPIO_PIN: u8 = 25;

/// BCM GPIO pin for the EC12 encoder quadrature Phase B output.
pub const ENCODER_PHASE_B_BCM_GPIO_PIN: u8 = 24;

/// BCM GPIO pin for the EC12 encoder push switch.
pub const ENCODER_PUSH_SW_BCM_GPIO_PIN: u8 = 22;

/// BCM GPIO pin for the common-anode RGB indicator red channel.
pub const ENCODER_LED_RED_BCM_GPIO_PIN: u8 = 20;

/// BCM GPIO pin for the common-anode RGB indicator green channel.
pub const ENCODER_LED_GREEN_BCM_GPIO_PIN: u8 = 16;

/// BCM GPIO pin for the common-anode RGB indicator blue channel.
pub const ENCODER_LED_BLUE_BCM_GPIO_PIN: u8 = 12;

/// Rotational direction decoded from EC12 quadrature phase transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RotationDirection {
    /// Detent rotated clockwise (volume up).
    Clockwise,
    /// Detent rotated counter-clockwise (volume down).
    CounterClockwise,
}

impl RotationDirection {
    /// Canonical lowercase slug for telemetry and CLI flags.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Clockwise => "cw",
            Self::CounterClockwise => "ccw",
        }
    }

    /// Parses a direction slug (`cw`, `ccw`) into a [`RotationDirection`].
    ///
    /// # Errors
    /// Returns a descriptive user-facing error message if `s` is not a known direction slug.
    pub fn parse_slug(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cw" | "clockwise" => Ok(Self::Clockwise),
            "ccw" | "counterclockwise" | "counter-clockwise" => Ok(Self::CounterClockwise),
            other => Err(format!(
                "invalid rotation direction '{other}': expected 'cw' or 'ccw'"
            )),
        }
    }
}

impl Display for RotationDirection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// RGB indicator color for the common-anode LED ring (channel `true` = lit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RgbColor {
    /// Red channel enabled.
    pub red: bool,
    /// Green channel enabled.
    pub green: bool,
    /// Blue channel enabled.
    pub blue: bool,
}

impl RgbColor {
    /// All channels off.
    pub const OFF: Self = Self {
        red: false,
        green: false,
        blue: false,
    };
    /// Red only.
    pub const RED: Self = Self {
        red: true,
        green: false,
        blue: false,
    };
    /// Green only.
    pub const GREEN: Self = Self {
        red: false,
        green: true,
        blue: false,
    };
    /// Blue only.
    pub const BLUE: Self = Self {
        red: false,
        green: false,
        blue: true,
    };
    /// Red and green (yellow).
    pub const YELLOW: Self = Self {
        red: true,
        green: true,
        blue: false,
    };
    /// Green and blue (cyan).
    pub const CYAN: Self = Self {
        red: false,
        green: true,
        blue: true,
    };
    /// Red and blue (magenta).
    pub const MAGENTA: Self = Self {
        red: true,
        green: false,
        blue: true,
    };
    /// All channels lit (white).
    pub const WHITE: Self = Self {
        red: true,
        green: true,
        blue: true,
    };

    /// Returns the canonical lowercase color name for this combination.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match (self.red, self.green, self.blue) {
            (false, false, false) => "off",
            (true, false, false) => "red",
            (false, true, false) => "green",
            (false, false, true) => "blue",
            (true, true, false) => "yellow",
            (false, true, true) => "cyan",
            (true, false, true) => "magenta",
            (true, true, true) => "white",
        }
    }

    /// Parses a color name (`off`, `red`, ..., `white`) into an [`RgbColor`].
    ///
    /// # Errors
    /// Returns a descriptive user-facing error message if `s` is not a known color name.
    pub fn parse_name(s: &str) -> Result<Self, String> {
        let parse = |name: &'static str, color: Self| {
            if s.trim().eq_ignore_ascii_case(name) {
                Ok(color)
            } else {
                Err(())
            }
        };
        for (name, color) in [
            ("off", Self::OFF),
            ("red", Self::RED),
            ("green", Self::GREEN),
            ("blue", Self::BLUE),
            ("yellow", Self::YELLOW),
            ("cyan", Self::CYAN),
            ("magenta", Self::MAGENTA),
            ("white", Self::WHITE),
        ] {
            if parse(name, color).is_ok() {
                return Ok(color);
            }
        }
        Err(format!(
            "invalid LED color '{s}': expected one of off, red, green, blue, yellow, cyan, magenta, white"
        ))
    }
}

impl Display for RgbColor {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Decodes a quadrature phase transition into a rotation direction.
///
/// Replicates the EC12 baseline logic: CW for `(00 -> 01)` or `(11 -> 10)`,
/// CCW for `(00 -> 10)` or `(11 -> 01)`; all other transitions are ignored.
///
/// States are encoded as `(phase_a << 1) | phase_b` using raw pin levels.
#[must_use]
pub fn quadrature_direction(prev: u8, curr: u8) -> Option<RotationDirection> {
    match (prev, curr) {
        (0b00, 0b01) | (0b11, 0b10) => Some(RotationDirection::Clockwise),
        (0b00, 0b10) | (0b11, 0b01) => Some(RotationDirection::CounterClockwise),
        _ => None,
    }
}

/// Point-in-time snapshot of encoder quadrature, switch, and LED state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderStatus {
    /// Raw Phase A pin level (`true` = high).
    pub phase_a: bool,
    /// Raw Phase B pin level (`true` = high).
    pub phase_b: bool,
    /// Push switch state (`true` = pressed).
    pub switch_pressed: bool,
    /// Cumulative clockwise detent count.
    pub cw_count: u64,
    /// Cumulative counter-clockwise detent count.
    pub ccw_count: u64,
    /// Cumulative push switch press count.
    pub press_count: u64,
    /// Cumulative push switch release count.
    pub release_count: u64,
    /// Currently latched RGB indicator color.
    pub led_color: RgbColor,
}

impl EncoderStatus {
    /// Returns the packed quadrature state byte (`(phase_a << 1) | phase_b`).
    #[must_use]
    pub fn quadrature_state(&self) -> u8 {
        (u8::from(self.phase_a) << 1) | u8::from(self.phase_b)
    }
}

impl Display for EncoderStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[EC12 Encoder Status: CW {} | CCW {} | Press {} / Release {}]",
            self.cw_count, self.ccw_count, self.press_count, self.release_count
        )?;
        writeln!(
            f,
            "  • Quadrature     : A={} B={} (state 0b{:02b})",
            u8::from(self.phase_a),
            u8::from(self.phase_b),
            self.quadrature_state()
        )?;
        writeln!(
            f,
            "  • Push Switch    : {}",
            if self.switch_pressed {
                "pressed"
            } else {
                "released"
            }
        )?;
        write!(f, "  • RGB Indicator  : {}", self.led_color)
    }
}

/// Hardware connection and GPIO pinout metadata for the EC12 encoder assembly.
#[derive(Debug, Clone)]
pub struct EncoderHardwareInfo {
    /// Quadrature Phase A GPIO descriptor.
    pub phase_a: GpioPinInfo,
    /// Quadrature Phase B GPIO descriptor.
    pub phase_b: GpioPinInfo,
    /// Push switch GPIO descriptor.
    pub push_switch: GpioPinInfo,
    /// RGB indicator red channel GPIO descriptor.
    pub led_red: GpioPinInfo,
    /// RGB indicator green channel GPIO descriptor.
    pub led_green: GpioPinInfo,
    /// RGB indicator blue channel GPIO descriptor.
    pub led_blue: GpioPinInfo,
}

impl EncoderHardwareInfo {
    fn new_rpi3_unchecked() -> Self {
        const PHASE_A_BCM: BcmPin = BcmPin::new(ENCODER_PHASE_A_BCM_GPIO_PIN);
        const PHASE_A_PHYS: PhysicalPin = PhysicalPin::new(22);
        const PHASE_B_BCM: BcmPin = BcmPin::new(ENCODER_PHASE_B_BCM_GPIO_PIN);
        const PHASE_B_PHYS: PhysicalPin = PhysicalPin::new(18);
        const PUSH_SW_BCM: BcmPin = BcmPin::new(ENCODER_PUSH_SW_BCM_GPIO_PIN);
        const PUSH_SW_PHYS: PhysicalPin = PhysicalPin::new(15);
        const LED_R_BCM: BcmPin = BcmPin::new(ENCODER_LED_RED_BCM_GPIO_PIN);
        const LED_R_PHYS: PhysicalPin = PhysicalPin::new(38);
        const LED_G_BCM: BcmPin = BcmPin::new(ENCODER_LED_GREEN_BCM_GPIO_PIN);
        const LED_G_PHYS: PhysicalPin = PhysicalPin::new(36);
        const LED_B_BCM: BcmPin = BcmPin::new(ENCODER_LED_BLUE_BCM_GPIO_PIN);
        const LED_B_PHYS: PhysicalPin = PhysicalPin::new(32);

        Self {
            phase_a: GpioPinInfo {
                bcm_pin: PHASE_A_BCM,
                physical_pin: PHASE_A_PHYS,
                signal_name: "EC12 Quadrature Phase A",
                bus_protocol: "GPIO (bcm2835 gpio / rppal)",
                electrical_config: "Active-LOW, internal pull-up to 3.3V",
            },
            phase_b: GpioPinInfo {
                bcm_pin: PHASE_B_BCM,
                physical_pin: PHASE_B_PHYS,
                signal_name: "EC12 Quadrature Phase B",
                bus_protocol: "GPIO (bcm2835 gpio / rppal)",
                electrical_config: "Active-LOW, internal pull-up to 3.3V",
            },
            push_switch: GpioPinInfo {
                bcm_pin: PUSH_SW_BCM,
                physical_pin: PUSH_SW_PHYS,
                signal_name: "EC12 Push Switch",
                bus_protocol: "GPIO (bcm2835 gpio / rppal)",
                electrical_config: "Active-HIGH to 3.3V, internal pull-down",
            },
            led_red: GpioPinInfo {
                bcm_pin: LED_R_BCM,
                physical_pin: LED_R_PHYS,
                signal_name: "RGB Indicator Red",
                bus_protocol: "GPIO (bcm2835 gpio / rppal)",
                electrical_config: "Active-LOW common anode, 330Ω series resistor",
            },
            led_green: GpioPinInfo {
                bcm_pin: LED_G_BCM,
                physical_pin: LED_G_PHYS,
                signal_name: "RGB Indicator Green",
                bus_protocol: "GPIO (bcm2835 gpio / rppal)",
                electrical_config: "Active-LOW common anode, 180Ω series resistor",
            },
            led_blue: GpioPinInfo {
                bcm_pin: LED_B_BCM,
                physical_pin: LED_B_PHYS,
                signal_name: "RGB Indicator Blue",
                bus_protocol: "GPIO (bcm2835 gpio / rppal)",
                electrical_config: "Active-LOW common anode, 180Ω series resistor",
            },
        }
    }

    /// Returns the standard Raspberry Pi 3 GPIO pinout for the EC12 encoder assembly.
    #[must_use]
    pub fn rpi3_default() -> Self {
        Self::new_rpi3_unchecked()
    }
}

/// Categorized failure kinds for encoder operations.
#[derive(Debug)]
pub(crate) enum EncoderErrorKind {
    Gpio(rppal::gpio::Error),
    Cancelled { message: Box<str> },
    MockFault { message: Box<str> },
}

/// Canonical error returned by encoder drivers and utilities.
#[derive(Debug)]
pub struct EncoderError {
    kind: EncoderErrorKind,
    backtrace: Backtrace,
}

impl EncoderError {
    pub(crate) fn new(kind: EncoderErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    pub(crate) fn from_gpio_err(err: rppal::gpio::Error) -> Self {
        Self::new(EncoderErrorKind::Gpio(err))
    }

    pub(crate) fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(EncoderErrorKind::Cancelled {
            message: message.into(),
        })
    }

    pub(crate) fn mock_fault(message: impl Into<Box<str>>) -> Self {
        Self::new(EncoderErrorKind::MockFault {
            message: message.into(),
        })
    }

    /// Returns `true` if this error was caused by a GPIO access or configuration failure.
    #[must_use]
    pub const fn is_gpio(&self) -> bool {
        matches!(self.kind, EncoderErrorKind::Gpio(_))
    }

    /// Returns `true` if this error was caused by task cancellation or runtime shutdown.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, EncoderErrorKind::Cancelled { .. })
    }

    /// Returns `true` if this error was caused by an injected mock fault.
    #[must_use]
    pub const fn is_mock_fault(&self) -> bool {
        matches!(self.kind, EncoderErrorKind::MockFault { .. })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            EncoderErrorKind::Gpio(err) => write!(f, "GPIO error: {err}"),
            EncoderErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
            EncoderErrorKind::MockFault { message } => write!(f, "injected mock fault: {message}"),
        }
    }
}

super::impl_canonical_error_display!(EncoderError);

impl std::error::Error for EncoderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            EncoderErrorKind::Gpio(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rppal::gpio::Error> for EncoderError {
    fn from(err: rppal::gpio::Error) -> Self {
        Self::from_gpio_err(err)
    }
}

impl FromJoinError for EncoderError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

/// Controllable in-memory mock for [`EncoderDevice`].
#[derive(Debug, Clone)]
pub struct EncoderDeviceMock {
    state: Arc<RwLock<MockState>>,
}

#[derive(Debug)]
struct MockState {
    phase_a: bool,
    phase_b: bool,
    switch_pressed: bool,
    led_color: RgbColor,
    cw_count: u64,
    ccw_count: u64,
    press_count: u64,
    release_count: u64,
    fault: Option<Box<str>>,
}

impl EncoderDeviceMock {
    fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockState {
                phase_a: true,
                phase_b: true,
                switch_pressed: false,
                led_color: RgbColor::OFF,
                cw_count: 0,
                ccw_count: 0,
                press_count: 0,
                release_count: 0,
                fault: None,
            })),
        }
    }

    /// Sets the simulated quadrature Phase A pin level and decodes the transition.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_phase_a(&self, high: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        let phase_b = state.phase_b;
        apply_phase_transition(&mut state, high, phase_b);
    }

    /// Sets the simulated quadrature Phase B pin level and decodes the transition.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_phase_b(&self, high: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        let phase_a = state.phase_a;
        apply_phase_transition(&mut state, phase_a, high);
    }

    /// Sets the simulated push switch state, counting press and release transitions.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_switch_pressed(&self, pressed: bool) {
        let mut state = self.state.write().expect("mock lock poisoned");
        if pressed && !state.switch_pressed {
            state.press_count += 1;
        } else if !pressed && state.switch_pressed {
            state.release_count += 1;
        }
        state.switch_pressed = pressed;
    }

    /// Latches the simulated RGB indicator color without driving physical pins.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_leds(&self, color: RgbColor) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.led_color = color;
    }

    /// Injects or clears a simulated hardware fault.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    pub fn set_fault(&self, fault: Option<Box<str>>) {
        let mut state = self.state.write().expect("mock lock poisoned");
        state.fault = fault;
    }

    /// Returns the currently latched mock RGB indicator color.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned.
    #[must_use]
    pub fn led_color(&self) -> RgbColor {
        let state = self.state.read().expect("mock lock poisoned");
        state.led_color
    }

    fn read_status(&self) -> Result<EncoderStatus, EncoderError> {
        let state = self.state.read().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(EncoderError::mock_fault(fault.clone()));
        }
        Ok(EncoderStatus {
            phase_a: state.phase_a,
            phase_b: state.phase_b,
            switch_pressed: state.switch_pressed,
            cw_count: state.cw_count,
            ccw_count: state.ccw_count,
            press_count: state.press_count,
            release_count: state.release_count,
            led_color: state.led_color,
        })
    }

    fn set_led_color(&self, color: RgbColor) -> Result<(), EncoderError> {
        let mut state = self.state.write().expect("mock lock poisoned");
        if let Some(ref fault) = state.fault {
            return Err(EncoderError::mock_fault(fault.clone()));
        }
        state.led_color = color;
        Ok(())
    }
}

/// Applies a quadrature phase transition to the shared mock state.
fn apply_phase_transition(state: &mut MockState, phase_a: bool, phase_b: bool) {
    let prev = (u8::from(state.phase_a) << 1) | u8::from(state.phase_b);
    let curr = (u8::from(phase_a) << 1) | u8::from(phase_b);
    if let Some(direction) = quadrature_direction(prev, curr) {
        match direction {
            RotationDirection::Clockwise => state.cw_count += 1,
            RotationDirection::CounterClockwise => state.ccw_count += 1,
        }
    }
    state.phase_a = phase_a;
    state.phase_b = phase_b;
}

/// Internal hardware peripheral device wrapping the Raspberry Pi GPIO pins.
struct HardwareDevice {
    phase_a: InputPin,
    phase_b: InputPin,
    push_switch: InputPin,
    led_red: OutputPin,
    led_green: OutputPin,
    led_blue: OutputPin,
    last_quadrature: u8,
    last_switch: bool,
    cw_count: u64,
    ccw_count: u64,
    press_count: u64,
    release_count: u64,
    led_color: RgbColor,
}

impl HardwareDevice {
    fn new() -> Result<Self, EncoderError> {
        let gpio = Gpio::new()?;
        let phase_a = gpio.get(ENCODER_PHASE_A_BCM_GPIO_PIN)?.into_input_pullup();
        let phase_b = gpio.get(ENCODER_PHASE_B_BCM_GPIO_PIN)?.into_input_pullup();
        let push_switch = gpio
            .get(ENCODER_PUSH_SW_BCM_GPIO_PIN)?
            .into_input_pulldown();
        let mut led_red = gpio.get(ENCODER_LED_RED_BCM_GPIO_PIN)?.into_output();
        let mut led_green = gpio.get(ENCODER_LED_GREEN_BCM_GPIO_PIN)?.into_output();
        let mut led_blue = gpio.get(ENCODER_LED_BLUE_BCM_GPIO_PIN)?.into_output();

        // Common-anode ring: logic HIGH drives all channels OFF at construction.
        led_red.set_high();
        led_green.set_high();
        led_blue.set_high();

        let level_a = phase_a.is_high();
        let level_b = phase_b.is_high();
        let switch_level = push_switch.is_high();

        Ok(Self {
            phase_a,
            phase_b,
            push_switch,
            led_red,
            led_green,
            led_blue,
            last_quadrature: (u8::from(level_a) << 1) | u8::from(level_b),
            last_switch: switch_level,
            cw_count: 0,
            ccw_count: 0,
            press_count: 0,
            release_count: 0,
            led_color: RgbColor::OFF,
        })
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "keeps the internal op signature aligned with the fallible mock and backend dispatch"
    )]
    fn read_status(&mut self) -> Result<EncoderStatus, EncoderError> {
        let phase_a = self.phase_a.is_high();
        let phase_b = self.phase_b.is_high();
        let switch_pressed = self.push_switch.is_high();

        let curr = (u8::from(phase_a) << 1) | u8::from(phase_b);
        if let Some(direction) = quadrature_direction(self.last_quadrature, curr) {
            match direction {
                RotationDirection::Clockwise => self.cw_count += 1,
                RotationDirection::CounterClockwise => self.ccw_count += 1,
            }
        }
        self.last_quadrature = curr;

        if switch_pressed && !self.last_switch {
            self.press_count += 1;
        } else if !switch_pressed && self.last_switch {
            self.release_count += 1;
        }
        self.last_switch = switch_pressed;

        Ok(EncoderStatus {
            phase_a,
            phase_b,
            switch_pressed,
            cw_count: self.cw_count,
            ccw_count: self.ccw_count,
            press_count: self.press_count,
            release_count: self.release_count,
            led_color: self.led_color,
        })
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "keeps the internal op signature aligned with the fallible mock and backend dispatch"
    )]
    fn set_led_color(&mut self, color: RgbColor) -> Result<(), EncoderError> {
        // Common-anode ring: channels are active-LOW (set_low = ON).
        if color.red {
            self.led_red.set_low();
        } else {
            self.led_red.set_high();
        }
        if color.green {
            self.led_green.set_low();
        } else {
            self.led_green.set_high();
        }
        if color.blue {
            self.led_blue.set_low();
        } else {
            self.led_blue.set_high();
        }
        self.led_color = color;
        Ok(())
    }
}

/// Internal GPIO / mock backend dispatch for [`EncoderDevice`].
enum Backend {
    Hardware { device: Box<Mutex<HardwareDevice>> },
    Mock(EncoderDeviceMock),
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hardware { .. } => f.debug_struct("Hardware").finish_non_exhaustive(),
            Self::Mock(mock) => f.debug_tuple("Mock").field(mock).finish(),
        }
    }
}

/// Cheaply cloneable abstraction for the EC12 encoder, push switch, and RGB indicator.
#[derive(Clone, Debug)]
pub struct EncoderDevice {
    inner: Arc<Backend>,
}

impl EncoderDevice {
    /// Creates a new [`EncoderDevice`] using the standard Raspberry Pi 3 GPIO pinout.
    ///
    /// # Errors
    /// Returns [`EncoderError`] if the GPIO peripheral cannot be opened or a pin claim fails.
    pub fn new() -> Result<Self, EncoderError> {
        let device = HardwareDevice::new()?;
        Ok(Self {
            inner: Arc::new(Backend::Hardware {
                device: Box::new(Mutex::new(device)),
            }),
        })
    }

    /// Creates a mocked [`EncoderDevice`] and returns its companion [`EncoderDeviceMock`].
    #[must_use]
    pub fn new_mocked() -> (Self, EncoderDeviceMock) {
        let mock = EncoderDeviceMock::new();
        let device = Self {
            inner: Arc::new(Backend::Mock(mock.clone())),
        };
        (device, mock)
    }

    /// Returns hardware and GPIO pinout metadata for this encoder assembly.
    #[must_use]
    pub fn hardware_info(&self) -> EncoderHardwareInfo {
        EncoderHardwareInfo::new_rpi3_unchecked()
    }

    /// Samples quadrature phases and the push switch, updating cumulative counters.
    ///
    /// Quadrature transitions are decoded per the EC12 baseline table; push switch
    /// press and release transitions increment their respective counters.
    ///
    /// # Errors
    /// Returns [`EncoderError`] if GPIO reads fail.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn read_status(&self) -> Result<EncoderStatus, EncoderError> {
        match self.inner.as_ref() {
            Backend::Hardware { device } => device
                .lock()
                .expect("hardware device lock poisoned")
                .read_status(),
            Backend::Mock(mock) => mock.read_status(),
        }
    }

    /// Drives the common-anode RGB indicator to `color` (active-LOW channels).
    ///
    /// # Errors
    /// Returns [`EncoderError`] if GPIO writes fail.
    ///
    /// # Panics
    /// Panics if an internal mutex lock is poisoned.
    pub fn set_leds(&self, color: RgbColor) -> Result<(), EncoderError> {
        match self.inner.as_ref() {
            Backend::Hardware { device } => device
                .lock()
                .expect("hardware device lock poisoned")
                .set_led_color(color),
            Backend::Mock(mock) => mock.set_led_color(color),
        }
    }

    /// Asynchronously samples encoder state without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`EncoderError`] if GPIO reads fail.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_status_async(&self) -> Result<EncoderStatus, EncoderError> {
        let this = self.clone();
        run_blocking("EC12 encoder status read", move || this.read_status()).await
    }

    /// Asynchronously drives the RGB indicator without blocking the async runtime.
    ///
    /// # Errors
    /// Returns [`EncoderError`] if GPIO writes fail.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn set_leds_async(&self, color: RgbColor) -> Result<(), EncoderError> {
        let this = self.clone();
        run_blocking("EC12 encoder LED write", move || this.set_leds(color)).await
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<EncoderDevice>();
    assert_send_sync::<EncoderDeviceMock>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::abort_and_map;

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
    fn quadrature_direction_matches_baseline_table() {
        assert_eq!(
            quadrature_direction(0b00, 0b01),
            Some(RotationDirection::Clockwise)
        );
        assert_eq!(
            quadrature_direction(0b11, 0b10),
            Some(RotationDirection::Clockwise)
        );
        assert_eq!(
            quadrature_direction(0b00, 0b10),
            Some(RotationDirection::CounterClockwise)
        );
        assert_eq!(
            quadrature_direction(0b11, 0b01),
            Some(RotationDirection::CounterClockwise)
        );
        assert_eq!(quadrature_direction(0b01, 0b11), None);
        assert_eq!(quadrature_direction(0b10, 0b00), None);
        assert_eq!(quadrature_direction(0b01, 0b10), None);
        assert_eq!(quadrature_direction(0b10, 0b01), None);
        assert_eq!(quadrature_direction(0b00, 0b00), None);
        assert_eq!(quadrature_direction(0b01, 0b01), None);
        assert_eq!(quadrature_direction(0b11, 0b11), None);
        assert_eq!(quadrature_direction(0b10, 0b10), None);
    }

    #[test]
    fn encoder_status_display_and_quadrature_state() {
        let status = EncoderStatus {
            phase_a: true,
            phase_b: false,
            switch_pressed: true,
            cw_count: 3,
            ccw_count: 2,
            press_count: 1,
            release_count: 0,
            led_color: RgbColor::GREEN,
        };
        assert_eq!(status.quadrature_state(), 0b10);
        let text = status.to_string();
        assert!(text.contains("CW 3 | CCW 2"));
        assert!(text.contains("Press 1 / Release 0"));
        assert!(text.contains("state 0b10"));
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
        assert!(!gpio_err.is_cancelled());
        assert!(!gpio_err.is_mock_fault());
        assert!(gpio_err.summary().contains("GPIO error:"));

        let cancelled = EncoderError::cancelled("task went away");
        assert!(cancelled.is_cancelled());
        assert!(
            cancelled
                .summary()
                .contains("task cancelled: task went away")
        );

        let fault = EncoderError::mock_fault("injected");
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
    fn mock_quadrature_transitions_count_detents() {
        let (device, mock) = EncoderDevice::new_mocked();

        let status = device.read_status().unwrap();
        assert!(status.phase_a && status.phase_b);
        assert_eq!(status.cw_count, 0);
        assert_eq!(status.ccw_count, 0);
        assert!(!status.switch_pressed);
        assert_eq!(status.led_color, RgbColor::OFF);

        // 11 -> 01 is CCW per the baseline table.
        mock.set_phase_a(false);
        let status = device.read_status().unwrap();
        assert_eq!(status.ccw_count, 1);
        assert!(!status.phase_a);

        // 01 -> 00 ignored, then 00 -> 01 is CW per the baseline table.
        mock.set_phase_b(false);
        mock.set_phase_b(true);
        mock.set_phase_a(true);
        let status = device.read_status().unwrap();
        assert_eq!(status.cw_count, 1);
        assert_eq!(status.ccw_count, 1);
        assert_eq!(status.quadrature_state(), 0b11);
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
        let err: EncoderError = abort_and_map("EC12 test task").await;
        assert!(err.is_cancelled());
        assert!(err.summary().contains("EC12 test task"));
    }

    #[test]
    fn debug_formatting_reports_mock_backend() {
        let (device, _mock) = EncoderDevice::new_mocked();
        let debug = format!("{device:?}");
        assert!(debug.contains("Mock"));
    }
}
