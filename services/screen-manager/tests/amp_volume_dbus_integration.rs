//! Integration tests for the screen-manager `amp-volume` **hardware** D-Bus
//! client, served against a fake `org.speaker.AudioManager1` on a private
//! `dbus-daemon`. Covers exactly the paths no mock can exercise: property
//! reads (`Level`/`Muted`), method writes (`SetLevel`/`SetMute`), typed bus
//! error mapping, and the event-driven `PropertiesChanged` watcher feeding
//! `run_worker`.
//!
//! Follows the repository integration-test pattern: private session bus,
//! daemon killed on drop, loud failure when `dbus-daemon` is missing.

use screen_manager::daemon::BtSharedUpdate;
use screen_manager::volume::{
    AmpVolumeClient, AmpVolumeCommand, AmpVolumeCommandQueue, run_worker,
};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::blocking::Proxy;

/// Fake `org.speaker.AudioManager1` amp service. Mutations emit a real
/// `PropertiesChanged` signal on the bus, like the real audio-manager.
struct FakeAmp {
    level: AtomicU8,
    muted: AtomicBool,
    /// When set, the next `SetLevel`/`SetMute` call fails (fault injection).
    fail_next: AtomicBool,
}

#[zbus::interface(name = "org.speaker.AudioManager1")]
impl FakeAmp {
    #[zbus(property)]
    fn level(&self) -> u8 {
        self.level.load(Ordering::Relaxed)
    }

    #[zbus(property)]
    fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Screen-initiated level write (`AmpVolumeClient::apply` path).
    async fn set_level(
        &self,
        percent: u8,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> Result<(), zbus::fdo::Error> {
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(zbus::fdo::Error::Failed("injected SetLevel failure".into()));
        }
        self.level.store(percent.min(100), Ordering::Relaxed);
        Self::level_changed(self, &emitter).await?;
        Ok(())
    }

    /// Screen-initiated mute write (`AmpVolumeClient::apply` path).
    async fn set_mute(
        &self,
        muted: bool,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> Result<(), zbus::fdo::Error> {
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(zbus::fdo::Error::Failed("injected SetMute failure".into()));
        }
        self.muted.store(muted, Ordering::Relaxed);
        Self::muted_changed(self, &emitter).await?;
        Ok(())
    }

    /// Test-only hook: flips fault injection on the next screen write.
    fn set_fail_next(&self, enabled: bool) {
        self.fail_next.store(enabled, Ordering::Relaxed);
    }
}

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
        .expect("dbus-daemon must be installed to run amp-volume D-Bus integration tests");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    (KillOnDrop(daemon), address)
}

/// Private bus serving the fake amp; daemon killed on drop.
struct Harness {
    // Kept alive for the harness lifetime; `KillOnDrop` reaps the daemon.
    _daemon: KillOnDrop,
    // Keeps the fake's connection (and thus the served object) alive.
    _server: zbus::blocking::Connection,
    client: AmpVolumeClient,
    /// Test-side proxy to the fake (fault injection).
    fake: Proxy<'static>,
}

impl Harness {
    fn new(initial_level: u8) -> Self {
        let (kill, address) = spawn_private_bus();

        let server = zbus::blocking::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.speaker.AudioManager")
            .unwrap()
            .serve_at(
                "/org/speaker/amp",
                FakeAmp {
                    level: AtomicU8::new(initial_level),
                    muted: AtomicBool::new(false),
                    fail_next: AtomicBool::new(false),
                },
            )
            .unwrap()
            .build()
            .unwrap();

        let client_conn = zbus::blocking::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let client = AmpVolumeClient::with_connection(&client_conn).unwrap();

        let fake_conn = zbus::blocking::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let fake = Proxy::new_owned(
            fake_conn,
            "org.speaker.AudioManager",
            "/org/speaker/amp",
            "org.speaker.AudioManager1",
        )
        .unwrap();

        Self {
            _daemon: kill,
            _server: server,
            client,
            fake,
        }
    }

    /// Arms fault injection for the next screen write.
    fn fail_next(&self) {
        self.fake.call::<_, _, ()>("SetFailNext", &(true,)).unwrap();
    }
}

