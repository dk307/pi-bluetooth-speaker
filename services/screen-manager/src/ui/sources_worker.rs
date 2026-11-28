//! Background workers for `org.speaker.AudioSources1` D-Bus routing and MPD (`rmpd`) monitoring.
//!
//! All D-Bus and MPD TCP I/O runs on dedicated worker threads (never on the Slint
//! render thread, per `M-NO-DBUS-ON-RENDER-THREAD`), publishing deltas into
//! [`crate::daemon::BtSharedUpdate`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use speaker_mpd::{MpdDevice, MpdPlaybackState, MpdStatus};
use zbus::{Connection, Proxy};

use crate::daemon::{BtSharedUpdate, COVER_DECODE_RETRY, decode_cover_bytes};
use crate::ui::now_playing::{
    AudioSourceEntry, AudioSourcesState, MpdCommand, MpdCommandQueue, SOURCE_ID_BLUETOOTH,
    SOURCE_ID_MPD, SourceCommand, SourceCommandQueue,
};

const AUDIO_MANAGER_DEST: &str = "org.speaker.AudioManager";
const AUDIO_SOURCES_PATH: &str = "/org/speaker/sources";
const AUDIO_SOURCES_IFACE: &str = "org.speaker.AudioSources1";

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PLAYING_POSITION_TICK: Duration = Duration::from_secs(1);
const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(2);

type RawSourceTuple = (String, String, String, u8, u32, bool);

fn default_mock_sources() -> AudioSourcesState {
    AudioSourcesState {
        active_source: Some(Box::from(SOURCE_ID_BLUETOOTH)),
        pinned_source: None,
        sources: vec![
            AudioSourceEntry {
                id: Box::from(SOURCE_ID_BLUETOOTH),
                display_name: Box::from("Bluetooth"),
                state: Box::from("idle"),
                sample_rate_hz: 0,
                is_active: true,
            },
            AudioSourceEntry {
                id: Box::from(SOURCE_ID_MPD),
                display_name: Box::from("Music Player"),
                state: Box::from("idle"),
                sample_rate_hz: 0,
                is_active: false,
            },
        ],
    }
}

fn publish_audio_sources(shared: &Arc<Mutex<BtSharedUpdate>>, state: AudioSourcesState) {
    let mut g = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.audio_sources = Some(state);
}

fn publish_mpd_status(shared: &Arc<Mutex<BtSharedUpdate>>, status: MpdStatus) {
    let mut g = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.mpd_playback = Some(status);
}

fn publish_mpd_cover(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    cover: Option<crate::daemon::CoverPixelBuffer>,
    seed: Option<u32>,
) {
    let mut g = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.mpd_cover = Some(cover);
    g.mpd_cover_seed = Some(seed);
}

