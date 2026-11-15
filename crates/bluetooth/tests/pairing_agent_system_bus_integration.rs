//! Covers the production entry point `PairingAgentHandle::register()`, which
//! reads `DBUS_SYSTEM_BUS_ADDRESS` through zbus's `Address::system()`.
//!
//! This test lives in its OWN test binary (rather than `device_admin_integration`)
//! because `std::env::set_var` must not run concurrently with any other
//! environment access: cargo executes test targets in separate processes,
//! sequentially, and this target contains a single test on a single harness
//! thread — so no sibling thread can read the environment (`Backtrace`,
//! `PATH` during `dbus-daemon` spawns) while it is mutated (data-race UB).

use std::sync::{Arc, Mutex};

use speaker_bluetooth::{PairingAgentHandle, SCREEN_AGENT_CAPABILITY, SCREEN_AGENT_PATH};
use speaker_common::diag::spawn_private_bus;
use zbus::blocking::Connection as BlockingConnection;
use zbus::zvariant::ObjectPath;

#[derive(Default)]
struct FakeCalls {
    registered: Vec<(String, String)>,
    default_requests: u32,
    unregistered: u32,
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
        self.calls
            .lock()
            .expect("fake lock")
            .registered
            .push((agent.to_string(), capability));
    }

    fn request_default_agent(&self, agent: ObjectPath<'_>) {
        let _ = agent;
        self.calls.lock().expect("fake lock").default_requests += 1;
    }

    fn unregister_agent(&self, agent: ObjectPath<'_>) {
        let _ = agent;
        self.calls.lock().expect("fake lock").unregistered += 1;
    }
}

/// Serves the subset of fake `org.bluez` that agent registration touches.
fn fake_agent_bus(address: &str, calls: &Arc<Mutex<FakeCalls>>) -> BlockingConnection {
    zbus::blocking::connection::Builder::address(address)
        .expect("bus address")
        .name("org.bluez")
        .expect("own the org.bluez name")
        .serve_at("/", zbus::fdo::ObjectManager)
        .expect("serve object manager")
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

#[test]
fn pairing_agent_register_connects_via_system_bus_address() {
    // SAFETY: this single-test binary never accesses the environment from any
    // other thread (see module doc) — no concurrent `getenv`/`setenv`/spawn.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nx") };
    let err = PairingAgentHandle::register(Arc::new(Mutex::new(None)))
        .expect_err("unreachable system bus must fail");
    assert!(err.is_dbus(), "expected typed D-Bus error, got {err:?}");

    // Positive: the production entry point must honor DBUS_SYSTEM_BUS_ADDRESS
    // (zbus reads it on every `Address::system()`) and register as the default
    // agent — this is the path the screen daemon will call.
    let (_guard, address) = spawn_private_bus();
    // SAFETY: same single-test-binary invariant as above.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", &address) };
    let calls = Arc::new(Mutex::new(FakeCalls::default()));
    let _fake = fake_agent_bus(&address, &calls);
    let handle = PairingAgentHandle::register(Arc::new(Mutex::new(None)))
        .expect("register via system bus address");

    let state = calls.lock().expect("fake lock");
    assert_eq!(
        state.registered,
        vec![(
            SCREEN_AGENT_PATH.to_string(),
            SCREEN_AGENT_CAPABILITY.to_string()
        )]
    );
    assert_eq!(state.default_requests, 1);
    drop(state);

    handle.unregister();
    assert_eq!(calls.lock().expect("fake lock").unregistered, 1);

    // SAFETY: same single-test-binary invariant as above.
    unsafe { std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS") };
}
