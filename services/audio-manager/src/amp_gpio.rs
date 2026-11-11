//! GPIO ownership of the MA12070P `/ENABLE`, `/MUTE`, booster, and `error`
//! lines.
//!
//! Pinout (Phase 0 ground truth, decompiled `merus-amp.dtbo` + live
//! `pinctrl`, 2026-11-11): `/ENABLE` = BCM GPIO 14 (active-LOW: LOW wakes
//! the amp from hardware reset), `/MUTE` = BCM GPIO 15 (active-LOW: LOW =
//! muted — verified against `ma120x0p.c` `mute_stream` (`if (mute) val = 0`)
//! and the `GPIOD_OUT_LOW` "make sure the device is muted" probe request),
//! booster = BCM GPIO 17 (5 V→20 V boost converter enable, kernel parity:
//! `booster_gp` high + 200 ms settle before `/ENABLE`), `error` = BCM GPIO 23
//! (fault indicator; LOW = fault per the open-drain merus design).
//!
//! With `dtoverlay=hifiberry-dac` nothing else claims these lines, so this
//! module is the sole owner. GPIO syscalls are blocking: callers that run in
//! async context must invoke the sleeping operations ([`AmpGpio::power_up`],
//! [`AmpGpio::reset`]) via `spawn_blocking`; the quick line writes
//! ([`AmpGpio::wake`], [`AmpGpio::set_hardware_mute`]) are safe to call
//! inline. The error-line edge watch arms a synchronous rppal interrupt and
//! then polls it in a dedicated thread (no async GPIO API exists in rppal
//! 0.22; see plan §5.3 — no new crates without user approval).

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rppal::gpio::{Gpio, InputPin, Level, OutputPin, Trigger};
use tracing::{debug, info, warn};

use crate::error::{AmpError, AmpResult};

/// BCM GPIO pin assignments (Phase 0-verified pinout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmpGpioConfig {
    /// `/ENABLE` — active-LOW amplifier enable/reset.
    pub enable: u8,
    /// `/MUTE` — active-LOW hardware mute line.
    pub mute: u8,
    /// Booster-converter enable (kernel `booster_gp`).
    pub booster: u8,
    /// `error` — fault indicator input (LOW = fault).
    pub error: u8,
}

impl Default for AmpGpioConfig {
    fn default() -> Self {
        Self {
            enable: 14,
            mute: 15,
            booster: 17,
            error: 23,
        }
    }
}

/// Booster PVDD settle time (kernel parity: `msleep(200)` after booster on).
const BOOSTER_SETTLE: Duration = Duration::from_millis(200);

/// Post-enable settle time before the first I2C transaction (kernel parity:
/// `msleep(50)` after claiming `/ENABLE`).
const ENABLE_SETTLE: Duration = Duration::from_millis(50);

/// `/ENABLE` HIGH pulse width for a reset cycle.
const RESET_PULSE: Duration = Duration::from_millis(1);

/// Post-reset settle time before register re-init.
const RESET_SETTLE: Duration = Duration::from_millis(10);

/// How long a single edge-watch poll blocks before re-checking shutdown.
const EDGE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Shared mock state behind [`AmpGpioMock`]. The mute state is stored as the
/// **electrical pin level** (`mute_low`), not the logical intent, so an
/// inverted write can never pass a mock assertion unnoticed.
#[derive(Default)]
struct MockGpioState {
    awake: bool,
    /// `true` = `/MUTE` pin driven LOW = electrically muted.
    mute_low: bool,
    booster_on: bool,
    error_active: bool,
    wake_calls: u32,
    reset_calls: u32,
    power_up_calls: u32,
    mute_calls: u32,
    /// Test-only hook fired when a mock `reset()` completes, modeling the
    /// register wipe the `/ENABLE` pulse causes on the I2C device (e.g.
    /// clearing the mock serial mute bit). `None` in normal tests.
    reset_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Lock a mock-state mutex with poisoned-data recovery (M-POISONED-MUTEX).
fn mock_lock(state: &Mutex<MockGpioState>) -> MutexGuard<'_, MockGpioState> {
    state.lock().unwrap_or_else(|p| {
        warn!("GPIO mock state lock poisoned; recovering data");
        p.into_inner()
    })
}

