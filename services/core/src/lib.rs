//! Core library for the Raspberry Pi Bluetooth Speaker & DSP Appliance.
//!
//! Exposes hardware peripheral drivers, mock controllers for unit and integration
//! testing, and shared domain models used by both the `core` daemon and
//! standalone hardware diagnostic utilities.

pub mod hardware;
pub mod runtime;
