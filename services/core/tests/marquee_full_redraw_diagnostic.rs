#![cfg(feature = "ui")]
//! Host-side diagnostic: drives the real shell UI frame-by-frame through the software
//! renderer's partial rendering to detect spurious full-window redraw storms and
//! attribute per-frame dirty regions to UI elements under simulated playback.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use slint::platform::software_renderer::{LineBufferProvider, TargetPixel};
use speaker_core::ui::{AppShell, Framebuffer, MainWindow, RgbaPremulPixel, ThemePreset};

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
