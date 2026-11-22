//! Integration tests for the `CamillaDSP` 5.0 Radial FFT & 24-band ERB visualizer:
//! - 24-band Glasberg–Moore ERB mapping & per-bin +3.0 dB/octave pink-noise tilt
//! - Frame-rate-independent spectrum ballistics & peak-hold latch/decay
//! - `VisualizerController` mock streaming lifecycle
//! - Slint `VisualizerOverlay` offscreen framebuffer rendering & navigation

use std::sync::Arc;
use std::time::{Duration, Instant};

use screen_manager::settings_catalog::{SettingId, apply_toggle};
use screen_manager::ui::{AppShell, Framebuffer, MainWindow, VisualizerUiState};
use screen_manager::visualizer::dsp_math::{
    ERB_BAND_COUNT, ErbBandMapper, SpectrumBallistics, erb_to_hz, hz_to_erb, normalize_dbfs,
    smoothing_coeff,
};
use screen_manager::visualizer::{VisualizerController, VisualizerMode, VisualizerOutput};
use slint::Model;
use speaker_bluetooth::BtPlaybackState;
use speaker_dsp::DspDevice;

fn build_log_grid(min_hz: f64, max_hz: f64, n_bins: usize) -> Arc<[f32]> {
    let lmin = min_hz.ln();
    let lmax = max_hz.ln();
    let denom = (n_bins - 1) as f64;
    (0..n_bins)
        .map(|i| (lmin + (lmax - lmin) * (i as f64 / denom)).exp() as f32)
        .collect::<Vec<_>>()
        .into()
}

#[test]
fn erb_mapper_zero_empty_bands_and_pink_noise_tilt() {
    let mut mapper = ErbBandMapper::new();
    let centers = mapper.center_frequencies_hz();
    assert_eq!(centers.len(), ERB_BAND_COUNT);
    assert!(centers[0] >= 30.0 && centers[ERB_BAND_COUNT - 1] <= 18_000.0);

    for &f in &[30.0, 100.0, 1000.0, 10_000.0, 18_000.0] {
        let rt = erb_to_hz(hz_to_erb(f));
        assert!(
            (rt - f).abs() < 1e-6,
            "ERB round-trip drift at {f} Hz: {rt}"
        );
    }

    let freqs = build_log_grid(30.0, 18_000.0, 128);
    let mags: Vec<f32> = freqs
        .iter()
        .map(|&f| -24.0 - 3.0 * (f / 1000.0).log2())
        .collect();

    let bands = mapper.map_to_bands(&freqs, &mags);
    let expected = normalize_dbfs(-24.0);
    for (i, &val) in bands.iter().enumerate() {
        assert!(
            (val - expected).abs() < 0.005,
            "band {i} should be calibrated flat at {expected}, got {val}"
        );
    }

    let empty_freqs: Arc<[f32]> = Arc::from([]);
    assert_eq!(
        mapper.map_to_bands(&empty_freqs, &[]),
        [0.0; ERB_BAND_COUNT]
    );
    assert_eq!(normalize_dbfs(f32::INFINITY), 0.0);
}

#[test]
fn spectrum_ballistics_frame_rate_independent_and_peak_hold() {
    let mut b30 = SpectrumBallistics::new();
    let mut b60 = SpectrumBallistics::new();
    let target = [0.8_f32; ERB_BAND_COUNT];

    let dt30 = Duration::from_secs_f64(1.0 / 30.0);
    let dt60 = Duration::from_secs_f64(1.0 / 60.0);

    let (bars30, _) = b30.step(&target, dt30);
    let _ = b60.step(&target, dt60);
    let (bars60, _) = b60.step(&target, dt60);
    assert!((bars30[0] - bars60[0]).abs() < 1e-4);

    assert!((smoothing_coeff(dt30, 0.015) - 0.892).abs() < 0.01);
    assert!((smoothing_coeff(dt60, 0.015) - 0.671).abs() < 0.01);
    assert!((smoothing_coeff(dt30, 0.180) - 0.169).abs() < 0.01);

    let mut ballistics = SpectrumBallistics::new();
    let (bars, peaks) = ballistics.step(&[1.0; ERB_BAND_COUNT], Duration::from_millis(100));
    let latched_peak = peaks[0];
    assert!(latched_peak >= bars[0]);

    let zero = [0.0; ERB_BAND_COUNT];
    let (_, peaks_during_hold) = ballistics.step(&zero, Duration::from_millis(300));
    assert!((peaks_during_hold[0] - latched_peak).abs() < 1e-5);

    let (_, peaks_after_hold) = ballistics.step(&zero, Duration::from_millis(400));
    assert!(peaks_after_hold[0] < latched_peak);
}

