//! Integration tests for the equalizer boot splash screen component.

use slint::ComponentHandle;
use speaker_core::ui::{Bgrx8888Pixel, BootCheckLine, Framebuffer, SplashScreen};

#[cfg(feature = "ui")]
fn splash_on_mocked_fb() -> (
    Framebuffer,
    std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    SplashScreen,
) {
    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");
    let splash = SplashScreen::new().expect("Failed to initialize SplashScreen");
    splash.show().expect("Failed to show SplashScreen");
    fb.render_frame(&window).expect("Render frame failed");
    (fb, window, splash)
}

#[cfg(feature = "ui")]
#[test]
fn splash_screen_current_check_default_and_updates() {
    let (_fb, _window, splash) = splash_on_mocked_fb();

    // The single check line starts with a running placeholder.
    let default_line = splash.get_current_check();
    assert!(default_line.running);
    assert!(!default_line.ok);

    splash.set_current_check(BootCheckLine {
        text: "CamillaDSP: running".into(),
        running: false,
        ok: true,
    });
    let line = splash.get_current_check();
    assert_eq!(line.text, "CamillaDSP: running");
    assert!(!line.running && line.ok);

    // Failed checks carry their detail text.
    splash.set_current_check(BootCheckLine {
        text: "I2S Amplifier: live faults: OTW".into(),
        running: false,
        ok: false,
    });
    let line = splash.get_current_check();
    assert!(!line.running && !line.ok);
    assert!(line.text.contains("OTW"));
}

#[cfg(feature = "ui")]
#[test]
fn splash_screen_renders_to_framebuffer() {
    let (fb, _window, _splash) = splash_on_mocked_fb();
    assert_eq!(fb.render_count(), 1);

    // Verify non-empty pixel render (theme chassis color is non-zero).
    let non_zero_pixels = fb
        .pixels()
        .iter()
        .filter(|p| **p != Bgrx8888Pixel::default())
        .count();
    assert!(
        non_zero_pixels > 0,
        "Rendered frame must contain drawn pixels"
    );
}

#[cfg(feature = "ui")]
#[test]
fn splash_screen_renders_check_line() {
    let (mut fb, window, splash) = splash_on_mocked_fb();
    assert_eq!(fb.render_count(), 1);

    splash.set_current_check(BootCheckLine {
        text: "Bluetooth Stack: 1 connected | streaming".into(),
        running: false,
        ok: true,
    });
    fb.render_frame(&window).expect("Render frame failed");
    assert_eq!(fb.render_count(), 2);
}
