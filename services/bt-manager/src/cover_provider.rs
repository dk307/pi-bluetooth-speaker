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
    fn is_active(&self) -> bool {
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

/// Fires the BIP keepalive after this much idle time. AOSP idles out the
/// OBEX session after roughly two idle minutes, after which the phone stops
/// advertising `ImgHandle` in `MediaPlayer1.Track` until the publisher
/// restarts, so idle stretches must see traffic to stay alive. The timer
/// resets after every real OBEX transaction (fetch, keepalive), so a
/// track-change GET always preempts a pending keepalive instead of
/// colliding with it.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Fallback poll cadence for the worker loop.
///
/// Track changes are pushed by the `BlueZ` signal listener (see
/// [`spawn_track_signal_listener`]); the timer only recovers from a lost
/// signal stream and drives the keepalive cadence, so it can stay slow —
/// the event path is what keeps track-change latency low without burning
/// CPU on tight polling.
const COVER_FALLBACK_POLL: Duration = Duration::from_secs(15);

/// Backoff applied after a failed cover fetch/publish for a handle.
///
/// A failing attempt (unwritable or full cache dir, peer returning truncated
/// JPEGs) would otherwise re-issue a full OBEX image GET every
/// [`COVER_FALLBACK_POLL`]; the backoff bounds that loop while still
/// converging — a later poll past the deadline retries once.
const RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Mirrors AOSP's `COVER_ART_STORAGE_MAX_ITEMS`: the phone keeps at most 32
/// artworks (identified by MD5) per session, so at most 32 distinct handles
/// can recur within one session.
const SESSION_CACHE_CAPACITY: usize = 32;

/// Cover-art cache scoped to the lifetime of the active BIP session.
///
/// AOSP's `AvrcpCoverArtStorage` deduplicates artwork by MD5 and re-advertises
/// the same 7-digit handle whenever the same image recurs (same album,
/// replayed tracks) — handles are only unique for the duration of one OBEX
/// session. Caching `(handle -> bytes)` therefore turns repeated handles into
/// instant re-publications with zero OBEX traffic. Entries are purged
/// whenever the session generation ([`BipSessionKeeper::epoch`]) changes.
#[derive(Debug, Default)]
struct SessionCoverCache {
    /// Session generation the entries belong to.
    epoch: u64,
    entries: HashMap<Box<str>, Arc<Vec<u8>>>,
}

impl SessionCoverCache {
    fn new() -> Self {
        Self::default()
    }

    /// Drops all entries after a session change.
    fn purge(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.entries.clear();
    }

    /// Returns the cached bytes for `handle`, if this session produced them.
    fn lookup(&self, handle: &str) -> Option<Arc<Vec<u8>>> {
        self.entries.get(handle).cloned()
    }

    /// Caches the validated image for `handle`; evicts everything when the
    /// AOSP-sized capacity is reached (handles then restart unpredictably).
    fn insert(&mut self, handle: &str, bytes: Arc<Vec<u8>>) {
        if self.entries.len() >= SESSION_CACHE_CAPACITY {
            self.entries.clear();
        }
        self.entries.insert(handle.into(), bytes);
    }
}

/// The cover-art worker: event-driven fetch loop with a slow fallback poll.
struct CoverArtWorker {
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
    /// Shutdown signal receiver.
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

impl CoverArtWorker {
    fn new(
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
            shutdown_rx,
        }
    }

    /// Runs one poll cycle: snapshot playback, maintain the BIP session, and
    /// fetch/publish artwork when the track changed.
    async fn poll_once(&mut self) {
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
    async fn keepalive(&mut self) {
        if let Err(e) = self.keeper.keepalive().await {
            tracing::warn!(
                error = e.summary(),
                "BIP keepalive failed; session will be re-established on the next ensure"
            );
        }
        self.keepalive_deadline = Some(Instant::now() + KEEPALIVE_INTERVAL);
    }

    async fn process_playback(&mut self, playback: BtPlaybackState) {
        let Some(address) = playback.device_address else {
            self.deactivate("playback has no device address").await;
            return;
        };
        // No ObexPort: fall back to SDP discovery of the BIP RFCOMM channel,
        // mirroring obexd, which queries SDP when the phone publishes no PSM.
        let transport = classify_obex_port(playback.obex_port);
        if let Err(e) = self.keeper.ensure(&address, &transport).await {
            tracing::warn!(
                error = e.summary(),
                address = %address.to_string(),
                "BIP session ensure failed; retrying next tick"
            );
            return;
        }
        // A session change invalidates every cached handle (handles are only
        // unique within one OBEX session, per AVRCP ESR10).
        let epoch = self.keeper.epoch();
        if epoch != self.session_cache.epoch {
            self.session_cache.purge(epoch);
            tracing::debug!(epoch, "cover-art session cache purged after session change");
        }
        if self.keepalive_deadline.is_none() {
            // The establish itself is OBEX traffic; start the idle timer.
            self.keepalive_deadline = Some(Instant::now() + KEEPALIVE_INTERVAL);
        }
        let Some(handle) = playback.image_handle else {
            // The track lost its artwork (or never had any): the published
            // state must not keep serving the previous track's JPEG. The
            // BIP session stays alive so the next track's handle advertises
            // without a full re-establishment.
            self.clear_published("track has no ImgHandle").await;
            return;
        };
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
    async fn run(&mut self) -> Box<dyn PlaybackSource> {
        tracing::info!(
            fallback_poll_secs = COVER_FALLBACK_POLL.as_secs(),
            "cover-art provider started (signal-driven, periodic fallback poll)"
        );
        let mut shutdown_rx = self.shutdown_rx.clone();
        loop {
            tokio::select! {
                () = tokio::time::sleep(COVER_FALLBACK_POLL) => self.poll_once().await,
                () = self.wake.notified() => self.poll_once().await,
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
struct EmptyPlaybackSource;

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
}

impl CoverArtShutdown {
    /// Triggers graceful shutdown of the cover-art worker.
    pub fn shutdown(&self) {
        let _ = self.tx.send(true);
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
    let shutdown_handle = CoverArtShutdown { tx: shutdown_tx };

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
                let wake = Arc::new(Notify::new());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bip_direct::BipError;
    use speaker_common::diag::ScopedTempDir;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_DEVICE: &str = "11:22:33:44:55:66";
    /// Minimal valid JPEG for `is_complete_jpeg` (>= 100 bytes): SOI,
    /// filler, EOI.
    const JPEG_BYTES: &[u8] = &make_jpeg(true);
    /// Truncated JPEG: valid SOI and length but missing the EOI marker.
    const TRUNCATED_JPEG: &[u8] = &make_jpeg(false);

    const fn make_jpeg(with_eoi: bool) -> [u8; 120] {
        let mut bytes = [0u8; 120];
        bytes[0] = 0xFF;
        bytes[1] = 0xD8;
        if with_eoi {
            bytes[118] = 0xFF;
            bytes[119] = 0xD9;
        }
        bytes
    }

    fn test_playback(handle: Option<&str>) -> BtPlaybackState {
        BtPlaybackState {
            device_address: Some(BtAddress::parse_str(TEST_DEVICE).expect("valid MAC")),
            device_name: "Pixel 10 Pro XL".into(),
            status: "playing".into(),
            title: "Galliyan (Unplugged)".into(),
            artist: "Shraddha Kapoor".into(),
            album: "Ek Villain".into(),
            position_ms: 0,
            duration_ms: 254_792,
            player_path: Some("/org/bluez/hci0/dev_11_22_33_44_55_66/player0".into()),
            obex_port: Some(4097),
            image_handle: handle.map(Box::from),
            cover_art_path: None,
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "scripted results must carry the Result shape the worker consumes"
    )]
    fn scripted_playback(handle: Option<&str>) -> Result<Option<BtPlaybackState>, BtError> {
        Ok(Some(test_playback(handle)))
    }

    /// Shared counters exercised through the worker's fakes.
    #[derive(Debug, Default)]
    struct Counters {
        fetches: Arc<AtomicUsize>,
        ensures: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
        keepalives: Arc<AtomicUsize>,
    }

    impl Counters {
        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }

        fn ensures(&self) -> usize {
            self.ensures.load(Ordering::SeqCst)
        }

        fn keepalives(&self) -> usize {
            self.keepalives.load(Ordering::SeqCst)
        }

        fn releases(&self) -> usize {
            self.releases.load(Ordering::SeqCst)
        }
    }

    /// OBEX responder always answering GETs with fixed bytes.
    /// Fake [`BipSessionKeeper`] counting ensure/fetch/release calls,
    /// recording the last transport used, and serving a fixed image body.
    struct FakeKeeper {
        body: &'static [u8],
        counters: Arc<Counters>,
        last_transport: Arc<Mutex<Option<BipTransport>>>,
        generation: Arc<AtomicUsize>,
        address: Mutex<Option<BtAddress>>,
        /// Injected ensure failure (error-path coverage).
        ensure_error: bool,
        /// Injected fetch failure (error-path coverage).
        fetch_error: bool,
        /// Injected keepalive failure (error-path coverage).
        keepalive_error: bool,
    }

    #[async_trait::async_trait]
    impl BipSessionKeeper for FakeKeeper {
        async fn ensure(
            &mut self,
            address: &BtAddress,
            transport: &BipTransport,
        ) -> Result<(), BipError> {
            self.counters.ensures.fetch_add(1, Ordering::SeqCst);
            if self.ensure_error {
                return Err(BipError::service_not_found("AA:BB:CC:DD:EE:FF"));
            }
            // Mirror `PersistentBipSession`: a device OR transport switch
            // releases and re-establishes, bumping the session generation.
            let transport_changed = *self
                .last_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                != Some(*transport);
            let held = self
                .address
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let switched = held.as_ref() != Some(address) || transport_changed;
            if switched {
                self.generation.fetch_add(1, Ordering::SeqCst);
                *held = Some(*address);
            }
            *self
                .last_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(*transport);
            Ok(())
        }

        async fn fetch_image(
            &mut self,
            _address: &BtAddress,
            transport: &BipTransport,
            _image_type: BipImageType,
            _handle: &str,
        ) -> Result<Vec<u8>, BipError> {
            self.counters.fetches.fetch_add(1, Ordering::SeqCst);
            if self.fetch_error {
                return Err(BipError::service_not_found("AA:BB:CC:DD:EE:FF"));
            }
            *self
                .last_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(*transport);
            Ok(self.body.to_vec())
        }

        fn epoch(&self) -> u64 {
            self.generation.load(Ordering::SeqCst) as u64
        }

        fn release(&mut self) {
            self.counters.releases.fetch_add(1, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
        }

        async fn keepalive(&mut self) -> Result<(), BipError> {
            self.counters.keepalives.fetch_add(1, Ordering::SeqCst);
            if self.keepalive_error {
                return Err(BipError::service_not_found("AA:BB:CC:DD:EE:FF"));
            }
            Ok(())
        }
    }

    /// Fake [`PlaybackSource`] yielding scripted results.
    struct FakeSource {
        results: Mutex<VecDeque<Result<Option<BtPlaybackState>, BtError>>>,
    }

    #[async_trait::async_trait]
    impl PlaybackSource for FakeSource {
        async fn status(&self) -> Result<Option<BtPlaybackState>, BtError> {
            let mut results = self
                .results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match results.pop_front() {
                Some(result) => result,
                // Exhausted script: behave as "nothing playing".
                None => Ok(None),
            }
        }
    }

    /// Assembles a worker with fake backends and returns the shared handles
    /// needed for assertions.
    struct TestHarness {
        dir: ScopedTempDir,
        state: SharedCoverState,
        counters: Arc<Counters>,
        last_transport: Arc<Mutex<Option<BipTransport>>>,
        epoch: Arc<AtomicUsize>,
        ensure_error: bool,
        fetch_error: bool,
        keepalive_error: bool,
        body: &'static [u8],
        source_results: VecDeque<Result<Option<BtPlaybackState>, BtError>>,
        #[allow(
            clippy::used_underscore_binding,
            reason = "kept alive to prevent watch channel closure"
        )]
        _shutdown_tx: tokio::sync::watch::Sender<bool>,
    }

    impl TestHarness {
        fn new(body: &'static [u8]) -> Self {
            let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
            Self {
                dir: ScopedTempDir::new("cover-provider-test"),
                state: SharedCoverState::new(),
                counters: Arc::new(Counters::default()),
                last_transport: Arc::new(Mutex::new(None)),
                epoch: Arc::new(AtomicUsize::new(0)),
                ensure_error: false,
                fetch_error: false,
                keepalive_error: false,
                body,
                source_results: VecDeque::new(),
                _shutdown_tx: shutdown_tx,
            }
        }

        fn script(&mut self, result: Result<Option<BtPlaybackState>, BtError>) {
            self.source_results.push_back(result);
        }

        fn worker(&mut self, emitter: Option<SignalEmitter<'static>>) -> CoverArtWorker {
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            self._shutdown_tx = shutdown_tx;
            CoverArtWorker::new(
                Box::new(FakeSource {
                    results: Mutex::new(std::mem::take(&mut self.source_results)),
                }),
                Box::new(FakeKeeper {
                    body: self.body,
                    counters: Arc::clone(&self.counters),
                    last_transport: Arc::clone(&self.last_transport),
                    generation: Arc::clone(&self.epoch),
                    address: Mutex::new(None),
                    ensure_error: self.ensure_error,
                    fetch_error: self.fetch_error,
                    keepalive_error: self.keepalive_error,
                }),
                self.state.clone(),
                self.dir.path().to_path_buf(),
                Arc::new(Notify::new()),
                emitter,
                shutdown_rx,
            )
        }

        fn last_transport(&self) -> Option<BipTransport> {
            *self
                .last_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    #[tokio::test]
    async fn poll_once_publishes_image_and_ensures_session() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        let state = harness.state.get();
        assert_eq!(state.device(), Some(TEST_DEVICE));
        assert_eq!(state.handle(), Some("5630701"));
        assert!(
            worker.track_change_at.is_none(),
            "successful publish consumes the track-change latency measurement"
        );
        let published = state.path().map_or_else(String::new, str::to_string);
        assert!(Path::new(&*published).exists(), "cache file exists");
        assert_eq!(
            std::fs::read(Path::new(&*published)).expect("readable"),
            JPEG_BYTES
        );
        assert_eq!(harness.counters.ensures(), 1);
        assert_eq!(harness.counters.fetches(), 1);
        assert_eq!(harness.counters.releases(), 0);
        assert_eq!(harness.last_transport(), Some(BipTransport::L2capPsm(4097)));
    }

    #[tokio::test]
    async fn second_poll_with_same_handle_deduplicates() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            1,
            "same handle must not refetch"
        );
        assert_eq!(harness.counters.ensures(), 2, "session kept alive per tick");
        assert_eq!(harness.counters.releases(), 0);
    }

    #[tokio::test]
    async fn handle_change_refetches_and_cleans_old_file() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630702")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        let first_path = harness
            .state
            .get()
            .path()
            .map_or_else(String::new, str::to_string);
        worker.poll_once().await;
        let state = harness.state.get();
        assert_eq!(state.handle(), Some("5630702"));
        assert_eq!(harness.counters.fetches(), 2);
        assert!(!Path::new(&*first_path).exists(), "old file cleaned");
    }

    #[tokio::test]
    async fn playback_gone_releases_session_and_clears_state() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(Ok(None));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(harness.counters.releases(), 1);
        let state = harness.state.get();
        assert_eq!(state.handle(), None);
        assert_eq!(state.path(), None);
    }

    #[tokio::test]
    async fn transient_status_error_keeps_state() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(Err(BtError::invalid_address(
            TEST_DEVICE,
            "simulated D-Bus hiccup",
        )));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        let state = harness.state.get();
        assert_eq!(state.handle(), Some("5630701"), "state kept");
        assert_eq!(harness.counters.releases(), 0, "no release on hiccup");
    }

    #[tokio::test]
    async fn incomplete_jpeg_is_rejected() {
        let mut harness = TestHarness::new(TRUNCATED_JPEG);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        let state = harness.state.get();
        assert_eq!(state.handle(), None, "incomplete image not published");
        assert_eq!(harness.counters.fetches(), 1);
    }

    #[tokio::test]
    async fn failed_fetch_is_backed_off_until_deadline_expires() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        harness.fetch_error = true;
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 1, "first attempt goes out");
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            1,
            "a failed fetch must not repeat within the backoff window"
        );
        // Simulate the backoff deadline expiring: a later poll retries once.
        let handle: Box<str> = "5630701".into();
        worker.suppress_retry = Some((handle, Instant::now()));
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            2,
            "past the deadline the fetch is retried"
        );
    }

    #[tokio::test]
    async fn store_failure_is_backed_off_until_deadline_expires() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        // Point the cache at a plain file: every write fails.
        let blocker = harness.dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("blocker file");
        worker.cache_dir = blocker;
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 1, "fetch was attempted");
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            1,
            "a failed store must not re-issue the OBEX GET within the backoff"
        );
        assert!(
            worker.suppress_retry.is_some(),
            "store failure arms the retry backoff"
        );
    }

    #[tokio::test]
    async fn missing_img_handle_clears_art_but_keeps_session() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(None));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.state.get().handle(), Some("5630701"));
        worker.poll_once().await;
        assert_eq!(
            harness.state.get().handle(),
            None,
            "a track without ImgHandle must not keep serving the previous track's JPEG"
        );
        assert_eq!(harness.counters.ensures(), 2, "session still ensured");
        assert_eq!(
            harness.counters.releases(),
            0,
            "clearing art must not drop the BIP session"
        );
        assert_eq!(harness.counters.fetches(), 1);
    }

    #[tokio::test]
    async fn no_handle_poll_arms_idle_deadline_without_sending_keepalive() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(None));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            0,
            "polls never send keepalive directly; the idle timer owns it"
        );
        assert!(
            worker.keepalive_deadline.is_some(),
            "successful ensure arms the idle keepalive timer"
        );
        assert_eq!(harness.counters.fetches(), 0, "no handle means no fetch");
        assert_eq!(harness.counters.ensures(), 1);
    }

    #[tokio::test]
    async fn dedup_poll_leaves_pending_idle_deadline_untouched() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 1);
        let armed = worker.keepalive_deadline.expect("armed by first poll");
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            0,
            "dedup poll is not OBEX traffic; keepalive stays pending"
        );
        assert_eq!(
            worker.keepalive_deadline,
            Some(armed),
            "no-op polls must not postpone the pending idle keepalive"
        );
    }

    #[tokio::test]
    async fn run_loop_fires_idle_keepalive_when_deadline_expires() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        let wake = Arc::new(Notify::new());
        let counters = Arc::clone(&harness.counters);
        let mut worker = harness.worker(None);
        worker.wake = Arc::clone(&wake);
        worker.keepalive_deadline = Some(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("backdate fits in Instant range"),
        );
        let task = tokio::spawn(async move { worker.run().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while counters.keepalives() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expired idle deadline must fire the keepalive");
        task.abort();
    }

    #[tokio::test]
    async fn deactivate_disarms_idle_keepalive() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(Ok(None));
        let mut worker = harness.worker(None);
        worker.keepalive_deadline = Some(Instant::now() + KEEPALIVE_INTERVAL);
        worker.poll_once().await;
        assert_eq!(harness.counters.releases(), 1, "session released");
        assert!(
            worker.keepalive_deadline.is_none(),
            "released session must disarm the idle keepalive"
        );
    }

    #[tokio::test]
    async fn missing_obex_port_falls_back_to_sdp_discovery() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        let mut playback = test_playback(Some("5630701"));
        playback.obex_port = None;
        harness.script(Ok(Some(playback)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(
            harness.last_transport(),
            Some(BipTransport::SdpRfcomm),
            "no ObexPort: fall back to SDP discovery like obexd"
        );
        assert_eq!(harness.counters.fetches(), 1);
        assert_eq!(harness.counters.releases(), 0);
        assert_eq!(harness.state.get().handle(), Some("5630701"));
    }

    #[tokio::test]
    async fn ensure_failure_skips_fetch_and_keeps_state() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.ensure_error = true;
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert!(
            harness.counters.fetches() == 0,
            "no fetch without a session"
        );
        assert_eq!(harness.state.get().handle(), None);
        assert_eq!(harness.counters.releases(), 0, "nothing to release");
    }

    #[tokio::test]
    async fn fetch_failure_keeps_previous_artwork() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630702")));
        harness.fetch_error = true;
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        let previous = harness.state.get();
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 2, "retry happened");
        assert_eq!(
            harness.state.get(),
            previous,
            "transient fetch failure must keep the last known good state"
        );
    }

    #[tokio::test]
    async fn store_failure_discards_fetched_image() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        // Point the cache at a plain file: every write fails.
        let blocker = harness.dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("blocker file");
        worker.cache_dir = blocker;
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 1, "fetch was attempted");
        assert_eq!(
            harness.state.get().handle(),
            None,
            "failed store must not publish anything"
        );
    }

    #[tokio::test]
    async fn device_switch_republishes_even_with_same_handle() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut other_phone = test_playback(Some("5630701"));
        other_phone.device_address =
            Some(BtAddress::parse_str("22:22:33:44:55:66").expect("valid MAC"));
        other_phone.device_name = "Other Phone".into();
        harness.script(Ok(Some(other_phone)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            2,
            "device switch changes the session generation: the per-session handle cache must be purged and the artwork re-fetched"
        );
        let state = harness.state.get();
        assert_eq!(state.device(), Some("22:22:33:44:55:66"));
        assert!(
            state
                .path()
                .expect("republished")
                .contains("22_22_33_44_55_66")
        );
    }

    #[tokio::test]
    async fn transport_change_purges_session_cache_and_refetches() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630702")));
        // Same phone re-registers BIP on a different PSM; the first track's
        // handle recurs across the re-registration.
        let mut moved = test_playback(Some("5630701"));
        moved.obex_port = Some(4099);
        harness.script(Ok(Some(moved)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            3,
            "a recurring handle after a transport change must be re-fetched: per-session handles are not comparable across BIP sessions"
        );
        assert_eq!(
            harness.last_transport(),
            Some(BipTransport::L2capPsm(4099)),
            "worker must follow the re-registered transport"
        );
        assert_eq!(
            worker.session_cache.epoch, 2,
            "two session generations (two ensures) must be visible to the cache"
        );
    }

    #[tokio::test]
    async fn session_change_with_colliding_handle_refetches_instead_of_dedup() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        // The phone re-registers BIP on a different PSM while the SAME track
        // keeps playing: the peer re-advertises the same handle value, but
        // handles are only unique within one BIP session.
        let mut moved = test_playback(Some("5630701"));
        moved.obex_port = Some(4099);
        harness.script(Ok(Some(moved)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            1,
            "same-session repeat of the published handle is deduplicated"
        );
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            2,
            "after a session change a colliding handle must be re-fetched, not deduplicated against the stale published state"
        );
        assert_eq!(
            harness.state.get().epoch(),
            Some(2),
            "the republished artwork carries the new session generation"
        );
    }

    #[tokio::test]
    async fn malformed_handle_is_rejected_before_any_fetch() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        let mut evil = test_playback(Some("../../etc/passwd"));
        evil.image_handle = Some("../../x".into());
        harness.script(Ok(Some(evil)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            0,
            "peer garbage handle must never reach OBEX or disk"
        );
        assert_eq!(harness.state.get().handle(), None);
    }

    #[tokio::test]
    async fn run_loop_wakes_on_notify_and_publishes() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let wake = Arc::new(Notify::new());
        let state = harness.state.clone();
        let mut worker = harness.worker(None);
        worker.wake = Arc::clone(&wake);
        let task = tokio::spawn(async move { worker.run().await });
        wake.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.get().path().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("notify must wake the worker into publishing the cover");
        task.abort();
    }

    #[test]
    fn shared_state_publish_and_clear_roundtrip() {
        let state = SharedCoverState::new();
        assert!(!state.get().is_active());
        state.publish(TEST_DEVICE.into(), "1000001".into(), "/tmp/x.jpg".into(), 0);
        let snapshot = state.get();
        assert_eq!(snapshot.device(), Some(TEST_DEVICE));
        assert_eq!(snapshot.handle(), Some("1000001"));
        assert_eq!(snapshot.path(), Some("/tmp/x.jpg"));
        let cleared = state.clear();
        assert!(cleared.is_active());
        assert!(!state.get().is_active());
    }

    #[tokio::test]
    async fn repeated_handle_is_restored_from_session_cache() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630702")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 2);
        let deadline_before = worker.keepalive_deadline;
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            2,
            "re-advertised handle must not re-fetch over OBEX"
        );
        assert_eq!(harness.counters.keepalives(), 0);
        let state = harness.state.get();
        assert_eq!(state.handle(), Some("5630701"));
        let published = state.path().map_or_else(String::new, str::to_string);
        assert!(Path::new(&*published).exists(), "cache-hit file re-stored");
        assert_eq!(
            worker.keepalive_deadline, deadline_before,
            "cache hit is not OBEX traffic; idle timer untouched"
        );
    }

    #[tokio::test]
    async fn session_change_purges_cached_handles() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630702")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        harness.epoch.fetch_add(1, Ordering::SeqCst);
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            3,
            "purged cache forces a fresh OBEX GET for the recurring handle"
        );
    }

    #[test]
    fn session_cache_evicts_at_aosp_capacity() {
        let mut cache = SessionCoverCache::new();
        for i in 0..=SESSION_CACHE_CAPACITY {
            cache.insert(&format!("{i:07}"), Arc::new(vec![0u8]));
        }
        assert_eq!(
            cache.entries.len(),
            1,
            "capacity 32: insert 33 clears and keeps only the newest"
        );
        cache.purge(7);
        assert_eq!(cache.epoch, 7);
        assert!(cache.lookup("0000000").is_none(), "purge clears entries");
    }
    // --- injected D-Bus emitters (private bus) -----------------------------

    /// Private session bus for emitter tests; killed on drop.
    struct BusGuard(std::process::Child);
    impl Drop for BusGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn spawn_private_bus_sync() -> (BusGuard, String) {
        use std::io::BufRead as _;
        let mut daemon = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be installed for emitter tests");
        let mut address = String::new();
        std::io::BufReader::new(daemon.stdout.take().expect("piped stdout"))
            .read_line(&mut address)
            .expect("read bus address");
        let address = address.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no bus address");
        (BusGuard(daemon), address)
    }

    async fn private_bus_emitter() -> (BusGuard, zbus::Connection, SignalEmitter<'static>) {
        let (guard, address) = spawn_private_bus_sync();
        let connection = zbus::connection::Builder::address(address.as_str())
            .expect("valid bus address")
            .build()
            .await
            .expect("connect to the private bus");
        let emitter = SignalEmitter::new(&connection, COVER_ART_OBJECT_PATH)
            .expect("emitter on the private bus")
            .to_owned();
        (guard, connection, emitter)
    }

    #[tokio::test]
    async fn publish_emits_image_updated_on_the_private_bus() {
        let (_guard, _conn, emitter) = private_bus_emitter().await;
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(Some(emitter));
        worker.poll_once().await;
        assert_eq!(
            harness.state.get().handle(),
            Some("5630701"),
            "published through a live emitter"
        );
    }

    #[tokio::test]
    async fn emit_failure_on_dead_bus_keeps_the_published_state() {
        let (guard, address) = spawn_private_bus_sync();
        let connection = zbus::connection::Builder::address(address.as_str())
            .expect("valid bus address")
            .build()
            .await
            .expect("connect to the private bus");
        let emitter = SignalEmitter::new(&connection, COVER_ART_OBJECT_PATH)
            .expect("emitter on the private bus")
            .to_owned();
        // Kill the bus before publishing: every emit fails (warn arm).
        drop(guard);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(Some(emitter));
        worker.poll_once().await;
        assert_eq!(
            harness.state.get().handle(),
            Some("5630701"),
            "a failed emit must not discard the published artwork"
        );
    }

    // --- FakeKeeper keepalive failure & cache-restore corruption -----------

    #[tokio::test]
    async fn keepalive_failure_warns_and_rearms_deadline() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.keepalive_error = true;
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.counters.keepalives(), 0, "poll must not keepalive");
        worker.keepalive().await;
        assert_eq!(harness.counters.keepalives(), 1, "keepalive attempted");
        assert!(
            worker.keepalive_deadline.is_some(),
            "failed keepalive still rearms the idle timer"
        );
    }

    #[tokio::test]
    async fn corrupt_session_cache_falls_back_to_fresh_fetch() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 1);
        // Poison the cache with invalid JPEG bytes: the restore publish
        // fails (incomplete JPEG) and the worker must fetch fresh.
        worker
            .session_cache
            .entries
            .insert("5630701".into(), Arc::new(TRUNCATED_JPEG.to_vec()));
        // Drop the published state so the dedup check does not short-circuit
        // (the session generation is unchanged, so the cache is not purged).
        drop(harness.state.clear());
        worker.poll_once().await;
        assert_eq!(
            harness.counters.fetches(),
            2,
            "corrupt cached bytes must fall back to a fresh OBEX GET"
        );
        assert_eq!(harness.state.get().handle(), Some("5630701"));
    }

    #[tokio::test]
    async fn store_rename_failure_reports_cache_publish_error() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        // A directory sits where the atomic rename must place the file.
        std::fs::create_dir_all(
            harness
                .dir
                .path()
                .join("speaker-cover-11_22_33_44_55_66-5630701.jpg"),
        )
        .expect("create blocking directory");
        worker.poll_once().await;
        assert_eq!(
            harness.state.get().handle(),
            None,
            "failed rename must not publish anything"
        );
        assert_eq!(harness.counters.fetches(), 1, "fetch was attempted");
    }
    #[tokio::test]
    async fn malformed_handle_clears_published_state() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut evil = test_playback(Some("5630701"));
        evil.image_handle = Some("../etc/passwd".into());
        harness.script(Ok(Some(evil)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.state.get().handle(), Some("5630701"));
        worker.poll_once().await;
        assert_eq!(
            harness.state.get().handle(),
            None,
            "peer garbage handle must not keep the previous track's artwork published"
        );
        assert_eq!(harness.counters.releases(), 0, "BIP session stays held");
        assert_eq!(harness.counters.fetches(), 1, "no fetch for peer garbage");
    }
}
