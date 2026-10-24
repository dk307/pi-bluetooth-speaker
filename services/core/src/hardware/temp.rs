//! Dallas / Maxim DS18B20 1-Wire digital temperature sensor driver.
//!
//! Reads chassis temperature telemetry from DS18B20 sensors connected to the
//! Raspberry Pi 1-Wire bus on **BCM GPIO 26 (Physical Pin 37)** via the Linux
//! kernel `w1-gpio` and `w1_therm` sysfs interface (`/sys/bus/w1/devices/28-*/w1_slave`).
//!
//! # Threading & Async Model
//! Physical 1-Wire conversion reads take approximately 750 ms in the kernel
//! driver. Callers running inside the Tokio runtime should use the `*_async`
//! methods ([`TempSensor::read_all_async`], [`TempSensor::read_primary_temperature_async`]),
//! which offload blocking sysfs file I/O to [`tokio::task::spawn_blocking`].

use std::backtrace::Backtrace;
use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::hardware::{BcmPin, GpioPinInfo, PhysicalPin};
use crate::runtime::{FromJoinError, run_blocking};

/// BCM GPIO pin assigned to the DS18B20 1-Wire data line (`BCM GPIO 26`).
pub const DS18B20_BCM_GPIO_PIN: BcmPin = BcmPin::new(26);

/// Physical 40-pin header pin assigned to the DS18B20 1-Wire data line (`Physical Pin 37`).
pub const DS18B20_PHYSICAL_PIN: PhysicalPin = PhysicalPin::new(37);

/// Device tree overlay configuring the 1-Wire bus in `/boot/firmware/config.txt`.
pub const DS18B20_DTOVERLAY: &str = "dtoverlay=w1-gpio,gpiopin=26,pullup=0";

/// Default Linux sysfs directory containing discovered 1-Wire slave devices.
pub const DS18B20_SYSFS_BASE_PATH: &str = "/sys/bus/w1/devices";

/// Default Linux sysfs path for the BCM2837 `SoC` thermal sensor.
pub const SOC_THERMAL_SYSFS_PATH: &str = "/sys/devices/virtual/thermal/thermal_zone0/temp";

/// Mandatory 1-Wire family prefix for Dallas DS18B20 digital thermometers (`0x28`).
const DS18B20_FAMILY_PREFIX: &str = "28-";

/// Standard format description for DS18B20 sensor identifiers.
pub(crate) const SENSOR_ID_FORMAT_HELP: &str = "expected '28-<hex>' format";

/// Minimum operating temperature supported by the DS18B20 hardware (`-55.0 °C` in millidegrees).
const MIN_MILLIDEGREES_C: i32 = -55_000;

/// Maximum operating temperature supported by the DS18B20 hardware (`+125.0 °C` in millidegrees).
const MAX_MILLIDEGREES_C: i32 = 125_000;

/// Validated temperature measurement from a DS18B20 sensor.
///
/// Internally stores temperature in millidegrees Celsius (`1/1000 °C`), matching
/// the exact fixed-point representation emitted by the Linux `w1_therm` driver
/// and guarding the DS18B20 hardware operating bounds (`-55.0 °C..=+125.0 °C`).
///
/// # Examples
/// ```
/// use speaker_core::hardware::Temperature;
///
/// let temp = Temperature::from_millidegrees_celsius(26_437)?;
/// assert!((temp.celsius() - 26.437).abs() < 1e-3);
/// assert!((temp.fahrenheit() - 79.5866).abs() < 1e-2);
/// # Ok::<(), speaker_core::hardware::TempError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Temperature {
    millidegrees_c: i32,
}

impl Temperature {
    /// Creates a `Temperature` from millidegrees Celsius (`-55_000..=125_000`) in `const` contexts.
    ///
    /// # Panics
    /// Panics if `millidegrees_c` is outside `-55_000..=125_000`. Prefer
    /// [`Temperature::from_millidegrees_celsius`] for fallible runtime construction.
    #[must_use]
    pub const fn new_millidegrees_celsius(millidegrees_c: i32) -> Self {
        assert!(
            millidegrees_c >= MIN_MILLIDEGREES_C && millidegrees_c <= MAX_MILLIDEGREES_C,
            "temperature millidegrees_c must be within DS18B20 range (-55_000..=125_000)"
        );
        Self { millidegrees_c }
    }

    /// Creates a `Temperature` from millidegrees Celsius (`-55_000..=125_000`).
    ///
    /// # Errors
    /// Returns [`TempError`] with [`TempError::is_out_of_range`] set to `true` if
    /// `millidegrees_c` falls outside the DS18B20 operating range (`-55 °C..=+125 °C`).
    pub fn from_millidegrees_celsius(millidegrees_c: i32) -> Result<Self, TempError> {
        if !(MIN_MILLIDEGREES_C..=MAX_MILLIDEGREES_C).contains(&millidegrees_c) {
            return Err(TempError::new(ErrorKind::OutOfRange { millidegrees_c }));
        }
        Ok(Self { millidegrees_c })
    }

    /// Creates a `Temperature` from degrees Celsius (`-55.0..=125.0`).
    ///
    /// # Errors
    /// Returns [`TempError`] with [`TempError::is_out_of_range`] set to `true` if
    /// `celsius` is `NaN`, infinite, or outside `-55.0..=125.0`.
    pub fn from_celsius(celsius: f32) -> Result<Self, TempError> {
        if !celsius.is_finite() {
            return Err(TempError::new(ErrorKind::OutOfRange {
                millidegrees_c: i32::MIN,
            }));
        }
        let rounded = (celsius * 1000.0).round();
        if rounded < (MIN_MILLIDEGREES_C as f32) || rounded > (MAX_MILLIDEGREES_C as f32) {
            return Err(TempError::new(ErrorKind::OutOfRange {
                millidegrees_c: rounded as i32,
            }));
        }
        Self::from_millidegrees_celsius(rounded as i32)
    }

    /// Returns the raw temperature in millidegrees Celsius (`1000 == 1.0 °C`).
    #[must_use]
    pub const fn millidegrees_celsius(self) -> i32 {
        self.millidegrees_c
    }

    /// Returns the temperature in degrees Celsius (`°C`).
    #[must_use]
    pub fn celsius(self) -> f32 {
        self.millidegrees_c as f32 / 1000.0
    }

    /// Returns the temperature in degrees Fahrenheit (`°F`).
    #[must_use]
    pub fn fahrenheit(self) -> f32 {
        crate::diag::fahrenheit(self.celsius())
    }
}

impl TryFrom<i32> for Temperature {
    type Error = TempError;

    fn try_from(millidegrees_c: i32) -> Result<Self, Self::Error> {
        Self::from_millidegrees_celsius(millidegrees_c)
    }
}

impl Display for Temperature {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}°C ({:.2}°F)", self.celsius(), self.fahrenheit())
    }
}

/// Validated 1-Wire DS18B20 device identifier (family code `28-*`).
///
/// Guards the invariant that the sensor ID begins with `"28-"` followed by a
/// non-empty ASCII hexadecimal serial identifier.
///
/// # Examples
/// ```
/// use speaker_core::hardware::SensorId;
///
/// let id = SensorId::parse("28-00000abcdef")?;
/// assert_eq!(id.as_str(), "28-00000abcdef");
/// # Ok::<(), speaker_core::hardware::TempError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SensorId(Box<str>);

