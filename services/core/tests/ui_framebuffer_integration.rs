#![cfg(feature = "ui")]
//! Integration tests for the Linux Framebuffer driver and Slint platform backend.

use std::fs;
use std::io;
use std::path::Path;
use std::rc::Rc;

use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};
use speaker_core::diag::ScopedTempDir;
use speaker_core::ui::framebuffer_test_helpers::FramebufferPlatform;
use speaker_core::ui::{Bgrx8888Pixel, Framebuffer, FramebufferError};

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
    assert!(fb.is_mocked());
    assert!(!fb.is_hardware_panel());
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 0);

    fb.unblank().unwrap();
    assert_eq!(fb.unblank_count(), 1);

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
fn mock_fault_injection_unblank_and_render() {
    let (mut fb, ctrl) = Framebuffer::new_mocked();

    ctrl.set_unblank_error("simulated unblank failure");
    let err = fb.unblank().unwrap_err();
    assert!(err.is_io());
    assert!(err.summary().contains("simulated unblank failure"));
    assert_eq!(fb.unblank_count(), 0);
    ctrl.clear_unblank_error();
    fb.unblank().unwrap();
    assert_eq!(fb.unblank_count(), 1);

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

    assert_eq!(ctrl.counters(), (1, 1));
}

#[test]
fn framebuffer_nonexistent_device() {
    let err = Framebuffer::open("/dev/nonexistent_fb_device_12345", None::<&Path>).unwrap_err();
    assert!(err.is_io());
    assert!(err.summary().contains("framebuffer I/O error"));
}

#[test]
fn framebuffer_constants_and_hardware_counters() {
    assert_eq!(Framebuffer::DEFAULT_WIDTH, 800);
    assert_eq!(Framebuffer::DEFAULT_HEIGHT, 480);
    assert_eq!(Framebuffer::DEFAULT_FB_PATH, "/dev/fb0");
    assert_eq!(
        Framebuffer::DEFAULT_BLANK_PATH,
        "/sys/class/graphics/fb0/blank"
    );

    let temp = ScopedTempDir::try_new("fb_counts_test").unwrap();
    let dev = temp.path().join("dev_fb");
    fs::write(&dev, vec![0u8; 800 * 480 * 4]).unwrap();

    let mut fb = Framebuffer::open(&dev, None::<&Path>).unwrap();
    assert_eq!(fb.unblank_count(), 0);
    assert_eq!(fb.render_count(), 0);
    fb.unblank().unwrap();
}

