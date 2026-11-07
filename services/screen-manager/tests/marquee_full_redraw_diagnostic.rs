#![cfg(feature = "ui")]
//! Host-side diagnostic: drives the real shell UI frame-by-frame through the software
//! renderer's partial rendering to detect spurious full-window redraw storms and
//! attribute per-frame dirty regions to UI elements under simulated playback.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use screen_manager::ui::{AppShell, Framebuffer, MainWindow, RgbaPremulPixel, ThemePreset};
use slint::platform::software_renderer::{LineBufferProvider, TargetPixel};

const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
const HEIGHT: usize = Framebuffer::DEFAULT_HEIGHT as usize;
const FRAMES: usize = 700;

/// Flat RGBA premultiplied scratch buffer exposing scanlines to the Slint renderer.
struct ScratchBuffer {
    pixels: Vec<RgbaPremulPixel>,
}

impl ScratchBuffer {
    fn new() -> Self {
        Self {
            pixels: vec![RgbaPremulPixel::background(); WIDTH * HEIGHT],
        }
    }
}

impl LineBufferProvider for &mut ScratchBuffer {
    type TargetPixel = RgbaPremulPixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let line_start = line * WIDTH;
        render_fn(&mut self.pixels[line_start + range.start..line_start + range.end]);
    }
}

#[test]
fn shell_frame_loop_has_no_persistent_full_window_redraws() {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_track_metadata(
        "A Deliberately Long Track Title That Overflows The Marquee Viewport Width",
        "Some Artist Name",
        "Audiophile DSP Sink",
    );
    shell.set_playing(true);
    shell.set_tab(0);

    let full_frames: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
    let mut rect_histogram: BTreeMap<String, usize> = BTreeMap::new();
    let mut position_ms: u32 = 0;
    for frame in 0..FRAMES {
        if frame % 15 == 0 {
            position_ms = position_ms.saturating_add(250);
            shell.set_playback_state(true, position_ms, 240_000);
        }
        std::thread::sleep(std::time::Duration::from_micros(16_700));
        slint::platform::update_timers_and_animations();
        let mut buffer = ScratchBuffer::new();
        let mut dirty = (0_usize, 0_usize, 0_usize, 0_usize);
        window.draw_if_needed(|renderer| {
            let region = renderer.render_by_line(&mut buffer);
            let origin = region.bounding_box_origin();
            let size = region.bounding_box_size();
            dirty = (
                usize::try_from(origin.x).unwrap_or(0),
                usize::try_from(origin.y).unwrap_or(0),
                usize::try_from(size.width).unwrap_or(0),
                usize::try_from(size.height).unwrap_or(0),
            );
            // Rect formatting allocates per frame; keep it inside the sampling
            // branch so the histogram never perturbs the frame timing it measures.
            if frame % 100 == 0 {
                let rects: Vec<String> = region
                    .iter()
                    .map(|(pos, size)| {
                        format!("({},{},{},{})", pos.x, pos.y, size.width, size.height)
                    })
                    .collect();
                *rect_histogram
                    .entry(format!("frame {frame}: {}", rects.join(" ")))
                    .or_default() += 1;
            }
        });
        let (_x, _y, width, height) = dirty;
        if width * height > (WIDTH * HEIGHT) / 2 {
            full_frames.borrow_mut().push(frame);
        }
    }

    for (entry, count) in &rect_histogram {
        println!("{entry} x{count}");
    }
    let full = full_frames.borrow().clone();
    println!(
        "full-window frames: {} of {FRAMES}, indexes={:?}",
        full.len(),
        &full[..full.len().min(16)],
    );
    // Known bounded behavior: one full-window redraw per position-text (mm:ss) change
    // (~1 Hz), plus the startup draws. A full-redraw STORM (every frame, as caused by
    // per-frame item-tree destruction) must never reappear: no two consecutive
    // full-window frames, and no more than one per second of simulated time.
    let storm_free = full.windows(2).all(|w| w[1] - w[0] > 1) && full.len() <= (FRAMES / 60) + 2;
    assert!(storm_free, "full-window redraw storm detected: {full:?}");
}

