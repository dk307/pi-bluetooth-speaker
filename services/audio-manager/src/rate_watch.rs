//! ALSA Loopback sample-rate watcher for dynamic `CamillaDSP` reconfiguration.
//!
//! Monitors `hw:Loopback` (`PCM Slave Active` and `PCM Slave Rate` on device 1,
//! subdevice 0 — the capture-side controls that reflect the playback writer on
//! `hw:Loopback,0,0` in `snd-aloop`). Also enables `PCM Notify` on device 1,
//! subdevice 0 so kernel control events fire immediately when a player opens,
//! closes, or changes sample rate.
//!
//! Whenever a player opens `hw:Loopback,0,0` at any sample rate (e.g. 44.1,
//! 48, 88.2, 96, 176.4, or 192 kHz), `rate_watch` parses and validates the
//! active `CamillaDSP` YAML config file using `camilladsp-schema`, updates
//! `devices.samplerate` and `devices.chunksize` (preserving all filters,
//! mixers, processors, and pipeline stages), and uploads the validated JSON
//! via `SetConfigJson`.

use std::ffi::CString;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use alsa::ctl::{Ctl, ElemId, ElemIface, ElemType, ElemValue};
use camilladsp_schema::config::{parse_config, validate_config};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use speaker_dsp::DspDevice;

/// Default `CamillaDSP` YAML configuration file read as the base template.
pub const DEFAULT_CAMILLADSP_CONFIG_PATH: &str = "/etc/camilladsp/crossover.yml";

/// Capture-side ALSA Loopback device index (`hw:Loopback,1,0`).
///
/// In `snd-aloop`, the `PCM Slave *` controls on device `1` reflect the state
/// of the playback writer on device `0` (`hw:Loopback,0,0`).
const LOOPBACK_CAPTURE_DEVICE: u32 = 1;

/// ALSA Loopback subdevice index (`0`).
const LOOPBACK_SUBDEVICE: u32 = 0;

/// Snapshot of `hw:Loopback` playback-writer state read from device 1 controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackState {
    /// `true` when `PCM Slave Active` is non-zero (a player holds `hw:Loopback,0,0`).
    pub active: bool,
    /// Current `PCM Slave Rate` in Hz reported by `snd-aloop`.
    pub rate_hz: u32,
}

/// Returns the `CamillaDSP` chunksize for the given sample rate.
///
/// Scales chunk size with sample-rate family (`1024` at <= 48 kHz, `2048` at
/// <= 96 kHz, `4096` above 96 kHz) so chunk period stays ~21 ms across rates.
#[must_use]
pub const fn chunksize_for_rate(rate_hz: u32) -> u32 {
    if rate_hz <= 48_000 {
        1024
    } else if rate_hz <= 96_000 {
        2048
    } else {
        4096
    }
}

/// Parses a `CamillaDSP` YAML config via `camilladsp-schema`, updates
/// `devices.samplerate` and `devices.chunksize`, validates the resulting
/// `Configuration` against CamillaDSP 5.0 schema rules, and returns the
/// serialized JSON string for `SetConfigJson`.
///
/// # Errors
/// Returns an error string if the YAML fails schema parsing, if `rate_hz` or
/// `chunksize` is zero, or if `camilladsp-schema` validation fails.
pub fn build_reconfigured_config_json(
    yaml: &str,
    rate_hz: u32,
    chunksize: u32,
) -> Result<String, String> {
    let rate = NonZeroUsize::new(rate_hz as usize)
        .ok_or_else(|| "sample rate must be non-zero".to_owned())?;
    let chunk = NonZeroUsize::new(chunksize as usize)
        .ok_or_else(|| "chunksize must be non-zero".to_owned())?;

    let mut cfg = parse_config(yaml).map_err(|e| format!("schema parse error: {e}"))?;
    cfg.devices.samplerate = rate;
    cfg.devices.chunksize = chunk;

    validate_config(&mut cfg, None).map_err(|e| format!("schema validation error: {e}"))?;

    serde_json::to_string(&cfg).map_err(|e| format!("JSON serialization error: {e}"))
}

/// Pure state machine deciding when to push a new sample-rate config to `CamillaDSP`.
#[derive(Debug, Default)]
pub struct RateWatchController {
    applied_rate: Option<u32>,
}

