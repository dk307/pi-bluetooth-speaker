//! Dedicated low-priority `CamillaDSP` spectrum telemetry worker and 33 Hz UI frame stepper.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use speaker_dsp::{
    DspDevice, LatestFrameSlot, SpectrumSnapshot, VisualizerSubscription,
    spectrum_subscription_for_rate,
};
use tokio::sync::mpsc;

use super::dsp_math::{ERB_BAND_COUNT, ErbBandMapper, SpectrumBallistics};

/// Active visualizer screen mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum VisualizerMode {
    /// Bilateral 48-bar radial FFT ring around central album art.
    #[default]
    RadialFft = 0,
    /// 24-band Glasberg–Moore ERB linear bar spectrum analyzer.
    LinearSpectrum = 1,
}

impl VisualizerMode {
    /// Total number of registered visualizer screens.
    pub const COUNT: i32 = 2;

    /// Converts an integer index (wrapping modulo `COUNT`) into a [`VisualizerMode`].
    #[must_use]
    pub const fn from_index(idx: i32) -> Self {
        match idx.rem_euclid(Self::COUNT) {
            1 => Self::LinearSpectrum,
            _ => Self::RadialFft,
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
    /// Smoothed 24-band ERB bar heights (`0.0..=1.0`).
    pub spectrum_bars: [f32; ERB_BAND_COUNT],
    /// 24-band peak-hold cap positions (`0.0..=1.0`).
    pub spectrum_peaks: [f32; ERB_BAND_COUNT],
}

impl Default for VisualizerOutput {
    fn default() -> Self {
        Self {
            spectrum_bars: [0.0; ERB_BAND_COUNT],
            spectrum_peaks: [0.0; ERB_BAND_COUNT],
        }
    }
}

enum WorkerCommand {
    SetActive { active: bool, max_rate_hz: f32 },
    Shutdown,
}

/// Coordinates the `Nice=15` `CamillaDSP` spectrum worker and UI-thread ballistics.
#[derive(Debug)]
pub struct VisualizerController {
    cmd_tx: mpsc::UnboundedSender<WorkerCommand>,
    worker_thread: Option<JoinHandle<()>>,
    spectrum_slot: Arc<LatestFrameSlot<SpectrumSnapshot>>,
    active: bool,
    force_60fps: bool,
    mode: VisualizerMode,
    erb_mapper: ErbBandMapper,
    ballistics: SpectrumBallistics,
    last_spectrum_targets: [f32; ERB_BAND_COUNT],
    last_tick: Option<Instant>,
}

impl VisualizerController {
    /// Spawns the low-priority spectrum worker thread for `dsp` (idle until `set_active_mode`).
    #[must_use]
    pub fn new(dsp: DspDevice) -> Self {
        let spectrum_slot = Arc::new(LatestFrameSlot::new());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let spectrum_slot_bg = Arc::clone(&spectrum_slot);
        let worker_thread = std::thread::Builder::new()
            .name("visualizer-dsp".to_string())
            .spawn(move || {
                lower_current_thread_priority();
                run_worker_runtime(dsp, spectrum_slot_bg, cmd_rx);
            })
            .ok();

        Self {
            cmd_tx,
            worker_thread,
            spectrum_slot,
            active: false,
            force_60fps: false,
            mode: VisualizerMode::default(),
            erb_mapper: ErbBandMapper::new(),
            ballistics: SpectrumBallistics::new(),
            last_spectrum_targets: [0.0; ERB_BAND_COUNT],
            last_tick: None,
        }
    }

    /// Activates or deactivates spectrum streaming for `mode` at `40 Hz` or `60 Hz`.
    ///
    /// Passing `None` immediately unsubscribes (`StopSubscription`) so idle
    /// `CamillaDSP` overhead drops to zero. Switching between `RadialFft` and
    /// `LinearSpectrum` keeps the active stream open without reconnecting.
    pub fn set_active_mode(&mut self, mode: Option<VisualizerMode>, force_60fps: bool) {
        let next_active = mode.is_some();
        if let Some(m) = mode {
            self.mode = m;
        }
        if self.active == next_active && (!next_active || self.force_60fps == force_60fps) {
            return;
        }
        self.active = next_active;
        self.force_60fps = force_60fps;
        self.last_tick = None;
        self.last_spectrum_targets = [0.0; ERB_BAND_COUNT];
        self.ballistics.reset();
        let _ = self.spectrum_slot.take();
        let max_rate_hz = if force_60fps { 60.0 } else { 40.0 };
        let _ = self.cmd_tx.send(WorkerCommand::SetActive {
            active: next_active,
            max_rate_hz,
        });
    }

    /// Returns the currently active visualizer mode, if visible.
    #[must_use]
    pub const fn active_mode(&self) -> Option<VisualizerMode> {
        if self.active { Some(self.mode) } else { None }
    }

    /// Advances UI ballistics for the current frame and returns the latest display values.
    pub fn tick(&mut self, now: Instant) -> VisualizerOutput {
        if !self.active {
            return VisualizerOutput::default();
        }
        let dt = self
            .last_tick
            .map_or(Duration::from_millis(30), |prev| {
                now.saturating_duration_since(prev)
            })
            .min(Duration::from_millis(100));
        self.last_tick = Some(now);

        if let Some(spec) = self.spectrum_slot.take() {
            self.last_spectrum_targets = self
                .erb_mapper
                .map_to_bands(&spec.frequencies, &spec.magnitudes);
        }
        let (bars, peaks) = self.ballistics.step(&self.last_spectrum_targets, dt);
        VisualizerOutput {
            spectrum_bars: bars,
            spectrum_peaks: peaks,
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
        let mut active_rate_hz: f32 = 0.0;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                WorkerCommand::Shutdown => break,
                WorkerCommand::SetActive {
                    active,
                    max_rate_hz,
                } => {
                    if !active {
                        if let Some(prev) = active_sub.take() {
                            prev.stop().await;
                        }
                        active_rate_hz = 0.0;
                    } else if active_sub.is_none() || (active_rate_hz - max_rate_hz).abs() > 0.1 {
                        if let Some(prev) = active_sub.take() {
                            prev.stop().await;
                        }
                        spectrum_slot.publish(SpectrumSnapshot::silent());
                        active_sub = Some(dsp.start_spectrum_stream(
                            spectrum_subscription_for_rate(max_rate_hz),
                            Arc::clone(&spectrum_slot),
                        ));
                        active_rate_hz = max_rate_hz;
                    }
                }
            }
        }
        if let Some(prev) = active_sub.take() {
            prev.stop().await;
        }
    });
}
