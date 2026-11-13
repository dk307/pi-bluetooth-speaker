//! Hardware peripheral drivers and mock abstractions for Raspberry Pi 3.
//!
//! Exposes drivers for:
//! - MA12070P I2C Audio Amplifier (`amp`)
//! - EC12 Rotary Encoder & RGB Status LED (`encoder`)
//! - BH1750 Ambient Light Sensor (`light`)
//! - DS18B20 1-Wire Temperature Sensor & `SoC` Thermals (`temp`)
//! - GPIO Pin Mappings & Validation (`pins`)

pub mod amp;
pub mod encoder;
pub mod light;
pub mod pins;
pub mod temp;

#[doc(inline)]
pub use pins::{BcmPin, GpioPinInfo, PhysicalPin, PinError};

#[doc(inline)]
pub use amp::{
    AmpDevice, AmpDeviceMock, AmpError, AmpErrorFlags, AmpFault, AmpHardwareInfo, AmpI2cAddress,
    AmpSettings, AmpStatus, AudioInputMode, DcuChannelStatus, DcuMonitor, I2sFormat, LimiterSlew,
    MA12070P_DEFAULT_I2C_ADDR, MA12070P_DEFAULT_I2C_BUS, MA12070P_MAX_I2C_ADDR,
    MA12070P_MIN_I2C_ADDR, MASTER_VOLUME_RAW_MAX, MIN_USABLE_DB, PERCENT_TO_RAW, Pm2Config,
    PowerModeCtrl, PowerModeMon, PowerModeProfile, REG_AUDIO_IN_MODE, REG_AUDIO_IN_MODE_EXT,
    REG_DCU_MON0, REG_DCU_MON1, REG_DCU_PM_MON, REG_EH_CLEAR, REG_ERROR_ACC, REG_ERROR_LIVE,
    REG_I2S_FORMAT, REG_PM2_CONFIG,
};

#[doc(inline)]
pub use amp::test_helpers as amp_test_helpers;

#[doc(inline)]
pub use light::{
    BH1750_ACCURACY_FACTOR, BH1750_ALT_I2C_ADDR, BH1750_DEFAULT_I2C_ADDR, BH1750_DEFAULT_I2C_BUS,
    I2cAddress, I2cHardwareInfo, Illuminance, LightDiagnostics, LightError, LightSensor,
    LightSensorMock,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use light::{HwState as LightHwState, test_helpers as light_test_helpers};

#[doc(inline)]
pub use encoder::{
    ENCODER_LED_BLUE_BCM_GPIO_PIN, ENCODER_LED_GREEN_BCM_GPIO_PIN, ENCODER_LED_RED_BCM_GPIO_PIN,
    ENCODER_PHASE_A_BCM_GPIO_PIN, ENCODER_PHASE_B_BCM_GPIO_PIN, ENCODER_PUSH_SW_BCM_GPIO_PIN,
    EncoderDevice, EncoderDeviceMock, EncoderError, EncoderHardwareInfo, EncoderStatus, RgbColor,
    RotationDirection,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use encoder::test_helpers as encoder_test_helpers;

#[doc(inline)]
pub use temp::{
    DS18B20_BCM_GPIO_PIN, DS18B20_DTOVERLAY, DS18B20_PHYSICAL_PIN, DS18B20_SYSFS_BASE_PATH,
    PowerSupplyMode, SensorDiagnostics, SensorId, SensorReading, SocThermal, TempError, TempSensor,
    TempSensorMock, Temperature, parse_w1_slave,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use temp::test_helpers as temp_test_helpers;
