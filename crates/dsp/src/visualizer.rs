//! Real-time visualizer telemetry streaming for `CamillaDSP` 5.0 (`SubscribeSpectrum`).
//!
//! Opens a dedicated WebSocket connection so asynchronous spectrum events never
//! interleave with control commands. Incoming frames are drained into a single
//! [`LatestFrameSlot`] so a slow UI frame drops older samples harmlessly instead
//! of building TCP backpressure.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use camilladsp_schema::protocol::{
    SpectrumSide, SpectrumSubscription, WsCommand, WsReply, WsResult,
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
        let mut guard = self.inner.lock().unwrap_or_else(|p| {
            tracing::warn!("LatestFrameSlot mutex poisoned during publish; recovering");
            p.into_inner()
        });
        *guard = Some(value);
    }

    /// Takes the latest frame if one has been published since the last call.
    #[must_use]
    pub fn take(&self) -> Option<T> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| {
            tracing::warn!("LatestFrameSlot mutex poisoned during take; recovering");
            p.into_inner()
        });
        guard.take()
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

/// Builds a `SubscribeSpectrum` configuration querying pre-crossover capture audio
/// capped at `max_rate_hz`.
#[must_use]
pub const fn spectrum_subscription_for_rate(max_rate_hz: f32) -> SpectrumSubscription {
    SpectrumSubscription {
        side: SpectrumSide::Capture,
        channel: None,
        min_freq: 30.0,
        max_freq: 18000.0,
        n_bins: 128,
        max_rate: Some(max_rate_hz),
    }
}

/// Default `SubscribeSpectrum` configuration querying pre-crossover capture audio at 40 Hz.
#[must_use]
pub const fn default_spectrum_subscription() -> SpectrumSubscription {
    spectrum_subscription_for_rate(40.0)
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
        if let Some(task) = self.task.take()
            && let Err(err) = task.await
            && err.is_panic()
        {
            std::panic::resume_unwind(err.into_panic());
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
    /// Opens a dedicated, state-aware `SubscribeSpectrum` stream writing into `slot`.
    ///
    /// Uses [`Self::subscribe_state`] to wait for [`ProcessingState::Running`]
    /// or [`ProcessingState::Paused`] whenever `CamillaDSP` stops (rate change
    /// or shutdown) and emits [`SpectrumSnapshot::silent`] while inactive.
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
        let mut state_rx = match dev.subscribe_state().await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to subscribe to DSP state for spectrum; retrying");
                slot.publish(SpectrumSnapshot::silent());
                tokio::select! {
                    () = wait_for_stop(&mut stop_rx) => break,
                    () = tokio::time::sleep(Duration::from_millis(300)) => continue,
                }
            }
        };

        let mut is_running = wait_for_running(&mut state_rx, &slot, &mut stop_rx).await;
        while is_running {
            match subscribe_once_spectrum(&host, port, &sub, &slot, &mut stop_rx).await {
                Ok(SpectrumExit::CallerStopped) => return,
                Ok(SpectrumExit::ProcessingStopped) => {
                    slot.publish(SpectrumSnapshot::silent());
                    is_running = wait_for_running(&mut state_rx, &slot, &mut stop_rx).await;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "Spectrum stream disconnected; resetting state watch");
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
    let mut ws = tokio::select! {
        () = wait_for_stop(stop_rx) => return Ok(SpectrumExit::CallerStopped),
        res = DspDevice::connect_ws(host, port) => res?,
    };
    let cmd = WsCommand::SubscribeSpectrum {
        value: SpectrumSubscription {
            side: sub.side,
            channel: sub.channel,
            min_freq: sub.min_freq,
            max_freq: sub.max_freq,
            n_bins: sub.n_bins,
            max_rate: sub.max_rate,
        },
    };
    let reply = tokio::select! {
        () = wait_for_stop(stop_rx) => {
            let _ = ws.close(None).await;
            return Ok(SpectrumExit::CallerStopped);
        }
        res = DspDevice::send_ws_command(&mut ws, &cmd) => res?,
    };

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
