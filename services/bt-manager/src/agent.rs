//! Bluetooth auto-trust pairing agent subsystem.
//!
//! Replaces the standalone legacy Python script (`bt-auto-trust-agent.py`) and
//! its systemd service (`bt-agent.service`) with a native, asynchronous pure-Rust
//! D-Bus agent running alongside the `bt-manager` monitor loop.
//!
//! # Architecture & Defect Remediation
//!
//! 1. **Non-Blocking Trust Assignment:** Trust mutations are dispatched
//!    asynchronously via `zbus` (`Properties.Set("org.bluez.Device1", "Trusted", true)`),
//!    preventing D-Bus re-entrancy deadlocks where synchronous property calls
//!    block `BlueZ`'s internal pairing state machine.
//! 2. **Raspberry Pi `BlueZ` 5.55+ Re-Pairing Fix:** Headless `NoInputNoOutput` agents
//!    exhibit a documented `BlueZ` regression where re-pairing fails silently if a
//!    mobile peer deletes ("forgets") the bond. By default, this module uses
//!    [`AgentCapabilityMode::DisplayYesNo`], which prompts `BlueZ` to dispatch
//!    `RequestConfirmation` and auto-confirms numeric comparisons.
//! 3. **Complete `org.bluez.Agent1` Handling:** Fully handles `Release`, `RequestPinCode`,
//!    `DisplayPinCode`, `RequestPasskey`, `DisplayPasskey`, `RequestConfirmation`,
//!    `RequestAuthorization`, `AuthorizeService`, and `Cancel`.
//! 4. **Scoped Path Management:** Uses `/org/speaker/agent` registered on the
//!    calling D-Bus connection name, preventing static `/test/agent` conflicts.
//! 5. **Lifecycle Supervision & Restart Resilience:** A supervisor task keeps
//!    the agent registered and automatically re-claims default status if
//!    `bluetoothd` stops or restarts.
//! 6. **Graceful Shutdown:** Controlled via cooperative channel signaling
//!    ([`AutoTrustShutdown`]) without global signal hijacking.

use std::backtrace::Backtrace;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zbus::names::InterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

/// Default D-Bus object path for the speaker auto-trust agent.
pub const DEFAULT_AGENT_PATH: &str = "/org/speaker/agent";

/// Default check and reconnection interval for the auto-trust agent supervisor (3 seconds).
pub const DEFAULT_SUPERVISOR_INTERVAL: Duration = Duration::from_secs(3);

/// Error kind for [`AgentError`].
#[derive(Debug)]
pub(crate) enum AgentErrorKind {
    /// Thread spawn failure.
    Spawn(io::Error),
}

/// Canonical error for auto-trust agent initialization failures.
#[derive(Debug)]
pub struct AgentError {
    kind: AgentErrorKind,
    backtrace: Backtrace,
}

impl AgentError {
    #[must_use]
    pub fn spawn(source: io::Error) -> Self {
        Self {
            kind: AgentErrorKind::Spawn(source),
            backtrace: Backtrace::capture(),
        }
    }

    #[must_use]
    pub const fn is_spawn(&self) -> bool {
        matches!(self.kind, AgentErrorKind::Spawn(_))
    }

    #[must_use]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    fn fmt_kind(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AgentErrorKind::Spawn(source) => {
                write!(f, "failed to spawn auto-trust agent thread: {source}")
            }
        }
    }
}

speaker_common::impl_canonical_error_display!(AgentError);

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AgentErrorKind::Spawn(source) => Some(source),
        }
    }
}

/// Returns the canonical default D-Bus [`OwnedObjectPath`] for the auto-trust agent.
#[must_use]
pub fn default_agent_path() -> OwnedObjectPath {
    match OwnedObjectPath::try_from(DEFAULT_AGENT_PATH) {
        Ok(path) => path,
        Err(_) => unreachable!("DEFAULT_AGENT_PATH is a validated constant"),
    }
}

/// `BlueZ` pairing agent capability negotiation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentCapabilityMode {
    /// Auto-accepts 6-digit SSP numeric comparisons, authorizes pairing,
    /// and authorizes Bluetooth services. Advertises `DisplayYesNo` to `BlueZ`,
    /// permanently solving the Raspberry Pi `BlueZ` 5.55+ re-pairing failure
    /// when a phone deletes a bond.
    #[default]
    DisplayYesNo,
    /// Strictly advertises `NoInputNoOutput` for headless unauthenticated pairing.
    NoInputNoOutput,
}

