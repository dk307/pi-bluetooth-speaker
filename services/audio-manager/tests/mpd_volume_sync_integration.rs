//! Integration tests for bidirectional MPD ↔ Amp volume sync.

use std::time::{Duration, Instant};

use audio_manager::amp_controller::{AmpCommand, AmpCommandKind};
use audio_manager::{
    AMP_PUSH_ECHO_SETTLE, AudioSourcesSnapshot, AutoSwitchPolicy, FRESH_MPD_SOURCE_SETTLE,
    MpdVolumeSyncState, SourceId, run_mpd_volume_sync,
};
use speaker_mpd::{MpdDevice, MpdEvent, MpdMockCommand, MpdSubsystem};
use tokio::sync::{mpsc, watch};

fn sid(raw: &str) -> SourceId {
    SourceId::parse(raw).unwrap()
}

#[test]
fn mpd_volume_sync_state_gates_settle_echo_and_direction() {
    let t0 = Instant::now();
    let mut state = MpdVolumeSyncState::default();
    assert!(!state.source_active());
    assert!(!state.should_forward_to_amp(50, t0));
    assert!(!state.should_push_to_mpd(50));

    // Switch-in arms settle window and requests initial amp -> MPD reconcile.
    assert!(state.set_source_active(true, t0));
    assert!(state.source_active());
    assert!(!state.set_source_active(true, t0 + Duration::from_millis(10)));

    // Suppressed during FRESH_MPD_SOURCE_SETTLE.
    assert!(!state.should_forward_to_amp(40, t0 + Duration::from_millis(500)));

    // Uncommitted query does not mutate state (transient I/O failure remains retryable).
    let t1 = t0 + FRESH_MPD_SOURCE_SETTLE + Duration::from_millis(1);
    assert!(state.should_push_to_mpd(50));
    assert!(state.should_push_to_mpd(50));

    // Committed rapid amp -> MPD pushes (50 -> 54) suppress echoes both during
    // AMP_PUSH_ECHO_SETTLE and on duplicate `last_synced` values afterward.
    state.record_reverse_pushed(50, t1);
    assert!(state.should_push_to_mpd(54));
    state.record_reverse_pushed(54, t1);
    assert!(!state.should_forward_to_amp(50, t1 + Duration::from_millis(50)));
    let t2 = t1 + AMP_PUSH_ECHO_SETTLE + Duration::from_millis(1);
    assert!(!state.should_forward_to_amp(54, t2));

    // Uncommitted forward query remains retryable; once committed, deduplicates & suppresses echo.
    assert!(state.should_forward_to_amp(72, t2));
    assert!(state.should_forward_to_amp(72, t2));
    state.record_forward_applied(72);
    assert!(!state.should_forward_to_amp(72, t2));
    assert!(!state.should_push_to_mpd(72));

    // arm_reconnect clears last_synced and re-arms FRESH_MPD_SOURCE_SETTLE.
    state.arm_reconnect(t2);
    assert!(state.should_push_to_mpd(72));
    assert!(!state.should_forward_to_amp(80, t2 + Duration::from_millis(500)));
    let t3 = t2 + FRESH_MPD_SOURCE_SETTLE + Duration::from_millis(1);
    assert!(state.should_forward_to_amp(80, t3));

    // Switching away clears active gate.
    assert!(!state.set_source_active(false, t3));
    assert!(!state.should_forward_to_amp(30, t3));
    assert!(!state.should_push_to_mpd(30));
}

