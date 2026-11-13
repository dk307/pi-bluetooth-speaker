//! Integration tests for the cover-art provider worker loop and session caching.

use bt_manager::bip_direct::{BipError, BipImageType, BipSessionKeeper, BipTransport};
use bt_manager::cover_provider::*;
use speaker_bluetooth::{BtAddress, BtError, BtPlaybackState, COVER_ART_OBJECT_PATH};
use speaker_common::diag::ScopedTempDir;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use zbus::object_server::SignalEmitter;

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
        phone_volume: None,
        avrcp_version: None,
        equalizer: None,
        repeat: None,
        shuffle: None,
        scan: None,
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
        worker.track_change_at().is_none(),
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
    worker.set_suppress_retry(Some((handle, Instant::now())));
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
    worker.set_cache_dir(blocker);
    worker.poll_once().await;
    assert_eq!(harness.counters.fetches(), 1, "fetch was attempted");
    worker.poll_once().await;
    assert_eq!(
        harness.counters.fetches(),
        1,
        "a failed store must not re-issue the OBEX GET within the backoff"
    );
    assert!(
        worker.has_suppress_retry(),
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
        worker.keepalive_deadline().is_some(),
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
    let armed = worker.keepalive_deadline().expect("armed by first poll");
    worker.poll_once().await;
    assert_eq!(
        harness.counters.keepalives(),
        0,
        "dedup poll is not OBEX traffic; keepalive stays pending"
    );
    assert_eq!(
        worker.keepalive_deadline(),
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
    worker.set_wake(Arc::clone(&wake));
    worker.set_keepalive_deadline(Some(
        Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("backdate fits in Instant range"),
    ));
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
    worker.set_keepalive_deadline(Some(Instant::now() + KEEPALIVE_INTERVAL));
    worker.poll_once().await;
    assert_eq!(harness.counters.releases(), 1, "session released");
    assert!(
        worker.keepalive_deadline().is_none(),
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
    worker.set_cache_dir(blocker);
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
        worker.session_cache().epoch(),
        2,
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
    worker.set_wake(Arc::clone(&wake));
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
    let deadline_before = worker.keepalive_deadline();
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
        worker.keepalive_deadline(),
        deadline_before,
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
        cache.len(),
        1,
        "capacity 32: insert 33 clears and keeps only the newest"
    );
    cache.purge(7);
    assert_eq!(cache.epoch(), 7);
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
        worker.keepalive_deadline().is_some(),
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
        .session_cache_mut()
        .insert("5630701", Arc::new(TRUNCATED_JPEG.to_vec()));
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

#[tokio::test]
async fn initial_connection_retries_fast_until_handle_appears() {
    let mut harness = TestHarness::new(JPEG_BYTES);
    // First poll: peer just connected, no ImgHandle yet.
    harness.script(scripted_playback(None));
    // Immediate re-query right after ensure still has no ImgHandle.
    harness.script(scripted_playback(None));
    // Next poll (fast retry): ImgHandle now present.
    harness.script(scripted_playback(Some("7654321")));
    let mut worker = harness.worker(None);

    worker.poll_once().await;
    assert_eq!(harness.state.get().handle(), None);
    assert_eq!(worker.missing_handle_retries(), 1);
    assert!(worker.pending_retry().is_some());

    // Second poll (as if pending_retry fired)
    worker.poll_once().await;
    assert_eq!(harness.state.get().handle(), Some("7654321"));
    assert_eq!(worker.missing_handle_retries(), 0);
    assert!(worker.pending_retry().is_none());
}

#[tokio::test]
async fn initial_connection_re_queries_after_new_bip_session() {
    let mut harness = TestHarness::new(JPEG_BYTES);
    // When process_playback starts, initial snapshot has no ImgHandle.
    // But the new session establishment prompts an immediate re-query,
    // which returns the handle right away!
    harness.script(scripted_playback(None));
    harness.script(scripted_playback(Some("1234567")));
    let mut worker = harness.worker(None);

    worker.poll_once().await;
    assert_eq!(harness.state.get().handle(), Some("1234567"));
    assert_eq!(harness.counters.ensures(), 1);
    assert_eq!(harness.counters.fetches(), 1);
    assert_eq!(worker.missing_handle_retries(), 0);
}

#[tokio::test]
async fn cover_art_shutdown_wake_notifies_listener() {
    let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
    let wake = Arc::new(Notify::new());
    let handle = CoverArtShutdown::new(shutdown_tx, Arc::clone(&wake));
    handle.wake();
    tokio::time::timeout(Duration::from_millis(100), wake.notified())
        .await
        .expect("wake() must notify wake");
}

#[tokio::test]
async fn track_change_resets_missing_handle_retries_budget() {
    let mut harness = TestHarness::new(JPEG_BYTES);
    let mut track1 = test_playback(None);
    track1.title = "Track 1".into();
    let mut track2 = test_playback(None);
    track2.title = "Track 2".into();

    // Script 1 & 2: initial poll plus immediate post-session re-query
    harness.script(Ok(Some(track1.clone())));
    harness.script(Ok(Some(track1.clone())));
    // Script 3: second poll for Track 1
    harness.script(Ok(Some(track1.clone())));
    // Script 4: Track 2 arrives
    harness.script(Ok(Some(track2.clone())));
    let mut worker = harness.worker(None);

    // First poll: initial connection, arms retry attempt 1
    worker.poll_once().await;
    assert_eq!(worker.missing_handle_retries(), 1);

    // Second poll (Track 1 still): retry attempt 2
    worker.poll_once().await;
    assert_eq!(worker.missing_handle_retries(), 2);

    // Third poll (Track 2 arrives): retry budget is reset to 0, then attempt 1 for Track 2
    worker.poll_once().await;
    assert_eq!(worker.missing_handle_retries(), 1);
    assert_eq!(
        worker.last_track(),
        Some(("Track 2", track2.artist.as_ref()))
    );
}
