//! Private-bus integration tests for the `org.speaker.Autobright1` contract.
//!
//! Each test owns a fresh `dbus-daemon` session bus (via the shared hermetic
//! helper) and fails loudly if the daemon binary is unavailable. The real
//! interface implementation and generated client proxy are used end to end.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use autobright::daemon::Command;
use autobright::dbus::{AutobrightProxy, PublishedState, emit_changes, serve};
use futures_util::StreamExt;
use speaker_common::diag::spawn_private_bus;
use zbus::connection::Builder;

/// Waits for a property stream to report `expected`.
///
/// zbus emits an initial cached-value event when a change stream is created,
/// so the first event is not necessarily the one caused by our command.
macro_rules! expect_property_change {
    ($stream:expr, $expected:expr) => {{
        let mut matched = false;
        for _ in 0..8 {
            let Some(change) = $stream.next().await else {
                break;
            };
            if change.get().await.expect("property read") == $expected {
                matched = true;
                break;
            }
        }
        assert!(
            matched,
            "expected a PropertiesChanged event for {:?}",
            $expected
        );
    }};
}

fn seeded_state() -> PublishedState {
    PublishedState {
        auto_enabled: true,
        brightness_percent: 40.0,
        ambient_lux: 123.0,
        manual_gamma: 1.5,
        available: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn methods_properties_and_signals_work_end_to_end() {
    let (_bus, address) = spawn_private_bus();
    let server = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("server connection to private bus");
    let client = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("client connection to private bus");

    let shared = Arc::new(Mutex::new(seeded_state()));
    let (tx, rx) = mpsc::channel::<Command>();
    serve(&server, Arc::clone(&shared), tx)
        .await
        .expect("export autobright interface");

    let proxy = AutobrightProxy::new(&client).await.expect("client proxy");
    let mut percent_changes = proxy.receive_brightness_percent_changed().await;
    let mut auto_changes = proxy.receive_auto_enabled_changed().await;

    assert_eq!(
        proxy.get_state().await.expect("get_state"),
        (true, 40.0, 123.0, 1.5)
    );
    assert!(proxy.available().await.expect("available"));
    assert_eq!(proxy.manual_gamma().await.expect("manual_gamma"), 1.5);

    proxy
        .set_manual_percent(42.5)
        .await
        .expect("SetManualPercent");
    assert_eq!(
        rx.recv().expect("manual command"),
        Command::SetManualPercent(42.5)
    );
    expect_property_change!(percent_changes, 42.5);
    assert_eq!(
        proxy
            .brightness_percent()
            .await
            .expect("brightness_percent"),
        42.5
    );
    // A slider drag implicitly leaves auto mode and publishes the transition.
    expect_property_change!(auto_changes, false);
    assert!(!proxy.auto_enabled().await.expect("auto_enabled"));

    proxy
        .set_auto_enabled(true)
        .await
        .expect("SetAutoEnabled(true)");
    assert_eq!(
        rx.recv().expect("auto command"),
        Command::SetAutoEnabled(true)
    );
    expect_property_change!(auto_changes, true);

    proxy
        .set_auto_enabled(false)
        .await
        .expect("SetAutoEnabled(false)");
    assert_eq!(
        rx.recv().expect("auto command"),
        Command::SetAutoEnabled(false)
    );
    expect_property_change!(auto_changes, false);

    proxy.reset_manual_bias().await.expect("ResetManualBias");
    assert_eq!(rx.recv().expect("reset command"), Command::ResetManualBias);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_manual_percent_is_rejected_on_the_wire() {
    let (_bus, address) = spawn_private_bus();
    let server = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("server connection");
    let client = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("client connection");

    let shared = Arc::new(Mutex::new(seeded_state()));
    let (tx, rx) = mpsc::channel::<Command>();
    serve(&server, shared, tx).await.expect("export interface");

    let proxy = AutobrightProxy::new(&client).await.expect("proxy");
    assert!(proxy.set_manual_percent(f64::NAN).await.is_err());
    assert!(proxy.set_manual_percent(-1.0).await.is_err());
    assert!(proxy.set_manual_percent(101.0).await.is_err());
    assert!(rx.try_recv().is_err(), "no command may escape validation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick_driven_state_updates_emit_properties_changed() {
    let (_bus, address) = spawn_private_bus();
    let server = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("server connection");
    let client = Builder::address(address.as_str())
        .expect("valid private bus address")
        .build()
        .await
        .expect("client connection");

    let shared = Arc::new(Mutex::new(seeded_state()));
    let (tx, _rx) = mpsc::channel::<Command>();
    serve(&server, Arc::clone(&shared), tx)
        .await
        .expect("export interface");

    let proxy = AutobrightProxy::new(&client).await.expect("proxy");
    let mut lux_changes = proxy.receive_ambient_lux_changed().await;
    let mut gamma_changes = proxy.receive_manual_gamma_changed().await;
    let mut available_changes = proxy.receive_available_changed().await;
    let mut auto_changes = proxy.receive_auto_enabled_changed().await;

    let previous = PublishedState {
        auto_enabled: true,
        brightness_percent: 10.0,
        ambient_lux: 1.0,
        manual_gamma: 1.0,
        available: false,
    };
    let next = PublishedState {
        auto_enabled: false,
        brightness_percent: 20.0,
        ambient_lux: 2.0,
        manual_gamma: 1.7,
        available: true,
    };
    // The tick loop publishes state before notifying; mirror that ordering.
    *shared.lock().expect("state lock") = next;
    emit_changes(&server, previous, next)
        .await
        .expect("emit PropertiesChanged");

    expect_property_change!(lux_changes, 2.0);
    expect_property_change!(gamma_changes, 1.7);
    expect_property_change!(available_changes, true);
    expect_property_change!(auto_changes, false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_test_uses_an_independent_bus_and_name() {
    let (_first_bus, first_address) = spawn_private_bus();
    let (_second_bus, second_address) = spawn_private_bus();
    assert_ne!(
        first_address, second_address,
        "private bus addresses must be unique"
    );

    for address in [&first_address, &second_address] {
        let connection = Builder::address(address.as_str())
            .expect("valid private bus address")
            .build()
            .await
            .expect("connection to private bus");
        let shared = Arc::new(Mutex::new(seeded_state()));
        let (tx, _rx) = mpsc::channel::<Command>();
        serve(&connection, shared, tx)
            .await
            .expect("name acquisition on a fresh bus");
    }
}
