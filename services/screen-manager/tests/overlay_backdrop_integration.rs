#![cfg(feature = "ui")]
//! Integration tests for the overlay backdrop blur and the overlay focus hand-off.
//!
//! Slint has no backdrop-blur primitive, so the screen that an overlay covers is
//! snapshotted from the framebuffer shadow buffer, blurred on the CPU and
//! published back to the UI as an image that the overlay renders behind itself.

use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

use screen_manager::daemon::sync_overlay_backdrop;
use screen_manager::ui::{
    AppShell, BACKDROP_BLUR_RADIUS, BackdropAction, Bgrx8888Pixel, Framebuffer, MainWindow,
    OVERLAY_TOP_INSET, OverlayBackdropTracker, bgrx_to_rgba, blur_premultiplied_rgba, blur_region,
};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::{ComponentHandle, SharedPixelBuffer};

/// Width of the appliance panel.
const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
/// Height of the appliance panel.
const HEIGHT: usize = Framebuffer::DEFAULT_HEIGHT as usize;

fn shell_with_window() -> (AppShell, Rc<MinimalSoftwareWindow>, Framebuffer) {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let shell = AppShell::new(MainWindow::new().expect("MainWindow"));
    shell.window().show().expect("show");
    (shell, window, fb)
}

/// Renders the whole window and returns its pixels.
fn render(fb: &mut Framebuffer, window: &MinimalSoftwareWindow) -> Vec<Bgrx8888Pixel> {
    slint::platform::update_timers_and_animations();
    fb.render_frame_full_redraw(window).expect("render frame");
    fb.pixels().to_vec()
}