/// Drives the real shell in Rust-side marquee composition mode: after a one-frame
/// background capture, scrolling must advance entirely in the framebuffer composer with
/// the Slint renderer staying idle on marquee-tick frames.
#[test]
fn shell_marquee_compose_mode_scrolls_without_slint_dirty() {
    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_track_metadata(
        "A Deliberately Long Track Title That Overflows The Marquee Viewport Width",
        "Some Artist Name With A Very Long Suffix That Definitely Overflows The Viewport",
        "Audiophile DSP Sink",
    );
    shell.set_playing(true);
    shell.set_tab(0);

    // Capture cycle (mirrors `core_daemon::sync_marquee_overlay`): the capture
    // frame must repaint the whole window or stale strip pixels would be baked
    // into the captured background (ghost text).
    shell.set_marquee_strips_visible(false);
    fb.render_frame_full_redraw(&window)
        .expect("capture render");
    let rects = shell.marquee_geometry();
    assert_eq!(rects.len(), 1);
    assert!(
        rects.iter().all(|&(_, _, w, h)| w > 0 && h > 0),
        "marquee viewports must keep their layout geometry while hidden: {rects:?}"
    );
    let mut backgrounds = Vec::with_capacity(rects.len());
    for &(x, y, w, h) in &rects {
        backgrounds.push(fb.capture_region(x, y, w, h).expect("capture region"));
    }
    shell.set_marquee_compose_in_rust(true);
    fb.set_marquee_rows(shell.marquee_row_specs());
    fb.install_marquee_backgrounds(backgrounds);
    shell.set_marquee_strips_visible(true);

    let specs = shell.marquee_row_specs();
    assert_eq!(specs.len(), 1, "the overflowing title row must compose");

    let title_rect = rects[0];
    let mut last_row: Option<Vec<u8>> = None;
    let mut distinct_rows = 0;
    let mut slint_drawn_frames = 0;
    // 160 frames cover the 2 s stationary pause plus ~1.6 s of scrolling.
    let frames = 160;
    for _ in 0..frames {
        std::thread::sleep(std::time::Duration::from_micros(16_700));
        slint::platform::update_timers_and_animations();
        fb.set_marquee_rows(shell.marquee_row_specs());
        let mut buffer = ScratchBuffer::new();
        if window.draw_if_needed(|renderer| {
            renderer.render_by_line(&mut buffer);
        }) {
            slint_drawn_frames += 1;
        }
        let _composed_or_skipped = fb.render_frame(&window).expect("compose render");
        let row = fb
            .capture_region(title_rect.0, title_rect.1, title_rect.2, title_rect.3)
            .expect("read composed row");
        let row_bytes: Vec<u8> = row.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
        if last_row.as_ref().is_some_and(|prev| *prev != row_bytes) {
            distinct_rows += 1;
        }
        last_row = Some(row_bytes);
    }

    println!(
        "slint_drawn_frames: {slint_drawn_frames}/{frames}, distinct composed rows: {distinct_rows}"
    );
    // Marquee motion must come from the composer, not from Slint redraws: only the
    // ~180 ms wave indicator ticks may wake the Slint renderer.
    assert!(
        slint_drawn_frames * 2 <= frames,
        "Slint drew on {slint_drawn_frames}/{frames} frames; marquee motion leaked into the renderer"
    );
    // The composed row must actually scroll (content changes as the strip advances).
    assert!(
        distinct_rows >= 2,
        "composed marquee row never changed; scroll composition is not advancing"
    );
}

