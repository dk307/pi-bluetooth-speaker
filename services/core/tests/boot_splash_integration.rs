//! Integration tests for the Clean Startup Splash Screen component.

#[cfg(feature = "ui")]
#[test]
fn splash_screen_defaults_and_property_updates() {
    use speaker_core::ui::{Framebuffer, SplashScreen};

    let fb = Framebuffer::new_mocked();
    let _window = fb.init_platform().expect("Failed to initialize platform");

    let splash = SplashScreen::new().expect("Failed to initialize SplashScreen");

    // Verify initial default properties
    assert_eq!(splash.get_progress(), 100.0);
    assert_eq!(splash.get_status_text(), "System Ready");
    assert!(splash.get_amp_status().contains("MA12070P"));
    assert!(splash.get_dsp_status().contains("CamillaDSP"));
    assert!(splash.get_bt_status().contains("BlueZ 5"));
    assert!(splash.get_display_status().contains("800x480"));
    assert!(splash.get_sensor_status().contains("DS18B20"));

    // Verify dynamic property mutations
    splash.set_progress(45.0);
    assert_eq!(splash.get_progress(), 45.0);

    splash.set_status_text("Initializing Active Crossover Filters...".into());
    assert_eq!(
        splash.get_status_text(),
        "Initializing Active Crossover Filters..."
    );

    splash.set_amp_status("InnoMaker Merus MA12070P (Fault Cleared)".into());
    assert_eq!(
        splash.get_amp_status(),
        "InnoMaker Merus MA12070P (Fault Cleared)"
    );

    splash.set_dsp_status("CamillaDSP 48kHz / 32-bit LE (Locked)".into());
    assert_eq!(
        splash.get_dsp_status(),
        "CamillaDSP 48kHz / 32-bit LE (Locked)"
    );

    splash.set_bt_status("Pixel 10 Pro XL Connected via aptX-HD".into());
    assert_eq!(
        splash.get_bt_status(),
        "Pixel 10 Pro XL Connected via aptX-HD"
    );

    splash.set_sensor_status("DS18B20 41.2°C | BH1750 145.0 Lux".into());
    assert_eq!(
        splash.get_sensor_status(),
        "DS18B20 41.2°C | BH1750 145.0 Lux"
    );

    // Verify NaN and boundary progress values
    splash.set_progress(f32::NAN);
    assert!(splash.get_progress().is_nan());
    splash.set_progress(-10.0);
    assert_eq!(splash.get_progress(), -10.0);
    splash.set_progress(150.0);
    assert_eq!(splash.get_progress(), 150.0);
}

#[cfg(feature = "ui")]
#[test]
fn splash_screen_renders_to_framebuffer() {
    use slint::ComponentHandle;
    use speaker_core::ui::{Bgrx8888Pixel, Framebuffer, SplashScreen};

    let mut fb = Framebuffer::new_mocked();
    let window = fb.init_platform().expect("Failed to initialize platform");
    let splash = SplashScreen::new().expect("Failed to initialize SplashScreen");

    splash.show().expect("Failed to show SplashScreen");
    let rendered = fb.render_frame(&window).expect("Render frame failed");
    assert!(rendered);
    assert_eq!(fb.render_count(), 1);

    // Verify non-empty pixel render (background color #090d12 is non-zero)
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
