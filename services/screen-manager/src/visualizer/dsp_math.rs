//! Perceptual ERB spectrum binning, pink-noise tilt, and frame-rate-independent ballistics.

use std::sync::Arc;
use std::time::Duration;

/// Number of perceptual ERB spectrum bands rendered in the spectrum visualizer.
pub const ERB_BAND_COUNT: usize = 24;

/// Lower frequency bound of the spectrum display in Hz.
pub const SPECTRUM_MIN_HZ: f64 = 30.0;

/// Upper frequency bound of the spectrum display in Hz.
pub const SPECTRUM_MAX_HZ: f64 = 18_000.0;

/// Minimum dBFS floor mapped to `0.0` normalized height.
pub const DISPLAY_MIN_DBFS: f32 = -60.0;

/// Maximum dBFS ceiling mapped to `1.0` normalized height.
pub const DISPLAY_MAX_DBFS: f32 = 0.0;

/// Fast spectrum attack time constant (`15 ms` per spec §3.1).
const SPECTRUM_ATTACK_TAU_SECS: f32 = 0.015;

/// Fluid spectrum decay time constant (`180 ms` per spec §3.1).
const SPECTRUM_DECAY_TAU_SECS: f32 = 0.180;

/// Peak-hold latch duration (`500 ms` per spec §3.1: 450–600 ms).
const PEAK_HOLD_DURATION: Duration = Duration::from_millis(500);

/// Linear dB-space peak-cap decay rate in normalized `0.0..=1.0` units per second
/// (`0.50 / s` = `30 dB/s` over the 60 dB display range).
const PEAK_DECAY_PER_SEC: f32 = 0.50;

/// Converts frequency in Hz to the Glasberg–Moore ERB-rate scale:
/// `ERB(f) = 21.4 * log10(0.00437 * f + 1)`.
#[must_use]
pub fn hz_to_erb(freq_hz: f64) -> f64 {
    21.4 * (0.00437 * freq_hz.max(0.0) + 1.0).log10()
}

/// Converts a Glasberg–Moore ERB-rate value back to frequency in Hz.
#[must_use]
pub fn erb_to_hz(erb: f64) -> f64 {
    (10.0_f64.powf(erb / 21.4) - 1.0) / 0.00437
}

/// Maps a dBFS value onto `0.0..=1.0` over `[DISPLAY_MIN_DBFS, DISPLAY_MAX_DBFS]`.
#[must_use]
pub fn normalize_dbfs(dbfs: f32) -> f32 {
    if !dbfs.is_finite() {
        return 0.0;
    }
    ((dbfs - DISPLAY_MIN_DBFS) / (DISPLAY_MAX_DBFS - DISPLAY_MIN_DBFS)).clamp(0.0, 1.0)
}

/// Computes frame-rate-independent exponential smoothing coefficient `1 - e^(-dt / tau)`.
#[must_use]
pub fn smoothing_coeff(dt: Duration, tau_secs: f32) -> f32 {
    if tau_secs <= 0.0 {
        return 1.0;
    }
    let dt_secs = dt.as_secs_f32().max(0.0);
    if dt_secs == 0.0 {
        return 0.0;
    }
    (1.0 - (-dt_secs / tau_secs).exp()).clamp(0.0, 1.0)
}

/// Maps `CamillaDSP`'s log-spaced spectrum bins into [`ERB_BAND_COUNT`] perceptual bands
/// with `+3.0 dB/octave` pink-noise tilt (referenced to 1 kHz) applied per bin.
#[derive(Debug, Clone)]
pub struct ErbBandMapper {
    band_edges_hz: [f64; ERB_BAND_COUNT + 1],
    cached_freqs: Arc<[f32]>,
    bin_tilts_db: Vec<f32>,
    bin_ranges: [(usize, usize); ERB_BAND_COUNT],
}

impl Default for ErbBandMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl ErbBandMapper {
    /// Creates a new 24-band Glasberg–Moore ERB mapper spanning `30 Hz .. 18 kHz`.
    #[must_use]
    pub fn new() -> Self {
        let erb_min = hz_to_erb(SPECTRUM_MIN_HZ);
        let erb_max = hz_to_erb(SPECTRUM_MAX_HZ);
        let mut band_edges_hz = [0.0_f64; ERB_BAND_COUNT + 1];
        for (i, edge) in band_edges_hz.iter_mut().enumerate() {
            let frac = i as f64 / ERB_BAND_COUNT as f64;
            *edge = erb_to_hz(erb_min + frac * (erb_max - erb_min));
        }

        Self {
            band_edges_hz,
            cached_freqs: Arc::from([]),
            bin_tilts_db: Vec::new(),
            bin_ranges: [(0, 0); ERB_BAND_COUNT],
        }
    }

    /// Returns the 24 band center frequencies in Hz.
    #[must_use]
    pub fn center_frequencies_hz(&self) -> [f64; ERB_BAND_COUNT] {
        let mut centers = [0.0_f64; ERB_BAND_COUNT];
        for (i, center) in centers.iter_mut().enumerate() {
            *center = (self.band_edges_hz[i] * self.band_edges_hz[i + 1]).sqrt();
        }
        centers
    }

