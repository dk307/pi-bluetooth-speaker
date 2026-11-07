//! Shared runtime, logging, diagnostic helpers, and glibc compatibility
//! for the Raspberry Pi Bluetooth Speaker & DSP Appliance.

pub mod diag;
pub mod glibc;
pub mod logging;
pub mod runtime;
pub mod systemd;

pub use systemd::{ServiceState, parse_systemd_state};