impl RateWatchController {
    /// Creates a new controller with no applied rate yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { applied_rate: None }
    }

    /// Evaluates a new [`LoopbackState`] and returns `Some(rate_hz)` if `CamillaDSP`
    /// should be configured for `rate_hz`.
    ///
    /// Call [`Self::confirm_applied`] only after `SetConfigJson` succeeds so failed
    /// writes are retried on the next update.
    pub fn pending_rate(&mut self, state: LoopbackState) -> Option<u32> {
        if !state.active || state.rate_hz == 0 {
            self.applied_rate = None;
            return None;
        }
        if self.applied_rate == Some(state.rate_hz) {
            None
        } else {
            Some(state.rate_hz)
        }
    }

    /// Records that `rate_hz` was successfully applied to `CamillaDSP`.
    pub fn confirm_applied(&mut self, rate_hz: u32) {
        self.applied_rate = Some(rate_hz);
    }

    /// Returns the currently applied sample rate, if any.
    #[must_use]
    pub const fn applied_rate(&self) -> Option<u32> {
        self.applied_rate
    }
}

/// Applies a [`LoopbackState`] update to `dsp` using `config_path` as the base
/// YAML template if a rate change is needed.
///
/// When `force_if_inactive` is `true` and `ctrl` believes `state.rate_hz` is
/// already applied, this checks `CamillaDSP`'s live state and re-applies the
/// config if the engine went `Inactive` (e.g. a fast `1 -> 0 -> 1` bounce on
/// `PCM Slave Active` at the same sample rate).
pub async fn apply_loopback_state(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    state: LoopbackState,
) -> bool {
    apply_loopback_state_ext(ctrl, dsp, config_path, state, false).await
}

async fn apply_loopback_state_ext(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    state: LoopbackState,
    force_if_inactive: bool,
) -> bool {
    if force_if_inactive
        && state.active
        && ctrl.applied_rate() == Some(state.rate_hz)
        && matches!(
            dsp.read_state_async().await,
            Ok(speaker_dsp::ProcessingState::Inactive)
        )
    {
        // Engine stopped (e.g. stop_on_inactive during a fast close/reopen at the same rate).
        ctrl.applied_rate = None;
    }
    let Some(rate_hz) = ctrl.pending_rate(state) else {
        return false;
    };
    let raw_yaml = match tokio::fs::read_to_string(config_path).await {
        Ok(contents) => contents,
        Err(e) => {
            warn!(
                path = %config_path.display(),
                error = %e,
                "Failed to read CamillaDSP base config; will retry"
            );
            return false;
        }
    };
    let chunksize = chunksize_for_rate(rate_hz);
    let config_json = match build_reconfigured_config_json(&raw_yaml, rate_hz, chunksize) {
        Ok(json) => json,
        Err(e) => {
            warn!(
                path = %config_path.display(),
                error = %e,
                "Failed to build/validate CamillaDSP config via camilladsp-schema"
            );
            return false;
        }
    };

    match dsp.set_config_json_async(&config_json).await {
        Ok(()) => {
            ctrl.confirm_applied(rate_hz);
            info!(
                rate_hz,
                chunksize,
                path = %config_path.display(),
                "Configured CamillaDSP for Loopback sample rate"
            );
            true
        }
        Err(e) => {
            warn!(rate_hz, error = %e, "Failed to configure CamillaDSP sample rate; will retry");
            false
        }
    }
}

/// Reconciles `ctrl` against `CamillaDSP`'s live status when a loopback stream is
/// active, recovering if `camilladsp.service` restarted with its static config.
pub async fn reconcile_with_engine(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    state: LoopbackState,
) -> bool {
    if !state.active || state.rate_hz == 0 {
        return false;
    }
    if ctrl.applied_rate() == Some(state.rate_hz) {
        let Ok(status) = dsp.read_status_async().await else {
            return false;
        };
        let live_rate = status.config.as_ref().and_then(|c| c.samplerate);
        if status.state != speaker_dsp::ProcessingState::Inactive
            && live_rate == Some(state.rate_hz)
        {
            return false;
        }
        warn!(
            expected_rate = state.rate_hz,
            ?live_rate,
            engine_state = %status.state,
            "CamillaDSP diverged from active Loopback rate; reconfiguring"
        );
        ctrl.applied_rate = None;
    }
    apply_loopback_state(ctrl, dsp, config_path, state).await
}

/// Runs the ALSA `hw:Loopback` rate-watcher task until the process shuts down.
pub async fn run(dsp: DspDevice) {
    let config_path = Path::new(DEFAULT_CAMILLADSP_CONFIG_PATH);
    let (tx, rx) = mpsc::channel::<(LoopbackState, bool)>(8);
    tokio::task::spawn_blocking(move || loopback_ctl_thread(&tx));
    run_with_rx(dsp, config_path, rx).await;
}

