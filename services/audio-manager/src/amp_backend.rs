//! ALSA backend for Merus MA12070P amplifier control.
//!
//! Provides `AmpBackend` trait with production `RealAmpBackend` and
//! test `MockAmpBackend` (via `new_mocked()` pair).

use std::sync::{Arc, Mutex};

use alsa::mixer::{Mixer, SelemChannelId, SelemId};
use anyhow::Context;
use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::amp_state::AmpState;
use crate::error::{AmpError, AmpResult};

/// Trait abstracting amplifier hardware operations.
/// Production uses `RealAmpBackend`; tests inject `MockAmpBackend`.
#[async_trait]
pub trait AmpBackend: Send + Sync {
    /// Write the complete amp state (level + mute) to hardware.
    /// Returns the state that was actually written (may differ if clamped).
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState>;

    /// Read the current hardware state (for validation/diagnostics).
    async fn read_state(&self) -> AmpResult<AmpState>;

    /// Validate that the hardware is present and responsive.
    async fn validate(&self) -> AmpResult<()>;

    /// Get the ALSA card name this backend operates on.
    fn card_name(&self) -> &str;
}

/// Production ALSA backend for the Merus MA12070P amplifier.
pub struct RealAmpBackend {
    card_name: String,
    // Mixer operations are blocking; we run them in a blocking task.
    // The mixer handle is kept open for the backend lifetime.
    mixer: Arc<Mutex<Mixer>>,
}

impl RealAmpBackend {
    /// Open the ALSA mixer for the given card and validate the A.Mstr Vol control.
    pub async fn open(card_name: &str) -> AmpResult<Self> {
        let card_name = card_name.to_string();
        let card_name_for_closure = card_name.clone();
        let mixer = tokio::task::spawn_blocking(move || -> AmpResult<Mixer> {
            let mixer = Mixer::new(&card_name_for_closure, false)
                .map_err(AmpError::Alsa)?;
            Ok(mixer)
        })
        .await
        .context("Mixer open task panicked")??;

        // Validate the control exists and has expected range
        Self::validate_control(&mixer, &card_name).await?;

        info!(card = %card_name, "RealAmpBackend opened successfully");
        Ok(Self {
            card_name,
            mixer: Arc::new(Mutex::new(mixer)),
        })
    }

    /// Validate the A.Mstr Vol control exists and has range 0..144 (blocking).
    fn validate_control_blocking(mixer: &Mixer, card_name: &str) -> AmpResult<()> {
        let selem_id = SelemId::new("A.Mstr Vol", 0);
        let elem = mixer.find_selem(&selem_id)
            .ok_or_else(|| AmpError::control_error(format!("A.Mstr Vol control not found on card {}", card_name)))?;

        let (min, max) = elem.get_playback_volume_range();
        if min != 0 || max != 144 {
            return Err(AmpError::control_error(format!(
                "A.Mstr Vol range mismatch: expected 0..144, got {}..{}",
                min, max
            )));
        }

        debug!(card = %card_name, min, max, "A.Mstr Vol control validated");
        Ok(())
    }

    /// Validate the A.Mstr Vol control exists and has range 0..144 (async wrapper).
    async fn validate_control(mixer: &Mixer, card_name: &str) -> AmpResult<()> {
        Self::validate_control_blocking(mixer, card_name)
    }

    /// Write volume to the mixer (blocking operation).
    fn write_volume_blocking(mixer: &Mixer, level_raw: u16) -> AmpResult<()> {
        let selem_id = SelemId::new("A.Mstr Vol", 0);
        let elem = mixer.find_selem(&selem_id)
            .ok_or_else(|| AmpError::control_error("A.Mstr Vol control disappeared"))?;

        // Write to both channels (mono control, but write both for safety)
        elem.set_playback_volume_all(level_raw as i64)
            .map_err(AmpError::Alsa)?;

        // Verify write
        let written = elem.get_playback_volume(SelemChannelId::FrontLeft)?;
        if written != level_raw as i64 {
            warn!(requested = level_raw, written, "Mixer write verification mismatch");
        }

        Ok(())
    }
}

#[async_trait]
impl AmpBackend for RealAmpBackend {
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState> {
        let mixer = self.mixer.clone();
        let level = if state.muted { 0 } else { state.level_raw };

        tokio::task::spawn_blocking(move || {
            let guard = mixer.lock().map_err(|e| AmpError::internal(e.to_string()))?;
            Self::write_volume_blocking(&*guard, level)
        })
        .await
        .context("Write volume task panicked")??;

        // Return the state that was actually applied
        Ok(AmpState { level_raw: level, muted: state.muted })
    }

