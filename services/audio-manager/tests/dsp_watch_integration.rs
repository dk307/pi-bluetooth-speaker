//! Integration tests for the DSP stream-lifecycle watcher (plan §11.3):
//! pure state-machine folding plus the full poll loop over the mocked
//! `DspDevice` seam (M-MOCKABLE-SYSCALLS).

use std::time::Duration;

use audio_manager::amp_controller::{AmpCommand, AmpCommandKind};
use audio_manager::dsp_watch::{EmittedLifecycle, LifecycleTracker, run};
use speaker_dsp::{DspDevice, DspDeviceMock, ProcessingState};

// ---------------------------------------------------------------- tracker

#[test]
fn first_poll_running_emits_stream_starting_once() {
    let mut t = LifecycleTracker::new();
    assert_eq!(
        t.step(&ProcessingState::Running),
        Some(AmpCommandKind::StreamStarting)
    );
    assert_eq!(t.step(&ProcessingState::Running), None, "edges only");
    assert_eq!(t.lifecycle(), EmittedLifecycle::Running);
}

#[test]
fn first_poll_paused_then_inactive_emits_nothing() {
    let mut t = LifecycleTracker::new();
    assert_eq!(t.step(&ProcessingState::Paused), None);
    assert_eq!(t.step(&ProcessingState::Inactive), None);
    assert_eq!(t.lifecycle(), EmittedLifecycle::Stopped);
}

#[test]
fn stalled_needs_two_consecutive_polls_to_stop() {
    let mut t = LifecycleTracker::new();
    assert_eq!(
        t.step(&ProcessingState::Running),
        Some(AmpCommandKind::StreamStarting)
    );
    assert_eq!(t.step(&ProcessingState::Stalled), None, "1-poll debounce");
    assert_eq!(t.lifecycle(), EmittedLifecycle::Running);
    assert_eq!(
        t.step(&ProcessingState::Stalled),
        Some(AmpCommandKind::StreamStopping)
    );
    assert_eq!(t.lifecycle(), EmittedLifecycle::Stopped);
}

#[test]
fn transient_stalled_bounce_recovers_without_events() {
    let mut t = LifecycleTracker::new();
    assert_eq!(
        t.step(&ProcessingState::Running),
        Some(AmpCommandKind::StreamStarting)
    );
    assert_eq!(t.step(&ProcessingState::Stalled), None);
    assert_eq!(
        t.step(&ProcessingState::Running),
        None,
        "no restart edge mid-play"
    );
    // Still Running: a later real stop emits exactly one stop edge.
    assert_eq!(
        t.step(&ProcessingState::Paused),
        Some(AmpCommandKind::StreamStopping)
    );
}

#[test]
fn starting_and_unknown_hold_the_last_lifecycle() {
    let mut t = LifecycleTracker::new();
    assert_eq!(t.step(&ProcessingState::Starting), None);
    assert_eq!(t.lifecycle(), EmittedLifecycle::Stopped);
    assert_eq!(t.step(&ProcessingState::Unknown("weird".into())), None);
    assert_eq!(
        t.step(&ProcessingState::Running),
        Some(AmpCommandKind::StreamStarting)
    );
    // Mid-play config reload: Starting/Unknown never flap the amp.
    assert_eq!(t.step(&ProcessingState::Starting), None);
    assert_eq!(t.step(&ProcessingState::Unknown("other".into())), None);
    assert_eq!(t.lifecycle(), EmittedLifecycle::Running);
    // And the eventual real stop still emits exactly once.
    assert_eq!(
        t.step(&ProcessingState::Inactive),
        Some(AmpCommandKind::StreamStopping)
    );
}

#[test]
fn stalled_to_paused_collapse_emits_one_stop() {
    let mut t = LifecycleTracker::new();
    assert_eq!(
        t.step(&ProcessingState::Running),
        Some(AmpCommandKind::StreamStarting)
    );
    assert_eq!(t.step(&ProcessingState::Stalled), None);
    assert_eq!(
        t.step(&ProcessingState::Stalled),
        Some(AmpCommandKind::StreamStopping)
    );
    assert_eq!(
        t.step(&ProcessingState::Paused),
        None,
        "2-state collapse: no re-edge"
    );
    assert_eq!(t.step(&ProcessingState::Inactive), None);
}

// ------------------------------------------------------------------- loop

