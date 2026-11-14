#![cfg(feature = "ui")]
//! Coverage tests for marquee strip cache error/edge paths: rasterizer
//! unavailability, zero-width text metrics, and the raw pixel accessor.

use screen_manager::ui::{
    Framebuffer, MainWindow, MarqueeStrip, MarqueeStripCache, MarqueeStripRasterizer, StripRequest,
};

fn request_with(text: &str, font_size_px: u32) -> StripRequest {
    StripRequest {
        text: text.into(),
        color: (0xF0, 0xF6, 0xFC),
        font_size_px,
        font_weight: 800,
        font_family: "Inter".into(),
    }
}

#[test]
fn strip_pixels_accessor_exposes_raw_buffer() {
    let empty = MarqueeStrip::empty();
    assert_eq!(empty.pixels(), []);
    assert_eq!(empty.pixels_arc().as_ref(), []);
}

#[test]
fn rasterizer_creation_without_claimed_secondary_window_fails_gracefully() {
    // Register the platform but deliberately do NOT claim the primary window:
    // the offscreen rasterizer then cannot obtain a secondary window and must
    // degrade to `None` (cache miss) instead of panicking.
    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("platform init");

    let mut cache = MarqueeStripCache::default();
    let miss = cache.rasterize_title(&request_with("Title", 32));
    assert!(
        miss.is_none(),
        "rasterizer creation failure must surface as a cache miss"
    );
    assert_eq!(cache.renders(), 0);
}

#[test]
fn rasterizer_new_fails_without_a_secondary_window() {
    // Fresh platform state with no prior component: the rasterizer's offscreen
    // window was never created by the software platform, so construction must
    // surface a typed platform error.
    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("platform init");

    let err = MarqueeStripRasterizer::new().expect_err("platform error");
    assert!(
        err.to_string()
            .contains("rasterizer window was not created"),
        "{err}"
    );
}

#[test]
fn rasterize_zero_width_text_yields_empty_strip() {
    // Claim the primary window first so the rasterizer receives a secondary one.
    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("platform init");
    let _main = MainWindow::new().expect("MainWindow");

    let mut rasterizer = MarqueeStripRasterizer::new().expect("rasterizer creation");

    // Any request whose measured text metrics collapse to zero width must be
    // treated as "no strip" rather than rasterizing a zero-sized surface.
    let zero_width_requests = [
        request_with("\u{200B}", 32), // zero-width space
        request_with("\u{2060}", 32), // word joiner
        request_with("\u{0301}", 32), // combining acute accent alone
        request_with("x", 0),         // zero font size
    ];
    let mut covered = false;
    for request in zero_width_requests {
        let strip = rasterizer.rasterize(&request);
        if strip.is_empty() {
            covered = true;
            break;
        }
    }
    assert!(
        covered,
        "no candidate produced zero-width text metrics; empty-strip branch uncovered"
    );
}
