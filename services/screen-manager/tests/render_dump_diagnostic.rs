#![cfg(feature = "ui")]
//! Host-side diagnostic: render one frame of the shell and dump raw RGBA to
//! `<tmp>/host_render.raw` for offline band-structure analysis.

use screen_manager::ui::{AppShell, Framebuffer, MainWindow, ThemePreset};
use slint::LogicalPosition;
use slint::platform::software_renderer::LineBufferProvider;
use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};
use slint::platform::{PointerEventButton, WindowEvent};

// The render harness runs on the live Pi (glibc 2.41) to validate the real
// UI with the device's fonts, so pin the `GLIBC_2.17` baseline: the kernel
// sysfs helpers use acosf/atan2f, and Slint 1.19's renderer pulls log10f.
speaker_common::glibc_compat!();
speaker_common::glibc_compat_log10!();

const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
const HEIGHT: usize = Framebuffer::DEFAULT_HEIGHT as usize;

struct ScratchBuffer {
    pixels: Vec<PremultipliedRgbaColor>,
}

impl ScratchBuffer {
    fn new() -> Self {
        Self {
            pixels: vec![PremultipliedRgbaColor::background(); WIDTH * HEIGHT],
        }
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
        let line_start = line * WIDTH;
        render_fn(&mut self.pixels[line_start + range.start..line_start + range.end]);
    }
}

/// Applies the seek-row timeline selected by `RENDER_DUMP_TIMELINE` so the
/// determinate / indeterminate / hidden presentations can be inspected offline.
fn apply_dump_timeline(shell: &AppShell, timeline: Option<&str>) {
    let playback = |position_ms, duration_ms| speaker_bluetooth::BtPlaybackState {
        status: "playing".into(),
        title: "Test Track Title".into(),
        artist: "Some Artist Name".into(),
        album: "Audiophile DSP Sink".into(),
        position_ms,
        duration_ms,
        ..speaker_bluetooth::BtPlaybackState::default()
    };
    match timeline {
        // Position only: thin shimmer track, position readout, no duration.
        Some("unknown") => shell.apply_bluetooth_playback(&playback(65_000, u32::MAX)),
        // Neither: the whole seek row is hidden.
        Some("none") => shell.apply_bluetooth_playback(&playback(u32::MAX, u32::MAX)),
        // Duration only: thin shimmer track that still shows the total length.
        Some("duration_only") => shell.apply_bluetooth_playback(&playback(u32::MAX, 860_000)),
        // Multi-hour track: `H:MM:SS` readouts are the widest strings the seek
        // row can produce and used to be clipped by the fixed 64 px slots.
        Some("long") => shell.set_playback_state(true, 3_723_000, 7_384_000),
        _ => shell.set_playback_state(true, 60_000, 500_000),
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
    let open_hud = std::env::var("RENDER_DUMP_HUD").ok().as_deref() == Some("1");
    let show_themes = std::env::var("RENDER_DUMP_THEMES").ok().as_deref() == Some("1");
    let at_min = std::env::var("RENDER_DUMP_AT_MIN").ok().as_deref() == Some("1");
    // Timeline states for the Now Playing seek row:
    //   `unknown` -> position only (indeterminate shimmer track)
    //   `none`    -> neither position nor duration (row hidden)
    let timeline = std::env::var("RENDER_DUMP_TIMELINE").ok();
    // Number of 20 ms timer steps to advance before rendering (lets the
    // indeterminate shimmer sweep reach a deterministic phase).
    let advance: u32 = std::env::var("RENDER_DUMP_ADVANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // Reproduce the device's `font_scale` (see `/etc/screen-manager/config.json`)
    // so text-overflow issues show up offline.
    let font_scale: f32 = std::env::var("RENDER_DUMP_FONT_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);

    let fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("platform init");
    let main_window = MainWindow::new().expect("MainWindow");
    if show_themes {
        main_window.set_show_themes(true);
    }
    let shell = AppShell::new(main_window);
    if at_min {
        // Park the fade stepper on its lower bound (0) so the row's minus
        // button renders in its disabled state.
        shell.window().set_volume_hud_fade_ms(0);
    }
    shell.apply_theme(ThemePreset::Peppy);
    shell.set_font_scale(font_scale);
    shell.set_track_metadata(
        "Test Track Title",
        "Some Artist Name",
        "Audiophile DSP Sink",
    );
    shell.set_playing(true);
    apply_dump_timeline(&shell, timeline.as_deref());
    shell.set_tab(tab);
    if open_hud {
        // Open the HUD the way the touch UI does: tap the volume icon in the
        // top-right of the status bar, then let the fade-in animation finish.
        for ev in [
            (WindowEvent::PointerPressed {
                position: LogicalPosition::new(690.0, 30.0),
                button: PointerEventButton::Left,
            }),
            (WindowEvent::PointerReleased {
                position: LogicalPosition::new(690.0, 30.0),
                button: PointerEventButton::Left,
            }),
        ] {
            window.dispatch_event(ev);
        }
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            slint::platform::update_timers_and_animations();
        }
    }

    for _ in 0..advance {
        std::thread::sleep(std::time::Duration::from_millis(20));
        slint::platform::update_timers_and_animations();
    }

    slint::platform::update_timers_and_animations();
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
