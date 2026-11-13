//! End-to-end tests for the audio-manager D-Bus `PropertiesChanged`
//! publication, served on a private `dbus-daemon`: a successful `SetLevel`
//! must announce the new `Level`/`Muted` to peers (the bt-manager reverse
//! volume-sync task consumes exactly this signal), and a failed write must
//! announce nothing.
//!
//! Follows the repository integration-test pattern: private session bus,
//! daemon killed on drop, loud failure when `dbus-daemon` is missing.

use audio_manager::{AmpState, AudioManagerInterface, MockAmpBackend, StateFile};
use futures_util::StreamExt;
use speaker_hardware::amp::PERCENT_TO_RAW;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use zbus::{MatchRule, MessageStream, Proxy};

/// Kills the private `dbus-daemon` when the test ends.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns a private session bus and returns its `KillOnDrop` guard plus the
/// bus address. Fails loudly when `dbus-daemon` is missing.
fn spawn_private_bus() -> (KillOnDrop, String) {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed to run audio-manager D-Bus integration tests");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    (KillOnDrop(daemon), address)
}

/// Serves the audio-manager interface (backed by the given mock backend) on a
/// private bus, owning the well-known `org.speaker.AudioManager` name.
async fn serve(
    address: &str,
    backend: audio_manager::MockAmpBackend,
    state_path: &std::path::Path,
) -> zbus::Connection {
    let conn = zbus::connection::Builder::address(address)
        .unwrap()
        .name("org.speaker.AudioManager")
        .unwrap()
        .build()
        .await
        .unwrap();
    let state = Arc::new(tokio::sync::Mutex::new(AmpState::default()));
    let iface = AudioManagerInterface::new(state, Arc::new(backend), StateFile::new(state_path));
    audio_manager::export_interface(&conn, iface).await.unwrap();
    conn
}

async fn amp_proxy(address: &str) -> Proxy<'static> {
    let conn = zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap();
    Proxy::new(
        &conn,
        "org.speaker.AudioManager",
        "/org/speaker/amp",
        "org.speaker.AudioManager1",
    )
    .await
    .unwrap()
}

async fn amp_signal_stream(address: &str) -> MessageStream {
    let conn = zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap();
    let rule = MatchRule::builder()
        .sender("org.speaker.AudioManager")
        .and_then(|b| b.path("/org/speaker/amp"))
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(zbus::match_rule::Builder::build)
        .unwrap();
    MessageStream::for_match_rule(rule, &conn, Some(8))
        .await
        .unwrap()
}

/// Extracts the changed-property keys from one `PropertiesChanged` message.
fn changed_keys(
    msg: &zbus::Message,
) -> std::collections::HashMap<String, zbus::zvariant::OwnedValue> {
    let (iface, props, _invalidated): (
        String,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        Vec<String>,
    ) = msg.body().deserialize().unwrap();
    assert_eq!(iface, "org.speaker.AudioManager1");
    props
}

#[tokio::test]
async fn set_level_announces_properties_changed() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, _ctrl) = MockAmpBackend::new_mocked();
    let (kill, address) = spawn_private_bus();
    let _server = serve(&address, backend, &dir.path().join("state.json")).await;
    let proxy = amp_proxy(&address).await;
    let mut stream = amp_signal_stream(&address).await;

    proxy.call_method("SetLevel", &(80u8)).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("PropertiesChanged must arrive within 5s")
        .unwrap()
        .unwrap();
    let props = changed_keys(&msg);
    let level: Option<u8> = props
        .get("Level")
        .and_then(|v| u8::try_from(v.clone()).ok());
    // Level must carry the committed value: the canonical inverse-curve
    // midpoint percent of the raw value that percent 80 maps to.
    let level = level.expect("Level property must be announced");
    let expected = AmpState {
        level_raw: PERCENT_TO_RAW[80],
        muted: false,
    }
    .level_percent(&PERCENT_TO_RAW);
    assert_eq!(level, expected, "unexpected Level: {level}");

    // The Muted property is announced in a second PropertiesChanged message.
    let msg2 = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("Muted PropertiesChanged must arrive within 5s")
        .unwrap()
        .unwrap();
    let props2 = changed_keys(&msg2);
    let muted: Option<bool> = props2
        .get("Muted")
        .and_then(|v| bool::try_from(v.clone()).ok());
    assert_eq!(
        muted,
        Some(false),
        "Muted property must carry the current value"
    );
    drop(proxy);
    drop(kill);
}

#[tokio::test]
async fn set_mute_announces_properties_changed() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, _ctrl) = MockAmpBackend::new_mocked();
    let (kill, address) = spawn_private_bus();
    let _server = serve(&address, backend, &dir.path().join("state.json")).await;
    let proxy = amp_proxy(&address).await;
    let mut stream = amp_signal_stream(&address).await;

    proxy.call_method("SetMute", &(true)).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("PropertiesChanged must arrive within 5s")
        .unwrap()
        .unwrap();
    let props = changed_keys(&msg);
    let muted: Option<bool> = props
        .get("Muted")
        .and_then(|v| bool::try_from(v.clone()).ok());
    assert_eq!(muted, Some(true), "Muted property must carry the new value");
    drop(proxy);
    drop(kill);
}

#[tokio::test]
async fn failed_write_announces_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, ctrl) = MockAmpBackend::new_mocked();
    let (kill, address) = spawn_private_bus();
    let _server = serve(&address, backend, &dir.path().join("state.json")).await;
    let proxy = amp_proxy(&address).await;
    let mut stream = amp_signal_stream(&address).await;

    ctrl.set_write_fail(true);
    let err: zbus::Error = proxy.call_method("SetLevel", &(50u8)).await.unwrap_err();
    assert!(err.to_string().contains("mock write failure"));

    // No signal must be published for a failed transaction.
    let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
    assert!(
        next.is_err(),
        "failed write must not emit PropertiesChanged"
    );
    drop(proxy);
    drop(kill);
}
