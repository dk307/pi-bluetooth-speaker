//! Integration tests for `CamillaDSP` client and pipeline status driver.

use speaker_core::hardware::{
    DEFAULT_DSP_BUFFER_CAPACITY, DEFAULT_DSP_HOST, DEFAULT_DSP_PORT, DspChannelLevels, DspDevice,
    DspError, DspHardwareInfo, DspHost, DspPort, DspStatus, ProcessingState, StopReason,
};
use speaker_core::runtime::abort_and_map;

#[test]
fn host_and_port_validation_and_hardware_metadata() {
    let host = DspHost::parse_str("127.0.0.1").unwrap();
    assert_eq!(host.as_str(), "127.0.0.1");

    for invalid in ["", "  ", "invalid host name", "host$name"] {
        DspHost::parse_str(invalid).unwrap_err();
    }

    let port = DspPort::parse_str("1234").unwrap();
    assert_eq!(port.as_u16(), 1234);

    for invalid in ["0", "70000", "+1234", "-1234", "", "abc"] {
        DspPort::parse_str(invalid).unwrap_err();
    }

    let hw = DspHardwareInfo::rpi3();
    assert_eq!(hw.service_unit, "camilladsp.service");
    assert_eq!(hw.default_host, DEFAULT_DSP_HOST);
    assert_eq!(hw.default_port, DEFAULT_DSP_PORT);
    assert_eq!(hw.chunk_size, DEFAULT_DSP_BUFFER_CAPACITY);
    assert_eq!(hw.crossover_freq_hz, 180);
    assert_eq!(hw.infrasonic_hpf_hz, 45);
    assert_eq!(hw.capture_device, "hw:Loopback,1,0");
    assert_eq!(hw.playback_device, "hw:sndrpimerusamp,0");
}

