//! Integration tests for the `CamillaDSP` 5.0 real-time visualizer subsystem:
//! - 24-band Glasberg–Moore ERB mapping & +3.0 dB/octave pink-noise tilt
//! - Frame-rate-independent spectrum ballistics & peak-hold latch/decay
//! - `PeppyMeter` 800×480 `meters.txt` parser & embedded asset completeness (all 25 skins)
//! - `VisualizerController` mock streaming lifecycle
//! - Slint `VisualizerOverlay` offscreen framebuffer rendering & navigation

use std::sync::Arc;
use std::time::{Duration, Instant};

use screen_manager::config::{VU_SKIN_NAMES, VuSkinName, VuZeroDbfs};
use screen_manager::settings_catalog::{
    SettingId, apply_step, apply_toggle, find_setting, row_state,
};
use screen_manager::ui::{AppShell, Framebuffer, MainWindow, VisualizerUiState};
use screen_manager::visualizer::dsp_math::{
    DEFAULT_ZERO_VU_DBFS, ERB_BAND_COUNT, ErbBandMapper, SpectrumBallistics, dbfs_to_vu_ratio,
    erb_to_hz, hz_to_erb, normalize_dbfs, smoothing_coeff,
};
use screen_manager::visualizer::skins::{
    LinearDirection, SkinGeometry, all_skins, embedded_skin_bytes, find_skin,
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

    // Verify ERB round-trip accuracy.
    for &f in &[30.0, 100.0, 1000.0, 10_000.0, 18_000.0] {
        let rt = erb_to_hz(hz_to_erb(f));
        assert!(
            (rt - f).abs() < 1e-6,
            "ERB round-trip drift at {f} Hz: {rt}"
        );
    }

    // Simulate a -3 dB/octave pink-noise spectrum at -24 dBFS @ 1 kHz.
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

    // Empty / stopped frames return all zeros.
    let empty_freqs: Arc<[f32]> = Arc::from([]);
    assert_eq!(
        mapper.map_to_bands(&empty_freqs, &[]),
        [0.0; ERB_BAND_COUNT]
    );
}

