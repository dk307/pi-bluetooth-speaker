//! ALSA backend for Merus MA12070P amplifier control.
//!
//! Provides `AmpBackend` trait with production `RealAmpBackend` and
//! test `MockAmpBackend` (via `new_mocked()` pair).
//!
//! Datasheet reference:
//! <https://www.infineon.com/assets/row/public/documents/24/49/infineon-ma12070p-datasheet-en.pdf>
//! (V1.1, 2022-02-09). Volume register map: §8.5 (`0x40`/`0x41` master
//! integer/fractional dB, `0x42`..`0x45` per-channel; Table 8-9 maps the
//! 10-bit setting to +24.00 .. −144.00 dB at 0.25 dB granularity). The
//! kernel ALSA control (`A.Mstr Vol`, raw 0..144) covers only the
//! −144..0 dB attenuation at 1 dB/step.
//!
//! Serial mute control (datasheet §8.5, register `0x36` bit 7
//! `audio_proc_mute`: 1 = mute, 0 = play) is **not** exposed by the
//! kernel driver as an ALSA switch; the board's `/MUTE` GPIO line is used
//! by the driver instead. Live-verified on the Pi: the device answers on
//! I2C address `0x20` (`/dev/i2c-1`; `0x23` NACKs writes) and a
//! read-modify-write of `0x36` bit 7 mutes/restores playback and the
//! setting persists (the kernel driver does not rewrite `0x36`).

use std::sync::{Arc, Mutex};

use alsa::mixer::{Mixer, SelemChannelId, SelemId};
use anyhow::Context;
use async_trait::async_trait;
use speaker_hardware::{AmpDevice, AmpDeviceMock, AmpI2cAddress};
use tracing::{debug, info, warn};

use crate::amp_state::AmpState;
use crate::error::{AmpError, AmpResult};

/// Trait abstracting amplifier hardware operations.
/// Production uses `RealAmpBackend`; tests inject `MockAmpBackend`.
// `async_trait` wraps each async method in a pinned `Future`, which is itself
// `#[must_use]`; clippy flags the generated wrapper as double-must-use.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait AmpBackend: Send + Sync {
    /// Write the complete amp state (level + mute) to hardware.
    /// Returns the state that was actually written (may differ if clamped).
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState>;

    /// Read the current hardware state (for validation/diagnostics).
    async fn read_state(&self) -> AmpResult<AmpState>;

    /// Validate that the hardware is present and responsive.
    async fn validate(&self) -> AmpResult<()>;

    /// Apply the MA12070P serial mute register (`0x36` bit 7). Independent
    /// of the level: the mixer keeps its level while muted.
    async fn set_serial_mute(&self, muted: bool) -> AmpResult<()>;

    /// Read the serial mute register state.
    async fn read_serial_mute(&self) -> AmpResult<bool>;

    /// Backend identifier for logs/diagnostics (e.g. the ALSA card name,
    /// `"i2c-ma12070p@0x20"`, or `"mock"`). The I2S playback card
    /// (`sndrpihifiberry`) is pipeline configuration and never a backend id.
    fn backend_id(&self) -> &str;
}

/// Production ALSA + I2C backend for the Merus MA12070P amplifier.
pub struct RealAmpBackend {
    card_name: String,
    // Mixer operations are blocking; we run them in a blocking task.
    // The mixer handle is kept open for the backend lifetime.
    mixer: Arc<Mutex<Mixer>>,
    // Serial mute register access (0x36 bit 7) via the Phase 1.5 AmpDevice.
    amp: Arc<AmpDevice>,
}