impl SensorId {
    fn is_valid_id(raw: &str) -> bool {
        raw.strip_prefix(DS18B20_FAMILY_PREFIX)
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    /// Parses a DS18B20 sensor ID from user-supplied CLI input without capturing a backtrace.
    ///
    /// # Errors
    /// Returns a descriptive user-facing error message if `id` does not begin with `"28-"`
    /// followed by non-empty ASCII hexadecimal digits.
    pub fn parse_str(id: &str) -> Result<Self, String> {
        let raw = id.trim();
        if !Self::is_valid_id(raw) {
            return Err(format!(
                "invalid DS18B20 sensor ID '{raw}': {SENSOR_ID_FORMAT_HELP}"
            ));
        }
        Ok(Self(raw.into()))
    }

    /// Validates and constructs a `SensorId` from a string slice.
    ///
    /// # Errors
    /// Returns [`TempError`] with [`TempError::is_invalid_sensor_id`] if `id` does
    /// not begin with `"28-"` or has an empty or non-hexadecimal serial suffix.
    pub fn parse(id: impl AsRef<str>) -> Result<Self, TempError> {
        let raw = id.as_ref().trim();
        if !Self::is_valid_id(raw) {
            return Err(TempError::new(ErrorKind::InvalidSensorId {
                value: raw.into(),
            }));
        }
        Ok(Self(raw.into()))
    }

    /// Returns the sensor identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SensorId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Display for SensorId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SensorId {
    type Err = TempError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for SensorId {
    type Error = TempError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for SensorId {
    type Error = TempError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

/// A single temperature reading paired with the originating DS18B20 sensor ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorReading {
    /// Identifier of the DS18B20 sensor (`28-*`).
    pub sensor_id: SensorId,
    /// Validated temperature measurement.
    pub temperature: Temperature,
}

impl Display for SensorReading {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {:6.2}°C  |  {:6.2}°F",
            self.sensor_id,
            self.temperature.celsius(),
            self.temperature.fahrenheit()
        )
    }
}

/// DS18B20 power supply mode reported by the kernel driver (`ext_power`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSupplyMode {
    /// Powered through an external Vdd line (active 3.3V).
    External,
    /// Parasite powered directly from the 1-Wire data line.
    Parasite,
    /// Unknown or unsupported kernel driver mode.
    Unknown(u8),
}

impl Display for PowerSupplyMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::External => f.write_str("External 3.3V (Vdd pin)"),
            Self::Parasite => f.write_str("Parasite power (data line)"),
            Self::Unknown(v) => write!(f, "Unknown ({v})"),
        }
    }
}

/// Detailed hardware diagnostics read from a DS18B20 sysfs device directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorDiagnostics {
    /// Sensor identifier.
    pub sensor_id: SensorId,
    /// Latest temperature reading.
    pub temperature: Temperature,
    /// Analog-to-digital converter resolution in bits (typically 9, 10, 11, or 12-bit).
    pub resolution_bits: Option<u8>,
    /// Power supply mode (external or parasite).
    pub power_mode: Option<PowerSupplyMode>,
    /// ADC conversion duration in milliseconds (typically 750 ms for 12-bit).
    pub conversion_time_ms: Option<u32>,
    /// Temperature alarm threshold registers (Low, High) in integer degrees Celsius.
    pub alarms: Option<(i8, i8)>,
    /// Raw ASCII scratchpad hex frame emitted by `w1_slave`.
    pub raw_payload: Box<str>,
}

impl Display for SensorDiagnostics {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "[Sensor Diagnostics: {}]", self.sensor_id)?;
        writeln!(f, "  • Temperature    : {}", self.temperature)?;
        if let Some(res) = self.resolution_bits {
            let prec = match res {
                9 => "0.5°C",
                10 => "0.25°C",
                11 => "0.125°C",
                12 => "0.0625°C",
                _ => "variable",
            };
            writeln!(f, "  • ADC Resolution : {res}-bit ({prec} precision)")?;
        }
        if let Some(pwr) = self.power_mode {
            writeln!(f, "  • Power Mode     : {pwr}")?;
        }
        if let Some(ms) = self.conversion_time_ms {
            writeln!(f, "  • Conv Time      : {ms} ms")?;
        }
        if let Some((low, high)) = self.alarms {
            writeln!(f, "  • Alarms (TL/TH) : Low: {low}°C | High: {high}°C")?;
        }
        write!(
            f,
            "  • Raw Scratchpad :\n    {}",
            self.raw_payload.trim_end().replace('\n', "\n    ")
        )
    }
}

/// Broadcom `SoC` core temperature reading from Linux thermal zone 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocThermal {
    temperature: Temperature,
}

impl SocThermal {
    /// Reads the `SoC` temperature from `/sys/devices/virtual/thermal/thermal_zone0/temp`.
    ///
    /// # Errors
    /// Returns [`TempError`] if the thermal zone file cannot be read or parsed.
    pub fn read() -> Result<Self, TempError> {
        Self::read_from_path(SOC_THERMAL_SYSFS_PATH)
    }

    /// Reads the `SoC` temperature from a specific sysfs path.
    ///
    /// # Errors
    /// Returns [`TempError`] if the file cannot be read or parsed into an integer millidegrees value.
    pub fn read_from_path(path: impl AsRef<Path>) -> Result<Self, TempError> {
        let content = fs::read_to_string(path.as_ref())?;
        let millidegrees = content.trim().parse::<i32>().map_err(|parse_err| {
            TempError::new(ErrorKind::ParseError {
                detail: format!(
                    "invalid SoC thermal integer in '{}': {parse_err}",
                    path.as_ref().display()
                )
                .into(),
            })
        })?;
        let temperature = Temperature::from_millidegrees_celsius(millidegrees)?;
        Ok(Self { temperature })
    }

    /// Returns the `SoC` temperature measurement.
    #[must_use]
    pub const fn temperature(self) -> Temperature {
        self.temperature
    }

    /// Computes the thermal gradient ($\Delta T = T_{\text{SoC}} - T_{\text{Chassis}}$) in degrees Celsius.
    #[must_use]
    pub fn delta_from_chassis(self, chassis: Temperature) -> f32 {
        self.temperature.celsius() - chassis.celsius()
    }
}

impl Display for SocThermal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "SoC: {}", self.temperature)
    }
}

/// Internal classification of DS18B20 temperature sensor failures.
#[derive(Debug)]
pub(crate) enum ErrorKind {
    /// No `28-*` DS18B20 sensors were discovered under the sysfs directory.
    NoSensorsDetected { base_path: PathBuf },
    /// The 1-Wire CRC check on line 1 of `w1_slave` did not end with `YES`.
    CrcMismatch { sensor_id: Option<Box<str>> },
    /// The `w1_slave` payload did not contain a valid `t=<millidegrees>` field.
    ParseError { detail: Box<str> },
    /// A temperature reading fell outside the DS18B20 operating bounds (`-55°C..=+125°C`).
    OutOfRange { millidegrees_c: i32 },
    /// A sensor identifier string failed validation.
    InvalidSensorId { value: Box<str> },
    /// Underlying filesystem or bus I/O failure.
    Io(io::Error),
    /// Asynchronous task was cancelled or the runtime shut down.
    Cancelled { message: Box<str> },
}

