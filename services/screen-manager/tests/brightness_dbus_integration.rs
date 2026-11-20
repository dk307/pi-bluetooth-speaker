//! Integration tests for the screen-manager brightness **hardware** D-Bus
//! client, served against a fake `org.speaker.Autobright1` on a private
//! `dbus-daemon`. Covers the paths a mock cannot: `GetState` reads, command
//! writes, typed bus-error mapping, and the `PropertiesChanged` signal watcher
//! feeding `run_worker`.
//!
//! Follows the repository integration-test pattern: private session bus,
//! daemon killed on drop, loud failure when `dbus-daemon` is missing.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use screen_manager::brightness::{
    BrightnessClient, BrightnessCommand, BrightnessCommandQueue, run_worker,
};
use screen_manager::daemon::BtSharedUpdate;
use zbus::blocking::Proxy;
use zbus::fdo;
use zbus::object_server::SignalEmitter;

/// Mutable snapshot behind the fake daemon.
#[derive(Debug, Clone, Copy)]
struct FakeState {
    auto_enabled: bool,
    brightness_percent: f64,
    ambient_lux: f64,
    manual_gamma: f64,
    available: bool,
}

/// Fake `org.speaker.Autobright1` daemon. Mutations emit real
/// `PropertiesChanged` signals, exactly like the production daemon.
struct FakeAutobright {
    state: Mutex<FakeState>,
    fail_next: AtomicBool,
}

#[zbus::interface(name = "org.speaker.Autobright1")]
impl FakeAutobright {
    #[zbus(property)]
    fn auto_enabled(&self) -> bool {
        self.state.lock().unwrap().auto_enabled
    }

    #[zbus(property)]
    fn brightness_percent(&self) -> f64 {
        self.state.lock().unwrap().brightness_percent
    }

    #[zbus(property)]
    fn ambient_lux(&self) -> f64 {
        self.state.lock().unwrap().ambient_lux
    }

    #[zbus(property)]
    fn manual_gamma(&self) -> f64 {
        self.state.lock().unwrap().manual_gamma
    }

    #[zbus(property)]
    fn available(&self) -> bool {
        self.state.lock().unwrap().available
    }

    /// Screen-initiated `GetState` (`BrightnessClient::state` path).
    fn get_state(&self) -> (bool, f64, f64, f64) {
        let s = *self.state.lock().unwrap();
        (
            s.auto_enabled,
            s.brightness_percent,
            s.ambient_lux,
            s.manual_gamma,
        )
    }

