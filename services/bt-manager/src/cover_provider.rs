//! D-Bus cover-art provider: the obexd replacement for the appliance.
//!
//! Runs alongside the `bt-manager` monitor loop. It keeps a persistent BIP
//! OBEX session alive to the connected phone (AOSP only advertises
//! `ImgHandle` values in `MediaPlayer1.Track` while a client session
//! exists — the role obexd played), pulls each track's artwork directly
//! over L2CAP/RFCOMM with [`crate::hardware::bluetooth::bip_direct`], and
//! publishes it on the system D-Bus for future consumers:
//!
//! - bus name `org.speaker.CoverArt`
//! - object path `/org/speaker/coverart`, interface `org.speaker.CoverArt1`
//! - properties `device`, `handle`, `path` (empty string when inactive)
//! - method `GetImage() -> ay` returning the raw JPEG bytes
//! - signals `ImageUpdated(device, handle, path)` and `ImageCleared()`
//!
//! Future `core` builds then pick the image up from D-Bus instead of making
//! their own cover-art connections.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::Notify;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use crate::bip_direct::{
    BipImageType, BipSessionKeeper, BipTransport, PersistentBipSession, classify_obex_port,
};
use speaker_bluetooth::cover_service::{COVER_ART_DBUS_NAME, COVER_ART_OBJECT_PATH};
use speaker_bluetooth::{BluetoothDevice, BtAddress, BtError, BtPlaybackState};
use speaker_bluetooth::{
    clean_old_cover_art_in, cover_art_path_for_in, ensure_cover_art_cache_dir_in, is_complete_jpeg,
    validate_bip_handle,
};

/// Published cover-art state mirrored on D-Bus.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoverState {
    /// The published image; `None` when nothing is published. All three
    /// fields always change together, so they live in one sum type.
    image: Option<ActiveCover>,
}

/// One published cover-art image (all fields set together).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCover {
    /// Canonical MAC address of the device the artwork belongs to.
    pub device: Box<str>,
    /// AOSP BIP image handle of the published image.
    pub handle: Box<str>,
    /// Cache file path of the published JPEG.
    pub path: Box<str>,
    /// BIP session generation (`BipSessionKeeper::epoch`) the image was
    /// published under. Handles are only unique within one OBEX session, so
    /// the worker must re-fetch even when a new session re-advertises the
    /// same handle value.
    pub epoch: u64,
}

impl CoverState {
    /// Whether any image is currently published.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.image.is_some()
    }

    /// Canonical MAC of the published device, if any.
    #[must_use]
    pub fn device(&self) -> Option<&str> {
        self.image.as_ref().map(|c| c.device.as_ref())
    }

    /// Published BIP image handle, if any.
    #[must_use]
    pub fn handle(&self) -> Option<&str> {
        self.image.as_ref().map(|c| c.handle.as_ref())
    }

    /// Published cache-file path, if any.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.image.as_ref().map(|c| c.path.as_ref())
    }

    /// BIP session generation the image was published under, if any.
    #[must_use]
    pub fn epoch(&self) -> Option<u64> {
        self.image.as_ref().map(|c| c.epoch)
    }
}

/// Shared handle to the published cover-art state (D-Bus interface + worker).
#[derive(Debug, Clone, Default)]
pub struct SharedCoverState(Arc<Mutex<CoverState>>);

impl SharedCoverState {
    /// Creates an empty shared state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CoverState> {
        // A poisoned lock means a panicked holder; the data is still valid
        // (plain strings), so recover instead of losing the published image.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Snapshot of the current state.
    #[must_use]
    pub fn get(&self) -> CoverState {
        self.lock().clone()
    }

    /// Publishes an active cover-art image for the BIP session `epoch`.
    pub fn publish(&self, device: Box<str>, handle: Box<str>, path: Box<str>, epoch: u64) {
        *self.lock() = CoverState {
            image: Some(ActiveCover {
                device,
                handle,
                path,
                epoch,
            }),
        };
    }

    /// Removes any published image, returning the previous state.
    #[must_use]
    pub fn clear(&self) -> CoverState {
        std::mem::take(&mut *self.lock())
    }
}

/// D-Bus interface implementation serving [`SharedCoverState`].
#[derive(Debug)]
pub struct CoverArtIface {
    state: SharedCoverState,
}

impl CoverArtIface {
    /// Serves `state` over the `org.speaker.CoverArt1` interface.
    #[must_use]
    pub fn new(state: SharedCoverState) -> Self {
        Self { state }
    }
}

#[zbus::interface(name = "org.speaker.CoverArt1")]
impl CoverArtIface {
    /// Canonical MAC address of the device the published artwork belongs to.
    #[zbus(property)]
    fn device(&self) -> String {
        self.state
            .get()
            .device()
            .map_or_else(String::new, str::to_string)
    }

    /// AOSP BIP image handle of the published artwork (empty when inactive).
    #[zbus(property)]
    fn handle(&self) -> String {
        self.state
            .get()
            .handle()
            .map_or_else(String::new, str::to_string)
    }