/// Error returned by DS18B20 1-Wire temperature sensor operations.
///
/// Provides predicate methods to inspect the failure category without exposing
/// internal representation details.
#[derive(Debug)]
pub struct TempError {
    kind: ErrorKind,
    backtrace: Backtrace,
}

impl TempError {
    pub(crate) fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns `true` if no DS18B20 (`28-*`) sensors were found on the 1-Wire bus.
    #[must_use]
    pub const fn is_no_sensors(&self) -> bool {
        matches!(self.kind, ErrorKind::NoSensorsDetected { .. })
    }

    /// Returns `true` if the 1-Wire bus CRC check failed (`crc=.. NO`).
    #[must_use]
    pub const fn is_crc_mismatch(&self) -> bool {
        matches!(self.kind, ErrorKind::CrcMismatch { .. })
    }

    /// Returns `true` if the `w1_slave` payload format could not be parsed.
    #[must_use]
    pub const fn is_parse_error(&self) -> bool {
        matches!(self.kind, ErrorKind::ParseError { .. })
    }

    /// Returns `true` if the temperature value is outside `-55.0 °C..=+125.0 °C`.
    #[must_use]
    pub const fn is_out_of_range(&self) -> bool {
        matches!(self.kind, ErrorKind::OutOfRange { .. })
    }

    /// Returns `true` if a sensor identifier did not match the `28-*` format.
    #[must_use]
    pub const fn is_invalid_sensor_id(&self) -> bool {
        matches!(self.kind, ErrorKind::InvalidSensorId { .. })
    }

    /// Returns `true` if an underlying filesystem I/O error occurred.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        matches!(self.kind, ErrorKind::Io(_))
    }

    /// Returns `true` if this error was caused by task cancellation or runtime shutdown.
    ///
    /// Daemon services should inspect this predicate during graceful shutdown to avoid
    /// treating normal task cancellation or service termination as hardware faults.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, ErrorKind::Cancelled { .. })
    }

    pub(crate) fn cancelled(message: impl Into<Box<str>>) -> Self {
        Self::new(ErrorKind::Cancelled {
            message: message.into(),
        })
    }

    /// Returns the captured backtrace for this error.
    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::NoSensorsDetected { base_path } => write!(
                f,
                "no DS18B20 sensors (28-*) detected under {}",
                base_path.display()
            ),
            ErrorKind::CrcMismatch {
                sensor_id: Some(id),
            } => write!(f, "1-Wire CRC check failed for sensor {id}"),
            ErrorKind::CrcMismatch { sensor_id: None } => {
                write!(f, "1-Wire CRC check failed (line 1 did not end with YES)")
            }
            ErrorKind::ParseError { detail } => {
                write!(f, "failed to parse w1_slave temperature payload: {detail}")
            }
            ErrorKind::OutOfRange { millidegrees_c } => write!(
                f,
                "temperature {millidegrees_c} m°C is outside DS18B20 range ({MIN_MILLIDEGREES_C}..={MAX_MILLIDEGREES_C} m°C)"
            ),
            ErrorKind::InvalidSensorId { value } => write!(
                f,
                "invalid DS18B20 sensor ID '{value}': {SENSOR_ID_FORMAT_HELP}"
            ),
            ErrorKind::Io(err) => write!(f, "1-Wire sysfs I/O error: {err}"),
            ErrorKind::Cancelled { message } => write!(f, "task cancelled: {message}"),
        }
    }
}

super::impl_canonical_error_display!(TempError);

impl std::error::Error for TempError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for TempError {
    fn from(err: io::Error) -> Self {
        Self::new(ErrorKind::Io(err))
    }
}

impl FromJoinError for TempError {
    fn from_cancelled(message: impl Into<Box<str>>) -> Self {
        Self::cancelled(message)
    }
}

/// Parses the two-line contents of a Linux kernel `w1_slave` file into a [`Temperature`].
///
/// Verifies that the first line ends with `"YES"` (indicating a valid 8-bit Dallas
/// CRC check) and extracts the `t=<millidegrees>` field from the second line.
///
/// # Examples
/// ```
/// use speaker_core::hardware::parse_w1_slave;
///
/// let raw = "a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES\n\
///            a7 01 4b 46 7f ff 0c 10 5c t=26437\n";
/// let temp = parse_w1_slave(raw)?;
/// assert_eq!(temp.millidegrees_celsius(), 26_437);
/// # Ok::<(), speaker_core::hardware::TempError>(())
/// ```
///
/// # Errors
/// Returns [`TempError`] if the payload has fewer than two lines, fails the CRC
/// check (`!line1.ends_with("YES")`), lacks a valid `t=` integer, or falls
/// outside the DS18B20 operating temperature range.
pub fn parse_w1_slave(raw: &str) -> Result<Temperature, TempError> {
    parse_w1_slave_for_sensor(raw, None)
}

fn parse_w1_slave_for_sensor(
    raw: &str,
    sensor_id: Option<&SensorId>,
) -> Result<Temperature, TempError> {
    let mut lines = raw.lines();
    let Some(first_line) = lines.next() else {
        return Err(TempError::new(ErrorKind::ParseError {
            detail: "w1_slave payload is empty".into(),
        }));
    };

    if !first_line.trim_end().ends_with("YES") {
        return Err(TempError::new(ErrorKind::CrcMismatch {
            sensor_id: sensor_id.map(|id| id.as_str().into()),
        }));
    }

    let Some(second_line) = lines.next() else {
        return Err(TempError::new(ErrorKind::ParseError {
            detail: "w1_slave payload is missing second line with t= value".into(),
        }));
    };

    let Some(eq_idx) = second_line.find("t=") else {
        return Err(TempError::new(ErrorKind::ParseError {
            detail: "second line of w1_slave does not contain 't='".into(),
        }));
    };

    let temp_str = second_line[eq_idx + 2..].trim();
    let millidegrees_c = temp_str.parse::<i32>().map_err(|parse_err| {
        TempError::new(ErrorKind::ParseError {
            detail: format!("invalid millidegrees integer after 't=' ('{temp_str}'): {parse_err}")
                .into(),
        })
    })?;

    Temperature::from_millidegrees_celsius(millidegrees_c)
}

/// Simulated state for a single mocked DS18B20 sensor.
#[derive(Debug, Clone)]
enum MockSensorEntry {
    /// Returns a validated temperature directly.
    Valid(Temperature),
    /// Parses a raw `w1_slave` payload string (useful for testing CRC and parse failures).
    RawPayload(Box<str>),
}

#[derive(Debug, Clone, Copy, Default)]
struct MockDiagnosticEntry {
    resolution_bits: Option<u8>,
    power_mode: Option<PowerSupplyMode>,
    conversion_time_ms: Option<u32>,
    alarms: Option<(i8, i8)>,
}

#[derive(Debug, Default)]
struct MockState {
    sensors: BTreeMap<SensorId, MockSensorEntry>,
    diagnostics: BTreeMap<SensorId, MockDiagnosticEntry>,
    forced_io_error: Option<Box<str>>,
}