/// Advances the Slint timers and animations so an overlay fade-in completes.
fn settle() {
    for _ in 0..40 {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Builds a solid RGBA image of the given size.
fn solid_image(width: u32, height: u32, rgba: [u8; 4]) -> slint::Image {
    let mut buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    let (pixels, _) = buffer.make_mut_bytes().as_chunks_mut::<4>();
    for chunk in pixels {
        chunk.copy_from_slice(&rgba);
    }
    slint::Image::from_rgba8_premultiplied(buffer)
}

#[test]
fn bgrx_to_rgba_reorders_the_channels_and_opaques_the_pixels() {
    let pixels = vec![Bgrx8888Pixel::new(10, 20, 30), Bgrx8888Pixel::new(1, 2, 3)];

    assert_eq!(
        bgrx_to_rgba(&pixels),
        vec![10, 20, 30, 255, 1, 2, 3, 255],
        "BGRX pixels must become opaque RGBA bytes in red/green/blue order"
    );
}

#[test]
fn blur_region_smooths_a_hard_edge() {
    // 16x16 frame: the left half is black, the right half is white.
    let mut frame = Vec::new();
    for _ in 0..16 {
        for x in 0..16 {
            let value = if x < 8 { 0 } else { 255 };
            frame.push(Bgrx8888Pixel::new(value, value, value));
        }
    }

    let blurred = blur_region(&frame, 16, 16, 0, 3).expect("blur region");

    assert_eq!(blurred.width, 16);
    assert_eq!(blurred.height, 16);
    assert_eq!(blurred.pixels.len(), 16 * 16 * 4);

    let middle = 8 * 16 + 8;
    let red = blurred.pixels[middle * 4];
    assert!(
        red > 0 && red < 255,
        "the blurred edge must be a gradient, got {red}"
    );
}

#[test]
fn blur_region_keeps_the_region_below_the_top_bar() {
    // 4x4 frame where every pixel is a distinct gray value.
    let mut frame = Vec::new();
    for i in 0..16u8 {
        frame.push(Bgrx8888Pixel::new(i, i, i));
    }

    let blurred = blur_region(&frame, 4, 4, 2, 1).expect("blur region");

    assert_eq!(blurred.width, 4);
    assert_eq!(
        blurred.height, 2,
        "only the rows below the top inset belong to the backdrop"
    );
    assert_eq!(blurred.pixels.len(), 4 * 2 * 4);
}

#[test]
fn blur_region_rejects_a_frame_that_does_not_match_the_geometry() {
    let frame = vec![Bgrx8888Pixel::new(0, 0, 0)];

    let err = blur_region(&frame, 8, 8, 0, BACKDROP_BLUR_RADIUS).expect_err("geometry mismatch");

    assert!(
        err.is_geometry(),
        "a size mismatch must be a geometry error"
    );
    assert!(
        !err.summary().is_empty(),
        "the canonical error must expose a summary"
    );
}

#[test]
fn blur_region_rejects_a_top_inset_that_covers_the_whole_frame() {
    let frame = vec![Bgrx8888Pixel::new(0, 0, 0); 16];

    let err = blur_region(&frame, 4, 4, 4, BACKDROP_BLUR_RADIUS).expect_err("inset too large");

    assert!(err.is_geometry());
}

#[test]
fn blur_premultiplied_rgba_rejects_a_buffer_that_is_too_short() {
    let mut rgba = vec![0u8; 8];

    let err = blur_premultiplied_rgba(&mut rgba, 4, 4, BACKDROP_BLUR_RADIUS)
        .expect_err("a buffer shorter than the announced geometry must be rejected");

    assert!(
        err.is_blur(),
        "a rejected buffer must be reported as a blur error, got {:?}",
        err.summary()
    );
    assert!(
        !err.is_geometry(),
        "a rejected buffer is not a capture geometry mismatch"
    );
    assert!(
        err.source().is_some(),
        "the blur error must keep the backend failure as its cause"
    );
    assert!(
        err.summary().contains("blur failed"),
        "the canonical summary must describe the blur failure, got {}",
        err.summary()
    );
}

#[test]
fn blur_premultiplied_rgba_rejects_an_empty_image() {
    let mut rgba = Vec::new();

    let err =
        blur_premultiplied_rgba(&mut rgba, 0, 0, BACKDROP_BLUR_RADIUS).expect_err("empty image");

    assert!(err.is_blur());
}

#[test]
fn the_geometry_error_carries_no_underlying_cause() {
    let frame = vec![Bgrx8888Pixel::new(0, 0, 0)];

    let err = blur_region(&frame, 8, 8, 0, BACKDROP_BLUR_RADIUS).expect_err("geometry mismatch");

    assert!(err.is_geometry());
    assert!(
        err.source().is_none(),
        "a capture geometry mismatch is self-contained"
    );
    assert!(
        err.summary().contains("geometry mismatch"),
        "the canonical summary must describe the geometry mismatch, got {}",
        err.summary()
    );
    assert!(
        err.to_string().contains("geometry mismatch"),
        "the display form must describe the geometry mismatch too"
    );
}

#[test]
fn backdrop_tracker_captures_once_per_overlay_period() {
    let mut tracker = OverlayBackdropTracker::new();

    assert_eq!(
        tracker.on_overlay_state_changed(false),
        BackdropAction::Keep,
        "no overlay on screen means nothing to capture"
    );
    assert_eq!(
        tracker.on_overlay_state_changed(true),
        BackdropAction::Capture,
        "the first overlay on screen must capture the scene it covers"
    );
    assert_eq!(
        tracker.on_overlay_state_changed(true),
        BackdropAction::Keep,
        "an open overlay must not be captured again"
    );
    assert_eq!(
        tracker.on_overlay_state_changed(false),
        BackdropAction::Clear,
        "the last overlay closing releases the backdrop"
    );
    assert_eq!(
        tracker.on_overlay_state_changed(false),
        BackdropAction::Keep,
        "a closed overlay must not clear the backdrop twice"
    );
    assert_eq!(
        tracker.on_overlay_state_changed(true),
        BackdropAction::Capture,
        "a re-opened overlay captures a fresh scene"
    );
    assert!(tracker.is_active());
}

#[test]
fn the_slint_overlay_top_inset_is_seeded_from_the_rust_constant() {
    let (shell, _window, _fb) = shell_with_window();

    // Whole-pixel logical insets round-trip exactly through the `f32` getter.
    #[expect(
        clippy::cast_sign_loss,
        reason = "the seeded inset is a non-negative whole pixel count"
    )]
    let slint_inset_px = shell.window().get_overlay_top_inset() as u32;

    assert_eq!(
        slint_inset_px, OVERLAY_TOP_INSET,
        "AppShell::new must seed the Slint inset from OVERLAY_TOP_INSET so the \
         blur crop and the overlay geometry cannot drift apart"
    );
}

#[test]
fn the_overlay_backdrop_is_published_when_an_overlay_opens() {
    let (shell, window, mut fb) = shell_with_window();
    let mut tracker = OverlayBackdropTracker::new();

    // Render the clean scene first: the shadow buffer has to hold the screen
    // that the overlay is about to cover.
    let _ = render(&mut fb, &window);

    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        0,
        "no backdrop may be published while no overlay is on screen"
    );

    shell.show_volume_hud();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);

    let size = shell.window().get_overlay_backdrop().size();
    assert_eq!(
        size.width,
        u32::try_from(WIDTH).expect("width fits in u32"),
        "the backdrop must span the whole panel width"
    );
    assert_eq!(
        size.height,
        u32::try_from(HEIGHT - OVERLAY_TOP_INSET as usize).expect("height fits in u32"),
        "the backdrop must only cover the area below the top bar"
    );

    // A second overlay opening while the first one is still up reuses the
    // already published backdrop instead of capturing the overlay itself.
    shell.show_codec_hud();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        u32::try_from(WIDTH).expect("width fits in u32"),
        "the published backdrop must survive an overlay hand-over"
    );

    shell.dismiss_codec_hud();
    settle();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        0,
        "the backdrop must be released once every overlay closed"
    );
}