    /// Maps an incoming `CamillaDSP` spectrum frame into 24 normalized `0.0..=1.0` band targets.
    ///
    /// Recomputes the bin-to-band lookup table and per-bin `+3.0 dB/octave` tilt
    /// only when `frequencies` changes. Returns all zeros when empty.
    pub fn map_to_bands(
        &mut self,
        frequencies: &Arc<[f32]>,
        magnitudes: &[f32],
    ) -> [f32; ERB_BAND_COUNT] {
        if frequencies.is_empty() || magnitudes.is_empty() {
            return [0.0; ERB_BAND_COUNT];
        }
        if !Arc::ptr_eq(&self.cached_freqs, frequencies)
            && self.cached_freqs.as_ref() != frequencies.as_ref()
        {
            self.rebuild_lut(frequencies);
        }

        let mut out = [0.0_f32; ERB_BAND_COUNT];
        let count = magnitudes.len().min(self.bin_tilts_db.len());
        for (band_idx, &(start, end)) in self.bin_ranges.iter().enumerate() {
            if start >= end || start >= count {
                continue;
            }
            let clamped_end = end.min(count);
            let mut peak_tilted_db = f32::NEG_INFINITY;
            for (mag, tilt) in magnitudes[start..clamped_end]
                .iter()
                .zip(&self.bin_tilts_db[start..clamped_end])
            {
                if mag.is_finite() {
                    let tilted = *mag + *tilt;
                    if tilted > peak_tilted_db {
                        peak_tilted_db = tilted;
                    }
                }
            }
            if peak_tilted_db.is_finite() {
                out[band_idx] = normalize_dbfs(peak_tilted_db);
            }
        }
        out
    }

    fn rebuild_lut(&mut self, frequencies: &Arc<[f32]>) {
        self.cached_freqs = Arc::clone(frequencies);
        let freqs = frequencies.as_ref();
        self.bin_tilts_db = freqs
            .iter()
            .map(|&f| {
                if f.is_finite() && f > 0.0 {
                    3.0 * (f / 1000.0).log2()
                } else {
                    0.0
                }
            })
            .collect();
        for (band_idx, range) in self.bin_ranges.iter_mut().enumerate() {
            let lo = self.band_edges_hz[band_idx] as f32;
            let hi = self.band_edges_hz[band_idx + 1] as f32;
            let start = freqs.partition_point(|&f| f < lo);
            let mut end = freqs.partition_point(|&f| f < hi);
            // Guarantee every band maps at least the nearest bin if the grid is sparse.
            if start == end && !freqs.is_empty() {
                let nearest = start.min(freqs.len() - 1);
                *range = (nearest, nearest + 1);
            } else {
                if band_idx + 1 == ERB_BAND_COUNT {
                    end = freqs.len();
                }
                *range = (start, end);
            }
        }
    }
}

/// Frame-rate-independent ballistics & peak-hold processor for 24 spectrum bands.
#[derive(Debug, Clone)]
pub struct SpectrumBallistics {
    bars: [f32; ERB_BAND_COUNT],
    peaks: [f32; ERB_BAND_COUNT],
    peak_hold_remaining: [Duration; ERB_BAND_COUNT],
}

impl Default for SpectrumBallistics {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectrumBallistics {
    /// Creates a zero-initialized ballistics processor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bars: [0.0; ERB_BAND_COUNT],
            peaks: [0.0; ERB_BAND_COUNT],
            peak_hold_remaining: [Duration::ZERO; ERB_BAND_COUNT],
        }
    }

    /// Resets all bars and peak caps to zero.
    pub fn reset(&mut self) {
        self.bars = [0.0; ERB_BAND_COUNT];
        self.peaks = [0.0; ERB_BAND_COUNT];
        self.peak_hold_remaining = [Duration::ZERO; ERB_BAND_COUNT];
    }

    /// Advances bars and peak-hold caps by `dt` toward `targets`, returning `(bars, peaks)`.
    pub fn step(
        &mut self,
        targets: &[f32; ERB_BAND_COUNT],
        dt: Duration,
    ) -> ([f32; ERB_BAND_COUNT], [f32; ERB_BAND_COUNT]) {
        let attack_alpha = smoothing_coeff(dt, SPECTRUM_ATTACK_TAU_SECS);
        let decay_alpha = smoothing_coeff(dt, SPECTRUM_DECAY_TAU_SECS);
        let peak_drop = PEAK_DECAY_PER_SEC * dt.as_secs_f32().max(0.0);

        for (i, &raw_target) in targets.iter().enumerate().take(ERB_BAND_COUNT) {
            let target = if raw_target.is_finite() {
                raw_target.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let current = self.bars[i];
            let alpha = if target >= current {
                attack_alpha
            } else {
                decay_alpha
            };
            let next_bar = (current + alpha * (target - current)).clamp(0.0, 1.0);
            self.bars[i] = next_bar;

            if next_bar >= self.peaks[i] {
                self.peaks[i] = next_bar;
                self.peak_hold_remaining[i] = PEAK_HOLD_DURATION;
            } else if let Some(rem) = self.peak_hold_remaining[i].checked_sub(dt)
                && !rem.is_zero()
            {
                self.peak_hold_remaining[i] = rem;
            } else {
                self.peak_hold_remaining[i] = Duration::ZERO;
                self.peaks[i] = (self.peaks[i] - peak_drop).max(next_bar);
            }
        }

        (self.bars, self.peaks)
    }
}