/// Controller for manipulating a mocked [`TempSensor`] in unit and integration tests.
///
/// Cheaply cloneable via an internal `Arc` so tests can retain a handle while
/// passing the associated [`TempSensor`] into services or actors.
///
/// # Examples
/// ```
/// use speaker_core::hardware::{SensorId, TempSensor, Temperature};
///
/// let (sensor, mock) = TempSensor::new_mocked();
/// let id = SensorId::parse("28-000001234567")?;
/// mock.set_temperature(id.clone(), Temperature::from_celsius(26.5)?);
///
/// let reading = sensor.read_sensor(&id)?;
/// assert_eq!(reading.temperature.millidegrees_celsius(), 26_500);
/// # Ok::<(), speaker_core::hardware::TempError>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct TempSensorMock {
    state: Arc<Mutex<MockState>>,
}

impl TempSensorMock {
    fn new() -> Self {
        Self::default()
    }

    /// Registers or updates a simulated sensor with a valid [`Temperature`].
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_temperature(&self, sensor_id: SensorId, temperature: Temperature) {
        let mut guard = self.state.lock().expect("mock state lock poisoned");
        guard
            .sensors
            .insert(sensor_id, MockSensorEntry::Valid(temperature));
    }

    /// Registers or updates a simulated sensor with raw `w1_slave` text content.
    ///
    /// Use this to inject CRC failures (`crc=00 NO`) or malformed payloads in tests.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_raw_w1_slave(&self, sensor_id: SensorId, raw_payload: impl Into<Box<str>>) {
        let mut guard = self.state.lock().expect("mock state lock poisoned");
        guard
            .sensors
            .insert(sensor_id, MockSensorEntry::RawPayload(raw_payload.into()));
    }

    /// Sets mock extended diagnostic parameters for a sensor.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_diagnostics(
        &self,
        sensor_id: SensorId,
        resolution_bits: Option<u8>,
        power_mode: Option<PowerSupplyMode>,
        conversion_time_ms: Option<u32>,
        alarms: Option<(i8, i8)>,
    ) {
        let mut guard = self.state.lock().expect("mock state lock poisoned");
        guard.diagnostics.insert(
            sensor_id,
            MockDiagnosticEntry {
                resolution_bits,
                power_mode,
                conversion_time_ms,
                alarms,
            },
        );
    }

    /// Forces all subsequent sensor discovery and read operations to fail with an I/O error.
    ///
    /// Simulates a disconnected 1-Wire bus or kernel driver fault.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn set_io_error(&self, message: impl Into<Box<str>>) {
        let mut guard = self.state.lock().expect("mock state lock poisoned");
        guard.forced_io_error = Some(message.into());
    }

    /// Clears any forced I/O error previously set via [`TempSensorMock::set_io_error`].
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn clear_io_error(&self) {
        let mut guard = self.state.lock().expect("mock state lock poisoned");
        guard.forced_io_error = None;
    }

    /// Removes all simulated sensors from the mock bus.
    ///
    /// # Panics
    /// Panics if the internal mock mutex has been poisoned by a prior test panic.
    pub fn clear_sensors(&self) {
        let mut guard = self.state.lock().expect("mock state lock poisoned");
        guard.sensors.clear();
        guard.diagnostics.clear();
    }

    fn discover(&self) -> Result<Vec<SensorId>, TempError> {
        let guard = self.state.lock().expect("mock state lock poisoned");
        if let Some(msg) = &guard.forced_io_error {
            return Err(TempError::from(io::Error::other(msg.as_ref())));
        }
        if guard.sensors.is_empty() {
            return Err(TempError::new(ErrorKind::NoSensorsDetected {
                base_path: PathBuf::from("mock://w1/devices"),
            }));
        }
        Ok(guard.sensors.keys().cloned().collect())
    }

    fn read_sensor(&self, sensor_id: &SensorId) -> Result<SensorReading, TempError> {
        let guard = self.state.lock().expect("mock state lock poisoned");
        if let Some(msg) = &guard.forced_io_error {
            return Err(TempError::from(io::Error::other(msg.as_ref())));
        }
        let Some(entry) = guard.sensors.get(sensor_id) else {
            return Err(TempError::from(io::Error::new(
                io::ErrorKind::NotFound,
                format!("mock sensor {sensor_id} not found"),
            )));
        };
        let temperature = match entry {
            MockSensorEntry::Valid(t) => *t,
            MockSensorEntry::RawPayload(raw) => parse_w1_slave_for_sensor(raw, Some(sensor_id))?,
        };
        Ok(SensorReading {
            sensor_id: sensor_id.clone(),
            temperature,
        })
    }

    fn read_diagnostics(&self, sensor_id: &SensorId) -> Result<SensorDiagnostics, TempError> {
        let reading = self.read_sensor(sensor_id)?;
        let guard = self.state.lock().expect("mock state lock poisoned");
        let raw_payload = match guard.sensors.get(sensor_id) {
            Some(MockSensorEntry::RawPayload(raw)) => raw.clone(),
            _ => format!(
                "00 00 00 00 00 00 00 00 00 : crc=00 YES\n00 00 00 00 00 00 00 00 00 t={}\n",
                reading.temperature.millidegrees_celsius()
            )
            .into_boxed_str(),
        };
        let diag_entry = guard
            .diagnostics
            .get(sensor_id)
            .copied()
            .unwrap_or(MockDiagnosticEntry {
                resolution_bits: Some(12),
                power_mode: Some(PowerSupplyMode::External),
                conversion_time_ms: Some(750),
                alarms: None,
            });
        Ok(SensorDiagnostics {
            sensor_id: sensor_id.clone(),
            temperature: reading.temperature,
            resolution_bits: diag_entry.resolution_bits,
            power_mode: diag_entry.power_mode,
            conversion_time_ms: diag_entry.conversion_time_ms,
            alarms: diag_entry.alarms,
            raw_payload,
        })
    }
}

#[derive(Debug)]
enum Backend {
    Sysfs { base_path: PathBuf },
    Mock(TempSensorMock),
}

/// DS18B20 1-Wire temperature sensor service.
///
/// Cheaply cloneable (`Arc`-backed) handle that can be shared across Tokio
/// actors in the `core` daemon, driven by [`TempSensorMock`] in tests, or used
/// directly by CLI diagnostic utilities such as `read_temp`.
///
/// # Examples
/// ```
/// use speaker_core::hardware::{SensorId, TempSensor, Temperature};
///
/// let (sensor, mock) = TempSensor::new_mocked();
/// let id = SensorId::parse("28-00000abcdef")?;
/// mock.set_temperature(id, Temperature::from_millidegrees_celsius(26_437)?);
///
/// let temp = sensor.read_primary_temperature()?;
/// assert_eq!(temp.millidegrees_celsius(), 26_437);
/// # Ok::<(), speaker_core::hardware::TempError>(())
/// ```
#[derive(Debug, Clone)]
pub struct TempSensor {
    inner: Arc<Backend>,
}

