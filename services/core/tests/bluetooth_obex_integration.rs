//! End-to-end OBEX cover-art tests against a private `dbus-daemon` hosting a fake
//! `org.bluez` / `org.bluez.obex` service, exercising the real D-Bus code paths
//! (`ensure_obex_session`, `fetch_cover_art`, session liveness) without hardware.

#![expect(
    clippy::used_underscore_binding,
    clippy::needless_pass_by_value,
    clippy::struct_excessive_bools,
    reason = "zbus interface macros dictate by-value, underscore-named D-Bus method arguments; the fake service mirrors BlueZ D-Bus signatures and independent behaviour flags"
)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use speaker_core::hardware::BluetoothDevice;
use zbus::fdo;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Tunable behaviour of the fake `BlueZ`/OBEX service.
struct Behavior {
    mac_path: String,
    handle: String,
    create_ok: bool,
    session_alive: bool,
    thumbnail_ok: bool,
    get_ok: bool,
    write_file: bool,
    status: String,
    create_calls: AtomicUsize,
}

type Shared = Arc<Mutex<Behavior>>;

fn jpeg_bytes() -> Vec<u8> {
    let mut data = vec![0xFF, 0xD8];
    data.extend(std::iter::repeat_n(0u8, 200));
    data.extend([0xFF, 0xD9]);
    data
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
        _props: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        let ok = self.0.lock().unwrap().get_ok;
        self.transfer(&file, ok)
    }
}

struct Transfer(Shared);

#[zbus::interface(name = "org.bluez.obex.Transfer1")]
impl Transfer {
    #[zbus(property)]
    fn status(&self) -> String {
        self.0.lock().unwrap().status.clone()
    }
}

/// Private bus daemon plus fake service; killed on drop.
struct Harness {
    daemon: Child,
    _server: zbus::blocking::Connection,
    device: BluetoothDevice,
    state: Shared,
    mac_prefix: String,
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
            thumbnail_ok: true,
            get_ok: true,
            write_file: true,
            status: "complete".into(),
            create_calls: AtomicUsize::new(0),
        };
        behavior(&mut b);
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
        let harness = Self {
            daemon,
            _server: server,
            device: BluetoothDevice::with_connection(client),
            state,
            mac_prefix: format!("speaker-cover-{mac_us}-"),
        };
        harness.remove_cover_files();
        harness
    }

    fn remove_cover_files(&self) {
        for e in std::fs::read_dir("/tmp").unwrap().flatten() {
            if e.file_name()
                .to_string_lossy()
                .starts_with(&self.mac_prefix)
            {
                let _ = std::fs::remove_file(e.path());
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
    });
    assert!(h.cover().is_none());
    assert!(h.cover().is_none());
    assert_eq!(h.create_calls(), 2, "failed fetch must drop cached session");
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