impl AgentCapabilityMode {
    /// Returns the `BlueZ` capability string sent to `AgentManager1.RegisterAgent`.
    #[must_use]
    pub const fn as_capability_str(self) -> &'static str {
        match self {
            Self::DisplayYesNo => "DisplayYesNo",
            Self::NoInputNoOutput => "NoInputNoOutput",
        }
    }

    /// Returns the canonical CLI / config kebab-case string slice.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DisplayYesNo => "display-yes-no",
            Self::NoInputNoOutput => "no-input-no-output",
        }
    }
}

impl Display for AgentCapabilityMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentCapabilityMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "display-yes-no" | "displayyesno" | "display_yes_no" | "auto" => Ok(Self::DisplayYesNo),
            "no-input-no-output" | "noinputnooutput" | "no_input_no_output" | "strict" => {
                Ok(Self::NoInputNoOutput)
            }
            other => Err(format!(
                "invalid capability mode '{other}': expected 'display-yes-no' or 'no-input-no-output'"
            )),
        }
    }
}

/// `BlueZ` `org.bluez.AgentManager1` D-Bus proxy interface.
#[zbus::proxy(
    default_service = "org.bluez",
    default_path = "/org/bluez",
    interface = "org.bluez.AgentManager1"
)]
pub trait AgentManager1 {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;

    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;

    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// In-process `BlueZ` `org.bluez.Agent1` D-Bus interface implementation.
#[derive(Debug, Clone)]
pub struct AutoTrustAgent {
    connection: zbus::Connection,
}

impl AutoTrustAgent {
    /// Creates a new `AutoTrustAgent` bound to the given D-Bus connection.
    #[must_use]
    pub const fn new(connection: zbus::Connection) -> Self {
        Self { connection }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
#[expect(
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc,
    reason = "D-Bus org.bluez.Agent1 interface contract requires specific signatures"
)]
impl AutoTrustAgent {
    pub fn release(&self) -> zbus::fdo::Result<()> {
        tracing::info!("BlueZ unregistered agent via Release()");
        Ok(())
    }

