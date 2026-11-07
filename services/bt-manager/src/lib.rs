//! Bluetooth manager appliance service library.

pub mod bip_direct;
pub mod config;
pub mod cover_provider;
pub mod daemon;

pub use config::{BtManagerConfig, ConfigError, PollIntervalSecs};