#[test]
fn settings_catalog_spectrum_style_and_fps_toggles() {
    use screen_manager::config::{CoreConfig, VisualizerAutoStartSecs};
    use screen_manager::settings_catalog::{SettingTier, apply_step, find_setting, row_state};

    let mut cfg = CoreConfig::default();
    assert!(cfg.spectrum_led_mask);
    assert!(!cfg.visualizer_force_60fps);
    assert!(cfg.visualizer_auto_start);
    assert_eq!(cfg.visualizer_auto_start_secs.value(), 5);
    assert_eq!(cfg.default_visualizer, VisualizerMode::RadialFft);

    assert!(apply_toggle(&mut cfg, SettingId::SpectrumLedMask, false));
    assert!(!cfg.spectrum_led_mask);
    assert!(apply_toggle(&mut cfg, SettingId::VisualizerFps, true));
    assert!(cfg.visualizer_force_60fps);

    assert_eq!(
        find_setting(SettingId::VisualizerAutoStart).tier,
        SettingTier::Visualizer
    );
    assert_eq!(
        find_setting(SettingId::VisualizerAutoStartSecs).tier,
        SettingTier::Visualizer
    );
    assert_eq!(
        find_setting(SettingId::DefaultVisualizer).tier,
        SettingTier::Visualizer
    );
    assert_eq!(
        find_setting(SettingId::SpectrumLedMask).tier,
        SettingTier::Visualizer
    );

    assert!(apply_toggle(
        &mut cfg,
        SettingId::VisualizerAutoStart,
        false
    ));
    assert!(!cfg.visualizer_auto_start);
    assert_eq!(
        row_state(&cfg, find_setting(SettingId::DefaultVisualizer)).value_text,
        "Radial FFT"
    );
    assert!(apply_step(&mut cfg, SettingId::DefaultVisualizer, 1));
    assert_eq!(cfg.default_visualizer, VisualizerMode::LinearSpectrum);
    assert_eq!(
        row_state(&cfg, find_setting(SettingId::DefaultVisualizer)).value_text,
        "ERB Spectrum"
    );
    assert!(apply_step(&mut cfg, SettingId::DefaultVisualizer, -1));
    assert_eq!(cfg.default_visualizer, VisualizerMode::RadialFft);

    assert!(apply_step(&mut cfg, SettingId::VisualizerAutoStartSecs, 1));
    assert_eq!(cfg.visualizer_auto_start_secs.value(), 6);
    assert_eq!(
        row_state(&cfg, find_setting(SettingId::VisualizerAutoStartSecs)).value_text,
        "6 s"
    );
    cfg.visualizer_auto_start_secs = VisualizerAutoStartSecs::new(1);
    assert!(apply_step(&mut cfg, SettingId::VisualizerAutoStartSecs, -1));
    assert_eq!(cfg.visualizer_auto_start_secs.value(), 1);
    cfg.visualizer_auto_start_secs = VisualizerAutoStartSecs::new(60);
    assert!(apply_step(&mut cfg, SettingId::VisualizerAutoStartSecs, 1));
    assert_eq!(cfg.visualizer_auto_start_secs.value(), 60);

    let _ = VisualizerAutoStartSecs::try_from(0).unwrap_err();
    let _ = VisualizerAutoStartSecs::try_from(61).unwrap_err();
    assert_eq!(VisualizerAutoStartSecs::try_from(15).unwrap().value(), 15);
    assert_eq!(VisualizerMode::RadialFft.slug(), "radial_fft");
    assert_eq!(VisualizerMode::LinearSpectrum.slug(), "linear_spectrum");
    assert_eq!(
        VisualizerMode::parse_slug("radial_fft"),
        Ok(VisualizerMode::RadialFft)
    );
    assert_eq!(
        VisualizerMode::parse_slug("linear_spectrum"),
        Ok(VisualizerMode::LinearSpectrum)
    );
    let _ = VisualizerMode::parse_slug("invalid").unwrap_err();
}

