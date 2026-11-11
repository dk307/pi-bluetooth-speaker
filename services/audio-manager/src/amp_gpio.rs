//! GPIO ownership of the MA12070P `/ENABLE`, `/MUTE`, and `error` lines.
//!
//! Pinout (Phase 0 ground truth, decompiled `merus-amp.dtbo` + live
//! `pinctrl`, 2026-11-11): `/ENABLE` = BCM GPIO 14 (active-LOW, driving it
//! LOW wakes the amp from hardware reset), `/MUTE` = BCM GPIO 15
//! (active-LOW; boot-time wiring only — the serial mute bit `0x36` bit 7 is
//! the runtime mute), `error` = BCM GPIO 23 (fault indicator; LOW = fault
//! per the open-drain merus design).
//!
//! With `dtoverlay=hifiberry-dac` nothing else claims these lines, so this
//! module is the sole owner. All Linux GPIO syscalls are blocking and are
//! invoked from `spawn_blocking` tasks; the error-line edge watch runs a
//! blocking `rppal` poll loop feeding an `mpsc` channel (no async GPIO API
//! exists in `rppal` 0.22; see plan §5.3 — no new crates without approval).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rppal::gpio::{Gpio, InputPin, OutputPin};
use tracing::{debug, info, warn};

use crate::error::{AmpError, AmpResult};

/// BCM GPIO pin assignments (Phase 0-verified pinout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmpGpioConfig {
    /// `/ENABLE` — active-LOW amplifier enable/reset.
    pub enable: u8,
    /// `/MUTE` — active-LOW hardware mute line.
    pub mute: u8,
    /// `error` — fault indicator input (LOW = fault).
    pub error: u8,
}

impl Default for AmpGpioConfig {
    fn default() -> Self {
        Self {
            enable: 14,
            mute: 15,
            error: 23,
        }
    }
}

/// How long a single edge-watch poll blocks before re-checking shutdown.
const EDGE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Shared mock state behind [`AmpGpioMock`].
#[derive(Debug, Default)]
struct MockGpioState {
    amp_awake: bool,
    hardware_muted: bool,
    error_active: bool,
    wake_calls: u32,
    mute_calls: u32,
}

/// Controller for the mocked GPIO backend (`new_mocked()` pairing).
#[derive(Debug, Clone)]
pub struct AmpGpioMock {
    state: Arc<Mutex<MockGpioState>>,
}

impl AmpGpioMock {
    /// Set whether the mock reports an active (LOW) error line.
    pub fn set_error_active(&self, active: bool) {
        self.state.lock().unwrap().error_active = active;
    }

    /// Assert the mock woke the amp (`wake_calls` >= 1).
    #[must_use]
    pub fn wake_calls(&self) -> u32 {
        self.state.lock().unwrap().wake_calls
    }

    /// Assert the last hardware-mute state written.
    #[must_use]
    pub fn hardware_muted(&self) -> bool {
        self.state.lock().unwrap().hardware_muted
    }

    /// Get the mute-write call count.
    #[must_use]
    pub fn mute_calls(&self) -> u32 {
        self.state.lock().unwrap().mute_calls
    }
}

/// GPIO backend: hardware via `rppal`, or an in-memory mock for host tests.
/// The hardware pins are boxed to keep the mock variant size-dominant.
enum GpioSeam {
    Hardware(Box<HardwarePins>),
    Mock { state: Arc<Mutex<MockGpioState>> },
}

/// Claimed rppal line handles.
struct HardwarePins {
    enable: OutputPin,
    mute: OutputPin,
    error: InputPin,
}

/// Sole owner of the amplifier GPIO lines. Cloneable; all transactions are
/// serialized by `rppal`'s internal gpiomem locking at the syscall layer.
#[derive(Clone)]
pub struct AmpGpio {
    seam: Arc<Mutex<GpioSeam>>,
}