/// Small test poll interval (production intervals are config-driven, never
/// `cfg(test)`-shrunk; tests inject their own).
const TEST_INTERVAL: Duration = Duration::from_millis(60);

struct LoopFixture {
    device: DspDevice,
    mock: DspDeviceMock,
    cmd_tx: tokio::sync::mpsc::Sender<AmpCommand>,
    cmd_rx: tokio::sync::mpsc::Receiver<AmpCommand>,
}

fn loop_fixture() -> LoopFixture {
    let (device, mock) = DspDevice::new_mocked();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<AmpCommand>(8);
    LoopFixture {
        device,
        mock,
        cmd_tx,
        cmd_rx,
    }
}

/// Drain the received command kinds without blocking.
async fn received(rx: &mut tokio::sync::mpsc::Receiver<AmpCommand>) -> Vec<AmpCommandKind> {
    let mut kinds = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        kinds.push(cmd.kind);
    }
    kinds
}

#[tokio::test]
async fn loop_first_poll_running_emits_starting_then_edges_once() {
    let LoopFixture {
        device,
        mock,
        cmd_tx,
        mut cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Running);
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));

    let started = std::time::Instant::now();
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("watcher must emit StreamStarting on first Running poll")
        .expect("channel open");
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));
    assert!(started.elapsed() < Duration::from_secs(1));

    // Continued Running polls: no duplicate edges.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        received(&mut cmd_rx).await.is_empty(),
        "no duplicate StreamStarting while Running"
    );

    // Stop the watcher via channel close (receiver owned by controller).
    // Dropping our sender AND receiver ends the loop.
    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher must exit when the command channel closes")
        .unwrap();
}

#[tokio::test]
async fn loop_running_to_paused_emits_both_edges_exactly_once() {
    let LoopFixture {
        device,
        mock,
        cmd_tx,
        mut cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Running);
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));

    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStarting")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));

    mock.set_state(ProcessingState::Paused);
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStopping")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStopping));

    // Paused → Inactive collapse: no further edges.
    mock.set_state(ProcessingState::Inactive);
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        received(&mut cmd_rx).await.is_empty(),
        "Paused/Inactive must collapse to a single Stopped edge"
    );

    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher must exit")
        .unwrap();
}

#[tokio::test]
async fn loop_stalled_stops_only_after_two_consecutive_polls() {
    let LoopFixture {
        device,
        mock,
        cmd_tx,
        mut cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Running);
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));

    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStarting")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));

    // One Stalled poll: debounce holds, no edge. Synchronize with the
    // poll cadence instead of a fixed sleep: the assertion window ends
    // BEFORE the next poll can fire (the watcher polls every
    // TEST_INTERVAL; run() floors the effective interval at 50 ms and
    // TEST_INTERVAL sits above that floor).
    mock.set_state(ProcessingState::Stalled);
    let within_one_poll = tokio::time::timeout(
        TEST_INTERVAL.saturating_sub(Duration::from_millis(15)),
        cmd_rx.recv(),
    )
    .await;
    assert!(
        within_one_poll.is_err(),
        "single Stalled must not stop (got an edge within one poll window)"
    );

    // Second consecutive Stalled: stop edge fires.
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("confirmed Stalled must emit StreamStopping")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStopping));

    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher must exit")
        .unwrap();
}

#[tokio::test]
async fn loop_telemetry_failure_holds_lifecycle_then_resumes() {
    let LoopFixture {
        device,
        mock,
        cmd_tx,
        mut cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Running);
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));

    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStarting")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));

    // WS failure with a state change underneath: must NOT emit a stop
    // (§4.5 hold-last-lifecycle).
    mock.inject_fault("simulated websocket failure");
    mock.set_state(ProcessingState::Paused);
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        received(&mut cmd_rx).await.is_empty(),
        "telemetry failure must hold the last lifecycle"
    );

    // Recovery: the next successful read folds the (changed) state and
    // emits the pending edge exactly once.
    mock.clear_fault();
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("recovered read must emit the stop edge")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStopping));

    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher must exit")
        .unwrap();
}

#[tokio::test]
async fn loop_exits_when_command_channel_closes() {
    let LoopFixture {
        device,
        mock,
        cmd_tx,
        cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Paused); // no edges ever
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));
    tokio::time::sleep(Duration::from_millis(60)).await;
    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher must exit when the command channel closes")
        .unwrap();
}

