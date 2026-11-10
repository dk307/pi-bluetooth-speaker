//! D-Bus API for audio-manager (org.speaker.AudioManager1).

use std::sync::Arc;

use tracing::{debug, info, warn};
use zbus::{interface, Connection};
use zbus::fdo::Error as ZBusError;

use crate::amp_backend::AmpBackend;
use crate::amp_state::AmpState;
use amp_curve::PERCENT_TO_RAW;
use crate::state_file::StateFile;

/// The D-Bus interface implementation.
pub struct AudioManagerInterface {
    state: Arc<tokio::sync::Mutex<AmpState>>,
    backend: Arc<dyn AmpBackend>,
    state_file: StateFile,
}

impl AudioManagerInterface {
    pub fn new(
        state: Arc<tokio::sync::Mutex<AmpState>>,
        backend: Arc<dyn AmpBackend>,
        state_file: StateFile,
    ) -> Self {
        Self { state, backend, state_file }
    }
}

/// The zbus interface definition for org.speaker.AudioManager1.
#[interface(name = "org.speaker.AudioManager1")]
impl AudioManagerInterface {
    /// Set the volume level as a percentage (0..100).
    /// Maps to the Merus A.Mstr Vol register via the perceptual curve.
    /// If muted, stores the level but does not apply to hardware.
    pub async fn set_level(&self, percent: u8) -> Result<(), ZBusError> {
        if percent > 100 {
            return Err(ZBusError::Failed(format!("percent {} out of range 0..100", percent)));
        }

        let mut state_guard = self.state.lock().await;
        let new_state = state_guard.with_percent(percent, &PERCENT_TO_RAW);

        // Write to hardware (only if not muted; if muted, backend still receives level but state.muted=true)
        let applied = self.backend.write_state(&new_state).await
            .map_err(|e| ZBusError::Failed(e.to_string()))?;

        // Commit state
        *state_guard = applied;

        // Persist
        if let Err(e) = self.state_file.save(&applied).await {
            warn!(error = %e, "Failed to persist state after SetLevel");
        }

        info!(percent, hw_level = applied.hardware_level(), muted = applied.muted, "SetLevel");
        Ok(())
    }

    /// Set mute state.
    /// true: write raw 0 (hardware mute), remember level.
    /// false: restore remembered level.
    pub async fn set_mute(&self, muted: bool) -> Result<(), ZBusError> {
        let mut state_guard = self.state.lock().await;
        let new_state = if muted { state_guard.mute() } else { state_guard.unmute() };

        let applied = self.backend.write_state(&new_state).await
            .map_err(|e| ZBusError::Failed(e.to_string()))?;

        // Update only the mute flag in the interface's logical state; keep the logical level_raw.
        // The backend's applied state contains the hardware level (0 when muted),
        // but our logical state must remember the pre-mute level.
        state_guard.muted = applied.muted;

        if let Err(e) = self.state_file.save(&applied).await {
            warn!(error = %e, "Failed to persist state after SetMute");
        }

        info!(muted = applied.muted, hw_level = applied.hardware_level(), "SetMute");
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