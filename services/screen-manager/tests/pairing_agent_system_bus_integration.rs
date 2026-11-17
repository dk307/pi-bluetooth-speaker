#![cfg(feature = "ui")]
//! Dedicated single-test binary for the pairing-agent system-bus contract
//! (plan §9; repo rule: `DBUS_SYSTEM_BUS_ADDRESS` mutation must never race
//! sibling tests).
//!
//! Every test here mutates `DBUS_SYSTEM_BUS_ADDRESS` — spawning a private
//! `dbus-daemon` and pointing zbus at it — and therefore serializes on
//! `ENV_GUARD`. No test in this binary expects the production system bus.
//! The remaining Bluetooth Devices screen tests live in
//! `bt_devices_integration.rs` and never touch the environment.

mod common;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use common::tick;
use screen_manager::daemon::{BT_PAIRING_TIMEOUT, BtMonitorState, BtSharedUpdate};
use speaker_bluetooth::{
    AdminAction, BluetoothDevice, BtAddress, DeviceCommand, DeviceCommandQueue, PairingStatus,
    SCREEN_AGENT_CAPABILITY, SCREEN_AGENT_PATH, TransportCommandQueue,
};
use speaker_common::diag::spawn_private_bus;
use zbus::blocking::Connection as BlockingConnection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const PHONE: BtAddress = BtAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const DEVICE_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const ADAPTER_ADDRESS: &str = "DC:A6:32:11:22:33";
const ADAPTER_ALIAS: &str = "FakePi";

/// Serializes every reader of `DBUS_SYSTEM_BUS_ADDRESS` within this test binary.
static ENV_GUARD: Mutex<()> = Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn set_bus_env(address: &str) {
    // SAFETY: tests are serialized by ENV_GUARD; no other test thread reads
    // DBUS_SYSTEM_BUS_ADDRESS while this guard is held.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", address) };
}

fn clear_bus_env() {
    // SAFETY: same serialized-guard invariant as set_bus_env.
    unsafe { std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS") };
}

/// In-memory `tracing` writer so failure diagnostics can show the raw error.
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log sink").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs(log: &Arc<Mutex<Vec<u8>>>) -> tracing::Dispatch {
    let sink = Arc::clone(log);
    tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(move || LogSink(Arc::clone(&sink)))
            .finish(),
    )
}

fn captured_log(log: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&log.lock().expect("log sink")).to_string()
}

/// Asserts the agent was registered on the fake before `Pair()` reached
/// `BlueZ`, with the exact registration payload.
fn assert_agent_registered_before_pair(calls: &Arc<Mutex<FakeCalls>>) {
    let guard = calls.lock().expect("fake lock");
    let seq = &guard.seq;
    let registered = seq
        .iter()
        .position(|entry| entry == "register_agent")
        .expect("agent registers first");
    let paired = seq
        .iter()
        .position(|entry| entry == "pair")
        .expect("Pair reaches BlueZ");
    assert!(registered < paired, "agent before Pair: {seq:?}");
    assert_eq!(
        guard.register_agent,
        vec![(
            SCREEN_AGENT_PATH.to_string(),
            SCREEN_AGENT_CAPABILITY.to_string()
        )]
    );
    assert_eq!(guard.request_default_agent, 1);
}

// Fake `org.bluez` for the live pairing state machine
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeCalls {
    seq: Vec<String>,
    register_agent: Vec<(String, String)>,
    request_default_agent: u32,
    unregister_agent: u32,
    pair_entered: u32,
    cancel_pairing: u32,
}

struct AdapterProps {
    address: &'static str,
    name: &'static str,
    alias: &'static str,
    powered: bool,
    pairable: bool,
    discovering: bool,
}

const ADAPTER_PROPS: AdapterProps = AdapterProps {
    address: ADAPTER_ADDRESS,
    name: "hci0",
    alias: ADAPTER_ALIAS,
    powered: true,
    pairable: true,
    discovering: false,
};

struct FakeAdapter1 {
    props: &'static AdapterProps,
}

#[zbus::interface(name = "org.bluez.Adapter1")]
impl FakeAdapter1 {
    #[zbus(property)]
    fn address(&self) -> String {
        self.props.address.to_string()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.props.name.to_string()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.props.alias.to_string()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        self.props.powered
    }

