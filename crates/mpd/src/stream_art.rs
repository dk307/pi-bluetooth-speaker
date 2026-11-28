//! Stream metadata extraction and cover-art resolution waterfall for MPD.
//!
//! Resolves artwork for both local MPD library tracks and HTTP/HTTPS radio
//! streams (ICY/SHOUTcast, HLS `.m3u8`, Ogg/Opus/FLAC) in strict priority order:
//! 1. Local / non-HTTP files via MPD `albumart` / `readpicture`.
//! 2. Source broadcast extraction (`icy-logo`, ICY `StreamUrl`, HLS `ID3v2`
//!    `APIC`/`WXXX`, Ogg/Opus/FLAC `METADATA_BLOCK_PICTURE`).
//! 3. Track metadata lookup (optional iTunes Search API by `(artist, title)`).
//! 4. Station fallback manifest (`/etc/speaker/stations.toml`).
//! 5. Optional configured `default_logo`.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use lru::LruCache;
use m3u8_rs::Playlist;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::client::{MpdDevice, MpdError, MpdTrack, codec_from_mime};

/// Default path for the station logo manifest.
pub const DEFAULT_STATIONS_MANIFEST_PATH: &str = "/etc/speaker/stations.toml";

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PLAYLIST_BYTES: usize = 512 * 1024;
const MAX_SEGMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STREAM_PROBE_BYTES: usize = 512 * 1024;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ICY_METADATA_BLOCKS: usize = 3;
const ART_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(32).unwrap();
const TRACK_LOOKUP_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

type ArtCache = Mutex<LruCache<(Box<str>, Box<str>), Vec<u8>>>;
type TrackLookupCache = Mutex<LruCache<(Box<str>, Box<str>), Option<Vec<u8>>>>;

/// Cache/trigger key identifying the current MPD track and stream metadata.
///
/// Unlike keying on `file` alone, including `title`, `artist`, and
/// `station_name` ensures ICY metadata track changes on a constant stream URL
/// re-trigger artwork resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MpdArtKey {
    /// Track file path or stream URL.
    pub file: Box<str>,
    /// Track title tag, if present.
    pub title: Option<Box<str>>,
    /// Primary artist tag, if present.
    pub artist: Option<Box<str>>,
    /// Station `Name` tag, if present.
    pub station_name: Option<Box<str>>,
}

impl MpdArtKey {
    /// Builds an [`MpdArtKey`] from an [`MpdTrack`].
    #[must_use]
    pub fn from_track(track: &MpdTrack) -> Self {
        Self {
            file: track.file.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            station_name: track.station_name.clone(),
        }
    }
}

/// Identifies which waterfall tier supplied the resolved artwork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ArtSource {
    /// MPD `albumart` or `readpicture` for local files.
    MpdNative,
    /// `icy-logo` HTTP response header.
    BroadcastIcyHeader,
    /// ICY in-band `StreamUrl` metadata frame.
    BroadcastStreamUrl,
    /// HLS `.m3u8` segment `ID3v2` `APIC` or `WXXX`.
    BroadcastHlsId3,
    /// Embedded `METADATA_BLOCK_PICTURE` in Ogg/Opus/FLAC stream.
    BroadcastContainerTag,
    /// External track metadata artwork lookup `(artist, title)`.
    TrackLookup,
    /// Matched rule in `/etc/speaker/stations.toml`.
    StationManifest,
    /// Station favicon lookup via Radio-Browser API.
    RadioBrowser,
    /// Configured `default_logo` in `/etc/speaker/stations.toml`.
    DefaultLogo,
    /// No artwork could be resolved.
    #[default]
    None,
}

/// Outcome of resolving artwork and stream codec metadata for an [`MpdTrack`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamArtResult {
    /// Raw encoded image bytes (JPEG/PNG/WebP), if resolved.
    pub image_bytes: Option<Vec<u8>>,
    /// Codec label inferred from stream HTTP `Content-Type` or container, if probed.
    pub probed_codec: Option<&'static str>,
    /// Waterfall tier that produced `image_bytes`.
    pub source: ArtSource,
    /// `true` if a transient network/MPD fault occurred so callers can retry.
    pub transient_error: bool,
}

