//! Offscreen pre-rendering of marquee text strips into cached [`slint::Image`] buffers.
//!
//! Re-shaping and re-rasterizing long marquee strings on every animation frame costs
//! ~8 ms of CPU per 16.7 ms frame on the Raspberry Pi 3's software renderer. This module
//! rasterizes each marquee strip (title, artist/album composite) exactly once per text,
//! color, or typography change into a transparent-background [`slint::Image`], which
//! [`AppShell`](super::shell::AppShell) then hands to the declarative `MarqueeText`
//! scroller for cheap per-frame pixel blits.

use std::fmt::{self, Formatter};
use std::rc::Rc;

use slint::Color;
use slint::ComponentHandle;
use slint::SharedString;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, TargetPixel,
};

use super::generated::MarqueeRasterizer;

/// Marquee strip typography, in physical pixels at the appliance panel's 1.0 scale factor.
///
/// These constants are the single source of truth for marquee typography since the strips
/// replace declarative `Text` elements inside `MarqueeText`.
#[derive(Debug)]
pub struct MarqueeTypography;

impl MarqueeTypography {
    /// Track title strip font size in pixels.
    pub const TITLE_FONT_SIZE_PX: u32 = 30;
    /// Track title strip font weight (extra bold).
    pub const TITLE_FONT_WEIGHT: i32 = 800;
    /// Artist/album strip font size in pixels.
    pub const ARTIST_FONT_SIZE_PX: u32 = 22;
    /// Artist/album strip font weight (Bold).
    pub const ARTIST_FONT_WEIGHT: i32 = 700;
}

/// Straight-alpha RGBA pixel implementing Slint's [`TargetPixel`] for offscreen rasterization.
///
/// Unlike the opaque scanout [`Bgrx8888Pixel`](super::framebuffer::Bgrx8888Pixel), this type
/// preserves per-pixel alpha so the rasterized strip can be blended over themed backgrounds.
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

/// A rasterized marquee text strip: a transparent-background image plus its intrinsic size.
#[derive(Debug, Clone)]
pub struct MarqueeStrip {
    image: slint::Image,
    width: u32,
    height: u32,
}

impl MarqueeStrip {
    /// An empty strip (no visible content; the marquee scroller renders nothing).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            image: slint::Image::default(),
            width: 0,
            height: 0,
        }
    }

    /// The strip image to scroll.
    #[must_use]
    pub const fn image(&self) -> &slint::Image {
        &self.image
    }

    /// Strip width in physical pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Strip height in physical pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Whether the strip carries no visible content.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Identifies a rasterization request; strips are re-rendered only when the key changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StripKey {
    text: Box<str>,
    color: (u8, u8, u8),
    font_size_px: u32,
    font_weight: i32,
    font_family: Box<str>,
}

/// A single marquee strip rasterization request.
#[derive(Debug, Clone)]
pub struct StripRequest {
    /// Text to rasterize on a single line.
    pub text: Box<str>,
    /// Text color (straight RGB, no alpha).
    pub color: (u8, u8, u8),
    /// Font size in pixels.
    pub font_size_px: u32,
    /// Font weight (400 = regular, 800 = extra bold).
    pub font_weight: i32,
    /// Font family name (must be registered in Slint's font database).
    pub font_family: Box<str>,
}

impl StripRequest {
    fn key(&self) -> StripKey {
        StripKey {
            text: self.text.clone(),
            color: self.color,
            font_size_px: self.font_size_px,
            font_weight: self.font_weight,
            font_family: self.font_family.clone(),
        }
    }
}

/// Two-slot marquee strip cache (title and artist/album) sharing one offscreen rasterizer.
///
/// Strips are re-rendered only when their [`StripRequest`] key changes, so repeated
/// 250 ms playback-state ticks with unchanged metadata perform no rasterization work.
#[derive(Debug, Default)]
pub struct MarqueeStripCache {
    rasterizer: Option<MarqueeStripRasterizer>,
    title: Option<(StripKey, MarqueeStrip)>,
    artist: Option<(StripKey, MarqueeStrip)>,
}

impl MarqueeStripCache {
    /// Rasterizes (or returns the cached) title strip for `request`.
    #[must_use]
    pub fn rasterize_title(&mut self, request: &StripRequest) -> MarqueeStrip {
        Self::rasterize_slot(&mut self.rasterizer, &mut self.title, request, "title")
    }

    /// Rasterizes (or returns the cached) artist/album strip for `request`.
    #[must_use]
    pub fn rasterize_artist(&mut self, request: &StripRequest) -> MarqueeStrip {
        Self::rasterize_slot(&mut self.rasterizer, &mut self.artist, request, "artist")
    }

    /// How many actual rasterization passes were performed (cache misses).
    #[must_use]
    pub fn renders(&self) -> usize {
        self.rasterizer
            .as_ref()
            .map_or(0, MarqueeStripRasterizer::renders)
    }

    fn rasterize_slot(
        rasterizer: &mut Option<MarqueeStripRasterizer>,
        slot: &mut Option<(StripKey, MarqueeStrip)>,
        request: &StripRequest,
        label: &str,
    ) -> MarqueeStrip {
        if let Some((key, strip)) = slot {
            if *key == request.key() {
                return strip.clone();
            }
        }
        if rasterizer.is_none() {
            match MarqueeStripRasterizer::new() {
                Ok(created) => *rasterizer = Some(created),
                Err(err) => {
                    tracing::warn!("Marquee strip rasterizer unavailable ({label}): {err}");
                    return slot
                        .as_ref()
                        .map_or_else(MarqueeStrip::empty, |(_, strip)| strip.clone());
                }
            }
        }
        let strip = rasterizer
            .as_mut()
            .expect("rasterizer created above")
            .rasterize(request);
        *slot = Some((request.key(), strip.clone()));
        strip
    }
}

