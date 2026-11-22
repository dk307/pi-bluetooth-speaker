//! Bridge between [`crate::visualizer`] skin/telemetry models and Slint's [`MainWindow`].

use std::cell::RefCell;
use std::rc::Rc;

use image::GenericImageView;
use slint::{Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::ui::{MainWindow, SpectrumBandItem};
use crate::visualizer::dsp_math::ERB_BAND_COUNT;
use crate::visualizer::skins::{
    LinearDirection, LinearGeometry, SkinGeometry, VuSkin, all_skins, embedded_skin_bytes,
    find_skin,
};
use crate::visualizer::{VisualizerMode, VisualizerOutput};

/// Holds the active decoded skin textures, pre-allocated spectrum model, and cached geometry.
pub struct VisualizerUiState {
    skins: Vec<VuSkin>,
    loaded_skin_name: RefCell<String>,
    loaded_skin: RefCell<Option<VuSkin>>,
    linear_indicator_size: RefCell<(f32, f32)>,
    spectrum_bands: Rc<VecModel<SpectrumBandItem>>,
}

impl std::fmt::Debug for VisualizerUiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualizerUiState")
            .field("loaded_skin_name", &self.loaded_skin_name.borrow())
            .field("skin_count", &self.skins.len())
            .finish_non_exhaustive()
    }
}

impl Default for VisualizerUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl VisualizerUiState {
    /// Initializes the skin registry and pre-allocates the 24-band spectrum `VecModel`.
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
            skins: all_skins(),
            loaded_skin_name: RefCell::new(String::new()),
            loaded_skin: RefCell::new(None),
            linear_indicator_size: RefCell::new((0.0, 0.0)),
            spectrum_bands,
        }
    }

    /// Binds the pre-allocated spectrum model to `window`.
    pub fn bind_to_window(&self, window: &MainWindow) {
        window.set_viz_spectrum_bands(ModelRc::from(Rc::clone(&self.spectrum_bands)));
    }

    /// Ensures the requested `skin_name` is decoded and uploaded to `window` (no-op if unchanged).
    pub fn ensure_skin_loaded(&self, window: &MainWindow, skin_name: &str) {
        if self
            .loaded_skin_name
            .borrow()
            .eq_ignore_ascii_case(skin_name)
            && self.loaded_skin.borrow().is_some()
        {
            return;
        }
        let Some(skin) = find_skin(&self.skins, skin_name).cloned() else {
            return;
        };

        if let Some(bgr) = decode_embedded_image(&skin.bgr_filename, false) {
            window.set_viz_vu_bgr(bgr.image);
        }
        if let Some(fgr_name) = &skin.fgr_filename
            && let Some(fgr) = decode_embedded_image(fgr_name, false)
        {
            window.set_viz_vu_fgr(fgr.image);
            window.set_viz_has_fgr(true);
        } else {
            window.set_viz_vu_fgr(slint::Image::default());
            window.set_viz_has_fgr(false);
        }

        let Some(indicator) = decode_embedded_image(&skin.indicator_filename, false) else {
            return;
        };
        let ind_w = indicator.width as f32;
        let ind_h = indicator.height as f32;
        window.set_viz_vu_indicator(indicator.image);

        match &skin.geometry {
            SkinGeometry::Circular(c) => {
                window.set_viz_is_circular(true);
                window.set_viz_is_stereo(skin.channels > 1);
                window.set_viz_vu_indicator_flipped(slint::Image::default());
                *self.linear_indicator_size.borrow_mut() = (0.0, 0.0);

                let pivot_x = ind_w / 2.0;
                let pivot_y = ind_h / 2.0 + c.distance;
                window.set_viz_left_pivot_x(pivot_x);
                window.set_viz_left_pivot_y(pivot_y);
                window.set_viz_left_needle_x(c.left_origin_x - pivot_x);
                window.set_viz_left_needle_y(c.left_origin_y - pivot_y);

                window.set_viz_right_pivot_x(pivot_x);
                window.set_viz_right_pivot_y(pivot_y);
                window.set_viz_right_needle_x(c.right_origin_x - pivot_x);
                window.set_viz_right_needle_y(c.right_origin_y - pivot_y);

                let (l_deg, r_deg) = skin.circular_angles(0.0, 0.0);
                window.set_viz_left_angle_deg(l_deg);
                window.set_viz_right_angle_deg(r_deg);
            }
            SkinGeometry::Linear(l) => {
                window.set_viz_is_circular(false);
                window.set_viz_is_stereo(skin.channels > 1);
                window.set_viz_linear_ind_w(ind_w);
                window.set_viz_linear_ind_h(ind_h);
                window.set_viz_linear_left_flipped(l.flip_left_x);
                window.set_viz_linear_right_flipped(l.flip_right_x);
                *self.linear_indicator_size.borrow_mut() = (ind_w, ind_h);

                if l.flip_left_x || l.flip_right_x {
                    if let Some(flipped) = decode_embedded_image(&skin.indicator_filename, true) {
                        window.set_viz_vu_indicator_flipped(flipped.image);
                    }
                } else {
                    window.set_viz_vu_indicator_flipped(slint::Image::default());
                }

                apply_linear_rects(window, l, ind_w, ind_h, 0.0, 0.0);
            }
        }

        self.loaded_skin_name.borrow_mut().clone_from(&skin.name);
        *self.loaded_skin.borrow_mut() = Some(skin);
    }

    /// Pushes a [`VisualizerOutput`] frame to `window` without reallocating models.
    pub fn apply_output(&self, window: &MainWindow, mode: VisualizerMode, out: &VisualizerOutput) {
        match mode {
            VisualizerMode::VuMeter => {
                if let Some(skin) = self.loaded_skin.borrow().as_ref() {
                    match &skin.geometry {
                        SkinGeometry::Circular(_) => {
                            let (l_deg, r_deg) =
                                skin.circular_angles(out.left_vu_ratio, out.right_vu_ratio);
                            window.set_viz_left_angle_deg(l_deg);
                            window.set_viz_right_angle_deg(r_deg);
                        }
                        SkinGeometry::Linear(l) => {
                            let (ind_w, ind_h) = *self.linear_indicator_size.borrow();
                            apply_linear_rects(
                                window,
                                l,
                                ind_w,
                                ind_h,
                                out.left_vu_ratio,
                                out.right_vu_ratio,
                            );
                        }
                    }
                }
            }
            VisualizerMode::Spectrum => {
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
    }
}

struct DecodedImage {
    image: slint::Image,
    width: u32,
    height: u32,
}

fn decode_embedded_image(filename: &str, flip_x: bool) -> Option<DecodedImage> {
    let bytes = embedded_skin_bytes(filename)?;
    let mut img = image::load_from_memory(bytes).ok()?;
    if flip_x {
        img = img.fliph();
    }
    let (width, height) = img.dimensions();
    let rgba = img.to_rgba8();
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), width, height);
    Some(DecodedImage {
        image: slint::Image::from_rgba8(buf),
        width,
        height,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LinearChannelRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    img_x: f32,
    img_y: f32,
}

fn stepped_span(l: &LinearGeometry, ratio: f32) -> f32 {
    let clamped = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let total_steps = l.position_regular + l.position_overload;
    #[expect(
        clippy::cast_sign_loss,
        reason = "clamped is finite in 0.0..=1.0 so product is non-negative"
    )]
    let lit_steps = ((clamped * total_steps as f32).round() as u32).min(total_steps);
    let reg_lit = lit_steps.min(l.position_regular);
    let ovl_lit = lit_steps.saturating_sub(l.position_regular);
    reg_lit as f32 * l.step_width_regular + ovl_lit as f32 * l.step_width_overload
}

