#![cfg(feature = "ui")]
//! Integration tests for the overlay backdrop blur and the overlay focus hand-off.
//!
//! Slint has no backdrop-blur primitive, so the screen that an overlay covers is
//! snapshotted from the framebuffer shadow buffer, blurred on the CPU and
//! published back to the UI as an image that the overlay renders behind itself.

use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

use screen_manager::config::{MAX_VOLUME_HUD_FADE_MS, VolumeHudFadeMs};
use screen_manager::daemon::sync_overlay_backdrop;
use screen_manager::ui::{
    AppShell, BACKDROP_BLUR_RADIUS, BackdropAction, Bgrx8888Pixel, Framebuffer, MainWindow,
    OVERLAY_TOP_INSET, OverlayBackdropTracker, bgrx_to_rgba, blur_premultiplied_rgba, blur_region,
    blur_rgba_region,
};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedPixelBuffer};

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
    assert!(
        shell.window().get_overlay_backdrop_available(),
        "a published backdrop must mark the scrim as weakened"
    );

    shell.dismiss_codec_hud();
    settle();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        0,
        "the backdrop must be released once every overlay closed"
    );
    assert!(
        !shell.window().get_overlay_backdrop_available(),
        "releasing the backdrop must restore full scrim dimming"
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

    // Stretch the fade to the maximum allowed duration (2 s) so the "mid-fade"
    // state is deterministic across the synchronous reopen assertions below.
    shell.set_volume_hud_fade_ms(VolumeHudFadeMs::new(MAX_VOLUME_HUD_FADE_MS));

    // Dismiss: the panel is still rendered (opacity > 0.01), so the overlay
    // must stay active.
    shell.dismiss_volume_hud();
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

    // Collapse the fade to zero and let the overlay close for real: only
    // after the fade completes with the overlay closed is the backdrop
    // released; the next open then captures a clean scene.
    shell.dismiss_volume_hud();
    shell.set_volume_hud_fade_ms(VolumeHudFadeMs::new(0));
    settle();
    sync_overlay_backdrop(&fb, &shell, &mut tracker);
    assert_eq!(
        shell.window().get_overlay_backdrop().size().width,
        0,
        "the backdrop must be released once the fade completed"
    );
}

