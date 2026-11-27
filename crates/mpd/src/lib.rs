//! MPD (`rmpd`) client, domain models, and mockable peripheral abstraction.

pub mod client;

#[doc(inline)]
pub use client::{
    DEFAULT_MPD_HOST, DEFAULT_MPD_PORT, DEFAULT_MPD_TIMEOUT, MAX_MPD_PORT, MIN_MPD_PORT, MpdDevice,
    MpdDeviceMock, MpdError, MpdEvent, MpdEventStream, MpdHost, MpdMockCommand, MpdPlaybackState,
    MpdPort, MpdStatus, MpdSubsystem, MpdTrack,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use client::test_helpers as mpd_test_helpers;
