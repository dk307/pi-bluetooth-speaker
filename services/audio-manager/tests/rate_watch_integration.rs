//! Integration tests for `audio_manager::rate_watch` dynamic sample-rate switching.

use audio_manager::rate_watch::{
    LoopbackState, RateWatchController, apply_loopback_state, build_reconfigured_config_json,
    chunksize_for_rate, reconcile_with_engine, run_with_rx,
};
use camilladsp_schema::config::Configuration;
use speaker_common::diag::ScopedTempDir;
use speaker_dsp::{DspConfigInfo, DspDevice, ProcessingState};

const SAMPLE_CONFIG_YAML: &str = "\
devices:
  samplerate: 48000
  chunksize: 1024
  queuelimit: 4
  enable_rate_adjust: true
  stop_on_rate_change: true
  silence_threshold: -60
  silence_timeout_s: 3.0
  capture:
    type: Alsa
    channels: 2
    device: \"hw:Loopback,1,0\"
    format: S32_LE
    stop_on_inactive: true
  playback:
    type: Alsa
    channels: 2
    device: \"hw:sndrpihifiberry,0\"
    format: S32_LE

filters:
  sub_hpf:
    type: Biquad
    parameters:
      type: Highpass
      freq: 45
      q: 0.707

pipeline:
  - type: Filter
    channels: [0]
    names:
      - sub_hpf
";

#[test]
fn chunksize_scales_with_sample_rate_family() {
    assert_eq!(chunksize_for_rate(44_100), 1024);
    assert_eq!(chunksize_for_rate(48_000), 1024);
    assert_eq!(chunksize_for_rate(88_200), 2048);
    assert_eq!(chunksize_for_rate(96_000), 2048);
    assert_eq!(chunksize_for_rate(176_400), 4096);
    assert_eq!(chunksize_for_rate(192_000), 4096);
}

#[test]
fn reconfigured_config_validates_schema_and_preserves_filters() {
    let json = build_reconfigured_config_json(SAMPLE_CONFIG_YAML, 96_000, 2048).unwrap();
    let cfg: Configuration = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg.devices.samplerate.get(), 96_000);
    assert_eq!(cfg.devices.chunksize.get(), 2048);
    assert!(cfg.filters.as_ref().unwrap().contains_key("sub_hpf"));
    assert_eq!(cfg.pipeline.as_ref().unwrap().len(), 1);

    // Rejects legacy CamillaDSP 4.1.3 key (`silence_timeout` instead of `silence_timeout_s`)
    let legacy_yaml = SAMPLE_CONFIG_YAML.replace("silence_timeout_s", "silence_timeout");
    assert!(build_reconfigured_config_json(&legacy_yaml, 48_000, 1024).is_err());

    // Rejects zero sample rate or chunksize
    assert!(build_reconfigured_config_json(SAMPLE_CONFIG_YAML, 0, 1024).is_err());
    assert!(build_reconfigured_config_json(SAMPLE_CONFIG_YAML, 48_000, 0).is_err());
}

