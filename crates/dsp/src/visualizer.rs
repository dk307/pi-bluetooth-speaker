//! Real-time visualizer telemetry streaming for `CamillaDSP` 5.0 (`SubscribeVuLevels` & `SubscribeSpectrum`).
//!
//! Each visualizer mode opens a dedicated WebSocket connection so asynchronous
//! telemetry events never interleave with control commands or another subscription.
//! Incoming frames are drained into a single [`LatestFrameSlot`] so a slow UI
//! frame drops older samples harmlessly instead of building TCP backpressure.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use camilladsp_schema::protocol::{
    SpectrumSide, SpectrumSubscription, VuSubscription, WsCommand, WsReply, WsResult,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use crate::client::{
    DspDevice, DspError, DspHost, DspPort, ProcessingState, check_ws_result, unexpected_reply,
};

/// Floor dBFS value used when no capture signal is available.
pub const SILENCE_DBFS: f32 = -100.0;

/// Single-slot latest-frame container that overwrites unread frames.
#[derive(Debug, Default)]
pub struct LatestFrameSlot<T> {
    inner: Mutex<Option<T>>,
}

impl<T> LatestFrameSlot<T> {
    /// Creates an empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Publishes a new frame, overwriting any unread previous frame.
    pub fn publish(&self, value: T) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(value);
        }
    }

    /// Takes the latest frame if one has been published since the last call.
    #[must_use]
    pub fn take(&self) -> Option<T> {
        self.inner.lock().ok().and_then(|mut g| g.take())
    }
}

/// Capture-side VU levels snapshot delivered by `SubscribeVuLevels`.
#[derive(Debug, Clone, PartialEq)]
pub struct VuLevelsSnapshot {
    /// Smoothed capture RMS levels per channel in dBFS.
    pub capture_rms: Vec<f32>,
    /// Smoothed capture peak levels per channel in dBFS.
    pub capture_peak: Vec<f32>,
}

impl VuLevelsSnapshot {
    /// Returns `(left, right)` capture RMS levels in dBFS.
    ///
    /// Duplicates channel 0 for mono streams and falls back to [`SILENCE_DBFS`]
    /// when no channels are present.
    #[must_use]
    pub fn capture_lr_rms(&self) -> (f32, f32) {
        extract_lr(&self.capture_rms)
    }

    /// Returns `(left, right)` capture peak levels in dBFS.
    #[must_use]
    pub fn capture_lr_peak(&self) -> (f32, f32) {
        extract_lr(&self.capture_peak)
    }
}

fn extract_lr(channels: &[f32]) -> (f32, f32) {
    match channels {
        [] => (SILENCE_DBFS, SILENCE_DBFS),
        [mono] => (*mono, *mono),
        [left, right, ..] => (*left, *right),
    }
}

/// Capture-side log-spaced spectrum snapshot delivered by `SubscribeSpectrum`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumSnapshot {
    /// Center frequencies of each bin in Hz. Empty when processing is stopped.
    pub frequencies: Arc<[f32]>,
    /// Peak magnitude per bin in dBFS.
    pub magnitudes: Vec<f32>,
}

impl SpectrumSnapshot {
    /// Creates an empty silent snapshot signaling that DSP processing stopped.
    #[must_use]
    pub fn silent() -> Self {
        Self {
            frequencies: Arc::from([]),
            magnitudes: Vec::new(),
        }
    }
}

/// Default `SubscribeVuLevels` configuration tuned for IEC 60268-17 needle feel at 40 Hz.
#[must_use]
pub const fn default_vu_subscription() -> VuSubscription {
    VuSubscription {
        max_rate: 40.0,
        attack: 65.0,
        release: 300.0,
    }
}

/// Default `SubscribeSpectrum` configuration querying pre-crossover capture audio.
#[must_use]
pub const fn default_spectrum_subscription() -> SpectrumSubscription {
    SpectrumSubscription {
        side: SpectrumSide::Capture,
        channel: None,
        min_freq: 30.0,
        max_freq: 18000.0,
        n_bins: 128,
        max_rate: Some(40.0),
    }
}