#[test]
fn spectrum_ballistics_frame_rate_independent_and_peak_hold() {
    // 2 steps at 30 Hz (33.33 ms) should match 4 steps at 60 Hz (16.67 ms).
    let mut b30 = SpectrumBallistics::new();
    let mut b60 = SpectrumBallistics::new();
    let target = [0.8_f32; ERB_BAND_COUNT];

    let dt30 = Duration::from_secs_f64(1.0 / 30.0);
    let dt60 = Duration::from_secs_f64(1.0 / 60.0);

    let (bars30, _) = b30.step(&target, dt30);
    let _ = b60.step(&target, dt60);
    let (bars60, _) = b60.step(&target, dt60);
    assert!((bars30[0] - bars60[0]).abs() < 1e-4);

    // Verify smoothing_coeff matches spec §3.1 table values (15 ms attack, 180 ms decay).
    assert!((smoothing_coeff(dt30, 0.015) - 0.892).abs() < 0.01);
    assert!((smoothing_coeff(dt60, 0.015) - 0.671).abs() < 0.01);
    assert!((smoothing_coeff(dt30, 0.180) - 0.169).abs() < 0.01);

    // Peak hold latches for 500 ms then decays linearly.
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
fn vu_calibration_and_nan_safety() {
    let zero_vu = dbfs_to_vu_ratio(DEFAULT_ZERO_VU_DBFS, DEFAULT_ZERO_VU_DBFS);
    assert!(
        (zero_vu - 0.686).abs() < 0.01,
        "0 VU at ~68.6%, got {zero_vu}"
    );
    assert_eq!(dbfs_to_vu_ratio(-60.0, DEFAULT_ZERO_VU_DBFS), 0.0);
    assert_eq!(dbfs_to_vu_ratio(0.0, DEFAULT_ZERO_VU_DBFS), 1.0);
    assert_eq!(dbfs_to_vu_ratio(f32::NAN, DEFAULT_ZERO_VU_DBFS), 0.0);
    assert_eq!(normalize_dbfs(f32::INFINITY), 0.0);
}

#[test]
fn meters_txt_parses_all_25_skins_and_embedded_assets_exist() {
    let skins = all_skins();
    assert_eq!(skins.len(), 25, "expected 25 skins in meters.txt");
    assert_eq!(VU_SKIN_NAMES.len(), 25);

    let circular_count = skins.iter().filter(|s| s.is_circular()).count();
    let linear_count = skins.iter().filter(|s| !s.is_circular()).count();
    assert_eq!(circular_count, 17);
    assert_eq!(linear_count, 8);

    for name in VU_SKIN_NAMES {
        let skin = find_skin(&skins, name).expect("skin must exist");
        assert_eq!(skin.name, name);
        assert!(
            embedded_skin_bytes(&skin.bgr_filename).is_some(),
            "missing bgr bytes for {}",
            skin.bgr_filename
        );
        assert!(
            embedded_skin_bytes(&skin.indicator_filename).is_some(),
            "missing indicator bytes for {}",
            skin.indicator_filename
        );
        if let Some(fgr) = &skin.fgr_filename {
            assert!(
                embedded_skin_bytes(fgr).is_some(),
                "missing fgr bytes for {fgr}"
            );
        }
    }

    // Verify specific geometries: mono off-screen pivot (`blue`), custom angles (`vertical-circular`), single linear (`chillout`).
    let blue = find_skin(&skins, "blue").unwrap();
    assert_eq!(blue.channels, 1);
    let (l_deg, r_deg) = blue.circular_angles(0.0, 1.0);
    assert_eq!(l_deg, r_deg);
    assert_eq!(l_deg, -45.0);

    let vc = find_skin(&skins, "vertical-circular").unwrap();
    let (left_angle, right_angle) = vc.circular_angles(0.0, 0.0);
    assert_eq!(left_angle, 120.0);
    assert_eq!(right_angle, -120.0);

    let relax = find_skin(&skins, "relax").unwrap();
    assert!(matches!(
        relax.geometry,
        SkinGeometry::Linear(ref l) if l.direction == LinearDirection::CenterEdges && l.flip_left_x
    ));

    let chillout = find_skin(&skins, "chillout").unwrap();
    assert!(matches!(
        chillout.geometry,
        SkinGeometry::Linear(ref l) if l.single_indicator
    ));
}

#[test]
fn settings_catalog_visualizer_entries_and_config_roundtrip() {
    let mut cfg = screen_manager::config::CoreConfig::default();
    assert_eq!(cfg.vu_skin.as_str(), "vintage");
    assert_eq!(cfg.vu_zero_dbfs.value(), -12);
    assert!(cfg.spectrum_led_mask);

    assert!(apply_step(&mut cfg, SettingId::VuSkin, 1));
    assert_eq!(cfg.vu_skin.as_str(), "dash");
    let skin_state = row_state(&cfg, find_setting(SettingId::VuSkin));
    assert_eq!(skin_state.value_text, "dash");

    assert!(apply_step(&mut cfg, SettingId::VuZeroDbfs, -1));
    assert_eq!(cfg.vu_zero_dbfs.value(), -13);

    assert!(apply_toggle(&mut cfg, SettingId::SpectrumLedMask, false));
    assert!(!cfg.spectrum_led_mask);

    let _ = VuSkinName::try_from("nonexistent").unwrap_err();
    let _ = VuZeroDbfs::try_from(-99).unwrap_err();
}

#[test]
fn visualizer_controller_lifecycle_and_mock_streaming() {
    let (dsp, _) = DspDevice::new_mocked();
    let mut ctrl = VisualizerController::new(dsp, DEFAULT_ZERO_VU_DBFS);
    assert_eq!(ctrl.active_mode(), None);

    ctrl.set_active_mode(Some(VisualizerMode::VuMeter));
    std::thread::sleep(Duration::from_millis(60));
    let vu_out = ctrl.tick(Instant::now());
    assert!(vu_out.left_vu_ratio > 0.0 && vu_out.right_vu_ratio > 0.0);

    ctrl.set_active_mode(Some(VisualizerMode::Spectrum));
    std::thread::sleep(Duration::from_millis(60));
    let spec_out = ctrl.tick(Instant::now() + Duration::from_millis(30));
    assert!(spec_out.spectrum_bars.iter().any(|&v| v > 0.0));

    ctrl.set_active_mode(None);
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

    // Render circular VU mode (`vintage`)
    let _ = fb.render_frame(&fb_window);

    // Switch to Spectrum mode via swipe callback
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

    // Test linear skin (`bar`) and mono off-screen pivot skin (`blue`)
    let mut cfg = shell.current_config();
    cfg.vu_skin = VuSkinName::try_from("bar").unwrap();
    shell.apply_config(&cfg);
    shell.open_visualizer();
    let _ = fb.render_frame(&fb_window);
    shell.close_visualizer();

    cfg.vu_skin = VuSkinName::try_from("blue").unwrap();
    shell.apply_config(&cfg);
    shell.open_visualizer();
    let _ = fb.render_frame(&fb_window);
    shell.close_visualizer();

    // Opening from Settings (tab 5) returns to Now Playing (tab 0) on close.
    shell.set_tab(5);
    shell.open_visualizer();
    assert!(shell.is_visualizer_active());
    shell.window().invoke_open_bt_devices();
    assert!(!shell.is_visualizer_active());

    shell.shutdown_visualizer();
}

#[test]
fn visualizer_bridge_renders_all_linear_directions_and_spectrum() {
    let fb = Framebuffer::new_mocked().0;
    let _fb_window = fb.init_platform().expect("init_platform");
    let window = MainWindow::new().expect("MainWindow builds");
    let bridge = VisualizerUiState::new();
    bridge.bind_to_window(&window);

    let half_out = VisualizerOutput {
        left_vu_ratio: 0.5,
        right_vu_ratio: 0.75,
        spectrum_bars: [0.6; ERB_BAND_COUNT],
        spectrum_peaks: [0.8; ERB_BAND_COUNT],
    };

    // Exercise every linear direction + single sliding cursor + circular skin
    for skin_name in [
        "vintage",
        "bar",
        "vertical-linear",
        "steam-punk",
        "relax",
        "fantasy",
        "chillout",
    ] {
        bridge.ensure_skin_loaded(&window, skin_name);
        bridge.apply_output(&window, VisualizerMode::VuMeter, &half_out);
    }

    // Verify vertical-linear bottom-top grows upward from bottom
    bridge.ensure_skin_loaded(&window, "vertical-linear");
    bridge.apply_output(&window, VisualizerMode::VuMeter, &half_out);
    assert!(window.get_viz_linear_left_h() > 0.0);
    assert!(window.get_viz_linear_left_img_y() < 0.0);

    // Verify chillout sliding cursor moves x without clipping width
    bridge.ensure_skin_loaded(&window, "chillout");
    bridge.apply_output(&window, VisualizerMode::VuMeter, &half_out);
    assert!(window.get_viz_linear_left_x() > 38.0);
    assert_eq!(window.get_viz_linear_left_w(), 39.0);

    // Verify spectrum VecModel update
    bridge.apply_output(&window, VisualizerMode::Spectrum, &half_out);
    let bands = window.get_viz_spectrum_bands();
    assert_eq!(bands.row_count(), ERB_BAND_COUNT);
    let first = bands.row_data(0).unwrap();
    assert!((first.value - 0.6).abs() < 1e-5);
    assert!((first.peak - 0.8).abs() < 1e-5);
}
