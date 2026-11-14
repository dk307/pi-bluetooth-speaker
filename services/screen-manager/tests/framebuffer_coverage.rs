#![cfg(feature = "ui")]
//! Coverage tests for framebuffer branches the main suite misses: the fbdev
//! character-device scanout contract, unblank fault paths, the file-mapped
//! full-redraw path, hardware-backend region capture, marquee composer edge
//! rows, and dirty-box unioning in the mmap render path.
//!
//! Hermetic by construction: temp files stand in for `/dev/fb0`, an `fb*`
//! symlink to `/dev/null` models a character device (the missing sysfs
//! contract must fail closed), and `/dev/full` exercises the write error.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use screen_manager::ui::{
    Bgrx8888Pixel, FrameTiming, Framebuffer, MainWindow, MarqueeRowSpec, RgbaPremulPixel,
};
use slint::ComponentHandle;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use speaker_common::diag::ScopedTempDir;

const SCANOUT_LEN: usize = 800 * 480 * 4;

fn temp_file(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, vec![0_u8; bytes]).expect("write temp fb file");
    path
}

fn transparent_strip(len: usize) -> Arc<[RgbaPremulPixel]> {
    Arc::from(vec![RgbaPremulPixel::default(); len].into_boxed_slice())
}

fn row(
    rect: (u32, u32, u32, u32),
    scroll_px: u32,
    cycle_px: u32,
    strip: Arc<[RgbaPremulPixel]>,
    strip_w: u32,
    strip_id: u64,
) -> MarqueeRowSpec {
    MarqueeRowSpec {
        rect,
        scroll_px,
        cycle_px,
        strip,
        strip_w,
        strip_id,
    }
}

#[test]
fn frame_timing_cpu_us_sums_active_phases() {
    let timing = FrameTiming {
        draw_us: 100,
        compose_us: 50,
        blit_us: 25,
        ..FrameTiming::default()
    };
    assert_eq!(timing.cpu_us(), 175);
    assert_eq!(FrameTiming::default().cpu_us(), 0);
}

#[test]
fn fb_named_character_device_fails_closed_without_sysfs_contract() {
    let temp = ScopedTempDir::try_new("fb_char_dev").expect("temp dir");
    // A symlink named `fb9` pointing at `/dev/null` models an fbdev character
    // device node without touching the real `/dev/fb0` (and without requiring
    // root to `mknod`). The absent `/sys/class/graphics/fb9` contract must
    // reject the open instead of mapping unvalidated geometry.
    let link = temp.path().join("fb9");
    std::os::unix::fs::symlink("/dev/null", &link).expect("symlink to /dev/null");

    let err = Framebuffer::open(&link, None::<&Path>).expect_err("contract must fail closed");
    assert!(err.is_invalid_geometry(), "{err}");
    assert!(err.summary().contains("virtual_size"), "{err}");
    assert!(err.summary().contains("fb9"), "{err}");
}

#[test]
fn unblank_reports_io_faults_and_skips_missing_paths() {
    let temp = ScopedTempDir::try_new("fb_unblank_faults").expect("temp dir");
    let dev = temp_file(temp.path(), "fake_fb", SCANOUT_LEN);

    // 1. Blank path is a directory: `OpenOptions::write` fails with EISDIR.
    let blocked = temp.path().join("blocked_blank");
    fs::create_dir(&blocked).expect("create dir");
    let mut fb = Framebuffer::open(&dev, Some(&blocked)).expect("open fb");
    let err = fb.unblank().expect_err("opening a directory must fail");
    assert!(err.is_io(), "{err}");

    // 2. `/dev/full` accepts the open but rejects the write with ENOSPC.
    assert!(
        Path::new("/dev/full").exists(),
        "/dev/full must exist to exercise the unblank write fault"
    );
    let mut fb = Framebuffer::open(&dev, Some(Path::new("/dev/full"))).expect("open fb");
    let err = fb.unblank().expect_err("/dev/full write must fail");
    assert!(err.is_io(), "{err}");
    assert!(err.summary().contains("/dev/full"), "{err}");

    // 3. Missing blank path skips unblank; the warning argument line only
    //    executes while a tracing subscriber is active.
    let missing = temp.path().join("does_not_exist");
    let mut fb = Framebuffer::open(&dev, Some(&missing)).expect("open fb");
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        fb.unblank().expect("missing blank path must not fail");
    });
}

/// Full-size file-backed framebuffer: mmap path with Slint dirty tracking.
fn mmap_fb_with_window(dir: &Path) -> (Framebuffer, Rc<MinimalSoftwareWindow>) {
    let dev = temp_file(dir, "mmap_fb", SCANOUT_LEN);
    let fb = Framebuffer::open(&dev, None::<&Path>).expect("open mmap fb");
    let window = fb.init_platform().expect("platform init");
    (fb, window)
}