    /// Cache file path of the published JPEG (empty when inactive).
    #[zbus(property)]
    fn path(&self) -> String {
        self.state
            .get()
            .path()
            .map_or_else(String::new, str::to_string)
    }

    /// Returns the raw JPEG bytes of the published artwork.
    ///
    /// # Errors
    /// Returns `Failed` when no image is published or the cache file cannot
    /// be read.
    fn get_image(&self) -> zbus::fdo::Result<Vec<u8>> {
        let path = self
            .state
            .get()
            .path()
            .ok_or_else(|| zbus::fdo::Error::Failed("no cover art published".into()))?
            .to_owned();
        std::fs::read(Path::new(&*path))
            .map_err(|e| zbus::fdo::Error::Failed(format!("cannot read {path}: {e}")))
    }

    /// Emitted when a new image (for `device`, identified by `handle`) was
    /// published at `path`.
    #[zbus(signal)]
    pub async fn image_updated(
        emitter: &SignalEmitter<'_>,
        device: &str,
        handle: &str,
        path: &str,
    ) -> zbus::Result<()>;

    /// Emitted when the published artwork was removed (playback ended,
    /// device disconnected, or the track lost its artwork).
    #[zbus(signal)]
    pub async fn image_cleared(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Source of playback snapshots for the worker (mockable).
#[async_trait::async_trait]
#[expect(
    clippy::double_must_use,
    reason = "async-trait injects #[must_use] on the desugared boxed-future signature"
)]
pub trait PlaybackSource: Send {
    /// Returns the active playback state, `None` when nothing plays.
    ///
    /// # Errors
    /// Propagates transport/D-Bus errors (transient hiccups included).
    async fn status(&self) -> Result<Option<BtPlaybackState>, BtError>;
}

/// Production [`PlaybackSource`] over the real D-Bus `BlueZ` snapshot.
struct DevicePlaybackSource {
    bt: BluetoothDevice,
}

#[async_trait::async_trait]
impl PlaybackSource for DevicePlaybackSource {
    async fn status(&self) -> Result<Option<BtPlaybackState>, BtError> {
        // Must use the async status: the sync wrapper relies on zbus's
        // blocking runtime, which panics when nested inside the provider
        // thread's own tokio runtime.
        Ok(self.bt.status_async().await?.playback)
    }
}

/// Fires the BIP keepalive after this much idle time.
///
/// AOSP idles out the OBEX session after roughly two idle minutes, after which the phone stops
/// advertising `ImgHandle` in `MediaPlayer1.Track` until the publisher restarts, so idle stretches
/// must see traffic to stay alive. The timer resets after every real OBEX transaction (fetch,
/// keepalive), so a track-change GET always preempts a pending keepalive instead of colliding with it.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Fallback poll cadence for the worker loop.
///
/// Track changes are pushed by the `BlueZ` signal listener (see
/// [`spawn_track_signal_listener`]); the timer only recovers from a lost
/// signal stream and drives the keepalive cadence, so it can stay slow —
/// the event path is what keeps track-change latency low without burning
/// CPU on tight polling.
pub const COVER_FALLBACK_POLL: Duration = Duration::from_secs(15);

/// Initial retry interval when a connected peer has not yet advertised an `ImgHandle`.
///
/// Handles initial connection cases where the BIP OBEX session was just established and `BlueZ`
/// or the phone is populating track metadata.
pub const INITIAL_HANDLE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum number of fast retries before settling back to [`COVER_FALLBACK_POLL`].
pub const MAX_INITIAL_HANDLE_RETRIES: u32 = 4;

/// Backoff applied after a failed cover fetch/publish for a handle.
///
/// A failing attempt (unwritable or full cache dir, peer returning truncated
/// JPEGs) would otherwise re-issue a full OBEX image GET every
/// [`COVER_FALLBACK_POLL`]; the backoff bounds that loop while still
/// converging — a later poll past the deadline retries once.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Mirrors AOSP's `COVER_ART_STORAGE_MAX_ITEMS`: the phone keeps at most 32
/// artworks (identified by MD5) per session, so at most 32 distinct handles
/// can recur within one session.
pub const SESSION_CACHE_CAPACITY: usize = 32;

/// Cover-art cache scoped to the lifetime of the active BIP session.
///
/// AOSP's `AvrcpCoverArtStorage` deduplicates artwork by MD5 and re-advertises
/// the same 7-digit handle whenever the same image recurs (same album,
/// replayed tracks) — handles are only unique for the duration of one OBEX
/// session. Caching `(handle -> bytes)` therefore turns repeated handles into
/// instant re-publications with zero OBEX traffic. Entries are purged
/// whenever the session generation ([`BipSessionKeeper::epoch`]) changes.
#[derive(Debug, Default)]
pub struct SessionCoverCache {
    /// Session generation the entries belong to.
    epoch: u64,
    entries: HashMap<Box<str>, Arc<Vec<u8>>>,
}

