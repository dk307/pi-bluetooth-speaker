//! Audio Manager library crate.
//!
//! This crate contains the core logic for the audio-manager daemon,
//! separated from the binary for testability.

pub mod amp_backend;
pub mod amp_state;
pub mod config;
pub mod dbus_api;
pub mod error;
pub mod i2c_mute;
pub mod state_file;

// Re-export key types for consumers (tests, etc.)
pub use amp_backend::{AmpBackend, MockAmpBackend, MockAmpCtrl, RealAmpBackend};
pub use amp_state::AmpState;
pub use config::Config;
pub use dbus_api::{AudioManagerInterface, export_interface};
pub use error::{AmpError, AmpResult};
pub use state_file::StateFile;
