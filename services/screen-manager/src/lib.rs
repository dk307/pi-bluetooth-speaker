//! Touchscreen Appliance Console & Screen Manager service library.

pub mod config;
pub mod daemon;
#[cfg(feature = "ui")]
pub mod ui;
#[cfg(feature = "ui")]
pub mod volume;