/// Offscreen single-strip rasterizer bound to its own throwaway Slint window.
///
/// Created lazily after the platform is registered; renders one strip per
/// [`rasterize`](Self::rasterize) call and caches only the most recent request.
pub struct MarqueeStripRasterizer {
    window: Rc<MinimalSoftwareWindow>,
    rasterizer: MarqueeRasterizer,
    key: Option<StripKey>,
    strip: MarqueeStrip,
    renders: usize,
}

impl fmt::Debug for MarqueeStripRasterizer {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarqueeStripRasterizer")
            .field("renders", &self.renders)
            .finish_non_exhaustive()
    }
}

impl MarqueeStripRasterizer {
    /// Creates the offscreen rasterizer on a fresh throwaway Slint window.
    ///
    /// Must be called after [`Framebuffer::init_platform`](super::framebuffer::Framebuffer::init_platform)
    /// has registered the software platform so the primary window claim is already taken.
    ///
    /// # Errors
    ///
    /// Returns [`FramebufferError`](super::framebuffer::FramebufferError) if the offscreen
    /// component cannot be instantiated or shown.
    pub fn new() -> Result<Self, super::framebuffer::FramebufferError> {
        let rasterizer = MarqueeRasterizer::new()
            .map_err(|err| super::framebuffer::FramebufferError::platform(err.to_string()))?;
        // `MarqueeRasterizer::new()` routes through the platform's `create_window_adapter`,
        // which remembers the freshly created secondary window it hands out.
        let window = super::framebuffer::take_last_secondary_window().ok_or_else(|| {
            super::framebuffer::FramebufferError::platform(
                "marquee rasterizer window was not created by the software platform".to_string(),
            )
        })?;
        rasterizer
            .show()
            .map_err(|err| super::framebuffer::FramebufferError::platform(err.to_string()))?;
        Ok(Self {
            window,
            rasterizer,
            key: None,
            strip: MarqueeStrip::empty(),
            renders: 0,
        })
    }

    /// Rasterizes `request` into a cached strip, re-rendering only when the request key
    /// differs from the previously rendered one.
    ///
    /// # Panics
    /// Panics if the Slint window adapter internals are in an invalid state (a Slint
    /// engine bug, never an expected runtime condition).
    #[must_use]
    pub fn rasterize(&mut self, request: &StripRequest) -> MarqueeStrip {
        if self.key.as_ref() == Some(&request.key()) {
            return self.strip.clone();
        }
        let (red, green, blue) = request.color;
        self.rasterizer
            .set_raster_text(SharedString::from(request.text.as_ref()));
        self.rasterizer
            .set_raster_color(Color::from_rgb_u8(red, green, blue));
        self.rasterizer
            .set_raster_font_family(SharedString::from(request.font_family.as_ref()));
        self.rasterizer.set_raster_font_weight(request.font_weight);

        #[expect(
            clippy::cast_precision_loss,
            reason = "font sizes are small u32 pixel values with exact f32 representation"
        )]
        let font_size_px = request.font_size_px as f32;
        self.rasterizer.set_raster_font_size_px(font_size_px);

        let width = ceil_length(self.rasterizer.get_text_width_px());
        let height = ceil_length(self.rasterizer.get_text_height_px());
        if width == 0 || height == 0 {
            self.key = Some(request.key());
            self.strip = MarqueeStrip::empty();
            return self.strip.clone();
        }

        #[expect(
            clippy::cast_precision_loss,
            reason = "raster dimensions are small u32 pixel values with exact f32 representation"
        )]
        let (width_f32, height_f32) = (width as f32, height as f32);
        self.window
            .set_size(slint::LogicalSize::new(width_f32, height_f32));
        self.window.request_redraw();

        let mut buffer = vec![RgbaPremulPixel::default(); width as usize * height as usize];
        self.window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, width as usize);
        });

        self.strip = MarqueeStrip {
            image: premul_to_image(&buffer, width, height),
            width,
            height,
        };
        self.key = Some(request.key());
        self.renders += 1;
        self.strip.clone()
    }

    /// How many actual rasterization passes were performed (cache misses).
    #[must_use]
    pub const fn renders(&self) -> usize {
        self.renders
    }
}

/// Rounds a text metric in pixels up to at least one physical pixel (zero stays zero).
#[must_use]
fn ceil_length(px: f32) -> u32 {
    if px <= 0.0 {
        0
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "value is verified positive and ceiled before truncation"
        )]
        let ceiled = px.ceil() as u32;
        ceiled
    }
}

/// Converts premultiplied render pixels into an RGBA [`slint::Image`].
fn premul_to_image(buffer: &[RgbaPremulPixel], width: u32, height: u32) -> slint::Image {
    let mut shared = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    for (dst, src) in shared.make_mut_slice().iter_mut().zip(buffer.iter()) {
        *dst = slint::Rgba8Pixel {
            r: src.red,
            g: src.green,
            b: src.blue,
            a: src.alpha,
        };
    }
    slint::Image::from_rgba8_premultiplied(shared)
}

const _: () = {
    assert!(size_of::<RgbaPremulPixel>() == 4);
    assert!(size_of::<slint::Rgba8Pixel>() == 4);
};