/// RAII handle for an active visualizer telemetry stream.
///
/// Dropping this handle signals the background reader task to send
/// `StopSubscription` and close its dedicated WebSocket connection.
#[derive(Debug)]
pub struct VisualizerSubscription {
    stop_tx: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl VisualizerSubscription {
    /// Explicitly stops the subscription and awaits clean WebSocket teardown.
    pub async fn stop(mut self) {
        let _ = self.stop_tx.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for VisualizerSubscription {
    fn drop(&mut self) {
        // Signal the reader task to send StopSubscription and close cleanly;
        // do not abort immediately so the WebSocket teardown can complete.
        let _ = self.stop_tx.send(true);
    }
}

async fn wait_for_stop(stop_rx: &mut watch::Receiver<bool>) {
    if *stop_rx.borrow() {
        return;
    }
    while stop_rx.changed().await.is_ok() {
        if *stop_rx.borrow() {
            return;
        }
    }
}

fn rate_hz_to_period_ms(max_rate_hz: f32) -> u64 {
    if !max_rate_hz.is_finite() || max_rate_hz <= 0.0 {
        return 25;
    }
    #[expect(
        clippy::cast_sign_loss,
        reason = "clamped to 16.0..=250.0 so value is positive and fits in u64"
    )]
    {
        (1000.0 / max_rate_hz).clamp(16.0, 250.0) as u64
    }
}

impl DspDevice {
    /// Opens a dedicated `SubscribeVuLevels` stream writing into `slot`.
    ///
    /// Automatically reconnects on transient disconnect until the returned
    /// [`VisualizerSubscription`] is dropped.
    #[must_use]
    pub fn start_vu_stream(
        &self,
        sub: VuSubscription,
        slot: Arc<LatestFrameSlot<VuLevelsSnapshot>>,
    ) -> VisualizerSubscription {
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let task = if let Some(mock) = self.is_mock().cloned() {
            let interval_ms = rate_hz_to_period_ms(sub.max_rate);
            tokio::spawn(async move {
                run_mock_vu_loop(mock, slot, Duration::from_millis(interval_ms), stop_rx).await;
            })
        } else if let Some((host, port)) = self.hardware_endpoint() {
            let host = host.clone();
            tokio::spawn(async move {
                run_hardware_vu_loop(host, port, sub, slot, stop_rx).await;
            })
        } else {
            tokio::spawn(async move {
                wait_for_stop(&mut stop_rx).await;
            })
        };

        VisualizerSubscription {
            stop_tx,
            task: Some(task),
        }
    }

