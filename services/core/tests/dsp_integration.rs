//! Integration and hardware driver tests for `CamillaDSP` engine.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use speaker_core::hardware::{
    DEFAULT_DSP_HOST, DEFAULT_DSP_PORT, DspChannelLevels, DspDevice, DspHost, DspPort, DspStatus,
    ProcessingState, ServiceState, StopReason, dsp_test_helpers, format_dbfs_meter,
    format_load_meter,
};

#[test]
fn host_validation_and_parsing() {
    assert_eq!(DspHost::default().as_str(), "127.0.0.1");
    assert!(DspHost::default().is_loopback());
    assert!(DspHost::parse_str("localhost").unwrap().is_loopback());
    assert!(DspHost::parse_str("::1").unwrap().is_loopback());
    assert!(!DspHost::parse_str("192.168.2.110").unwrap().is_loopback());
    DspHost::parse_str("").unwrap_err();
    DspHost::parse_str("   ").unwrap_err();
    DspHost::parse_str("bad host name").unwrap_err();
    DspHost::parse_str("host$name").unwrap_err();
    DspHost::parse_str("---").unwrap_err();
    DspHost::parse_str("..").unwrap_err();
    DspHost::parse_str("-bad").unwrap_err();
    DspHost::parse_str("bad-").unwrap_err();
    DspHost::new("127.0.0.1").unwrap();
    DspHost::new("").unwrap_err();
}

#[test]
fn port_validation_and_parsing() {
    assert_eq!(DspPort::default().as_u16(), 1234);
    assert_eq!(DspPort::new(1234).unwrap().as_u16(), 1234);
    DspPort::new(0).unwrap_err();
    assert_eq!(DspPort::parse_str("1234").unwrap().as_u16(), 1234);
    assert_eq!(DspPort::parse_str("8080").unwrap().as_u16(), 8080);
    DspPort::parse_str("0").unwrap_err();
    DspPort::parse_str("70000").unwrap_err();
    DspPort::parse_str("+1234").unwrap_err();
    DspPort::parse_str("-1234").unwrap_err();
    DspPort::parse_str("").unwrap_err();
    DspPort::parse_str("abc").unwrap_err();
}

#[test]
fn processing_state_predicates_and_display() {
    let running = ProcessingState::Running;
    assert!(running.is_running());
    assert!(!running.is_paused());
    assert!(!running.is_stalled());
    assert_eq!(running.to_string(), "Running");
    assert_eq!(running.description(), "Audio processing active and running");

    let paused = ProcessingState::Paused;
    assert!(!paused.is_running());
    assert!(paused.is_paused());
    assert_eq!(paused.to_string(), "Paused");

    let stalled = ProcessingState::Stalled;
    assert!(stalled.is_stalled());
    assert_eq!(stalled.to_string(), "Stalled");

    let starting = ProcessingState::Starting;
    assert_eq!(starting.to_string(), "Starting");

    let inactive = ProcessingState::Inactive;
    assert_eq!(inactive.to_string(), "Inactive");

    let unknown = ProcessingState::Unknown("Custom".into());
    assert_eq!(unknown.to_string(), "Custom");
    assert_eq!(unknown.description(), "Unknown state");

    assert_eq!(
        ProcessingState::parse_state("Running"),
        ProcessingState::Running
    );
    assert_eq!(
        ProcessingState::parse_state("PAUSED"),
        ProcessingState::Paused
    );
    assert_eq!(
        ProcessingState::parse_state("stalled"),
        ProcessingState::Stalled
    );
}