impl SessionCoverCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops all entries after a session change.
    pub fn purge(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.entries.clear();
    }

    /// Returns the cached bytes for `handle`, if this session produced them.
    #[must_use]
    pub fn lookup(&self, handle: &str) -> Option<Arc<Vec<u8>>> {
        self.entries.get(handle).cloned()
    }

    /// Caches the validated image for `handle`; evicts everything when the
    /// AOSP-sized capacity is reached (handles then restart unpredictably).
    pub fn insert(&mut self, handle: &str, bytes: Arc<Vec<u8>>) {
        if self.entries.len() >= SESSION_CACHE_CAPACITY {
            self.entries.clear();
        }
        self.entries.insert(handle.into(), bytes);
    }

    /// Returns the active session generation.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the number of cached items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The cover-art worker: event-driven fetch loop with a slow fallback poll.
pub struct CoverArtWorker {
    source: Box<dyn PlaybackSource>,
    keeper: Box<dyn BipSessionKeeper>,
    state: SharedCoverState,
    cache_dir: PathBuf,
    /// Woken by the `BlueZ` `Track` signal listener; each permit coalesces the
    /// track changes that arrived since the last poll into one fetch cycle.
    wake: Arc<Notify>,
    emitter: Option<SignalEmitter<'static>>,
    /// When the next idle keepalive fires. Rearmed after every real OBEX
    /// activity (fetch, keepalive) and disarmed when the session is released,
    /// so a track-change signal arriving first preempts the keepalive.
    keepalive_deadline: Option<Instant>,
    /// Instant when the active track's `ImgHandle` was first observed, used to
    /// report the track-change → published latency on the publish log line.
    track_change_at: Option<Instant>,
    /// Handle and retry deadline after a failed fetch/publish, so a broken
    /// cache dir or a peer returning truncated JPEGs cannot degenerate into
    /// a permanent full-image OBEX GET every [`COVER_FALLBACK_POLL`].
    suppress_retry: Option<(Box<str>, Instant)>,
    /// Session-scoped handle cache; purged on every session generation change.
    session_cache: SessionCoverCache,
    /// Last observed (title, artist) to reset retry budget on track changes.
    last_track: Option<(Box<str>, Box<str>)>,
    /// Fast retry counter for initial connection handle resolution.
    missing_handle_retries: u32,
    /// Pending deadline for fast handle retry.
    pending_retry: Option<Instant>,
    /// Shutdown signal receiver.
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

#[cfg(feature = "test-util")]
impl CoverArtWorker {
    /// Returns the instant when the active track change was observed.
    #[must_use]
    pub const fn track_change_at(&self) -> Option<Instant> {
        self.track_change_at
    }

    /// Returns the idle keepalive deadline.
    #[must_use]
    pub const fn keepalive_deadline(&self) -> Option<Instant> {
        self.keepalive_deadline
    }

    /// Sets the idle keepalive deadline for test scenarios.
    pub fn set_keepalive_deadline(&mut self, deadline: Option<Instant>) {
        self.keepalive_deadline = deadline;
    }

    /// Returns `true` if a retry suppression is active.
    #[must_use]
    pub fn has_suppress_retry(&self) -> bool {
        self.suppress_retry.is_some()
    }

    /// Overrides retry suppression for test scenarios.
    pub fn set_suppress_retry(&mut self, val: Option<(Box<str>, Instant)>) {
        self.suppress_retry = val;
    }

    /// Overrides cache dir for test scenarios.
    pub fn set_cache_dir(&mut self, dir: PathBuf) {
        self.cache_dir = dir;
    }

    /// Overrides wake notifier for test scenarios.
    pub fn set_wake(&mut self, wake: Arc<Notify>) {
        self.wake = wake;
    }

    /// Returns a reference to the session cache.
    #[must_use]
    pub const fn session_cache(&self) -> &SessionCoverCache {
        &self.session_cache
    }

    /// Returns a mutable reference to the session cache.
    pub fn session_cache_mut(&mut self) -> &mut SessionCoverCache {
        &mut self.session_cache
    }

    /// Returns the number of missing handle retries.
    #[must_use]
    pub const fn missing_handle_retries(&self) -> u32 {
        self.missing_handle_retries
    }

    /// Returns the pending retry instant.
    #[must_use]
    pub const fn pending_retry(&self) -> Option<Instant> {
        self.pending_retry
    }

    /// Returns the last observed track tuple (title, artist).
    #[must_use]
    pub fn last_track(&self) -> Option<(&str, &str)> {
        self.last_track
            .as_ref()
            .map(|(t, a)| (t.as_ref(), a.as_ref()))
    }
}

impl std::fmt::Debug for CoverArtWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverArtWorker")
            .field("cache_dir", &self.cache_dir)
            .field("keepalive_deadline", &self.keepalive_deadline)
            .field("missing_handle_retries", &self.missing_handle_retries)
            .finish_non_exhaustive()
    }
}