#[test]
fn visualizer_default_mode_and_autostart_config_roundtrip() {
    use screen_manager::config::{CoreConfig, VisualizerAutoStartSecs};

    let fb = Framebuffer::new_mocked().0;
    let _fb_window = fb.init_platform().expect("init_platform");
    let window = MainWindow::new().expect("MainWindow builds");
    let shell = AppShell::new(window);
    shell.wire_visualizer(DspDevice::new_mocked().0);

    let cfg = CoreConfig {
        visualizer_auto_start: false,
        visualizer_auto_start_secs: VisualizerAutoStartSecs::new(12),
        default_visualizer: VisualizerMode::LinearSpectrum,
        ..CoreConfig::default()
    };
    shell.apply_config(&cfg);

    let roundtrip = shell.current_config();
    assert!(!roundtrip.visualizer_auto_start);
    assert_eq!(roundtrip.visualizer_auto_start_secs.value(), 12);
    assert_eq!(roundtrip.default_visualizer, VisualizerMode::LinearSpectrum);

    // Opening the visualizer uses the configured default mode (LinearSpectrum = 1).
    shell.open_visualizer();
    assert!(shell.is_visualizer_active());
    assert_eq!(shell.window().get_visualizer_mode(), 1);
    shell.close_visualizer();

    // Auto-start playback transition with auto-start disabled does not start timer/open.
    shell.apply_bluetooth_playback(&BtPlaybackState {
        status: "playing".into(),
        ..BtPlaybackState::default()
    });
    assert!(!shell.is_visualizer_active());

    // Enabling auto-start with a 1s delay opens the visualizer when playback transitions to playing.
    shell.apply_bluetooth_playback(&BtPlaybackState {
        status: "paused".into(),
        ..BtPlaybackState::default()
    });
    shell.apply_config(&CoreConfig {
        visualizer_auto_start: true,
        visualizer_auto_start_secs: VisualizerAutoStartSecs::new(1),
        default_visualizer: VisualizerMode::LinearSpectrum,
        ..CoreConfig::default()
    });
    shell.apply_bluetooth_playback(&BtPlaybackState {
        status: "playing".into(),
        ..BtPlaybackState::default()
    });
    assert!(!shell.is_visualizer_active());
    std::thread::sleep(Duration::from_millis(1050));
    slint::platform::update_timers_and_animations();
    assert!(shell.is_visualizer_active());
    assert_eq!(shell.window().get_visualizer_mode(), 1);

    shell.shutdown_visualizer();
}