#[test]
fn tapping_the_backdrop_dismisses_the_hud_through_the_focus_scope() {
    let (shell, window, _fb) = shell_with_window();

    shell.show_volume_hud();
    assert!(shell.is_volume_hud_visible(), "the Volume HUD must be open");

    // Tap the central readout area of the open HUD: the only touch handlers
    // there are the full-viewport FocusScope and, below it, the backdrop
    // TouchArea. With `focus-on-click: false` the scope must ignore the tap
    // so it falls through and triggers the early dismissal.
    let position = LogicalPosition::new(400.0, 220.0);
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerExited);

    assert!(
        !shell.is_volume_hud_visible(),
        "tapping the overlay backdrop must dismiss the HUD; a tap-swallowing \
         FocusScope would break the early-dismiss interaction"
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

#[test]
fn the_scrim_dims_the_published_backdrop() {
    let (shell, window, mut fb) = shell_with_window();

    shell.show_codec_hud();
    settle();

    // The scrim must stack ABOVE the published backdrop so its 0.25 dimming
    // renders on top of the blur. Measure the per-channel brightness lift
    // between a white and a black backdrop with everything else unchanged:
    // the translucent panel (alpha 0.75) blends over a layer that the scrim
    // has already dimmed to 0.75 of the backdrop, so the lift is
    // 0.25 * 0.75 * 255 ≈ 48. If the backdrop were stacked above the scrim
    // (the regression this guards), the opaque image would hide the scrim and
    // the lift would be the full 0.25 * 255 ≈ 64 instead.
    let w = u32::try_from(WIDTH).expect("width fits in u32");
    let h = u32::try_from(HEIGHT - OVERLAY_TOP_INSET as usize).expect("height fits in u32");
    let white = solid_image(w, h, [255, 255, 255, 255]);
    let black = solid_image(w, h, [0, 0, 0, 255]);

    shell.set_overlay_backdrop(white);
    settle();
    let with_white = render(&mut fb, &window);

    shell.set_overlay_backdrop(black);
    settle();
    let with_black = render(&mut fb, &window);

    // Sample a pixel inside the overlay panel but outside its centered column.
    let index = 300 * WIDTH + 20;
    let white_px = with_white[index];
    let black_px = with_black[index];
    assert_ne!(
        white_px, black_px,
        "the published backdrop must reach the framebuffer"
    );

    let lift = |a: u8, b: u8| i32::from(a) - i32::from(b);
    for (channel, value) in [
        ("red", lift(white_px.red, black_px.red)),
        ("green", lift(white_px.green, black_px.green)),
        ("blue", lift(white_px.blue, black_px.blue)),
    ] {
        assert!(
            (42..=54).contains(&value),
            "the 0.25 scrim must dim the backdrop above it: {channel} lift {value}, \
             expected ~48 (a lift of ~64 means the backdrop hides the scrim)",
        );
    }
}

#[test]
fn the_panel_is_opaque_when_no_backdrop_is_published() {
    let (shell, window, mut fb) = shell_with_window();

    // KMS fallback: no backdrop is ever published, so the panels must render
    // fully opaque. Switch between two screen tabs (same `top_bar_height`,
    // different top-bar title and viewport body) underneath each open HUD and
    // compare the whole overlay region: with an opaque panel nothing of the
    // live scene bleeds through, so every pixel at or below the top bar must
    // be identical between the two scenes. A translucent panel would let the
    // dimmed live content ghost through (the regression this guards).
    for open in [AppShell::show_volume_hud, AppShell::show_codec_hud] {
        shell.window().set_active_tab(1);
        open(&shell);
        // Simulate a failed/unavailable snapshot so the panel must fall back to
        // its opaque gradient rather than letting live content ghost through.
        shell.set_overlay_backdrop(slint::Image::default());
        settle();
        let top_bar_px = shell.window().get_top_bar_height().ceil();
        assert!(
            top_bar_px.is_finite() && top_bar_px >= 0.0,
            "top_bar_height must be non-negative: {top_bar_px}"
        );
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "top_bar_px is finite, >= 0.0, and bounded by the 480px window height"
        )]
        let overlay_top = top_bar_px as usize;
        let tab_vu = render(&mut fb, &window);

        shell.window().set_active_tab(5);
        settle();
        let tab_settings = render(&mut fb, &window);

        // Positive control: the tab switch really changed the scene above the
        // overlay area ("VU Meter" vs "Settings"), so the comparison below is
        // meaningful.
        assert_ne!(
            tab_vu[..overlay_top * WIDTH],
            tab_settings[..overlay_top * WIDTH],
            "the tab switch must change the top bar for this test to be meaningful"
        );

        let ghost = tab_vu
            .iter()
            .zip(tab_settings.iter())
            .enumerate()
            .skip(overlay_top * WIDTH)
            .find(|(_, (a, b))| a != b);
        assert!(
            ghost.is_none(),
            "live content must not ghost through the panel when no backdrop is \
             published: first differing pixel at {:?}",
            ghost.map(|(i, _)| i)
        );
    }
}

#[test]
fn blur_rgba_region_crops_blurs_and_validates_geometry() {
    let err =
        blur_rgba_region(&[0u8; 16], 10, 10, 2, 1).expect_err("short RGBA buffer must be rejected");
    assert!(err.is_geometry());

    let valid = vec![255u8; 10 * 10 * 4];
    let err = blur_rgba_region(&valid, 10, 10, 10, 1)
        .expect_err("top inset covering whole frame must be rejected");
    assert!(err.is_geometry());

    let mut frame = vec![0u8; 20 * 20 * 4];
    for y in 0..20 {
        for x in 0..20 {
            let idx = (y * 20 + x) * 4;
            frame[idx] = if x < 10 { 200 } else { 20 };
            frame[idx + 3] = 255;
        }
    }
    let blurred = blur_rgba_region(&frame, 20, 20, 4, 3).expect("valid RGBA blur");
    assert_eq!(blurred.width, 20);
    assert_eq!(blurred.height, 16);
    // Mid-boundary pixel should be smoothed between 200 and 20.
    let mid_r = blurred.pixels[(8 * 20 + 10) * 4];
    assert!(
        mid_r > 20 && mid_r < 200,
        "expected smoothed edge, got {mid_r}"
    );
}
