//! Touchscreen Appliance Console & Screen Manager service library.

pub mod config;
pub mod daemon;
#[cfg(feature = "ui")]
pub mod screen_brightness;
pub mod settings_catalog;
#[cfg(feature = "ui")]
pub mod ui;
#[cfg(feature = "ui")]
pub mod visualizer;
#[cfg(feature = "ui")]
pub mod volume;