    /// Opens a dedicated, state-aware `SubscribeSpectrum` stream writing into `slot`.
    ///
    /// Uses [`Self::subscribe_state`] to wait for [`ProcessingState::Running`]
    /// whenever `CamillaDSP` stops (silence timeout, pause, or sample-rate change)
    /// and emits [`SpectrumSnapshot::silent`] while inactive.
    #[must_use]
    pub fn start_spectrum_stream(
        &self,
        sub: SpectrumSubscription,
        slot: Arc<LatestFrameSlot<SpectrumSnapshot>>,
    ) -> VisualizerSubscription {
        let (stop_tx, stop_rx) = watch::channel(false);
        let dev = self.clone();
        let task = tokio::spawn(async move {
            run_resilient_spectrum_loop(dev, sub, slot, stop_rx).await;
        });

        VisualizerSubscription {
            stop_tx,
            task: Some(task),
        }
    }
}

async fn run_mock_vu_loop(
    mock: crate::client::DspDeviceMock,
    slot: Arc<LatestFrameSlot<VuLevelsSnapshot>>,
    period: Duration,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(period);
    loop {
        tokio::select! {
            () = wait_for_stop(&mut stop_rx) => break,
            _ = ticker.tick() => {
                let (capture_rms, capture_peak) = mock.levels_snapshot();
                slot.publish(VuLevelsSnapshot {
                    capture_rms,
                    capture_peak,
                });
            }
        }
    }
}

async fn run_hardware_vu_loop(
    host: DspHost,
    port: DspPort,
    sub: VuSubscription,
    slot: Arc<LatestFrameSlot<VuLevelsSnapshot>>,
    mut stop_rx: watch::Receiver<bool>,
) {
    loop {
        if *stop_rx.borrow() {
            break;
        }
        match subscribe_once_vu(&host, port, sub, &slot, &mut stop_rx).await {
            Ok(StoppedByCaller::Yes) => break,
            Ok(StoppedByCaller::No) => {}
            Err(err) => {
                tracing::debug!(error = %err, "VU stream connection ended; retrying");
            }
        }
        tokio::select! {
            () = wait_for_stop(&mut stop_rx) => break,
            () = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoppedByCaller {
    Yes,
    No,
}

async fn subscribe_once_vu(
    host: &DspHost,
    port: DspPort,
    sub: VuSubscription,
    slot: &LatestFrameSlot<VuLevelsSnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<StoppedByCaller, DspError> {
    let mut ws = DspDevice::connect_ws(host, port).await?;
    match DspDevice::send_ws_command(&mut ws, &WsCommand::SubscribeVuLevels { value: sub }).await? {
        WsReply::SubscribeVuLevels { result } => check_ws_result("SubscribeVuLevels", result)?,
        other => return Err(unexpected_reply("SubscribeVuLevels", &other)),
    }
    tracing::info!(%host, port = port.as_u16(), "Subscribed to CamillaDSP VuLevels stream");

    let mut logged_first = false;
    loop {
        tokio::select! {
            () = wait_for_stop(stop_rx) => {
                send_stop_and_close(&mut ws).await;
                return Ok(StoppedByCaller::Yes);
            }
            msg = ws.next() => {
                let Some(frame) = msg else {
                    return Ok(StoppedByCaller::No);
                };
                match frame {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<WsReply>(&text) {
                            Ok(WsReply::VuLevelsEvent { value, .. }) => {
                                if !logged_first {
                                    logged_first = true;
                                    tracing::info!(
                                        capture_rms = ?value.capture_rms,
                                        "Received first CamillaDSP VuLevelsEvent"
                                    );
                                }
                                slot.publish(VuLevelsSnapshot {
                                    capture_rms: value.capture_rms,
                                    capture_peak: value.capture_peak,
                                });
                            }
                            Ok(_) => {}
                            Err(err) => {
                                tracing::warn!(error = %err, "Failed to parse VuLevelsEvent frame");
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => return Ok(StoppedByCaller::No),
                    Ok(_) => {}
                }
            }
        }
    }
}

async fn run_resilient_spectrum_loop(
    dev: DspDevice,
    sub: SpectrumSubscription,
    slot: Arc<LatestFrameSlot<SpectrumSnapshot>>,
    mut stop_rx: watch::Receiver<bool>,
) {
    if let Some(mock) = dev.is_mock().cloned() {
        run_mock_spectrum_loop(mock, sub, slot, stop_rx).await;
        return;
    }
    let Some((host, port)) = dev.hardware_endpoint() else {
        wait_for_stop(&mut stop_rx).await;
        return;
    };
    let host = host.clone();

    loop {
        if *stop_rx.borrow() {
            break;
        }
        let Ok(mut state_rx) = dev.subscribe_state().await else {
            slot.publish(SpectrumSnapshot::silent());
            tokio::select! {
                () = wait_for_stop(&mut stop_rx) => break,
                () = tokio::time::sleep(Duration::from_millis(300)) => continue,
            }
        };

        // Wait until CamillaDSP reports Running before sending SubscribeSpectrum.
        let mut is_running = wait_for_running(&mut state_rx, &slot, &mut stop_rx).await;
        while is_running {
            match subscribe_once_spectrum(&host, port, &sub, &slot, &mut stop_rx).await {
                Ok(SpectrumExit::CallerStopped) => return,
                Ok(SpectrumExit::ProcessingStopped) => {
                    slot.publish(SpectrumSnapshot::silent());
                    is_running = wait_for_running(&mut state_rx, &slot, &mut stop_rx).await;
                }
                Err(err) => {
                    tracing::debug!(error = %err, "Spectrum stream disconnected; resetting state watch");
                    slot.publish(SpectrumSnapshot::silent());
                    tokio::select! {
                        () = wait_for_stop(&mut stop_rx) => return,
                        () = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                    break;
                }
            }
        }
        if *stop_rx.borrow() {
            break;
        }
        tokio::select! {
            () = wait_for_stop(&mut stop_rx) => break,
            () = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

async fn wait_for_running(
    state_rx: &mut tokio::sync::mpsc::Receiver<ProcessingState>,
    slot: &LatestFrameSlot<SpectrumSnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        tokio::select! {
            () = wait_for_stop(stop_rx) => return false,
            state = state_rx.recv() => {
                match state {
                    Some(ProcessingState::Running | ProcessingState::Paused) => return true,
                    Some(_) => slot.publish(SpectrumSnapshot::silent()),
                    None => return false,
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpectrumExit {
    CallerStopped,
    ProcessingStopped,
}

async fn subscribe_once_spectrum(
    host: &DspHost,
    port: DspPort,
    sub: &SpectrumSubscription,
    slot: &LatestFrameSlot<SpectrumSnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<SpectrumExit, DspError> {
    let mut ws = DspDevice::connect_ws(host, port).await?;
    let reply = DspDevice::send_ws_command(
        &mut ws,
        &WsCommand::SubscribeSpectrum {
            value: SpectrumSubscription {
                side: sub.side,
                channel: sub.channel,
                min_freq: sub.min_freq,
                max_freq: sub.max_freq,
                n_bins: sub.n_bins,
                max_rate: sub.max_rate,
            },
        },
    )
    .await?;

    match reply {
        WsReply::SubscribeSpectrum {
            result: WsResult::Ok,
        } => {}
        WsReply::SubscribeSpectrum {
            result: WsResult::ProcessingNotRunningError | WsResult::ProcessingStopped,
        } => {
            let _ = ws.close(None).await;
            return Ok(SpectrumExit::ProcessingStopped);
        }
        WsReply::SubscribeSpectrum { result } => check_ws_result("SubscribeSpectrum", result)?,
        other => return Err(unexpected_reply("SubscribeSpectrum", &other)),
    }
    tracing::info!(%host, port = port.as_u16(), "Subscribed to CamillaDSP Spectrum stream");

    let mut logged_first = false;
    loop {
        tokio::select! {
            () = wait_for_stop(stop_rx) => {
                send_stop_and_close(&mut ws).await;
                return Ok(SpectrumExit::CallerStopped);
            }
            msg = ws.next() => {
                let Some(frame) = msg else {
                    return Err(DspError::protocol_error("spectrum socket closed unexpectedly"));
                };
                match frame {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<WsReply>(&text) {
                            Ok(WsReply::SpectrumEvent { result, value }) => {
                                if matches!(result, WsResult::ProcessingStopped) {
                                    return Ok(SpectrumExit::ProcessingStopped);
                                }
                                if let Some(data) = value {
                                    if !logged_first {
                                        logged_first = true;
                                        tracing::info!(
                                            bins = data.frequencies.len(),
                                            "Received first CamillaDSP SpectrumEvent"
                                        );
                                    }
                                    slot.publish(SpectrumSnapshot {
                                        frequencies: data.frequencies,
                                        magnitudes: data.magnitudes,
                                    });
                                }
                            }
                            Ok(_) => {}
                            Err(err) => {
                                tracing::warn!(error = %err, "Failed to parse SpectrumEvent frame");
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => return Err(DspError::protocol_error("spectrum socket closed")),
                    Ok(_) => {}
                }
            }
        }
    }
}

async fn run_mock_spectrum_loop(
    mock: crate::client::DspDeviceMock,
    sub: SpectrumSubscription,
    slot: Arc<LatestFrameSlot<SpectrumSnapshot>>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let period_ms = sub.max_rate.map_or(25, rate_hz_to_period_ms);
    let freqs: Arc<[f32]> = build_log_frequencies(sub.min_freq, sub.max_freq, sub.n_bins).into();
    let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
    loop {
        tokio::select! {
            () = wait_for_stop(&mut stop_rx) => break,
            _ = ticker.tick() => {
                let (capture_rms, _) = mock.levels_snapshot();
                let avg_db = if capture_rms.is_empty() {
                    SILENCE_DBFS
                } else {
                    capture_rms.iter().sum::<f32>() / capture_rms.len() as f32
                };
                let magnitudes = vec![avg_db; freqs.len()];
                slot.publish(SpectrumSnapshot {
                    frequencies: Arc::clone(&freqs),
                    magnitudes,
                });
            }
        }
    }
}

fn build_log_frequencies(min_freq: f64, max_freq: f64, n_bins: usize) -> Vec<f32> {
    if n_bins == 0 {
        return Vec::new();
    }
    if n_bins == 1 {
        return vec![min_freq as f32];
    }
    let log_min = min_freq.max(1.0).ln();
    let log_max = max_freq.max(min_freq + 1.0).ln();
    let denom = (n_bins - 1) as f64;
    (0..n_bins)
        .map(|i| (log_min + (log_max - log_min) * (i as f64 / denom)).exp() as f32)
        .collect()
}

async fn send_stop_and_close<S>(ws: &mut S)
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    if let Ok(payload) = serde_json::to_string(&WsCommand::StopSubscription) {
        let _ = tokio::time::timeout(
            Duration::from_millis(300),
            ws.send(Message::Text(payload.into())),
        )
        .await;
    }
    let _ = tokio::time::timeout(Duration::from_millis(300), ws.send(Message::Close(None))).await;
}
