//! Integration tests for the Bluetooth auto-trust pairing agent (`AutoTrustAgent`).
//!
//! Follows repository pattern using an isolated private `dbus-daemon` session.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bt_manager::agent::{
    AgentCapabilityMode, AutoTrustAgent, DEFAULT_AGENT_PATH, run_agent_supervisor, spawn_auto_trust,
};
use tokio::sync::watch;
use zbus::zvariant::ObjectPath;

/// RAII guard that kills a spawned child process on drop.
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
        .expect("dbus-daemon must be installed to run agent integration tests");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().expect("daemon stdout"))
        .read_line(&mut address)
        .expect("read address");
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no bus address");
    (KillOnDrop(daemon), address)
}

#[tokio::test]
async fn agent_local_methods_return_correct_spec_responses() {
    let (_daemon, address) = spawn_private_bus();
    let conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("connection to private bus");

    let agent = AutoTrustAgent::new(conn);
    let dev = ObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").expect("valid dev");

    // RequestPinCode returns "0000" for legacy PIN pairing.
    let pin = agent.request_pin_code(dev.as_ref()).expect("pin code");
    assert_eq!(pin, "0000");

    // DisplayPinCode returns Ok(())
    agent
        .display_pin_code(dev.as_ref(), "0000".into())
        .expect("display pin");

    // RequestPasskey returns 0
    let passkey = agent.request_passkey(dev.as_ref()).expect("passkey");
    assert_eq!(passkey, 0);

    // DisplayPasskey returns Ok(())
    agent
        .display_passkey(dev.as_ref(), 123_456, 6)
        .expect("display passkey");

    // RequestConfirmation auto-confirms numeric comparison
    agent
        .request_confirmation(dev.as_ref(), 123_456)
        .expect("confirmation");

    // RequestAuthorization auto-authorizes pairing
    agent
        .request_authorization(dev.as_ref())
        .expect("authorization");

    // AuthorizeService auto-authorizes incoming service
    agent
        .authorize_service(dev.as_ref(), "0000110d-0000-1000-8000-00805f9b34fb".into())
        .expect("authorize service");

    // Release and Cancel return Ok(())
    agent.release().expect("release");
    agent.cancel().expect("cancel");
}

#[tokio::test]
async fn agent_dispatches_over_dbus_wire() {
    let (_daemon, address) = spawn_private_bus();
    let server_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .name("org.speaker.AgentHost")
        .expect("valid name")
        .build()
        .await
        .expect("server connection");

    let agent = AutoTrustAgent::new(server_conn.clone());
    let path = ObjectPath::try_from(DEFAULT_AGENT_PATH).expect("valid agent path");
    server_conn
        .object_server()
        .at(&path, agent)
        .await
        .expect("serve agent");

    let client_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("client connection");

    let proxy = zbus::Proxy::new_owned(
        client_conn,
        "org.speaker.AgentHost",
        DEFAULT_AGENT_PATH,
        "org.bluez.Agent1",
    )
    .await
    .expect("build proxy");

    let dev = ObjectPath::try_from("/org/bluez/hci0/dev_11_22_33_44_55_66").expect("valid dev");

    // Over-the-wire RequestPinCode call
    let reply = proxy
        .call_method("RequestPinCode", &(dev.as_ref(),))
        .await
        .expect("call RequestPinCode");
    let pin: String = reply.body().deserialize().expect("deserialize pin");
    assert_eq!(pin, "0000");

    // Over-the-wire RequestPasskey call
    let reply = proxy
        .call_method("RequestPasskey", &(dev.as_ref(),))
        .await
        .expect("call RequestPasskey");
    let passkey: u32 = reply.body().deserialize().expect("deserialize passkey");
    assert_eq!(passkey, 0);

    // Over-the-wire RequestConfirmation call
    let reply = proxy
        .call_method("RequestConfirmation", &(dev.as_ref(), 654_321_u32))
        .await
        .expect("call RequestConfirmation");
    let _: () = reply.body().deserialize().expect("deserialize unit");

    // Over-the-wire AuthorizeService call
    let reply = proxy
        .call_method(
            "AuthorizeService",
            &(dev.as_ref(), "0000110d-0000-1000-8000-00805f9b34fb"),
        )
        .await
        .expect("call AuthorizeService");
    let _: () = reply.body().deserialize().expect("deserialize unit");

    // Over-the-wire Cancel call
    let reply = proxy.call_method("Cancel", &()).await.expect("call Cancel");
    let _: () = reply.body().deserialize().expect("deserialize unit");
}

#[tokio::test]
async fn supervisor_shuts_down_on_signal() {
    let (_daemon, address) = spawn_private_bus();
    let conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("conn");

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let supervisor_task = tokio::spawn(async move {
        run_agent_supervisor(
            conn,
            DEFAULT_AGENT_PATH.to_string(),
            AgentCapabilityMode::DisplayYesNo,
            &mut shutdown_rx,
        )
        .await;
    });

    // Allow supervisor to initialize and attempt registration
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send shutdown signal
    shutdown_tx.send(true).expect("send shutdown");

    // Supervisor should terminate promptly
    tokio::time::timeout(Duration::from_secs(2), supervisor_task)
        .await
        .expect("supervisor exited within timeout")
        .expect("task join succeeded");
}

#[test]
fn spawn_auto_trust_shuts_down_cleanly() {
    let handle = spawn_auto_trust(AgentCapabilityMode::NoInputNoOutput)
        .expect("spawn auto trust background thread");
    handle.shutdown();
    std::thread::sleep(Duration::from_millis(50));
}

#[tokio::test]
async fn supervisor_handles_invalid_object_path() {
    let (_daemon, address) = spawn_private_bus();
    let conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("conn");

    let (_shutdown_tx, mut shutdown_rx) = watch::channel(false);
    // Invalid object path causes run_agent_supervisor to return immediately
    run_agent_supervisor(
        conn,
        "invalid-not-starting-with-slash".to_string(),
        AgentCapabilityMode::DisplayYesNo,
        &mut shutdown_rx,
    )
    .await;
}

#[tokio::test]
async fn unregister_agent_fails_gracefully_when_bluez_not_present() {
    let (_daemon, address) = spawn_private_bus();
    let conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("conn");

    let path = ObjectPath::try_from(DEFAULT_AGENT_PATH).expect("valid path");
    let res = bt_manager::agent::unregister_agent_on(&conn, &path).await;
    assert!(res.is_err(), "unregister without BlueZ must return error");
}