impl CoverArtWorker {
    pub fn new(
        source: Box<dyn PlaybackSource>,
        keeper: Box<dyn BipSessionKeeper>,
        state: SharedCoverState,
        cache_dir: PathBuf,
        wake: Arc<Notify>,
        emitter: Option<SignalEmitter<'static>>,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        Self {
            source,
            keeper,
            state,
            cache_dir,
            wake,
            emitter,
            keepalive_deadline: None,
            track_change_at: None,
            suppress_retry: None,
            session_cache: SessionCoverCache::new(),
            last_track: None,
            missing_handle_retries: 0,
            pending_retry: None,
            shutdown_rx,
        }
    }

    /// Runs one poll cycle: snapshot playback, maintain the BIP session, and
    /// fetch/publish artwork when the track changed.
    pub async fn poll_once(&mut self) {
        match self.source.status().await {
            Ok(Some(playback)) => self.process_playback(playback).await,
            Ok(None) => self.deactivate("playback ended").await,
            Err(e) => {
                // A transient D-Bus hiccup must never flash an empty state
                // over live playback; keep the last known good state.
                tracing::warn!(error = %e, "cover-art playback snapshot failed; keeping state");
            }
        }
    }

    /// Sends the idle keepalive when the deadline expires and rearms the
    /// idle timer.
    ///
    /// The deadline resets after every real OBEX activity (establish, fetch,
    /// keepalive), never on no-op polls, so `SetPath` only goes on the wire
    /// when the session has been genuinely idle for [`KEEPALIVE_INTERVAL`] —
    /// a track-change signal arriving first preempts the pending keepalive
    /// instead of colliding with the next GET.
    pub async fn keepalive(&mut self) {
        if let Err(e) = self.keeper.keepalive().await {
            tracing::warn!(
                error = e.summary(),
                "BIP keepalive failed; session will be re-established on the next ensure"
            );
        }
        self.keepalive_deadline = Some(Instant::now() + KEEPALIVE_INTERVAL);
    }

    /// Clears published artwork and arms a fast retry when a connected peer has
    /// not yet provided an `ImgHandle` (e.g. during initial connection).
    async fn handle_missing_image_handle(&mut self) {
        self.clear_published("track has no ImgHandle").await;
        if self.missing_handle_retries < MAX_INITIAL_HANDLE_RETRIES {
            self.missing_handle_retries = self.missing_handle_retries.saturating_add(1);
            let delay = INITIAL_HANDLE_RETRY_INTERVAL.saturating_mul(self.missing_handle_retries);
            tracing::debug!(
                attempt = self.missing_handle_retries,
                delay_ms = delay.as_millis() as u64,
                "scheduling fast retry for initial ImgHandle"
            );
            self.pending_retry = Some(Instant::now() + delay);
        }
    }

    /// Ensures the active BIP session for `address`, purging session cache on epoch changes
    /// and re-querying playback status if the handle was initially missing.
    async fn ensure_bip_session(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
        playback: &mut BtPlaybackState,
    ) -> Option<u64> {
        let prev_epoch = self.keeper.epoch();
        if let Err(e) = self.keeper.ensure(address, transport).await {
            tracing::warn!(
                error = e.summary(),
                address = %address.to_string(),
                "BIP session ensure failed; retrying next tick"
            );
            return None;
        }
        let epoch = self.keeper.epoch();
        if epoch != self.session_cache.epoch {
            self.session_cache.purge(epoch);
            tracing::debug!(epoch, "cover-art session cache purged after session change");
        }
        if self.keepalive_deadline.is_none() {
            self.keepalive_deadline = Some(Instant::now() + KEEPALIVE_INTERVAL);
        }
        if epoch != prev_epoch && playback.image_handle.is_none() {
            match self.source.status().await {
                Ok(Some(refreshed)) => *playback = refreshed,
                Ok(None) => tracing::debug!("no updated playback during post-session re-query"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "failed to re-query playback status after BIP session establishment"
                ),
            }
        }
        Some(epoch)
    }

    async fn process_playback(&mut self, playback: BtPlaybackState) {
        let Some(address) = playback.device_address else {
            self.deactivate("playback has no device address").await;
            return;
        };
        let current_track = (playback.title.clone(), playback.artist.clone());
        if self.last_track.as_ref() != Some(&current_track) {
            self.last_track = Some(current_track);
            self.missing_handle_retries = 0;
            self.pending_retry = None;
        }

        let transport = classify_obex_port(playback.obex_port);
        let mut playback = playback;
        let Some(epoch) = self
            .ensure_bip_session(&address, &transport, &mut playback)
            .await
        else {
            return;
        };
        let Some(handle) = playback.image_handle else {
            self.handle_missing_image_handle().await;
            return;
        };
        self.missing_handle_retries = 0;
        self.pending_retry = None;
        // Defense in depth: AOSP handles are exactly 7 digits; anything else
        // is peer garbage and must never reach an OBEX GET (the disk write is
        // already guarded by `validate_bip_handle` inside
        // `cover_art_path_for_in`).
        if validate_bip_handle(&handle).is_none() {
            tracing::warn!(
                device = %playback.device_name,
                "rejecting malformed ImgHandle from peer"
            );
            self.clear_published("track has a malformed ImgHandle")
                .await;
            return;
        }
        let device = address.to_string();
        // Dedup on BOTH device and handle: two phones can advertise the same
        // session-scoped handle, and the published state must never keep
        // pointing at the previous device's file.
        let state = self.state.get();
        if state.device() == Some(device.as_str())
            && state.handle() == Some(&*handle)
            && state.epoch() == Some(epoch)
        {
            tracing::trace!(%handle, "cover art already published for this handle");
            return;
        }
        drop(state);
        // Same image re-advertised within this session (same album, replay):
        // republish the cached bytes with zero OBEX traffic. The disk store
        // rewrites the one-file-per-MAC cache; no keepalive rearm, this is
        // not OBEX activity.
        if let Some(bytes) = self.session_cache.lookup(&handle) {
            let started = Instant::now();
            match self.publish_image(&address, &handle, &bytes, epoch).await {
                Ok(path) => {
                    let track_to_cover_ms = self
                        .track_change_at
                        .take()
                        .map(|at| at.elapsed().as_millis() as u64);
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        track_to_cover_ms = track_to_cover_ms.unwrap_or(0),
                        %handle,
                        path = %path,
                        "cover art restored from session cache"
                    );
                    self.suppress_retry = None;
                    return;
                }
                Err(reason) => {
                    tracing::warn!(%handle, "session-cache restore failed: {reason}; fetching fresh");
                }
            }
        }
        // A fresh OBEX GET for a handle whose last attempt just failed is
        // deferred past the backoff deadline; the session-cache restore path
        // above stays active so recovery is instant once bytes exist.
        if let Some((failed, until)) = &self.suppress_retry
            && *failed == handle
            && Instant::now() < *until
        {
            tracing::debug!(
                %handle,
                retry_in_ms = until.saturating_duration_since(Instant::now()).as_millis() as u64,
                "previous fetch attempt failed; backing off before retrying"
            );
            return;
        }
        // First observation of this track's handle starts the latency
        // measurement; retries keep the original detection instant so the
        // reported latency always spans detection → published.
        if self.track_change_at.is_none() {
            self.track_change_at = Some(Instant::now());
        }
        tracing::info!(
            device = %playback.device_name,
            %handle,
            "fetching cover art for new track"
        );
        self.fetch_and_publish(&address, &transport, &handle, epoch)
            .await;
    }

    async fn fetch_and_publish(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
        handle: &str,
        epoch: u64,
    ) {
        let started = Instant::now();
        let result = self
            .keeper
            .fetch_image(address, transport, BipImageType::Native, handle)
            .await;
        match result {
            Ok(bytes) => {
                let byte_count = bytes.len();
                let bytes = Arc::new(bytes);
                let store_started = Instant::now();
                match self.publish_image(address, handle, &bytes, epoch).await {
                    Ok(path) => {
                        let store_ms = store_started.elapsed().as_millis() as u64;
                        let track_to_cover_ms = self
                            .track_change_at
                            .take()
                            .map(|at| at.elapsed().as_millis() as u64);
                        tracing::info!(
                            bytes = byte_count,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            store_ms,
                            track_to_cover_ms = track_to_cover_ms.unwrap_or(0),
                            %handle,
                            path = %path,
                            "cover art published on D-Bus"
                        );
                        self.session_cache.insert(handle, Arc::clone(&bytes));
                        self.suppress_retry = None;
                        // A successful fetch is OBEX traffic; restart the
                        // idle keepalive timer.
                        self.keepalive_deadline = Some(Instant::now() + KEEPALIVE_INTERVAL);
                    }
                    Err(reason) => {
                        self.track_change_at = None;
                        self.suppress_retry = Some((handle.into(), Instant::now() + RETRY_BACKOFF));
                        tracing::warn!(
                            bytes = byte_count,
                            %handle,
                            "discarding fetched cover art: {reason}"
                        );
                    }
                }
            }
            Err(e) => {
                self.suppress_retry = Some((handle.into(), Instant::now() + RETRY_BACKOFF));
                tracing::warn!(
                    error = e.summary(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    %handle,
                    "cover art fetch failed; keeping previous artwork"
                );
            }
        }
    }

    /// Stores the image, publishes it on D-Bus and emits `ImageUpdated`.
    ///
    /// Shared by the fetch path and the session-cache restore path so the
    /// store → publish → signal sequence cannot drift.
    ///
    /// # Errors
    /// Returns the store failure reason; nothing is published on error.
    async fn publish_image(
        &mut self,
        address: &BtAddress,
        handle: &str,
        bytes: &[u8],
        epoch: u64,
    ) -> Result<String, String> {
        // The rename lands the file first, the D-Bus state is published
        // second, and obsolete files for this MAC are cleaned only afterwards
        // — the published path can never point at a just-deleted file.
        let (path, sanitized_mac, sanitized_handle) = self.store_image(*address, handle, bytes)?;
        let device: Box<str> = address.to_string().into_boxed_str();
        self.state.publish(
            device.clone(),
            sanitized_handle.clone().into(),
            path.clone().into(),
            epoch,
        );
        if let Some(emitter) = &self.emitter
            && let Err(e) =
                CoverArtIface::image_updated(emitter, &device, &sanitized_handle, &path).await
        {
            tracing::warn!(error = %e, "failed to emit ImageUpdated");
        }
        clean_old_cover_art_in(&self.cache_dir, &sanitized_mac, &sanitized_handle);
        Ok(path)
    }

    /// Writes the fetched bytes to the cache (tmp file + atomic rename) and
    /// returns the published path, rejecting incomplete JPEGs.
    fn store_image(
        &self,
        address: BtAddress,
        handle: &str,
        bytes: &[u8],
    ) -> Result<(String, String, String), String> {
        let Some((cache_path, sanitized_mac, sanitized_handle)) =
            cover_art_path_for_in(&self.cache_dir, address, handle)
        else {
            return Err("cannot derive cache path".into());
        };
        let tmp_path = format!("{cache_path}.tmp.jpg");
        if let Err(e) = std::fs::write(Path::new(&tmp_path), bytes) {
            return Err(format!("cannot write {tmp_path}: {e}"));
        }
        if !is_complete_jpeg(Path::new(&tmp_path)) {
            let _ = std::fs::remove_file(Path::new(&tmp_path));
            return Err("responder returned an incomplete JPEG".into());
        }
        if let Err(e) = std::fs::rename(Path::new(&tmp_path), Path::new(&cache_path)) {
            let _ = std::fs::remove_file(Path::new(&tmp_path));
            return Err(format!("cannot publish {cache_path}: {e}"));
        }
        Ok((cache_path, sanitized_mac, sanitized_handle))
    }

    /// Tears the session down and clears the published state (emitting
    /// `ImageCleared` exactly once when an image was published).
    async fn deactivate(&mut self, reason: &str) {
        self.keeper.release();
        self.session_cache.purge(self.keeper.epoch());
        self.keepalive_deadline = None;
        self.last_track = None;
        self.missing_handle_retries = 0;
        self.pending_retry = None;
        self.clear_published(reason).await;
    }

    /// Clears the published state and emits `ImageCleared`, keeping the BIP
    /// session and its keepalive timer alive. Used when the track lost its
    /// artwork: dropping the session would make the phone stop advertising
    /// `ImgHandle` for the next track, which costs a full re-establishment.
    async fn clear_published(&mut self, reason: &str) {
        self.track_change_at = None;
        self.suppress_retry = None;
        let previous = self.state.clear();
        if previous.is_active() {
            tracing::info!(reason, "cover art cleared");
            if let Some(emitter) = &self.emitter
                && let Err(e) = CoverArtIface::image_cleared(emitter).await
            {
                tracing::warn!(error = %e, "failed to emit ImageCleared");
            }
        } else {
            tracing::debug!(reason, "cover art stays inactive");
        }
    }

    /// Runs the event-driven fetch loop until the runtime is torn down.
    ///
    /// Each `BlueZ` `Track` signal (or the fallback timer) triggers one poll
    /// cycle; `poll_once` deduplicates unchanged tracks, so a burst of
    /// signals coalesces into a single fetch.
    ///
    /// Takes `&mut self` (not `self`) so the caller can keep ownership and
    /// avoid dropping the worker — which owns the blocking D-Bus connection —
    /// inside a tokio runtime context.
    pub async fn run(&mut self) -> Box<dyn PlaybackSource> {
        tracing::info!(
            fallback_poll_secs = COVER_FALLBACK_POLL.as_secs(),
            "cover-art provider started (signal-driven, periodic fallback poll)"
        );
        let mut shutdown_rx = self.shutdown_rx.clone();
        loop {
            tokio::select! {
                () = tokio::time::sleep(COVER_FALLBACK_POLL) => self.poll_once().await,
                () = self.wake.notified() => self.poll_once().await,
                () = async {
                    if let Some(deadline) = self.pending_retry {
                        tokio::time::sleep_until(deadline.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.pending_retry = None;
                    self.poll_once().await;
                }
                // Idle keepalive: fires only when no signal or fallback poll
                // beat it to the loop, and its deadline is rearm-armed after
                // every real OBEX transaction.
                () = tokio::time::sleep_until(
                    self.keepalive_deadline
                        .unwrap_or_else(|| Instant::now() + KEEPALIVE_INTERVAL)
                        .into(),
                ), if self.keepalive_deadline.is_some() => self.keepalive().await,
                // Graceful shutdown signal.
                _ = shutdown_rx.changed() => {
                    tracing::info!("cover-art provider received shutdown signal");
                    self.deactivate("shutdown requested").await;
                    break;
                },
            }
        }
        // Return the source so it can be dropped outside the runtime context.
        std::mem::replace(&mut self.source, Box::new(EmptyPlaybackSource))
    }
}

/// Empty playback source used as a placeholder after the real source is
/// extracted from the worker during shutdown.
#[derive(Debug, Default)]
pub struct EmptyPlaybackSource;

#[async_trait::async_trait]
impl PlaybackSource for EmptyPlaybackSource {
    async fn status(&self) -> Result<Option<BtPlaybackState>, BtError> {
        Ok(None)
    }
}

/// Builds the D-Bus server connection and returns it with an owned emitter.
async fn build_cover_art_server(
    state: SharedCoverState,
) -> Result<(zbus::Connection, SignalEmitter<'static>), Box<str>> {
    let builder = zbus::connection::Builder::system()
        .map_err(|e| Box::<str>::from(format!("cannot open system D-Bus: {e}")))?;
    build_cover_art_server_on(builder, state).await
}

/// Publishes `CoverArtIface` on a caller-provided connection builder.
///
/// Split from [`build_cover_art_server`] so integration tests can publish
/// the real service on a private bus (the system bus is only reachable in
/// production).
///
/// # Errors
/// Returns the failure reason when the name registration, object serving,
/// connection build or emitter creation fails.
pub async fn build_cover_art_server_on(
    builder: zbus::connection::Builder<'static>,
    state: SharedCoverState,
) -> Result<(zbus::Connection, SignalEmitter<'static>), Box<str>> {
    let connection = builder
        .name(COVER_ART_DBUS_NAME)
        .map_err(|e| Box::<str>::from(format!("invalid bus name {COVER_ART_DBUS_NAME}: {e}")))?
        .serve_at(COVER_ART_OBJECT_PATH, CoverArtIface::new(state))
        .map_err(|e| Box::<str>::from(format!("cannot serve {COVER_ART_OBJECT_PATH}: {e}")))?
        .build()
        .await
        .map_err(|e| Box::<str>::from(format!("cannot start cover-art D-Bus service: {e}")))?;
    let emitter = SignalEmitter::new(&connection, COVER_ART_OBJECT_PATH)
        .map_err(|e| Box::<str>::from(format!("cannot create signal emitter: {e}")))?
        .to_owned();
    Ok((connection, emitter))
}

/// Initial backoff before re-subscribing a dead `BlueZ` track signal stream.
const SIGNAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Backoff ceiling for the reconnect loop. A permanently dead bus (e.g.
/// `dbus-daemon` restarted underneath a long-lived service) would otherwise
/// emit a `warn!` every [`SIGNAL_RECONNECT_BACKOFF`] for the process
/// lifetime; the retry period doubles per failure up to this cap.
const SIGNAL_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Subscribes to `BlueZ` `MediaPlayer1` `Track` changes on an existing async
/// connection and wakes the worker for each one.
///
/// Split from [`spawn_track_signal_listener`] so integration tests can
/// inject a private-bus connection; the spawned task lives for the lifetime
/// of `connection` and keeps running while the provider thread's runtime
/// exists. A dead stream (transport error or close, e.g. a `BlueZ` restart)
/// never ends the task: after a backoff the match rule is rebuilt from the
/// same connection, so signal-driven track-change latency recovers by itself
/// instead of degrading permanently to the fallback poll. The retry period
/// doubles per failure up to [`SIGNAL_RECONNECT_MAX_BACKOFF`] and resets on
/// the first received message, so a permanently dead bus cannot degenerate
/// into a warn-every-500 ms log-spam loop.
///
/// Subscribes and listens until the connection dies or the process exits.
pub fn track_signal_listener(connection: zbus::Connection, wake: Arc<Notify>) {
    tokio::spawn(async move {
        let mut failures: u32 = 0;
        let delay = |failures: u32| {
            // 500 ms, 1 s, 2 s, ... capped at the 30 s ceiling.
            SIGNAL_RECONNECT_BACKOFF
                .saturating_mul(1u32.checked_shl(failures.min(7)).unwrap_or(u32::MAX))
                .min(SIGNAL_RECONNECT_MAX_BACKOFF)
        };
        loop {
            // All rule-builder setters are fallible; the closure gives them
            // a `?` context inside the reconnect loop (no task-local `?`).
            let build_rule = || -> Result<_, zbus::Error> {
                Ok(zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .sender("org.bluez")?
                    .interface("org.freedesktop.DBus.Properties")?
                    .member("PropertiesChanged")?
                    .arg0ns("org.bluez.MediaPlayer1")?
                    .build())
            };
            let rule = match build_rule() {
                Ok(rule) => rule,
                Err(e) => {
                    tracing::error!(error = %e, "track signal match rule failed to build");
                    return;
                }
            };
            let mut stream =
                match zbus::MessageStream::for_match_rule(rule, &connection, None).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            retry_in_ms = delay(failures).as_millis() as u64,
                            "track signal subscription failed; reconnecting after backoff"
                        );
                        tokio::time::sleep(delay(failures)).await;
                        failures = failures.saturating_add(1);
                        continue;
                    }
                };
            loop {
                match stream.next().await {
                    Some(Ok(message)) => {
                        failures = 0;
                        // PropertiesChanged body: (interface, changed, invalidated)
                        match message
                            .body()
                            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                        {
                            Ok((_, changed, _)) if changed.contains_key("Track") => {
                                wake.notify_one();
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "ignoring malformed `BlueZ` PropertiesChanged"
                                );
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            retry_in_ms = delay(failures).as_millis() as u64,
                            "track signal stream failed; reconnecting after backoff"
                        );
                        break;
                    }
                    None => {
                        tracing::warn!(
                            retry_in_ms = delay(failures).as_millis() as u64,
                            "track signal stream closed; reconnecting after backoff"
                        );
                        break;
                    }
                }
            }
            tokio::time::sleep(delay(failures)).await;
            failures = failures.saturating_add(1);
        }
    });
}

