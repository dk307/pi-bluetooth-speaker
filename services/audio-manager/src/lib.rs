//! Audio Manager library crate.
//!
//! This crate contains the core logic for the audio-manager daemon,
//! separated from the binary for testability.

pub mod amp_backend;
pub mod amp_controller;
pub mod amp_gpio;
pub mod amp_state;
pub mod config;
pub mod dbus_api;
pub mod dsp_watch;
pub mod error;
pub mod rate_watch;
pub mod sources;
pub mod state_file;

// Re-export key types for consumers (tests, etc.)
pub use amp_backend::{AmpBackend, MockAmpBackend, MockAmpCtrl, RealAmpBackend};
pub use amp_state::AmpState;
pub use config::Config;
pub use dbus_api::{AudioManagerInterface, export_interface};
pub use error::{AmpError, AmpResult};
pub use sources::arbiter::{ArbiterOutcome, DspTarget, SourceArbiter, SourceStatusView};
pub use sources::control::{
    MockSourceControlCtrl, SourceControl, SourceControlError, SourceController,
};
pub use sources::dbus_sources::{
    AUDIO_SOURCES_INTERFACE, AUDIO_SOURCES_OBJECT_PATH, AudioSourcesInterface,
    AudioSourcesSnapshot, SourceCommand, SourceCommandKind, export_sources_interface,
};
pub use sources::manager::{SourceManager, SourceManagerConfig};
pub use sources::registry::{
    AutoSwitchPolicy, CaptureSpec, ControlSpec, DEFAULT_SOURCE_RATE_HZ, MAX_LOOPBACK_SUBDEVICES,
    SourceDefinition, SourceId, SourceRegistry,
};
pub use sources::state_file::{PersistedSourceState, SourceStateFile};
pub use state_file::StateFile;