#[test]
fn stop_reason_predicates_and_display() {
    let none = StopReason::None;
    assert!(none.is_none());
    assert!(!none.is_error());
    assert_eq!(none.to_string(), "None");
    assert_eq!(none.description(), "No stop recorded");

    let done = StopReason::Done;
    assert!(!done.is_none());
    assert!(!done.is_error());

    let cap_err = StopReason::CaptureError;
    assert!(cap_err.is_error());
    assert_eq!(cap_err.to_string(), "CaptureError");

    let play_err = StopReason::PlaybackError;
    assert!(play_err.is_error());

    let cap_change = StopReason::CaptureFormatChange;
    assert!(!cap_change.is_error());

    let play_change = StopReason::PlaybackFormatChange;
    assert!(!play_change.is_error());

    let unknown = StopReason::Unknown("X".into());
    assert_eq!(unknown.to_string(), "X");
    assert_eq!(unknown.description(), "Unknown stop reason");

    assert_eq!(StopReason::parse_reason("None"), StopReason::None);
    assert_eq!(
        StopReason::parse_reason("CaptureError"),
        StopReason::CaptureError
    );
}

#[test]
fn dsp_channel_levels_and_floors() {
    let levels = DspChannelLevels::new(vec![-12.5, -14.0], vec![-18.0, -19.5]);
    assert_eq!(levels.channels(), 2);
    assert_eq!(levels.peak_or_floor(0), -12.5);
    assert_eq!(levels.peak_or_floor(1), -14.0);
    assert_eq!(levels.peak_or_floor(2), -100.0);
    assert_eq!(levels.rms_or_floor(0), -18.0);
    assert_eq!(levels.rms_or_floor(1), -19.5);
    assert_eq!(levels.rms_or_floor(5), -100.0);
}

#[test]
fn dsp_status_calculations() {
    let status = DspStatus {
        service: ServiceState::Active,
        host: DspHost::default(),
        port: DspPort::default(),
        version: Some("4.1.3".into()),
        config_title: Some("Test".into()),
        config_path: Some("/etc/camilladsp/crossover.yml".into()),
        capture_rate: Some(48000),
        state: ProcessingState::Running,
        processing_load: 5.0,
        resampler_load: Some(0.1),
        rate_adjust: 1.000_020,
        buffer_level: 512,
        buffer_capacity: 1024,
        volume_db: Some(-6.0),
        muted: Some(false),
        clipped_samples: 0,
        stop_reason: StopReason::None,
        capture: DspChannelLevels::new(vec![-12.0], vec![-16.0]),
        playback: DspChannelLevels::new(vec![-14.0], vec![-18.0]),
    };

    assert!((status.buffer_fill_percent() - 50.0).abs() < 0.01);
    assert!((status.rate_adjust_ppm() - 20.0).abs() < 0.01);
    assert!(status.is_operational());
    assert!(!status.has_clipping());

    let mut clipped = status.clone();
    clipped.clipped_samples = 42;
    assert!(!clipped.is_operational());
    assert!(clipped.has_clipping());

    let mut zero_buf = status.clone();
    zero_buf.buffer_capacity = 0;
    assert_eq!(zero_buf.buffer_fill_percent(), 0.0);
}

#[test]
fn format_meters() {
    let bar_full = format_dbfs_meter(0.0, 10);
    assert_eq!(bar_full, "[██████████]");

    let bar_silence = format_dbfs_meter(-60.0, 10);
    assert_eq!(bar_silence, "[░░░░░░░░░░]");

    let bar_half = format_dbfs_meter(-30.0, 10);
    assert_eq!(bar_half, "[█████░░░░░]");

    let load_bar = format_load_meter(50.0, 10);
    assert_eq!(load_bar, "[█████░░░░░]");

    assert_eq!(format_dbfs_meter(0.0, 0), "");
    assert_eq!(format_load_meter(0.0, 0), "");
}