    #[zbus(property)]
    fn pairable(&self) -> bool {
        self.props.pairable
    }

    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.props.discovering
    }
}

struct DeviceProps {
    address: &'static str,
    name: &'static str,
    alias: &'static str,
    connected: bool,
    paired: bool,
    trusted: bool,
}

const DEVICE_PROPS: DeviceProps = DeviceProps {
    address: "AA:BB:CC:DD:EE:FF",
    name: "iPhone",
    alias: "iPhone 15 Pro",
    connected: false,
    paired: true,
    trusted: true,
};

struct FakeDevice1 {
    calls: Arc<Mutex<FakeCalls>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    props: &'static DeviceProps,
}

/// Waits until pairing is cancelled (condvar, spurious-wakeup safe) or the
/// 5s safety window elapses; returns `true` when cancelled. Runs on tokio's
/// blocking pool so the zbus executor stays free for concurrent calls.
fn wait_released(release: &(Mutex<bool>, Condvar)) -> bool {
    let (lock, cv) = release;
    let mut released = lock.lock().expect("release lock");
    while !*released {
        let (guard, timeout) = cv
            .wait_timeout(released, Duration::from_secs(5))
            .expect("release condvar");
        released = guard;
        if timeout.timed_out() {
            break;
        }
    }
    let cancelled = *released;
    *released = false; // re-arm the next pairing session
    cancelled
}

#[zbus::interface(name = "org.bluez.Device1")]
impl FakeDevice1 {
    #[zbus(property)]
    fn address(&self) -> String {
        self.props.address.to_string()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.props.name.to_string()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.props.alias.to_string()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.props.connected
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        self.props.paired
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.props.trusted
    }

    /// Blocks like `BlueZ`'s `Pair()` until the daemon cancels it (or the 5s
    /// safety fallback elapses) and always completes with an authentication
    /// error, mimicking a cancelled pairing window.
    ///
    /// The condvar wait runs on tokio's blocking pool: a zbus interface
    /// method future drives the connection's dispatch path while it runs, so
    /// parking that executor stalls every other call on the fake (notably the
    /// worker's `GetManagedObjects` snapshot, deadlocking the test).
    async fn pair(&self) -> zbus::fdo::Result<()> {
        self.record_pair_entry();
        let release = Arc::clone(&self.release);
        let cancelled = tokio::task::spawn_blocking(move || wait_released(&release))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("pair wait join: {e}")))?;
        Err(zbus::fdo::Error::Failed(if cancelled {
            "org.bluez.Error.AuthenticationCanceled".to_string()
        } else {
            "pair wait expired".to_string()
        }))
    }

    /// Records `Pair()` entry on the fake; sync so the guard never leaks
    /// into the async `pair()` future (Send across await).
    fn record_pair_entry(&self) {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("pair".to_string());
        calls.pair_entered += 1;
    }

    fn cancel_pairing(&self) {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("cancel_pairing".to_string());
        calls.cancel_pairing += 1;
        drop(calls);
        let (lock, cv) = &*self.release;
        *lock.lock().expect("release lock") = true;
        cv.notify_all();
    }
}

struct FakeAgentManager {
    calls: Arc<Mutex<FakeCalls>>,
}

#[zbus::interface(name = "org.bluez.AgentManager1")]
#[expect(
    clippy::needless_pass_by_value,
    reason = "D-Bus interface method signatures are fixed by the wire contract"
)]
impl FakeAgentManager {
    fn register_agent(&self, agent: ObjectPath<'_>, capability: String) {
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("register_agent".to_string());
        calls.register_agent.push((agent.to_string(), capability));
    }

    fn request_default_agent(&self, agent: ObjectPath<'_>) {
        let _ = agent;
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("request_default_agent".to_string());
        calls.request_default_agent += 1;
    }

    fn unregister_agent(&self, agent: ObjectPath<'_>) {
        let _ = agent;
        let mut calls = self.calls.lock().expect("fake lock");
        calls.seq.push("unregister_agent".to_string());
        calls.unregister_agent += 1;
    }
}