fn poll_until_positive(ctrl: &mut VisualizerController, timeout: Duration) -> VisualizerOutput {
    let deadline = Instant::now() + timeout;
    loop {
        let out = ctrl.tick(Instant::now());
        if out.spectrum_bars.iter().any(|&v| v > 0.0) {
            return out;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for positive spectrum frame"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn visualizer_controller_lifecycle_and_mock_streaming() {
    let (dsp, _) = DspDevice::new_mocked();
    let mut ctrl = VisualizerController::new(dsp);
    assert_eq!(ctrl.active_mode(), None);

    ctrl.set_active_mode(Some(VisualizerMode::RadialFft), false);
    let radial_out = poll_until_positive(&mut ctrl, Duration::from_secs(2));
    assert!(radial_out.spectrum_bars.iter().any(|&v| v > 0.0));

    // Switching to LinearSpectrum keeps the stream active seamlessly.
    ctrl.set_active_mode(Some(VisualizerMode::LinearSpectrum), false);
    let linear_out = ctrl.tick(Instant::now() + Duration::from_millis(30));
    assert!(linear_out.spectrum_bars.iter().any(|&v| v > 0.0));

    // Enabling 60 FPS restarts stream at 60 Hz cleanly.
    ctrl.set_active_mode(Some(VisualizerMode::RadialFft), true);
    let fast_out = poll_until_positive(&mut ctrl, Duration::from_secs(2));
    assert!(fast_out.spectrum_bars.iter().any(|&v| v > 0.0));

    ctrl.set_active_mode(None, false);
    assert_eq!(ctrl.active_mode(), None);
    ctrl.shutdown();
}

#[test]
fn visualizer_ui_framebuffer_rendering_and_navigation() {
    let mut fb = Framebuffer::new_mocked().0;
    let fb_window = fb.init_platform().expect("init_platform");
    let window = MainWindow::new().expect("MainWindow builds");
    let shell = AppShell::new(window);
    shell.wire_visualizer(DspDevice::new_mocked().0);

    assert!(!shell.is_visualizer_active());
    shell.open_visualizer();
    assert!(shell.is_visualizer_active());
    assert_eq!(shell.window().get_visualizer_mode(), 0);

    // Render Radial FFT mode (mode 0)
    let _ = fb.render_frame(&fb_window);

    // Switch to Linear Spectrum mode (mode 1) via swipe callback
    shell.window().invoke_next_visualizer();
    assert_eq!(shell.window().get_visualizer_mode(), 1);
    let _ = fb.render_frame(&fb_window);

    // Switch back via prev_visualizer
    shell.window().invoke_prev_visualizer();
    assert_eq!(shell.window().get_visualizer_mode(), 0);

    // Auto-close on playback stop transition
    shell.apply_bluetooth_playback(&BtPlaybackState {
        status: "playing".into(),
        ..BtPlaybackState::default()
    });
    assert!(shell.is_visualizer_active());
    shell.apply_bluetooth_playback(&BtPlaybackState {
        status: "paused".into(),
        ..BtPlaybackState::default()
    });
    assert!(!shell.is_visualizer_active());

    // Opening from Settings (tab 5) preserves active_tab=5 when closed or when BT devices opens
    shell.set_tab(5);
    shell.open_visualizer();
    assert!(shell.is_visualizer_active());
    shell.window().invoke_open_bt_devices();
    assert!(!shell.is_visualizer_active());
    assert_eq!(shell.current_tab(), 5);

    shell.shutdown_visualizer();
}

#[test]
fn visualizer_bridge_updates_24_band_vec_model() {
    let fb = Framebuffer::new_mocked().0;
    let _fb_window = fb.init_platform().expect("init_platform");
    let window = MainWindow::new().expect("MainWindow builds");
    let bridge = VisualizerUiState::new();
    bridge.bind_to_window(&window);

    let out = VisualizerOutput {
        spectrum_bars: [0.6; ERB_BAND_COUNT],
        spectrum_peaks: [0.8; ERB_BAND_COUNT],
    };
    bridge.apply_output(&out);

    let bands = window.get_viz_spectrum_bands();
    assert_eq!(bands.row_count(), ERB_BAND_COUNT);
    let first = bands.row_data(0).unwrap();
    assert!((first.value - 0.6).abs() < 1e-5);
    assert!((first.peak - 0.8).abs() < 1e-5);

    // Sub-pixel change (< 0.003) is skipped to avoid unnecessary VecModel dirtying.
    let jitter = VisualizerOutput {
        spectrum_bars: [0.601; ERB_BAND_COUNT],
        spectrum_peaks: [0.801; ERB_BAND_COUNT],
    };
    bridge.apply_output(&jitter);
    assert!((bands.row_data(0).unwrap().value - 0.6).abs() < 1e-5);

    // Radial FFT mode renders a 228x228 SharedPixelBuffer image with non-zero pixels.
    bridge.apply_for_mode(&window, VisualizerMode::RadialFft, &out);
    let radial_img = window.get_viz_radial_image();
    assert_eq!(radial_img.size().width, 228);
    assert_eq!(radial_img.size().height, 228);
    let rgba = radial_img.to_rgba8_premultiplied().expect("rgba8 buffer");
    assert!(rgba.as_bytes().iter().any(|&b| b != 0));
}
