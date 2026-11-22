//! Dedicated low-priority `CamillaDSP` telemetry worker and 33 Hz UI frame stepper.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use speaker_dsp::{
    DspDevice, LatestFrameSlot, SILENCE_DBFS, SpectrumSnapshot, VisualizerSubscription,
    VuLevelsSnapshot, default_spectrum_subscription, default_vu_subscription,
};
use tokio::sync::mpsc;

use super::dsp_math::{
    DEFAULT_ZERO_VU_DBFS, ERB_BAND_COUNT, ErbBandMapper, SpectrumBallistics, dbfs_to_vu_ratio,
};

/// Active visualizer screen mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum VisualizerMode {
    /// Analog / linear `PeppyMeter` VU display.
    #[default]
    VuMeter = 0,
    /// 24-band Glasberg–Moore ERB spectrum analyzer.
    Spectrum = 1,
}

impl VisualizerMode {
    /// Total number of registered visualizer screens.
    pub const COUNT: i32 = 2;

    /// Converts an integer index (wrapping modulo `COUNT`) into a [`VisualizerMode`].
    #[must_use]
    pub const fn from_index(idx: i32) -> Self {
        match idx.rem_euclid(Self::COUNT) {
            1 => Self::Spectrum,
            _ => Self::VuMeter,
        }
    }

    /// Returns the next visualizer mode, wrapping around.
    #[must_use]
    pub const fn next(self) -> Self {
        Self::from_index(self as i32 + 1)
    }

    /// Returns the previous visualizer mode, wrapping around.
    #[must_use]
    pub const fn prev(self) -> Self {
        Self::from_index(self as i32 - 1)
    }
}

/// Output snapshot produced on each UI timer tick (`~33 Hz`).
#[derive(Debug, Clone, PartialEq)]
pub struct VisualizerOutput {
    /// Normalized left VU ratio (`0.0..=1.0`).
    pub left_vu_ratio: f32,
    /// Normalized right VU ratio (`0.0..=1.0`).
    pub right_vu_ratio: f32,
    /// Smoothed 24-band ERB bar heights (`0.0..=1.0`).
    pub spectrum_bars: [f32; ERB_BAND_COUNT],
    /// 24-band peak-hold cap positions (`0.0..=1.0`).
    pub spectrum_peaks: [f32; ERB_BAND_COUNT],
}

impl Default for VisualizerOutput {
    fn default() -> Self {
        Self {
            left_vu_ratio: 0.0,
            right_vu_ratio: 0.0,
            spectrum_bars: [0.0; ERB_BAND_COUNT],
            spectrum_peaks: [0.0; ERB_BAND_COUNT],
        }
    }
}

enum WorkerCommand {
    SetMode(Option<VisualizerMode>),
    Shutdown,
}

/// Coordinates the `Nice=15` `CamillaDSP` telemetry worker and UI-thread ballistics.
#[derive(Debug)]
pub struct VisualizerController {
    cmd_tx: mpsc::UnboundedSender<WorkerCommand>,
    worker_thread: Option<JoinHandle<()>>,
    vu_slot: Arc<LatestFrameSlot<VuLevelsSnapshot>>,
    spectrum_slot: Arc<LatestFrameSlot<SpectrumSnapshot>>,
    active_mode: Option<VisualizerMode>,
    zero_vu_dbfs: f32,
    erb_mapper: ErbBandMapper,
    ballistics: SpectrumBallistics,
    last_spectrum_targets: [f32; ERB_BAND_COUNT],
    last_vu_ratios: (f32, f32),
    last_tick: Option<Instant>,
}

impl VisualizerController {
    /// Spawns the low-priority telemetry worker thread for `dsp` (idle until `set_active_mode`).
    #[must_use]
    pub fn new(dsp: DspDevice, zero_vu_dbfs: f32) -> Self {
        let vu_slot = Arc::new(LatestFrameSlot::new());
        let spectrum_slot = Arc::new(LatestFrameSlot::new());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let vu_slot_bg = Arc::clone(&vu_slot);
        let spectrum_slot_bg = Arc::clone(&spectrum_slot);
        let worker_thread = std::thread::Builder::new()
            .name("visualizer-dsp".to_string())
            .spawn(move || {
                lower_current_thread_priority();
                run_worker_runtime(dsp, vu_slot_bg, spectrum_slot_bg, cmd_rx);
            })
            .ok();

        Self {
            cmd_tx,
            worker_thread,
            vu_slot,
            spectrum_slot,
            active_mode: None,
            zero_vu_dbfs: if zero_vu_dbfs.is_finite() {
                zero_vu_dbfs
            } else {
                DEFAULT_ZERO_VU_DBFS
            },
            erb_mapper: ErbBandMapper::new(),
            ballistics: SpectrumBallistics::new(),
            last_spectrum_targets: [0.0; ERB_BAND_COUNT],
            last_vu_ratios: (0.0, 0.0),
            last_tick: None,
        }
    }