#[zbus::proxy(
    interface = "org.bluez.Agent1",
    gen_async = false,
    blocking_name = "AgentClientProxyBlocking"
)]
trait AgentClient {
    fn request_confirmation(&self, device: &ObjectPath<'_>, passkey: u32) -> zbus::Result<()>;
}

fn blocking_conn(address: &str) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .build()
        .expect("blocking connection")
}

/// Full fake `org.bluez`: `ObjectManager` at `/` (production-accurate and
/// required for `GetManagedObjects`), adapter, device, and agent manager.
fn fake_bluez(
    address: &str,
    calls: &Arc<Mutex<FakeCalls>>,
    release: &Arc<(Mutex<bool>, Condvar)>,
) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at("/", zbus::fdo::ObjectManager)
        .expect("serve object manager")
        .serve_at(
            ADAPTER_PATH,
            FakeAdapter1 {
                props: &ADAPTER_PROPS,
            },
        )
        .expect("serve adapter")
        .serve_at(
            DEVICE_PATH,
            FakeDevice1 {
                calls: Arc::clone(calls),
                release: Arc::clone(release),
                props: &DEVICE_PROPS,
            },
        )
        .expect("serve device")
        .serve_at(
            "/org/bluez",
            FakeAgentManager {
                calls: Arc::clone(calls),
            },
        )
        .expect("serve agent manager")
        .build()
        .expect("fake org.bluez connection")
}

/// Minimal fake `org.bluez`: only the agent manager (mock-backend pairing
/// tests never query `GetManagedObjects`).
fn fake_agent_manager(address: &str, calls: &Arc<Mutex<FakeCalls>>) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at(
            "/org/bluez",
            FakeAgentManager {
                calls: Arc::clone(calls),
            },
        )
        .expect("serve agent manager")
        .build()
        .expect("fake org.bluez connection")
}

fn unique_names(probe: &BlockingConnection) -> Vec<String> {
    zbus::blocking::fdo::DBusProxy::new(probe)
        .expect("bus proxy")
        .list_names()
        .expect("ListNames")
        .iter()
        .map(std::string::ToString::to_string)
        .filter(|name| name.starts_with(':'))
        .collect()
}

/// Plays `BlueZ`: confirms the pairing passkey to the screen's agent, found
/// as the only connection that appeared after pairing began.
fn confirm_passkey(probe: &BlockingConnection, baseline: &[String]) {
    let mut fresh: Vec<String> = unique_names(probe)
        .into_iter()
        .filter(|name| !baseline.contains(name))
        .collect();
    let agent = fresh
        .pop()
        .expect("the pairing agent must own a bus connection");
    assert!(fresh.is_empty(), "only the agent may be new: {fresh:?}");

    let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
    let proxy = AgentClientProxyBlocking::builder(probe)
        .destination(agent.as_str())
        .expect("agent destination")
        .path(SCREEN_AGENT_PATH)
        .expect("agent path")
        .build()
        .expect("agent client");
    proxy
        .request_confirmation(&device, 482_910)
        .expect("confirmation reaches the agent");
}

