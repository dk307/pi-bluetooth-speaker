//! Touchscreen Appliance Console & Screen Manager service library.

#[cfg(feature = "ui")]
pub mod brightness;
pub mod config;
pub mod daemon;
pub mod settings_catalog;
#[cfg(feature = "ui")]
pub mod ui;
#[cfg(feature = "ui")]
pub mod volume;