    /// Updates the `0 VU` dBFS calibration reference.
    pub fn set_zero_vu_dbfs(&mut self, zero_vu_dbfs: f32) {
        if zero_vu_dbfs.is_finite() {
            self.zero_vu_dbfs = zero_vu_dbfs;
        }
    }

    /// Activates or deactivates telemetry streaming for `mode`.
    ///
    /// Passing `None` immediately unsubscribes (`StopSubscription`) so idle
    /// `CamillaDSP` overhead drops to zero.
    pub fn set_active_mode(&mut self, mode: Option<VisualizerMode>) {
        if self.active_mode == mode {
            return;
        }
        self.active_mode = mode;
        self.last_tick = None;
        self.last_spectrum_targets = [0.0; ERB_BAND_COUNT];
        self.ballistics.reset();
        self.last_vu_ratios = (0.0, 0.0);
        let _ = self.vu_slot.take();
        let _ = self.spectrum_slot.take();
        let _ = self.cmd_tx.send(WorkerCommand::SetMode(mode));
    }

    /// Returns the currently active visualizer mode, if any.
    #[must_use]
    pub const fn active_mode(&self) -> Option<VisualizerMode> {
        self.active_mode
    }

    /// Advances UI ballistics for the current frame and returns the latest display values.
    pub fn tick(&mut self, now: Instant) -> VisualizerOutput {
        let dt = self
            .last_tick
            .map_or(Duration::from_millis(30), |prev| {
                now.saturating_duration_since(prev)
            })
            .min(Duration::from_millis(100));
        self.last_tick = Some(now);

        match self.active_mode {
            None => VisualizerOutput::default(),
            Some(VisualizerMode::VuMeter) => {
                if let Some(vu) = self.vu_slot.take() {
                    let (l_db, r_db) = vu.capture_lr_rms();
                    self.last_vu_ratios = (
                        dbfs_to_vu_ratio(l_db, self.zero_vu_dbfs),
                        dbfs_to_vu_ratio(r_db, self.zero_vu_dbfs),
                    );
                }
                VisualizerOutput {
                    left_vu_ratio: self.last_vu_ratios.0,
                    right_vu_ratio: self.last_vu_ratios.1,
                    ..VisualizerOutput::default()
                }
            }
            Some(VisualizerMode::Spectrum) => {
                if let Some(spec) = self.spectrum_slot.take() {
                    self.last_spectrum_targets = self
                        .erb_mapper
                        .map_to_bands(&spec.frequencies, &spec.magnitudes);
                }
                let (bars, peaks) = self.ballistics.step(&self.last_spectrum_targets, dt);
                VisualizerOutput {
                    spectrum_bars: bars,
                    spectrum_peaks: peaks,
                    ..VisualizerOutput::default()
                }
            }
        }
    }

    /// Stops any active stream and joins the background worker thread cleanly.
    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.worker_thread.take()
            && let Err(panic_payload) = handle.join()
        {
            std::panic::resume_unwind(panic_payload);
        }
    }
}

impl Drop for VisualizerController {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

fn lower_current_thread_priority() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `PRIO_PROCESS` with `who = 0` adjusts only the calling thread's
        // Linux scheduling nice value to 15 (below the UI thread at Nice=10).
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 15) };
        if rc != 0 {
            tracing::debug!("Unable to lower visualizer worker priority to Nice=15");
        }
    }
}

fn run_worker_runtime(
    dsp: DspDevice,
    vu_slot: Arc<LatestFrameSlot<VuLevelsSnapshot>>,
    spectrum_slot: Arc<LatestFrameSlot<SpectrumSnapshot>>,
    mut cmd_rx: mpsc::UnboundedReceiver<WorkerCommand>,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        tracing::error!("Failed to build Tokio runtime for visualizer worker");
        return;
    };

    rt.block_on(async move {
        let mut active_sub: Option<VisualizerSubscription> = None;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                WorkerCommand::Shutdown => break,
                WorkerCommand::SetMode(mode) => {
                    if let Some(prev) = active_sub.take() {
                        prev.stop().await;
                    }
                    active_sub =
                        match mode {
                            None => None,
                            Some(VisualizerMode::VuMeter) => {
                                vu_slot.publish(VuLevelsSnapshot {
                                    capture_rms: vec![SILENCE_DBFS, SILENCE_DBFS],
                                    capture_peak: vec![SILENCE_DBFS, SILENCE_DBFS],
                                });
                                Some(dsp.start_vu_stream(
                                    default_vu_subscription(),
                                    Arc::clone(&vu_slot),
                                ))
                            }
                            Some(VisualizerMode::Spectrum) => {
                                spectrum_slot.publish(SpectrumSnapshot::silent());
                                Some(dsp.start_spectrum_stream(
                                    default_spectrum_subscription(),
                                    Arc::clone(&spectrum_slot),
                                ))
                            }
                        };
                }
            }
        }
        if let Some(prev) = active_sub.take() {
            prev.stop().await;
        }
    });
}
