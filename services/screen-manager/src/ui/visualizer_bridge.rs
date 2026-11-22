//! Bridge between [`crate::visualizer`] spectrum telemetry and Slint's [`MainWindow`].

use std::rc::Rc;

use slint::{Model, ModelRc, VecModel};

use crate::ui::{MainWindow, SpectrumBandItem};
use crate::visualizer::VisualizerOutput;
use crate::visualizer::dsp_math::ERB_BAND_COUNT;

/// Holds the pre-allocated 24-band [`VecModel`] updated in place at 33 Hz.
pub struct VisualizerUiState {
    spectrum_bands: Rc<VecModel<SpectrumBandItem>>,
}

impl std::fmt::Debug for VisualizerUiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualizerUiState")
            .field("band_count", &self.spectrum_bands.row_count())
            .finish()
    }
}

impl Default for VisualizerUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl VisualizerUiState {
    /// Pre-allocates the 24-band spectrum `VecModel`.
    #[must_use]
    pub fn new() -> Self {
        let spectrum_bands = Rc::new(VecModel::from(vec![
            SpectrumBandItem {
                value: 0.0,
                peak: 0.0,
            };
            ERB_BAND_COUNT
        ]));
        Self { spectrum_bands }
    }

    /// Binds the pre-allocated spectrum model to `window`.
    pub fn bind_to_window(&self, window: &MainWindow) {
        window.set_viz_spectrum_bands(ModelRc::from(Rc::clone(&self.spectrum_bands)));
    }

    /// Pushes a [`VisualizerOutput`] frame to `window` in place without reallocating.
    pub fn apply_output(&self, out: &VisualizerOutput) {
        for i in 0..ERB_BAND_COUNT {
            self.spectrum_bands.set_row_data(
                i,
                SpectrumBandItem {
                    value: out.spectrum_bars[i],
                    peak: out.spectrum_peaks[i],
                },
            );
        }
    }
}
