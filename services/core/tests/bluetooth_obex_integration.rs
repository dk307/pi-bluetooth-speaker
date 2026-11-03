//! End-to-end OBEX cover-art tests against a private `dbus-daemon` hosting a fake
//! `org.bluez` / `org.bluez.obex` service, exercising the real D-Bus code paths
//! (`ensure_obex_session`, `fetch_cover_art`, session liveness) without hardware.

#![expect(
    clippy::used_underscore_binding,
    clippy::needless_pass_by_value,
    clippy::struct_excessive_bools,
    clippy::unused_self,
    reason = "zbus interface macros dictate by-value, underscore-named D-Bus method arguments; the fake service mirrors BlueZ D-Bus signatures and independent behaviour flags"
)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use speaker_core::diag::ScopedTempDir;
use speaker_core::hardware::BluetoothDevice;
use zbus::fdo;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Tunable behaviour of the fake `BlueZ`/OBEX service.
struct Behavior {
    mac_path: String,
    handle: String,
    create_ok: bool,
    session_alive: bool,
    properties_ok: bool,
    descriptors: Vec<HashMap<String, OwnedValue>>,
    thumbnail_ok: bool,
    get_ok: bool,
    /// Accepts zero-descriptor `Get` (Strategy A native pull) requests.
    native_get_ok: bool,
    write_file: bool,
    status: String,
    /// Value reported by the `Transferred` property of `Transfer1`.
    transferred: u64,
    create_calls: AtomicUsize,
    /// Number of zero-descriptor (native) `Get` calls observed.
    native_get_calls: AtomicUsize,
    last_get_pixel: String,
    device_connected: bool,
    /// Position (milliseconds) reported by the fake `MediaPlayer1`.
    position_ms: u32,
    /// Number of `Seek` calls observed on the fake `MediaPlayer1`.
    seek_calls: AtomicUsize,
    /// Last `Seek` offset (microseconds) seen on the fake `MediaPlayer1`.
    last_seek_offset_us: i64,
}

type Shared = Arc<Mutex<Behavior>>;

fn jpeg_bytes() -> Vec<u8> {
    let mut data = vec![0xFF, 0xD8];
    data.extend(std::iter::repeat_n(0u8, 200));
    data.extend([0xFF, 0xD9]);
    data
}

fn descriptor(kv: &[(&str, Value<'_>)]) -> HashMap<String, OwnedValue> {
    kv.iter()
        .map(|(k, v)| {
            (
                (*k).to_owned(),
                v.try_to_owned().expect("static descriptor value"),
            )
        })
        .collect()
}

/// Mirrors the real-phone descriptors observed over OBEX BIP (200x200 only).
fn baseline_descriptors() -> Vec<HashMap<String, OwnedValue>> {
    vec![
        descriptor(&[
            ("type", Value::from("native")),
            ("encoding", Value::from("JPEG")),
            ("pixel", Value::from("200*200")),
        ]),
        descriptor(&[
            ("type", Value::from("variant")),
            ("encoding", Value::from("PNG")),
            ("pixel", Value::from("200*200")),
            ("maxsize", Value::from(200u64)),
        ]),
    ]
}

/// Open-ended BIP pixel range descriptor (Android-style `0*0-65535*65535`).
fn range_descriptors() -> Vec<HashMap<String, OwnedValue>> {
    vec![descriptor(&[
        ("type", Value::from("native")),
        ("encoding", Value::from("JPEG")),
        ("pixel", Value::from("0*0-65535*65535")),
        ("maxsize", Value::from(250u64)),
    ])]
}

struct ObjectManager(Shared);

#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl ObjectManager {
    fn get_managed_objects(
        &self,
    ) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        let b = self.0.lock().unwrap();
        let mut track: HashMap<&str, Value<'_>> = HashMap::new();
        track.insert("Title", Value::from("Song"));
        track.insert("ImgHandle", Value::from(b.handle.as_str()));
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert("Status".into(), Value::from("playing").try_into().unwrap());
        props.insert(
            "Device".into(),
            Value::from(zbus::zvariant::ObjectPath::try_from(b.mac_path.as_str()).unwrap())
                .try_into()
                .unwrap(),
        );
        props.insert("Track".into(), Value::from(track).try_into().unwrap());
        props.insert(
            "Position".into(),
            Value::from(b.position_ms).try_into().unwrap(),
        );
        props.insert("ObexPort".into(), Value::from(4097u16).try_into().unwrap());
        let mut ifaces = HashMap::new();
        ifaces.insert("org.bluez.MediaPlayer1".to_string(), props);
        let mut out = HashMap::new();
        out.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_x/player0").unwrap(),
            ifaces,
        );
        out
    }
}

struct MediaPlayer(Shared);