impl TempSensor {
    /// Creates a `TempSensor` backed by the standard Linux 1-Wire sysfs path (`/sys/bus/w1/devices`).
    ///
    /// Construction is pure in-memory metadata initialization and performs no filesystem
    /// syscalls; all sysfs directory and device reads are deferred lazily to [`discover_sensors`](Self::discover_sensors)
    /// and [`read_sensor`](Self::read_sensor).
    #[must_use]
    #[expect(
        clippy::new_without_default,
        reason = "Hardware I/O drivers omit Default per M-MOCKABLE-SYSCALLS"
    )]
    pub fn new() -> Self {
        Self::with_sysfs_path(DS18B20_SYSFS_BASE_PATH)
    }

    /// Creates a `TempSensor` reading from a custom sysfs directory path.
    ///
    /// Useful for testing file discovery and `w1_slave` parsing against a
    /// synthetic directory tree on any operating system. Performs no filesystem I/O
    /// at construction time.
    #[must_use]
    pub fn with_sysfs_path(base_path: impl AsRef<Path>) -> Self {
        Self {
            inner: Arc::new(Backend::Sysfs {
                base_path: base_path.as_ref().to_path_buf(),
            }),
        }
    }

    /// Creates a mocked `TempSensor` paired with its [`TempSensorMock`] controller.
    #[must_use]
    pub fn new_mocked() -> (Self, TempSensorMock) {
        let mock = TempSensorMock::new();
        let sensor = Self {
            inner: Arc::new(Backend::Mock(mock.clone())),
        };
        (sensor, mock)
    }

    /// Returns the GPIO pin assignment and electrical configuration for the DS18B20 sensor.
    #[must_use]
    pub const fn gpio_info(&self) -> GpioPinInfo {
        GpioPinInfo {
            signal_name: "DS18B20 1-Wire Temperature Sensor Data",
            bcm_pin: DS18B20_BCM_GPIO_PIN,
            physical_pin: DS18B20_PHYSICAL_PIN,
            bus_protocol: "1-Wire (w1-gpio)",
            electrical_config: DS18B20_DTOVERLAY,
        }
    }

    /// Returns the sysfs base path inspected by this sensor instance.
    #[must_use]
    pub fn sysfs_base_path(&self) -> &Path {
        match self.inner.as_ref() {
            Backend::Sysfs { base_path } => base_path.as_path(),
            Backend::Mock(_) => Path::new(DS18B20_SYSFS_BASE_PATH),
        }
    }

    /// Discovers all attached DS18B20 (`28-*`) sensors in deterministic sorted order.
    ///
    /// # Errors
    /// Returns [`TempError`] if the sysfs directory cannot be read or if zero
    /// `28-*` sensors are present (`err.is_no_sensors() == true`).
    pub fn discover_sensors(&self) -> Result<Vec<SensorId>, TempError> {
        match self.inner.as_ref() {
            Backend::Sysfs { base_path } => discover_sysfs_sensors(base_path),
            Backend::Mock(mock) => mock.discover(),
        }
    }

    /// Reads the current temperature from a specific DS18B20 sensor.
    ///
    /// # Errors
    /// Returns [`TempError`] if reading `<base_path>/<sensor_id>/w1_slave` fails,
    /// the CRC check fails, or the temperature value is malformed or out of bounds.
    pub fn read_sensor(&self, sensor_id: &SensorId) -> Result<SensorReading, TempError> {
        match self.inner.as_ref() {
            Backend::Sysfs { base_path } => {
                let slave_path = base_path.join(sensor_id.as_str()).join("w1_slave");
                let raw = fs::read_to_string(slave_path)?;
                let temperature = parse_w1_slave_for_sensor(&raw, Some(sensor_id))?;
                Ok(SensorReading {
                    sensor_id: sensor_id.clone(),
                    temperature,
                })
            }
            Backend::Mock(mock) => mock.read_sensor(sensor_id),
        }
    }

    /// Reads detailed hardware diagnostics from a specific DS18B20 sensor.
    ///
    /// Queries `w1_slave`, `resolution`, `ext_power`, `conv_time`, and `alarms`.
    ///
    /// # Errors
    /// Returns [`TempError`] if reading or parsing the primary temperature fails.
    pub fn read_diagnostics(&self, sensor_id: &SensorId) -> Result<SensorDiagnostics, TempError> {
        match self.inner.as_ref() {
            Backend::Sysfs { base_path } => {
                let dev_dir = base_path.join(sensor_id.as_str());
                let slave_path = dev_dir.join("w1_slave");
                let raw_payload = fs::read_to_string(slave_path)?;
                let temperature = parse_w1_slave_for_sensor(&raw_payload, Some(sensor_id))?;

                let resolution_bits = fs::read_to_string(dev_dir.join("resolution"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u8>().ok());

                let power_mode = fs::read_to_string(dev_dir.join("ext_power"))
                    .ok()
                    .and_then(|s| match s.trim() {
                        "1" => Some(PowerSupplyMode::External),
                        "0" => Some(PowerSupplyMode::Parasite),
                        val => val.parse::<u8>().ok().map(PowerSupplyMode::Unknown),
                    });

                let conversion_time_ms = fs::read_to_string(dev_dir.join("conv_time"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok());

                let alarms = fs::read_to_string(dev_dir.join("alarms"))
                    .ok()
                    .and_then(|s| {
                        let mut parts = s.split_whitespace();
                        let low = parts.next()?.parse::<i8>().ok()?;
                        let high = parts.next()?.parse::<i8>().ok()?;
                        Some((low, high))
                    });

                Ok(SensorDiagnostics {
                    sensor_id: sensor_id.clone(),
                    temperature,
                    resolution_bits,
                    power_mode,
                    conversion_time_ms,
                    alarms,
                    raw_payload: raw_payload.into_boxed_str(),
                })
            }
            Backend::Mock(mock) => mock.read_diagnostics(sensor_id),
        }
    }

    /// Discovers and reads all attached DS18B20 sensors on the 1-Wire bus.
    ///
    /// # Errors
    /// Returns [`TempError`] if no sensors are discovered or if reading a sensor fails.
    pub fn read_all(&self) -> Result<Vec<SensorReading>, TempError> {
        self.discover_sensors()?
            .iter()
            .map(|id| self.read_sensor(id))
            .collect()
    }

    /// Reads the temperature of the primary (first discovered) DS18B20 sensor.
    ///
    /// Designed for the core daemon's thermal controller and top status bar.
    ///
    /// # Errors
    /// Returns [`TempError`] if no sensors are discovered or if reading the primary sensor fails.
    pub fn read_primary_temperature(&self) -> Result<Temperature, TempError> {
        let ids = self.discover_sensors()?;
        let Some(first) = ids.first() else {
            return Err(TempError::new(ErrorKind::NoSensorsDetected {
                base_path: self.sysfs_base_path().to_path_buf(),
            }));
        };
        self.read_sensor(first).map(|r| r.temperature)
    }

    /// Asynchronously discovers all attached DS18B20 (`28-*`) sensors without blocking Tokio.
    ///
    /// # Errors
    /// Returns [`TempError`] if no sensors are found or if sysfs I/O fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn discover_sensors_async(&self) -> Result<Vec<SensorId>, TempError> {
        let this = self.clone();
        run_blocking("1-Wire sensor discovery", move || this.discover_sensors()).await
    }

    /// Asynchronously reads a specific DS18B20 sensor on the blocking threadpool.
    ///
    /// # Errors
    /// Returns [`TempError`] if reading or parsing `w1_slave` fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_sensor_async(
        &self,
        sensor_id: &SensorId,
    ) -> Result<SensorReading, TempError> {
        let this = self.clone();
        let id = sensor_id.clone();
        run_blocking("1-Wire sensor read", move || this.read_sensor(&id)).await
    }

    /// Asynchronously reads diagnostics from a DS18B20 sensor on the blocking threadpool.
    ///
    /// # Errors
    /// Returns [`TempError`] if reading diagnostics fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_diagnostics_async(
        &self,
        sensor_id: &SensorId,
    ) -> Result<SensorDiagnostics, TempError> {
        let this = self.clone();
        let id = sensor_id.clone();
        run_blocking("1-Wire diagnostics read", move || {
            this.read_diagnostics(&id)
        })
        .await
    }

    /// Asynchronously discovers and reads all DS18B20 sensors on the blocking threadpool.
    ///
    /// # Errors
    /// Returns [`TempError`] if no sensors are discovered or if any sensor read fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_all_async(&self) -> Result<Vec<SensorReading>, TempError> {
        let this = self.clone();
        run_blocking("1-Wire read-all sensors", move || this.read_all()).await
    }

    /// Asynchronously reads the primary DS18B20 sensor temperature on the blocking threadpool.
    ///
    /// # Errors
    /// Returns [`TempError`] if no sensors are present or if the sensor read fails.
    ///
    /// # Panics
    /// Panics if the underlying blocking task panics, resuming unwinding to propagate
    /// programming errors to the caller.
    pub async fn read_primary_temperature_async(&self) -> Result<Temperature, TempError> {
        let this = self.clone();
        run_blocking("1-Wire primary temperature read", move || {
            this.read_primary_temperature()
        })
        .await
    }
}

