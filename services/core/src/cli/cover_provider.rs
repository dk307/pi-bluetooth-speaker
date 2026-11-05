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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zbus::object_server::SignalEmitter;

use crate::hardware::bluetooth::bip_direct::{
    BipImageType, BipSessionKeeper, BipTransport, PersistentBipSession, classify_obex_port,
};
use crate::hardware::bluetooth::cover_service::{COVER_ART_DBUS_NAME, COVER_ART_OBJECT_PATH};
use crate::hardware::bluetooth::{
    clean_old_cover_art_in, cover_art_path_for_in, ensure_cover_art_cache_dir_in, is_complete_jpeg,
};
use crate::hardware::{BluetoothDevice, BtAddress, BtError, BtPlaybackState};

/// Published cover-art state mirrored on D-Bus.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoverState {
    /// Canonical MAC address of the device the artwork belongs to.
    device: Option<Box<str>>,
    /// AOSP BIP image handle of the published image.
    handle: Option<Box<str>>,
    /// Cache file path of the published JPEG.
    path: Option<Box<str>>,
}

impl CoverState {
    /// Whether any image is currently published.
    fn is_active(&self) -> bool {
        self.handle.is_some() && self.path.is_some()
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

    /// Publishes an active cover-art image.
    pub fn publish(&self, device: Box<str>, handle: Box<str>, path: Box<str>) {
        *self.lock() = CoverState {
            device: Some(device),
            handle: Some(handle),
            path: Some(path),
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
            .device
            .map_or_else(String::new, |d| d.to_string())
    }

    /// AOSP BIP image handle of the published artwork (empty when inactive).
    #[zbus(property)]
    fn handle(&self) -> String {
        self.state
            .get()
            .handle
            .map_or_else(String::new, |h| h.to_string())
    }

    /// Cache file path of the published JPEG (empty when inactive).
    #[zbus(property)]
    fn path(&self) -> String {
        self.state
            .get()
            .path
            .map_or_else(String::new, |p| p.to_string())
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
            .path
            .ok_or_else(|| zbus::fdo::Error::Failed("no cover art published".into()))?;
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
#[allow(
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

/// The cover-art polling worker.
/// Sends a BIP keepalive at most this often. AOSP idles out the OBEX
/// session after roughly two idle minutes, after which the phone stops
/// advertising `ImgHandle` in `MediaPlayer1.Track` until the publisher
/// restarts, so idle stretches must see traffic to stay alive.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

struct CoverArtWorker {
    source: Box<dyn PlaybackSource>,
    keeper: Box<dyn BipSessionKeeper>,
    state: SharedCoverState,
    cache_dir: PathBuf,
    poll_interval: Duration,
    emitter: Option<SignalEmitter<'static>>,
    last_keepalive: Option<Instant>,
    /// Instant when the active track's `ImgHandle` was first observed, used to
    /// report the track-change → published latency on the publish log line.
    track_change_at: Option<Instant>,
}

impl CoverArtWorker {
    fn new(
        source: Box<dyn PlaybackSource>,
        keeper: Box<dyn BipSessionKeeper>,
        state: SharedCoverState,
        cache_dir: PathBuf,
        poll_interval: Duration,
        emitter: Option<SignalEmitter<'static>>,
    ) -> Self {
        Self {
            source,
            keeper,
            state,
            cache_dir,
            poll_interval,
            emitter,
            last_keepalive: None,
            track_change_at: None,
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

    /// Sends the periodic OBEX keepalive when the held session has been idle
    /// past [`KEEPALIVE_INTERVAL`].
    ///
    /// Runs on the early-return paths (`no ImgHandle`, `already published`)
    /// so the session stays warm and the phone keeps advertising handles even
    /// when no fetch traffic flows.
    async fn maybe_keepalive(&mut self) {
        let due = match self.last_keepalive {
            Some(last) => last.elapsed() >= KEEPALIVE_INTERVAL,
            None => true,
        };
        if !due {
            return;
        }
        self.last_keepalive = Some(Instant::now());
        if let Err(e) = self.keeper.keepalive().await {
            tracing::warn!(
                error = e.summary(),
                "BIP keepalive failed; session will be re-established on the next ensure"
            );
        }
    }

    async fn process_playback(&mut self, playback: BtPlaybackState) {
        let Some(address) = playback.device_address else {
            self.deactivate("playback has no device address").await;
            return;
        };
        let Some(obex_port) = playback.obex_port else {
            self.deactivate("MediaPlayer1 has no ObexPort").await;
            return;
        };
        let transport = classify_obex_port(Some(obex_port));
        if let Err(e) = self.keeper.ensure(&address, &transport).await {
            tracing::warn!(
                error = e.summary(),
                address = %address.to_string(),
                "BIP session ensure failed; retrying next tick"
            );
            return;
        }
        let Some(handle) = playback.image_handle else {
            tracing::debug!(
                device = %playback.device_name,
                "cover art unavailable: no ImgHandle in Track"
            );
            self.maybe_keepalive().await;
            return;
        };
        if self.state.get().handle.as_ref() == Some(&handle) {
            tracing::trace!(%handle, "cover art already published for this handle");
            self.maybe_keepalive().await;
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
        self.fetch_and_publish(&address, &transport, &handle).await;
    }

    async fn fetch_and_publish(
        &mut self,
        address: &BtAddress,
        transport: &BipTransport,
        handle: &str,
    ) {
        let started = Instant::now();
        let result = self
            .keeper
            .fetch_image(address, transport, BipImageType::Native, handle)
            .await;
        match result {
            Ok(bytes) => match self.store_image(*address, handle, &bytes) {
                Ok(path) => {
                    let device = address.to_string().into_boxed_str();
                    self.state
                        .publish(device.clone(), handle.into(), path.clone().into());
                    if let Some(emitter) = &self.emitter {
                        if let Err(e) =
                            CoverArtIface::image_updated(emitter, &device, handle, &path).await
                        {
                            tracing::warn!(error = %e, "failed to emit ImageUpdated");
                        }
                    }
                    let track_to_cover_ms = self
                        .track_change_at
                        .take()
                        .map(|at| at.elapsed().as_millis() as u64);
                    tracing::info!(
                        bytes = bytes.len(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        track_to_cover_ms = track_to_cover_ms.unwrap_or(0),
                        %handle,
                        path = %path,
                        "cover art published on D-Bus"
                    );
                    // A successful fetch is OBEX traffic; the session is
                    // warm again, so restart the keepalive cadence.
                    self.last_keepalive = Some(Instant::now());
                }
                Err(reason) => {
                    self.track_change_at = None;
                    tracing::warn!(
                        bytes = bytes.len(),
                        %handle,
                        "discarding fetched cover art: {reason}"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    error = e.summary(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    %handle,
                    "cover art fetch failed; keeping previous artwork"
                );
            }
        }
    }

    /// Writes the fetched bytes to the cache (tmp file + atomic rename) and
    /// returns the published path, rejecting incomplete JPEGs.
    fn store_image(
        &self,
        address: BtAddress,
        handle: &str,
        bytes: &[u8],
    ) -> Result<String, String> {
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
        clean_old_cover_art_in(&self.cache_dir, &sanitized_mac, &sanitized_handle);
        Ok(cache_path)
    }

    /// Tears the session down and clears the published state (emitting
    /// `ImageCleared` exactly once when an image was published).
    async fn deactivate(&mut self, reason: &str) {
        self.keeper.release();
        self.last_keepalive = None;
        let previous = self.state.clear();
        if previous.is_active() {
            tracing::info!(reason, "cover art deactivated");
            if let Some(emitter) = &self.emitter {
                if let Err(e) = CoverArtIface::image_cleared(emitter).await {
                    tracing::warn!(error = %e, "failed to emit ImageCleared");
                }
            }
        } else {
            tracing::debug!(reason, "cover art stays inactive");
        }
    }

    /// Runs the polling loop until the runtime is torn down.
    ///
    /// Takes `&mut self` (not `self`) so the caller can keep ownership and
    /// avoid dropping the worker — which owns the blocking D-Bus connection —
    /// inside a tokio runtime context.
    async fn run(&mut self) {
        tracing::info!(
            interval_secs = self.poll_interval.as_secs(),
            "cover-art provider polling started"
        );
        loop {
            self.poll_once().await;
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

/// Builds the D-Bus server connection and returns it with an owned emitter.
async fn build_cover_art_server(
    state: SharedCoverState,
) -> Result<(zbus::Connection, SignalEmitter<'static>), Box<str>> {
    let connection = zbus::connection::Builder::system()
        .map_err(|e| Box::<str>::from(format!("cannot open system D-Bus: {e}")))?
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

/// Spawns the production cover-art provider on a dedicated thread.
///
/// The thread owns its own D-Bus connection and tokio runtime so a stall in
/// the provider can never block the monitor loop.
///
/// # Errors
/// Returns an error string when the D-Bus connection or the worker thread
/// cannot be created. The monitor loop keeps running either way (see
/// [`crate::cli::bt_manager::run_app`]).
pub fn spawn_production(cache_dir: PathBuf, poll_interval: Duration) -> Result<(), String> {
    let bt =
        BluetoothDevice::new().map_err(|e| format!("cannot connect to the system D-Bus: {e}"))?;
    let source = DevicePlaybackSource { bt };
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
                let mut worker = CoverArtWorker::new(
                    Box::new(source),
                    Box::new(PersistentBipSession::default()),
                    state,
                    cache_dir,
                    poll_interval,
                    Some(emitter),
                );
                worker.run().await;
                // `run` only returns on shutdown; the worker owns the source
                // (and its blocking connection), which must not be dropped
                // inside the runtime context, so leak it intentionally — the
                // process is exiting anyway.
                std::mem::forget(worker);
                drop(connection);
                None
            });
            drop(source);
        })
        .map_err(|e| format!("cannot spawn cover-art provider thread: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::ScopedTempDir;
    use crate::hardware::bluetooth::bip_direct::BipError;
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

    #[allow(
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
    }

    #[async_trait::async_trait]
    impl BipSessionKeeper for FakeKeeper {
        async fn ensure(
            &mut self,
            _address: &BtAddress,
            transport: &BipTransport,
        ) -> Result<(), BipError> {
            self.counters.ensures.fetch_add(1, Ordering::SeqCst);
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
            *self
                .last_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(*transport);
            Ok(self.body.to_vec())
        }

        fn release(&mut self) {
            self.counters.releases.fetch_add(1, Ordering::SeqCst);
        }

        async fn keepalive(&mut self) -> Result<(), BipError> {
            self.counters.keepalives.fetch_add(1, Ordering::SeqCst);
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
        body: &'static [u8],
        source_results: VecDeque<Result<Option<BtPlaybackState>, BtError>>,
    }

    impl TestHarness {
        fn new(body: &'static [u8]) -> Self {
            Self {
                dir: ScopedTempDir::new("cover-provider-test"),
                state: SharedCoverState::new(),
                counters: Arc::new(Counters::default()),
                last_transport: Arc::new(Mutex::new(None)),
                body,
                source_results: VecDeque::new(),
            }
        }

        fn script(&mut self, result: Result<Option<BtPlaybackState>, BtError>) {
            self.source_results.push_back(result);
        }

        fn worker(&mut self, emitter: Option<SignalEmitter<'static>>) -> CoverArtWorker {
            CoverArtWorker::new(
                Box::new(FakeSource {
                    results: Mutex::new(std::mem::take(&mut self.source_results)),
                }),
                Box::new(FakeKeeper {
                    body: self.body,
                    counters: Arc::clone(&self.counters),
                    last_transport: Arc::clone(&self.last_transport),
                }),
                self.state.clone(),
                self.dir.path().to_path_buf(),
                Duration::from_secs(5),
                emitter,
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
        assert_eq!(state.device.as_deref(), Some(TEST_DEVICE));
        assert_eq!(state.handle.as_deref(), Some("5630701"));
        assert!(
            worker.track_change_at.is_none(),
            "successful publish consumes the track-change latency measurement"
        );
        let published = state.path.expect("published path");
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
        let first_path = harness.state.get().path.expect("first path");
        worker.poll_once().await;
        let state = harness.state.get();
        assert_eq!(state.handle.as_deref(), Some("5630702"));
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
        assert_eq!(state.handle, None);
        assert_eq!(state.path, None);
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
        assert_eq!(state.handle.as_deref(), Some("5630701"), "state kept");
        assert_eq!(harness.counters.releases(), 0, "no release on hiccup");
    }

    #[tokio::test]
    async fn incomplete_jpeg_is_rejected() {
        let mut harness = TestHarness::new(TRUNCATED_JPEG);
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        let state = harness.state.get();
        assert_eq!(state.handle, None, "incomplete image not published");
        assert_eq!(harness.counters.fetches(), 1);
    }

    #[tokio::test]
    async fn missing_img_handle_keeps_session_and_state() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(None));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.counters.ensures(), 1, "session still ensured");
        assert_eq!(harness.counters.fetches(), 0);
        assert_eq!(harness.state.get().handle, None);
        assert_eq!(harness.counters.releases(), 0);
    }

    #[tokio::test]
    async fn no_handle_poll_sends_keepalive_after_interval() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(None));
        harness.script(scripted_playback(None));
        harness.script(scripted_playback(None));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            1,
            "first idle poll sends a keepalive"
        );
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            1,
            "keepalive is throttled within the interval"
        );
        worker.last_keepalive = Some(
            Instant::now()
                .checked_sub(KEEPALIVE_INTERVAL + Duration::from_secs(1))
                .expect("backdate fits in Instant range"),
        );
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            2,
            "keepalive fires again past the interval"
        );
        assert_eq!(harness.counters.fetches(), 0, "no handle means no fetch");
        assert_eq!(
            harness.counters.ensures(),
            3,
            "session ensured on every poll"
        );
    }

    #[tokio::test]
    async fn dedup_poll_sends_keepalive_only_when_interval_elapsed() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        harness.script(scripted_playback(Some("5630701")));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        assert_eq!(harness.counters.fetches(), 1);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            0,
            "fresh fetch traffic keeps the session warm; no keepalive due"
        );
        worker.last_keepalive = Some(
            Instant::now()
                .checked_sub(KEEPALIVE_INTERVAL + Duration::from_secs(1))
                .expect("backdate fits in Instant range"),
        );
        worker.poll_once().await;
        assert_eq!(
            harness.counters.keepalives(),
            1,
            "stale interval triggers keepalive on the dedup path"
        );
        assert_eq!(
            harness.counters.fetches(),
            1,
            "same handle still not refetched"
        );
    }

    #[tokio::test]
    async fn missing_obex_port_deactivates() {
        let mut harness = TestHarness::new(JPEG_BYTES);
        harness.script(scripted_playback(Some("5630701")));
        let mut playback = test_playback(Some("5630701"));
        playback.obex_port = None;
        harness.script(Ok(Some(playback)));
        let mut worker = harness.worker(None);
        worker.poll_once().await;
        worker.poll_once().await;
        assert_eq!(harness.counters.releases(), 1);
        assert_eq!(harness.state.get().handle, None);
    }

    #[test]
    fn shared_state_publish_and_clear_roundtrip() {
        let state = SharedCoverState::new();
        assert!(!state.get().is_active());
        state.publish(TEST_DEVICE.into(), "1000001".into(), "/tmp/x.jpg".into());
        let snapshot = state.get();
        assert_eq!(snapshot.device.as_deref(), Some(TEST_DEVICE));
        assert_eq!(snapshot.handle.as_deref(), Some("1000001"));
        assert_eq!(snapshot.path.as_deref(), Some("/tmp/x.jpg"));
        let cleared = state.clear();
        assert!(cleared.is_active());
        assert!(!state.get().is_active());
    }
}