#[tokio::test]
async fn rate_watch_controller_applies_switches_and_retries_on_fault() {
    let tmp = ScopedTempDir::new("rate_watch_test");
    let cfg_path = tmp.path().join("crossover.yml");
    std::fs::write(&cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    let (dsp, mock) = DspDevice::new_mocked();
    let mut ctrl = RateWatchController::new();

    // Inactive state does not push config
    assert!(
        !apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: false,
                rate_hz: 48_000
            }
        )
        .await
    );
    assert!(mock.last_set_config_json().is_none());

    // Fault during SetConfigJson must NOT mark rate as applied (retries next tick)
    mock.inject_fault("ws down");
    assert!(
        !apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: true,
                rate_hz: 44_100
            }
        )
        .await
    );
    assert_eq!(ctrl.applied_rate(), None);

    // Clear fault -> applies 44100 Hz while preserving filters
    mock.clear_fault();
    assert!(
        apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: true,
                rate_hz: 44_100
            }
        )
        .await
    );
    assert_eq!(ctrl.applied_rate(), Some(44_100));
    let pushed: Configuration =
        serde_json::from_str(&mock.last_set_config_json().unwrap()).unwrap();
    assert_eq!(pushed.devices.samplerate.get(), 44_100);
    assert_eq!(pushed.devices.chunksize.get(), 1024);
    assert!(pushed.filters.as_ref().unwrap().contains_key("sub_hpf"));

    // Same rate while active is a no-op
    assert!(
        !apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: true,
                rate_hz: 44_100
            }
        )
        .await
    );

    // Rate change to 192000 Hz pushes new config with chunksize 4096
    assert!(
        apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: true,
                rate_hz: 192_000
            }
        )
        .await
    );
    assert_eq!(ctrl.applied_rate(), Some(192_000));
    let pushed_192: Configuration =
        serde_json::from_str(&mock.last_set_config_json().unwrap()).unwrap();
    assert_eq!(pushed_192.devices.samplerate.get(), 192_000);
    assert_eq!(pushed_192.devices.chunksize.get(), 4096);

    // Stop clears applied_rate so next start at same rate re-applies cleanly
    assert!(
        !apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: false,
                rate_hz: 192_000
            }
        )
        .await
    );
    assert_eq!(ctrl.applied_rate(), None);
    assert!(
        apply_loopback_state(
            &mut ctrl,
            &dsp,
            &cfg_path,
            LoopbackState {
                active: true,
                rate_hz: 192_000
            }
        )
        .await
    );
    assert_eq!(ctrl.applied_rate(), Some(192_000));

    // Missing config file or invalid YAML returns false without marking applied
    let missing = tmp.path().join("missing.yml");
    let mut ctrl2 = RateWatchController::new();
    assert!(
        !apply_loopback_state(
            &mut ctrl2,
            &dsp,
            &missing,
            LoopbackState {
                active: true,
                rate_hz: 48_000
            }
        )
        .await
    );
    let bad_path = tmp.path().join("bad.yml");
    std::fs::write(&bad_path, "not: valid: camilladsp").unwrap();
    assert!(
        !apply_loopback_state(
            &mut ctrl2,
            &dsp,
            &bad_path,
            LoopbackState {
                active: true,
                rate_hz: 48_000
            }
        )
        .await
    );
}

#[tokio::test]
async fn run_with_rx_reapplies_when_engine_inactive_and_exits_on_close() {
    let tmp = ScopedTempDir::new("rate_watch_rx");
    let cfg_path = tmp.path().join("crossover.yml");
    std::fs::write(&cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    let (dsp, mock) = DspDevice::new_mocked();
    let (tx, rx) = tokio::sync::mpsc::channel(8);

    let cfg_clone = cfg_path.clone();
    let dsp_clone = dsp.clone();
    let handle = tokio::spawn(async move {
        run_with_rx(dsp_clone, &cfg_clone, rx).await;
    });

    // Initial active @ 48000 Hz
    tx.send((
        LoopbackState {
            active: true,
            rate_hz: 48_000,
        },
        false,
    ))
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(mock.last_set_config_json().is_some());

    // Simulate engine going Inactive (fast close/reopen at same rate) with from_event=true
    mock.set_state(ProcessingState::Inactive);
    tx.send((
        LoopbackState {
            active: true,
            rate_hz: 48_000,
        },
        true,
    ))
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Close channel -> task exits cleanly
    drop(tx);
    handle.await.unwrap();
}

#[tokio::test]
async fn reconcile_with_engine_recovers_after_dsp_restart() {
    let tmp = ScopedTempDir::new("rate_watch_reconcile");
    let cfg_path = tmp.path().join("crossover.yml");
    std::fs::write(&cfg_path, SAMPLE_CONFIG_YAML).unwrap();

    let (dsp, mock) = DspDevice::new_mocked();
    let mut ctrl = RateWatchController::new();
    let state_44k = LoopbackState {
        active: true,
        rate_hz: 44_100,
    };

    // Initial configure at 44100 Hz
    assert!(apply_loopback_state(&mut ctrl, &dsp, &cfg_path, state_44k).await);
    mock.set_config(Some(DspConfigInfo {
        samplerate: Some(44_100),
        ..DspConfigInfo::default()
    }));

    // Matching live rate is a no-op
    assert!(!reconcile_with_engine(&mut ctrl, &dsp, &cfg_path, state_44k).await);

    // Simulate camilladsp.service restart reverting to static 48000 Hz config
    mock.set_config(Some(DspConfigInfo {
        samplerate: Some(48_000),
        ..DspConfigInfo::default()
    }));
    assert!(reconcile_with_engine(&mut ctrl, &dsp, &cfg_path, state_44k).await);
    let pushed: Configuration =
        serde_json::from_str(&mock.last_set_config_json().unwrap()).unwrap();
    assert_eq!(pushed.devices.samplerate.get(), 44_100);
}