impl RealAmpBackend {
    /// Open the ALSA mixer for the given card and validate the A.Mstr Vol control.
    pub async fn open(card_name: &str) -> AmpResult<Self> {
        let card_name = card_name.to_string();
        let card_name_for_closure = card_name.clone();
        let mixer = tokio::task::spawn_blocking(move || -> AmpResult<Mixer> {
            let mixer = Mixer::new(&card_name_for_closure, false).map_err(AmpError::Alsa)?;
            Ok(mixer)
        })
        .await
        .context("Mixer open task panicked")??;

        // Open the I2C bus for the serial mute register (fails fast: mute
        // would be silently broken if the bus were unavailable).
        let amp = tokio::task::spawn_blocking(|| {
            speaker_hardware::AmpDevice::with_bus_and_address(
                speaker_hardware::MA12070P_DEFAULT_I2C_BUS,
                AmpI2cAddress::default(),
            )
        })
        .await
        .context("AmpDevice open task panicked")??;

        // Validate the control exists and has expected range
        Self::validate_control(&mixer, &card_name).await?;

        info!(card = %card_name, "RealAmpBackend opened successfully");
        Ok(Self {
            card_name,
            mixer: Arc::new(Mutex::new(mixer)),
            amp: Arc::new(amp),
        })
    }

    /// Validate the A.Mstr Vol control exists and has range 0..144 (blocking).
    fn validate_control_blocking(mixer: &Mixer, card_name: &str) -> AmpResult<()> {
        let selem_id = SelemId::new("A.Mstr Vol", 0);
        let elem = mixer.find_selem(&selem_id).ok_or_else(|| {
            AmpError::control_error(format!(
                "A.Mstr Vol control not found on card {}",
                card_name
            ))
        })?;

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
        let elem = mixer
            .find_selem(&selem_id)
            .ok_or_else(|| AmpError::control_error("A.Mstr Vol control disappeared"))?;

        // Write to both channels (mono control, but write both for safety)
        elem.set_playback_volume_all(level_raw as i64)
            .map_err(AmpError::Alsa)?;

        // Verify write
        let written = elem.get_playback_volume(SelemChannelId::FrontLeft)?;
        if written != level_raw as i64 {
            warn!(
                requested = level_raw,
                written, "Mixer write verification mismatch"
            );
        }

        Ok(())
    }
    /// Write the mixer level (blocking task wrapper).
    ///
    /// # Errors
    /// Returns [`AmpError`] on mixer faults.
    async fn write_mixer_level(&self, level_raw: u16) -> AmpResult<()> {
        let mixer = self.mixer.clone();
        tokio::task::spawn_blocking(move || {
            let guard = mixer
                .lock()
                .map_err(|e| AmpError::internal(e.to_string()))?;
            Self::write_volume_blocking(&guard, level_raw)
        })
        .await
        .context("Write volume task panicked")??;
        Ok(())
    }
}

#[async_trait]
impl AmpBackend for RealAmpBackend {
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState> {
        // Pop-free ordering (mirrors `I2cAmpBackend`): muting → mute first,
        // then the volume; unmuting → write the volume while still muted,
        // then clear the mute bit last (never expose the reset-default
        // 0x18 = 0 dB level). Serial-bit write failures reject with the
        // hardware untouched; a mixer-write failure after the bit is
        // applied fails silent (safe direction) and heals on the next
        // write.
        if state.muted {
            self.set_serial_mute(true).await?;
            self.write_mixer_level(state.level_raw).await?;
        } else {
            self.write_mixer_level(state.level_raw).await?;
            self.set_serial_mute(false).await?;
        }
        // Return the logical state that was requested.
        Ok(*state)
    }

    async fn read_state(&self) -> AmpResult<AmpState> {
        // The serial mute bit is the authoritative mute state.
        let serial_muted = self.read_serial_mute().await?;
        let mixer = self.mixer.clone();
        tokio::task::spawn_blocking(move || -> AmpResult<AmpState> {
            let mixer = mixer
                .lock()
                .map_err(|e| AmpError::internal(e.to_string()))?;
            let selem_id = SelemId::new("A.Mstr Vol", 0);
            let elem = mixer
                .find_selem(&selem_id)
                .ok_or_else(|| AmpError::control_error("A.Mstr Vol control disappeared"))?;
            let raw = elem.get_playback_volume(SelemChannelId::FrontLeft)? as u16;
            Ok(AmpState {
                level_raw: raw,
                muted: serial_muted,
            })
        })
        .await
        .context("Read state task panicked")?
    }