    async fn read_state(&self) -> AmpResult<AmpState> {
        let mixer = self.mixer.clone();
        tokio::task::spawn_blocking(move || {
            let mixer = mixer.lock().map_err(|e| AmpError::internal(e.to_string()))?;
            let selem_id = SelemId::new("A.Mstr Vol", 0);
            let elem = mixer.find_selem(&selem_id)
                .ok_or_else(|| AmpError::control_error("A.Mstr Vol control disappeared"))?;
            let raw = elem.get_playback_volume(SelemChannelId::FrontLeft)? as u16;
            // We can't read mute from hardware (no mute switch), so infer from level
            let muted = raw == 0;
            Ok(AmpState { level_raw: raw, muted })
        })
        .await
        .context("Read state task panicked")?
    }

    async fn validate(&self) -> AmpResult<()> {
        let mixer = self.mixer.clone();
        let card_name = self.card_name.clone();
        tokio::task::spawn_blocking(move || {
            let guard = mixer.lock().map_err(|e| AmpError::internal(e.to_string()))?;
            Self::validate_control_blocking(&*guard, &card_name)
        })
        .await
        .context("Validate task panicked")?
    }

    fn card_name(&self) -> &str {
        &self.card_name
    }
}

/// Shared mock state for `MockAmpBackend` and `MockAmpCtrl`.
#[derive(Debug, Default, Clone)]
struct MockState {
    state: AmpState,
    last_write: Option<AmpState>,
    should_fail_write: bool,
    should_fail_read: bool,
    should_fail_validate: bool,
    validate_called: u32,
    write_called: u32,
    read_called: u32,
}

/// Controller for `MockAmpBackend` — used by tests to inject faults and inspect calls.
#[derive(Debug, Clone)]
pub struct MockAmpCtrl {
    inner: Arc<Mutex<MockState>>,
}

impl MockAmpCtrl {
    /// Set whether the next write should fail.
    pub fn set_write_fail(&self, fail: bool) {
        self.inner.lock().unwrap().should_fail_write = fail;
    }

    /// Set whether the next read should fail.
    pub fn set_read_fail(&self, fail: bool) {
        self.inner.lock().unwrap().should_fail_read = fail;
    }

    /// Set whether validation should fail.
    pub fn set_validate_fail(&self, fail: bool) {
        self.inner.lock().unwrap().should_fail_validate = fail;
    }

    /// Get the last state that was written (for assertion).
    pub fn last_write(&self) -> Option<AmpState> {
        self.inner.lock().unwrap().last_write
    }

    /// Get the current mocked state.
    pub fn current_state(&self) -> AmpState {
        self.inner.lock().unwrap().state
    }

    /// Set the mocked state directly (for test setup).
    pub fn set_state(&self, state: AmpState) {
        self.inner.lock().unwrap().state = state;
    }

    /// Get call counts for verification.
    pub fn call_counts(&self) -> (u32, u32, u32) {
        let s = self.inner.lock().unwrap();
        (s.validate_called, s.write_called, s.read_called)
    }
}

/// Mock ALSA backend for testing.
#[derive(Debug, Clone)]
pub struct MockAmpBackend {
    inner: Arc<Mutex<MockState>>,
}

impl MockAmpBackend {
    /// Create a new mock backend and its controller pair.
    pub fn new_mocked() -> (Self, MockAmpCtrl) {
        let inner = Arc::new(Mutex::new(MockState::default()));
        let backend = Self { inner: inner.clone() };
        let ctrl = MockAmpCtrl { inner };
        (backend, ctrl)
    }
}

#[async_trait]
impl AmpBackend for MockAmpBackend {
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState> {
        let mut guard = self.inner.lock().map_err(|e| AmpError::internal(e.to_string()))?;
        guard.write_called += 1;

        if guard.should_fail_write {
            guard.should_fail_write = false;
            return Err(AmpError::internal("mock write failure"));
        }

        let level = if state.muted { 0 } else { state.level_raw };
        let applied = AmpState { level_raw: level, muted: state.muted };
        guard.state = applied;
        guard.last_write = Some(applied);
        Ok(applied)
    }

    async fn read_state(&self) -> AmpResult<AmpState> {
        let mut guard = self.inner.lock().map_err(|e| AmpError::internal(e.to_string()))?;
        guard.read_called += 1;

        if guard.should_fail_read {
            guard.should_fail_read = false;
            return Err(AmpError::internal("mock read failure"));
        }

        Ok(guard.state)
    }

    async fn validate(&self) -> AmpResult<()> {
        let mut guard = self.inner.lock().map_err(|e| AmpError::internal(e.to_string()))?;
        guard.validate_called += 1;

        if guard.should_fail_validate {
            return Err(AmpError::hardware_validation("mock validation failure"));
        }

        Ok(())
    }

    fn card_name(&self) -> &str {
        "mock"
    }
}