/// Manual `Debug` impl: the reset hook is not `Debug`.
impl fmt::Debug for MockGpioState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockGpioState")
            .field("awake", &self.awake)
            .field("mute_low", &self.mute_low)
            .field("booster_on", &self.booster_on)
            .field("error_active", &self.error_active)
            .field("wake_calls", &self.wake_calls)
            .field("reset_calls", &self.reset_calls)
            .field("power_up_calls", &self.power_up_calls)
            .field("mute_calls", &self.mute_calls)
            .field("reset_hook", &self.reset_hook.is_some())
            .finish()
    }
}

/// Controller for the mocked GPIO backend (`new_mocked()` pairing).
#[derive(Debug, Clone)]
pub struct AmpGpioMock {
    state: Arc<Mutex<MockGpioState>>,
}

impl AmpGpioMock {
    /// Set whether the mock reports an active (LOW) error line.
    pub fn set_error_active(&self, active: bool) {
        mock_lock(&self.state).error_active = active;
    }

    /// Number of `wake()` calls (amp driven awake from reset).
    #[must_use]
    pub fn wake_calls(&self) -> u32 {
        mock_lock(&self.state).wake_calls
    }

    /// Number of `reset()` calls (HIGH→LOW `/ENABLE` reset pulses).
    #[must_use]
    pub fn reset_calls(&self) -> u32 {
        mock_lock(&self.state).reset_calls
    }

    /// Number of `power_up()` calls (booster + enable sequence).
    #[must_use]
    pub fn power_up_calls(&self) -> u32 {
        mock_lock(&self.state).power_up_calls
    }

    /// `true` when the `/MUTE` pin is electrically LOW (muted).
    #[must_use]
    pub fn mute_pin_low(&self) -> bool {
        mock_lock(&self.state).mute_low
    }

    /// `true` when the booster line is driven HIGH.
    #[must_use]
    pub fn booster_on(&self) -> bool {
        mock_lock(&self.state).booster_on
    }

    /// `true` when the amp is awake (`/ENABLE` LOW).
    #[must_use]
    pub fn amp_awake(&self) -> bool {
        mock_lock(&self.state).awake
    }

    /// Number of mute-line writes.
    #[must_use]
    pub fn mute_calls(&self) -> u32 {
        mock_lock(&self.state).mute_calls
    }

    /// Install a hook fired after each mock `reset()` completes, modeling
    /// the `/ENABLE` register wipe on the I2C device (e.g. clearing the
    /// mock serial mute bit). Pass `None` to remove.
    pub fn set_reset_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        mock_lock(&self.state).reset_hook = hook;
    }
}

/// GPIO backend: hardware via `rppal`, or an in-memory mock for host tests.
/// The hardware pins are boxed to keep the mock variant size-dominant.
enum GpioSeam {
    Hardware(Box<HardwarePins>),
    Mock { state: Arc<Mutex<MockGpioState>> },
}

/// Claimed rppal line handles. The `error` input is `Option` because the
/// edge watch takes it out of the seam into its dedicated thread (the
/// blocking `poll_interrupt` call must not hold the seam lock).
struct HardwarePins {
    enable: OutputPin,
    mute: OutputPin,
    booster: OutputPin,
    error: Option<InputPin>,
}

/// Sole owner of the amplifier GPIO lines. Cloneable; all line access is
/// serialized through the seam mutex (short critical sections only — the
/// blocking edge watch owns its pin outside the lock).
#[derive(Clone)]
pub struct AmpGpio {
    seam: Arc<Mutex<GpioSeam>>,
}