/// Raw TOML configuration for a single station logo rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StationRuleConfig {
    /// Case-insensitive substring or regex matched against `station_name` / `title`.
    pub name_match: Option<Box<str>>,
    /// Case-insensitive substring or regex matched against `song.file` URL.
    pub url_match: Option<Box<str>>,
    /// Path to a local PNG/JPEG/WebP logo file.
    pub logo: Box<str>,
}

/// Raw TOML schema for `/etc/speaker/stations.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StationManifestConfig {
    /// Optional path to a default fallback logo image.
    pub default_logo: Option<Box<str>>,
    /// Whether to query track artwork by `(artist, title)` when broadcast art is absent.
    #[serde(default = "default_true")]
    pub enable_track_art_lookup: bool,
    /// Whether to query Radio-Browser for station logos when no local manifest rule matches.
    #[serde(default = "default_true")]
    pub enable_station_art_lookup: bool,
    /// Ordered list of station matching rules.
    #[serde(default)]
    pub station: Vec<StationRuleConfig>,
}

impl Default for StationManifestConfig {
    fn default() -> Self {
        Self {
            default_logo: None,
            enable_track_art_lookup: true,
            enable_station_art_lookup: true,
            station: Vec::new(),
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
struct CompiledStationRule {
    name_regex: Option<Regex>,
    url_regex: Option<Regex>,
    logo: Box<str>,
}

/// Compiled station logo manifest for fast fallback matching.
#[derive(Debug, Clone)]
pub struct StationManifest {
    default_logo: Option<Box<str>>,
    enable_track_art_lookup: bool,
    enable_station_art_lookup: bool,
    rules: Vec<CompiledStationRule>,
}

impl Default for StationManifest {
    fn default() -> Self {
        Self {
            default_logo: None,
            enable_track_art_lookup: true,
            enable_station_art_lookup: true,
            rules: Vec::new(),
        }
    }
}

impl StationManifest {
    /// Parses and compiles a [`StationManifest`] from a TOML string.
    ///
    /// # Errors
    /// Returns [`MpdError`] if the TOML syntax or any regex pattern is invalid.
    pub fn from_toml_str(raw: &str) -> Result<Self, MpdError> {
        let cfg: StationManifestConfig = toml::from_str(raw)
            .map_err(|e| MpdError::invalid_state(format!("invalid stations.toml: {e}")))?;
        let mut rules = Vec::with_capacity(cfg.station.len());
        for rule in cfg.station {
            let name_regex = rule
                .name_match
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(compile_case_insensitive_regex)
                .transpose()?;
            let url_regex = rule
                .url_match
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(compile_case_insensitive_regex)
                .transpose()?;
            rules.push(CompiledStationRule {
                name_regex,
                url_regex,
                logo: rule.logo,
            });
        }
        Ok(Self {
            default_logo: cfg.default_logo,
            enable_track_art_lookup: cfg.enable_track_art_lookup,
            enable_station_art_lookup: cfg.enable_station_art_lookup,
            rules,
        })
    }

    /// Loads a [`StationManifest`] from `path`, falling back to [`Self::default()`]
    /// if the file does not exist or cannot be parsed.
    #[must_use]
    pub fn load_or_default(path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match Self::from_toml_str(&raw) {
            Ok(manifest) => manifest,
            Err(e) => {
                tracing::warn!(error = %e.summary(), path = %path.display(), "Failed to parse stations manifest; using defaults");
                Self::default()
            }
        }
    }

    /// Returns whether external track artwork lookup is enabled.
    #[must_use]
    pub const fn track_art_lookup_enabled(&self) -> bool {
        self.enable_track_art_lookup
    }

    /// Returns whether Radio-Browser station-logo lookup is enabled.
    #[must_use]
    pub const fn station_art_lookup_enabled(&self) -> bool {
        self.enable_station_art_lookup
    }

    /// Returns the configured `default_logo` path, if any.
    #[must_use]
    pub fn default_logo(&self) -> Option<&str> {
        self.default_logo.as_deref()
    }

    /// Matches a track's `file` URL or `station_name` against configured station rules.
    #[must_use]
    pub fn match_station_logo(&self, url: &str, station_name: Option<&str>) -> Option<&str> {
        for rule in &self.rules {
            let url_matched = rule.url_regex.as_ref().is_some_and(|re| re.is_match(url));
            let name_matched = rule
                .name_regex
                .as_ref()
                .is_some_and(|re| station_name.is_some_and(|n| re.is_match(n)));
            if url_matched || name_matched {
                return Some(rule.logo.as_ref());
            }
        }
        None
    }
}

fn compile_case_insensitive_regex(pattern: &str) -> Result<Regex, MpdError> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| MpdError::invalid_state(format!("invalid station regex '{pattern}': {e}")))
}

/// Parsed in-band ICY metadata fields from a SHOUTcast/Icecast frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IcyMetadata {
    /// `StreamTitle='...'` value, if present and non-empty.
    pub stream_title: Option<Box<str>>,
    /// `StreamUrl='...'` value, if present and non-empty.
    pub stream_url: Option<Box<str>>,
}

