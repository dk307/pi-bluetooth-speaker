//! D-Bus control plane for the autobrightness daemon.
//!
//! The daemon is the sole writer of the backlight sysfs node. Clients (the
//! brightness overlay in `screen-manager`) send commands to this interface;
//! its five properties are emitted with `PropertiesChanged` whenever the tick
//! loop publishes a new state.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface, proxy};

use crate::daemon::Command;
use crate::error::AutobrightError;

/// Well-known bus name owned by the daemon.
pub const DBUS_NAME: &str = "org.speaker.Autobright";
/// Object path exposing `org.speaker.Autobright1`.
pub const DBUS_PATH: &str = "/org/speaker/autobright";

/// Immutable snapshot of the daemon state published over D-Bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PublishedState {
    /// Whether automatic brightness writes are enabled.
    pub auto_enabled: bool,
    /// Perceptual brightness percentage in `0.0..=100.0`.
    pub brightness_percent: f64,
    /// Last successfully sampled ambient illuminance in lux.
    pub ambient_lux: f64,
    /// Manual gamma bias (`1.0` means no bias).
    pub manual_gamma: f64,
    /// Whether the daemon has completed its first hardware read.
    pub available: bool,
}

impl Default for PublishedState {
    fn default() -> Self {
        Self {
            auto_enabled: true,
            brightness_percent: 0.0,
            ambient_lux: 0.0,
            manual_gamma: 1.0,
            available: false,
        }
    }
}

/// The zbus interface implementation for `org.speaker.Autobright1`.
#[derive(Debug)]
pub struct AutobrightInterface {
    state: Arc<Mutex<PublishedState>>,
    commands: Sender<Command>,
}

impl AutobrightInterface {
    /// Builds an interface backed by shared state and a tick-loop command queue.
    #[must_use]
    pub const fn new(state: Arc<Mutex<PublishedState>>, commands: Sender<Command>) -> Self {
        Self { state, commands }
    }

    fn snapshot(&self) -> PublishedState {
        match self.state.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => {
                warn!("D-Bus state mutex poisoned; recovering stored state");
                *poisoned.into_inner()
            }
        }
    }

    fn update(&self, mutate: impl FnOnce(&mut PublishedState)) {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("D-Bus state mutex poisoned; recovering stored state");
                poisoned.into_inner()
            }
        };
        mutate(&mut guard);
    }
}

#[interface(name = "org.speaker.Autobright1")]
impl AutobrightInterface {
    /// Sets manual brightness and leaves auto mode.
    ///
    /// The overlay sends only this method on a slider drag, so the daemon
    /// suppresses ambient writes until `SetAutoEnabled(true)`.
    async fn set_manual_percent(
        &self,
        percent: f64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
            return Err(fdo::Error::InvalidArgs(format!(
                "percent {percent} out of range 0..100"
            )));
        }
        self.commands
            .send(Command::SetManualPercent(percent))
            .map_err(|e| fdo::Error::Failed(format!("command queue closed: {e}")))?;
        // Optimistic publish: the tick loop reconciles the real value within
        // one tick and emits a corrected change signal. A manual slider drag
        // always leaves auto mode; SetAutoEnabled(true) is the only way back.
        self.update(|state| {
            state.brightness_percent = percent;
            state.auto_enabled = false;
        });
        self.brightness_percent_changed(&emitter).await?;
        self.auto_enabled_changed(&emitter).await?;
        info!(percent, auto_enabled = false, "SetManualPercent");
        Ok(())
    }

    /// Enables or suppresses automatic brightness writes.
    async fn set_auto_enabled(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.commands
            .send(Command::SetAutoEnabled(enabled))
            .map_err(|e| fdo::Error::Failed(format!("command queue closed: {e}")))?;
        self.update(|state| state.auto_enabled = enabled);
        self.auto_enabled_changed(&emitter).await?;
        info!(auto_enabled = enabled, "SetAutoEnabled");
        Ok(())
    }

    /// Clears the manual gamma bias without snapping current brightness.
    fn reset_manual_bias(&self) -> fdo::Result<()> {
        self.commands
            .send(Command::ResetManualBias)
            .map_err(|e| fdo::Error::Failed(format!("command queue closed: {e}")))?;
        info!("ResetManualBias");
        Ok(())
    }

    /// Returns `(auto_enabled, brightness_percent, ambient_lux, manual_gamma)`.
    fn get_state(&self) -> (bool, f64, f64, f64) {
        let state = self.snapshot();
        (
            state.auto_enabled,
            state.brightness_percent,
            state.ambient_lux,
            state.manual_gamma,
        )
    }

    /// Whether automatic brightness writes are enabled.
    #[zbus(property)]
    fn auto_enabled(&self) -> bool {
        self.snapshot().auto_enabled
    }

    /// Current perceptual brightness percentage.
    #[zbus(property)]
    fn brightness_percent(&self) -> f64 {
        self.snapshot().brightness_percent
    }

    /// Last sampled ambient illuminance in lux.
    #[zbus(property)]
    fn ambient_lux(&self) -> f64 {
        self.snapshot().ambient_lux
    }

    /// Current manual gamma bias.
    #[zbus(property)]
    fn manual_gamma(&self) -> f64 {
        self.snapshot().manual_gamma
    }

    /// Whether the daemon has completed its first hardware read.
    #[zbus(property)]
    fn available(&self) -> bool {
        self.snapshot().available
    }
}