/// Ticks until the worker reaps the pairing session (agent unregistered).
fn tick_until_reaped(
    calls: &Arc<Mutex<FakeCalls>>,
    state: &mut BtMonitorState,
    bt: &BluetoothDevice,
    shared: &Mutex<BtSharedUpdate>,
    transport: &TransportCommandQueue,
    commands: &DeviceCommandQueue,
) {
    for _ in 0..50 {
        let reaped = calls.lock().expect("fake lock").unregister_agent;
        if reaped >= 1 {
            return;
        }
        tick(state, bt, shared, transport, commands, BT_PAIRING_TIMEOUT);
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the pairing agent must be unregistered when the session reaps");
}

#[test]
fn pairing_registers_agent_before_pair_and_fails_without_a_bus() {
    let _guard = lock_env();
    set_bus_env("unix:path=/nx");

    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    commands.push(DeviceCommand::Pair(PHONE));
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("registration failure stages a toast");
    assert!(update.pairing.is_none(), "no session without an agent");
    assert!(update.toast.is_some());
    assert_eq!(
        mock.last_admin_action(),
        None,
        "Pair must not be issued before the agent registers"
    );

    // A second attempt retries registration; an earlier session would have
    // staged "Operation already in progress" instead of another failure.
    commands.push(DeviceCommand::Pair(PHONE));
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("second failure stages a toast");
    assert!(update.toast.is_some());
    assert_ne!(
        update.toast.as_deref(),
        Some("Operation already in progress")
    );
    assert_eq!(mock.last_admin_action(), None);

    clear_bus_env();
}

#[test]
fn mock_pairing_success_reaps_agent_and_stops_discovery() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_agent_manager(&address, &calls);
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    commands.push(DeviceCommand::StartDiscovery);
    commands.push(DeviceCommand::Pair(PHONE));
    let mut terminal = None;
    for _ in 0..50 {
        let update = tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        );
        if let Some(update) = update
            && let Some(status) = update.pairing
            && !matches!(
                status,
                PairingStatus::Connecting { .. } | PairingStatus::AwaitingConfirmation { .. }
            )
        {
            terminal = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(terminal, Some(PairingStatus::Paired { .. })),
        "mock pairing must complete, got {terminal:?}"
    );

    let guard = calls.lock().expect("fake lock");
    assert_eq!(guard.register_agent.len(), 1, "one agent registered");
    assert_eq!(guard.request_default_agent, 1);
    assert_eq!(guard.unregister_agent, 1, "terminal reaps the agent");
    drop(guard);
    assert!(!mock.discovering(), "reap stops screen-started discovery");
    assert_eq!(mock.last_admin_action(), Some(AdminAction::StopDiscovery));

    // The reap stopped discovery, so one tick still publishes the cleared
    // scanning flag; only after that settles is CancelPairing a pure no-op.
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    commands.push(DeviceCommand::CancelPairing);
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_none(),
        "a reaped session turns CancelPairing into a no-op"
    );
    clear_bus_env();
}

#[test]
fn mock_pairing_fault_publishes_failed_terminal() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_agent_manager(&address, &calls);
    let (bt, mock) = BluetoothDevice::new_mocked();
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    mock.set_fault(Some("pair fault".into()));
    commands.push(DeviceCommand::Pair(PHONE));
    let mut terminal = None;
    for _ in 0..50 {
        let update = tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        );
        if let Some(update) = update
            && let Some(status) = update.pairing
            && !matches!(status, PairingStatus::Connecting { .. })
        {
            terminal = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match terminal {
        Some(PairingStatus::Failed { message }) => {
            assert_eq!(message.as_ref(), "Operation failed");
        }
        other => panic!("faulted pairing must fail, got {other:?}"),
    }

    let guard = calls.lock().expect("fake lock");
    assert_eq!(guard.unregister_agent, 1, "failure also reaps the agent");
    drop(guard);
    clear_bus_env();
}