/// Parses a single ICY metadata payload block (`StreamTitle='...';StreamUrl='...';`).
#[must_use]
pub fn parse_icy_metadata_block(block: &[u8]) -> IcyMetadata {
    let text = String::from_utf8_lossy(block);
    let trimmed = text.trim_end_matches('\0');
    IcyMetadata {
        stream_title: extract_icy_field(trimmed, "StreamTitle="),
        stream_url: extract_icy_field(trimmed, "StreamUrl="),
    }
}

fn extract_icy_field(text: &str, key: &str) -> Option<Box<str>> {
    let lower = text.to_ascii_lowercase();
    let key_lower = key.to_ascii_lowercase();
    let idx = lower.find(&key_lower)?;
    let rest = &text[idx + key.len()..];
    let quote = rest.chars().next().filter(|&c| c == '\'' || c == '"')?;
    let after_open = &rest[quote.len_utf8()..];
    // ICY fields terminate at `<quote>;` or `<quote>` at end-of-block.
    let end = after_open
        .find(&format!("{quote};"))
        .or_else(|| after_open.rfind(quote))?;
    let val = after_open[..end].trim();
    (!val.is_empty()).then(|| Box::from(val))
}

/// Reads up to `max_blocks` ICY metadata frames from a byte stream after `metaint`
/// audio-byte intervals, stopping as soon as a `StreamUrl` is found.
pub fn extract_icy_stream_metadata<R: Read>(
    reader: &mut R,
    metaint: usize,
    max_blocks: usize,
) -> Option<IcyMetadata> {
    if metaint == 0 || metaint > MAX_STREAM_PROBE_BYTES {
        return None;
    }
    let mut skip_buf = vec![0u8; metaint.min(16 * 1024)];
    let mut best: Option<IcyMetadata> = None;

    for _ in 0..max_blocks {
        let mut remaining = metaint;
        while remaining > 0 {
            let chunk = remaining.min(skip_buf.len());
            if reader.read_exact(&mut skip_buf[..chunk]).is_err() {
                return best;
            }
            remaining -= chunk;
        }
        let mut len_byte = [0u8; 1];
        if reader.read_exact(&mut len_byte).is_err() {
            return best;
        }
        let meta_len = usize::from(len_byte[0]) * 16;
        if meta_len == 0 {
            continue;
        }
        let mut meta_buf = vec![0u8; meta_len];
        if reader.read_exact(&mut meta_buf).is_err() {
            return best;
        }
        let parsed = parse_icy_metadata_block(&meta_buf);
        let has_url = parsed.stream_url.is_some();
        if parsed.stream_url.is_some() || parsed.stream_title.is_some() {
            best = Some(parsed);
        }
        if has_url {
            return best;
        }
    }
    best
}

/// Result of inspecting an audio container or ID3 tag block with `lofty`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmbeddedTagArt {
    /// Embedded picture bytes (`APIC` or `METADATA_BLOCK_PICTURE`).
    pub picture_bytes: Option<Vec<u8>>,
    /// Artwork URL from `ID3v2` `WXXX` / user URL frame, if no embedded picture was found.
    pub artwork_url: Option<Box<str>>,
}