/// Drives the rate-watcher state machine from an injected `LoopbackState` receiver.
pub async fn run_with_rx(
    dsp: DspDevice,
    config_path: &Path,
    mut rx: mpsc::Receiver<(LoopbackState, bool)>,
) {
    let mut ctrl = RateWatchController::new();
    let mut last_state: Option<LoopbackState> = None;

    loop {
        let needs_retry =
            last_state.is_some_and(|s| s.active && ctrl.applied_rate() != Some(s.rate_hz));
        let reconcile_interval = if needs_retry {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(2)
        };
        tokio::select! {
            maybe_msg = rx.recv() => {
                let Some((state, from_event)) = maybe_msg else {
                    debug!("Loopback rate-watch channel closed; exiting");
                    break;
                };
                last_state = Some(state);
                apply_loopback_state_ext(&mut ctrl, &dsp, config_path, state, from_event).await;
            }
            () = tokio::time::sleep(reconcile_interval), if last_state.is_some_and(|s| s.active) => {
                if let Some(state) = last_state {
                    reconcile_with_engine(&mut ctrl, &dsp, config_path, state).await;
                }
            }
        }
    }
}

fn loopback_ctl_thread(tx: &mpsc::Sender<(LoopbackState, bool)>) {
    while !tx.is_closed() {
        let Some(ctl) = open_loopback_ctl() else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        poll_loopback_ctl(&ctl, tx);
    }
}

fn open_loopback_ctl() -> Option<Ctl> {
    // Open non-blocking so draining events with `ctl.read()` returns `Ok(None)`.
    let ctl = Ctl::new("hw:Loopback", true)
        .inspect_err(|e| warn!(error = %e, "Failed to open hw:Loopback ALSA ctl; retrying"))
        .ok()?;
    enable_pcm_notify(&ctl);
    if let Err(e) = ctl.subscribe_events(true) {
        warn!(error = %e, "Failed to subscribe to hw:Loopback ALSA ctl events");
        return None;
    }
    Some(ctl)
}

fn poll_loopback_ctl(ctl: &Ctl, tx: &mpsc::Sender<(LoopbackState, bool)>) {
    let mut prev: Option<LoopbackState> = None;
    let mut from_event = false;
    while !tx.is_closed() {
        if let Some(state) = read_loopback_state(ctl)
            && (prev != Some(state) || from_event)
        {
            if tx.blocking_send((state, from_event)).is_err() {
                return;
            }
            prev = Some(state);
        }
        from_event = match ctl.wait(Some(500)) {
            Ok(true) => {
                while let Ok(Some(_)) = ctl.read() {}
                true
            }
            Ok(false) => false,
            Err(e) => {
                warn!(error = %e, "hw:Loopback ALSA ctl wait failed; reopening");
                return;
            }
        };
    }
}

fn make_pcm_elem_id(name: &str) -> Option<ElemId> {
    let c_name = CString::new(name).ok()?;
    let mut id = ElemId::new(ElemIface::PCM);
    id.set_device(LOOPBACK_CAPTURE_DEVICE);
    id.set_subdevice(LOOPBACK_SUBDEVICE);
    id.set_name(&c_name);
    Some(id)
}

fn enable_pcm_notify(ctl: &Ctl) {
    let Some(notify_id) = make_pcm_elem_id("PCM Notify") else {
        return;
    };
    let Ok(mut val) = ElemValue::new(ElemType::Boolean) else {
        return;
    };
    val.set_id(&notify_id);
    if val.set_boolean(0, true).is_some()
        && let Err(e) = ctl.elem_write(&val)
    {
        warn!(error = %e, "Failed to enable PCM Notify on hw:Loopback device 1");
    }
}

fn read_loopback_state(ctl: &Ctl) -> Option<LoopbackState> {
    let active_id = make_pcm_elem_id("PCM Slave Active")?;
    let rate_id = make_pcm_elem_id("PCM Slave Rate")?;

    let mut active_val = ElemValue::new(ElemType::Boolean).ok()?;
    active_val.set_id(&active_id);
    ctl.elem_read(&mut active_val).ok()?;
    let active = active_val.get_boolean(0).unwrap_or(false);

    let mut rate_val = ElemValue::new(ElemType::Integer).ok()?;
    rate_val.set_id(&rate_id);
    ctl.elem_read(&mut rate_val).ok()?;
    let rate_hz = rate_val
        .get_integer(0)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);

    Some(LoopbackState { active, rate_hz })
}
