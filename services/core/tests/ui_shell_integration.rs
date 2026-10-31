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

    // CD Player service enablement & tab synchronization
    assert!(!shell.is_cd_enabled());
    assert!(!shell.window().get_cd_enabled());
    shell.set_tab(3);
    assert_eq!(
        shell.current_tab(),
        0,
        "tab 3 should fall back to 0 when cd is disabled"
    );

    shell.set_cd_enabled(true);
    assert!(shell.is_cd_enabled());
    assert!(shell.window().get_cd_enabled());
    shell.set_tab(3);
    assert_eq!(
        shell.current_tab(),
        3,
        "tab 3 should be accessible when cd is enabled"
    );

    shell.set_cd_enabled(false);
    assert!(!shell.is_cd_enabled());
    assert_eq!(
        shell.current_tab(),
        0,
        "disabling cd while on tab 3 must reset to tab 0"
    );

    // Re-enable CD for general tab testing (0..=5)
    shell.set_cd_enabled(true);
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

    assert_eq!(shell.cycle_theme(), ThemePreset::Peppy);
    assert_eq!(shell.current_theme(), ThemePreset::Peppy);

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

    // Invoke tab selection callback (CD disabled falls back to 0)
    shell.window().invoke_tab_selected(3);
    assert_eq!(shell.current_tab(), 0);
    assert_eq!(shell.window().get_active_tab(), 0);

    // Invoke tab selection with CD enabled
    shell.set_cd_enabled(true);
    shell.window().invoke_tab_selected(3);
    assert_eq!(shell.current_tab(), 3);
    assert_eq!(shell.window().get_active_tab(), 3);

    // Invoke tab selection with out-of-range index (clamps to 5)
    shell.window().invoke_tab_selected(99);
    assert_eq!(shell.current_tab(), 5);
    assert_eq!(shell.window().get_active_tab(), 5);

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

#[cfg(feature = "ui")]
#[test]
fn ui_shell_now_playing_properties_and_playback_state() {
    use speaker_core::hardware::BtPlaybackState;
    use speaker_core::ui::{AppShell, Framebuffer, MainWindow};

    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    // Initial state
    assert!(!shell.is_playing());
    assert!(!shell.is_muted());
    assert_eq!(shell.window().get_progress_ratio(), 0.0);
    assert_eq!(shell.window().get_position_text(), "00:00");
    assert_eq!(shell.window().get_duration_text(), "00:00");

    // Playback state updates
    shell.set_playback_state(true, 65_000, 195_000);
    assert!(shell.is_playing());
    assert_eq!(shell.window().get_position_text(), "01:05");
    assert_eq!(shell.window().get_duration_text(), "03:15");
    let ratio = shell.window().get_progress_ratio();
    assert!((ratio - (65.0 / 195.0)).abs() < 1e-4);

    // Zero duration guard
    shell.set_playback_state(false, 0, 0);
    assert!(!shell.is_playing());
    assert_eq!(shell.window().get_progress_ratio(), 0.0);

    // Mute toggling
    shell.set_muted(true);
    assert!(shell.is_muted());
    assert!(shell.window().get_is_muted());
    shell.set_muted(false);
    assert!(!shell.is_muted());
    assert!(!shell.window().get_is_muted());

    // Playing toggle directly
    shell.set_playing(true);
    assert!(shell.is_playing());

    // Stream details
    shell.set_stream_details("AAC-LC | 44.1kHz/16-bit ➔ DSP 48k");
    assert_eq!(
        shell.window().get_stream_details(),
        "AAC-LC | 44.1kHz/16-bit ➔ DSP 48k"
    );

    // Peak levels & NaN / boundary clamping
    shell.set_peak_levels(0.85, 0.42);
    assert_eq!(shell.window().get_left_peak_ratio(), 0.85);
    assert_eq!(shell.window().get_right_peak_ratio(), 0.42);

    shell.set_peak_levels(1.5, -0.5);
    assert_eq!(shell.window().get_left_peak_ratio(), 1.0);
    assert_eq!(shell.window().get_right_peak_ratio(), 0.0);

    shell.set_peak_levels(f32::NAN, f32::INFINITY);
    assert_eq!(shell.window().get_left_peak_ratio(), 0.0);
    assert_eq!(shell.window().get_right_peak_ratio(), 1.0);

    // Cover art toggle
    assert!(!shell.window().get_has_cover_art());
    shell.set_has_cover_art(true);
    assert!(shell.window().get_has_cover_art());
    shell.set_has_cover_art(false);
    assert!(!shell.window().get_has_cover_art());

    // Apply BtPlaybackState
    let playback = BtPlaybackState {
        device_address: None,
        device_name: "Pixel Phone".into(),
        status: "playing".into(),
        title: "Telegraph Road".into(),
        artist: "Dire Straits".into(),
        album: "Love over Gold".into(),
        position_ms: 300_000,
        duration_ms: 860_000,
        player_path: None,
    };
    shell.apply_bluetooth_playback(&playback);
    assert_eq!(shell.window().get_track_title(), "Telegraph Road");
    assert_eq!(shell.window().get_track_artist(), "Dire Straits");
    assert_eq!(shell.window().get_track_album(), "Love over Gold");
    assert_eq!(shell.window().get_track_or_device(), "Pixel Phone");
    assert!(shell.is_playing());
    assert_eq!(shell.window().get_position_text(), "05:00");
    assert_eq!(shell.window().get_duration_text(), "14:20");

    // Apply demo playback
    shell.apply_demo_playback();
    assert_eq!(shell.current_tab(), 0);
    assert_eq!(shell.window().get_active_source(), "Bluetooth");
    assert_eq!(shell.window().get_track_title(), "Money for Nothing");
    assert_eq!(shell.window().get_track_artist(), "Dire Straits");
    assert_eq!(shell.window().get_track_album(), "Brothers in Arms");
    assert_eq!(shell.window().get_volume_value(), 78.0);
    assert!(shell.is_playing());
}