#[test]
fn live_pairing_publishes_passkey_then_times_out() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let _fake = fake_bluez(&address, &calls, &release);
    let probe = blocking_conn(&address);
    let bt = BluetoothDevice::with_connection(blocking_conn(&address));
    let baseline = unique_names(&probe);
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let first = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("first tick publishes rows from the fake adapter");
    assert_eq!(first.rows.len(), 1);
    assert_eq!(first.adapter_alias.as_deref(), Some(ADAPTER_ALIAS));

    // Short injected deadline: agent registration and the passkey must be
    // observed well inside it (production passes BT_PAIRING_TIMEOUT).
    let deadline = Duration::from_millis(800);
    commands.push(DeviceCommand::Pair(PHONE));
    let log = Arc::new(Mutex::new(Vec::new()));
    let dispatch = capture_logs(&log);
    let update = tracing::dispatcher::with_default(&dispatch, || {
        tick(&mut state, &bt, &shared, &transport, &commands, deadline)
    })
    .expect("connecting publishes");
    assert!(
        matches!(update.pairing, Some(PairingStatus::Connecting { .. })),
        "expected Connecting, got {:?} (toast: {:?}) | log: {} | seq: {:?}",
        update.pairing,
        update.toast,
        captured_log(&log),
        calls.lock().expect("fake lock").seq
    );
    assert_agent_registered_before_pair(&calls);

    confirm_passkey(&probe, &baseline);
    let update =
        tick(&mut state, &bt, &shared, &transport, &commands, deadline).expect("passkey publishes");
    match update.pairing {
        Some(PairingStatus::AwaitingConfirmation { name, passkey, .. }) => {
            assert_eq!(passkey, 482_910);
            assert_eq!(name.as_ref(), "iPhone 15 Pro");
        }
        other => panic!("expected passkey confirmation, got {other:?}"),
    }

    // A second Pair while one is in flight must not restart the attempt; the
    // refusal rides the live session's status along so the shell cannot
    // mistake it for "no session" and release the pairing view.
    commands.push(DeviceCommand::Pair(PHONE));
    let update = tick(&mut state, &bt, &shared, &transport, &commands, deadline)
        .expect("in-progress toast publishes");
    assert_eq!(
        update.toast.as_deref(),
        Some("Operation already in progress")
    );
    assert!(
        matches!(update.pairing, Some(PairingStatus::Connecting { .. })),
        "live status must ride along with the refusal, got {:?}",
        update.pairing
    );
    assert_eq!(
        calls.lock().expect("fake lock").pair_entered,
        1,
        "second Pair must not reach BlueZ"
    );

    std::thread::sleep(Duration::from_millis(900));
    let update =
        tick(&mut state, &bt, &shared, &transport, &commands, deadline).expect("timeout publishes");
    assert!(matches!(update.pairing, Some(PairingStatus::TimedOut)));
    assert_eq!(update.toast.as_deref(), Some("Pairing timed out"));
    assert_eq!(
        calls.lock().expect("fake lock").cancel_pairing,
        1,
        "the deadline cancels the BlueZ pair"
    );

    tick_until_reaped(&calls, &mut state, &bt, &shared, &transport, &commands);
    commands.push(DeviceCommand::CancelPairing);
    assert!(
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
        .is_none(),
        "a reaped session turns CancelPairing into a no-op"
    );
    clear_bus_env();
}

#[test]
fn live_pairing_user_cancellation_publishes_cancelled() {
    let _guard = lock_env();
    let (_bus, address) = spawn_private_bus();
    set_bus_env(&address);
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let _fake = fake_bluez(&address, &calls, &release);
    let bt = BluetoothDevice::with_connection(blocking_conn(&address));
    let shared = Mutex::new(BtSharedUpdate::default());
    let transport = TransportCommandQueue::new();
    let commands = DeviceCommandQueue::new();
    let mut state = BtMonitorState::default();
    let _ = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    );

    commands.push(DeviceCommand::Pair(PHONE));
    let log = Arc::new(Mutex::new(Vec::new()));
    let dispatch = capture_logs(&log);
    let update = tracing::dispatcher::with_default(&dispatch, || {
        tick(
            &mut state,
            &bt,
            &shared,
            &transport,
            &commands,
            BT_PAIRING_TIMEOUT,
        )
    })
    .expect("connecting publishes");
    assert!(
        matches!(update.pairing, Some(PairingStatus::Connecting { .. })),
        "expected Connecting, got {:?} (toast: {:?}) | log: {} | seq: {:?}",
        update.pairing,
        update.toast,
        captured_log(&log),
        calls.lock().expect("fake lock").seq
    );

    commands.push(DeviceCommand::CancelPairing);
    let update = tick(
        &mut state,
        &bt,
        &shared,
        &transport,
        &commands,
        BT_PAIRING_TIMEOUT,
    )
    .expect("cancellation publishes");
    assert!(matches!(update.pairing, Some(PairingStatus::Cancelled)));
    assert!(update.toast.is_none(), "cancellation is not an error toast");
    assert_eq!(calls.lock().expect("fake lock").cancel_pairing, 1);

    tick_until_reaped(&calls, &mut state, &bt, &shared, &transport, &commands);
    let guard = calls.lock().expect("fake lock");
    assert_eq!(guard.unregister_agent, 1);
    assert_eq!(guard.pair_entered, 1);
    assert!(guard.seq.contains(&"request_default_agent".to_string()));
    drop(guard);
    clear_bus_env();
}