/// Extracts embedded cover art or artwork URLs from audio container/ID3 bytes using `lofty`.
#[must_use]
pub fn extract_embedded_tag_art(bytes: &[u8]) -> EmbeddedTagArt {
    if bytes.is_empty() {
        return EmbeddedTagArt::default();
    }
    let Ok(probe) = Probe::new(Cursor::new(bytes)).guess_file_type() else {
        return EmbeddedTagArt::default();
    };
    let Ok(tagged_file) = probe.read() else {
        return EmbeddedTagArt::default();
    };
    for tag in tagged_file.tags() {
        if let Some(pic) = tag.pictures().first() {
            return EmbeddedTagArt {
                picture_bytes: Some(pic.data().to_vec()),
                artwork_url: None,
            };
        }
    }
    for tag in tagged_file.tags() {
        if let Some(url) = tag
            .get_string(ItemKey::AudioFileUrl)
            .or_else(|| tag.get_string(ItemKey::AudioSourceUrl))
            .or_else(|| tag.get_string(ItemKey::CommercialInformationUrl))
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        {
            return EmbeddedTagArt {
                picture_bytes: None,
                artwork_url: Some(Box::from(url)),
            };
        }
    }
    EmbeddedTagArt::default()
}

/// Parses an HLS `.m3u8` playlist and returns the resolved URI of the latest segment
/// (or first variant playlist if `playlist_bytes` is a master playlist).
#[must_use]
pub fn parse_hls_target_url(playlist_bytes: &[u8], base_url: &str) -> Option<(String, bool)> {
    match m3u8_rs::parse_playlist_res(playlist_bytes).ok()? {
        Playlist::MasterPlaylist(master) => master
            .variants
            .first()
            .map(|v| (resolve_relative_url(base_url, &v.uri), true)),
        Playlist::MediaPlaylist(media) => media
            .segments
            .last()
            .map(|s| (resolve_relative_url(base_url, &s.uri), false)),
    }
}

/// Resolves a relative or root-relative URI against `base_url`.
#[must_use]
pub fn resolve_relative_url(base_url: &str, reference: &str) -> String {
    let trimmed = reference.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    if let Some(scheme_end) = base_url.find("://") {
        let after_scheme = &base_url[scheme_end + 3..];
        let origin_end = after_scheme
            .find('/')
            .map_or(base_url.len(), |idx| scheme_end + 3 + idx);
        let origin = &base_url[..origin_end];
        if trimmed.starts_with('/') {
            return format!("{origin}{trimmed}");
        }
        let base_dir_end = base_url
            .split(['?', '#'])
            .next()
            .and_then(|p| p.rfind('/'))
            .filter(|&idx| idx >= origin_end)
            .map(|idx| idx + 1);
        return match base_dir_end {
            Some(dir_end) => format!("{}{trimmed}", &base_url[..dir_end]),
            None => format!("{origin}/{trimmed}"),
        };
    }
    trimmed.to_string()
}

/// Mock controller for [`StreamArtResolver`] (used by integration tests).
#[derive(Debug, Clone)]
pub struct StreamArtResolverMock {
    state: Arc<Mutex<MockArtState>>,
}

#[derive(Debug, Default)]
struct MockArtState {
    stream_probes: HashMap<Box<str>, StreamArtResult>,
    track_lookup: HashMap<(Box<str>, Box<str>), Vec<u8>>,
    station_lookup: HashMap<Box<str>, Vec<u8>>,
    local_files: HashMap<Box<str>, Vec<u8>>,
}

impl StreamArtResolverMock {
    /// Registers a simulated broadcast probe result for `stream_url`.
    pub fn set_stream_probe(&self, stream_url: &str, result: StreamArtResult) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stream_probes
            .insert(Box::from(stream_url), result);
    }

    /// Registers a simulated track-lookup image for `(artist, title)`.
    pub fn set_track_lookup(&self, artist: &str, title: &str, bytes: Vec<u8>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .track_lookup
            .insert(
                (
                    Box::from(artist.to_ascii_lowercase()),
                    Box::from(title.to_ascii_lowercase()),
                ),
                bytes,
            );
    }

    /// Registers a simulated Radio-Browser station-logo image for `station_name`.
    pub fn set_station_lookup(&self, station_name: &str, bytes: Vec<u8>) {
        let cleaned = clean_station_query(station_name).to_ascii_lowercase();
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .station_lookup
            .insert(Box::from(cleaned), bytes);
    }

    /// Registers simulated local logo file bytes for a manifest path.
    pub fn set_local_file(&self, path: &str, bytes: Vec<u8>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .local_files
            .insert(Box::from(path), bytes);
    }
}

