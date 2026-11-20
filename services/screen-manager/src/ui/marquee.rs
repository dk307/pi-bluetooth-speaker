//! Premultiplied `SharedPixelBuffer` rasterizer for overflowing marquee titles.
//!
//! Renders the full title string into a single `Rgba8Pixel` buffer only when
//! the title text, font scale, or theme color changes, so 60 Hz scrolling in
//! Slint only translates `Image.x` without per-frame text shaping or glyph
//! rasterization.

use swash::FontRef;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::Format;

/// Embedded `ExtraBold` font matching `MarqueeText` (`font-weight: 800`).
const INTER_EXTRABOLD_TTF: &[u8] = include_bytes!("../../../../assets/fonts/Inter-ExtraBold.ttf");

/// `VideoCore` IV `GL_MAX_TEXTURE_SIZE` ceiling (Issue #11785): textures wider
/// than 2048 px fail `glTexImage2D` with `GL_INVALID_VALUE` and render black.
pub const MAX_MARQUEE_STRIP_WIDTH: u32 = 2048;

/// Base marquee font size in logical pixels (matches `now_playing.slint`).
const BASE_FONT_SIZE_PX: f32 = 32.0;

/// Extra vertical padding (px) above ascent and below descent so hinted
/// ascenders/descenders (e.g. `g`, `y`, `q`, `É`) are never clipped.
const VERTICAL_SLACK_PX: f32 = 2.0;

/// Rasterizes `text` at `BASE_FONT_SIZE_PX * font_scale` with premultiplied
/// alpha for `color` (`(r, g, b)`), returning an empty image for empty `text`.
///
/// # Panics
/// Panics if the embedded `Inter-ExtraBold.ttf` asset fails to parse (`M-PANIC-ON-BUG`).
#[must_use]
pub fn rasterize_marquee_strip(text: &str, font_scale: f32, color: (u8, u8, u8)) -> slint::Image {
    if text.is_empty() {
        return slint::Image::default();
    }

    let font = FontRef::from_index(INTER_EXTRABOLD_TTF, 0)
        .expect("embedded Inter-ExtraBold.ttf must parse");
    let size_px = (BASE_FONT_SIZE_PX * font_scale.clamp(1.0, 1.35)).round();
    let metrics = font.metrics(&[]).scale(size_px);
    let ascent = metrics.ascent.ceil() + VERTICAL_SLACK_PX / 2.0;
    let descent = metrics.descent.abs().ceil() + VERTICAL_SLACK_PX / 2.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "ascent + descent is positive and bounded (~60 px at 1.35x scale)"
    )]
    let height = ((ascent + descent).ceil().max(1.0) as u32).min(MAX_MARQUEE_STRIP_WIDTH);

    let charmap = font.charmap();
    let glyph_metrics = font.glyph_metrics(&[]).scale(size_px);

    // First pass: collect glyph IDs, advances, and total width (clamped to 2048 px).
    let mut glyphs = Vec::with_capacity(text.chars().count());
    let mut cursor_x: f32 = 0.0;
    for ch in text.chars() {
        let gid = charmap.map(ch);
        let advance = glyph_metrics.advance_width(gid);
        if cursor_x + advance > MAX_MARQUEE_STRIP_WIDTH as f32 {
            break;
        }
        glyphs.push((gid, cursor_x));
        cursor_x += advance;
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "cursor_x is non-negative and <= MAX_MARQUEE_STRIP_WIDTH"
    )]
    let width = (cursor_x.ceil() as u32).clamp(1, MAX_MARQUEE_STRIP_WIDTH);

    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    let pixels = buf.make_mut_slice();

    let mut ctx = ScaleContext::new();
    let mut scaler = ctx.builder(font).size(size_px).hint(true).build();
    let mut render = Render::new(&[Source::Outline]);

    for (gid, gx) in glyphs {
        let Some(img) = render.format(Format::Alpha).render(&mut scaler, gid) else {
            continue;
        };
        if img.content != Content::Mask || img.placement.width == 0 || img.placement.height == 0 {
            continue;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "glyph origin is bounded within the 2048x64 strip"
        )]
        let origin_x = gx.round() as i32 + img.placement.left;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "baseline + placement.top is bounded within the strip height"
        )]
        let origin_y = ascent as i32 - img.placement.top;

        blit_glyph_mask(pixels, width, height, origin_x, origin_y, &img, color);
    }

    slint::Image::from_rgba8_premultiplied(buf)
}

/// Composites a single alpha-mask glyph into a premultiplied `Rgba8Pixel` buffer.
fn blit_glyph_mask(
    pixels: &mut [slint::Rgba8Pixel],
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
    img: &swash::scale::image::Image,
    color: (u8, u8, u8),
) {
    let (cr, cg, cb) = (u16::from(color.0), u16::from(color.1), u16::from(color.2));
    let gw = img.placement.width as usize;
    let gh = img.placement.height as usize;
    for row in 0..gh {
        let Ok(row_i32) = i32::try_from(row) else {
            break;
        };
        let Ok(py) = usize::try_from(origin_y + row_i32) else {
            continue;
        };
        if py >= height as usize {
            continue;
        }
        let dst_row = py * width as usize;
        let src_row = row * gw;
        for col in 0..gw {
            let Ok(col_i32) = i32::try_from(col) else {
                break;
            };
            let Ok(px) = usize::try_from(origin_x + col_i32) else {
                continue;
            };
            if px >= width as usize {
                continue;
            }
            let cov = img.data[src_row + col];
            if cov == 0 {
                continue;
            }
            let dst = &mut pixels[dst_row + px];
            // Saturating premultiplied alpha composite (handles overlapping kerning).
            let a = u16::from(cov);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "(u8 * u8 + 127) / 255 fits in u8"
            )]
            let (pr, pg, pb) = (
                ((cr * a + 127) / 255) as u8,
                ((cg * a + 127) / 255) as u8,
                ((cb * a + 127) / 255) as u8,
            );
            dst.r = dst.r.saturating_add(pr);
            dst.g = dst.g.saturating_add(pg);
            dst.b = dst.b.saturating_add(pb);
            dst.a = dst.a.saturating_add(cov);
        }
    }
}