/// Subscribes to `BlueZ` `MediaPlayer1` `Track` changes on the system bus.
///
/// # Errors
/// Propagates `zbus::Error` when the system bus connection or the match-rule
/// subscription cannot be established; callers log and continue on the
/// periodic fallback poll.
async fn spawn_track_signal_listener(wake: Arc<Notify>) {
    match zbus::Connection::system().await {
        Ok(connection) => track_signal_listener(connection, wake),
        Err(e) => tracing::warn!(
            error = %e,
            "system bus connection failed; relying on periodic polling only"
        ),
    }
}

/// Spawns the production cover-art provider on a dedicated thread.
///
/// The thread owns its own D-Bus connection and tokio runtime so a stall in
/// the provider can never block the monitor loop.
///
/// # Errors
/// Returns an error string when the D-Bus connection or the worker thread
/// cannot be created. The monitor loop keeps running either way (see
/// [`crate::cli::bt_manager::run_app`]).
/// Shutdown handle for the cover-art worker.
#[derive(Debug, Clone)]
pub struct CoverArtShutdown {
    tx: tokio::sync::watch::Sender<bool>,
    wake: Arc<Notify>,
}

impl CoverArtShutdown {
    /// Creates a new [`CoverArtShutdown`] handle.
    #[must_use]
    pub fn new(tx: tokio::sync::watch::Sender<bool>, wake: Arc<Notify>) -> Self {
        Self { tx, wake }
    }
    /// Triggers graceful shutdown of the cover-art worker.
    pub fn shutdown(&self) {
        let _ = self.tx.send(true);
    }