    pub fn request_pin_code(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<String> {
        tracing::info!(%device, "RequestPinCode called; auto-trusting and returning '0000'");
        spawn_set_trusted(self.connection.clone(), device.into());
        Ok("0000".to_string())
    }

    pub fn display_pin_code(
        &self,
        device: ObjectPath<'_>,
        pincode: String,
    ) -> zbus::fdo::Result<()> {
        let _ = pincode;
        tracing::info!(%device, "DisplayPinCode called");
        Ok(())
    }

    pub fn request_passkey(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<u32> {
        tracing::info!(%device, "RequestPasskey called; auto-trusting and returning 0");
        let _ = spawn_set_trusted(self.connection.clone(), device.into());
        Ok(0)
    }

    pub fn display_passkey(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
        entered: u16,
    ) -> zbus::fdo::Result<()> {
        let _ = passkey;
        tracing::info!(%device, entered, "DisplayPasskey called");
        Ok(())
    }

    pub fn request_confirmation(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        tracing::info!(
            %device,
            passkey,
            "RequestConfirmation called; auto-confirming numeric comparison and auto-trusting"
        );
        let _ = spawn_set_trusted(self.connection.clone(), device.into());
        Ok(())
    }

    pub fn request_authorization(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        tracing::info!(%device, "RequestAuthorization called; auto-authorizing and auto-trusting");
        let _ = spawn_set_trusted(self.connection.clone(), device.into());
        Ok(())
    }

    pub fn authorize_service(&self, device: ObjectPath<'_>, uuid: String) -> zbus::fdo::Result<()> {
        tracing::info!(
            %device,
            %uuid,
            "AuthorizeService called; auto-authorizing service UUID and auto-trusting"
        );
        let _ = spawn_set_trusted(self.connection.clone(), device.into());
        Ok(())
    }

    pub fn cancel(&self) -> zbus::fdo::Result<()> {
        tracing::info!("BlueZ canceled pairing request via Cancel()");
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BlueZOwnerState {
    Present(String),
    Absent,
    Unknown,
}

async fn get_bluez_owner(connection: &zbus::Connection) -> BlueZOwnerState {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(connection).await else {
        return BlueZOwnerState::Unknown;
    };
    let Ok(name) = zbus::names::BusName::try_from("org.bluez") else {
        return BlueZOwnerState::Unknown;
    };
    match dbus.get_name_owner(name).await {
        Ok(unique_name) => BlueZOwnerState::Present(unique_name.to_string()),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => BlueZOwnerState::Absent,
        Err(_) => BlueZOwnerState::Unknown,
    }
}

fn is_already_exists(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => name.as_str() == "org.bluez.Error.AlreadyExists",
        _ => err.to_string().contains("AlreadyExists"),
    }
}

/// Sets the `device.Trusted` property to `true` on `org.bluez`.
///
/// # Errors
/// Returns an error if the D-Bus call to `org.bluez` fails.
pub async fn set_device_trusted(
    connection: &zbus::Connection,
    device: &ObjectPath<'_>,
) -> zbus::Result<()> {
    let proxy = zbus::fdo::PropertiesProxy::builder(connection)
        .destination("org.bluez")?
        .path(device)?
        .build()
        .await?;
    let iface = InterfaceName::try_from("org.bluez.Device1")
        .map_err(|e| zbus::Error::Address(e.to_string()))?;
    proxy
        .set(iface, "Trusted", Value::Bool(true))
        .await
        .map_err(|e| zbus::Error::FDO(Box::new(e)))
}

/// Spawns a background task to set `device.Trusted = true` asynchronously.
///
/// If a Tokio runtime handle is available in the current context, the task is
/// spawned on that runtime and its [`tokio::task::JoinHandle`] is returned.
/// Otherwise, a warning is logged and `None` is returned so caller threads
/// never panic.
pub fn spawn_set_trusted(
    connection: zbus::Connection,
    device: OwnedObjectPath,
) -> Option<tokio::task::JoinHandle<()>> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        Some(handle.spawn(async move {
            match set_device_trusted(&connection, &device).await {
                Ok(()) => tracing::info!(%device, "Device successfully marked as Trusted: yes"),
                Err(e) => {
                    tracing::warn!(%device, error = %e, "Failed to set device Trusted property");
                }
            }
        }))
    } else {
        tracing::warn!(%device, "No Tokio runtime available to set device Trusted asynchronously");
        None
    }
}

/// Registers the auto-trust agent on the given D-Bus connection.
///
/// # Errors
/// Returns an error if the agent cannot be registered with `BlueZ` `AgentManager1`.
pub async fn register_agent_on(
    connection: &zbus::Connection,
    agent_path: &ObjectPath<'_>,
    mode: AgentCapabilityMode,
) -> zbus::Result<()> {
    let proxy = AgentManager1Proxy::new(connection).await?;
    proxy
        .register_agent(agent_path, mode.as_capability_str())
        .await?;
    if let Err(e) = proxy.request_default_agent(agent_path).await {
        tracing::warn!(error = %e, "Failed to request default agent status; continuing as registered agent");
    } else {
        tracing::info!(
            path = %agent_path,
            capability = mode.as_capability_str(),
            "Agent registered as BlueZ default agent"
        );
    }
    Ok(())
}

/// Unregisters the auto-trust agent on the given D-Bus connection.
///
/// # Errors
/// Returns an error if the agent unregistration fails.
pub async fn unregister_agent_on(
    connection: &zbus::Connection,
    agent_path: &ObjectPath<'_>,
) -> zbus::Result<()> {
    let proxy = AgentManager1Proxy::new(connection).await?;
    proxy.unregister_agent(agent_path).await
}

/// Shutdown handle for the background auto-trust agent worker.
#[derive(Debug, Clone)]
pub struct AutoTrustShutdown {
    tx: watch::Sender<bool>,
}

impl AutoTrustShutdown {
    /// Creates a new shutdown handle with the provided sender.
    #[must_use]
    pub const fn new(tx: watch::Sender<bool>) -> Self {
        Self { tx }
    }