#[zbus::interface(name = "org.bluez.MediaPlayer1")]
impl MediaPlayer {
    fn seek(&self, offset: i64) {
        let mut b = self.0.lock().unwrap();
        b.seek_calls.fetch_add(1, Ordering::SeqCst);
        b.last_seek_offset_us = offset;
    }
}

struct Client(Shared);

#[zbus::interface(name = "org.bluez.obex.Client1")]
impl Client {
    fn create_session(
        &self,
        _dest: String,
        _args: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        let mut b = self.0.lock().unwrap();
        b.create_calls.fetch_add(1, Ordering::SeqCst);
        if !b.create_ok {
            return Err(fdo::Error::Failed("create refused".into()));
        }
        b.session_alive = true;
        Ok(OwnedObjectPath::try_from("/org/bluez/obex/client/session0").unwrap())
    }
}

struct Session(Shared);

#[zbus::interface(name = "org.bluez.obex.Session1")]
impl Session {
    #[zbus(property)]
    fn destination(&self) -> fdo::Result<String> {
        if self.0.lock().unwrap().session_alive {
            Ok("AA:BB:CC:DD:EE:FF".into())
        } else {
            Err(fdo::Error::Failed("session gone".into()))
        }
    }
}

struct Image(Shared);

struct Device(Shared);

#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.0.lock().unwrap().device_connected
    }

    fn connect(&self) {}
}

impl Image {
    fn transfer(
        &self,
        file: &str,
        ok: bool,
    ) -> fdo::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        let b = self.0.lock().unwrap();
        if !ok {
            return Err(fdo::Error::Failed("image op failed".into()));
        }
        if b.write_file {
            std::fs::write(file, jpeg_bytes()).unwrap();
        }
        Ok((
            OwnedObjectPath::try_from("/org/bluez/obex/client/session0/transfer0").unwrap(),
            HashMap::new(),
        ))
    }
}

#[zbus::interface(name = "org.bluez.obex.Image1")]
impl Image {
    fn get_thumbnail(
        &self,
        file: String,
        _handle: String,
    ) -> fdo::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        let ok = self.0.lock().unwrap().thumbnail_ok;
        self.transfer(&file, ok)
    }

    fn get(
        &self,
        file: String,
        _handle: String,
        props: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        let mut b = self.0.lock().unwrap();
        if let Some(pixel) = props
            .get("Pixel")
            .and_then(|v| v.downcast_ref::<&str>().ok())
        {
            pixel.clone_into(&mut b.last_get_pixel);
            let ok = b.get_ok;
            drop(b);
            return self.transfer(&file, ok);
        }
        // Empty properties = zero-descriptor native `Get` (Strategy A).
        b.native_get_calls.fetch_add(1, Ordering::SeqCst);
        let ok = b.native_get_ok;
        drop(b);
        self.transfer(&file, ok)
    }

    fn properties(&self, _handle: String) -> fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        let b = self.0.lock().unwrap();
        if !b.properties_ok {
            return Err(fdo::Error::Failed("properties failed".into()));
        }
        Ok(b.descriptors.clone())
    }
}

struct Transfer(Shared);

#[zbus::interface(name = "org.bluez.obex.Transfer1")]
impl Transfer {
    #[zbus(property)]
    fn status(&self) -> String {
        self.0.lock().unwrap().status.clone()
    }

    #[zbus(property)]
    fn transferred(&self) -> u64 {
        self.0.lock().unwrap().transferred
    }

    fn cancel(&self) {}
}

/// Private bus daemon plus fake service; killed on drop.
struct Harness {
    daemon: Child,
    _server: zbus::blocking::Connection,
    device: BluetoothDevice,
    state: Shared,
    mac_prefix: String,
    caps_dir: ScopedTempDir,
    cache_dir: ScopedTempDir,
}

impl Harness {
    fn new(mac: &str, behavior: impl FnOnce(&mut Behavior)) -> Self {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be installed to run OBEX integration tests");
        let mut addr = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut addr)
            .unwrap();
        let addr = addr.trim().to_string();

        let mac_us = mac.replace(':', "_");
        let mut b = Behavior {
            mac_path: format!("/org/bluez/hci0/dev_{mac_us}"),
            handle: format!("h{mac_us}"),
            create_ok: true,
            session_alive: true,
            properties_ok: true,
            descriptors: baseline_descriptors(),
            thumbnail_ok: true,
            get_ok: true,
            native_get_ok: true,
            write_file: true,
            status: "complete".into(),
            transferred: 0,
            create_calls: AtomicUsize::new(0),
            native_get_calls: AtomicUsize::new(0),
            last_get_pixel: String::new(),
            device_connected: true,
            position_ms: 45_000,
            seek_calls: AtomicUsize::new(0),
            last_seek_offset_us: 0,
        };
        behavior(&mut b);
        let mac_path = b.mac_path.clone();
        let state: Shared = Arc::new(Mutex::new(b));

