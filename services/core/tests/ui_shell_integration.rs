//! Integration tests for the UI Shell and Unified Global Theme System (Sections 3 & 3.1).

#[cfg(feature = "ui")]
#[test]
fn ui_shell_defaults_and_property_updates() {
    use speaker_core::ui::{AppShell, Framebuffer, MainWindow, ThemePreset};

    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    // Initial defaults
    assert_eq!(shell.current_tab(), 0);
    assert_eq!(shell.current_theme(), ThemePreset::McIntoshBlue);
    assert_eq!(shell.window().get_active_tab(), 0);
    assert_eq!(shell.window().get_active_source(), "Bluetooth");
    assert_eq!(shell.window().get_stream_codec(), "aptX-HD 48kHz");
    assert_eq!(shell.window().get_volume_value(), 75.0);

    // CD Player service enablement (disabled by default)
    assert!(!shell.is_cd_enabled());
    assert!(!shell.window().get_cd_enabled());
    shell.set_cd_enabled(true);
    assert!(shell.is_cd_enabled());
    assert!(shell.window().get_cd_enabled());
    shell.set_cd_enabled(false);
    assert!(!shell.is_cd_enabled());

    // Tab navigation updates
    for tab in 0..=5 {
        shell.set_tab(tab);
        assert_eq!(shell.current_tab(), tab);
        assert_eq!(shell.window().get_active_tab(), tab);
    }

    // Boundary clamping for tab
    shell.set_tab(-5);
    assert_eq!(shell.current_tab(), 0);
    shell.set_tab(99);
    assert_eq!(shell.current_tab(), 5);

    // Status bar updates
    shell.set_source("CD Player");
    assert_eq!(shell.window().get_active_source(), "CD Player");

    shell.set_track_or_device("Dire Straits - Sultans of Swing");
    assert_eq!(
        shell.window().get_track_or_device(),
        "Dire Straits - Sultans of Swing"
    );

    shell.set_stream_codec("CD-DA 44.1kHz");
    assert_eq!(shell.window().get_stream_codec(), "CD-DA 44.1kHz");

    // Track metadata
    shell.set_track_metadata("Money for Nothing", "Dire Straits", "Brothers in Arms");
    assert_eq!(shell.window().get_track_title(), "Money for Nothing");
    assert_eq!(shell.window().get_track_artist(), "Dire Straits");
    assert_eq!(shell.window().get_track_album(), "Brothers in Arms");

    // Volume updates & NaN guards
    shell.set_volume(82.5);
    assert_eq!(shell.window().get_volume_value(), 82.5);
    shell.set_volume(150.0);
    assert_eq!(shell.window().get_volume_value(), 100.0);
    shell.set_volume(-10.0);
    assert_eq!(shell.window().get_volume_value(), 0.0);
    shell.set_volume(f32::NAN);
    assert_eq!(shell.window().get_volume_value(), 0.0);
}

#[cfg(feature = "ui")]
#[test]
fn ui_shell_theme_cycling_and_hot_swapping() {
    use speaker_core::ui::{AppShell, Framebuffer, MainWindow, ThemePreset};

    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    assert_eq!(shell.current_theme(), ThemePreset::McIntoshBlue);

    // Test cyclic theme switching
    assert_eq!(shell.cycle_theme(), ThemePreset::MarantzAmber);
    assert_eq!(shell.current_theme(), ThemePreset::MarantzAmber);

    assert_eq!(shell.cycle_theme(), ThemePreset::CyberpunkNeon);
    assert_eq!(shell.current_theme(), ThemePreset::CyberpunkNeon);

    assert_eq!(shell.cycle_theme(), ThemePreset::StudioDark);
    assert_eq!(shell.current_theme(), ThemePreset::StudioDark);

    assert_eq!(shell.cycle_theme(), ThemePreset::McIntoshBlue);
    assert_eq!(shell.current_theme(), ThemePreset::McIntoshBlue);

    // Explicit application
    shell.apply_theme(ThemePreset::CyberpunkNeon);
    assert_eq!(shell.current_theme(), ThemePreset::CyberpunkNeon);
}

#[cfg(feature = "ui")]
#[test]
fn ui_shell_callbacks_and_debug_formatting() {
    use speaker_core::ui::{AppShell, Framebuffer, MainWindow, ThemePreset};

    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    // Verify debug formatting
    let debug_str = format!("{shell:?}");
    assert!(debug_str.contains("AppShell"));
    assert!(debug_str.contains("active_theme"));
    assert!(debug_str.contains("active_tab"));

    // Invoke callbacks through Slint window bindings
    shell.window().invoke_source_clicked();
    shell.window().invoke_night_mode_clicked();
    shell.window().invoke_volume_changed(50.0);
    shell.window().invoke_toggle_play_pause();

    // Invoke tab selection callback
    shell.window().invoke_tab_selected(3);
    assert_eq!(shell.current_tab(), 3);
    assert_eq!(shell.window().get_active_tab(), 3);

    // Invoke theme selection callback
    shell.window().invoke_select_theme(1);
    assert_eq!(shell.current_theme(), ThemePreset::MarantzAmber);

    // Invoke cycle theme callback
    shell.window().invoke_cycle_theme();
    assert_eq!(shell.current_theme(), ThemePreset::CyberpunkNeon);
}

#[cfg(feature = "ui")]
#[test]
fn ui_shell_renders_to_framebuffer() {
    use slint::ComponentHandle;
    use speaker_core::ui::{AppShell, Bgrx8888Pixel, Framebuffer, MainWindow};

    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");
    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    shell.window().show().expect("Failed to show MainWindow");
    let rendered = fb.render_frame(&window).expect("Render frame failed");
    assert!(rendered);
    assert_eq!(fb.render_count(), 1);

    // Verify non-empty pixel render (dark chassis background drawn across 800x480)
    let non_zero_pixels = fb
        .pixels()
        .iter()
        .filter(|p| **p != Bgrx8888Pixel::default())
        .count();
    assert!(
        non_zero_pixels > 0,
        "Rendered frame must contain drawn pixels across the 800x480 canvas"
    );
}
