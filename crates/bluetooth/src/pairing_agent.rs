//! Screen-owned temporary `BlueZ` pairing agent.
//!
//! [`PairingAgentHandle`] registers `/org/speaker/screen_agent` as the default
//! `org.bluez.Agent1` for the duration of exactly one user-initiated pairing,
//! captures the numeric comparison into a shared [`PairingCode`] slot for the
//! UI to display, auto-confirms the comparison (matching the appliance's
//! headless `DisplayYesNo` behaviour), and releases the default-agent role on
//! [`PairingAgentHandle::unregister`] or drop so the `bt-manager` auto-trust
//! agent reclaims it.
//!
//! The handle owns a dedicated `BlockingConnection` so the agent survives the
//! shared `DbusClient` connection's listener reconnects and has an
//! unconditional drop order. Blocking D-Bus calls must run on plain worker
//! threads, never inside a Tokio runtime context.

use std::sync::{Arc, Mutex};

use zbus::blocking::Connection as BlockingConnection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::connection::{BtAddress, BtError};
use crate::device_admin::device_address_from_path;

/// D-Bus object path of the screen-owned pairing agent.
pub const SCREEN_AGENT_PATH: &str = "/org/speaker/screen_agent";

/// `BlueZ` capability advertised by the screen-owned pairing agent.
pub const SCREEN_AGENT_CAPABILITY: &str = "DisplayYesNo";

/// Numeric comparison captured for display on the pairing screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingCode {
    /// Remote device the comparison belongs to.
    pub device: BtAddress,
    /// Six-digit numeric comparison (`0..=999_999`).
    pub passkey: u32,
}

#[zbus::proxy(
    default_service = "org.bluez",
    default_path = "/org/bluez",
    interface = "org.bluez.AgentManager1",
    gen_async = false,
    blocking_name = "BlueZAgentManagerProxyBlocking"
)]
trait BlueZAgentManager {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Owned registration of the screen pairing agent on its own connection.
#[derive(Debug)]
pub struct PairingAgentHandle {
    conn: BlockingConnection,
    codes: Arc<Mutex<Option<PairingCode>>>,
    registered: bool,
}

impl PairingAgentHandle {
    /// Registers the agent on the system bus as the default `BlueZ` agent.
    ///
    /// # Errors
    /// Returns [`BtError`] when the system bus is unreachable or `BlueZ`
    /// rejects the registration.
    pub fn register(codes: Arc<Mutex<Option<PairingCode>>>) -> Result<Self, BtError> {
        let conn = BlockingConnection::system()?;
        Self::register_on(conn, codes)
    }

    /// Registers the agent on an injected bus connection (tests, private bus).
    ///
    /// # Errors
    /// Returns [`BtError`] when the interface export or `BlueZ` registration
    /// fails; the connection then closes and `BlueZ` drops the partial agent.
    pub fn register_on(
        conn: BlockingConnection,
        codes: Arc<Mutex<Option<PairingCode>>>,
    ) -> Result<Self, BtError> {
        let path = agent_path()?;
        // Zbus starts its object-server task lazily via `tokio::spawn`, which
        // panics outside a runtime context. This runs on plain worker threads,
        // so export the interface inside zbus's global runtime block instead of
        // the blocking `object_server()` accessor.
        zbus::block_on(async {
            let server = conn.inner().object_server();
            server
                .at(
                    path.as_str(),
                    ScreenPairingAgent {
                        codes: Arc::clone(&codes),
                    },
                )
                .await
        })?;
        let manager = BlueZAgentManagerProxyBlocking::new(&conn)?;
        manager.register_agent(&path, SCREEN_AGENT_CAPABILITY)?;
        manager.request_default_agent(&path)?;
        tracing::info!(
            path = SCREEN_AGENT_PATH,
            capability = SCREEN_AGENT_CAPABILITY,
            "screen pairing agent registered as default"
        );
        Ok(Self {
            conn,
            codes,
            registered: true,
        })
    }

