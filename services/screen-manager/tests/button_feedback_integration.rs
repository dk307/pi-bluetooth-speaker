#![cfg(feature = "ui")]
//! Regression tests for Material 3 press feedback on the now-playing screen.
//!
//! The appliance uses a touchscreen with very short taps. Before the shared
//! `StateLayerArea` fix, its state layer and ripple were `if pressed : ...`
//! items, so a quick tap created and destroyed them before the opacity
//! animation could render — the buttons looked completely unresponsive. These
//! tests drive the real Slint window frame-by-frame and assert that a visible
//! state change occurs while pressed AND persists briefly after release.

use screen_manager::ui::{AppShell, Framebuffer, MainWindow, ThemePreset};
use slint::LogicalPosition;
use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, PremultipliedRgbaColor, TargetPixel,
};
use slint::platform::{PointerEventButton, WindowEvent};

speaker_common::glibc_compat!();
speaker_common::glibc_compat_log10!();

const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
const HEIGHT: usize = Framebuffer::DEFAULT_HEIGHT as usize;

/// Persistent frame buffer: the software renderer only redraws dirty regions,
/// so every frame must accumulate into the same buffer.
struct ScratchBuffer {
    pixels: Vec<PremultipliedRgbaColor>,
}

impl ScratchBuffer {
    fn new() -> Self {
        Self {
            pixels: vec![PremultipliedRgbaColor::background(); WIDTH * HEIGHT],
        }
    }

    fn draw(&mut self, window: &MinimalSoftwareWindow) {
        window.draw_if_needed(|renderer| {
            renderer.render_by_line(self);
        });
    }

    fn region(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for y in y0..y1 {
            for x in x0..x1 {
                let p = self.pixels[y * WIDTH + x];
                out.extend_from_slice(&[p.red, p.green, p.blue, p.alpha]);
            }
        }
        out
    }
}

impl LineBufferProvider for &mut ScratchBuffer {
    type TargetPixel = PremultipliedRgbaColor;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let start = line * WIDTH;
        render_fn(&mut self.pixels[start + range.start..start + range.end]);
    }
}

fn count_diff(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).filter(|(x, y)| x != y).count()
}

struct Harness {
    window: std::rc::Rc<MinimalSoftwareWindow>,
    shell: AppShell,
    buffer: ScratchBuffer,
}

fn harness() -> Harness {
    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_font_scale(1.15);
    shell.set_track_metadata("Test Track Title", "Some Artist", "Audiophile DSP Sink");
    shell.set_playing(true);
    shell.set_tab(0);
    let mut buffer = ScratchBuffer::new();
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        slint::platform::update_timers_and_animations();
        buffer.draw(&window);
    }
    Harness {
        window,
        shell,
        buffer,
    }
}

fn press(h: &mut Harness, x: f32, y: f32) {
    h.window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(x, y),
        button: PointerEventButton::Left,
    });
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        slint::platform::update_timers_and_animations();
        h.buffer.draw(&h.window);
    }
}

fn release(h: &mut Harness, x: f32, y: f32) {
    h.window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(x, y),
        button: PointerEventButton::Left,
    });
    // A single frame after release: the state layer must still be fading out.
    slint::platform::update_timers_and_animations();
    h.buffer.draw(&h.window);
}

#[test]
fn settings_button_renders_press_feedback_and_opens_settings() {
    let mut h = harness();
    // Settings tile: 56 dp * 1.075 widget scale at 20 dp padding -> (718..778, 398..458).
    let region = (715, 395, 781, 461);
    let idle = h.buffer.region(region.0, region.1, region.2, region.3);

    press(&mut h, 748.0, 428.0);
    let pressed = h.buffer.region(region.0, region.1, region.2, region.3);
    assert!(
        count_diff(&idle, &pressed) > 100,
        "settings button must show a visible pressed state"
    );

    release(&mut h, 748.0, 428.0);
    let released = h.buffer.region(region.0, region.1, region.2, region.3);
    assert!(
        count_diff(&idle, &released) > 50,
        "press feedback must persist briefly after a quick tap instead of vanishing instantly"
    );
    assert_eq!(
        h.shell.current_tab(),
        5,
        "settings tile must open the Settings tab"
    );
}

#[test]
fn transport_buttons_render_press_feedback() {
    // (label, center x, center y, region)
    let buttons = [
        ("previous", 468.0, 345.0, (432, 309, 504, 381)),
        ("play/pause", 588.0, 345.0, (540, 297, 636, 393)),
        ("next", 708.0, 345.0, (672, 309, 744, 381)),
    ];
    for (label, x, y, region) in buttons {
        let mut h = harness();
        let idle = h.buffer.region(region.0, region.1, region.2, region.3);
        press(&mut h, x, y);
        let pressed = h.buffer.region(region.0, region.1, region.2, region.3);
        assert!(
            count_diff(&idle, &pressed) > 100,
            "{label} transport button must show a visible pressed state"
        );
    }
}
