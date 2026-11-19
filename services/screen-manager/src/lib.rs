//! Touchscreen Appliance Console & Screen Manager service library.

pub mod config;
pub mod daemon;
pub mod settings_catalog;
#[cfg(feature = "ui")]
pub mod ui;
#[cfg(feature = "ui")]
pub mod volume;
