//! Pluggable source control adapters for pausing displaced audio sources.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use speaker_mpd::{MpdDevice, MpdError, MpdHost, MpdPort};
use tracing::debug;
use zbus::{Connection, Proxy};

use crate::sources::registry::{ControlSpec, SourceId, SourceRegistry};

/// Default timeout for remote pause/yield commands so a hung source cannot stall routing.
pub const SOURCE_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

const BLUETOOTH_DBUS_DEST: &str = "org.speaker.Bluetooth";
const BLUETOOTH_DBUS_PATH: &str = "/org/speaker/bluetooth";
const BLUETOOTH_DBUS_IFACE: &str = "org.speaker.Bluetooth1";

/// Error returned by [`SourceControl::pause`].
#[derive(Debug, thiserror::Error)]
pub enum SourceControlError {
    /// MPD TCP control error (`speaker-mpd`).
    #[error("MPD control error: {0}")]
    Mpd(#[from] MpdError),
    /// D-Bus control error (`bt-manager`).
    #[error("D-Bus control error: {0}")]
    DBus(#[from] zbus::Error),
    /// Operation exceeded bounded timeout.
    #[error("Source control timed out: {0}")]
    Timeout(Box<str>),
    /// Injected mock error.
    #[error("Mock source control error: {0}")]
    Mock(Box<str>),
}

/// Abstract pause/yield interface for multi-source handovers.
#[expect(
    clippy::double_must_use,
    reason = "async-trait injects #[must_use] on every desugared boxed-future signature"
)]
#[async_trait]
pub trait SourceControl: Send + Sync {
    /// Requests that `source_id` pause or yield playback.
    async fn pause(&self, source_id: &SourceId) -> Result<(), SourceControlError>;
}

#[derive(Debug, Clone)]
enum EndpointAdapter {
    BluetoothDbus,
    Mpd(MpdDevice),
    Noop,
}

/// Companion controller for [`SourceController::new_mocked`].
#[derive(Debug, Clone, Default)]
pub struct MockSourceControlCtrl {
    inner: Arc<Mutex<MockControlState>>,
}

#[derive(Debug, Default)]
struct MockControlState {
    paused: Vec<SourceId>,
    fail_for: HashSet<SourceId>,
    fail_all: bool,
}

impl MockSourceControlCtrl {
    fn lock(&self) -> std::sync::MutexGuard<'_, MockControlState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns all pause calls recorded in order.
    #[must_use]
    pub fn paused_sources(&self) -> Vec<SourceId> {
        self.lock().paused.clone()
    }

    /// Clears recorded pause calls.
    pub fn clear_paused(&self) {
        self.lock().paused.clear();
    }

    /// Configures `source_id` to fail on subsequent `pause()` calls.
    pub fn set_fail_for(&self, source_id: &SourceId, fail: bool) {
        let mut g = self.lock();
        if fail {
            g.fail_for.insert(source_id.clone());
        } else {
            g.fail_for.remove(source_id);
        }
    }

    /// Configures all `pause()` calls to fail when `fail == true`.
    pub fn set_fail_all(&self, fail: bool) {
        self.lock().fail_all = fail;
    }
}

/// Concrete [`SourceControl`] implementation with real and mock backends.
#[derive(Debug, Clone)]
pub struct SourceController {
    backend: ControlBackend,
}

#[derive(Debug, Clone)]
enum ControlBackend {
    Real {
        adapters: HashMap<SourceId, EndpointAdapter>,
        dbus_conn: Option<Connection>,
        timeout: Duration,
    },
    Mock(Arc<Mutex<MockControlState>>),
}

impl SourceController {
    /// Builds a real source controller from `registry` and an optional system/private D-Bus connection.
    #[must_use]
    pub fn from_registry(registry: &SourceRegistry, dbus_conn: Option<Connection>) -> Self {
        Self::from_registry_with_timeout(registry, dbus_conn, SOURCE_CONTROL_TIMEOUT)
    }

    /// Builds a real source controller with a custom per-call timeout.
    #[must_use]
    pub fn from_registry_with_timeout(
        registry: &SourceRegistry,
        dbus_conn: Option<Connection>,
        timeout: Duration,
    ) -> Self {
        let mut adapters = HashMap::with_capacity(registry.definitions().len());
        for def in registry.definitions() {
            let adapter = match &def.control {
                ControlSpec::BluetoothDbus => EndpointAdapter::BluetoothDbus,
                ControlSpec::MpdTcp { host, port } => {
                    EndpointAdapter::Mpd(mpd_device_for(host, *port, timeout))
                }
                ControlSpec::None => EndpointAdapter::Noop,
            };
            adapters.insert(def.id.clone(), adapter);
        }
        Self {
            backend: ControlBackend::Real {
                adapters,
                dbus_conn,
                timeout,
            },
        }
    }

    /// Creates a deterministic mock controller for host integration tests.
    #[must_use]
    pub fn new_mocked() -> (Self, MockSourceControlCtrl) {
        let ctrl = MockSourceControlCtrl::default();
        let dev = Self {
            backend: ControlBackend::Mock(Arc::clone(&ctrl.inner)),
        };
        (dev, ctrl)
    }
}

fn mpd_device_for(host: &MpdHost, port: MpdPort, timeout: Duration) -> MpdDevice {
    MpdDevice::with_endpoint(host.clone(), port).with_timeout(timeout)
}

#[async_trait]
impl SourceControl for SourceController {
    async fn pause(&self, source_id: &SourceId) -> Result<(), SourceControlError> {
        match &self.backend {
            ControlBackend::Mock(state) => {
                let mut g = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if g.fail_all || g.fail_for.contains(source_id) {
                    return Err(SourceControlError::Mock(
                        format!("mock pause failure for '{source_id}'").into(),
                    ));
                }
                g.paused.push(source_id.clone());
                Ok(())
            }
            ControlBackend::Real {
                adapters,
                dbus_conn,
                timeout,
            } => {
                let Some(adapter) = adapters.get(source_id) else {
                    debug!(%source_id, "No control adapter registered for source");
                    return Ok(());
                };
                match adapter {
                    EndpointAdapter::Noop => Ok(()),
                    EndpointAdapter::Mpd(mpd) => {
                        mpd.pause().await?;
                        Ok(())
                    }
                    EndpointAdapter::BluetoothDbus => {
                        let Some(conn) = dbus_conn.as_ref() else {
                            return Err(SourceControlError::Timeout(
                                "D-Bus connection unavailable for Bluetooth PauseActive".into(),
                            ));
                        };
                        pause_bluetooth_dbus(conn, *timeout).await
                    }
                }
            }
        }
    }
}

async fn pause_bluetooth_dbus(
    conn: &Connection,
    timeout: Duration,
) -> Result<(), SourceControlError> {
    let proxy = tokio::time::timeout(
        timeout,
        Proxy::new(
            conn,
            BLUETOOTH_DBUS_DEST,
            BLUETOOTH_DBUS_PATH,
            BLUETOOTH_DBUS_IFACE,
        ),
    )
    .await
    .map_err(|elapsed| {
        SourceControlError::Timeout(format!("Bluetooth proxy build timed out: {elapsed}").into())
    })??;

    tokio::time::timeout(timeout, proxy.call_method("PauseActive", &()))
        .await
        .map_err(|elapsed| {
            SourceControlError::Timeout(
                format!("Bluetooth PauseActive timed out: {elapsed}").into(),
            )
        })??;
    Ok(())
}
