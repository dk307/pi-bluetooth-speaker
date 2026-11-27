//! ALSA Loopback sample-rate and capture-device watcher for dynamic `CamillaDSP` routing.
//!
//! Monitors `hw:Loopback` (`PCM Slave Active` and `PCM Slave Rate` on device 1
//! across all configured subdevices) and enables `PCM Notify` so kernel events
//! fire immediately when any source opens, closes, or changes sample rate.
//!
//! Whenever the active source changes or changes sample rate, `rate_watch`
//! parses and validates the base `CamillaDSP` YAML config via `camilladsp-schema`,
//! updates `devices.capture.device` (when specified), `devices.samplerate`, and
//! `devices.chunksize`, and uploads the validated JSON via `SetConfigJson`.

use std::collections::HashMap;
use std::ffi::CString;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use alsa::ctl::{Ctl, ElemId, ElemIface, ElemType, ElemValue};
use camilladsp_schema::config::{CaptureDevice, parse_config, validate_config};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use speaker_dsp::{DspDevice, ProcessingState};

/// Default `CamillaDSP` YAML configuration file read as the base template.
pub const DEFAULT_CAMILLADSP_CONFIG_PATH: &str = "/etc/camilladsp/crossover.yml";

/// Default capture device for single-source `rate_watch` callers.
const DEFAULT_LOOPBACK_CAPTURE_DEVICE: &str = "hw:Loopback,1,0";

/// Capture-side ALSA Loopback device index (`hw:Loopback,1,*`).
///
/// In `snd-aloop`, the `PCM Slave *` controls on device `1` reflect the state
/// of the playback writer on device `0` (`hw:Loopback,0,*`).
const LOOPBACK_CAPTURE_DEVICE: u32 = 1;

/// Snapshot of one `hw:Loopback` subdevice playback-writer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackState {
    /// `true` when `PCM Slave Active` is non-zero (a player holds `hw:Loopback,0,{N}`).
    pub active: bool,
    /// Current `PCM Slave Rate` in Hz reported by `snd-aloop`.
    pub rate_hz: u32,
}

/// Desired `CamillaDSP` capture device and sample-rate routing target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspRouteRequest {
    /// Target ALSA capture device string (e.g., `hw:Loopback,1,1`).
    pub capture_device: Box<str>,
    /// Target sample rate in Hz (`> 0`).
    pub rate_hz: u32,
    /// `true` when the active source is currently streaming.
    pub streaming: bool,
    /// `true` to push config even when `streaming == false` (detach from prior source).
    pub force_apply: bool,
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

/// Parses a `CamillaDSP` YAML config via `camilladsp-schema`, optionally updates
/// `devices.capture.device`, updates `devices.samplerate` and `devices.chunksize`,
/// validates against `CamillaDSP` 5.0 schema rules, and returns JSON for `SetConfigJson`.
///
/// # Errors
/// Returns an error string if the YAML fails schema parsing, if `capture_device`
/// is provided when `devices.capture` is not ALSA, if `rate_hz` or `chunksize` is
/// zero, or if `camilladsp-schema` validation fails.
pub fn build_reconfigured_config_json_for(
    yaml: &str,
    capture_device: Option<&str>,
    rate_hz: u32,
    chunksize: u32,
) -> Result<String, String> {
    let rate = NonZeroUsize::new(rate_hz as usize)
        .ok_or_else(|| "sample rate must be non-zero".to_owned())?;
    let chunk = NonZeroUsize::new(chunksize as usize)
        .ok_or_else(|| "chunksize must be non-zero".to_owned())?;

    let mut cfg = parse_config(yaml).map_err(|e| format!("schema parse error: {e}"))?;
    if let Some(target_dev) = capture_device {
        let CaptureDevice::Alsa { device, .. } = &mut cfg.devices.capture else {
            return Err("CamillaDSP capture device is not ALSA".to_owned());
        };
        *device = target_dev.to_owned();
    }
    cfg.devices.samplerate = rate;
    cfg.devices.chunksize = chunk;

    validate_config(&mut cfg, None).map_err(|e| format!("schema validation error: {e}"))?;

    serde_json::to_string(&cfg).map_err(|e| format!("JSON serialization error: {e}"))
}