#[test]
fn mmap_full_redraw_invalidates_rows_captures_and_unions_dirty_boxes() {
    let temp = ScopedTempDir::try_new("fb_mmap_union").expect("temp dir");
    let (mut fb, window) = mmap_fb_with_window(temp.path());

    // A real component so the first full redraw produces a non-empty Slint
    // dirty region that intersects the marquee rows.
    let main = MainWindow::new().expect("MainWindow");
    main.show().expect("show MainWindow");

    let rows = vec![
        row((0, 0, 4, 1), 0, 8, transparent_strip(4), 4, 1),
        row((10, 0, 4, 1), 0, 8, transparent_strip(4), 4, 2),
    ];
    fb.set_marquee_rows(rows.clone());
    // Hardware-backend region capture feeds the composer backgrounds.
    let bg0 = fb.capture_region(0, 0, 4, 1).expect("capture row 0");
    let bg1 = fb.capture_region(10, 0, 4, 1).expect("capture row 1");
    fb.install_marquee_backgrounds(vec![bg0, bg1]);

    // Force-full frame: repaint-buffer switching + intersecting invalidation +
    // two-row compose (dirty union inside compose_changed) + dirty-box union.
    let drawn = fb
        .render_frame_full_redraw(&window)
        .expect("full redraw render");
    assert!(drawn);
    let timing = fb.last_frame_timing().expect("timing recorded");
    assert!(timing.dirty_px > 0, "full redraw must report dirty pixels");

    // Second frame: nothing dirty -> Slint skips the draw, rows keep their
    // fingerprints, no composition work, nothing to blit.
    assert!(!fb.render_frame(&window).expect("idle render"));

    // Third frame: scroll advances -> composer-only work. The Slint dirty box
    // is empty, so the union must fall back to the marquee box alone.
    let scrolled: Vec<MarqueeRowSpec> = rows
        .iter()
        .map(|spec| MarqueeRowSpec {
            scroll_px: spec.scroll_px + 1,
            ..spec.clone()
        })
        .collect();
    fb.set_marquee_rows(scrolled);
    assert!(fb.render_frame(&window).expect("compose-only render"));
}

#[test]
fn truncated_file_framebuffer_full_redraw_then_idle_frame() {
    let temp = ScopedTempDir::try_new("fb_file_frame").expect("temp dir");
    // Far smaller than the scanout: mmap is skipped, the buffered file path
    // with its full-flush semantics is used instead.
    let dev = temp_file(temp.path(), "short_fb", 16);
    let mut fb = Framebuffer::open(&dev, None::<&Path>).expect("open fb");
    assert!(!fb.is_mocked());
    let window = fb.init_platform().expect("platform init");

    // Forced full redraw takes the `force_full` repaint-buffer branches.
    assert!(
        fb.render_frame_full_redraw(&window)
            .expect("forced full redraw")
    );

    // No redraw pending: Slint skips the draw and the frame is not effective,
    // so neither the invalidation nor the flush branch runs.
    assert!(!fb.render_frame(&window).expect("idle render"));
}

#[test]
fn mock_composer_handles_out_of_bounds_and_degenerate_rows() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");

    let rows = vec![
        // Row extending past the right edge: skipped before composing.
        row((798, 0, 4, 1), 0, 8, transparent_strip(4), 4, 10),
        // Background shorter than the row: per-line slice fallback.
        row((0, 0, 10, 1), 0, 8, transparent_strip(4), 4, 11),
        // Two-line row with a single-line strip: line 1 skips composition.
        row((0, 2, 10, 2), 0, 8, transparent_strip(4), 4, 12),
        // Zero tile cycle: composition returns immediately.
        row((0, 4, 4, 1), 0, 0, transparent_strip(4), 4, 13),
        // Scroll beyond the strip width: the first tile body is skipped.
        row((0, 6, 8, 1), 4, 4, transparent_strip(4), 4, 14),
    ];
    fb.set_marquee_rows(rows);
    fb.install_marquee_backgrounds(vec![
        vec![Bgrx8888Pixel::default(); 4].into_boxed_slice(),
        vec![Bgrx8888Pixel::default(); 4].into_boxed_slice(),
        vec![Bgrx8888Pixel::default(); 20].into_boxed_slice(),
        vec![Bgrx8888Pixel::default(); 4].into_boxed_slice(),
        vec![Bgrx8888Pixel::default(); 8].into_boxed_slice(),
    ]);

    assert!(
        fb.render_frame(&window).expect("compose render"),
        "at least one in-bounds row must compose"
    );
}
