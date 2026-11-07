//! Bluetooth manager appliance service library.

pub mod agent;
pub mod bip_direct;
pub mod config;
pub mod cover_provider;
pub mod daemon;

pub use agent::{AgentCapabilityMode, AutoTrustAgent, AutoTrustShutdown};
pub use config::{BtManagerConfig, ConfigError, PollIntervalSecs};