impl AmpGpio {
    /// Claim the GPIO lines on `/dev/gpiochip0` (BCM numbering) and hold the
    /// amp **awake** (GPIO `/ENABLE` LOW) and **hardware-muted** (GPIO15
    /// LOW) — the safe boot orientation; the controller unmutes after the
    /// PLL-lock handshake.
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] if the chip or any line cannot be claimed.
    pub fn open(config: AmpGpioConfig) -> AmpResult<Self> {
        let gpio = Gpio::new().map_err(crate::error::AmpError::Gpio)?;
        let enable = gpio
            .get(config.enable)
            .map_err(crate::error::AmpError::Gpio)?
            .into_output();
        // Start LOW (muted) — never unmute hardware before the PLL handshake.
        let mute = gpio
            .get(config.mute)
            .map_err(crate::error::AmpError::Gpio)?
            .into_output();
        let error = gpio
            .get(config.error)
            .map_err(crate::error::AmpError::Gpio)?
            .into_input();
        info!(
            enable_pin = config.enable,
            mute_pin = config.mute,
            error_pin = config.error,
            "GPIO lines claimed (amp awake, hardware muted)"
        );
        Ok(Self {
            seam: Arc::new(Mutex::new(GpioSeam::Hardware(Box::new(HardwarePins {
                enable,
                mute,
                error,
            })))),
        })
    }

    /// Mocked backend pair (`new_mocked() -> (Self, Ctrl)`).
    #[must_use]
    pub fn new_mocked() -> (Self, AmpGpioMock) {
        let state = Arc::new(Mutex::new(MockGpioState::default()));
        let ctrl = AmpGpioMock {
            state: Arc::clone(&state),
        };
        (
            Self {
                seam: Arc::new(Mutex::new(GpioSeam::Mock { state })),
            },
            ctrl,
        )
    }

    /// Drive `/ENABLE` LOW to wake the amp (recovery path / boot only).
    /// Idempotent: the pin stays an output driven LOW afterwards.
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults or [`AmpError::Poisoned`]
    /// if the seam lock was poisoned by a panicked holder.
    pub fn wake(&self) -> AmpResult<()> {
        let mut seam = self.seam_lock()?;
        match &mut *seam {
            GpioSeam::Hardware(pins) => {
                pins.enable.write(rppal::gpio::Level::Low);
                debug!("GPIO /ENABLE driven LOW (amp awake)");
            }
            GpioSeam::Mock { state } => {
                let mut s = state.lock().expect("mock lock poisoned");
                s.amp_awake = true;
                s.wake_calls += 1;
            }
        }
        Ok(())
    }

    /// Drive the `/MUTE` line: `true` = muted (LOW), `false` = unmuted
    /// (HIGH). Called only after the PLL-lock handshake passes.
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults or [`AmpError::Poisoned`]
    /// if the seam lock was poisoned by a panicked holder.
    pub fn set_hardware_mute(&self, muted: bool) -> AmpResult<()> {
        let mut seam = self.seam_lock()?;
        match &mut *seam {
            GpioSeam::Hardware(pins) => {
                // Active-LOW: LOW asserts mute.
                pins.mute.write(muted.into());
                debug!(muted, "GPIO /MUTE updated");
            }
            GpioSeam::Mock { state } => {
                let mut s = state.lock().expect("mock lock poisoned");
                s.hardware_muted = muted;
                s.mute_calls += 1;
            }
        }
        Ok(())
    }

    /// Read the `error` line: `true` = fault active (LOW).
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults or [`AmpError::Poisoned`]
    /// if the seam lock was poisoned by a panicked holder.
    pub fn error_active(&self) -> AmpResult<bool> {
        let mut seam = self.seam_lock()?;
        match &mut *seam {
            GpioSeam::Hardware(pins) => {
                // Active-LOW: reading LOW means fault asserted.
                Ok(pins.error.read() == rppal::gpio::Level::Low)
            }
            GpioSeam::Mock { state } => Ok(state.lock().expect("mock lock poisoned").error_active),
        }
    }

    /// Blocking error-line edge watch: invokes `on_edge` on every falling
    /// edge (fault asserted). The callback returns `false` to stop the
    /// watch (shutdown). Designed to be spawned inside a `spawn_blocking`
    /// task per plan §5.3.
    /// spawned inside a `spawn_blocking` task per plan §5.3.
    ///
    /// # Panics
    /// Panics if the seam or mock state lock is poisoned (a panicked holder
    /// is a bug; the watch thread dies and the safety tick covers detection).
    pub fn watch_error_edges<F: FnMut() -> bool + Send>(&self, mut on_edge: F) {
        let mut seam = match self.seam.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        match &mut *seam {
            GpioSeam::Hardware(pins) => loop {
                match pins.error.poll_interrupt(true, Some(EDGE_POLL_INTERVAL)) {
                    Ok(Some(_)) => {
                        debug!("GPIO error line falling edge");
                        if !on_edge() {
                            return;
                        }
                    }
                    Ok(None) => {
                        // Poll timeout: cycle again; the next poll re-arms.
                    }
                    Err(e) => {
                        // Transient OS-level watch faults: back off rather
                        // than hot-spinning; the safety tick covers detection.
                        warn!(error = %e, "GPIO error-line poll failed; backing off");
                        std::thread::sleep(EDGE_POLL_INTERVAL);
                    }
                }
            },
            GpioSeam::Mock { state } => {
                let state = Arc::clone(state);
                drop(seam);
                loop {
                    let fire = state.lock().expect("mock lock poisoned").error_active;
                    if fire && !on_edge() {
                        return;
                    }
                    std::thread::sleep(EDGE_POLL_INTERVAL);
                }
            }
        }
    }

    fn seam_lock(&self) -> AmpResult<std::sync::MutexGuard<'_, GpioSeam>> {
        self.seam.lock().map_err(|_p| {
            warn!("GPIO seam lock poisoned; recovering data (M-POISONED-MUTEX)");
            AmpError::internal("gpio seam lock poisoned")
        })
    }
}