#[test]
fn mock_device_operations_and_fault_injection() {
    let (dsp, mock) = DspDevice::new_mocked();
    let status = dsp.read_status().unwrap();
    assert_eq!(status.state, ProcessingState::Running);
    assert_eq!(status.clipped_samples, 0);

    mock.set_state(ProcessingState::Paused);
    mock.set_processing_load(12.5);
    mock.set_buffer_level(768);
    mock.set_rate_adjust(0.999_990);
    mock.set_clipped_samples(5);
    mock.set_volume(Some(-10.0), Some(true));
    mock.set_levels(vec![-10.0], vec![-14.0], vec![-12.0], vec![-16.0]);
    mock.set_service_state(ServiceState::Inactive);

    let updated = dsp.read_status().unwrap();
    assert_eq!(updated.state, ProcessingState::Paused);
    assert_eq!(updated.processing_load, 12.5);
    assert_eq!(updated.buffer_level, 768);
    assert_eq!(updated.clipped_samples, 5);
    assert_eq!(updated.volume_db, Some(-10.0));
    assert_eq!(updated.muted, Some(true));
    assert_eq!(updated.service, ServiceState::Inactive);

    dsp.reset_clipped_samples().unwrap();
    assert_eq!(dsp.read_status().unwrap().clipped_samples, 0);

    // Fault injection
    mock.inject_fault("simulated connection failure");
    let err = dsp.read_status().unwrap_err();
    assert!(err.is_protocol_error());
    dsp.reset_clipped_samples().unwrap_err();

    mock.clear_fault();
    dsp.read_status().unwrap();
}

#[test]
fn dsp_error_predicates_and_summary() {
    let err =
        dsp_test_helpers::make_connection_failed_error("127.0.0.1", 1234, "connection refused");
    assert!(err.is_connection_failed());
    assert!(!err.is_protocol_error());
    assert!(!err.is_timeout());
    assert!(!err.is_command_failed());
    assert!(!err.is_cancelled());
    assert_eq!(
        err.summary(),
        "Failed to connect to CamillaDSP at ws://127.0.0.1:1234: connection refused"
    );

    let proto_err = dsp_test_helpers::make_protocol_error("corrupted frame");
    assert!(proto_err.is_protocol_error());
    assert_eq!(
        proto_err.summary(),
        "CamillaDSP WebSocket protocol error: corrupted frame"
    );

    let timeout_err = dsp_test_helpers::make_timeout_error("deadline exceeded");
    assert!(timeout_err.is_timeout());

    let cmd_err = dsp_test_helpers::make_command_failed_error("GetVolume", "volume not enabled");
    assert!(cmd_err.is_command_failed());

    let invalid_err = dsp_test_helpers::make_invalid_state_error("port cannot be 0");
    assert!(invalid_err.is_invalid_state());
    assert!(!err.is_invalid_state());
    assert_eq!(
        invalid_err.summary(),
        "Invalid DSP parameter or state: port cannot be 0"
    );

    let cancel_err = dsp_test_helpers::make_cancelled_error("task aborted");
    assert!(cancel_err.is_cancelled());
    assert_eq!(cancel_err.summary(), "DSP task cancelled: task aborted");
}

#[test]
fn parse_json_helpers() {
    let json_arr = serde_json::json!([-10.5, -20.0, "invalid", 5.0]);
    let parsed = dsp_test_helpers::helper_parse_f32_list(&json_arr);
    assert_eq!(parsed, vec![-10.5, -20.0, 5.0]);

    let levels_obj = serde_json::json!({
        "capture_peak": [-12.0, -14.0],
        "capture_rms": [-18.0, -20.0],
        "playback_peak": [-15.0, -16.0],
        "playback_rms": [-21.0, -22.0]
    });
    let (cp, cr, pp, pr) = dsp_test_helpers::helper_parse_levels_object(&levels_obj);
    assert_eq!(cp, vec![-12.0, -14.0]);
    assert_eq!(cr, vec![-18.0, -20.0]);
    assert_eq!(pp, vec![-15.0, -16.0]);
    assert_eq!(pr, vec![-21.0, -22.0]);
}

