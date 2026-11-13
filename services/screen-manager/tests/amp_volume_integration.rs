//! Integration tests for the screen-manager `amp-volume` client, queue,
//! coalescing, and worker publish/revert semantics.

use screen_manager::daemon::BtSharedUpdate;
use screen_manager::volume::{
    AmpVolumeClient, AmpVolumeCommand, AmpVolumeCommandQueue, AmpVolumeState, coalesce, run_worker,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn amp_state() -> AmpVolumeState {
    AmpVolumeState::new(50, false)
}

#[test]
fn queue_push_drain_and_clone() {
    let queue = AmpVolumeCommandQueue::new();
    assert!(queue.is_empty());
    queue.push(AmpVolumeCommand::SetLevel(30));
    let clone = queue.clone();
    assert_eq!(clone.drain(), vec![AmpVolumeCommand::SetLevel(30)]);
    assert!(queue.is_empty());
}

#[test]
fn coalesce_keeps_last_value_per_kind_in_order() {
    let drained = vec![
        AmpVolumeCommand::SetMute(false),
        AmpVolumeCommand::SetLevel(10),
        AmpVolumeCommand::SetLevel(40),
        AmpVolumeCommand::SetMute(true),
    ];
    // Kinds keep their first-seen relative order; only values are coalesced.
    assert_eq!(
        coalesce(drained),
        vec![
            AmpVolumeCommand::SetMute(true),
            AmpVolumeCommand::SetLevel(40),
        ]
    );
}

#[test]
fn coalesce_empty_batch() {
    assert_eq!(coalesce(Vec::new()), Vec::new());
}

#[test]
fn state_new_clamps_percent() {
    assert_eq!(AmpVolumeState::new(200, false).level, 100);
    assert!(AmpVolumeState::new(10, true).muted);
}

#[test]
fn mock_client_applies_commands_and_records() {
    let (client, ctrl) = AmpVolumeClient::new_mocked(amp_state());
    let state = client.apply(AmpVolumeCommand::SetLevel(70)).unwrap();
    assert_eq!(state.level, 70);
    let state = client.apply(AmpVolumeCommand::SetMute(true)).unwrap();
    assert!(state.muted);
    assert_eq!(
        ctrl.executed(),
        vec![
            AmpVolumeCommand::SetLevel(70),
            AmpVolumeCommand::SetMute(true),
        ]
    );
    assert_eq!(client.state().unwrap().level, 70);
}

#[test]
fn mock_client_write_failure_rejects_without_state_change() {
    let (client, ctrl) = AmpVolumeClient::new_mocked(amp_state());
    ctrl.fail_next_write();
    let err = client
        .apply(AmpVolumeCommand::SetLevel(80))
        .expect_err("injected failure must surface");
    assert!(err.to_string().contains("injected mock write failure"));
    // State is unchanged; the failure was one-shot.
    assert_eq!(client.state().unwrap().level, 50);
    assert_eq!(
        client.apply(AmpVolumeCommand::SetLevel(80)).unwrap().level,
        80
    );
}

#[test]
fn worker_applies_commands_and_publishes_authoritative_state() {
    let (client, _ctrl) = AmpVolumeClient::new_mocked(amp_state());
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let queue = AmpVolumeCommandQueue::new();
    let running = Arc::new(AtomicBool::new(true));

    let worker_queue = queue.clone();
    let worker = std::thread::spawn({
        let shared = Arc::clone(&shared);
        let running = Arc::clone(&running);
        move || run_worker(&client, &shared, &worker_queue, &running)
    });

    // Wait for the initial seed publish.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if shared.lock().unwrap().amp_volume.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(shared.lock().unwrap().amp_volume, Some(amp_state()));

    queue.push(AmpVolumeCommand::SetLevel(20));
    queue.push(AmpVolumeCommand::SetMute(true));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let published = shared.lock().unwrap().amp_volume;
        if published == Some(AmpVolumeState::new(20, true)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never published the commanded state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    worker.join().expect("worker must exit cleanly");
}

#[test]
fn worker_failed_command_reverts_optimistic_state() {
    let (client, ctrl) = AmpVolumeClient::new_mocked(amp_state());
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let queue = AmpVolumeCommandQueue::new();
    let running = Arc::new(AtomicBool::new(true));

    let worker_queue = queue.clone();
    let worker = std::thread::spawn({
        let shared = Arc::clone(&shared);
        let running = Arc::clone(&running);
        move || run_worker(&client, &shared, &worker_queue, &running)
    });

    // Wait for the initial seed publish (50, unmuted).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if shared.lock().unwrap().amp_volume.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Inject a write failure, then push a command: the optimistic UI flip
    // (to 90) must be reverted to the last known-good 50.
    ctrl.fail_next_write();
    queue.push(AmpVolumeCommand::SetLevel(90));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let published = shared.lock().unwrap().amp_volume;
        if published == Some(amp_state()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never reverted the optimistic flip; published {published:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    worker.join().expect("worker must exit cleanly");
}

#[test]
fn worker_external_change_is_republished() {
    let (client, ctrl) = AmpVolumeClient::new_mocked(amp_state());
    let shared = Arc::new(Mutex::new(BtSharedUpdate::default()));
    let queue = AmpVolumeCommandQueue::new();
    let running = Arc::new(AtomicBool::new(true));

    let worker_queue = queue.clone();
    let worker = std::thread::spawn({
        let shared = Arc::clone(&shared);
        let running = Arc::clone(&running);
        move || run_worker(&client, &shared, &worker_queue, &running)
    });

    // Wait for the initial seed publish, then simulate a phone-initiated
    // change (bt-manager sync) on the audio-manager side.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if shared.lock().unwrap().amp_volume.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ctrl.set_state(AmpVolumeState::new(42, false));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let published = shared.lock().unwrap().amp_volume;
        if published == Some(AmpVolumeState::new(42, false)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never published the external change; published {published:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    worker.join().expect("worker must exit cleanly");
}