fn discover_sysfs_sensors(base_path: &Path) -> Result<Vec<SensorId>, TempError> {
    let entries = match fs::read_dir(base_path) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(TempError::new(ErrorKind::NoSensorsDetected {
                base_path: base_path.to_path_buf(),
            }));
        }
        Err(err) => return Err(TempError::from(err)),
    };

    let mut sensors = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if name_str.starts_with(DS18B20_FAMILY_PREFIX) {
            if let Ok(id) = SensorId::parse(name_str) {
                sensors.push(id);
            }
        }
    }

    sensors.sort();

    if sensors.is_empty() {
        return Err(TempError::new(ErrorKind::NoSensorsDetected {
            base_path: base_path.to_path_buf(),
        }));
    }

    Ok(sensors)
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TempSensor>();
    assert_send_sync::<TempSensorMock>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::ScopedTempDir;
    use crate::runtime::abort_and_map;
    use std::error::Error;

    #[test]
    fn temperature_conversions_and_bounds() {
        let t = Temperature::from_millidegrees_celsius(26_437).unwrap();
        assert_eq!(t.millidegrees_celsius(), 26_437);
        assert!((t.celsius() - 26.437).abs() < 1e-4);
        assert!((t.fahrenheit() - 79.5866).abs() < 1e-3);
        assert_eq!(t.to_string(), "26.44°C (79.59°F)");

        // Negative temperature (-5.125 °C -> 22.775 °F)
        let neg = Temperature::from_millidegrees_celsius(-5_125).unwrap();
        assert!((neg.celsius() - (-5.125)).abs() < 1e-4);
        assert!((neg.fahrenheit() - 22.775).abs() < 1e-3);

        // Boundary check: below -55 °C and above +125 °C must fail
        let low_err = Temperature::from_millidegrees_celsius(-55_001).unwrap_err();
        assert!(low_err.is_out_of_range());

        let high_err = Temperature::from_millidegrees_celsius(125_001).unwrap_err();
        assert!(high_err.is_out_of_range());

        let nan_err = Temperature::from_celsius(f32::NAN).unwrap_err();
        assert!(nan_err.is_out_of_range());
    }

    #[test]
    fn sensor_id_validates_family_prefix_and_suffix() {
        let valid = SensorId::parse("28-00000abcdef0").unwrap();
        assert_eq!(valid.as_str(), "28-00000abcdef0");

        assert!(
            SensorId::parse("10-00000abcdef0")
                .unwrap_err()
                .is_invalid_sensor_id()
        );
        assert!(SensorId::parse("28-").unwrap_err().is_invalid_sensor_id());
        assert!(
            SensorId::parse("28-abc/def")
                .unwrap_err()
                .is_invalid_sensor_id()
        );
        // Non-hexadecimal characters ('g', 'z') must be rejected per 1-Wire DS18B20 specification
        assert!(
            SensorId::parse("28-00000diag001")
                .unwrap_err()
                .is_invalid_sensor_id()
        );
        assert!(
            SensorId::parse("28-0000000xyz01")
                .unwrap_err()
                .is_invalid_sensor_id()
        );
    }

    #[test]
    fn parse_w1_slave_handles_valid_crc_and_rejects_corrupt_frames() {
        let valid_payload = "\
a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES
a7 01 4b 46 7f ff 0c 10 5c t=26437
";
        let temp = parse_w1_slave(valid_payload).unwrap();
        assert_eq!(temp.millidegrees_celsius(), 26_437);

        let crc_fail_payload = "\
a7 01 4b 46 7f ff 0c 10 5c : crc=5c NO
a7 01 4b 46 7f ff 0c 10 5c t=26437
";
        let err = parse_w1_slave(crc_fail_payload).unwrap_err();
        assert!(err.is_crc_mismatch());

        let missing_t_payload = "\
a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES
a7 01 4b 46 7f ff 0c 10 5c
";
        let err = parse_w1_slave(missing_t_payload).unwrap_err();
        assert!(err.is_parse_error());

        // Empty payload
        assert!(parse_w1_slave("").unwrap_err().is_parse_error());

        // Single line with YES but missing second line
        assert!(
            parse_w1_slave("a7 01 4b 46 : crc=5c YES\n")
                .unwrap_err()
                .is_parse_error()
        );

        // Missing colon on line 1 without YES: the 1-Wire kernel driver signals hardware
        // cyclic redundancy check failures with "NO" on line 1; thus payloads lacking "YES"
        // classify as CRC mismatches.
        let no_colon = "a7 01 4b 46 7f ff 0c 10 5c crc=5c NO\na7 01 4b 46 7f ff 0c 10 5c t=26437\n";
        assert!(parse_w1_slave(no_colon).unwrap_err().is_crc_mismatch());

        // Invalid integer in t=
        let bad_int =
            "a7 01 4b 46 7f ff 0c 10 5c : crc=5c YES\na7 01 4b 46 7f ff 0c 10 5c t=not_an_int\n";
        assert!(parse_w1_slave(bad_int).unwrap_err().is_parse_error());
    }

    #[test]
    fn mock_sensor_supports_readings_and_fault_injection() {
        let (sensor, mock) = TempSensor::new_mocked();

        // Initially empty -> NoSensorsDetected
        assert!(sensor.discover_sensors().unwrap_err().is_no_sensors());

        let id1 = SensorId::parse("28-000000000001").unwrap();
        let id2 = SensorId::parse("28-000000000002").unwrap();
        mock.set_temperature(id1.clone(), Temperature::from_celsius(26.44).unwrap());
        mock.set_raw_w1_slave(
            id2.clone(),
            "ff 07 4b 46 7f ff 0c 10 5c : crc=5c YES\nff 07 4b 46 7f ff 0c 10 5c t=31250\n",
        );

        let all = sensor.read_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].sensor_id, id1);
        assert_eq!(all[0].temperature.millidegrees_celsius(), 26_440);
        assert_eq!(all[1].sensor_id, id2);
        assert_eq!(all[1].temperature.millidegrees_celsius(), 31_250);

        // Extended diagnostics
        let diag = sensor.read_diagnostics(&id1).unwrap();
        assert_eq!(diag.resolution_bits, Some(12));
        assert_eq!(diag.power_mode, Some(PowerSupplyMode::External));

        // Diagnostics for sensor with RawPayload
        let diag_raw = sensor.read_diagnostics(&id2).unwrap();
        assert!(diag_raw.raw_payload.contains("ff 07 4b 46"));

        // Inject CRC error on id2
        mock.set_raw_w1_slave(id2.clone(), "00 : crc=00 NO\n00 t=31250\n");
        assert!(sensor.read_sensor(&id2).unwrap_err().is_crc_mismatch());

        // Inject bus I/O error
        mock.set_io_error("1-Wire bus disconnected");
        assert!(sensor.read_primary_temperature().unwrap_err().is_io());

        mock.clear_io_error();
        assert_eq!(
            sensor
                .read_primary_temperature()
                .unwrap()
                .millidegrees_celsius(),
            26_440
        );
    }

    #[tokio::test]
    async fn async_methods_read_from_sensor_without_blocking() {
        fn assert_send<T: Send>(_: T) {}
        let (sensor, mock) = TempSensor::new_mocked();
        let id = SensorId::parse("28-00000abcdef1").unwrap();
        assert_send(sensor.discover_sensors_async());
        assert_send(sensor.read_primary_temperature_async());
        assert_send(sensor.read_all_async());
        assert_send(sensor.read_sensor_async(&id));
        assert_send(sensor.read_diagnostics_async(&id));

        mock.set_temperature(
            id.clone(),
            Temperature::from_millidegrees_celsius(28_500).unwrap(),
        );

        let discovered = sensor.discover_sensors_async().await.unwrap();
        assert_eq!(discovered, vec![id.clone()]);

        let primary = sensor.read_primary_temperature_async().await.unwrap();
        assert_eq!(primary.millidegrees_celsius(), 28_500);

        let reading = sensor.read_sensor_async(&id).await.unwrap();
        assert_eq!(reading.temperature.millidegrees_celsius(), 28_500);

        let diag = sensor.read_diagnostics_async(&id).await.unwrap();
        assert_eq!(diag.temperature.millidegrees_celsius(), 28_500);
    }

    #[tokio::test]
    async fn cancelled_blocking_task_maps_to_temp_error_cancelled() {
        let err: TempError = abort_and_map("1-Wire sensor read").await;
        assert!(err.is_cancelled());
        assert!(!err.is_io());
        assert!(
            err.to_string()
                .contains("1-Wire sensor read task was cancelled")
        );
    }

    #[test]
    fn soc_thermal_parses_content() {
        let temp_dir = ScopedTempDir::new("soc_thermal_test");
        let path = temp_dir.path().join("temp");
        fs::write(&path, "41856\n").unwrap();

        let soc = SocThermal::read_from_path(&path).unwrap();
        assert_eq!(soc.temperature().millidegrees_celsius(), 41_856);
        assert!((soc.temperature().celsius() - 41.856).abs() < 1e-3);
        assert_eq!(soc.to_string(), "SoC: 41.86°C (107.34°F)");

        let chassis = Temperature::from_millidegrees_celsius(24_000).unwrap();
        assert!((soc.delta_from_chassis(chassis) - 17.856).abs() < 1e-2);

        // Parse error
        let bad_path = temp_dir.path().join("bad_temp");
        fs::write(&bad_path, "not-a-number\n").unwrap();
        let err = SocThermal::read_from_path(&bad_path).unwrap_err();
        assert!(err.is_parse_error());

        // Missing file
        let missing_err =
            SocThermal::read_from_path(temp_dir.path().join("nonexistent")).unwrap_err();
        assert!(missing_err.is_io());
    }

    #[test]
    fn temperature_constructors_and_try_from() {
        let t = Temperature::new_millidegrees_celsius(25_000);
        assert_eq!(t.millidegrees_celsius(), 25_000);

        let from_try: Temperature = 30_000.try_into().unwrap();
        assert_eq!(from_try.millidegrees_celsius(), 30_000);

        let err_try: Result<Temperature, _> = 200_000.try_into();
        let _ = err_try.unwrap_err();

        assert!(
            Temperature::from_celsius(-60.0)
                .unwrap_err()
                .is_out_of_range()
        );
        assert!(
            Temperature::from_celsius(130.0)
                .unwrap_err()
                .is_out_of_range()
        );
        assert!(
            Temperature::from_celsius(f32::INFINITY)
                .unwrap_err()
                .is_out_of_range()
        );
        assert!(
            Temperature::from_celsius(f32::NEG_INFINITY)
                .unwrap_err()
                .is_out_of_range()
        );
        assert!(
            Temperature::from_celsius(f32::NAN)
                .unwrap_err()
                .is_out_of_range()
        );
    }

    #[test]
    #[should_panic(expected = "temperature millidegrees_c must be within DS18B20 range")]
    fn temperature_new_panics_on_low_overflow() {
        let _ = Temperature::new_millidegrees_celsius(-55_001);
    }

    #[test]
    #[should_panic(expected = "temperature millidegrees_c must be within DS18B20 range")]
    fn temperature_new_panics_on_high_overflow() {
        let _ = Temperature::new_millidegrees_celsius(125_001);
    }

    #[test]
    fn sensor_id_traits_and_conversions() {
        let id_str = "28-01201b7d5396";
        let id: SensorId = id_str.parse().unwrap();
        assert_eq!(id.as_ref(), id_str);
        assert_eq!(id.to_string(), id_str);

        let id_from_str: SensorId = id_str.try_into().unwrap();
        assert_eq!(id_from_str, id);

        let id_from_string: SensorId = id_str.to_string().try_into().unwrap();
        assert_eq!(id_from_string, id);
    }

    #[test]
    fn sensor_reading_and_diagnostics_display_formatting() {
        let sensor_id = SensorId::parse("28-01201b7d5396").unwrap();
        let temperature = Temperature::from_millidegrees_celsius(22_500).unwrap();
        let reading = SensorReading {
            sensor_id: sensor_id.clone(),
            temperature,
        };
        let formatted = reading.to_string();
        assert!(formatted.contains("[28-01201b7d5396]"));
        assert!(formatted.contains("22.50°C"));

        // PowerSupplyMode Display
        assert_eq!(
            PowerSupplyMode::External.to_string(),
            "External 3.3V (Vdd pin)"
        );
        assert_eq!(
            PowerSupplyMode::Parasite.to_string(),
            "Parasite power (data line)"
        );
        assert_eq!(PowerSupplyMode::Unknown(42).to_string(), "Unknown (42)");

        // Diagnostics with all resolutions
        for (res, prec) in [
            (9, "0.5°C"),
            (10, "0.25°C"),
            (11, "0.125°C"),
            (12, "0.0625°C"),
            (8, "variable"),
        ] {
            let diag = SensorDiagnostics {
                sensor_id: sensor_id.clone(),
                temperature,
                resolution_bits: Some(res),
                power_mode: Some(PowerSupplyMode::External),
                conversion_time_ms: Some(750),
                alarms: Some((0, 70)),
                raw_payload: "raw frame\n".into(),
            };
            let text = diag.to_string();
            assert!(text.contains(prec));
            assert!(text.contains("External 3.3V"));
            assert!(text.contains("750 ms"));
            assert!(text.contains("Low: 0°C | High: 70°C"));
        }

        // Minimal diagnostics with None fields
        let minimal_diag = SensorDiagnostics {
            sensor_id,
            temperature,
            resolution_bits: None,
            power_mode: None,
            conversion_time_ms: None,
            alarms: None,
            raw_payload: "raw\n".into(),
        };
        let min_text = minimal_diag.to_string();
        assert_ne!(min_text, "");
        assert!(!min_text.contains("ADC Resolution"));
    }

    #[test]
    fn temp_error_all_variants_predicates_and_display() {
        let no_sensors = TempError::new(ErrorKind::NoSensorsDetected {
            base_path: PathBuf::from("/sys/bus/w1/devices"),
        });
        assert!(no_sensors.is_no_sensors());
        assert!(
            no_sensors
                .to_string()
                .contains("no DS18B20 sensors (28-*) detected")
        );
        assert!(no_sensors.source().is_none());
        let _ = no_sensors.backtrace();

        let crc_with_id = TempError::new(ErrorKind::CrcMismatch {
            sensor_id: Some("28-001".into()),
        });
        assert!(crc_with_id.is_crc_mismatch());
        assert!(
            crc_with_id
                .to_string()
                .contains("1-Wire CRC check failed for sensor 28-001")
        );

        let crc_no_id = TempError::new(ErrorKind::CrcMismatch { sensor_id: None });
        assert!(crc_no_id.is_crc_mismatch());
        assert!(crc_no_id.to_string().contains("1-Wire CRC check failed"));

        let parse_err = TempError::new(ErrorKind::ParseError {
            detail: "missing t=".into(),
        });
        assert!(parse_err.is_parse_error());
        assert!(parse_err.to_string().contains("failed to parse w1_slave"));

        let range_err = TempError::new(ErrorKind::OutOfRange {
            millidegrees_c: 150_000,
        });
        assert!(range_err.is_out_of_range());
        assert!(
            range_err
                .to_string()
                .contains("temperature 150000 m°C is outside")
        );

        let invalid_sensor_err = TempError::new(ErrorKind::InvalidSensorId {
            value: "bad-id".into(),
        });
        assert!(invalid_sensor_err.is_invalid_sensor_id());
        assert!(
            invalid_sensor_err
                .to_string()
                .contains("invalid DS18B20 sensor ID 'bad-id'")
        );

        let temp_io_err =
            TempError::from(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert!(temp_io_err.is_io());
        assert!(temp_io_err.to_string().contains("1-Wire sysfs I/O error"));
        assert!(temp_io_err.source().is_some());

        let cancel_err = TempError::cancelled("worker cancelled");
        assert!(cancel_err.is_cancelled());
        assert!(!cancel_err.is_io());
        assert!(!cancel_err.is_no_sensors());
        assert!(matches!(
            cancel_err.backtrace().status(),
            std::backtrace::BacktraceStatus::Captured | std::backtrace::BacktraceStatus::Disabled
        ));
        assert!(
            cancel_err
                .to_string()
                .contains("task cancelled: worker cancelled")
        );
        assert!(cancel_err.source().is_none());

        // Machine-oriented single-line summary omits captured backtrace
        for err in [
            &no_sensors,
            &crc_with_id,
            &crc_no_id,
            &parse_err,
            &range_err,
            &invalid_sensor_err,
            &temp_io_err,
            &cancel_err,
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

    #[tokio::test]
    async fn temp_sensor_read_all_async_and_metadata() {
        let (sensor, mock) = TempSensor::new_mocked();
        assert_eq!(sensor.sysfs_base_path(), Path::new(DS18B20_SYSFS_BASE_PATH));

        let gpio = sensor.gpio_info();
        assert_eq!(gpio.bcm_pin.as_u8(), 26);
        assert_eq!(gpio.physical_pin.as_u8(), 37);

        let id = SensorId::parse("28-000000000042").unwrap();
        mock.set_temperature(id.clone(), Temperature::from_celsius(20.0).unwrap());

        let all = sensor.read_all_async().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].sensor_id, id);
    }

    #[test]
    fn temp_sensor_mock_diagnostics_and_clear() {
        let (sensor, mock) = TempSensor::new_mocked();
        let id = SensorId::parse("28-000000000042").unwrap();
        mock.set_temperature(id.clone(), Temperature::from_celsius(20.0).unwrap());

        mock.set_diagnostics(
            id.clone(),
            Some(10),
            Some(PowerSupplyMode::Parasite),
            Some(188),
            Some((-10, 50)),
        );
        let diag = sensor.read_diagnostics(&id).unwrap();
        assert_eq!(diag.resolution_bits, Some(10));
        assert_eq!(diag.power_mode, Some(PowerSupplyMode::Parasite));
        assert_eq!(diag.conversion_time_ms, Some(188));
        assert_eq!(diag.alarms, Some((-10, 50)));

        // Test sensor not found
        let missing_id = SensorId::parse("28-000000000099").unwrap();
        let _ = sensor.read_sensor(&missing_id).unwrap_err();

        // Test clear_sensors
        mock.clear_sensors();
        assert!(sensor.discover_sensors().unwrap_err().is_no_sensors());
    }

    #[test]
    fn temp_sensor_sysfs_path_defaults_and_errors() {
        // Hermeticity invariant: TempSensor::new() and with_sysfs_path() perform zero
        // filesystem syscalls at construction time, succeeding even when the path does not exist.
        let default_sensor = TempSensor::new();
        assert_eq!(
            default_sensor.sysfs_base_path(),
            Path::new(DS18B20_SYSFS_BASE_PATH)
        );
        let nonexistent = TempSensor::with_sysfs_path("/nonexistent/hermetic/w1/path");
        assert_eq!(
            nonexistent.sysfs_base_path(),
            Path::new("/nonexistent/hermetic/w1/path")
        );

        // Test sysfs path that is a file (triggers Err(NotADirectory) in discover_sysfs_sensors)
        let temp_dir = ScopedTempDir::new("ds18b20_file_test");
        let temp_file = temp_dir.path().join("not_a_dir");
        fs::write(&temp_file, "not a directory").unwrap();
        let file_sensor = TempSensor::with_sysfs_path(&temp_file);
        let res = file_sensor.discover_sensors();
        assert!(res.unwrap_err().is_io());
    }
}