#[test]
fn hardware_info_constants() {
    let dsp = DspDevice::new();
    let hw = dsp.hardware_info();
    assert_eq!(hw.service_unit, "camilladsp.service");
    assert_eq!(hw.default_port, 1234);
    assert_eq!(hw.chunk_size, 1024);
    assert_eq!(hw.crossover_freq_hz, 180);

    assert_eq!(dsp.host().as_str(), DEFAULT_DSP_HOST);
    assert_eq!(dsp.port().as_u16(), DEFAULT_DSP_PORT);
    let debug_str = format!("{dsp:?}");
    assert!(debug_str.contains("DspDevice::Hardware"));

    let default_dsp = DspDevice::default();
    assert_eq!(default_dsp.port().as_u16(), 1234);

    let (mock_dev, mock_ctrl) = DspDevice::new_mocked();
    let mock_debug = format!("{mock_dev:?}");
    assert!(mock_debug.contains("DspDevice::Mock"));
    let ctrl_debug = format!("{mock_ctrl:?}");
    assert!(ctrl_debug.contains("DspDeviceMock"));

    mock_ctrl.set_resampler_load(Some(0.5));
    mock_ctrl.set_stop_reason(StopReason::Done);
    mock_ctrl.set_version(Some("4.2.0"));
    mock_ctrl.set_config_title(Some("New Config"));
    mock_ctrl.set_capture_rate(Some(96000));
    let mock_status = mock_dev.read_status().unwrap();
    assert_eq!(mock_status.resampler_load, Some(0.5));
    assert_eq!(mock_status.stop_reason, StopReason::Done);
    assert_eq!(mock_status.version.as_deref(), Some("4.2.0"));
    assert_eq!(mock_status.config_title.as_deref(), Some("New Config"));
    assert_eq!(mock_status.capture_rate, Some(96000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hardware_backend_websocket_roundtrip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = listener.local_addr().unwrap().port();

    let server_task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(msg) = ws.next().await {
                    let Ok(msg) = msg else { break };
                    if let Message::Text(text) = msg {
                        let cmd: String = serde_json::from_str(&text).unwrap();
                        let resp = match cmd.as_str() {
                            "GetVersion" => {
                                serde_json::json!({"GetVersion": {"result": "Ok", "value": "4.1.3"}})
                            }
                            "GetConfigTitle" => {
                                serde_json::json!({"GetConfigTitle": {"result": "Ok", "value": "Test Engine"}})
                            }
                            "GetConfigFilePath" => {
                                serde_json::json!({"GetConfigFilePath": {"result": "Ok", "value": "/etc/camilladsp/crossover.yml"}})
                            }
                            "GetState" => {
                                serde_json::json!({"GetState": {"result": "Ok", "value": "Running"}})
                            }
                            "GetProcessingLoad" => {
                                serde_json::json!({"GetProcessingLoad": {"result": "Ok", "value": 4.25}})
                            }
                            "GetResamplerLoad" => {
                                serde_json::json!({"GetResamplerLoad": {"result": "Ok", "value": 0.15}})
                            }
                            "GetRateAdjust" => {
                                serde_json::json!({"GetRateAdjust": {"result": "Ok", "value": 1.000_010}})
                            }
                            "GetBufferLevel" => {
                                serde_json::json!({"GetBufferLevel": {"result": "Ok", "value": 512}})
                            }
                            "GetClippedSamples" => {
                                serde_json::json!({"GetClippedSamples": {"result": "Ok", "value": 0}})
                            }
                            "GetCaptureRate" => {
                                serde_json::json!({"GetCaptureRate": {"result": "Ok", "value": 48000}})
                            }
                            "GetStopReason" => {
                                serde_json::json!({"GetStopReason": {"result": "Ok", "value": "None"}})
                            }
                            "GetVolume" => {
                                serde_json::json!({"GetVolume": {"result": "Ok", "value": -6.0}})
                            }
                            "GetMute" => {
                                serde_json::json!({"GetMute": {"result": "Ok", "value": false}})
                            }
                            "GetSignalLevels" => serde_json::json!({
                                "GetSignalLevels": {
                                    "result": "Ok",
                                    "value": {
                                        "capture_peak": [-15.0, -14.5],
                                        "capture_rms": [-20.0, -19.5],
                                        "playback_peak": [-18.0, -17.5],
                                        "playback_rms": [-23.0, -22.5]
                                    }
                                }
                            }),
                            "ResetClippedSamples" => {
                                serde_json::json!({"ResetClippedSamples": {"result": "Ok"}})
                            }
                            other => {
                                serde_json::json!({other: {"result": "Error", "value": "unsupported"}})
                            }
                        };
                        if ws
                            .send(Message::Text(serde_json::to_string(&resp).unwrap().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });
        }
    });

    let dsp = DspDevice::with_host_and_port(DspHost::default(), DspPort::new(local_port).unwrap());

    let status = dsp.read_status_async().await.unwrap();
    assert_eq!(status.version.as_deref(), Some("4.1.3"));
    assert_eq!(status.config_title.as_deref(), Some("Test Engine"));
    assert_eq!(status.state, ProcessingState::Running);
    assert_eq!(status.processing_load, 4.25);
    assert_eq!(status.resampler_load, Some(0.15));
    assert_eq!(status.rate_adjust, 1.000_010);
    assert_eq!(status.buffer_level, 512);
    assert_eq!(status.volume_db, Some(-6.0));
    assert_eq!(status.muted, Some(false));
    assert_eq!(status.capture.peaks.as_ref(), &[-15.0, -14.5]);
    assert_eq!(status.playback.peaks.as_ref(), &[-18.0, -17.5]);

    // Synchronous read_status with block_in_place inside tokio
    let sync_status = dsp.read_status().unwrap();
    assert_eq!(sync_status.state, ProcessingState::Running);

    // Async reset
    dsp.reset_clipped_samples_async().await.unwrap();

    // Sync reset
    dsp.reset_clipped_samples().unwrap();

    server_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hardware_backend_levels_fallback_and_command_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_port = listener.local_addr().unwrap().port();

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(msg) = ws.next().await {
            let Ok(msg) = msg else { break };
            if let Message::Text(text) = msg {
                let cmd: String = serde_json::from_str(&text).unwrap();
                let resp = match cmd.as_str() {
                    "GetState" => {
                        serde_json::json!({"GetState": {"result": "Ok", "value": "Running"}})
                    }
                    "GetProcessingLoad" => {
                        serde_json::json!({"GetProcessingLoad": {"result": "Ok", "value": 1.5}})
                    }
                    "GetBufferLevel" => {
                        serde_json::json!({"GetBufferLevel": {"result": "Ok", "value": 512}})
                    }
                    "GetRateAdjust" => {
                        serde_json::json!({"GetRateAdjust": {"result": "Ok", "value": 1.0}})
                    }
                    "GetClippedSamples" => {
                        serde_json::json!({"GetClippedSamples": {"result": "Ok", "value": 0}})
                    }
                    "GetStopReason" => {
                        serde_json::json!({"GetStopReason": {"result": "Ok", "value": "None"}})
                    }
                    "GetSignalLevels" => {
                        serde_json::json!({"GetSignalLevels": {"result": "Error", "value": "not implemented"}})
                    }
                    "GetCaptureSignalPeak" => {
                        serde_json::json!({"GetCaptureSignalPeak": {"result": "Ok", "value": [-10.0, -11.0]}})
                    }
                    "GetCaptureSignalRms" => {
                        serde_json::json!({"GetCaptureSignalRms": {"result": "Ok", "value": [-16.0, -17.0]}})
                    }
                    "GetPlaybackSignalPeak" => {
                        serde_json::json!({"GetPlaybackSignalPeak": {"result": "Ok", "value": [-12.0, -13.0]}})
                    }
                    "GetPlaybackSignalRms" => {
                        serde_json::json!({"GetPlaybackSignalRms": {"result": "Ok", "value": [-18.0, -19.0]}})
                    }
                    other => {
                        serde_json::json!({other: {"result": "Error", "value": "not implemented"}})
                    }
                };
                ws.send(Message::Text(serde_json::to_string(&resp).unwrap().into()))
                    .await
                    .unwrap();
            }
        }
    });

    let dsp = DspDevice::with_host_and_port(DspHost::default(), DspPort::new(local_port).unwrap());

    let status = dsp.read_status_async().await.unwrap();
    assert_eq!(status.capture.peaks.as_ref(), &[-10.0, -11.0]);
    assert_eq!(status.capture.rms.as_ref(), &[-16.0, -17.0]);
    assert_eq!(status.playback.peaks.as_ref(), &[-12.0, -13.0]);
    assert_eq!(status.playback.rms.as_ref(), &[-18.0, -19.0]);

    server_task.abort();
}

