#![cfg(feature = "ui")]
//! Integration tests for the offscreen marquee strip rasterizer and its cache.

use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};
use speaker_core::ui::{
    Framebuffer, MainWindow, MarqueeStrip, MarqueeStripRasterizer, MarqueeTypography,
    RgbaPremulPixel, StripRequest,
};

/// Registers the software platform and claims the primary window like the daemon does,
/// so subsequently created components receive their own offscreen windows.
fn init_platform_and_claim_primary_window() -> MainWindow {
    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");
    MainWindow::new().expect("Failed to initialize MainWindow")
}

fn request(text: &str) -> StripRequest {
    StripRequest {
        text: text.into(),
        color: (0xF0, 0xF6, 0xFC),
        font_size_px: MarqueeTypography::ARTIST_FONT_SIZE_PX,
        font_weight: MarqueeTypography::ARTIST_FONT_WEIGHT,
        font_family: "Inter".into(),
    }
}

#[test]
fn marquee_rasterizer_renders_non_empty_strip() {
    init_platform_and_claim_primary_window();

    let mut rasterizer = MarqueeStripRasterizer::new().expect("rasterizer creation");
    let strip = rasterizer.rasterize(&request("Bohemian Rhapsody"));
    assert!(!strip.is_empty());
    assert!(strip.width() > 0);
    assert!(strip.height() > 0);
    assert_eq!(rasterizer.renders(), 1);

    let image_size = strip.image().size();
    assert_eq!(image_size.width, strip.width());
    assert_eq!(image_size.height, strip.height());
}

#[test]
fn marquee_rasterizer_dedupes_identical_requests() {
    init_platform_and_claim_primary_window();

    let mut rasterizer = MarqueeStripRasterizer::new().expect("rasterizer creation");
    let first = rasterizer.rasterize(&request("Same Text"));
    let second = rasterizer.rasterize(&request("Same Text"));
    assert_eq!(rasterizer.renders(), 1);
    assert_eq!(first.width(), second.width());

    let _ = rasterizer.rasterize(&request("Changed Text"));
    assert_eq!(rasterizer.renders(), 2);
}

#[test]
fn marquee_rasterizer_empty_text_yields_empty_strip() {
    init_platform_and_claim_primary_window();

    let mut rasterizer = MarqueeStripRasterizer::new().expect("rasterizer creation");
    let strip = rasterizer.rasterize(&request(""));
    assert!(strip.is_empty());
    assert_eq!(strip.width(), 0);
    assert_eq!(strip.height(), 0);
}

#[test]
fn marquee_strip_cache_dedupes_per_slot() {
    init_platform_and_claim_primary_window();

    let mut cache = speaker_core::ui::MarqueeStripCache::default();
    let title = cache.rasterize_title(&request("Title One"));
    let artist = cache.rasterize_artist(&request("Artist One"));
    assert!(!title.is_empty());
    assert!(!artist.is_empty());
    assert_eq!(cache.renders(), 2, "two distinct slots rasterize once each");

    // Repeated identical requests in alternating slots must not re-render.
    let _ = cache.rasterize_artist(&request("Artist One"));
    let _ = cache.rasterize_title(&request("Title One"));
    assert_eq!(cache.renders(), 2);

    // Changed text re-renders only the changed slot.
    let _ = cache.rasterize_title(&request("Title Two"));
    assert_eq!(cache.renders(), 3);
}

#[test]
fn marquee_strip_empty_request_is_cached_without_rasterization() {
    init_platform_and_claim_primary_window();

    let mut cache = speaker_core::ui::MarqueeStripCache::default();
    let empty = cache.rasterize_title(&request(""));
    assert!(empty.is_empty());
    let again = cache.rasterize_title(&request(""));
    assert!(again.is_empty());
    assert_eq!(
        cache.renders(),
        0,
        "empty requests skip rasterization entirely"
    );
}

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

#[test]
fn marquee_rasterizer_debug_renders_summary() {
    init_platform_and_claim_primary_window();

    let rasterizer = MarqueeStripRasterizer::new().expect("rasterizer creation");
    let debug = format!("{rasterizer:?}");
    assert!(debug.contains("MarqueeStripRasterizer"));
    assert!(debug.contains("renders"));
}

#[test]
fn marquee_typography_constants_match_appliance_design() {
    assert_eq!(MarqueeTypography::TITLE_FONT_SIZE_PX, 30);
    assert_eq!(MarqueeTypography::TITLE_FONT_WEIGHT, 800);
    assert_eq!(MarqueeTypography::ARTIST_FONT_SIZE_PX, 22);
    assert_eq!(MarqueeTypography::ARTIST_FONT_WEIGHT, 700);
}

#[test]
fn marquee_strip_empty_default_has_no_image_content() {
    let empty = MarqueeStrip::empty();
    assert!(empty.is_empty());
    assert_eq!(empty.image().size().width, 0);
}