#[test]
fn framebuffer_hardware_tempfile_mmap() {
    let temp = ScopedTempDir::try_new("fb_hw_test").unwrap();
    let dev_file = temp.path().join("fake_fb");
    let blank_file = temp.path().join("fake_blank");

    fs::write(&dev_file, vec![0u8; 800 * 480 * 4]).unwrap();
    fs::write(&blank_file, b"1\n").unwrap();

    let mut fb = Framebuffer::open(&dev_file, Some(&blank_file)).unwrap();
    assert!(!fb.is_mocked());
    assert_eq!(fb.unblank_count(), 0);

    fb.unblank().unwrap();
    let blank_content = fs::read_to_string(&blank_file).unwrap();
    assert_eq!(blank_content, "0\n");

    let window = fb.init_platform().unwrap();
    window.request_redraw();
    let drawn = fb.render_frame(&window).unwrap();
    assert!(drawn);
    assert_eq!(fb.pixels().len(), 384_000);
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
fn fb_named_regular_file_skips_sysfs_contract() {
    let temp = ScopedTempDir::try_new("fb_named_file_test").unwrap();
    let dev = temp.path().join("fb0");
    fs::write(&dev, vec![0u8; 800 * 480 * 4]).unwrap();
    let fb = Framebuffer::open(&dev, None::<&Path>).unwrap();
    assert!(!fb.is_mocked());
    assert_eq!(fb.pixel_count(), 384_000);
    assert_eq!(fb.byte_len(), 1_536_000);
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

#[test]
fn validate_sysfs_geometry_cases() {
    let temp = ScopedTempDir::try_new("sysfs_geom_test").unwrap();
    let valid = temp.path().join("valid");
    fs::write(&valid, "800,480\n").unwrap();
    Framebuffer::validate_sysfs_geometry(&valid, "fb0").unwrap();

    let mismatch = temp.path().join("mismatch");
    fs::write(&mismatch, "1920,1080\n").unwrap();
    let err_mismatch = Framebuffer::validate_sysfs_geometry(&mismatch, "fb0").unwrap_err();
    assert!(err_mismatch.is_invalid_geometry());

    let malformed = temp.path().join("malformed");
    fs::write(&malformed, "800\n").unwrap();
    let err_malformed = Framebuffer::validate_sysfs_geometry(&malformed, "fb0").unwrap_err();
    assert!(err_malformed.is_invalid_geometry());

    let bad_w = temp.path().join("bad_w");
    fs::write(&bad_w, "invalid,480\n").unwrap();
    let err_bad_w = Framebuffer::validate_sysfs_geometry(&bad_w, "fb0").unwrap_err();
    assert!(err_bad_w.is_invalid_geometry());

    let bad_h = temp.path().join("bad_h");
    fs::write(&bad_h, "800,invalid\n").unwrap();
    let err_bad_h = Framebuffer::validate_sysfs_geometry(&bad_h, "fb0").unwrap_err();
    assert!(err_bad_h.is_invalid_geometry());
    let missing = temp.path().join("missing_file");
    let err_missing = Framebuffer::validate_sysfs_geometry(&missing, "fb0").unwrap_err();
    assert!(err_missing.is_io());
}

#[test]
fn validate_sysfs_format_contract_cases() {
    let temp = ScopedTempDir::try_new("sysfs_fmt_test").unwrap();

    let bpp_ok = temp.path().join("bpp_ok");
    fs::write(&bpp_ok, "32\n").unwrap();
    Framebuffer::validate_sysfs_bits_per_pixel(&bpp_ok, "fb0").unwrap();

    let bpp16 = temp.path().join("bpp16");
    fs::write(&bpp16, "16\n").unwrap();
    let err_bpp = Framebuffer::validate_sysfs_bits_per_pixel(&bpp16, "fb0").unwrap_err();
    assert!(err_bpp.is_invalid_geometry());
    assert!(err_bpp.summary().contains("reports 16bpp"));

    let bpp_junk = temp.path().join("bpp_junk");
    fs::write(&bpp_junk, "notanumber\n").unwrap();
    assert!(
        Framebuffer::validate_sysfs_bits_per_pixel(&bpp_junk, "fb0")
            .unwrap_err()
            .summary()
            .contains("malformed sysfs bits_per_pixel")
    );

    let stride_ok = temp.path().join("stride_ok");
    fs::write(&stride_ok, "3200\n").unwrap();
    Framebuffer::validate_sysfs_stride(&stride_ok, "fb0").unwrap();

    let stride_padded = temp.path().join("stride_padded");
    fs::write(&stride_padded, "3840\n").unwrap();
    let err_stride = Framebuffer::validate_sysfs_stride(&stride_padded, "fb0").unwrap_err();
    assert!(err_stride.is_invalid_geometry());
    assert!(err_stride.summary().contains("stride 3840"));

    let stride_missing = temp.path().join("stride_missing");
    assert!(
        Framebuffer::validate_sysfs_stride(&stride_missing, "fb0")
            .unwrap_err()
            .is_io()
    );
}

#[test]
fn fb_scanout_contract_fails_closed_on_missing_attributes() {
    let temp = ScopedTempDir::try_new("fb_contract_test").unwrap();

    // Hermetic: temp dir has no sysfs attributes at all, so the contract
    // must fail closed with a typed invalid_geometry error.
    let err = Framebuffer::validate_fb_scanout_contract(temp.path(), "fb0").unwrap_err();
    assert!(err.is_invalid_geometry());
    assert!(err.summary().contains("virtual_size"));

    // A directory with a complete, matching contract passes.
    let contract_dir = temp.path().join("contract");
    fs::create_dir(&contract_dir).unwrap();
    fs::write(contract_dir.join("virtual_size"), "800,480\n").unwrap();
    fs::write(contract_dir.join("bits_per_pixel"), "32\n").unwrap();
    fs::write(contract_dir.join("stride"), "3200\n").unwrap();
    Framebuffer::validate_fb_scanout_contract(&contract_dir, "fb0").unwrap();

    // Partial contract (missing stride) fails closed.
    let partial_dir = temp.path().join("partial");
    fs::create_dir(&partial_dir).unwrap();
    fs::write(partial_dir.join("virtual_size"), "800,480\n").unwrap();
    fs::write(partial_dir.join("bits_per_pixel"), "32\n").unwrap();
    let err_partial = Framebuffer::validate_fb_scanout_contract(&partial_dir, "fb0").unwrap_err();
    assert!(err_partial.is_invalid_geometry());
    assert!(err_partial.summary().contains("stride"));
}

#[test]
fn framebuffer_regular_file_truncated_falls_back_to_buffered() {
    let temp = ScopedTempDir::try_new("fb_truncated_test").unwrap();
    let dev_file = temp.path().join("short_fb");
    // Only write 16 bytes (much smaller than 800 * 480 * 4)
    fs::write(&dev_file, vec![0u8; 16]).unwrap();

    let mut fb = Framebuffer::open(&dev_file, None::<&Path>).unwrap();
    assert!(!fb.is_mocked());
    let window = fb.init_platform().unwrap();
    window.request_redraw();
    let rendered = fb.render_frame(&window).unwrap();
    assert!(rendered);
    assert_eq!(fb.pixels().len(), 384_000);
}

#[test]
fn framebuffer_unblank_with_missing_blank_file() {
    let temp = ScopedTempDir::try_new("fb_missing_blank_test").unwrap();
    let dev = temp.path().join("fake_fb");
    fs::write(&dev, vec![0u8; 800 * 480 * 4]).unwrap();
    let non_existent_blank = temp.path().join("does_not_exist");
    let mut fb = Framebuffer::open(&dev, Some(&non_existent_blank)).unwrap();
    fb.unblank().unwrap();
}
