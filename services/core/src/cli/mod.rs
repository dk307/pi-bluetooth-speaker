//! Standalone CLI diagnostic application logic.
//!
//! Exposes argument parsing, iteration handling, and execution loops
//! for hardware diagnostic utilities, allowing full unit and integration testing
//! outside binary source files.

pub mod amp;
pub mod bt;
pub mod core_daemon;
pub mod dsp;
pub mod encoder;
pub mod light;
pub mod temp;
