//! Integration tests for the Bluetooth auto-trust pairing agent (`AutoTrustAgent`).
//!
//! Follows repository pattern using an isolated private `dbus-daemon` session.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bt_manager::agent::{
    AgentCapabilityMode, AgentError, AutoTrustAgent, DEFAULT_AGENT_PATH, default_agent_path,
    is_already_exists, run_agent_supervisor, spawn_auto_trust_with,
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
            default_agent_path(),
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
fn spawn_auto_trust_with_custom_bus_shuts_down_cleanly() {
    let (_daemon, address) = spawn_private_bus();
    let handle = spawn_auto_trust_with(AgentCapabilityMode::DisplayYesNo, move || {
        let addr = address.clone();
        async move {
            zbus::connection::Builder::address(addr.as_str())?
                .build()
                .await
        }
    })
    .expect("spawn auto trust background thread");
    handle.shutdown();
    std::thread::sleep(Duration::from_millis(50));
}

#[test]
fn spawn_set_trusted_outside_tokio_runtime_does_not_panic() {
    let (_daemon, address) = spawn_private_bus();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(async {
        zbus::connection::Builder::address(address.as_str())
            .expect("valid address")
            .build()
            .await
            .expect("conn")
    });
    let path = zbus::zvariant::OwnedObjectPath::try_from("/org/bluez/test").unwrap();
    assert!(
        bt_manager::agent::spawn_set_trusted(conn, path).is_none(),
        "spawn_set_trusted outside runtime must return None"
    );
}

#[tokio::test]
async fn supervisor_shuts_down_when_sender_dropped() {
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
            default_agent_path(),
            AgentCapabilityMode::DisplayYesNo,
            &mut shutdown_rx,
        )
        .await;
    });

    // Drop the sender without explicitly calling .shutdown()
    drop(shutdown_tx);

    // Supervisor should detect the dropped handle and terminate without busy-spinning
    tokio::time::timeout(Duration::from_secs(2), supervisor_task)
        .await
        .expect("supervisor exited promptly on sender drop")
        .expect("task join succeeded");
}

