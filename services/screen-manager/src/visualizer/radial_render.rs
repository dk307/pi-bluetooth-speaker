//! CPU-efficient `SharedPixelBuffer` renderer for the 48-bar bilateral Radial FFT visualizer.
//!
//! Precomputes a pixel lookup table (LUT) for all 48 rotated spokes at startup so
//! per-frame rendering performs zero trigonometry, zero square roots, and zero heap
//! allocations — Slint composites a single `456×456` `Image` instead of 144 rotated
//! scene-graph rectangles.

use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::dsp_math::ERB_BAND_COUNT;

/// Width and height in pixels of the square Radial FFT render buffer (`2 * 228`).
pub const RADIAL_BUF_SIZE: u32 = 456;

const RADIAL_BUF_USIZE: usize = RADIAL_BUF_SIZE as usize;
const RADIAL_CENTER: f32 = 228.0;
const R_INNER: f32 = 88.0;
const MAX_BAR_LEN: f32 = 136.0;
const R_OUTER: f32 = R_INNER + MAX_BAR_LEN;
const MIN_BAR_LEN: f32 = 3.0;
const PEAK_CAP_THICKNESS: f32 = 3.0;
const SPOKE_HALF_WIDTH: f32 = 3.0;

#[derive(Debug, Clone, Copy)]
struct SpokePixel {
    byte_offset: usize,
    radial_px: f32,
    alpha_u8: u8,
    bar_premul: [u8; 4],
}

/// Precomputed LUT renderer for the 48-spoke bilateral Radial FFT ring.
#[derive(Debug)]
pub struct RadialFftRenderer {
    band_pixels: [Vec<SpokePixel>; ERB_BAND_COUNT],
    pixels: SharedPixelBuffer<Rgba8Pixel>,
}

impl Default for RadialFftRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl RadialFftRenderer {
    /// Builds the 48-spoke pixel LUT and allocates the `456×456` RGBA pixel buffer.
    #[must_use]
    pub fn new() -> Self {
        let mut band_pixels: [Vec<SpokePixel>; ERB_BAND_COUNT] =
            std::array::from_fn(|_| Vec::with_capacity(1_800));

        for (idx, band_vec) in band_pixels.iter_mut().enumerate() {
            for side in [-1.0_f32, 1.0_f32] {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "idx is in 0..24 which is exact in f32"
                )]
                let angle_rad = side * (idx as f32 + 0.5) * 7.5_f32.to_radians();
                append_spoke_pixels(angle_rad, band_vec);
            }
        }

        Self {
            band_pixels,
            pixels: SharedPixelBuffer::new(RADIAL_BUF_SIZE, RADIAL_BUF_SIZE),
        }
    }

    /// Renders `bars` and `peaks` into the internal buffer and returns a Slint [`slint::Image`].
    pub fn render(
        &mut self,
        bars: &[f32; ERB_BAND_COUNT],
        peaks: &[f32; ERB_BAND_COUNT],
        peak_rgb: [u8; 3],
    ) -> slint::Image {
        let bytes = self.pixels.make_mut_bytes();
        bytes.fill(0);

        for (band_idx, spoke_pixels) in self.band_pixels.iter().enumerate() {
            let bar_len = (MAX_BAR_LEN * bars[band_idx].clamp(0.0, 1.0)).max(MIN_BAR_LEN);
            let bar_end = R_INNER + bar_len;

            let peak_len = (MAX_BAR_LEN * peaks[band_idx].clamp(0.0, 1.0)).max(MIN_BAR_LEN);
            let peak_end = (R_INNER + peak_len + PEAK_CAP_THICKNESS).min(R_OUTER);
            let peak_start = peak_end - PEAK_CAP_THICKNESS;

            for sp in spoke_pixels {
                let dst = &mut bytes[sp.byte_offset..sp.byte_offset + 4];
                if sp.radial_px >= peak_start && sp.radial_px <= peak_end {
                    dst.copy_from_slice(&premul_rgba(peak_rgb, sp.alpha_u8));
                } else if sp.radial_px <= bar_end {
                    dst.copy_from_slice(&sp.bar_premul);
                }
            }
        }

        slint::Image::from_rgba8_premultiplied(self.pixels.clone())
    }
}