    /// Screen-initiated manual slider write; leaves auto mode like production.
    async fn set_manual_percent(
        &self,
        percent: f64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(fdo::Error::Failed(
                "injected SetManualPercent failure".into(),
            ));
        }
        {
            let mut s = self.state.lock().unwrap();
            s.brightness_percent = percent;
            s.auto_enabled = false;
        };
        Self::brightness_percent_changed(self, &emitter).await?;
        Self::auto_enabled_changed(self, &emitter).await?;
        Ok(())
    }

    /// Screen-initiated AUTO toggle.
    async fn set_auto_enabled(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(fdo::Error::Failed("injected SetAutoEnabled failure".into()));
        }
        self.state.lock().unwrap().auto_enabled = enabled;
        Self::auto_enabled_changed(self, &emitter).await?;
        Ok(())
    }

    /// Screen-initiated gamma reset.
    fn reset_manual_bias(&self) -> fdo::Result<()> {
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(fdo::Error::Failed(
                "injected ResetManualBias failure".into(),
            ));
        }
        self.state.lock().unwrap().manual_gamma = 1.0;
        Ok(())
    }

    /// Test-only hook: flips fault injection on the next screen write.
    fn set_fail_next(&self, enabled: bool) {
        self.fail_next.store(enabled, Ordering::Relaxed);
    }

    /// Test-only hook: simulates a daemon-initiated ambient change with no
    /// preceding UI command, emitting the same signals as a real tick.
    async fn force_external_change(
        &self,
        brightness_percent: f64,
        ambient_lux: f64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        {
            let mut s = self.state.lock().unwrap();
            s.brightness_percent = brightness_percent;
            s.ambient_lux = ambient_lux;
        };
        let _ = Self::brightness_percent_changed(self, &emitter).await;
        let _ = Self::ambient_lux_changed(self, &emitter).await;
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns a private session bus and returns its guard plus the bus address.
/// Fails loudly when `dbus-daemon` is missing.
fn spawn_private_bus() -> (KillOnDrop, String) {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must be installed to run brightness D-Bus integration tests");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().expect("piped stdout"))
        .read_line(&mut address)
        .expect("read bus address");
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    (KillOnDrop(daemon), address)
}

struct Harness {
    _daemon: KillOnDrop,
    _server: zbus::blocking::Connection,
    client: BrightnessClient,
    fake: Proxy<'static>,
}

impl Harness {
    fn new() -> Self {
        let (kill, address) = spawn_private_bus();
        let server = zbus::blocking::connection::Builder::address(address.as_str())
            .expect("server bus address")
            .name("org.speaker.Autobright")
            .expect("own autobright bus name")
            .serve_at(
                "/org/speaker/autobright",
                FakeAutobright {
                    state: Mutex::new(FakeState {
                        auto_enabled: true,
                        brightness_percent: 72.0,
                        ambient_lux: 180.0,
                        manual_gamma: 1.0,
                        available: true,
                    }),
                    fail_next: AtomicBool::new(false),
                },
            )
            .expect("serve fake autobright")
            .build()
            .expect("build server connection");

        let client_conn = zbus::blocking::connection::Builder::address(address.as_str())
            .expect("client bus address")
            .build()
            .expect("build client connection");
        let client = BrightnessClient::with_connection(&client_conn).expect("brightness client");

        let fake_conn = zbus::blocking::connection::Builder::address(address.as_str())
            .expect("fake bus address")
            .build()
            .expect("build fake connection");
        let fake = Proxy::new_owned(
            fake_conn,
            "org.speaker.Autobright",
            "/org/speaker/autobright",
            "org.speaker.Autobright1",
        )
        .expect("fake proxy");

        Self {
            _daemon: kill,
            _server: server,
            client,
            fake,
        }
    }

    fn fail_next(&self) {
        self.fake
            .call::<_, _, ()>("SetFailNext", &(true,))
            .expect("arm fault injection");
    }
}

fn published(shared: &Mutex<BtSharedUpdate>) -> Option<(bool, f32)> {
    shared
        .lock()
        .unwrap()
        .brightness
        .map(|state| (state.auto_enabled, state.brightness_percent))
}

#[test]
fn state_reads_get_state_over_private_bus() {
    let h = Harness::new();
    let state = h.client.state().expect("GetState must succeed");
    assert!(state.auto_enabled);
    assert_eq!(state.brightness_percent, 72.0);
    assert_eq!(state.ambient_lux, 180.0);
}

#[test]
fn apply_writes_methods_and_reads_back() {
    let h = Harness::new();

    let state = h
        .client
        .apply(BrightnessCommand::SetPercent(34.0))
        .expect("SetManualPercent must succeed");
    assert_eq!(state.brightness_percent, 34.0);
    assert!(!state.auto_enabled);
    assert_eq!(h.client.state().unwrap().brightness_percent, 34.0);

    let state = h
        .client
        .apply(BrightnessCommand::SetAuto(true))
        .expect("SetAutoEnabled must succeed");
    assert!(state.auto_enabled);

    h.client
        .apply(BrightnessCommand::ResetBias)
        .expect("ResetManualBias must succeed");
    assert_eq!(h.client.state().unwrap().manual_gamma, 1.0);
}

#[test]
fn failed_write_maps_to_typed_bus_error_with_source() {
    let h = Harness::new();
    h.fail_next();
    let err = h
        .client
        .apply(BrightnessCommand::SetPercent(10.0))
        .expect_err("injected failure must propagate");
    assert!(err.to_string().contains("brightness D-Bus error"));
    let source = std::error::Error::source(&err).expect("bus error must expose the zbus cause");
    assert!(
        source
            .to_string()
            .contains("injected SetManualPercent failure"),
        "{source}"
    );
    assert_eq!(h.client.state().unwrap().brightness_percent, 72.0);
}

/// Full worker lifecycle on the private bus: a UI command and a daemon-
/// initiated `PropertiesChanged` both publish into `BtSharedUpdate`, exercising
/// `spawn_signal_watcher`, `notify_change` and `drain_idle` end to end.
#[test]
fn worker_applies_commands_and_external_changes() {
    let h = Harness::new();
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let commands = BrightnessCommandQueue::new();
    let running = Arc::new(AtomicBool::new(true));

    let worker = {
        let shared = Arc::clone(&shared);
        let commands = commands.clone();
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            run_worker(&h.client, &shared, &commands, &running);
        })
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while published(&shared) != Some((true, 72.0)) {
        assert!(
            Instant::now() < deadline,
            "worker never published initial seed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    commands.push(BrightnessCommand::SetPercent(41.0));
    let deadline = Instant::now() + Duration::from_secs(5);
    while published(&shared) != Some((false, 41.0)) {
        assert!(
            Instant::now() < deadline,
            "brightness command never applied"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // No queued command: a real PropertiesChanged from the daemon must wake
    // the watcher and be republished by the worker's idle poll.
    h.fake
        .call::<_, _, ()>("ForceExternalChange", &(88.0f64, 640.0f64))
        .expect("force external change");

    let deadline = Instant::now() + Duration::from_secs(5);
    while published(&shared) != Some((false, 88.0)) {
        assert!(
            Instant::now() < deadline,
            "external brightness change never published (watcher dead?)"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    worker.join().expect("worker panicked");
}