fn compute_linear_channel_rect(
    l: &LinearGeometry,
    is_right: bool,
    ind_w: f32,
    ind_h: f32,
    ratio: f32,
) -> LinearChannelRect {
    let (ch_x, ch_y, flipped) = if is_right {
        (l.right_x, l.right_y, l.flip_right_x)
    } else {
        (l.left_x, l.left_y, l.flip_left_x)
    };

    if l.single_indicator {
        let clamped = if ratio.is_finite() {
            ratio.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let travel = l.position_regular as f32 * l.step_width_regular;
        return LinearChannelRect {
            x: ch_x + clamped * travel,
            y: ch_y,
            w: ind_w,
            h: ind_h,
            img_x: 0.0,
            img_y: 0.0,
        };
    }

    let span = stepped_span(l, ratio);
    match l.direction {
        LinearDirection::LeftRight => {
            let w = span.min(ind_w);
            LinearChannelRect {
                x: ch_x,
                y: ch_y,
                w,
                h: ind_h,
                img_x: 0.0,
                img_y: 0.0,
            }
        }
        LinearDirection::BottomTop => {
            let h = span.min(ind_h);
            let hidden = ind_h - h;
            LinearChannelRect {
                x: ch_x,
                y: ch_y + hidden,
                w: ind_w,
                h,
                img_x: 0.0,
                img_y: -hidden,
            }
        }
        LinearDirection::TopBottom => {
            let h = span.min(ind_h);
            LinearChannelRect {
                x: ch_x,
                y: ch_y,
                w: ind_w,
                h,
                img_x: 0.0,
                img_y: 0.0,
            }
        }
        LinearDirection::CenterEdges | LinearDirection::EdgesCenter => {
            let w = span.min(ind_w);
            if flipped {
                let hidden = ind_w - w;
                LinearChannelRect {
                    x: ch_x - w,
                    y: ch_y,
                    w,
                    h: ind_h,
                    img_x: -hidden,
                    img_y: 0.0,
                }
            } else {
                LinearChannelRect {
                    x: ch_x,
                    y: ch_y,
                    w,
                    h: ind_h,
                    img_x: 0.0,
                    img_y: 0.0,
                }
            }
        }
    }
}

fn apply_linear_rects(
    window: &MainWindow,
    l: &LinearGeometry,
    ind_w: f32,
    ind_h: f32,
    left_ratio: f32,
    right_ratio: f32,
) {
    let left = compute_linear_channel_rect(l, false, ind_w, ind_h, left_ratio);
    window.set_viz_linear_left_x(left.x);
    window.set_viz_linear_left_y(left.y);
    window.set_viz_linear_left_w(left.w);
    window.set_viz_linear_left_h(left.h);
    window.set_viz_linear_left_img_x(left.img_x);
    window.set_viz_linear_left_img_y(left.img_y);

    let right = compute_linear_channel_rect(l, true, ind_w, ind_h, right_ratio);
    window.set_viz_linear_right_x(right.x);
    window.set_viz_linear_right_y(right.y);
    window.set_viz_linear_right_w(right.w);
    window.set_viz_linear_right_h(right.h);
    window.set_viz_linear_right_img_x(right.img_x);
    window.set_viz_linear_right_img_y(right.img_y);
}