fn append_spoke_pixels(angle_rad: f32, out: &mut Vec<SpokePixel>) {
    let ux = angle_rad.sin();
    let uy = -angle_rad.cos();
    let px = angle_rad.cos();
    let py = angle_rad.sin();

    let corners = [
        (
            RADIAL_CENTER + ux * R_INNER - px * SPOKE_HALF_WIDTH,
            RADIAL_CENTER + uy * R_INNER - py * SPOKE_HALF_WIDTH,
        ),
        (
            RADIAL_CENTER + ux * R_INNER + px * SPOKE_HALF_WIDTH,
            RADIAL_CENTER + uy * R_INNER + py * SPOKE_HALF_WIDTH,
        ),
        (
            RADIAL_CENTER + ux * R_OUTER - px * SPOKE_HALF_WIDTH,
            RADIAL_CENTER + uy * R_OUTER - py * SPOKE_HALF_WIDTH,
        ),
        (
            RADIAL_CENTER + ux * R_OUTER + px * SPOKE_HALF_WIDTH,
            RADIAL_CENTER + uy * R_OUTER + py * SPOKE_HALF_WIDTH,
        ),
    ];

    let min_x = clamp_coord(
        corners
            .iter()
            .map(|c| c.0)
            .fold(f32::INFINITY, f32::min)
            .floor()
            - 1.0,
    );
    let max_x = clamp_coord(
        corners
            .iter()
            .map(|c| c.0)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil()
            + 1.0,
    );
    let min_y = clamp_coord(
        corners
            .iter()
            .map(|c| c.1)
            .fold(f32::INFINITY, f32::min)
            .floor()
            - 1.0,
    );
    let max_y = clamp_coord(
        corners
            .iter()
            .map(|c| c.1)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil()
            + 1.0,
    );

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            #[expect(
                clippy::cast_precision_loss,
                reason = "x and y are bounded by 0..456 which is exact in f32"
            )]
            let rx = x as f32 + 0.5 - RADIAL_CENTER;
            #[expect(
                clippy::cast_precision_loss,
                reason = "x and y are bounded by 0..456 which is exact in f32"
            )]
            let ry = y as f32 + 0.5 - RADIAL_CENTER;

            let radial = rx * ux + ry * uy;
            if !(R_INNER..=R_OUTER).contains(&radial) {
                continue;
            }
            let perp = (rx * px + ry * py).abs();
            if perp >= SPOKE_HALF_WIDTH {
                continue;
            }

            let alpha_u8 = unit_to_u8((SPOKE_HALF_WIDTH - perp).clamp(0.0, 1.0));
            if alpha_u8 == 0 {
                continue;
            }

            let grad_t = ((R_OUTER - radial) / MAX_BAR_LEN).clamp(0.0, 1.0);
            let bar_premul = premul_rgba(sample_bar_gradient(grad_t), alpha_u8);
            out.push(SpokePixel {
                byte_offset: (y * RADIAL_BUF_USIZE + x) * 4,
                radial_px: radial,
                alpha_u8,
                bar_premul,
            });
        }
    }
}

fn clamp_coord(v: f32) -> usize {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0.0..=(RADIAL_BUF_SIZE - 1) before cast"
    )]
    {
        v.clamp(0.0, (RADIAL_BUF_USIZE - 1) as f32) as usize
    }
}

fn unit_to_u8(v: f32) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0.0..=255.0 before cast"
    )]
    {
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    }
}

#[inline]
fn premul_rgba(rgb: [u8; 3], alpha: u8) -> [u8; 4] {
    let a = u16::from(alpha);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "(u8 * u8 + 127) / 255 is strictly in 0..=255"
    )]
    let r = ((u16::from(rgb[0]) * a + 127) / 255) as u8;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "(u8 * u8 + 127) / 255 is strictly in 0..=255"
    )]
    let g = ((u16::from(rgb[1]) * a + 127) / 255) as u8;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "(u8 * u8 + 127) / 255 is strictly in 0..=255"
    )]
    let b = ((u16::from(rgb[2]) * a + 127) / 255) as u8;
    [r, g, b, alpha]
}

fn sample_bar_gradient(t: f32) -> [u8; 3] {
    const STOPS: [(f32, [u8; 3]); 4] = [
        (0.00, [0xFF, 0x52, 0x52]),
        (0.28, [0xFF, 0xB3, 0x00]),
        (0.68, [0x26, 0xE6, 0xA4]),
        (1.00, [0x00, 0xB8, 0xD4]),
    ];

    let t = t.clamp(0.0, 1.0);
    for pair in STOPS.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if t <= t1 {
            let local = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
            return [
                lerp_u8(c0[0], c1[0], local),
                lerp_u8(c0[1], c1[1], local),
                lerp_u8(c0[2], c1[2], local),
            ];
        }
    }
    STOPS[3].1
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let af = f32::from(a);
    let bf = f32::from(b);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "linear interpolation between two u8 values is strictly in 0.0..=255.0"
    )]
    {
        (af + (bf - af) * t).round().clamp(0.0, 255.0) as u8
    }
}