/// Exports the interface and claims the well-known bus name.
///
/// # Errors
/// Returns [`AutobrightError`] when object export, name acquisition, or the
/// underlying D-Bus connection fails.
pub async fn serve(
    connection: &Connection,
    state: Arc<Mutex<PublishedState>>,
    commands: Sender<Command>,
) -> Result<(), AutobrightError> {
    connection
        .object_server()
        .at(DBUS_PATH, AutobrightInterface::new(state, commands))
        .await?;

    let name = zbus::names::WellKnownName::try_from(DBUS_NAME)
        .map_err(|e| AutobrightError::config(format!("invalid D-Bus name: {e}")))?;
    let fdo = fdo::DBusProxy::new(connection).await?;
    let reply = fdo
        .request_name(name, fdo::RequestNameFlags::DoNotQueue.into())
        .await?;
    if reply != fdo::RequestNameReply::PrimaryOwner {
        return Err(AutobrightError::config(format!(
            "D-Bus name {DBUS_NAME} is already owned"
        )));
    }
    info!(
        name = DBUS_NAME,
        path = DBUS_PATH,
        "D-Bus interface exported"
    );
    Ok(())
}

/// Forwards new published states as `PropertiesChanged` signals.
///
/// Runs as a dedicated async task so the tick loop never performs D-Bus I/O.
pub async fn publish_loop(
    connection: Connection,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<PublishedState>,
) {
    let mut previous = PublishedState::default();
    while let Some(next) = updates.recv().await {
        if let Err(e) = emit_changes(&connection, previous, next).await {
            warn!(error = %e, "failed to emit PropertiesChanged");
        }
        previous = next;
    }
}

/// Emits `PropertiesChanged` for every field that changed between snapshots.
///
/// # Errors
/// Returns [`AutobrightError`] when the interface reference cannot be
/// resolved or a signal cannot be emitted.
pub async fn emit_changes(
    connection: &Connection,
    previous: PublishedState,
    next: PublishedState,
) -> Result<(), AutobrightError> {
    if previous == next {
        return Ok(());
    }
    let interface_ref = connection
        .object_server()
        .interface::<_, AutobrightInterface>(DBUS_PATH)
        .await?;
    let interface = interface_ref.get_mut().await;
    let emitter = interface_ref.signal_emitter();

    if previous.auto_enabled != next.auto_enabled {
        interface.auto_enabled_changed(emitter).await?;
    }
    if float_changed(previous.brightness_percent, next.brightness_percent) {
        interface.brightness_percent_changed(emitter).await?;
    }
    if float_changed(previous.ambient_lux, next.ambient_lux) {
        interface.ambient_lux_changed(emitter).await?;
    }
    if float_changed(previous.manual_gamma, next.manual_gamma) {
        interface.manual_gamma_changed(emitter).await?;
    }
    if previous.available != next.available {
        interface.available_changed(emitter).await?;
    }
    Ok(())
}

fn float_changed(previous: f64, next: f64) -> bool {
    previous.to_bits() != next.to_bits()
}

/// Client proxy used by UI consumers and D-Bus integration tests.
#[proxy(
    interface = "org.speaker.Autobright1",
    default_path = "/org/speaker/autobright",
    default_service = "org.speaker.Autobright",
    gen_blocking = false
)]
pub trait Autobright {
    /// Sets manual brightness percentage.
    fn set_manual_percent(&self, percent: f64) -> zbus::Result<()>;

    /// Enables or disables automatic brightness.
    fn set_auto_enabled(&self, enabled: bool) -> zbus::Result<()>;

    /// Clears the manual gamma bias.
    fn reset_manual_bias(&self) -> zbus::Result<()>;

    /// Returns the full `(auto, percent, lux, gamma)` state tuple.
    fn get_state(&self) -> zbus::Result<(bool, f64, f64, f64)>;

    /// Reads the `AutoEnabled` property.
    #[zbus(property)]
    fn auto_enabled(&self) -> zbus::Result<bool>;

    /// Reads the `BrightnessPercent` property.
    #[zbus(property)]
    fn brightness_percent(&self) -> zbus::Result<f64>;

    /// Reads the `AmbientLux` property.
    #[zbus(property)]
    fn ambient_lux(&self) -> zbus::Result<f64>;

    /// Reads the `ManualGamma` property.
    #[zbus(property)]
    fn manual_gamma(&self) -> zbus::Result<f64>;

    /// Reads the `Available` property.
    #[zbus(property)]
    fn available(&self) -> zbus::Result<bool>;
}