    async fn set_serial_mute(&self, muted: bool) -> AmpResult<()> {
        self.amp.write_serial_mute_async(muted).await?;
        // Written on every SetLevel too (bit rewrite); the user-visible
        // lifecycle is logged at info! by the D-Bus layer.
        debug!(muted, "Serial mute register updated");
        Ok(())
    }

    async fn read_serial_mute(&self) -> AmpResult<bool> {
        self.amp
            .read_settings_async()
            .await
            .map_err(AmpError::from)
            .map(|s| s.serial_muted)
    }

    async fn validate(&self) -> AmpResult<()> {
        let mixer = self.mixer.clone();
        let card_name = self.card_name.clone();
        tokio::task::spawn_blocking(move || {
            let guard = mixer
                .lock()
                .map_err(|e| AmpError::internal(e.to_string()))?;
            Self::validate_control_blocking(&guard, &card_name)
        })
        .await
        .context("Validate task panicked")?
    }

    fn backend_id(&self) -> &str {
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
    should_fail_serial_mute: bool,
    serial_muted: bool,
    serial_mute_calls: u32,
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

    /// Set whether the next serial mute write should fail.
    pub fn set_serial_mute_fail(&self, fail: bool) {
        self.inner.lock().unwrap().should_fail_serial_mute = fail;
    }

    /// Set the mocked serial mute register state (for read_serial_mute).
    pub fn set_serial_muted(&self, muted: bool) {
        self.inner.lock().unwrap().serial_muted = muted;
    }

    /// Get the last serial mute state that was written (for assertion).
    pub fn last_serial_mute(&self) -> Option<bool> {
        let s = self.inner.lock().unwrap();
        (s.serial_mute_calls > 0).then_some(s.serial_muted)
    }

    /// Get the serial mute call count.
    #[must_use]
    pub fn serial_mute_calls(&self) -> u32 {
        self.inner.lock().unwrap().serial_mute_calls
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
        let backend = Self {
            inner: inner.clone(),
        };
        let ctrl = MockAmpCtrl { inner };
        (backend, ctrl)
    }
}

#[async_trait]
impl AmpBackend for MockAmpBackend {
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AmpError::internal(e.to_string()))?;
        guard.write_called += 1;

        if guard.should_fail_write {
            guard.should_fail_write = false;
            return Err(AmpError::internal("mock write failure"));
        }

        // Return the logical state that was requested (preserves level_raw even when muted)
        // The hardware level is available via AmpState::hardware_level()
        let applied = *state;
        guard.state = applied;
        guard.last_write = Some(applied);
        // Mirror the real backend: write_state applies the serial mute bit.
        guard.serial_muted = state.muted;
        guard.serial_mute_calls += 1;
        Ok(applied)
    }

    async fn read_state(&self) -> AmpResult<AmpState> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AmpError::internal(e.to_string()))?;
        guard.read_called += 1;

        if guard.should_fail_read {
            guard.should_fail_read = false;
            return Err(AmpError::internal("mock read failure"));
        }

        Ok(guard.state)
    }

    async fn validate(&self) -> AmpResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AmpError::internal(e.to_string()))?;
        guard.validate_called += 1;

        if guard.should_fail_validate {
            return Err(AmpError::hardware_validation("mock validation failure"));
        }

        Ok(())
    }

    async fn set_serial_mute(&self, muted: bool) -> AmpResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AmpError::internal(e.to_string()))?;
        guard.serial_mute_calls += 1;

        if guard.should_fail_serial_mute {
            guard.should_fail_serial_mute = false;
            return Err(AmpError::internal("mock serial mute failure"));
        }

        guard.serial_muted = muted;
        Ok(())
    }

    async fn read_serial_mute(&self) -> AmpResult<bool> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| AmpError::internal(e.to_string()))?;
        Ok(guard.serial_muted)
    }

    fn backend_id(&self) -> &str {
        "mock"
    }
}