struct MockAgentManager {
    registered: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[zbus::interface(name = "org.bluez.AgentManager1")]
#[expect(
    clippy::unused_self,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    reason = "D-Bus AgentManager1 contract requires specific signatures"
)]
impl MockAgentManager {
    fn register_agent(&self, agent: ObjectPath<'_>, capability: &str) -> zbus::fdo::Result<()> {
        let _ = (agent, capability);
        self.registered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn request_default_agent(&self, agent: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        let _ = agent;
        Ok(())
    }

    fn unregister_agent(&self, agent: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        let _ = agent;
        self.registered
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn supervisor_registers_when_bluez_becomes_available() {
    let (_daemon, address) = spawn_private_bus();
    let conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("conn");

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let supervisor_task = tokio::spawn(async move {
        bt_manager::agent::run_agent_supervisor_with_interval(
            conn,
            default_agent_path(),
            AgentCapabilityMode::DisplayYesNo,
            Duration::from_millis(50),
            &mut shutdown_rx,
        )
        .await;
    });

    // Wait 60ms; BlueZ not present yet
    tokio::time::sleep(Duration::from_millis(60)).await;

    // Now spawn MockAgentManager owning "org.bluez"
    let bluez_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .name("org.bluez")
        .expect("request name")
        .build()
        .await
        .expect("bluez conn");

    let registered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mock = MockAgentManager {
        registered: std::sync::Arc::clone(&registered),
    };
    let bluez_path = ObjectPath::try_from("/org/bluez").expect("path");
    bluez_conn
        .object_server()
        .at(&bluez_path, mock)
        .await
        .expect("export mock bluez");

    // Supervisor should detect the new owner on the next tick and register!
    let mut success = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if registered.load(std::sync::atomic::Ordering::SeqCst) {
            success = true;
            break;
        }
    }
    assert!(
        success,
        "supervisor must register once org.bluez becomes available"
    );

    // Clean up
    shutdown_tx.send(true).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), supervisor_task)
        .await
        .expect("clean exit")
        .expect("join");
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

struct MockBluezDevice {
    trusted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[zbus::interface(name = "org.bluez.Device1")]
impl MockBluezDevice {
    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.trusted.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[zbus(property)]
    fn set_trusted(&self, value: bool) {
        self.trusted
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn set_device_trusted_updates_property_on_bluez() {
    let (_daemon, address) = spawn_private_bus();
    let trusted_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let device_mock = MockBluezDevice {
        trusted: std::sync::Arc::clone(&trusted_flag),
    };
    let dev_path = ObjectPath::try_from("/org/bluez/hci0/dev_11_22_33_44_55_66").unwrap();

    let bluez_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .name("org.bluez")
        .expect("valid name")
        .build()
        .await
        .expect("bluez connection");

    bluez_conn
        .object_server()
        .at(&dev_path, device_mock)
        .await
        .expect("export device");

    let client_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("client connection");

    assert!(!trusted_flag.load(std::sync::atomic::Ordering::SeqCst));

    bt_manager::agent::set_device_trusted(&client_conn, &dev_path)
        .await
        .expect("set_device_trusted must succeed");

    assert!(trusted_flag.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn spawn_set_trusted_in_tokio_runtime_marks_device_trusted() {
    let (_daemon, address) = spawn_private_bus();
    let trusted_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let device_mock = MockBluezDevice {
        trusted: std::sync::Arc::clone(&trusted_flag),
    };
    let dev_path =
        zbus::zvariant::OwnedObjectPath::try_from("/org/bluez/hci0/dev_11_22_33_44_55_66").unwrap();

    let bluez_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .name("org.bluez")
        .expect("valid name")
        .build()
        .await
        .expect("bluez connection");

    bluez_conn
        .object_server()
        .at(dev_path.as_ref(), device_mock)
        .await
        .expect("export device");

    let client_conn = zbus::connection::Builder::address(address.as_str())
        .expect("valid address")
        .build()
        .await
        .expect("client connection");

    let join_handle = bt_manager::agent::spawn_set_trusted(client_conn, dev_path)
        .expect("must return JoinHandle inside Tokio runtime");
    let _ = join_handle.await;

    let mut success = false;
    for _ in 0..50 {
        if trusted_flag.load(std::sync::atomic::Ordering::SeqCst) {
            success = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        success,
        "spawn_set_trusted must asynchronously set Trusted property to true"
    );
}

#[test]
fn capability_mode_as_str_and_display() {
    assert_eq!(
        AgentCapabilityMode::DisplayYesNo.as_capability_str(),
        "DisplayYesNo"
    );
    assert_eq!(
        AgentCapabilityMode::NoInputNoOutput.as_capability_str(),
        "NoInputNoOutput"
    );
    assert_eq!(AgentCapabilityMode::DisplayYesNo.as_str(), "display-yes-no");
    assert_eq!(
        AgentCapabilityMode::NoInputNoOutput.as_str(),
        "no-input-no-output"
    );
    assert_eq!(
        format!("{}", AgentCapabilityMode::DisplayYesNo),
        "display-yes-no"
    );
    assert_eq!(
        format!("{}", AgentCapabilityMode::NoInputNoOutput),
        "no-input-no-output"
    );
}

#[test]
fn capability_mode_from_str() {
    assert_eq!(
        "display-yes-no".parse::<AgentCapabilityMode>().unwrap(),
        AgentCapabilityMode::DisplayYesNo
    );
    assert_eq!(
        "displayyesno".parse::<AgentCapabilityMode>().unwrap(),
        AgentCapabilityMode::DisplayYesNo
    );
    assert_eq!(
        "auto".parse::<AgentCapabilityMode>().unwrap(),
        AgentCapabilityMode::DisplayYesNo
    );
    assert_eq!(
        "no-input-no-output".parse::<AgentCapabilityMode>().unwrap(),
        AgentCapabilityMode::NoInputNoOutput
    );
    assert_eq!(
        "strict".parse::<AgentCapabilityMode>().unwrap(),
        AgentCapabilityMode::NoInputNoOutput
    );
    let _ = "invalid-mode".parse::<AgentCapabilityMode>().unwrap_err();
}

#[test]
fn capability_mode_serde() {
    let json = serde_json::to_string(&AgentCapabilityMode::DisplayYesNo).unwrap();
    assert_eq!(json, "\"display-yes-no\"");
    let parsed: AgentCapabilityMode = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, AgentCapabilityMode::DisplayYesNo);

    let json = serde_json::to_string(&AgentCapabilityMode::NoInputNoOutput).unwrap();
    assert_eq!(json, "\"no-input-no-output\"");
    let parsed: AgentCapabilityMode = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, AgentCapabilityMode::NoInputNoOutput);
}

#[test]
fn default_agent_path_is_valid() {
    assert_eq!(default_agent_path().as_str(), DEFAULT_AGENT_PATH);
}

#[test]
fn agent_error_predicates_and_display() {
    let err = AgentError::spawn(std::io::Error::other("thread spawn failed"));
    assert!(err.is_spawn());
    assert_eq!(
        err.summary(),
        "failed to spawn auto-trust agent thread: thread spawn failed"
    );
    assert!(std::error::Error::source(&err).is_some());
}

#[test]
fn already_exists_detection() {
    let err_already = zbus::Error::Address("org.bluez.Error.AlreadyExists".into());
    assert!(is_already_exists(&err_already));

    let other_err = zbus::Error::Address("other error".into());
    assert!(!is_already_exists(&other_err));
}
