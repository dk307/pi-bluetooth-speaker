//! End-to-end tests for the cover-art D-Bus publisher (`CoverArtIface`) served
//! on a private `dbus-daemon`, verifying exactly the contract a future core
//! daemon will consume: empty-state properties, `GetImage` rejection, and a
//! full publish/query/clear lifecycle.
//!
//! Follows the repository pattern from `bluetooth_obex_integration.rs`:
//! private session bus, distinct MAC per test, daemon killed on drop, loud
//! failure when `dbus-daemon` is missing.

use speaker_core::cli::cover_provider::{CoverArtIface, SharedCoverState};
use speaker_core::diag::ScopedTempDir;
use speaker_core::hardware::BtPlaybackState;
use speaker_core::hardware::bluetooth::{BtAddress, resolve_cover_art_with};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use zbus::blocking::Proxy;

/// A minimal but complete JPEG fixture: >= 100 bytes (the `is_complete_jpeg`
/// minimum) and terminated with the EOI marker.
fn jpeg_fixture() -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8];
    bytes.resize(140, 0x00);
    bytes[100..136].copy_from_slice(&[0xAB; 36]);
    bytes.push(0xFF);
    bytes.push(0xD9);
    bytes
}

/// Private bus daemon serving `CoverArtIface`; killed on drop.
struct Harness {
    daemon: Child,
    server: zbus::blocking::Connection,
    state: SharedCoverState,
    proxy: Proxy<'static>,
    dir: ScopedTempDir,
}

impl Harness {
    fn new() -> Self {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be installed to run cover-provider integration tests");
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no bus address");

        let state = SharedCoverState::new();
        let server = zbus::blocking::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.speaker.CoverArt")
            .unwrap()
            .serve_at("/org/speaker/coverart", CoverArtIface::new(state.clone()))
            .unwrap()
            .build()
            .unwrap();

        let client = zbus::blocking::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let proxy = Proxy::new_owned(
            client,
            "org.speaker.CoverArt",
            "/org/speaker/coverart",
            "org.speaker.CoverArt1",
        )
        .unwrap();

        Self {
            daemon,
            server,
            state,
            proxy,
            dir: ScopedTempDir::new("cover_provider_dbus"),
        }
    }

    /// Publishes the fixture image for `mac`/`handle` into the shared state.
    fn publish_fixture(&self, mac: &str, handle: &str) -> Vec<u8> {
        let bytes = jpeg_fixture();
        let path = self.dir.path().join(format!(
            "speaker-cover-{mac_us}-{handle}.jpg",
            mac_us = mac.replace(':', "_")
        ));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        self.state.publish(
            mac.into(),
            handle.into(),
            path.to_string_lossy().into_owned().into(),
        );
        bytes
    }

    /// Reads a property. zbus pascal-cases member names, so the D-Bus
    /// property backing `device` is `Device`.
    fn property(&self, name: &str) -> String {
        let dbus_name = format!("{}{}", name[..1].to_uppercase(), &name[1..]);
        self.proxy.get_property::<String>(&dbus_name).unwrap()
    }

    fn get_image(&self) -> Result<Vec<u8>, zbus::Error> {
        let reply = self.proxy.call_method("GetImage", &())?;
        reply.body().deserialize::<Vec<u8>>()
    }

    fn get_image_error_message(&self) -> String {
        match self.get_image() {
            Ok(bytes) => panic!("GetImage unexpectedly returned {} bytes", bytes.len()),
            Err(zbus::Error::MethodError(_, message, _)) => {
                message.unwrap_or_else(|| "<no message>".into())
            }
            Err(e) => panic!("unexpected GetImage error class: {e}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[test]
fn empty_state_exposes_no_image() {
    let h = Harness::new();

    assert_eq!(h.property("device"), "");
    assert_eq!(h.property("handle"), "");
    assert_eq!(h.property("path"), "");

    let message = h.get_image_error_message();
    assert!(
        message.contains("no cover art published"),
        "unexpected rejection message: {message}"
    );
}

#[test]
fn published_image_is_queryable_over_dbus() {
    let h = Harness::new();
    let expected = h.publish_fixture("AA:BB:CC:DD:EE:F1", "5630701");

    assert_eq!(h.property("device"), "AA:BB:CC:DD:EE:F1");
    assert_eq!(h.property("handle"), "5630701");
    assert!(
        h.property("path")
            .ends_with("speaker-cover-AA_BB_CC_DD_EE_F1-5630701.jpg"),
        "unexpected path property {}",
        h.property("path")
    );

    assert_eq!(h.get_image().unwrap(), expected);
}

#[test]
fn cleared_state_rejects_get_image_again() {
    let h = Harness::new();
    h.publish_fixture("AA:BB:CC:DD:EE:F2", "5630702");
    let _cleared = h.state.clear();

    assert_eq!(h.property("device"), "");
    assert_eq!(h.property("handle"), "");
    assert_eq!(h.property("path"), "");
    assert!(
        h.get_image_error_message()
            .contains("no cover art published"),
        "cleared state must reject GetImage"
    );
}

/// Kills the private `dbus-daemon` when the test ends.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `Track` `PropertiesChanged` on `org.bluez.MediaPlayer1` must wake the
/// worker; other property changes must not. Exercises the real match-rule
/// subscription on a private `dbus-daemon` (the emitter owns `org.bluez` so
/// the sender filter matches, exactly like the real phone stack).
#[tokio::test]
async fn track_signal_listener_wakes_only_on_track_changes() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;
    use zbus::zvariant::Value;

    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed to run cover-provider integration tests");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    let _kill = KillOnDrop(daemon);

    // The listener's match rule filters `sender=org.bluez`; owning that name
    // on the fake server makes the filter pass, like the real BlueZ daemon.
    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .build()
        .await
        .unwrap();
    let listener_conn = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let wake = Arc::new(Notify::new());
    speaker_core::cli::cover_provider::track_signal_listener(listener_conn, Arc::clone(&wake));
    // The subscription now happens inside the spawned task; give it time to
    // register the match rule before the test emits any signals.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let emitter = zbus::object_server::SignalEmitter::new(
        &server,
        "/org/bluez/hci0/dev_30_E0_44_67_38_A2/player0",
    )
    .unwrap()
    .to_owned();

    // A non-Track property change must NOT wake the worker.
    let mut status = HashMap::new();
    status.insert(
        "Status".to_string(),
        Value::Value(Box::new(Value::Str("playing".into()))),
    );
    emitter
        .emit(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                "org.bluez.MediaPlayer1".to_string(),
                status,
                Vec::<String>::new(),
            ),
        )
        .await
        .unwrap();
    let quiet = tokio::time::timeout(Duration::from_millis(300), wake.notified()).await;
    assert!(quiet.is_err(), "non-Track change must not wake the worker");

    // A malformed PropertiesChanged body must not kill the stream: the
    // listener logs and keeps working.
    emitter
        .emit(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &"not a properties-changed tuple",
        )
        .await
        .unwrap();

    // A Track change must wake the worker.
    let mut track = HashMap::new();
    track.insert("Track".to_string(), Value::Value(Box::new(Value::U32(0))));
    emitter
        .emit(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                "org.bluez.MediaPlayer1".to_string(),
                track,
                Vec::<String>::new(),
            ),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), wake.notified())
        .await
        .expect("Track change must wake the worker");
}