#[derive(Debug)]
enum ResolverBackend {
    Live {
        agent: ureq::Agent,
        art_cache: ArtCache,
        track_cache: TrackLookupCache,
    },
    Mock(Arc<Mutex<MockArtState>>),
}

/// Resolves cover art and stream codec metadata for MPD tracks and radio streams.
#[derive(Debug, Clone)]
pub struct StreamArtResolver {
    manifest: Arc<Mutex<StationManifest>>,
    backend: Arc<ResolverBackend>,
}

impl StreamArtResolver {
    /// Creates a live HTTP/disk-backed [`StreamArtResolver`] using `manifest`.
    #[must_use]
    pub fn new(manifest: StationManifest) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(DEFAULT_HTTP_TIMEOUT))
            .max_redirects(3)
            .build();
        let agent: ureq::Agent = config.into();
        Self {
            manifest: Arc::new(Mutex::new(manifest)),
            backend: Arc::new(ResolverBackend::Live {
                agent,
                art_cache: Mutex::new(LruCache::new(ART_CACHE_CAPACITY)),
                track_cache: Mutex::new(LruCache::new(TRACK_LOOKUP_CACHE_CAPACITY)),
            }),
        }
    }

    /// Creates a mocked [`StreamArtResolver`] and its companion [`StreamArtResolverMock`].
    #[must_use]
    pub fn new_mocked(manifest: StationManifest) -> (Self, StreamArtResolverMock) {
        let state = Arc::new(Mutex::new(MockArtState::default()));
        let resolver = Self {
            manifest: Arc::new(Mutex::new(manifest)),
            backend: Arc::new(ResolverBackend::Mock(Arc::clone(&state))),
        };
        (resolver, StreamArtResolverMock { state })
    }

    /// Replaces the active [`StationManifest`] (e.g., on `SIGHUP` reload).
    pub fn reload_manifest(&self, manifest: StationManifest) {
        *self
            .manifest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = manifest;
    }

    /// Resolves cover art and stream codec metadata for `track` using the waterfall.
    pub async fn resolve_for_track(&self, track: &MpdTrack, mpd: &MpdDevice) -> StreamArtResult {
        if track.file.is_empty() {
            return StreamArtResult::default();
        }
        // Tier 1: Local / non-HTTP files via MPD native `albumart` / `readpicture`.
        if !track.is_stream_url() {
            match mpd.read_album_art(track.file.as_ref()).await {
                Ok(Some(bytes)) => {
                    return StreamArtResult {
                        image_bytes: Some(bytes),
                        probed_codec: Some(track.codec_label()),
                        source: ArtSource::MpdNative,
                        transient_error: false,
                    };
                }
                Ok(None) => return self.resolve_manifest_fallback(track),
                Err(e) => {
                    tracing::debug!(error = %e.summary(), file = %track.file, "MPD native albumart failed");
                    let mut fallback = self.resolve_manifest_fallback(track);
                    if fallback.image_bytes.is_none() {
                        fallback.transient_error = true;
                    }
                    return fallback;
                }
            }
        }

        // Tier 2: Source broadcast extraction (ICY / HLS / Ogg-Opus-FLAC).
        let this = self.clone();
        let track_owned = track.clone();
        match tokio::task::spawn_blocking(move || this.resolve_stream_blocking(&track_owned)).await
        {
            Ok(res) => res,
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(_) => StreamArtResult::default(),
        }
    }

    fn resolve_stream_blocking(&self, track: &MpdTrack) -> StreamArtResult {
        let mut probed_codec: Option<&'static str> = None;

        // Tier 2: Source broadcast extraction.
        let broadcast = match &*self.backend {
            ResolverBackend::Live {
                agent, art_cache, ..
            } => probe_live_stream(agent, art_cache, track.file.as_ref()),
            ResolverBackend::Mock(mock) => mock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stream_probes
                .get(track.file.as_ref())
                .cloned()
                .unwrap_or_default(),
        };
        if broadcast.probed_codec.is_some() {
            probed_codec = broadcast.probed_codec;
        }
        if broadcast.image_bytes.is_some() {
            return broadcast;
        }

        // Tier 3: Track metadata artwork lookup `(artist, title)`.
        let (track_lookup_enabled, station_lookup_enabled) = {
            let m = self
                .manifest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (m.track_art_lookup_enabled(), m.station_art_lookup_enabled())
        };
        if track_lookup_enabled
            && let (Some(artist), Some(title)) = track.resolved_artist_title()
            && let Some(bytes) = self.lookup_track_artwork(artist, title)
        {
            return StreamArtResult {
                image_bytes: Some(bytes),
                probed_codec,
                source: ArtSource::TrackLookup,
                transient_error: false,
            };
        }

        // Tier 4: Station manifest rule match.
        if let Some(mut res) = self.resolve_station_manifest(track) {
            res.probed_codec = probed_codec;
            return res;
        }

        // Tier 5: Radio-Browser station logo lookup.
        if station_lookup_enabled
            && let Some(station) = track
                .station_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            && let Some(bytes) = self.lookup_station_artwork(station)
        {
            return StreamArtResult {
                image_bytes: Some(bytes),
                probed_codec,
                source: ArtSource::RadioBrowser,
                transient_error: false,
            };
        }

        // Tier 6: Default logo fallback.
        let mut fallback = self.resolve_default_logo();
        fallback.probed_codec = probed_codec;
        if fallback.image_bytes.is_none() && broadcast.transient_error {
            fallback.transient_error = true;
        }
        fallback
    }

    fn lookup_station_artwork(&self, station_name: &str) -> Option<Vec<u8>> {
        let cleaned = clean_station_query(station_name);
        if cleaned.is_empty() {
            return None;
        }
        let key: (Box<str>, Box<str>) = (
            Box::from("__station__"),
            Box::from(cleaned.to_ascii_lowercase()),
        );
        match &*self.backend {
            ResolverBackend::Mock(mock) => mock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .station_lookup
                .get(key.1.as_ref())
                .cloned(),
            ResolverBackend::Live {
                agent, track_cache, ..
            } => {
                if let Some(cached) = track_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&key)
                {
                    return cached.clone();
                }
                let fetched = fetch_radio_browser_favicon(agent, &cleaned);
                track_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .put(key, fetched.clone());
                fetched
            }
        }
    }

    fn lookup_track_artwork(&self, artist: &str, title: &str) -> Option<Vec<u8>> {
        let key = (
            Box::from(artist.trim().to_ascii_lowercase()),
            Box::from(title.trim().to_ascii_lowercase()),
        );
        match &*self.backend {
            ResolverBackend::Mock(mock) => mock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .track_lookup
                .get(&key)
                .cloned(),
            ResolverBackend::Live {
                agent, track_cache, ..
            } => {
                if let Some(cached) = track_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&key)
                {
                    return cached.clone();
                }
                let fetched = fetch_itunes_artwork(agent, artist, title);
                track_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .put(key, fetched.clone());
                fetched
            }
        }
    }

    fn resolve_manifest_fallback(&self, track: &MpdTrack) -> StreamArtResult {
        self.resolve_station_manifest(track)
            .unwrap_or_else(|| self.resolve_default_logo())
    }

    fn resolve_station_manifest(&self, track: &MpdTrack) -> Option<StreamArtResult> {
        let matched_logo = {
            let manifest = self
                .manifest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manifest
                .match_station_logo(track.file.as_ref(), track.station_name.as_deref())
                .map(Box::<str>::from)
        };
        if let Some(path) = matched_logo
            && let Some(bytes) = self.read_local_logo(path.as_ref())
        {
            return Some(StreamArtResult {
                image_bytes: Some(bytes),
                probed_codec: None,
                source: ArtSource::StationManifest,
                transient_error: false,
            });
        }
        None
    }

    fn resolve_default_logo(&self) -> StreamArtResult {
        let default_logo = self
            .manifest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .default_logo()
            .map(Box::<str>::from);
        if let Some(path) = default_logo
            && let Some(bytes) = self.read_local_logo(path.as_ref())
        {
            return StreamArtResult {
                image_bytes: Some(bytes),
                probed_codec: None,
                source: ArtSource::DefaultLogo,
                transient_error: false,
            };
        }
        StreamArtResult::default()
    }

    fn read_local_logo(&self, path: &str) -> Option<Vec<u8>> {
        match &*self.backend {
            ResolverBackend::Mock(mock) => mock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .local_files
                .get(path)
                .cloned(),
            ResolverBackend::Live { .. } => {
                let meta = std::fs::metadata(path).ok()?;
                if meta.len() > MAX_IMAGE_BYTES as u64 {
                    return None;
                }
                std::fs::read(path).ok()
            }
        }
    }
}

