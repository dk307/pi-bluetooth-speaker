#![cfg(feature = "ui")]
//! Integration test for the Now Playing top bar volume readout: the level stays
//! visible while muted and is tinted like the mute icon.

use screen_manager::ui::{AppShell, Bgrx8888Pixel, Framebuffer, MainWindow, MaterialPalette};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::{Color, ComponentHandle};

const WIDTH: usize = Framebuffer::DEFAULT_WIDTH as usize;
/// Height of the 60 px top bar.
const BAR_HEIGHT: usize = 60;
/// Width of the mute icon image; the level readout is rendered to its right.
const MUTE_ICON_WIDTH: usize = 44;

fn shell_with_window() -> (AppShell, std::rc::Rc<MinimalSoftwareWindow>, Framebuffer) {
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

/// Columns of the top bar that contain a pixel of the given color.
fn columns_with_color(pixels: &[Bgrx8888Pixel], color: Color) -> Vec<usize> {
    let channel = |value: u8| -> i16 { i16::from(value) };
    let (r, g, b) = (
        channel(color.red()),
        channel(color.green()),
        channel(color.blue()),
    );
    let matches = |p: &Bgrx8888Pixel| {
        (i16::from(p.red) - r).abs() <= 2
            && (i16::from(p.green) - g).abs() <= 2
            && (i16::from(p.blue) - b).abs() <= 2
    };
    let mut columns = Vec::new();
    for x in 0..WIDTH {
        let hit = (0..BAR_HEIGHT).any(|y| matches(&pixels[y * WIDTH + x]));
        if hit {
            columns.push(x);
        }
    }
    columns
}

#[test]
fn muted_top_bar_still_shows_the_volume_level() {
    let (shell, window, mut fb) = shell_with_window();
    shell.set_volume(75.0);

    let palette = shell.window().global::<MaterialPalette>();
    let secondary = palette.get_secondary();
    let error = palette.get_error();

    // Unmuted: the level is drawn in the secondary color and the error color is
    // not used anywhere in the top bar.
    let unmuted = render(&mut fb, &window);
    assert!(
        !columns_with_color(&unmuted, secondary).is_empty(),
        "the volume readout must be drawn in the secondary color"
    );
    assert!(
        columns_with_color(&unmuted, error).is_empty(),
        "an unmuted top bar must not use the error color"
    );

    // Muted: the mute icon and the level readout share the error color.
    shell.set_muted(true);
    let muted = render(&mut fb, &window);
    let error_columns = columns_with_color(&muted, error);
    assert!(
        !error_columns.is_empty(),
        "the mute icon must use the error color"
    );
    let span =
        error_columns.last().expect("error column") - error_columns.first().expect("error column");
    assert!(
        span > MUTE_ICON_WIDTH,
        "the level readout must stay visible while muted: the error color only spans \
         {span} px, which is still inside the {MUTE_ICON_WIDTH} px mute icon"
    );
}