#[tokio::test]
async fn mpd_volume_sync_loop_syncs_bidirectionally_on_active_source() {
    let (mpd, mpd_mock) = MpdDevice::new_mocked();
    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::LastPlayWins);
    snapshot.update(
        Some(sid("bluetooth")),
        None,
        AutoSwitchPolicy::LastPlayWins,
        Vec::new(),
    );

    let (amp_cmd_tx, mut amp_cmd_rx) = mpsc::channel::<AmpCommand>(8);
    let (level_tx, level_rx) = watch::channel(45u8);

    let handle = tokio::spawn(run_mpd_volume_sync(
        sid("rmpd"),
        mpd,
        snapshot.clone(),
        amp_cmd_tx,
        level_rx,
        None,
    ));

    // While bluetooth is active, MPD mixer events and amp level changes are ignored.
    tokio::time::sleep(Duration::from_millis(30)).await;
    mpd_mock.set_volume(Some(80));
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    level_tx.send(50).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(amp_cmd_rx.try_recv().is_err());
    assert!(mpd_mock.commands().is_empty());

    // Switching ActiveSource to rmpd pushes current amp volume (50) -> MPD.
    snapshot.update(
        Some(sid("rmpd")),
        None,
        AutoSwitchPolicy::LastPlayWins,
        Vec::new(),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if mpd_mock.commands().contains(&MpdMockCommand::SetVolume(50)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("switch-in should push amp volume 50 to MPD");

    // Amp level change while rmpd is active pushes SetVolume(60) to MPD.
    level_tx.send(60).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if mpd_mock.commands().contains(&MpdMockCommand::SetVolume(60)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("amp level change should push SetVolume(60) to MPD");

    // Wait out FRESH_MPD_SOURCE_SETTLE + AMP_PUSH_ECHO_SETTLE, then emit a burst of
    // 10 Mixer events -> coalesced into a single SetLevel(72) -> AmpController.
    tokio::time::sleep(FRESH_MPD_SOURCE_SETTLE + AMP_PUSH_ECHO_SETTLE + Duration::from_millis(50))
        .await;
    mpd_mock.set_volume(Some(72));
    for _ in 0..10 {
        mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    }

    let cmd = tokio::time::timeout(Duration::from_secs(1), amp_cmd_rx.recv())
        .await
        .expect("MPD mixer event should forward SetLevel(72) to amp")
        .unwrap();
    assert_eq!(cmd.kind, AmpCommandKind::SetLevel(72));
    if let Some(reply) = cmd.reply {
        let _ = reply.send(Ok(()));
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        amp_cmd_rx.try_recv().is_err(),
        "burst of queued Mixer events must coalesce into a single SetLevel"
    );

    // Disconnect while rmpd is active triggers reconnect and reconciles amp volume (60) -> MPD.
    mpd_mock.clear_commands();
    mpd_mock.emit_event(MpdEvent::Disconnected("eof".into()));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mpd_mock.commands().contains(&MpdMockCommand::SetVolume(60)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("MPD reconnect while active must reconcile amp volume -> MPD");

    drop(amp_cmd_rx);
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn mpd_volume_sync_handles_initial_active_faults_and_dbus_signal() {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;

    use audio_manager::{AmpState, AudioManagerInterface, StateFile};
    use futures_util::StreamExt;
    use speaker_common::diag::ScopedTempDir;
    use zbus::{MatchRule, MessageStream};

    struct BusGuard(Child);
    impl Drop for BusGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn private dbus-daemon");
    let mut addr = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut addr)
        .unwrap();
    let addr = addr.trim().to_owned();
    let _bus = BusGuard(child);

    let server_conn = zbus::connection::Builder::address(addr.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let client_conn = zbus::connection::Builder::address(addr.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();

    let (amp_cmd_tx, mut amp_cmd_rx) = mpsc::channel::<AmpCommand>(8);
    let tmp = ScopedTempDir::new("mpd_vol_sync_dbus");
    let mirror = Arc::new(tokio::sync::Mutex::new(AmpState::default()));
    let iface = AudioManagerInterface::with_commands(
        mirror,
        StateFile::new(tmp.path().join("amp.json")),
        amp_cmd_tx.clone(),
    );
    server_conn
        .object_server()
        .at("/org/speaker/amp", iface)
        .await
        .unwrap();

    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .unwrap()
        .member("PropertiesChanged")
        .unwrap()
        .path("/org/speaker/amp")
        .unwrap()
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &client_conn, Some(8))
        .await
        .unwrap();

    // Start with rmpd ALREADY active and an injected MPD fault to exercise
    // initial subscribe_events backoff retry + post-connect reconcile.
    let (mpd, mpd_mock) = MpdDevice::new_mocked();
    mpd_mock.inject_fault("mpd starting up");
    let snapshot = AudioSourcesSnapshot::new(AutoSwitchPolicy::LastPlayWins);
    snapshot.update(
        Some(sid("rmpd")),
        None,
        AutoSwitchPolicy::LastPlayWins,
        Vec::new(),
    );
    let (level_tx, level_rx) = watch::channel(40u8);

    let handle = tokio::spawn(run_mpd_volume_sync(
        sid("rmpd"),
        mpd,
        snapshot.clone(),
        amp_cmd_tx,
        level_rx,
        Some(server_conn),
    ));

    // Wait briefly so first subscribe attempt fails, then clear fault and await SetVolume(40).
    tokio::time::sleep(Duration::from_millis(80)).await;
    mpd_mock.clear_fault();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mpd_mock.commands().contains(&MpdMockCommand::SetVolume(40)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial connect after fault clear must push SetVolume(40)");

    // Wait past FRESH_MPD_SOURCE_SETTLE (2s).
    tokio::time::sleep(FRESH_MPD_SOURCE_SETTLE + Duration::from_millis(100)).await;

    // Non-mixer event is ignored.
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Player));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(amp_cmd_rx.try_recv().is_err());

    // Fault during amp -> MPD level push is tolerated; since it failed, retrying same
    // level (44) later succeeds rather than being deduplicated away.
    mpd_mock.inject_fault("transient write error");
    level_tx.send(44).unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    mpd_mock.clear_fault();

    // 1. MPD -> Amp succeeds and emits D-Bus Level PropertiesChanged.
    mpd_mock.set_volume(Some(68));
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    let cmd = tokio::time::timeout(Duration::from_secs(1), amp_cmd_rx.recv())
        .await
        .expect("SetLevel(68) expected")
        .unwrap();
    assert_eq!(cmd.kind, AmpCommandKind::SetLevel(68));
    cmd.reply.unwrap().send(Ok(())).unwrap();

    let sig = tokio::time::timeout(Duration::from_secs(1), signals.next())
        .await
        .expect("D-Bus PropertiesChanged expected")
        .unwrap()
        .unwrap();
    assert_eq!(sig.header().member().unwrap().as_str(), "PropertiesChanged");

    // 2. Amp rejects MPD volume update -> last_synced is NOT updated, so retrying 70 works.
    mpd_mock.set_volume(Some(70));
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    let cmd2 = tokio::time::timeout(Duration::from_secs(1), amp_cmd_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cmd2.kind, AmpCommandKind::SetLevel(70));
    cmd2.reply.unwrap().send(Err("i2c fault".into())).unwrap();

    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));
    let cmd2_retry = tokio::time::timeout(Duration::from_secs(1), amp_cmd_rx.recv())
        .await
        .expect("failed SetLevel(70) must remain retryable")
        .unwrap();
    assert_eq!(cmd2_retry.kind, AmpCommandKind::SetLevel(70));
    drop(cmd2_retry.reply);
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Closing level_tx exits the loop cleanly.
    drop(level_tx);
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}
