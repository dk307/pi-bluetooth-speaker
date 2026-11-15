#![cfg(feature = "ui")]
//! Integration tests for the offscreen framebuffer test harness and its
//! hermetic Slint platform backend.
//!
//! Production rendering goes through Slint's `backend-linuxkms`; this suite
//! only covers the in-memory BGRX surface, the software-renderer window
//! plumbing, the mock fault-injection controls, and the test event loop.

use std::io;
use std::rc::Rc;

use screen_manager::ui::framebuffer_test_helpers::{FramebufferPlatform, signal_test_loop_quit};
use screen_manager::ui::{Bgrx8888Pixel, Framebuffer, FramebufferError};
use slint::platform::Platform as _;
use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};

#[test]
fn bgrx_pixel_byte_layout_and_conversion() {
    let p = Bgrx8888Pixel::new(0x12, 0x34, 0x56);
    assert_eq!(p.red, 0x12);
    assert_eq!(p.green, 0x34);
    assert_eq!(p.blue, 0x56);
    assert_eq!(p.alpha, 255);

    let slice = [p];
    let bytes = Bgrx8888Pixel::as_bytes(&slice);
    assert_eq!(bytes, &[0x56, 0x34, 0x12, 0xFF]);

    let mut raw = [0xAA, 0xBB, 0xCC, 0xDD];
    let pixels = Bgrx8888Pixel::try_from_bytes_mut(&mut raw).unwrap();
    assert_eq!(pixels.len(), 1);
    assert_eq!(pixels[0].blue, 0xAA);
    assert_eq!(pixels[0].green, 0xBB);
    assert_eq!(pixels[0].red, 0xCC);
    assert_eq!(pixels[0].alpha, 0xDD);
}

#[test]
fn bgrx_target_pixel_blending() {
    let p_rgb = <Bgrx8888Pixel as TargetPixel>::from_rgb(10, 20, 30);
    assert_eq!(p_rgb, Bgrx8888Pixel::new(10, 20, 30));

    let mut p = Bgrx8888Pixel::background();
    assert_eq!(p, Bgrx8888Pixel::new(0, 0, 0));

    // Blend with fully opaque red color
    p.blend(PremultipliedRgbaColor {
        red: 200,
        green: 100,
        blue: 50,
        alpha: 255,
    });
    assert_eq!(p.red, 200);
    assert_eq!(p.green, 100);
    assert_eq!(p.blue, 50);

    // Blend with 50% transparent overlay
    let mut bg = Bgrx8888Pixel::new(100, 100, 100);
    bg.blend(PremultipliedRgbaColor {
        red: 50,
        green: 0,
        blue: 0,
        alpha: 128,
    });
    assert!(bg.red >= 90);
}

#[test]
fn mock_framebuffer_lifecycle() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    assert_eq!(fb.width(), 800);
    assert_eq!(fb.height(), 480);
    assert_eq!(fb.pixel_count(), 384_000);
    assert_eq!(fb.byte_len(), 1_536_000);
    assert_eq!(fb.render_count(), 0);

    let window = fb.init_platform().unwrap();
    // Request redraw
    window.request_redraw();

    let drawn = fb.render_frame(&window).unwrap();
    assert!(drawn);
    assert_eq!(fb.render_count(), 1);

    // Subsequent render without redraw request should return false
    let drawn_again = fb.render_frame(&window).unwrap();
    assert!(!drawn_again);
    assert_eq!(fb.render_count(), 1);
}

#[test]
fn mock_fault_injection_render() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();

    let window = fb.init_platform().unwrap();
    window.request_redraw();
    ctrl.set_render_error("simulated render failure");
    let err = fb.render_frame(&window).unwrap_err();
    assert!(err.is_io());
    assert!(err.summary().contains("simulated render failure"));
    assert_eq!(fb.render_count(), 0);
    ctrl.clear_render_error();
    assert!(fb.render_frame(&window).unwrap());
    assert_eq!(fb.render_count(), 1);

    assert_eq!(ctrl.counters(), 1);
}