/// Legacy single-device wrapper over [`build_reconfigured_config_json_for`].
///
/// # Errors
/// Propagates [`build_reconfigured_config_json_for`] errors.
pub fn build_reconfigured_config_json(
    yaml: &str,
    rate_hz: u32,
    chunksize: u32,
) -> Result<String, String> {
    build_reconfigured_config_json_for(yaml, None, rate_hz, chunksize)
}

/// Pure state machine deciding when to push a new `(capture_device, rate_hz)` config to `CamillaDSP`.
#[derive(Debug, Default)]
pub struct RateWatchController {
    applied: Option<(Box<str>, u32)>,
    warned_failure: bool,
}

impl RateWatchController {
    /// Creates a new controller with no applied route yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            applied: None,
            warned_failure: false,
        }
    }

    /// Evaluates a [`DspRouteRequest`] and returns `Some((device, rate_hz))` when
    /// `CamillaDSP` should be configured.
    pub fn pending_route(&mut self, req: &DspRouteRequest) -> Option<(Box<str>, u32)> {
        if req.rate_hz == 0 {
            self.applied = None;
            self.warned_failure = false;
            return None;
        }
        if !req.streaming {
            self.applied = None;
            self.warned_failure = false;
            if req.force_apply {
                return Some((req.capture_device.clone(), req.rate_hz));
            }
            return None;
        }
        if self.matches(&req.capture_device, req.rate_hz) {
            None
        } else {
            Some((req.capture_device.clone(), req.rate_hz))
        }
    }

    /// Records that `(capture_device, rate_hz)` was successfully applied to `CamillaDSP`.
    pub fn confirm_route_applied(&mut self, capture_device: &str, rate_hz: u32) {
        self.applied = Some((Box::from(capture_device), rate_hz));
        self.warned_failure = false;
    }

    /// Clears the cached applied route so the next check forces a re-push.
    pub fn invalidate(&mut self) {
        self.applied = None;
    }

    /// Returns `true` when `applied` matches `(capture_device, rate_hz)`.
    #[must_use]
    pub fn matches(&self, capture_device: &str, rate_hz: u32) -> bool {
        self.applied
            .as_ref()
            .is_some_and(|(dev, r)| dev.as_ref() == capture_device && *r == rate_hz)
    }

    /// Returns the currently applied sample rate, if any.
    #[must_use]
    pub fn applied_rate(&self) -> Option<u32> {
        self.applied.as_ref().map(|(_, rate)| *rate)
    }

    /// Returns the currently applied capture device, if any.
    #[must_use]
    pub fn applied_device(&self) -> Option<&str> {
        self.applied.as_ref().map(|(dev, _)| dev.as_ref())
    }

    /// Returns `true` on the first failure in a streak (for `warn!`), then `false` (`debug!`).
    fn should_warn_failure(&mut self) -> bool {
        if self.warned_failure {
            false
        } else {
            self.warned_failure = true;
            true
        }
    }
}