    /// Wakes the cover-art worker to execute a poll cycle immediately.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Spawns the cover-art provider in a background thread and returns a
/// shutdown handle that can be used to trigger graceful termination.
///
/// The worker runs its own tokio runtime and D-Bus connection, and
/// publishes cover art on the system bus under `org.speaker.CoverArt`.
///
/// # Errors
/// Returns an error string if the background thread cannot be spawned.
pub fn spawn_production(
    cache_dir: PathBuf,
    bt: &BluetoothDevice,
) -> Result<CoverArtShutdown, String> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let wake = Arc::new(Notify::new());
    let shutdown_handle = CoverArtShutdown {
        tx: shutdown_tx,
        wake: Arc::clone(&wake),
    };

    let source = DevicePlaybackSource { bt: bt.clone() };
    std::thread::Builder::new()
        .name("cover-art-provider".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    tracing::error!(error = %e, "cover-art provider runtime failed to start");
                    return;
                }
            };
            // The source owns a zbus blocking connection whose `Drop` uses
            // zbus's own blocking runtime. Dropping it inside a tokio runtime
            // context panics ("Cannot start a runtime from within a
            // runtime"), so every path below hands the source back out of
            // `block_on`; it is dropped only after the runtime context ends.
            let source = runtime.block_on(async move {
                if let Err(e) = ensure_cover_art_cache_dir_in(&cache_dir) {
                    tracing::error!(error = %e, "cover-art cache directory unavailable");
                    return Some(source);
                }
                let state = SharedCoverState::new();
                let (connection, emitter) = match build_cover_art_server(state.clone()).await {
                    Ok(server) => server,
                    Err(e) => {
                        tracing::error!(error = %e, "cover-art D-Bus service failed to start");
                        return Some(source);
                    }
                };
                tracing::info!(
                    bus = COVER_ART_DBUS_NAME,
                    object = COVER_ART_OBJECT_PATH,
                    "cover-art D-Bus service published"
                );
                spawn_track_signal_listener(Arc::clone(&wake)).await;
                let mut worker = CoverArtWorker::new(
                    Box::new(source),
                    Box::new(PersistentBipSession::default()),
                    state,
                    cache_dir,
                    wake,
                    Some(emitter),
                    shutdown_rx,
                );
                let _source = worker.run().await;
                drop(connection);
                None
            });
            drop(source);
        })
        .map_err(|e| format!("cannot spawn cover-art provider thread: {e}"))?;

    Ok(shutdown_handle)
}