fn amp_level(shared: &Mutex<BtSharedUpdate>) -> Option<u8> {
    shared.lock().unwrap().amp_volume.map(|s| s.level)
}

#[test]
fn state_reads_properties_over_private_bus() {
    let h = Harness::new(73);
    let state = h.client.state().expect("property read must succeed");
    assert_eq!(state.level, 73);
    assert!(!state.muted);
}

#[test]
fn apply_writes_methods_and_reads_back() {
    let h = Harness::new(50);

    let state = h
        .client
        .apply(AmpVolumeCommand::SetLevel(34))
        .expect("SetLevel must succeed");
    assert_eq!(state.level, 34);
    assert_eq!(state.level, h.client.state().unwrap().level);

    let state = h
        .client
        .apply(AmpVolumeCommand::SetMute(true))
        .expect("SetMute must succeed");
    assert!(state.muted);
    assert!(h.client.state().unwrap().muted);
}

#[test]
fn failed_write_maps_to_typed_bus_error_with_source() {
    let h = Harness::new(50);
    h.fail_next();
    let err = h
        .client
        .apply(AmpVolumeCommand::SetLevel(10))
        .expect_err("injected failure must propagate");
    assert!(err.to_string().contains("amp volume D-Bus error"));
    let source = std::error::Error::source(&err).expect("bus error must expose the zbus cause");
    assert!(
        source.to_string().contains("injected SetLevel failure"),
        "{source}"
    );
    // A failed write must not mutate the fake.
    assert_eq!(h.client.state().unwrap().level, 50);
}

/// Full worker lifecycle on the private bus: command application and an
/// out-of-band `PropertiesChanged` (the phone-initiated sync path) both
/// publish into `BtSharedUpdate` — exercising `spawn_signal_watcher`,
/// `notify_change`, and `drain_idle` end-to-end.
#[test]
fn run_worker_applies_commands_and_external_changes() {
    let h = Harness::new(50);
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let commands = AmpVolumeCommandQueue::new();
    let running = Arc::new(AtomicBool::new(true));

    let worker = {
        let shared = Arc::clone(&shared);
        let commands = commands.clone();
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            run_worker(&h.client, &shared, &commands, &running);
        })
    };

    // Initial seed publish from the worker.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while amp_level(&shared) != Some(50) {
        assert!(
            std::time::Instant::now() < deadline,
            "worker never published the initial seed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Screen command: encoder detent.
    commands.push(AmpVolumeCommand::SetLevel(20));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while amp_level(&shared) != Some(20) {
        assert!(
            std::time::Instant::now() < deadline,
            "command never applied"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Out-of-band change on the fake (no queued command): `SetLevel` flips
    // the level and emits a real `PropertiesChanged`, exactly like
    // bt-manager syncing a phone volume change into audio-manager. The
    // worker's signal watcher must wake it and publish the new level.
    h.fake.call::<_, _, ()>("SetLevel", &(66u8,)).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while amp_level(&shared) != Some(66) {
        assert!(
            std::time::Instant::now() < deadline,
            "external change never published (watcher dead?)"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    worker.join().expect("worker panicked");
}

#[test]
fn drain_idle_wakes_on_change_notification_and_command() {
    let queue = AmpVolumeCommandQueue::new();

    // A change notification ends the idle wait early and is consumed once.
    let notifier = queue.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        notifier.notify_change();
    });
    let (commands, changed) = queue.drain_idle(Duration::from_secs(5));
    assert_eq!(commands, Vec::<AmpVolumeCommand>::new());
    assert!(changed);
    handle.join().expect("notifier thread panicked");

    // A command pushed mid-wait is returned immediately (latency fix): the
    // single condvar serves both wake-up sources.
    let pusher = queue.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        pusher.push(AmpVolumeCommand::SetLevel(42));
    });
    let (commands, changed) = queue.drain_idle(Duration::from_secs(5));
    assert_eq!(commands, vec![AmpVolumeCommand::SetLevel(42)]);
    assert!(!changed);
    handle.join().expect("pusher thread panicked");

    // After both wake-ups were consumed, the wait times out with nothing.
    let (commands, changed) = queue.drain_idle(Duration::from_millis(20));
    assert_eq!(commands, Vec::<AmpVolumeCommand>::new());
    assert!(!changed);
}