#[test]
fn stalled_unknown_stalled_counts_as_consecutive() {
    // Documented behavior: `Hold` (Unknown/Starting) does NOT reset the
    // Stalled debounce counter — Stalled → Unknown → Stalled emits the
    // stop. Conservative and safe (mutes on a possibly-dead stream); the
    // engine reported capture starvation twice within the hold window.
    let mut t = LifecycleTracker::new();
    assert_eq!(
        t.step(&ProcessingState::Running),
        Some(AmpCommandKind::StreamStarting)
    );
    assert_eq!(t.step(&ProcessingState::Stalled), None);
    assert_eq!(t.step(&ProcessingState::Unknown("blip".into())), None);
    assert_eq!(
        t.step(&ProcessingState::Stalled),
        Some(AmpCommandKind::StreamStopping),
        "Unknown between two Stalled polls does not reset the debounce"
    );
}

// ------------------------------------------------- adaptive poll intervals

#[test]
fn poll_interval_halves_while_running_and_floors_at_minimum() {
    use audio_manager::dsp_watch::poll_interval;
    use std::time::Duration;

    // Stopped: the configured idle interval, never below the floor.
    assert_eq!(
        poll_interval(Duration::from_millis(500), EmittedLifecycle::Stopped),
        Duration::from_millis(500)
    );
    assert_eq!(
        poll_interval(Duration::from_millis(50), EmittedLifecycle::Stopped),
        poll_interval(Duration::from_millis(50), EmittedLifecycle::Stopped)
    );
    assert_eq!(
        poll_interval(Duration::from_millis(10), EmittedLifecycle::Stopped),
        audio_manager::dsp_watch::MIN_POLL_INTERVAL,
        "idle below MIN floor: clamped to MIN for Stopped too"
    );

    // Running: half the idle interval.
    assert_eq!(
        poll_interval(Duration::from_millis(2000), EmittedLifecycle::Running),
        Duration::from_millis(1000)
    );
    // Running below the floor: clamped to MIN.
    assert_eq!(
        poll_interval(Duration::from_millis(100), EmittedLifecycle::Running),
        audio_manager::dsp_watch::MIN_POLL_INTERVAL,
        "half-of-idle below the floor must clamp to MIN_POLL_INTERVAL"
    );
}

#[tokio::test]
async fn loop_telemetry_failure_holds_lifecycle_and_recovers() {
    use std::time::Duration;

    let LoopFixture {
        device,
        mock,
        cmd_tx,
        mut cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Running);
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));

    // First poll: Running -> StreamStarting edge.
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStarting edge")
        .expect("channel open");
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));

    // Telemetry goes dark: the last lifecycle is HELD (no flap edges).
    mock.inject_fault("ws connection lost");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        received(&mut cmd_rx).await.is_empty(),
        "telemetry failure must never emit an edge"
    );

    // Telemetry recovers: still Running, still no edge; then a real stop
    // emits exactly one StreamStopping.
    mock.clear_fault();
    mock.set_state(ProcessingState::Inactive);
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStopping edge after recovery")
        .expect("channel open");
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStopping));

    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher exits when the channel closes")
        .unwrap();
}

#[tokio::test]
async fn loop_reissues_starting_while_running_until_confirmed() {
    use std::time::Duration;

    let LoopFixture {
        device,
        mock,
        cmd_tx,
        mut cmd_rx,
    } = loop_fixture();
    mock.set_state(ProcessingState::Running);
    let task = tokio::spawn(run(device, cmd_tx.clone(), TEST_INTERVAL));

    // First poll: StreamStarting.
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStarting")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));

    // The DSP stays Running; the watcher re-issues StreamStarting on a
    // bounded cadence so a failed controller handshake is retried (a
    // resend on an already-playing controller is a no-op).
    let cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
        .await
        .expect("retry StreamStarting must arrive while DSP stays Running")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStarting));

    // Leaving Running resets the retry budget: exactly one StreamStopping.
    mock.set_state(ProcessingState::Inactive);
    let cmd = tokio::time::timeout(Duration::from_secs(2), cmd_rx.recv())
        .await
        .expect("StreamStopping")
        .unwrap();
    assert!(matches!(cmd.kind, AmpCommandKind::StreamStopping));

    drop(cmd_tx);
    drop(cmd_rx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("watcher exits on channel close")
        .unwrap();
}