#[test]
fn reopening_an_overlay_mid_fade_keeps_the_published_backdrop() {
    let (shell, window, mut fb) = shell_with_window();
    let mut tracker = OverlayBackdropTracker::new();

    let _ = render(&mut fb, &window);
    shell.show_volume_hud();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        u32::try_from(WIDTH).expect("width fits in u32"),
        "the overlay open must publish a backdrop"
    );

    // Evaluate the fade-in so the panel is actually on screen before it is
    // dismissed again (without this tick the opacity is still at 0.0).
    let _ = render(&mut fb, &window);

    // Dismiss and advance only part of the 200 ms fade: the panel is still
    // rendered (opacity > 0.01), so the overlay must stay active.
    shell.dismiss_volume_hud();
    for _ in 0..4 {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        shell.is_volume_hud_active(),
        "the fading panel must keep the overlay active during the fade"
    );

    // Reopening inside the fade window must neither release the frozen
    // backdrop nor re-capture a buffer that still contains the fading panel.
    shell.show_volume_hud();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        u32::try_from(WIDTH).expect("width fits in u32"),
        "the frozen backdrop must survive a reopen during the fade"
    );

    // Only after the fade completes with the overlay closed is the backdrop
    // released; the next open then captures a clean scene.
    shell.dismiss_volume_hud();
    settle();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        0,
        "the backdrop must be released once the fade completed"
    );
}

#[test]
fn the_volume_hud_takes_the_focus_when_it_opens() {
    let (shell, _window, _fb) = shell_with_window();

    assert!(
        !shell.volume_hud_has_focus(),
        "no overlay may own the focus before it opens"
    );

    shell.show_volume_hud();
    assert!(
        shell.volume_hud_has_focus(),
        "the Volume HUD must take the focus when it opens"
    );

    shell.dismiss_volume_hud();
    assert!(
        !shell.volume_hud_has_focus(),
        "the Volume HUD must release the focus when it closes"
    );
}

#[test]
fn the_codec_hud_takes_the_focus_when_it_opens() {
    let (shell, _window, _fb) = shell_with_window();

    assert!(!shell.codec_hud_has_focus());

    shell.show_codec_hud();
    assert!(
        shell.codec_hud_has_focus(),
        "the codec overlay must take the focus when it opens"
    );

    shell.dismiss_codec_hud();
    assert!(
        !shell.codec_hud_has_focus(),
        "the codec overlay must release the focus when it closes"
    );
}

#[test]
fn opening_the_other_overlay_hands_the_focus_over() {
    let (shell, _window, _fb) = shell_with_window();

    shell.show_volume_hud();
    assert!(shell.volume_hud_has_focus());

    shell.show_codec_hud();
    slint::platform::update_timers_and_animations();

    assert!(
        shell.codec_hud_has_focus(),
        "the newly opened overlay must own the focus"
    );
    assert!(
        !shell.volume_hud_has_focus(),
        "the hidden overlay must not keep the focus"
    );
}

#[test]
fn the_blurred_backdrop_is_rendered_behind_the_overlay() {
    let (shell, window, mut fb) = shell_with_window();

    // A saturated magenta backdrop cannot be produced by the panel palette, so
    // a magenta-dominant pixel proves that the backdrop reached the framebuffer.
    let backdrop = solid_image(
        u32::try_from(WIDTH).expect("width fits in u32"),
        u32::try_from(HEIGHT - OVERLAY_TOP_INSET as usize).expect("height fits in u32"),
        [255, 0, 255, 255],
    );

    shell.show_codec_hud();
    settle();
    let without_backdrop = render(&mut fb, &window);

    shell.set_overlay_backdrop(backdrop);
    settle();
    let with_backdrop = render(&mut fb, &window);

    // Sample a pixel inside the overlay panel but outside its centered column.
    let index = 300 * WIDTH + 20;
    let before = without_backdrop[index];
    let after = with_backdrop[index];

    assert_ne!(
        before, after,
        "the published backdrop must change the rendered frame"
    );
    assert!(
        after.red > after.green + 20 && after.blue > after.green + 20,
        "the backdrop must show through the translucent overlay panel, got \
         red={} green={} blue={}",
        after.red,
        after.green,
        after.blue
    );

    // The top bar stays untouched: the backdrop only covers the viewport area.
    let bar = 30 * WIDTH + 20;
    assert_eq!(
        with_backdrop[bar], without_backdrop[bar],
        "the blurred backdrop must not cover the top status bar"
    );
}
