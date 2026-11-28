//! MPD (`rmpd`) client, domain models, and mockable peripheral abstraction.

pub mod client;
#[cfg(feature = "stream-art")]
pub mod stream_art;

#[doc(inline)]
pub use client::{
    DEFAULT_MPD_HOST, DEFAULT_MPD_PORT, DEFAULT_MPD_TIMEOUT, MAX_MPD_PORT, MIN_MPD_PORT, MpdDevice,
    MpdDeviceMock, MpdError, MpdEvent, MpdEventStream, MpdHost, MpdMockCommand, MpdPlaybackState,
    MpdPort, MpdStatus, MpdSubsystem, MpdTrack,
};
#[cfg(feature = "stream-art")]
#[doc(inline)]
pub use stream_art::{
    ArtSource, DEFAULT_STATIONS_MANIFEST_PATH, EmbeddedTagArt, IcyMetadata, MpdArtKey,
    StationManifest, StationManifestConfig, StationRuleConfig, StreamArtResolver,
    StreamArtResolverMock, StreamArtResult, extract_embedded_tag_art, extract_icy_stream_metadata,
    parse_hls_target_url, parse_icy_metadata_block, resolve_relative_url,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use client::test_helpers as mpd_test_helpers;