/// Applies a [`DspRouteRequest`] to `dsp` using `config_path` as the base YAML template.
///
/// When `force_if_inactive` is `true` and `ctrl` believes the target is already applied,
/// checks `CamillaDSP`'s live state and re-applies if the engine went `Inactive`.
pub async fn apply_route_request(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    req: &DspRouteRequest,
    force_if_inactive: bool,
) -> bool {
    if force_if_inactive
        && req.streaming
        && ctrl.matches(&req.capture_device, req.rate_hz)
        && matches!(dsp.read_state_async().await, Ok(ProcessingState::Inactive))
    {
        ctrl.invalidate();
    }

    let Some((target_device, rate_hz)) = ctrl.pending_route(req) else {
        return false;
    };

    let raw_yaml = match tokio::fs::read_to_string(config_path).await {
        Ok(contents) => contents,
        Err(e) => {
            if ctrl.should_warn_failure() {
                warn!(path = %config_path.display(), error = %e, "Failed to read CamillaDSP base config; will retry");
            } else {
                debug!(path = %config_path.display(), error = %e, "Failed to read CamillaDSP base config; retrying");
            }
            return false;
        }
    };

    let chunksize = chunksize_for_rate(rate_hz);
    let config_json = match build_reconfigured_config_json_for(
        &raw_yaml,
        Some(&target_device),
        rate_hz,
        chunksize,
    ) {
        Ok(json) => json,
        Err(e) => {
            if ctrl.should_warn_failure() {
                warn!(path = %config_path.display(), error = %e, "Failed to build/validate CamillaDSP config via camilladsp-schema");
            } else {
                debug!(path = %config_path.display(), error = %e, "Failed to build/validate CamillaDSP config via camilladsp-schema");
            }
            return false;
        }
    };

    match dsp.set_config_json_async(&config_json).await {
        Ok(()) => {
            if req.streaming {
                ctrl.confirm_route_applied(&target_device, rate_hz);
            } else {
                // Detached onto an idle source; clear applied so the next active stream re-arms cleanly.
                ctrl.invalidate();
            }
            info!(
                capture_device = %target_device,
                rate_hz,
                chunksize,
                streaming = req.streaming,
                path = %config_path.display(),
                "Configured CamillaDSP capture route and sample rate"
            );
            true
        }
        Err(e) => {
            if ctrl.should_warn_failure() {
                warn!(
                    capture_device = %target_device,
                    rate_hz,
                    error = %e,
                    "Failed to configure CamillaDSP route/rate; will retry"
                );
            } else {
                debug!(
                    capture_device = %target_device,
                    rate_hz,
                    error = %e,
                    "Failed to configure CamillaDSP route/rate; retrying"
                );
            }
            false
        }
    }
}

/// Applies a [`LoopbackState`] update to `dsp` on default subdevice 0.
pub async fn apply_loopback_state(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    state: LoopbackState,
) -> bool {
    let req = DspRouteRequest {
        capture_device: Box::from(DEFAULT_LOOPBACK_CAPTURE_DEVICE),
        rate_hz: state.rate_hz,
        streaming: state.active,
        force_apply: false,
    };
    apply_route_request(ctrl, dsp, config_path, &req, false).await
}

/// Reconciles `ctrl` against `CamillaDSP`'s live status when a stream is active,
/// recovering if `camilladsp.service` restarted with its static config.
///
/// Skips divergence checks while `status.state == ProcessingState::Starting` because
/// `SetConfigJson` applies asynchronously in `CamillaDSP` 5.0.
pub async fn reconcile_route_with_engine(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    req: &DspRouteRequest,
) -> bool {
    if !req.streaming || req.rate_hz == 0 {
        return false;
    }
    if ctrl.matches(&req.capture_device, req.rate_hz) {
        let status = match dsp.read_status_async().await {
            Ok(s) => s,
            Err(e) => {
                debug!(error = %e, "Skipping engine route reconcile; DSP status unavailable");
                return false;
            }
        };
        if status.state == ProcessingState::Starting {
            debug!("Skipping engine route reconcile while CamillaDSP is Starting");
            return false;
        }
        let live_rate = status.config.as_ref().and_then(|c| c.samplerate);
        let live_device = status
            .config
            .as_ref()
            .and_then(|c| c.capture_device.as_deref());
        let rate_mismatch = live_rate.is_some_and(|r| r != req.rate_hz);
        let device_mismatch = live_device.is_some_and(|d| d != req.capture_device.as_ref());
        if status.state != ProcessingState::Inactive && !rate_mismatch && !device_mismatch {
            return false;
        }
        warn!(
            expected_device = %req.capture_device,
            expected_rate = req.rate_hz,
            ?live_device,
            ?live_rate,
            engine_state = %status.state,
            "CamillaDSP diverged from active route; reconfiguring"
        );
        ctrl.invalidate();
    }
    apply_route_request(ctrl, dsp, config_path, req, false).await
}

/// Legacy single-subdevice reconciliation wrapper over [`reconcile_route_with_engine`].
pub async fn reconcile_with_engine(
    ctrl: &mut RateWatchController,
    dsp: &DspDevice,
    config_path: &Path,
    state: LoopbackState,
) -> bool {
    let req = DspRouteRequest {
        capture_device: Box::from(DEFAULT_LOOPBACK_CAPTURE_DEVICE),
        rate_hz: state.rate_hz,
        streaming: state.active,
        force_apply: false,
    };
    reconcile_route_with_engine(ctrl, dsp, config_path, &req).await
}