fn probe_live_stream(
    agent: &ureq::Agent,
    art_cache: &ArtCache,
    stream_url: &str,
) -> StreamArtResult {
    let is_hls = stream_url
        .split(['?', '#'])
        .next()
        .is_some_and(|p| p.to_ascii_lowercase().ends_with(".m3u8"));
    if is_hls {
        return probe_hls_stream(agent, art_cache, stream_url);
    }

    let mut resp = match agent
        .get(stream_url)
        .header("Icy-MetaData", "1")
        .header("User-Agent", "pi-bluetooth-speaker/2.3")
        .call()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, url = %stream_url, "HTTP stream probe failed");
            return StreamArtResult {
                transient_error: true,
                ..StreamArtResult::default()
            };
        }
    };

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let probed_codec = content_type.as_deref().and_then(codec_from_mime);

    if let Some(logo_url) = resp
        .headers()
        .get("icy-logo")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        && let Some(bytes) = fetch_cached_image(agent, art_cache, stream_url, logo_url)
    {
        return StreamArtResult {
            image_bytes: Some(bytes),
            probed_codec,
            source: ArtSource::BroadcastIcyHeader,
            transient_error: false,
        };
    }

    let metaint = resp
        .headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());

    if let Some(interval) = metaint {
        let mut reader = resp
            .body_mut()
            .as_reader()
            .take(MAX_STREAM_PROBE_BYTES as u64);
        if let Some(meta) =
            extract_icy_stream_metadata(&mut reader, interval, MAX_ICY_METADATA_BLOCKS)
            && let Some(art_url) = meta.stream_url.as_deref()
            && (art_url.starts_with("http://") || art_url.starts_with("https://"))
            && let Some(bytes) = fetch_cached_image(agent, art_cache, stream_url, art_url)
        {
            return StreamArtResult {
                image_bytes: Some(bytes),
                probed_codec,
                source: ArtSource::BroadcastStreamUrl,
                transient_error: false,
            };
        }
    } else if matches!(probed_codec, Some("OGG" | "OPUS" | "FLAC")) {
        let mut buf = Vec::new();
        let _ = resp
            .body_mut()
            .as_reader()
            .take(MAX_STREAM_PROBE_BYTES as u64)
            .read_to_end(&mut buf);
        let embedded = extract_embedded_tag_art(&buf);
        if let Some(bytes) = embedded.picture_bytes {
            return StreamArtResult {
                image_bytes: Some(bytes),
                probed_codec,
                source: ArtSource::BroadcastContainerTag,
                transient_error: false,
            };
        }
    }

    StreamArtResult {
        image_bytes: None,
        probed_codec,
        source: ArtSource::None,
        transient_error: false,
    }
}

