//! Premultiplied RGBA pixel type shared with Slint's software renderer.
//!
//! The KMS/FemtoVG migration removed the offscreen marquee strip rasterizer
//! (`MarqueeStripRasterizer`, `MarqueeStripCache`, `MarqueeTypography`): under
//! `backend-linuxkms` any secondary Slint component eagerly creates a real DRM/GL
//! window whose teardown crashed Mesa (SEGV in `driUnbindContext`, see
//! `docs/kms_femtovg_migration_plan.md` §3.4). The marquee now scrolls live
//! `Text` elements natively in `ui/now_playing.slint`; the `FemtoVG` renderer caches glyph
//! shapes in its atlas, so per-frame cost is a GPU blit of the dirty rows.
//!
//! [`RgbaPremulPixel`] survives as the shared [`TargetPixel`] implementation for
//! offscreen software rendering (test harness captures).

use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};

/// Straight-alpha RGBA pixel implementing Slint's [`TargetPixel`] for offscreen rasterization.
///
/// Unlike the opaque scanout [`Bgrx8888Pixel`](super::framebuffer::Bgrx8888Pixel), this type
/// preserves per-pixel alpha so offscreen renders can be blended over themed backgrounds.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RgbaPremulPixel {
    /// Red color component (premultiplied by alpha).
    pub red: u8,
    /// Green color component (premultiplied by alpha).
    pub green: u8,
    /// Blue color component (premultiplied by alpha).
    pub blue: u8,
    /// Alpha coverage (0 = fully transparent, 255 = opaque).
    pub alpha: u8,
}

impl TargetPixel for RgbaPremulPixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let inv_alpha = u16::from(u8::MAX - color.alpha);
        self.red = ((u16::from(self.red) * inv_alpha) / 255) as u8 + color.red;
        self.green = ((u16::from(self.green) * inv_alpha) / 255) as u8 + color.green;
        self.blue = ((u16::from(self.blue) * inv_alpha) / 255) as u8 + color.blue;
        self.alpha = ((u16::from(self.alpha) * inv_alpha) / 255) as u8 + color.alpha;
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha: u8::MAX,
        }
    }

    fn background() -> Self {
        Self::default()
    }
}

const _: () = {
    assert!(size_of::<RgbaPremulPixel>() == 4);
    assert!(size_of::<slint::Rgba8Pixel>() == 4);
};
