//! Core library for the Raspberry Pi Bluetooth Speaker & DSP Appliance.
//!
//! Exposes hardware peripheral drivers, mock controllers for unit and integration
//! testing, asynchronous runtime task join helpers, and shared domain models used
//! by both the `core` daemon and standalone hardware diagnostic utilities.

pub mod diag;
pub mod hardware;
pub mod runtime;