/// Production I2C-only backend for the Phase 2 `hifiberry-dac` mode.
///
/// Wraps the Phase 1.5 [`AmpDevice`] write path (speaker-hardware crate):
/// volume is the VLP master register `0x40` (`reg = 168 − raw`, live-verified
/// 2026-11-11) and mute is the serial bit `0x36` bit 7. Serial mute remains
/// the primary mute; `/MUTE` (GPIO15) is boot-time wiring owned by
/// [`crate::amp_gpio`], not by this backend.
///
/// # Errors
/// Backend constructors return [`AmpError::I2c`] if the bus or device cannot
/// be opened; every operation maps hardware faults through the canonical
/// [`AmpError`] with `source` preservation.
pub struct I2cAmpBackend {
    amp: Arc<AmpDevice>,
    id: String,
}

impl I2cAmpBackend {
    /// Open the hardware I2C backend (non-destructive validation read).
    ///
    /// Note: with `dtoverlay=hifiberry-dac`, the amp boots in `/ENABLE`
    /// reset until the GPIO bring-up ([`crate::amp_gpio::AmpGpio::wake`])
    /// drives GPIO14 low — I2C **writes** NACK in that state. Boot
    /// ordering in `main.rs` is: wake → this backend → register init.
    ///
    /// # Errors
    /// Returns [`AmpError`] if the I2C bus/device cannot be opened.
    pub async fn open(bus: u8, address: AmpI2cAddress) -> AmpResult<Self> {
        let amp =
            tokio::task::spawn_blocking(move || AmpDevice::with_bus_and_address(bus, address))
                .await
                .context("AmpDevice open task panicked")??;
        let id = format!("i2c-ma12070p@{address}");
        info!(backend = %id, "I2cAmpBackend opened");
        Ok(Self {
            amp: Arc::new(amp),
            id,
        })
    }

    /// Open the mocked backend pair for tests
    /// (`new_mocked() -> (Self, Ctrl)` per M-MOCKABLE-SYSCALLS).
    #[must_use]
    pub fn new_mocked() -> (Self, AmpDeviceMock) {
        let (amp, mock) = AmpDevice::new_mocked();
        (
            Self {
                amp: Arc::new(amp),
                id: "i2c-ma12070p@0x20".to_string(),
            },
            mock,
        )
    }

    /// Direct access to the underlying device for the controller's
    /// register-level operations (PLL handshake, VLP init, fault recovery).
    #[must_use]
    pub fn device(&self) -> &Arc<AmpDevice> {
        &self.amp
    }
}

#[async_trait]
impl AmpBackend for I2cAmpBackend {
    async fn write_state(&self, state: &AmpState) -> AmpResult<AmpState> {
        // Pop-free ordering: muting → mute first, then the volume;
        // unmuting → write the volume while still muted, then clear the
        // mute bit last (never expose the reset-default 0x18 = 0 dB level).
        if state.muted {
            self.amp.write_serial_mute_async(true).await?;
            self.amp.write_master_volume_async(state.level_raw).await?;
        } else {
            self.amp.write_master_volume_async(state.level_raw).await?;
            self.amp.write_serial_mute_async(false).await?;
        }
        Ok(*state)
    }

    async fn read_state(&self) -> AmpResult<AmpState> {
        let this = self;
        let amp = Arc::clone(&this.amp);
        tokio::task::spawn_blocking(move || amp.read_settings())
            .await
            .context("Amp settings read task panicked")?
            .map_err(crate::error::AmpError::from)
            .map(|settings| AmpState {
                level_raw: settings.master_volume_raw,
                muted: settings.serial_muted,
            })
    }

    async fn validate(&self) -> AmpResult<()> {
        self.read_state().await.map(|_| ())
    }

    async fn set_serial_mute(&self, muted: bool) -> AmpResult<()> {
        self.amp.write_serial_mute_async(muted).await?;
        debug!(muted, "Serial mute register updated");
        Ok(())
    }

    async fn read_serial_mute(&self) -> AmpResult<bool> {
        let settings = self.read_state().await;
        settings.map(|s| s.muted)
    }

    fn backend_id(&self) -> &str {
        &self.id
    }
}