/// `query_snapshot` (the `core` consumer path) reads the published snapshot
/// through a real D-Bus round-trip on the private bus.
#[test]
fn query_snapshot_reads_published_snapshot() {
    let h = Harness::new();
    h.publish_fixture("AA:BB:CC:DD:EE:F1", "5630701");

    let snapshot = speaker_core::hardware::bluetooth::cover_service::query_snapshot(&h.server)
        .expect("published snapshot must be readable");
    assert_eq!(snapshot.device.as_ref(), "AA:BB:CC:DD:EE:F1");
    assert_eq!(snapshot.handle.as_ref(), "5630701");
    assert!(
        snapshot
            .path
            .as_ref()
            .contains("AA_BB_CC_DD_EE_F1-5630701.jpg")
    );
}

/// With an inactive published state (empty handle), the consumer gets `None`.
#[test]
fn query_snapshot_returns_none_for_inactive_state() {
    let h = Harness::new();
    let snapshot = speaker_core::hardware::bluetooth::cover_service::query_snapshot(&h.server);
    assert!(
        snapshot.is_none(),
        "empty published state must read as None"
    );
}

/// With the cover-art service absent entirely (fresh bus without the
/// provider), the consumer degrades to `None` instead of erroring.
#[test]
fn query_snapshot_returns_none_when_service_absent() {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    let _kill = KillOnDrop(daemon);

    let conn = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .unwrap();
    let snapshot = speaker_core::hardware::bluetooth::cover_service::query_snapshot(&conn);
    assert!(
        snapshot.is_none(),
        "absent service must degrade to None (ServiceUnknown path)"
    );
}

/// The consumer fast path (disk cache) and slow path (D-Bus snapshot) both
/// resolve `cover_art_path` for the active track.
#[test]
fn resolve_cover_art_resolves_from_cache_and_service() {
    let h = Harness::new();
    let addr = BtAddress::parse_str("AA:BB:CC:DD:EE:F1").expect("valid MAC");
    let mut playback = Some(BtPlaybackState {
        device_address: Some(addr),
        image_handle: Some("5630701".into()),
        ..BtPlaybackState::default()
    });

    // Slow path: no cache file; the service publishes handle 5630701.
    h.publish_fixture("AA:BB:CC:DD:EE:F1", "5630701");
    resolve_cover_art_with(&h.server, h.dir.path(), &mut playback);
    assert_eq!(
        playback
            .as_ref()
            .expect("playback")
            .cover_art_path
            .as_deref(),
        Some(h.property("path").as_str()),
        "service snapshot must resolve cover_art_path"
    );

    // Fast path: the cache file for the (MAC, handle) pair exists now.
    let mut playback = Some(BtPlaybackState {
        device_address: Some(addr),
        image_handle: Some("5630701".into()),
        ..BtPlaybackState::default()
    });
    resolve_cover_art_with(&h.server, h.dir.path(), &mut playback);
    assert_eq!(
        playback
            .as_ref()
            .expect("playback")
            .cover_art_path
            .as_deref(),
        Some(h.property("path").as_str()),
        "disk-cache fast path must resolve cover_art_path"
    );
}

/// Handle mismatch between the track and the published snapshot must NOT
/// resolve (the consumer waits for the provider to catch up).
#[test]
fn resolve_cover_art_ignores_mismatched_snapshot() {
    let h = Harness::new();
    h.publish_fixture("AA:BB:CC:DD:EE:F1", "5630701");
    let addr = BtAddress::parse_str("AA:BB:CC:DD:EE:F1").expect("valid MAC");
    let mut playback = Some(BtPlaybackState {
        device_address: Some(addr),
        image_handle: Some("5630702".into()),
        ..BtPlaybackState::default()
    });
    resolve_cover_art_with(&h.server, h.dir.path(), &mut playback);
    assert_eq!(
        playback.as_ref().expect("playback").cover_art_path,
        None,
        "mismatched published handle must not resolve"
    );
}