/// Spawns the `hw:Loopback` ALSA ctl thread monitoring `subdevices` and returns its receiver + join handle.
#[must_use]
pub fn spawn_subdevice_watcher(
    subdevices: Vec<u32>,
) -> (
    mpsc::Receiver<(u32, LoopbackState, bool)>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel::<(u32, LoopbackState, bool)>(32);
    let handle = tokio::task::spawn_blocking(move || loopback_ctl_thread(&subdevices, &tx));
    (rx, handle)
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
                let req = DspRouteRequest {
                    capture_device: Box::from(DEFAULT_LOOPBACK_CAPTURE_DEVICE),
                    rate_hz: state.rate_hz,
                    streaming: state.active,
                    force_apply: false,
                };
                apply_route_request(&mut ctrl, &dsp, config_path, &req, from_event).await;
            }
            () = tokio::time::sleep(reconcile_interval), if last_state.is_some_and(|s| s.active) => {
                if let Some(state) = last_state {
                    reconcile_with_engine(&mut ctrl, &dsp, config_path, state).await;
                }
            }
        }
    }
}

fn loopback_ctl_thread(subdevices: &[u32], tx: &mpsc::Sender<(u32, LoopbackState, bool)>) {
    while !tx.is_closed() {
        let Some(ctl) = open_loopback_ctl(subdevices) else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        poll_loopback_ctl(&ctl, subdevices, tx);
    }
}

fn open_loopback_ctl(subdevices: &[u32]) -> Option<Ctl> {
    // Open non-blocking so draining events with `ctl.read()` returns `Ok(None)`.
    let ctl = Ctl::new("hw:Loopback", true)
        .inspect_err(|e| warn!(error = %e, "Failed to open hw:Loopback ALSA ctl; retrying"))
        .ok()?;
    for &sub in subdevices {
        enable_pcm_notify(&ctl, sub);
    }
    if let Err(e) = ctl.subscribe_events(true) {
        warn!(error = %e, "Failed to subscribe to hw:Loopback ALSA ctl events");
        return None;
    }
    Some(ctl)
}

fn poll_loopback_ctl(ctl: &Ctl, subdevices: &[u32], tx: &mpsc::Sender<(u32, LoopbackState, bool)>) {
    let mut prev: HashMap<u32, LoopbackState> = HashMap::with_capacity(subdevices.len());
    let mut from_event = false;
    while !tx.is_closed() {
        for &sub in subdevices {
            if let Some(state) = read_loopback_state(ctl, sub)
                && (prev.get(&sub) != Some(&state) || from_event)
            {
                if tx.blocking_send((sub, state, from_event)).is_err() {
                    return;
                }
                prev.insert(sub, state);
            }
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

fn make_pcm_elem_id(name: &str, subdevice: u32) -> Option<ElemId> {
    let c_name = CString::new(name).ok()?;
    let mut id = ElemId::new(ElemIface::PCM);
    id.set_device(LOOPBACK_CAPTURE_DEVICE);
    id.set_subdevice(subdevice);
    id.set_name(&c_name);
    Some(id)
}

fn enable_pcm_notify(ctl: &Ctl, subdevice: u32) {
    let Some(notify_id) = make_pcm_elem_id("PCM Notify", subdevice) else {
        return;
    };
    let Ok(mut val) = ElemValue::new(ElemType::Boolean) else {
        return;
    };
    val.set_id(&notify_id);
    if val.set_boolean(0, true).is_some()
        && let Err(e) = ctl.elem_write(&val)
    {
        warn!(subdevice, error = %e, "Failed to enable PCM Notify on hw:Loopback device 1");
    }
}

fn read_loopback_state(ctl: &Ctl, subdevice: u32) -> Option<LoopbackState> {
    let active_id = make_pcm_elem_id("PCM Slave Active", subdevice)?;
    let rate_id = make_pcm_elem_id("PCM Slave Rate", subdevice)?;

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