    /// Returns the most recently recorded pairing code, if any.
    #[must_use]
    pub fn current_code(&self) -> Option<PairingCode> {
        *self.codes.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("pairing code mutex poisoned; recovering");
            poisoned.into_inner()
        })
    }

    /// Releases the default-agent role explicitly; idempotent with `Drop`.
    pub fn unregister(mut self) {
        self.unregister_inner();
    }

    fn unregister_inner(&mut self) {
        if !self.registered {
            return;
        }
        // Mark released first so a failing call is never retried by `Drop`.
        self.registered = false;
        let path = match agent_path() {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(error = %e, "failed to build pairing agent path for release");
                return;
            }
        };
        match BlueZAgentManagerProxyBlocking::new(&self.conn)
            .and_then(|manager| manager.unregister_agent(&path))
        {
            Ok(()) => {
                tracing::info!(path = SCREEN_AGENT_PATH, "screen pairing agent released");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to unregister screen pairing agent");
            }
        }
    }
}

impl Drop for PairingAgentHandle {
    fn drop(&mut self) {
        // `BlueZ` also drops agents whose bus connection dies, so a failed
        // unregister here only delays the default-agent hand-back to the
        // `bt-manager` supervisor.
        self.unregister_inner();
    }
}

/// Builds the validated agent object path from the module constant.
fn agent_path() -> Result<OwnedObjectPath, BtError> {
    OwnedObjectPath::try_from(SCREEN_AGENT_PATH)
        .map_err(|e| BtError::dbus_with_source(format!("invalid pairing agent path: {e}"), e))
}

/// `org.bluez.Agent1` implementation recording passkeys for the UI.
#[derive(Debug)]
pub struct ScreenPairingAgent {
    codes: Arc<Mutex<Option<PairingCode>>>,
}

impl ScreenPairingAgent {
    fn lock_codes(&self) -> std::sync::MutexGuard<'_, Option<PairingCode>> {
        self.codes.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("pairing code mutex poisoned; recovering");
            poisoned.into_inner()
        })
    }

    fn record(&self, device: &ObjectPath<'_>, passkey: u32) {
        let Some(addr) = device_address_from_path(device.as_str()) else {
            // An unidentified path must not leave a previous device's code on
            // screen: drop the stale value instead of silently keeping it.
            *self.lock_codes() = None;
            tracing::warn!(device = %device, "pairing passkey not recorded: unparseable device path");
            return;
        };
        *self.lock_codes() = Some(PairingCode {
            device: addr,
            passkey,
        });
        tracing::info!(device = %addr, passkey, "pairing passkey recorded for confirmation");
    }

    fn clear(&self, device: &ObjectPath<'_>) {
        *self.lock_codes() = None;
        tracing::info!(device = %device, "pairing cancelled; passkey cleared");
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
#[expect(
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc,
    reason = "D-Bus org.bluez.Agent1 interface contract requires specific signatures"
)]
impl ScreenPairingAgent {
    pub fn release(&self) -> zbus::fdo::Result<()> {
        tracing::info!("pairing agent released by BlueZ");
        Ok(())
    }

    pub fn request_pin_code(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<String> {
        // Legacy PIN pairing has no six-digit comparison to display.
        tracing::info!(%device, "RequestPinCode requested; returning '0000'");
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
        tracing::info!(%device, "RequestPasskey called; returning 0");
        Ok(0)
    }

    pub fn display_passkey(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
        entered: u16,
    ) -> zbus::fdo::Result<()> {
        tracing::info!(%device, passkey, entered, "DisplayPasskey received");
        self.record(&device, passkey);
        Ok(())
    }

    pub fn request_confirmation(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        // Auto-confirm: the appliance shows the code and pairing proceeds
        // without a confirmation button (bt-manager parity).
        self.record(&device, passkey);
        tracing::info!(%device, "auto-confirming numeric comparison");
        Ok(())
    }

    pub fn request_authorization(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        tracing::info!(%device, "auto-authorizing pairing request");
        Ok(())
    }

    pub fn authorize_service(&self, device: ObjectPath<'_>, uuid: String) -> zbus::fdo::Result<()> {
        tracing::info!(%device, %uuid, "auto-authorizing service");
        Ok(())
    }

    pub fn cancel(&self, device: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        self.clear(&device);
        Ok(())
    }
}
