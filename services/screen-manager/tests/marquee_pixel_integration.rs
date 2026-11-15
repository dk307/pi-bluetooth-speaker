#![cfg(feature = "ui")]
//! Integration tests for the shared premultiplied RGBA offscreen pixel type.

use screen_manager::ui::RgbaPremulPixel;
use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};

#[test]
fn rgba_premul_pixel_blending_composites_premultiplied_over() {
    // Blending an opaque color replaces the destination entirely.
    let mut opaque = RgbaPremulPixel::default();
    opaque.blend(PremultipliedRgbaColor {
        red: 0x30,
        green: 0x20,
        blue: 0x10,
        alpha: 0xFF,
    });
    assert_eq!(opaque, RgbaPremulPixel::from_rgb(0x30, 0x20, 0x10));

    // Blending a half-transparent premultiplied color halves the destination contribution.
    let mut half = RgbaPremulPixel::from_rgb(40, 80, 120);
    half.blend(PremultipliedRgbaColor {
        red: 0x10,
        green: 0x32,
        blue: 0x64,
        alpha: 0x80,
    });
    assert_eq!(
        half.red,
        u8::try_from((u16::from(40u8) * 127) / 255 + 0x10).unwrap()
    );
    assert_eq!(
        half.green,
        u8::try_from((u16::from(80u8) * 127) / 255 + 0x32).unwrap()
    );
    assert_eq!(
        half.blue,
        u8::try_from((u16::from(120u8) * 127) / 255 + 0x64).unwrap()
    );
    assert_eq!(half.alpha, 255);
}

#[test]
fn rgba_premul_pixel_background_is_fully_transparent() {
    let background = RgbaPremulPixel::background();
    assert_eq!(background, RgbaPremulPixel::default());
    assert_eq!(background.alpha, 0);
}