#[tokio::test]
async fn hardware_backend_connection_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let dsp = DspDevice::with_host_and_port(DspHost::default(), DspPort::new(port).unwrap());
    let err = dsp.read_status_async().await.unwrap_err();
    assert!(err.is_connection_failed());

    let reset_err = dsp.reset_clipped_samples_async().await.unwrap_err();
    assert!(reset_err.is_connection_failed());
}

#[test]
fn dsp_domain_models_exhaustive_descriptions_and_display() {
    let host = DspHost::default();
    assert_eq!(format!("{host}"), "127.0.0.1");

    let port = DspPort::default();
    assert_eq!(format!("{port}"), "1234");

    for (state, expected_desc) in [
        (
            ProcessingState::Running,
            "Audio processing active and running",
        ),
        (
            ProcessingState::Paused,
            "Processing paused (silence detected)",
        ),
        (
            ProcessingState::Inactive,
            "Inactive (waiting for configuration)",
        ),
        (ProcessingState::Starting, "Starting up pipeline"),
        (ProcessingState::Stalled, "Stalled (capture device starved)"),
        (ProcessingState::Unknown("custom".into()), "Unknown state"),
    ] {
        assert_eq!(state.description(), expected_desc);
        assert_ne!(state.as_str(), "");
        assert_eq!(format!("{state}"), state.as_str());
    }

    assert!(ProcessingState::Running.is_running());
    assert!(!ProcessingState::Paused.is_running());
    assert!(ProcessingState::Paused.is_paused());
    assert!(!ProcessingState::Running.is_paused());
    assert!(ProcessingState::Stalled.is_stalled());
    assert!(!ProcessingState::Running.is_stalled());

    assert_eq!(
        ProcessingState::parse_state("unknown_state"),
        ProcessingState::Unknown("unknown_state".into())
    );

    for (reason, expected_desc) in [
        (StopReason::None, "No stop recorded"),
        (StopReason::Done, "Stream completed"),
        (StopReason::CaptureError, "Capture device hardware error"),
        (StopReason::PlaybackError, "Playback device hardware error"),
        (
            StopReason::CaptureFormatChange,
            "Capture audio format changed",
        ),
        (
            StopReason::PlaybackFormatChange,
            "Playback audio format changed",
        ),
        (StopReason::Unknown("custom".into()), "Unknown stop reason"),
    ] {
        assert_eq!(reason.description(), expected_desc);
        assert_ne!(reason.as_str(), "");
        assert_eq!(format!("{reason}"), reason.as_str());
    }

    assert!(StopReason::None.is_none());
    assert!(!StopReason::Done.is_none());
    assert!(StopReason::CaptureError.is_error());
    assert!(StopReason::PlaybackError.is_error());
    assert!(!StopReason::Done.is_error());

    assert_eq!(
        StopReason::parse_reason("custom_reason"),
        StopReason::Unknown("custom_reason".into())
    );

    // Channel levels out-of-bounds peak/rms or floor
    let levels = DspChannelLevels {
        peaks: Box::new([-5.0]),
        rms: Box::new([-12.0]),
    };
    assert_eq!(levels.channels(), 1);
    assert_eq!(levels.peak_or_floor(0), -5.0);
    assert_eq!(levels.peak_or_floor(99), -100.0);
    assert_eq!(levels.rms_or_floor(0), -12.0);
    assert_eq!(levels.rms_or_floor(99), -100.0);
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
