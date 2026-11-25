//! Bridge between [`crate::visualizer`] spectrum telemetry and Slint's [`MainWindow`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::ui::{MainWindow, MaterialPalette, SpectrumBandItem};
use crate::visualizer::dsp_math::ERB_BAND_COUNT;
use crate::visualizer::{RadialFftRenderer, RadialPalette, VisualizerMode, VisualizerOutput};

/// Minimum normalized bar/peak delta (~0.4 px) before pushing a Slint update.
const SUBPIXEL_DEADBAND: f32 = 0.003;

/// Holds the pre-allocated 24-band [`VecModel`] and `SharedPixelBuffer` Radial FFT renderer.
pub struct VisualizerUiState {
    spectrum_bands: Rc<VecModel<SpectrumBandItem>>,
    radial_renderer: RefCell<RadialFftRenderer>,
    last_linear_bars: Cell<[f32; ERB_BAND_COUNT]>,
    last_linear_peaks: Cell<[f32; ERB_BAND_COUNT]>,
    last_radial_bars: Cell<[f32; ERB_BAND_COUNT]>,
    last_radial_peaks: Cell<[f32; ERB_BAND_COUNT]>,
    last_radial_palette: Cell<Option<RadialPalette>>,
    radial_initialized: Cell<bool>,
}

impl std::fmt::Debug for VisualizerUiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualizerUiState")
            .field("band_count", &self.spectrum_bands.row_count())
            .field("radial_initialized", &self.radial_initialized.get())
            .finish_non_exhaustive()
    }
}

impl Default for VisualizerUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl VisualizerUiState {
    /// Pre-allocates the 24-band spectrum `VecModel` and the 48-spoke Radial FFT LUT/buffer.
    #[must_use]
    pub fn new() -> Self {
        let spectrum_bands = Rc::new(VecModel::from(vec![
            SpectrumBandItem {
                value: 0.0,
                peak: 0.0,
            };
            ERB_BAND_COUNT
        ]));
        Self {
            spectrum_bands,
            radial_renderer: RefCell::new(RadialFftRenderer::new()),
            last_linear_bars: Cell::new([0.0; ERB_BAND_COUNT]),
            last_linear_peaks: Cell::new([0.0; ERB_BAND_COUNT]),
            last_radial_bars: Cell::new([0.0; ERB_BAND_COUNT]),
            last_radial_peaks: Cell::new([0.0; ERB_BAND_COUNT]),
            last_radial_palette: Cell::new(None),
            radial_initialized: Cell::new(false),
        }
    }

    /// Binds the pre-allocated spectrum model and initial Radial FFT image to `window`.
    pub fn bind_to_window(&self, window: &MainWindow) {
        window.set_viz_spectrum_bands(ModelRc::from(Rc::clone(&self.spectrum_bands)));
        let zero = VisualizerOutput::default();
        self.render_radial_to_window(window, &zero, radial_palette(window));
    }

    /// Pushes a [`VisualizerOutput`] frame to the active mode's Slint target only.
    pub fn apply_for_mode(
        &self,
        window: &MainWindow,
        mode: VisualizerMode,
        out: &VisualizerOutput,
    ) {
        match mode {
            VisualizerMode::RadialFft => {
                let palette = radial_palette(window);
                if !self.radial_initialized.get()
                    || self.last_radial_palette.get() != Some(palette)
                    || frame_changed(
                        &self.last_radial_bars.get(),
                        &self.last_radial_peaks.get(),
                        out,
                    )
                {
                    self.render_radial_to_window(window, out, palette);
                }
            }
            VisualizerMode::LinearSpectrum => {
                self.apply_output(out);
            }
        }
    }

    /// Pushes a [`VisualizerOutput`] frame to `spectrum_bands`, skipping sub-pixel changes.
    pub fn apply_output(&self, out: &VisualizerOutput) {
        let mut prev_bars = self.last_linear_bars.get();
        let mut prev_peaks = self.last_linear_peaks.get();
        let mut any_changed = false;

        for i in 0..ERB_BAND_COUNT {
            let v = out.spectrum_bars[i];
            let p = out.spectrum_peaks[i];
            if value_changed(prev_bars[i], v) || value_changed(prev_peaks[i], p) {
                self.spectrum_bands
                    .set_row_data(i, SpectrumBandItem { value: v, peak: p });
                prev_bars[i] = v;
                prev_peaks[i] = p;
                any_changed = true;
            }
        }

        if any_changed {
            self.last_linear_bars.set(prev_bars);
            self.last_linear_peaks.set(prev_peaks);
        }
    }

    fn render_radial_to_window(
        &self,
        window: &MainWindow,
        out: &VisualizerOutput,
        palette: RadialPalette,
    ) {
        let img = self.radial_renderer.borrow_mut().render(
            &out.spectrum_bars,
            &out.spectrum_peaks,
            palette,
        );
        window.set_viz_radial_image(img);
        self.last_radial_bars.set(out.spectrum_bars);
        self.last_radial_peaks.set(out.spectrum_peaks);
        self.last_radial_palette.set(Some(palette));
        self.radial_initialized.set(true);
    }
}

fn color_rgb(c: slint::Color) -> [u8; 3] {
    [c.red(), c.green(), c.blue()]
}

fn radial_palette(window: &MainWindow) -> RadialPalette {
    let p = window.global::<MaterialPalette>();
    RadialPalette {
        base: color_rgb(p.get_primary_container()),
        mid: color_rgb(p.get_primary()),
        high: color_rgb(p.get_secondary()),
        tip: color_rgb(p.get_tertiary()),
        peak: color_rgb(p.get_on_surface()),
    }
}

fn value_changed(prev: f32, next: f32) -> bool {
    (next - prev).abs() >= SUBPIXEL_DEADBAND || (next == 0.0 && prev != 0.0)
}

fn frame_changed(
    prev_bars: &[f32; ERB_BAND_COUNT],
    prev_peaks: &[f32; ERB_BAND_COUNT],
    out: &VisualizerOutput,
) -> bool {
    for i in 0..ERB_BAND_COUNT {
        if value_changed(prev_bars[i], out.spectrum_bars[i])
            || value_changed(prev_peaks[i], out.spectrum_peaks[i])
        {
            return true;
        }
    }
    false
}