    /// Triggers graceful shutdown of the auto-trust agent worker.
    pub fn shutdown(&self) {
        let _ = self.tx.send(true);
    }
}

/// Runs the supervisor loop for the auto-trust agent until shutdown is signaled.
pub async fn run_agent_supervisor(
    connection: zbus::Connection,
    agent_path: OwnedObjectPath,
    mode: AgentCapabilityMode,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    run_agent_supervisor_with_interval(
        connection,
        agent_path,
        mode,
        DEFAULT_SUPERVISOR_INTERVAL,
        shutdown_rx,
    )
    .await;
}

/// Runs the supervisor loop with a custom check interval for testing or appliance configuration.
pub async fn run_agent_supervisor_with_interval(
    connection: zbus::Connection,
    agent_path: OwnedObjectPath,
    mode: AgentCapabilityMode,
    interval: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let agent = AutoTrustAgent::new(connection.clone());
    if let Err(e) = connection.object_server().at(&agent_path, agent).await {
        tracing::error!(path = %agent_path, error = %e, "Failed to export Agent1 interface on ObjectServer");
        return;
    }

    tracing::info!(
        path = %agent_path,
        capability = mode.as_capability_str(),
        "Starting Bluetooth auto-trust pairing agent supervisor"
    );

    let mut is_registered = false;
    let mut last_owner: Option<String> = None;
    let mut check_interval = tokio::time::interval(interval);

    while !*shutdown_rx.borrow() {
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = check_interval.tick() => {
                if connection.is_closed() {
                    tracing::warn!("D-Bus connection closed; agent terminating");
                    break;
                }

                match get_bluez_owner(&connection).await {
                    BlueZOwnerState::Present(current_owner) => {
                        if last_owner.as_deref() != Some(&current_owner) {
                            if last_owner.is_some() {
                                tracing::info!(
                                    "BlueZ daemon restarted; re-registering auto-trust agent"
                                );
                            }
                            last_owner = Some(current_owner);
                            is_registered = false;
                        }
                    }
                    BlueZOwnerState::Absent => {
                        if last_owner.is_some() {
                            tracing::warn!("BlueZ daemon disconnected; agent registration dropped");
                            last_owner = None;
                            is_registered = false;
                        }
                    }
                    BlueZOwnerState::Unknown => {
                        // Transient D-Bus query failure; do not declare BlueZ disconnected
                    }
                }

                if last_owner.is_some() && !is_registered {
                    match register_agent_on(&connection, &agent_path, mode).await {
                        Ok(()) => {
                            is_registered = true;
                            tracing::info!("Auto-trust agent successfully registered with BlueZ");
                        }
                        Err(e) if is_already_exists(&e) => {
                            is_registered = true;
                            tracing::info!("Auto-trust agent already registered with BlueZ");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to register agent with BlueZ; retrying");
                        }
                    }
                }
            }
        }
    }

    tracing::info!("Auto-trust agent shutting down; unregistering from BlueZ");
    if is_registered {
        if let Err(e) = unregister_agent_on(&connection, &agent_path).await {
            tracing::warn!(path = %agent_path, error = %e, "Failed to unregister agent during shutdown");
        } else {
            tracing::info!(path = %agent_path, "Agent successfully unregistered from BlueZ");
        }
    }
    let _ = connection
        .object_server()
        .remove::<AutoTrustAgent, _>(&agent_path)
        .await;
}

/// Spawns the auto-trust agent on a background thread with its own tokio runtime and connection.
///
/// # Errors
/// Returns an [`AgentError`] if the background thread cannot be spawned.
pub fn spawn_auto_trust(mode: AgentCapabilityMode) -> Result<AutoTrustShutdown, AgentError> {
    spawn_auto_trust_with(mode, zbus::Connection::system)
}

/// Spawns the auto-trust agent on a background thread using a custom connection factory.
///
/// Enables mockable/hermetic unit and integration testing without requiring the host system bus.
///
/// # Errors
/// Returns an [`AgentError`] if the background thread cannot be spawned.
pub fn spawn_auto_trust_with<F, Fut>(
    mode: AgentCapabilityMode,
    conn_factory: F,
) -> Result<AutoTrustShutdown, AgentError>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = zbus::Result<zbus::Connection>> + Send + 'static,
{
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let handle = AutoTrustShutdown::new(shutdown_tx);

    std::thread::Builder::new()
        .name("bt-auto-trust".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to start auto-trust agent tokio runtime");
                    return;
                }
            };

            runtime.block_on(async move {
                let mut retry_delay = Duration::from_secs(1);
                while !*shutdown_rx.borrow() {
                    match conn_factory().await {
                        Ok(conn) => {
                            retry_delay = Duration::from_secs(1);
                            // Run the BlueZ 5.82 <-> bluez-alsa v5.0.0 audio transport reconciler
                            // alongside the agent supervisor on the active D-Bus connection.
                            let reconciler_handle =
                                speaker_bluetooth::spawn_bt_transport_reconciler(conn.clone());

                            run_agent_supervisor(conn, default_agent_path(), mode, &mut shutdown_rx)
                                .await;

                            reconciler_handle.abort();
                            let _ = reconciler_handle.await;
                            if *shutdown_rx.borrow() {
                                break;
                            }
                            tracing::warn!(
                                "Auto-trust agent connection dropped; reconnecting in {retry_delay:?}"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Cannot establish D-Bus connection for auto-trust agent; retrying in {retry_delay:?}"
                            );
                        }
                    }

                    tokio::select! {
                        () = tokio::time::sleep(retry_delay) => {
                            retry_delay = (retry_delay * 2).min(Duration::from_secs(10));
                        }
                        res = shutdown_rx.changed() => {
                            if res.is_err() || *shutdown_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
            });
        })
        .map_err(AgentError::spawn)?;

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let err = AgentError::spawn(io::Error::other("thread spawn failed"));
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
}