fn probe_hls_stream(
    agent: &ureq::Agent,
    art_cache: &ArtCache,
    playlist_url: &str,
) -> StreamArtResult {
    let mut current_url = playlist_url.to_string();
    for _ in 0..2 {
        let Ok(mut resp) = agent.get(&current_url).call() else {
            return StreamArtResult {
                probed_codec: Some("HLS"),
                ..StreamArtResult::default()
            };
        };
        let mut playlist_bytes = Vec::new();
        if resp
            .body_mut()
            .as_reader()
            .take(MAX_PLAYLIST_BYTES as u64)
            .read_to_end(&mut playlist_bytes)
            .is_err()
        {
            return StreamArtResult {
                probed_codec: Some("HLS"),
                ..StreamArtResult::default()
            };
        }
        let Some((target, is_master)) = parse_hls_target_url(&playlist_bytes, &current_url) else {
            break;
        };
        if is_master {
            current_url = target;
            continue;
        }
        let Ok(mut seg_resp) = agent.get(&target).call() else {
            break;
        };
        let mut seg_bytes = Vec::new();
        if seg_resp
            .body_mut()
            .as_reader()
            .take(MAX_SEGMENT_BYTES as u64)
            .read_to_end(&mut seg_bytes)
            .is_ok()
        {
            let art = extract_embedded_tag_art(&seg_bytes);
            if let Some(bytes) = art.picture_bytes {
                return StreamArtResult {
                    image_bytes: Some(bytes),
                    probed_codec: Some("HLS"),
                    source: ArtSource::BroadcastHlsId3,
                    transient_error: false,
                };
            }
            if let Some(url) = art.artwork_url
                && let Some(bytes) = fetch_cached_image(agent, art_cache, playlist_url, &url)
            {
                return StreamArtResult {
                    image_bytes: Some(bytes),
                    probed_codec: Some("HLS"),
                    source: ArtSource::BroadcastHlsId3,
                    transient_error: false,
                };
            }
        }
        break;
    }
    StreamArtResult {
        image_bytes: None,
        probed_codec: Some("HLS"),
        source: ArtSource::None,
        transient_error: false,
    }
}