/// Non-overflowing titles must still be visible in compose mode: the Slint strip
/// is hidden there, so the composer must install a static single-tile row
/// (scroll 0, cycle wider than the viewport) for the populated row.
#[test]
fn shell_marquee_compose_mode_shows_non_overflow_rows() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_track_metadata("Short", "Short", "Audiophile DSP Sink");
    shell.set_playing(true);
    shell.set_tab(0);

    // Capture cycle (mirrors `core_daemon::sync_marquee_overlay`).
    shell.set_marquee_strips_visible(false);
    fb.render_frame_full_redraw(&window)
        .expect("capture render");
    let rects = shell.marquee_geometry();
    let mut backgrounds = Vec::with_capacity(rects.len());
    for &(x, y, w, h) in &rects {
        backgrounds.push(fb.capture_region(x, y, w, h).expect("capture region"));
    }
    let title_background = backgrounds[0].clone();
    shell.set_marquee_compose_in_rust(true);
    fb.set_marquee_rows(shell.marquee_row_specs());
    fb.install_marquee_backgrounds(backgrounds);
    shell.set_marquee_strips_visible(true);

    let specs = shell.marquee_row_specs();
    assert_eq!(specs.len(), 1, "the populated row must compose");
    let title = &specs[0];
    assert_eq!(
        title.scroll_px, 0,
        "non-overflow row must not scroll: {title:?}"
    );
    assert!(
        title.cycle_px > title.rect.2,
        "non-overflow cycle must exceed the viewport width: {title:?}"
    );

    fb.render_frame(&window).expect("compose render");
    // The non-overflow strip content must actually appear on screen: capture the
    // title row and verify it is not the empty background.
    let row = fb
        .capture_region(title.rect.0, title.rect.1, title.rect.2, title.rect.3)
        .expect("read composed row");
    assert_ne!(
        &*row, &*title_background,
        "non-overflow strip was never composed onto the background"
    );
}

/// Re-capturing the marquee background after rows were already composed (e.g. a
/// song change that alters the row geometry) must produce a CLEAN snapshot: the
/// composer must be inert during the capture render, otherwise `render_frame`'s
/// post-draw compose step writes the still-installed strip back into the region
/// that is being captured (visible as a static ghost copy of the old text).
#[test]
fn shell_marquee_background_recapture_is_ghost_free() {
    let (mut fb, _ctrl) = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_track_metadata(
        "A Deliberately Long Track Title That Overflows The Marquee Viewport Width",
        "Some Artist Name With A Very Long Suffix That Definitely Overflows The Viewport",
        "Audiophile DSP Sink",
    );
    shell.set_playing(true);
    shell.set_tab(0);

    // Reference: capture the pristine background before any composition ran.
    shell.set_marquee_strips_visible(false);
    fb.render_frame_full_redraw(&window)
        .expect("pristine render");
    let rects = shell.marquee_geometry();
    let pristine: Vec<Box<[screen_manager::ui::Bgrx8888Pixel]>> = rects
        .iter()
        .map(|&(x, y, w, h)| fb.capture_region(x, y, w, h).expect("pristine capture"))
        .collect();

    // Bring the overlay up and let the composer run (this is the state a song
    // change starts from: composed pixels in the shadow buffer).
    shell.set_marquee_compose_in_rust(true);
    fb.set_marquee_rows(shell.marquee_row_specs());
    fb.install_marquee_backgrounds(pristine.clone());
    shell.set_marquee_strips_visible(true);
    for _ in 0..5 {
        slint::platform::update_timers_and_animations();
        fb.set_marquee_rows(shell.marquee_row_specs());
        fb.render_frame(&window).expect("compose render");
    }

    // Simulate a song change: geometry churns, the render loop re-captures. The
    // capture cycle must clear the rows so the composer cannot repaint the old
    // strip into the snapshot region during the capture render.
    shell.set_marquee_strips_visible(false);
    fb.set_marquee_rows(Vec::new());
    fb.render_frame_full_redraw(&window)
        .expect("recapture render");
    let recaptured: Vec<Box<[screen_manager::ui::Bgrx8888Pixel]>> = rects
        .iter()
        .map(|&(x, y, w, h)| fb.capture_region(x, y, w, h).expect("recapture"))
        .collect();
    for (i, (fresh, old)) in recaptured.iter().zip(pristine.iter()).enumerate() {
        assert_eq!(
            &**fresh, &**old,
            "row {i}: recaptured background contains composed strip pixels (ghost)"
        );
    }
}