#[tokio::test]
async fn shared_clones_and_async_polling_observe_mock_updates() {
    let (dsp, mock) = DspDevice::new_mocked();
    let dsp_clone = dsp.clone();

    // Verify initial simulated status
    let initial = dsp.read_status_async().await.unwrap();
    assert!(initial.is_operational());
    assert_eq!(initial.state, ProcessingState::Running);
    assert_eq!(initial.clipped_samples, 0);

    // Update state via mock controller
    mock.set_state(ProcessingState::Paused);
    mock.set_processing_load(15.2);
    mock.set_buffer_level(800);
    mock.set_clipped_samples(12);
    mock.set_volume(Some(-12.0), Some(true));
    mock.set_levels(
        vec![-10.0, -11.0],
        vec![-14.0, -15.0],
        vec![-12.0, -13.0],
        vec![-16.0, -17.0],
    );

    let updated = dsp_clone.read_status_async().await.unwrap();
    assert!(!updated.is_operational());
    assert_eq!(updated.state, ProcessingState::Paused);
    assert_eq!(updated.processing_load, 15.2);
    assert_eq!(updated.buffer_level, 800);
    assert_eq!(updated.clipped_samples, 12);
    assert_eq!(updated.volume_db, Some(-12.0));
    assert_eq!(updated.muted, Some(true));
    assert_eq!(updated.capture.peaks.as_ref(), &[-10.0, -11.0]);
    assert_eq!(updated.playback.peaks.as_ref(), &[-12.0, -13.0]);

    // Test async reset clipped
    dsp_clone.reset_clipped_samples_async().await.unwrap();
    let after_reset = dsp.read_status_async().await.unwrap();
    assert_eq!(after_reset.clipped_samples, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_device_clones_read_and_reset() {
    let (dsp, mock) = DspDevice::new_mocked();
    let d1 = dsp.clone();
    let d2 = dsp.clone();

    let h1 = tokio::spawn(async move {
        for _ in 0..100 {
            let res = d1.read_status_async().await;
            res.unwrap();
        }
    });

    let h2 = tokio::spawn(async move {
        for i in 0..100 {
            let load = (i % 20) as f32;
            mock.set_processing_load(load);
            if i % 10 == 0 {
                d2.reset_clipped_samples_async().await.unwrap();
            }
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();
    let _ = dsp.read_status().unwrap();
}

#[tokio::test]
async fn runtime_task_cancellation_mapping() {
    let err = abort_and_map::<DspError>("dsp read").await;
    assert!(err.is_cancelled());
    assert_eq!(
        err.summary(),
        "DSP task cancelled: dsp read task was cancelled"
    );
}

#[test]
fn dsp_status_predicates_and_conversions() {
    let status = DspStatus {
        service: speaker_core::hardware::ServiceState::Active,
        host: DspHost::default(),
        port: DspPort::default(),
        version: Some("4.1.3".into()),
        config_title: Some("Crossover".into()),
        config_path: Some("/etc/camilladsp/crossover.yml".into()),
        capture_rate: Some(48000),
        state: ProcessingState::Running,
        processing_load: 4.5,
        resampler_load: Some(0.2),
        rate_adjust: 1.000_015,
        buffer_level: 512,
        buffer_capacity: 1024,
        volume_db: Some(-3.0),
        muted: Some(false),
        clipped_samples: 0,
        stop_reason: StopReason::None,
        capture: DspChannelLevels::new(vec![-10.0, -11.0], vec![-14.0, -15.0]),
        playback: DspChannelLevels::new(vec![-12.0, -13.0], vec![-16.0, -17.0]),
    };

    assert!(status.is_operational());
    assert!(!status.has_clipping());
    assert!((status.buffer_fill_percent() - 50.0).abs() < 0.01);
    assert!((status.rate_adjust_ppm() - 15.0).abs() < 0.01);
    assert_eq!(status.capture.channels(), 2);
    assert_eq!(status.playback.channels(), 2);
    assert_eq!(status.capture.peak_or_floor(0), -10.0);
    assert_eq!(status.capture.rms_or_floor(0), -14.0);
    assert_eq!(status.capture.peak_or_floor(99), -100.0);
    assert_eq!(status.capture.rms_or_floor(99), -100.0);

    let mut clipped_status = status.clone();
    clipped_status.clipped_samples = 42;
    assert!(!clipped_status.is_operational());
    assert!(clipped_status.has_clipping());

    // Test ProcessingState methods
    let running = ProcessingState::Running;
    assert!(running.is_running());
    assert!(!running.is_paused());
    assert!(!running.is_stalled());
    assert_eq!(running.as_str(), "Running");
    assert_ne!(running.description(), "");

    let paused = ProcessingState::Paused;
    assert!(!paused.is_running());
    assert!(paused.is_paused());
    assert!(!paused.is_stalled());
    assert_eq!(paused.as_str(), "Paused");

    let stalled = ProcessingState::Stalled;
    assert!(!stalled.is_running());
    assert!(!stalled.is_paused());
    assert!(stalled.is_stalled());
    assert_eq!(stalled.as_str(), "Stalled");

    let unknown_state = ProcessingState::Unknown("Custom".into());
    assert_eq!(unknown_state.as_str(), "Custom");
    assert_eq!(unknown_state.description(), "Unknown state");

    // Test StopReason methods
    let none_reason = StopReason::None;
    assert!(none_reason.is_none());
    assert!(!none_reason.is_error());
    assert_eq!(none_reason.as_str(), "None");
    assert_ne!(none_reason.description(), "");

    let cap_err = StopReason::CaptureError;
    assert!(!cap_err.is_none());
    assert!(cap_err.is_error());
    assert_eq!(cap_err.as_str(), "CaptureError");

    let unknown_reason = StopReason::Unknown("CustomReason".into());
    assert_eq!(unknown_reason.as_str(), "CustomReason");
    assert_eq!(unknown_reason.description(), "Unknown stop reason");

    // Test meters
    assert_ne!(speaker_core::hardware::format_dbfs_meter(-20.0, 10), "");
    assert_ne!(speaker_core::hardware::format_load_meter(45.0, 10), "");
}