fn sleep_interruptible(duration: Duration, running: &AtomicBool) {
    let mut remaining = duration;
    while !remaining.is_zero() && running.load(Ordering::Relaxed) {
        let slice = remaining.min(Duration::from_millis(100));
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

async fn sleep_async_interruptible(duration: Duration, running: &AtomicBool) {
    let mut remaining = duration;
    while !remaining.is_zero() && running.load(Ordering::Relaxed) {
        let slice = remaining.min(Duration::from_millis(200));
        tokio::time::sleep(slice).await;
        remaining = remaining.saturating_sub(slice);
    }
}

/// Spawns the `audio-sources` D-Bus worker thread.
///
/// # Errors
/// Returns an [`std::io::Error`] if spawning the OS thread fails.
pub fn spawn_audio_sources_worker(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &Arc<AtomicBool>,
    commands: &SourceCommandQueue,
    mocked: bool,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let shared = Arc::clone(shared);
    let running = Arc::clone(running);
    let commands = commands.clone();
    std::thread::Builder::new()
        .name("audio-sources".into())
        .spawn(move || {
            if mocked {
                run_mock_audio_sources(&shared, &running, &commands);
                return;
            }
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("Failed to build tokio runtime for audio-sources worker");
                return;
            };
            rt.block_on(run_live_audio_sources(&shared, &running, &commands));
        })
}

fn run_mock_audio_sources(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &AtomicBool,
    commands: &SourceCommandQueue,
) {
    let mut state = default_mock_sources();
    publish_audio_sources(shared, state.clone());
    while running.load(Ordering::Relaxed) {
        let mut changed = false;
        for cmd in commands.drain() {
            match cmd {
                SourceCommand::SelectSource(id) => {
                    if state.sources.iter().any(|s| s.id == id) {
                        state.active_source = Some(id.clone());
                        for s in &mut state.sources {
                            s.is_active = s.id == id;
                        }
                        changed = true;
                    }
                }
                SourceCommand::PinSource(pin) => {
                    state.pinned_source = pin;
                    changed = true;
                }
            }
        }
        if changed {
            publish_audio_sources(shared, state.clone());
        }
        std::thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

async fn read_sources_snapshot(proxy: &Proxy<'_>) -> zbus::Result<AudioSourcesState> {
    let active_raw: String = proxy.get_property("ActiveSource").await?;
    let pinned_raw: String = proxy.get_property("PinnedSource").await?;
    let tuples: Vec<RawSourceTuple> = proxy.get_property("Sources").await?;

    let active_source = (!active_raw.is_empty()).then(|| Box::from(active_raw.as_str()));
    let pinned_source = (!pinned_raw.is_empty()).then(|| Box::from(pinned_raw.as_str()));
    let sources = tuples
        .into_iter()
        .map(
            |(id, display_name, state, _priority, sample_rate_hz, is_active)| AudioSourceEntry {
                id: id.into_boxed_str(),
                display_name: display_name.into_boxed_str(),
                state: state.into_boxed_str(),
                sample_rate_hz,
                is_active,
            },
        )
        .collect();

    Ok(AudioSourcesState {
        active_source,
        pinned_source,
        sources,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "D-Bus reconnect and select! event loop"
)]
async fn run_live_audio_sources(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &AtomicBool,
    commands: &SourceCommandQueue,
) {
    let mut backoff = Duration::from_secs(1);
    while running.load(Ordering::Relaxed) {
        let conn = match tokio::time::timeout(DBUS_CALL_TIMEOUT, Connection::system()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "System D-Bus unavailable; retrying audio-sources connection"
                );
                sleep_interruptible(backoff, running);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
            Err(_) => {
                tracing::warn!("System D-Bus connect timed out; retrying audio-sources connection");
                sleep_interruptible(backoff, running);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        let proxy = match tokio::time::timeout(
            DBUS_CALL_TIMEOUT,
            Proxy::new(
                &conn,
                AUDIO_MANAGER_DEST,
                AUDIO_SOURCES_PATH,
                AUDIO_SOURCES_IFACE,
            ),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Failed to build AudioSources1 proxy");
                sleep_interruptible(backoff, running);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
            Err(_) => {
                tracing::warn!("Timed out building AudioSources1 proxy");
                sleep_interruptible(backoff, running);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        let mut has_initial_snapshot =
            match tokio::time::timeout(DBUS_CALL_TIMEOUT, read_sources_snapshot(&proxy)).await {
                Ok(Ok(initial)) => {
                    publish_audio_sources(shared, initial);
                    backoff = Duration::from_secs(1);
                    true
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Initial AudioSources1 snapshot failed; will retry");
                    false
                }
                Err(_) => {
                    tracing::warn!("Initial AudioSources1 snapshot timed out; will retry");
                    false
                }
            };

        let props_proxy = match zbus::fdo::PropertiesProxy::builder(&conn)
            .destination(AUDIO_MANAGER_DEST)
            .and_then(|b| b.path(AUDIO_SOURCES_PATH))
        {
            Ok(b) => match b.build().await {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to build PropertiesProxy for AudioSources1");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "Invalid AudioSources1 PropertiesProxy target");
                None
            }
        };

        let mut changes = match props_proxy {
            Some(ref p) => p.receive_properties_changed().await.ok(),
            None => None,
        };

        let mut cmd_tick = tokio::time::interval(COMMAND_POLL_INTERVAL);
        let mut fallback_poll = tokio::time::interval(Duration::from_secs(5));

        while running.load(Ordering::Relaxed) {
            tokio::select! {
                _ = cmd_tick.tick() => {
                    for cmd in commands.drain() {
                        execute_source_command(&proxy, shared, cmd, &mut has_initial_snapshot).await;
                    }
                }
                maybe_sig = async {
                    match changes.as_mut() {
                        Some(stream) => stream.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if maybe_sig.is_none() {
                        break;
                    }
                    if let Ok(snap) = read_sources_snapshot(&proxy).await {
                        publish_audio_sources(shared, snap);
                        has_initial_snapshot = true;
                    }
                }
                _ = fallback_poll.tick() => {
                    if (!has_initial_snapshot || changes.is_none())
                        && let Ok(snap) = read_sources_snapshot(&proxy).await
                    {
                        publish_audio_sources(shared, snap);
                        has_initial_snapshot = true;
                    }
                }
            }
        }
    }
}

async fn execute_source_command(
    proxy: &Proxy<'_>,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    cmd: SourceCommand,
    has_initial_snapshot: &mut bool,
) {
    let res = match cmd {
        SourceCommand::SelectSource(id) => tokio::time::timeout(
            DBUS_CALL_TIMEOUT,
            proxy.call_method("SelectSource", &(id.as_ref(),)),
        )
        .await
        .map(|r| r.map(|_| ())),
        SourceCommand::PinSource(pin) => {
            let arg = pin.as_deref().unwrap_or("");
            tokio::time::timeout(DBUS_CALL_TIMEOUT, proxy.call_method("PinSource", &(arg,)))
                .await
                .map(|r| r.map(|_| ()))
        }
    };
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "AudioSources1 command failed"),
        Err(_) => tracing::warn!("AudioSources1 command timed out; refreshing state"),
    }
    if let Ok(Ok(snap)) =
        tokio::time::timeout(DBUS_CALL_TIMEOUT, read_sources_snapshot(proxy)).await
    {
        publish_audio_sources(shared, snap);
        *has_initial_snapshot = true;
    }
}

/// Spawns the MPD (`rmpd`) monitor and transport worker thread.
///
/// # Errors
/// Returns an [`std::io::Error`] if spawning the OS thread fails.
pub fn spawn_mpd_monitor_worker(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &Arc<AtomicBool>,
    commands: &MpdCommandQueue,
    mocked: bool,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let device = if mocked {
        MpdDevice::new_mocked().0
    } else {
        MpdDevice::new()
    };
    spawn_mpd_monitor_with_device(shared, running, commands, device)
}

/// Spawns the MPD monitor worker against an explicit [`MpdDevice`] (useful for tests).
///
/// # Errors
/// Returns an [`std::io::Error`] if spawning the OS thread fails.
pub fn spawn_mpd_monitor_with_device(
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &Arc<AtomicBool>,
    commands: &MpdCommandQueue,
    device: MpdDevice,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let shared = Arc::clone(shared);
    let running = Arc::clone(running);
    let commands = commands.clone();
    std::thread::Builder::new()
        .name("mpd-monitor".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("Failed to build tokio runtime for mpd-monitor worker");
                return;
            };
            rt.block_on(run_mpd_worker(&device, &shared, &running, &commands));
        })
}

async fn execute_mpd_command(
    device: &MpdDevice,
    cmd: MpdCommand,
    current_state: MpdPlaybackState,
) -> MpdPlaybackState {
    let (res, next_state) = match cmd {
        MpdCommand::TogglePlayPause => match current_state {
            MpdPlaybackState::Playing => (device.pause().await, MpdPlaybackState::Paused),
            MpdPlaybackState::Paused => (device.resume().await, MpdPlaybackState::Playing),
            MpdPlaybackState::Stopped => (device.play().await, MpdPlaybackState::Playing),
        },
        MpdCommand::Next => (device.next().await, current_state),
        MpdCommand::Previous => (device.previous().await, current_state),
        MpdCommand::Seek(pos) => (device.seek(pos).await, current_state),
        MpdCommand::SetRepeat(en) => (device.set_repeat(en).await, current_state),
        MpdCommand::SetRandom(en) => (device.set_random(en).await, current_state),
    };
    if let Err(e) = res {
        tracing::warn!(error = %e.summary(), ?cmd, "MPD transport command failed");
        current_state
    } else {
        next_state
    }
}

async fn refresh_mpd_cover(
    device: &MpdDevice,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    last_art_file: &mut Option<Box<str>>,
    last_failed_art: &mut Option<(Box<str>, Instant)>,
    status: &MpdStatus,
) {
    let next_file: Option<Box<str>> = status
        .current_track
        .as_ref()
        .map(|t| t.file.clone())
        .filter(|f| !f.is_empty());
    if *last_art_file == next_file {
        return;
    }
    let Some(ref file) = next_file else {
        *last_art_file = None;
        *last_failed_art = None;
        publish_mpd_cover(shared, None, None);
        return;
    };
    if let Some((failed_file, at)) = last_failed_art
        && failed_file == file
        && at.elapsed() < COVER_DECODE_RETRY
    {
        return;
    }
    match device.read_album_art(file.as_ref()).await {
        Ok(Some(bytes)) => {
            if let Some((buf, seed)) = decode_cover_bytes(&bytes) {
                last_art_file.clone_from(&next_file);
                *last_failed_art = None;
                publish_mpd_cover(shared, Some(buf), seed);
            } else {
                *last_failed_art = Some((file.clone(), Instant::now()));
                tracing::debug!(file = %file, "MPD album art decode failed; keeping prior art");
            }
        }
        Ok(None) => {
            last_art_file.clone_from(&next_file);
            *last_failed_art = None;
            publish_mpd_cover(shared, None, None);
        }
        Err(e) => {
            *last_failed_art = Some((file.clone(), Instant::now()));
            tracing::debug!(error = %e.summary(), file = %file, "MPD album art fetch failed");
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "MPD reconnect and select! event loop"
)]
async fn run_mpd_worker(
    device: &MpdDevice,
    shared: &Arc<Mutex<BtSharedUpdate>>,
    running: &AtomicBool,
    commands: &MpdCommandQueue,
) {
    let mut backoff = Duration::from_secs(1);
    let mut last_status: Option<MpdStatus> = None;
    let mut last_art_file: Option<Box<str>> = None;
    let mut last_failed_art: Option<(Box<str>, Instant)> = None;

    while running.load(Ordering::Relaxed) {
        let mut events = match device.subscribe_events().await {
            Ok(ev) => {
                backoff = Duration::from_secs(1);
                Some(ev)
            }
            Err(e) => {
                tracing::debug!(
                    error = %e.summary(),
                    backoff_ms = backoff.as_millis() as u64,
                    "MPD unavailable; retrying mpd-monitor connection"
                );
                None
            }
        };

        if events.is_none() {
            // Publish an idle/stopped snapshot when MPD drops so the UI does
            // not keep showing a playing track for an unreachable server.
            if let Some(prev) = last_status.take() {
                last_art_file = None;
                last_failed_art = None;
                publish_mpd_cover(shared, None, None);
                publish_mpd_status(
                    shared,
                    MpdStatus {
                        state: MpdPlaybackState::Stopped,
                        elapsed: None,
                        duration: None,
                        current_track: None,
                        bitrate_kbps: None,
                        sample_rate_hz: None,
                        bit_depth: None,
                        channels: None,
                        ..prev
                    },
                );
            }
            // Drain any stale queued commands while disconnected, then wait.
            let _ = commands.drain();
            sleep_async_interruptible(backoff, running).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        }

        if let Ok(st) = device.read_status().await {
            refresh_mpd_cover(
                device,
                shared,
                &mut last_art_file,
                &mut last_failed_art,
                &st,
            )
            .await;
            publish_mpd_status(shared, st.clone());
            last_status = Some(st);
        }

        let mut cmd_tick = tokio::time::interval(COMMAND_POLL_INTERVAL);
        let mut pos_tick = tokio::time::interval(PLAYING_POSITION_TICK);

        while running.load(Ordering::Relaxed) {
            tokio::select! {
                _ = cmd_tick.tick() => {
                    let pending = commands.drain();
                    if !pending.is_empty() {
                        let mut cur_state = last_status
                            .as_ref()
                            .map_or(MpdPlaybackState::Stopped, |s| s.state);
                        for cmd in pending {
                            cur_state = execute_mpd_command(device, cmd, cur_state).await;
                        }
                        if let Ok(st) = device.read_status().await {
                            refresh_mpd_cover(device, shared, &mut last_art_file, &mut last_failed_art, &st).await;
                            publish_mpd_status(shared, st.clone());
                            last_status = Some(st);
                        }
                    }
                }
                _ = pos_tick.tick() => {
                    let should_poll = last_status.as_ref().is_none_or(|s| {
                        matches!(s.state, MpdPlaybackState::Playing)
                            || last_failed_art
                                .as_ref()
                                .is_some_and(|(_, at)| at.elapsed() >= COVER_DECODE_RETRY)
                    });
                    if should_poll && let Ok(st) = device.read_status().await {
                        let art_retry_due = last_failed_art
                            .as_ref()
                            .is_some_and(|(_, at)| at.elapsed() >= COVER_DECODE_RETRY);
                        if last_status.as_ref() != Some(&st) || art_retry_due {
                            refresh_mpd_cover(device, shared, &mut last_art_file, &mut last_failed_art, &st).await;
                        }
                        if last_status.as_ref() != Some(&st) {
                            publish_mpd_status(shared, st.clone());
                            last_status = Some(st);
                        }
                    }
                }
                ev = async {
                    match events.as_mut() {
                        Some(stream) => stream.recv().await,
                        None => None,
                    }
                } => {
                    match ev {
                        Some(speaker_mpd::MpdEvent::Disconnected(reason)) => {
                            tracing::debug!(reason = %reason, "MPD event stream disconnected");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            break;
                        }
                        Some(speaker_mpd::MpdEvent::SubsystemChanged(_)) => {
                            if let Ok(st) = device.read_status().await {
                                refresh_mpd_cover(device, shared, &mut last_art_file, &mut last_failed_art, &st).await;
                                publish_mpd_status(shared, st.clone());
                                last_status = Some(st);
                            }
                        }
                        None => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Joins a background worker thread, re-raising any panic (`M-PANIC-ON-BUG`).
pub fn join_worker(res: std::io::Result<std::thread::JoinHandle<()>>, label: &str) {
    match res {
        Ok(handle) => {
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, worker = label, "Worker thread could not be started");
        }
    }
}
