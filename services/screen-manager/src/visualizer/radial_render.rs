//! CPU-efficient `SharedPixelBuffer` renderer for the 48-bar bilateral Radial FFT visualizer.
//!
//! Precomputes a pixel lookup table (LUT) for all 48 rotated spokes at startup so
//! per-frame rendering performs zero trigonometry, zero square roots, and zero heap
//! allocations — Slint composites a single `456×456` `Image` instead of 144 rotated
//! scene-graph rectangles.

use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::dsp_math::ERB_BAND_COUNT;

/// Width and height in pixels of the half-resolution Radial FFT render buffer (`2 * 114`),
/// upscaled 2× by the GPU texture sampler to `456×456` to cut CPU & `glTexImage2D` bandwidth 4×.
pub const RADIAL_BUF_SIZE: u32 = 228;

const RADIAL_BUF_USIZE: usize = RADIAL_BUF_SIZE as usize;
const RADIAL_CENTER: f32 = 114.0;
// Half-resolution geometry (2× upscaled by Slint to match `r_inner: 122px` in visualizer_overlay.slint).
const R_INNER: f32 = 61.0;
const MAX_BAR_LEN: f32 = 51.0;
const R_OUTER: f32 = R_INNER + MAX_BAR_LEN;
const MIN_BAR_LEN: f32 = 1.5;
const SPOKE_HALF_WIDTH: f32 = 2.125;

/// Theme-derived RGB color stops for the Radial FFT visualizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadialPalette {
    /// Inner-most bar color at the hub base (`grad_t = 1.0`).
    pub base: [u8; 3],
    /// Lower-mid bar color (`grad_t = 0.80`).
    pub mid: [u8; 3],
    /// Upper-high bar color (`grad_t = 0.25`).
    pub high: [u8; 3],
    /// Outer tip / peak-level bar color (`grad_t = 0.0`).
    pub tip: [u8; 3],
}

impl Default for RadialPalette {
    fn default() -> Self {
        Self {
            base: [0x07, 0x3C, 0x4D],
            mid: [0x00, 0xC8, 0xFF],
            high: [0x70, 0xE1, 0xFF],
            tip: [0xFF, 0x33, 0x44],
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SpokePixel {
    byte_offset: usize,
    radial_px: f32,
    grad_t: f32,
    alpha_u8: u8,
    bar_premul: [u8; 4],
}

/// Precomputed LUT renderer for the 48-spoke bilateral Radial FFT ring.
///
/// Uses a two-slot ping-pong `SharedPixelBuffer` pair so `make_mut_bytes()` mutates
/// the inactive buffer in place without per-frame heap reallocation while Slint holds
/// the previous frame's `Image`.
#[derive(Debug)]
pub struct RadialFftRenderer {
    band_pixels: [Vec<SpokePixel>; ERB_BAND_COUNT],
    palette: RadialPalette,
    buffers: [SharedPixelBuffer<Rgba8Pixel>; 2],
    next_buf: usize,
}

impl Default for RadialFftRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl RadialFftRenderer {
    /// Builds the 48-spoke pixel LUT and allocates the ping-pong `228×228` RGBA buffers.
    #[must_use]
    pub fn new() -> Self {
        let palette = RadialPalette::default();
        let mut band_pixels: [Vec<SpokePixel>; ERB_BAND_COUNT] =
            std::array::from_fn(|_| Vec::with_capacity(1_800));

        // Map all 24 ERB bands across 48 spokes using 4-quadrant butterfly symmetry:
        // even bands (0,2..22) run 12→3 o'clock and 6→9 o'clock; odd bands (1,3..23)
        // run 6→3 o'clock and 12→9 o'clock. This balances bass at both top & bottom
        // with smooth, seam-free treble transitions on the left & right.
        for (idx, band_vec) in band_pixels.iter_mut().enumerate() {
            #[expect(
                clippy::cast_precision_loss,
                reason = "idx / 2 is in 0..12 which is exact in f32"
            )]
            let step_deg = ((idx / 2) as f32 + 0.5) * 7.5_f32;
            let angles_deg = if idx.is_multiple_of(2) {
                [step_deg, 180.0 + step_deg]
            } else {
                [180.0 - step_deg, 360.0 - step_deg]
            };
            for deg in angles_deg {
                append_spoke_pixels(deg.to_radians(), &palette, band_vec);
            }
        }

        Self {
            band_pixels,
            palette,
            buffers: [
                SharedPixelBuffer::new(RADIAL_BUF_SIZE, RADIAL_BUF_SIZE),
                SharedPixelBuffer::new(RADIAL_BUF_SIZE, RADIAL_BUF_SIZE),
            ],
            next_buf: 0,
        }
    }

    /// Recomputes pre-multiplied bar colors in the LUT when the theme palette changes.
    pub fn set_palette(&mut self, palette: RadialPalette) {
        if self.palette == palette {
            return;
        }
        self.palette = palette;
        for band_vec in &mut self.band_pixels {
            for sp in band_vec {
                sp.bar_premul =
                    premul_rgba(sample_bar_gradient(&self.palette, sp.grad_t), sp.alpha_u8);
            }
        }
    }

    /// Renders `bars` into the inactive ping-pong buffer and returns a Slint [`slint::Image`].
    pub fn render(&mut self, bars: &[f32; ERB_BAND_COUNT], palette: RadialPalette) -> slint::Image {
        self.set_palette(palette);
        let idx = self.next_buf;
        self.next_buf = 1 - idx;
        let bytes = self.buffers[idx].make_mut_bytes();

        for (band_idx, spoke_pixels) in self.band_pixels.iter().enumerate() {
            let bar_len = (MAX_BAR_LEN * bars[band_idx].clamp(0.0, 1.0)).max(MIN_BAR_LEN);
            let bar_end = R_INNER + bar_len;

            for sp in spoke_pixels {
                let dst = &mut bytes[sp.byte_offset..sp.byte_offset + 4];
                if sp.radial_px <= bar_end {
                    dst.copy_from_slice(&sp.bar_premul);
                } else {
                    dst.fill(0);
                }
            }
        }

        slint::Image::from_rgba8_premultiplied(self.buffers[idx].clone())
    }
}

fn append_spoke_pixels(angle_rad: f32, palette: &RadialPalette, out: &mut Vec<SpokePixel>) {
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
            let bar_premul = premul_rgba(sample_bar_gradient(palette, grad_t), alpha_u8);
            out.push(SpokePixel {
                byte_offset: (y * RADIAL_BUF_USIZE + x) * 4,
                radial_px: radial,
                grad_t,
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

fn sample_bar_gradient(palette: &RadialPalette, t: f32) -> [u8; 3] {
    let stops: [(f32, [u8; 3]); 4] = [
        (0.00, palette.tip),
        (0.25, palette.high),
        (0.80, palette.mid),
        (1.00, palette.base),
    ];

    let t = t.clamp(0.0, 1.0);
    for pair in stops.windows(2) {
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
    stops[3].1
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
