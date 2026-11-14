#![cfg(feature = "ui")]
//! Coverage tests for the boot probe set, including the real `CamillaDSP` probe
//! driven against a hermetic fake WebSocket server on loopback.
//!
//! The remaining hardware probe closures (`probe_amp`, `probe_bluetooth`,
//! `probe_sensors`) are deliberately NOT driven to success: they open real
//! I2C/sysfs/BlueZ endpoints (their failure paths are exercised through the
//! daemon's splash runs).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use screen_manager::ui::BootProbe;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

fn instant_probe_set() -> BootProbe {
    BootProbe::from_probes(std::array::from_fn(|_| {
        Box::new(|| Ok("ok".into())) as Box<dyn FnOnce() -> Result<Box<str>, Box<str>> + Send>
    }))
}

#[test]
fn boot_probe_debug_reports_probe_count() {
    let probes = instant_probe_set();
    let debug = format!("{probes:?}");
    assert!(debug.contains("BootProbe"), "{debug}");
    assert!(debug.contains("probe_count: 5"), "{debug}");
}

#[test]
fn boot_probe_real_debug_reports_probe_count() {
    // Only the `Debug` formatter runs here; the probe closures are never executed.
    let debug = format!("{:?}", BootProbe::real());
    assert!(debug.contains("probe_count: 5"), "{debug}");
}

// ---------------------------------------------------------------------------
// Hermetic fake CamillaDSP WebSocket server (default appliance endpoint
// `127.0.0.1:1234`) driving the real `probe_dsp` closure to both outcomes.
// ---------------------------------------------------------------------------

/// Engine state observed by the fake server (`GetState` response).
const DSP_PAUSED: u8 = 0;
const DSP_RUNNING: u8 = 1;

/// `CamillaDSP` wire envelope: `{ "<cmd>": { "result": "Ok"|"Error", "value": … } }`.
fn envelope(cmd: &str, result: &str, value: Value) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("result".into(), json!(result));
    body.insert("value".into(), value);
    let mut outer = serde_json::Map::new();
    outer.insert(cmd.to_string(), Value::Object(body));
    Value::Object(outer)
}

/// Serves the `CamillaDSP` command set on loopback until `shutdown` is raised.
/// Connections are handled sequentially, matching the probe's one-shot usage.
fn spawn_fake_camilladsp(
    engine_state: Arc<AtomicU8>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fake CamillaDSP tokio runtime");
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 1234))
                .await
                .expect("bind CamillaDSP default endpoint 127.0.0.1:1234");
            while !shutdown.load(Ordering::Relaxed) {
                // Bounded accept so the loop can observe the shutdown flag.
                let accepted =
                    tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                let Ok(Ok((stream, _))) = accepted else {
                    continue;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    match msg {
                        Message::Text(text) => {
                            let cmd: String = serde_json::from_str(&text).unwrap_or_default();
                            let response = match cmd.as_str() {
                                "GetState" => envelope(
                                    &cmd,
                                    "Ok",
                                    json!(if engine_state.load(Ordering::Relaxed) == DSP_RUNNING {
                                        "Running"
                                    } else {
                                        "Paused"
                                    }),
                                ),
                                "GetProcessingLoad" => envelope(&cmd, "Ok", json!(3.5)),
                                "GetRateAdjust" => envelope(&cmd, "Ok", json!(1.0)),
                                "GetBufferLevel" => envelope(&cmd, "Ok", json!(12_000_u64)),
                                "GetClippedSamples" => envelope(&cmd, "Ok", json!(0_u64)),
                                "GetStopReason" => envelope(&cmd, "Ok", json!("None")),
                                "GetSignalLevels" => envelope(
                                    &cmd,
                                    "Ok",
                                    json!({
                                        "capture_peak": [0.2],
                                        "capture_rms": [0.1],
                                        "playback_peak": [0.3],
                                        "playback_rms": [0.15],
                                    }),
                                ),
                                "GetVersion" => envelope(&cmd, "Ok", json!("4.1.3")),
                                "GetConfigTitle" => envelope(&cmd, "Ok", json!("boot-coverage")),
                                "GetConfigFilePath" => {
                                    envelope(&cmd, "Ok", json!("/tmp/boot-coverage.toml"))
                                }
                                "GetResamplerLoad" => envelope(&cmd, "Ok", json!(0.02)),
                                "GetCaptureRate" => envelope(&cmd, "Ok", json!(48_000_u64)),
                                "GetVolume" => envelope(&cmd, "Ok", json!(-1.5)),
                                "GetMute" => envelope(&cmd, "Ok", json!(false)),
                                _ => envelope(
                                    &cmd,
                                    "Error",
                                    json!("unsupported command in fake server"),
                                ),
                            };
                            if ws
                                .send(Message::Text(response.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            }
        });
    })
}

#[test]
fn real_dsp_probe_reports_both_engine_states_against_fake_server() {
    use screen_manager::ui::boot::{BootCheckId, BootCheckStatus};

    let engine_state = Arc::new(AtomicU8::new(DSP_PAUSED));
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = spawn_fake_camilladsp(Arc::clone(&engine_state), Arc::clone(&shutdown));

    let collect = || {
        let mut dsp_lines: Vec<(BootCheckStatus, Box<str>)> = Vec::new();
        BootProbe::real().run(&mut |update| {
            if update.id == BootCheckId::Dsp {
                dsp_lines.push((update.status, update.text));
            }
        });
        dsp_lines
    };

    // Paused engine: the probe must fail with the non-operational state detail.
    let paused = collect();
    assert_eq!(paused.len(), 2, "running marker plus outcome");
    assert_eq!(paused[0].0, BootCheckStatus::Running);
    assert_eq!(paused[1].0, BootCheckStatus::Failed, "paused must fail");
    assert!(paused[1].1.contains("state:"), "{:?}", paused[1].1);

    // Running engine: the probe must report operational telemetry.
    engine_state.store(DSP_RUNNING, Ordering::Relaxed);
    let running = collect();
    assert_eq!(running.len(), 2, "running marker plus outcome");
    assert_eq!(running[1].0, BootCheckStatus::Passed, "running must pass");
    assert!(running[1].1.contains("v4.1.3 | load"), "{:?}", running[1].1);

    shutdown.store(true, Ordering::Relaxed);
    server.join().expect("fake CamillaDSP thread");
}
