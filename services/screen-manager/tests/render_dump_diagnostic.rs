#![cfg(feature = "ui")]
//! Host-side diagnostic: render one frame of the shell and dump raw RGBA to
//! `<tmp>/host_render.raw` for offline band-structure analysis.

use screen_manager::ui::{AppShell, Framebuffer, MainWindow, RgbaPremulPixel, ThemePreset};
use slint::platform::software_renderer::LineBufferProvider;
use slint::platform::software_renderer::TargetPixel;

const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
const HEIGHT: usize = Framebuffer::DEFAULT_HEIGHT as usize;

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
fn dump_single_frame_for_layout_analysis() {
    let out_path: std::path::PathBuf = match std::env::var("RENDER_DUMP_PATH") {
        Ok(p) => p.into(),
        Err(_) => std::env::temp_dir().join("host_render.raw"),
    };
    let tab: i32 = std::env::var("RENDER_DUMP_TAB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    let shell = AppShell::new(main_window);
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_track_metadata(
        "Test Track Title",
        "Some Artist Name",
        "Audiophile DSP Sink",
    );
    shell.set_playing(true);
    shell.set_tab(tab);

    slint::platform::update_timers_and_animations();
    fb.set_marquee_rows(shell.marquee_row_specs());
    let mut buffer = ScratchBuffer::new();
    window.draw_if_needed(|renderer| {
        renderer.render_by_line(&mut buffer);
    });

    // Convert premultiplied RGBA into plain bytes (premul is fine for band stats).
    let bytes: Vec<u8> = buffer
        .pixels
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
        .collect();
    std::fs::write(out_path, bytes).expect("write dump");
}
