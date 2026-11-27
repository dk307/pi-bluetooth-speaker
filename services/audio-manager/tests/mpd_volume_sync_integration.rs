//! Integration tests for bidirectional MPD ↔ Amp volume sync.

use std::time::{Duration, Instant};

use audio_manager::amp_controller::{AmpCommand, AmpCommandKind};
use audio_manager::{
    AudioSourcesSnapshot, AutoSwitchPolicy, FRESH_MPD_SOURCE_SETTLE, MpdVolumeSyncState, SourceId,
    run_mpd_volume_sync,
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

    // Amp -> MPD push records echo so MPD's reflected mixer event is ignored.
    assert!(state.should_push_to_mpd(65));
    let t1 = t0 + FRESH_MPD_SOURCE_SETTLE + Duration::from_millis(1);
    assert!(!state.should_forward_to_amp(65, t1));

    // Genuine MPD mixer change forwards to amp, deduplicates, and suppresses amp echo.
    assert!(state.should_forward_to_amp(72, t1));
    assert!(!state.should_forward_to_amp(72, t1));
    assert!(!state.should_push_to_mpd(72));

    // Subsequent amp knob change pushes to MPD and bounds echo queue.
    for v in 10..25 {
        assert!(state.should_push_to_mpd(v));
    }
    assert!(!state.should_forward_to_amp(24, t1));

    // Switching away clears active gate.
    assert!(!state.set_source_active(false, t1));
    assert!(!state.should_forward_to_amp(30, t1));
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

    // Wait out FRESH_MPD_SOURCE_SETTLE, then MPD mixer event forwards 72 -> AmpController.
    tokio::time::sleep(FRESH_MPD_SOURCE_SETTLE + Duration::from_millis(50)).await;
    mpd_mock.set_volume(Some(72));
    mpd_mock.emit_event(MpdEvent::SubsystemChanged(MpdSubsystem::Mixer));

    let cmd = tokio::time::timeout(Duration::from_secs(1), amp_cmd_rx.recv())
        .await
        .expect("MPD mixer event should forward SetLevel(72) to amp")
        .unwrap();
    assert_eq!(cmd.kind, AmpCommandKind::SetLevel(72));
    if let Some(reply) = cmd.reply {
        let _ = reply.send(Ok(()));
    }

    // Disconnect event triggers clean reconnect; closing amp_cmd_rx exits task.
    mpd_mock.emit_event(MpdEvent::Disconnected("eof".into()));
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(amp_cmd_rx);
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}