#[test]
fn framebuffer_error_display_and_summary() {
    use std::error::Error as _;

    let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied");
    let err = FramebufferError::io("/dev/fb0", io_err);
    assert!(err.is_io());
    assert!(!err.is_invalid_geometry());
    assert!(!err.is_platform());
    assert!(err.source().is_some());
    assert!(err.io_error().is_some());
    assert_eq!(
        err.io_error().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    let _ = err.backtrace();

    let sum = err.summary();
    assert!(sum.contains("/dev/fb0"));
    assert!(sum.contains("Permission denied"));
    let disp = err.to_string();
    assert!(disp.contains("framebuffer I/O error on '/dev/fb0'"));

    let plat_err = FramebufferError::platform("failed to set context");
    assert!(plat_err.is_platform());
    assert!(!plat_err.is_io());
    assert!(plat_err.source().is_none());
    assert!(plat_err.io_error().is_none());
    assert!(
        plat_err
            .summary()
            .contains("slint platform registration error")
    );

    let geom_err = FramebufferError::invalid_geometry(800, 480, "test mismatch");
    assert!(geom_err.is_invalid_geometry());
    assert!(!geom_err.is_io());
    assert!(geom_err.source().is_none());
    assert!(geom_err.io_error().is_none());
}

#[test]
fn try_from_bytes_mut_validation() {
    let mut valid_bytes = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let pixels = Bgrx8888Pixel::try_from_bytes_mut(&mut valid_bytes).unwrap();
    assert_eq!(pixels.len(), 2);

    let mut invalid_bytes = [1u8, 2, 3];
    let err = Bgrx8888Pixel::try_from_bytes_mut(&mut invalid_bytes).unwrap_err();
    assert!(err.is_invalid_geometry());
    assert!(err.summary().contains("is not a multiple of 4"));
}

#[test]
fn init_platform_idempotent_window_reuse() {
    let (fb, _ctrl) = Framebuffer::new_mocked();
    let win1 = fb.init_platform().unwrap();
    let win2 = fb.init_platform().unwrap();
    assert!(Rc::ptr_eq(&win1, &win2));
}

#[test]
fn framebuffer_platform_window_adapter_fallback() {
    let platform = FramebufferPlatform;
    let adapter = slint::platform::Platform::create_window_adapter(&platform);
    let _ = adapter.unwrap();
}

/// After the primary window claim, further components must receive their own
/// throwaway offscreen window (never the primary) so a second component can be
/// created without disturbing the primary window's state.
#[test]
fn framebuffer_platform_secondary_window_is_independent_of_primary() {
    let (fb, _ctrl) = Framebuffer::new_mocked();
    let primary = fb.init_platform().expect("platform registration");
    let first = slint::platform::Platform::create_window_adapter(&FramebufferPlatform)
        .expect("primary claim adapter");
    let secondary = slint::platform::Platform::create_window_adapter(&FramebufferPlatform)
        .expect("secondary window adapter");
    let primary: Rc<dyn slint::platform::WindowAdapter> = primary;
    assert!(
        Rc::ptr_eq(&primary, &first),
        "the first adapter claim must bind to the primary window"
    );
    assert!(
        !Rc::ptr_eq(&primary, &secondary),
        "secondary components must not share the primary window"
    );
}

#[test]
fn capture_region_rejects_out_of_bounds() {
    let (fb, _ctrl) = Framebuffer::new_mocked();
    let err = fb.capture_region(797, 0, 4, 1).unwrap_err();
    assert!(err.is_invalid_geometry());
    let err = fb.capture_region(0, 0, 0, 1).unwrap_err();
    assert!(err.is_invalid_geometry());
    let err = fb.capture_region(0, 480, 4, 1).unwrap_err();
    assert!(err.is_invalid_geometry());
    fb.capture_region(0, 0, 4, 1).unwrap();
}

/// The hermetic event loop must advance Slint timers, drain the cross-thread
/// invoke queue, and exit once `slint::quit_event_loop()` fires.
#[test]
fn test_event_loop_runs_timers_and_quits() {
    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();

    let timer_fired = Rc::new(std::cell::Cell::new(false));
    let invoke_drained = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let fired = Rc::clone(&timer_fired);
    let drained = std::sync::Arc::clone(&invoke_drained);
    slint::Timer::single_shot(std::time::Duration::from_millis(5), move || {
        fired.set(true);
        slint::invoke_from_event_loop(move || {
            drained.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("invoke queued");
        slint::quit_event_loop().expect("quit via event-loop proxy");
    });

    FramebufferPlatform
        .run_event_loop()
        .expect("test event loop returns cleanly");
    assert!(
        timer_fired.get(),
        "single-shot timer must fire inside the loop"
    );
    assert!(
        invoke_drained.load(std::sync::atomic::Ordering::SeqCst),
        "queued closure must be drained by the loop"
    );
}

/// Direct quit signalling (the fallback used when a platform exposes no
/// event-loop proxy) must also terminate the loop.
#[test]
fn test_event_loop_quits_via_direct_signal() {
    let (fb, _ctrl) = Framebuffer::new_mocked();
    let _window = fb.init_platform().unwrap();

    slint::Timer::single_shot(std::time::Duration::from_millis(5), signal_test_loop_quit);

    FramebufferPlatform
        .run_event_loop()
        .expect("test event loop exits on direct signal");
}