impl AmpGpio {
    /// Claim the GPIO lines on `/dev/gpiochip0` (BCM numbering) with the
    /// **safe orientation asserted at claim time** (not inherited by luck):
    /// `/MUTE` LOW (electrically muted), booster LOW (off), `/ENABLE` HIGH
    /// (amp in hardware reset — the hifiberry boot state).
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] if the chip or any line cannot be claimed.
    pub fn open(config: AmpGpioConfig) -> AmpResult<Self> {
        let gpio = Gpio::new().map_err(AmpError::Gpio)?;
        let enable = gpio
            .get(config.enable)
            .map_err(AmpError::Gpio)?
            .into_output_high();
        let mute = gpio
            .get(config.mute)
            .map_err(AmpError::Gpio)?
            .into_output_low();
        let booster = gpio
            .get(config.booster)
            .map_err(AmpError::Gpio)?
            .into_output_low();
        let error = gpio.get(config.error).map_err(AmpError::Gpio)?.into_input();
        info!(
            enable_pin = config.enable,
            mute_pin = config.mute,
            booster_pin = config.booster,
            error_pin = config.error,
            "GPIO lines claimed (amp in reset, hardware muted, booster off)"
        );
        Ok(Self {
            seam: Arc::new(Mutex::new(GpioSeam::Hardware(Box::new(HardwarePins {
                enable,
                mute,
                booster,
                error: Some(error),
            })))),
        })
    }

    /// Mocked backend pair (`new_mocked() -> (Self, Ctrl)`).
    #[must_use]
    pub fn new_mocked() -> (Self, AmpGpioMock) {
        let state = Arc::new(Mutex::new(MockGpioState {
            // Mock starts in the same safe orientation `open()` asserts.
            mute_low: true,
            ..MockGpioState::default()
        }));
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

    /// Full kernel-parity power-up sequence (blocking; call via
    /// `spawn_blocking` from async context):
    /// booster HIGH → 200 ms PVDD settle → `/ENABLE` LOW → 50 ms settle.
    /// Mirrors `ma120x0p_i2c_probe` so PVDD is stable before the amp exits
    /// reset and before the first I2C transaction.
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults.
    pub fn power_up(&self) -> AmpResult<()> {
        {
            let mut seam = self.seam_lock();
            match &mut *seam {
                GpioSeam::Hardware(pins) => {
                    pins.booster.write(Level::High);
                }
                GpioSeam::Mock { state } => {
                    mock_lock(state).booster_on = true;
                }
            }
        }
        std::thread::sleep(BOOSTER_SETTLE);
        {
            let mut seam = self.seam_lock();
            match &mut *seam {
                GpioSeam::Hardware(pins) => {
                    pins.enable.write(Level::Low);
                }
                GpioSeam::Mock { state } => {
                    let mut s = mock_lock(state);
                    s.awake = true;
                    s.power_up_calls += 1;
                }
            }
        }
        std::thread::sleep(ENABLE_SETTLE);
        debug!("GPIO power-up complete (booster on, amp awake)");
        Ok(())
    }

    /// Drive `/ENABLE` LOW to wake the amp (no booster/settle sequence —
    /// used when power rails are known-good). Idempotent.
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults.
    pub fn wake(&self) -> AmpResult<()> {
        let mut seam = self.seam_lock();
        match &mut *seam {
            GpioSeam::Hardware(pins) => {
                pins.enable.write(Level::Low);
                debug!("GPIO /ENABLE driven LOW (amp awake)");
            }
            GpioSeam::Mock { state } => {
                let mut s = mock_lock(state);
                s.awake = true;
                s.wake_calls += 1;
            }
        }
        Ok(())
    }

    /// `/ENABLE` reset cycle (blocking; call via `spawn_blocking`): HIGH
    /// pulse wipes all registers, then LOW + settle wakes the amp. The
    /// `/MUTE` line is forced LOW for the whole cycle (a register wipe
    /// clears the serial mute bit, so the GPIO line is the only guaranteed
    /// audio-path gate during and after the pulse). Used by fault recovery
    /// so the "full register re-init" starts from true reset defaults.
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults.
    pub fn reset(&self) -> AmpResult<()> {
        {
            let mut seam = self.seam_lock();
            match &mut *seam {
                GpioSeam::Hardware(pins) => {
                    pins.enable.write(Level::High);
                    pins.mute.write(Level::Low);
                }
                GpioSeam::Mock { state } => {
                    let mut s = mock_lock(state);
                    s.awake = false;
                    s.mute_low = true;
                }
            }
        }
        std::thread::sleep(RESET_PULSE);
        {
            let mut seam = self.seam_lock();
            let fired = match &mut *seam {
                GpioSeam::Hardware(pins) => {
                    pins.enable.write(Level::Low);
                    None
                }
                GpioSeam::Mock { state } => {
                    let mut s = mock_lock(state);
                    s.awake = true;
                    s.reset_calls += 1;
                    s.reset_hook.clone()
                }
            };
            // Fire the test hook outside the lock (a register wipe clears
            // the serial mute bit; mocks model this via the hook).
            if let Some(hook) = fired {
                hook();
            }
        }
        std::thread::sleep(RESET_SETTLE);
        debug!("GPIO /ENABLE reset cycle complete");
        Ok(())
    }

    /// Drive the `/MUTE` line. `true` = muted = pin **LOW** (active-LOW,
    /// kernel-verified polarity).
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults.
    pub fn set_hardware_mute(&self, muted: bool) -> AmpResult<()> {
        let mut seam = self.seam_lock();
        match &mut *seam {
            GpioSeam::Hardware(pins) => {
                pins.mute
                    .write(if muted { Level::Low } else { Level::High });
                debug!(muted, "GPIO /MUTE updated (LOW = muted)");
            }
            GpioSeam::Mock { state } => {
                let mut s = mock_lock(state);
                s.mute_low = muted;
                s.mute_calls += 1;
            }
        }
        Ok(())
    }

    /// Read the `error` line: `true` = fault active (LOW).
    ///
    /// # Errors
    /// Returns [`AmpError::Gpio`] on hardware faults, or
    /// [`AmpError::internal`] if the line was taken by the edge watch (the
    /// watch + safety tick cover detection in that case).
    pub fn error_active(&self) -> AmpResult<bool> {
        let mut seam = self.seam_lock();
        match &mut *seam {
            GpioSeam::Hardware(pins) => {
                let Some(error) = pins.error.as_mut() else {
                    return Err(AmpError::internal("error line owned by the edge watch"));
                };
                // Active-LOW: reading LOW means fault asserted.
                Ok(error.read() == Level::Low)
            }
            GpioSeam::Mock { state } => Ok(mock_lock(state).error_active),
        }
    }

    /// Blocking error-line edge watch: invokes `on_edge` on every falling
    /// edge (fault asserted). The callback returns `false` to stop the
    /// watch (shutdown). Designed to be spawned inside a `spawn_blocking`
    /// task per plan §5.3.
    ///
    /// On hardware, the synchronous interrupt is armed once
    /// (`set_interrupt(Trigger::FallingEdge, None)` — `poll_interrupt` is a
    /// no-op otherwise) and the `InputPin` is then **taken out of the seam**
    /// so the blocking poll never holds the seam lock: concurrent
    /// `wake()`/`set_hardware_mute()` calls proceed while polling.
    ///
    /// # Panics
    /// Panics if the mock state lock is poisoned beyond recovery (a
    /// panicked holder is a bug; the safety tick covers fault detection).
    pub fn watch_error_edges<F: FnMut() -> bool + Send>(&self, mut on_edge: F) {
        let (mock_state, hardware_pin) = {
            let mut seam = match self.seam.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            match &mut *seam {
                GpioSeam::Mock { state } => (Some(Arc::clone(state)), None),
                GpioSeam::Hardware(pins) => {
                    // Arm the synchronous interrupt; poll_interrupt only
                    // reports events after this configuration.
                    let Some(error) = pins.error.as_mut() else {
                        warn!("Error-line edge watch already running");
                        return;
                    };
                    if let Err(e) = error.set_interrupt(Trigger::FallingEdge, None) {
                        warn!(error = %e, "Failed to arm error-line interrupt; edge watch disabled");
                        return;
                    }
                    (None, pins.error.take())
                }
            }
        };
        if let Some(state) = mock_state {
            loop {
                let fire = mock_lock(&state).error_active;
                if fire && !on_edge() {
                    return;
                }
                std::thread::sleep(EDGE_POLL_INTERVAL);
            }
        }
        let Some(mut error) = hardware_pin else {
            return;
        };
        loop {
            match error.poll_interrupt(true, Some(EDGE_POLL_INTERVAL)) {
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
                    // Transient OS-level watch faults: back off rather than
                    // hot-spinning; the safety tick covers detection.
                    warn!(error = %e, "GPIO error-line poll failed; backing off");
                    std::thread::sleep(EDGE_POLL_INTERVAL);
                }
            }
        }
    }

    fn seam_lock(&self) -> MutexGuard<'_, GpioSeam> {
        self.seam.lock().unwrap_or_else(|p| {
            warn!("GPIO seam lock poisoned; recovering data (M-POISONED-MUTEX)");
            p.into_inner()
        })
    }
}
