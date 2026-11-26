//! D-Bus API for audio-manager (org.speaker.AudioManager1).

use std::sync::Arc;

use tracing::{debug, info, warn};
use zbus::fdo::Error as ZBusError;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use crate::amp_backend::AmpBackend;
use crate::amp_controller::{AmpCommand, AmpCommandKind};
use crate::amp_state::AmpState;
use crate::state_file::StateFile;
use speaker_hardware::amp::PERCENT_TO_RAW;

/// The D-Bus interface implementation.
pub struct AudioManagerInterface {
    pub state: Arc<tokio::sync::Mutex<AmpState>>,
    backend: Option<Arc<dyn AmpBackend>>,
    state_file: StateFile,
    /// Phase 2 i2c mode: commands flow through the [`AmpController`] loop.
    commands: Option<tokio::sync::mpsc::Sender<AmpCommand>>,
}

impl AudioManagerInterface {
    pub fn new(
        state: Arc<tokio::sync::Mutex<AmpState>>,
        backend: Arc<dyn AmpBackend>,
        state_file: StateFile,
    ) -> Self {
        Self {
            state,
            backend: Some(backend),
            state_file,
            commands: None,
        }
    }

    /// Phase 2 i2c-mode constructor: the controller loop owns the backend;
    /// D-Bus calls are enqueued (with a reply channel) and the shared state
    /// mirror serves property reads.
    #[must_use]
    pub fn with_commands(
        mirror: Arc<tokio::sync::Mutex<AmpState>>,
        state_file: StateFile,
        commands: tokio::sync::mpsc::Sender<AmpCommand>,
    ) -> Self {
        Self {
            state: mirror,
            backend: None,
            state_file,
            commands: Some(commands),
        }
    }

    /// Send a command and await the controller's hardware result.
    async fn dispatch(&self, kind: AmpCommandKind) -> Result<(), ZBusError> {
        let Some(tx) = self.commands.as_ref() else {
            return Err(ZBusError::Failed("i2c command channel unavailable".into()));
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(AmpCommand {
            kind,
            reply: Some(reply_tx),
        })
        .await
        .map_err(|e| ZBusError::Failed(format!("controller command channel closed: {e}")))?;
        reply_rx
            .await
            .map_err(|e| ZBusError::Failed(e.to_string()))?
            .map_err(ZBusError::Failed)?;
        Ok(())
    }

    /// Core level write (D-Bus-free; tests call this directly). The zbus
    /// wrapper emits `PropertiesChanged` after this succeeds.
    pub async fn set_level_inner(&self, percent: u8) -> Result<(), ZBusError> {
        if percent > 100 {
            return Err(ZBusError::Failed(format!(
                "percent {} out of range 0..100",
                percent
            )));
        }

        // Phase 2 i2c mode: the controller owns hardware + persistence.
        if self.commands.is_some() {
            return self.dispatch(AmpCommandKind::SetLevel(percent)).await;
        }

        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| ZBusError::Failed("no backend configured".into()))?;
        let mut state_guard = self.state.lock().await;
        let new_state = state_guard.with_percent(percent, &PERCENT_TO_RAW);

        let applied = backend
            .write_state(&new_state)
            .await
            .map_err(|e| ZBusError::Failed(e.summary()))?;

        // Commit state
        *state_guard = applied;

        // Persist
        if let Err(e) = self.state_file.save(&applied).await {
            warn!(error = %e, "Failed to persist state after SetLevel");
        }

        info!(
            percent,
            hw_level = applied.hardware_level(),
            muted = applied.muted,
            "SetLevel"
        );
        Ok(())
    }

    /// Core mute write (D-Bus-free; tests call this directly). The zbus
    /// wrapper emits `PropertiesChanged` after this succeeds.
    pub async fn set_mute_inner(&self, muted: bool) -> Result<(), ZBusError> {
        // Phase 2 i2c mode: the controller owns the mute state machine.
        if self.commands.is_some() {
            return self.dispatch(AmpCommandKind::SetMute(muted)).await;
        }

        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| ZBusError::Failed("no backend configured".into()))?;
        let mut state_guard = self.state.lock().await;
        let new_state = if muted {
            state_guard.mute()
        } else {
            state_guard.unmute()
        };

        let applied = backend
            .write_state(&new_state)
            .await
            .map_err(|e| ZBusError::Failed(e.summary()))?;

        *state_guard = applied;

        if let Err(e) = self.state_file.save(&applied).await {
            warn!(error = %e, "Failed to persist state after SetMute");
        }

        info!(muted = applied.muted, level = applied.level_raw, "SetMute");
        Ok(())
    }
}

/// The zbus interface definition for org.speaker.AudioManager1.
#[interface(name = "org.speaker.AudioManager1")]
impl AudioManagerInterface {
    /// Set the volume level as a percentage (0..100).
    /// Maps to the Merus A.Mstr Vol register via the perceptual curve.
    /// If muted, the level is still applied to the mixer (silence is
    /// guaranteed by the serial mute bit), so unmute is instant.
    pub async fn set_level(
        &self,
        percent: u8,
        #[zbus(signal_emitter)] signal_emitter: SignalEmitter<'_>,
    ) -> Result<(), ZBusError> {
        // Signals are emitted only after the hardware transaction succeeded,
        // so a failed command never announces state it did not apply.
        self.set_level_inner(percent).await?;
        self.level_changed(&signal_emitter).await?;
        self.muted_changed(&signal_emitter).await?;
        Ok(())
    }

    /// Set mute state (serial mute register; the mixer level is untouched).
    pub async fn set_mute(
        &self,
        muted: bool,
        #[zbus(signal_emitter)] signal_emitter: SignalEmitter<'_>,
    ) -> Result<(), ZBusError> {
        self.set_mute_inner(muted).await?;
        self.muted_changed(&signal_emitter).await?;
        self.level_changed(&signal_emitter).await?;
        Ok(())
    }

    /// Get the current target level in percent (0..100).
    /// This is the level the user set, not necessarily what's on hardware if muted.
    #[zbus(property)]
    pub async fn level(&self) -> u8 {
        let state = self.state.lock().await;
        state.level_percent(&PERCENT_TO_RAW)
    }

    /// Get the current mute state.
    #[zbus(property)]
    pub async fn muted(&self) -> bool {
        let state = self.state.lock().await;
        state.muted
    }
}

/// Export the AudioManagerInterface on the D-Bus object path.
pub async fn export_interface(
    connection: &Connection,
    interface: AudioManagerInterface,
) -> Result<(), ZBusError> {
    let object_server = connection.object_server();
    object_server.at("/org/speaker/amp", interface).await?;
    debug!("Exported org.speaker.AudioManager1 at /org/speaker/amp");
    Ok(())
}
