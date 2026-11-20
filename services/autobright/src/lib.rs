//! Ambient auto-brightness appliance service library.
//!
//! The crate owns the single writer of the display backlight. A 50 ms tick
//! loop reads the BH1750 ambient sensor through [`speaker_hardware`], advances
//! the pure [`speaker_hardware::AutoBrightness`] policy, and publishes its
//! state over `org.speaker.Autobright1` so the UI never writes sysfs directly.

#[doc(inline)]
pub use config::{
    AutobrightOptions, DEFAULT_BACKLIGHT_DEVICE, DEFAULT_CONFIG_PATH, DEFAULT_DURATION_SECS,
    DEFAULT_IDLE_RESET_SECS, DEFAULT_TICK_MS, ResolvedConfig, parse_cli_args_from, parse_u64_arg,
    print_usage,
};

#[doc(inline)]
pub use daemon::{
    BacklightIo, Command, LoopState, LuxSource, TickAction, TickInput, TickOutcome, run_tick,
    run_with,
};

#[doc(inline)]
pub use dbus::{
    AutobrightInterface, AutobrightProxy, DBUS_NAME, DBUS_PATH, PublishedState, emit_changes,
    publish_loop, serve,
};

#[doc(inline)]
pub use error::AutobrightError;

pub mod config;
pub mod daemon;
pub mod dbus;
pub mod error;