#[cfg(feature = "ui")]
#[test]
fn ui_shell_now_playing_callbacks() {
    use speaker_core::ui::{AppShell, Framebuffer, MainWindow};

    let fb = Framebuffer::new_mocked().0;
    let _window = fb.init_platform().expect("Failed to initialize platform");

    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    // Invoke transport callbacks on unwired shell (does not falsely claim playing)
    assert!(!shell.is_playing());
    shell.window().invoke_toggle_play_pause();
    assert!(!shell.is_playing());

    assert!(!shell.is_muted());
    shell.window().invoke_toggle_mute();
    assert!(shell.is_muted());
    shell.window().invoke_toggle_mute();
    assert!(!shell.is_muted());

    // Seeking and position text sync
    shell.set_playback_state(false, 0, 100_000);
    shell.window().invoke_seek(0.65);
    assert_eq!(shell.window().get_progress_ratio(), 0.65);
    assert_eq!(shell.window().get_position_text(), "01:05");
    shell.window().invoke_seek(1.5);
    assert_eq!(shell.window().get_progress_ratio(), 1.0);
    assert_eq!(shell.window().get_position_text(), "01:40");
    shell.window().invoke_seek(-0.2);
    assert_eq!(shell.window().get_progress_ratio(), 0.0);
    assert_eq!(shell.window().get_position_text(), "00:00");
    shell.window().invoke_seek(f32::NAN);
    assert_eq!(shell.window().get_progress_ratio(), 0.0);
    assert_eq!(shell.window().get_position_text(), "00:00");

    // Wire interactive bluetooth transport
    let (bt, _) = speaker_core::hardware::BluetoothDevice::new_mocked();
    shell.wire_bluetooth_transport(bt.clone());

    // Initial mock state is playing
    assert!(bt.status().unwrap().playback.unwrap().is_playing());
    shell.window().invoke_toggle_play_pause();
    assert!(!bt.status().unwrap().playback.unwrap().is_playing());
    assert!(!shell.is_playing());
    shell.window().invoke_toggle_play_pause();
    assert!(bt.status().unwrap().playback.unwrap().is_playing());
    assert!(shell.is_playing());

    shell.window().invoke_next_track();
    assert_eq!(shell.window().get_track_title(), "Money for Nothing");
    shell.window().invoke_previous_track();
    assert_eq!(shell.window().get_track_title(), "Money for Nothing");
    shell.window().invoke_seek(0.5);
    assert_eq!(bt.status().unwrap().playback.unwrap().position_ms, 115_000);

    // Callbacks without panicking
    shell.window().invoke_stream_pill_clicked();
}

#[cfg(feature = "ui")]
#[test]
fn ui_shell_demo_playback_renders_to_framebuffer() {
    use slint::ComponentHandle;
    use speaker_core::ui::{AppShell, Bgrx8888Pixel, Framebuffer, MainWindow};

    let mut fb = Framebuffer::new_mocked().0;
    let window = fb.init_platform().expect("Failed to initialize platform");
    let main_window = MainWindow::new().expect("Failed to initialize MainWindow");
    let shell = AppShell::new(main_window);

    shell.apply_demo_playback();
    shell.window().show().expect("Failed to show MainWindow");
    let rendered = fb.render_frame(&window).expect("Render frame failed");
    assert!(rendered);
    assert_eq!(fb.render_count(), 1);

    let non_zero_pixels = fb
        .pixels()
        .iter()
        .filter(|p| **p != Bgrx8888Pixel::default())
        .count();
    assert!(
        non_zero_pixels > 0,
        "Rendered demo playback frame must contain drawn pixels across the 800x480 canvas"
    );
}