fn fetch_cached_image(
    agent: &ureq::Agent,
    art_cache: &ArtCache,
    stream_url: &str,
    art_url: &str,
) -> Option<Vec<u8>> {
    let key = (Box::from(stream_url), Box::from(art_url));
    if let Some(cached) = art_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Some(cached.clone());
    }
    let bytes = fetch_image_bytes(agent, art_url)?;
    art_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .put(key, bytes.clone());
    Some(bytes)
}

fn fetch_image_bytes(agent: &ureq::Agent, url: &str) -> Option<Vec<u8>> {
    let mut resp = agent.get(url).call().ok()?;
    let is_image = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim().to_ascii_lowercase().starts_with("image/"));
    if !is_image {
        return None;
    }
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    Some(bytes)
}

#[derive(Deserialize)]
struct ItunesSearchResponse {
    results: Vec<ItunesTrackResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItunesTrackResult {
    artwork_url100: Option<String>,
}

fn fetch_itunes_artwork(agent: &ureq::Agent, artist: &str, title: &str) -> Option<Vec<u8>> {
    let query = format!("{artist} {title}");
    let mut resp = agent
        .get("https://itunes.apple.com/search")
        .query("term", &query)
        .query("entity", "song")
        .query("limit", "1")
        .call()
        .ok()?;
    let reader = resp.body_mut().as_reader().take(256 * 1024);
    let parsed: ItunesSearchResponse = serde_json::from_reader(reader).ok()?;
    let art_100 = parsed.results.first()?.artwork_url100.as_deref()?;
    let hi_res = art_100.replace("100x100bb", "600x600bb");
    fetch_image_bytes(agent, &hi_res).or_else(|| fetch_image_bytes(agent, art_100))
}

#[derive(Deserialize)]
struct RadioBrowserStation {
    favicon: Option<String>,
}

fn clean_station_query(raw: &str) -> String {
    // Strip parenthesized/bracketed format suffixes like "(HLS)" or "[SomaFM]".
    let before_paren = raw.split(['(', '[']).next().unwrap_or(raw);
    before_paren
        .split(':')
        .next()
        .unwrap_or(before_paren)
        .trim()
        .to_string()
}

fn fetch_radio_browser_favicon(agent: &ureq::Agent, station_name: &str) -> Option<Vec<u8>> {
    let mut resp = agent
        .get("https://de1.api.radio-browser.info/json/stations/search")
        .header("User-Agent", "pi-bluetooth-speaker/2.3")
        .query("name", station_name)
        .query("limit", "3")
        .query("hidebroken", "true")
        .query("order", "clickcount")
        .query("reverse", "true")
        .call()
        .ok()?;
    let reader = resp.body_mut().as_reader().take(256 * 1024);
    let stations: Vec<RadioBrowserStation> = serde_json::from_reader(reader).ok()?;
    for st in stations {
        if let Some(url) = st
            .favicon
            .as_deref()
            .map(str::trim)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            && let Some(bytes) = fetch_image_bytes(agent, url)
        {
            return Some(bytes);
        }
    }
    None
}

/// Compile-time proof that [`StreamArtResolver`] and [`StreamArtResolverMock`] are `Send + Sync`.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StreamArtResolver>();
    assert_send_sync::<StreamArtResolverMock>();
    assert_send_sync::<MpdArtKey>();
    assert_send_sync::<StationManifest>();
};