        let server = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .serve_at("/", ObjectManager(Arc::clone(&state)))
            .unwrap()
            .serve_at("/org/bluez/obex", Client(Arc::clone(&state)))
            .unwrap()
            .serve_at(
                "/org/bluez/obex/client/session0",
                Session(Arc::clone(&state)),
            )
            .unwrap()
            .serve_at("/org/bluez/obex/client/session0", Image(Arc::clone(&state)))
            .unwrap()
            .serve_at(
                "/org/bluez/obex/client/session0/transfer0",
                Transfer(Arc::clone(&state)),
            )
            .unwrap()
            .serve_at(mac_path, Device(Arc::clone(&state)))
            .unwrap()
            .serve_at(
                "/org/bluez/hci0/dev_x/player0",
                MediaPlayer(Arc::clone(&state)),
            )
            .unwrap()
            .name("org.bluez")
            .unwrap()
            .name("org.bluez.obex")
            .unwrap()
            .build()
            .unwrap();

        let client = zbus::blocking::connection::Builder::address(addr.as_str())
            .unwrap()
            .build()
            .unwrap();
        let caps_dir = ScopedTempDir::new("obex_caps");
        let cache_dir = ScopedTempDir::new("obex_covers");
        let harness = Self {
            daemon,
            _server: server,
            device: BluetoothDevice::with_connection(client)
                .with_device_caps_dir(caps_dir.path())
                .with_cache_dir(cache_dir.path()),
            state,
            mac_prefix: format!("speaker-cover-{mac_us}-"),
            caps_dir,
            cache_dir,
        };
        harness.remove_cover_files();
        harness
    }

    fn remove_cover_files(&self) {
        // Legacy `/tmp` leftovers are also pruned; active runs write only to the
        // harness-owned cache directory (see `with_cache_dir`).
        for dir in [std::path::Path::new("/tmp"), self.cache_dir.path()] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                if e.file_name()
                    .to_string_lossy()
                    .starts_with(&self.mac_prefix)
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    fn cover(&self) -> Option<Box<str>> {
        self.device
            .playback_state()
            .unwrap()
            .expect("fake player present")
            .cover_art_path
    }

    fn create_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .create_calls
            .load(Ordering::SeqCst)
    }

    fn last_get_pixel(&self) -> String {
        self.state.lock().unwrap().last_get_pixel.clone()
    }

    fn native_get_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .native_get_calls
            .load(Ordering::SeqCst)
    }

    fn seek_calls(&self) -> usize {
        self.state.lock().unwrap().seek_calls.load(Ordering::SeqCst)
    }

    fn last_seek_offset_us(&self) -> i64 {
        self.state.lock().unwrap().last_seek_offset_us
    }

    fn caps_dir_path(&self) -> std::path::PathBuf {
        self.caps_dir.path().to_path_buf()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.remove_cover_files();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[test]
fn cover_art_fetched_cached_and_session_recreated() {
    let h = Harness::new("11:00:00:00:00:01", |_| {});
    let path = h.cover().expect("cover art fetched over OBEX");
    assert!(std::path::Path::new(&*path).exists());
    assert_eq!(h.create_calls(), 1);

    // Cached on disk and live session is reused.
    assert_eq!(h.cover().as_deref(), Some(&*path));
    assert_eq!(h.create_calls(), 1);

    // A dead session is detected and recreated.
    h.state.lock().unwrap().session_alive = false;
    assert_eq!(h.cover().as_deref(), Some(&*path));
    assert_eq!(h.create_calls(), 2);
}

#[test]
fn cover_art_falls_back_to_get_when_thumbnail_fails() {
    let h = Harness::new("11:00:00:00:00:02", |b| b.thumbnail_ok = false);
    assert!(h.cover().is_some());
}

#[test]
fn cover_art_none_when_thumbnail_and_get_fail_and_session_cleared() {
    let h = Harness::new("11:00:00:00:00:03", |b| {
        b.thumbnail_ok = false;
        b.get_ok = false;
        b.native_get_ok = false;
    });
    assert!(h.cover().is_none());
    assert_eq!(h.create_calls(), 1);
    assert!(h.cover().is_none());
    assert_eq!(
        h.create_calls(),
        2,
        "each failed fetch drops the cached session and the next fetch recreates it"
    );
}

#[test]
fn cover_art_none_when_transfer_reports_error() {
    let h = Harness::new("11:00:00:00:00:04", |b| b.status = "error".into());
    assert!(h.cover().is_none());
}

#[test]
fn cover_art_none_when_session_creation_refused() {
    let h = Harness::new("11:00:00:00:00:05", |b| b.create_ok = false);
    assert!(h.cover().is_none());
    assert_eq!(h.create_calls(), 1);
}

#[test]
fn cover_art_none_when_transfer_never_completes() {
    let h = Harness::new("11:00:00:00:00:06", |b| {
        b.status = "active".into();
        b.write_file = false;
    });
    assert!(h.cover().is_none());
}

#[test]
fn cover_art_fetched_via_bip_get_when_range_advertised() {
    let h = Harness::new("11:00:00:00:00:07", |b| {
        b.descriptors = range_descriptors();
        b.native_get_ok = false;
    });
    let path = h.cover().expect("high-res cover fetched via BIP Get");
    assert!(std::path::Path::new(&*path).exists());
    assert_eq!(h.last_get_pixel(), "800*800");
    assert_eq!(h.create_calls(), 1);
}

#[test]
fn cover_art_falls_back_to_thumbnail_when_bip_get_refused() {
    let h = Harness::new("11:00:00:00:00:08", |b| {
        b.descriptors = range_descriptors();
        b.native_get_ok = false;
        b.get_ok = false;
    });
    assert!(h.cover().is_some());
}

#[test]
fn cover_art_fetched_via_native_zero_descriptor_pull() {
    let h = Harness::new("11:00:00:00:00:09", |_| {});
    let path = h.cover().expect("native cover fetched over OBEX");
    assert!(std::path::Path::new(&*path).exists());
    assert_eq!(h.native_get_calls(), 1);
    assert_eq!(
        h.last_get_pixel(),
        "",
        "variant loop must not run after a successful native pull"
    );
    assert_eq!(h.create_calls(), 1);
}

#[test]
fn cover_art_native_pull_skipped_when_advertised_size_exceeds_cap() {
    let h = Harness::new("11:00:00:00:00:10", |b| {
        b.descriptors = vec![descriptor(&[
            ("type", Value::from("native")),
            ("encoding", Value::from("JPEG")),
            ("pixel", Value::from("1000*1000")),
            ("size", Value::from(2_000_000u64)),
        ])];
    });
    let path = h.cover().expect("thumbnail fallback after native skip");
    assert!(std::path::Path::new(&*path).exists());
    assert_eq!(h.native_get_calls(), 0);
}

#[test]
fn native_cap_persisted_after_native_pull_refusal() {
    let h = Harness::new("11:00:00:00:00:12", |b| {
        b.descriptors = vec![descriptor(&[
            ("type", Value::from("native")),
            ("encoding", Value::from("JPEG")),
            ("pixel", Value::from("1000*1000")),
            ("size", Value::from(500_000u64)),
        ])];
        b.native_get_ok = false;
    });
    assert!(h.cover().is_some());
    let caps = speaker_core::hardware::load_native_caps_in(&h.caps_dir_path());
    assert_eq!(caps.get("11_00_00_00_00_12"), Some(&499_999));
}

#[test]
fn player_seek_translates_absolute_ms_to_relative_us() {
    let h = Harness::new("11:00:00:00:00:99", |_| {});
    h.device.player_seek(60_000).expect("seek accepted");
    assert_eq!(h.seek_calls(), 1);
    assert_eq!(h.last_seek_offset_us(), 15_000_000);
}

/// Regression test for the listener-thread leak: dropping the `DbusClient`
/// closes the connection so the blocked `MessageIterator::next` unblocks and
/// the `bt-dbus-signals` thread exits (it cannot observe the stop flag while
/// parked on a quiet bus).
#[test]
fn listener_thread_released_on_client_drop() {
    // Count only threads named like the Bluetooth listener (`bt-dbus-signals*`)
    // instead of the process-global thread count: integration tests in this
    // binary run in parallel and sibling tests' zbus connections spawn/tear
    // down their own executor threads, which would race with a global count
    // (see PR review finding).
    fn listener_count() -> usize {
        std::fs::read_dir("/proc/self/task")
            .expect("read /proc/self/task")
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                std::fs::read_to_string(e.path().join("comm"))
                    .map(|c| c.trim().starts_with("bt-dbus-signals"))
                    .unwrap_or(false)
            })
            .count()
    }

    /// RAII guard: kills the session dbus-daemon on drop, so an assertion
    /// failure (panic) cannot leak the spawned child process.
    struct DaemonGuard(std::process::Child);
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let mut daemon = DaemonGuard(
        Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be installed to run integration tests"),
    );
    let addr = BufReader::new(daemon.0.stdout.take().unwrap())
        .lines()
        .next()
        .unwrap()
        .unwrap();
    let client = zbus::blocking::connection::Builder::address(addr.trim())
        .unwrap()
        .build()
        .unwrap();

    let before = listener_count();
    let device = BluetoothDevice::with_connection(client);
    let with_listener = listener_count();
    assert!(
        with_listener > before,
        "listener thread must be spawned with the client"
    );

    drop(device);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while listener_count() > before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let after = listener_count();
    assert!(
        after <= before,
        "listener thread must exit after the client is dropped (before={before}, after={after})"
    );
}
