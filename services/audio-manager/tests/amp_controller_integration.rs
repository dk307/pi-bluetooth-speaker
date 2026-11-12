//! Integration tests for the Phase 2 `AmpController` event-loop state
//! machine (plan §3.1), driven entirely through the mocked
//! `I2cAmpBackend`/`AmpGpio` seams (M-MOCKABLE-SYSCALLS).

use std::sync::Arc;
use std::time::Duration;

use audio_manager::StateFile;
use audio_manager::amp_backend::{I2cAmpBackend, MockAmpBackend};
use audio_manager::amp_controller::{AmpCommand, AmpCommandKind, AmpController, CtrlState};
use audio_manager::amp_gpio::{AmpGpio, AmpGpioMock};
use audio_manager::amp_state::AmpState;
use speaker_hardware::{AmpDevice, AmpDeviceMock, AmpErrorFlags, AmpFault};
use tempfile::tempdir;

/// Build live-error flags with a single fault bit set.
fn fault_flags(fault: AmpFault) -> AmpErrorFlags {
    AmpErrorFlags::from_bits(1 << fault.bit())
}

struct Fixture {
    controller: AmpController,
    amp_mock: AmpDeviceMock,
    gpio_mock: AmpGpioMock,
    state_file: StateFile,
    _dir: tempfile::TempDir,
}

/// Build a controller over fully mocked seams and a temp state file.
fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let (backend, amp_mock) = I2cAmpBackend::new_mocked();
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    let state_file = StateFile::new(dir.path().join("amp-state.json"));
    let device = backend.device().clone();
    let controller = AmpController::new(
        Arc::new(backend),
        device,
        gpio,
        state_file.clone(),
        AmpState::default(),
    );
    Fixture {
        controller,
        amp_mock,
        gpio_mock,
        state_file,
        _dir: dir,
    }
}

#[tokio::test]
async fn boot_init_wakes_configures_and_reconciles() {
    let mut f = fixture();

    // Boot with the persisted user state: raw 60, unmuted.
    let persisted = AmpState::new(60, false).unwrap();
    f.controller.boot_init(persisted).await.unwrap();

    // Kernel-driver probe-equivalent register init was applied.
    assert!(f.amp_mock.mock_vlp_enabled(), "VLP must be enabled at boot");
    assert!(
        f.amp_mock.mock_limiter_enabled(),
        "Limiter must be enabled at boot"
    );
    assert_eq!(f.amp_mock.mock_limiter_thresholds(), -15);
    // Kernel-parity power-up ran (booster + enable + settles).
    assert_eq!(f.gpio_mock.power_up_calls(), 1);
    assert!(f.gpio_mock.booster_on());
    assert!(f.gpio_mock.amp_awake());
    // Hardware /MUTE stays LOW (muted) until the first PLL handshake.
    assert!(f.gpio_mock.mute_pin_low());
    // Controller is muted awaiting first stream; level reconciled.
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert_eq!(f.controller.state().level_raw, 60);
}

#[tokio::test]
async fn set_level_updates_state_and_persists() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();

    f.controller
        .handle_command(AmpCommandKind::SetLevel(100))
        .await
        .unwrap();
    assert_eq!(f.controller.state().level_raw, 144);
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 144);

    // State file round-trip.
    assert_eq!(f.state_file.load().unwrap().level_raw, 144);
}

#[tokio::test]
async fn set_mute_true_immediately_mutes() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();

    f.controller
        .handle_command(AmpCommandKind::SetMute(true))
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(f.controller.state().muted);
    assert!(f.amp_mock.mock_serial_muted());
}

#[tokio::test]
async fn unmute_while_idle_records_intent_and_plays_on_next_stream() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, true).unwrap())
        .await
        .unwrap();

    // Unmute while muted with no stream: the user INTENT is recorded
    // immediately (mirror + state file), while the hardware audio path
    // stays muted until the next PLL handshake (audit round 4, Issue 1).
    f.controller
        .handle_command(AmpCommandKind::SetMute(false))
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(
        !f.controller.state().muted,
        "user unmute intent must be recorded at once, not deferred"
    );
    assert!(
        !f.controller.mirror().lock().await.muted,
        "D-Bus Muted property must reflect the unmute immediately"
    );
    assert!(
        !f.state_file.load().unwrap().muted,
        "unmute intent must survive a daemon restart"
    );
    assert!(
        f.amp_mock.mock_serial_muted(),
        "hardware audio path stays muted while no stream runs"
    );
    assert!(f.gpio_mock.mute_pin_low());

    // Stream starts with no I2S clock: handshake times out, stays muted.
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::PllClockLoss));
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);

    // PLL locks (mock clears the fault): handshake passes and unmutes.
    f.amp_mock.set_live_errors(AmpErrorFlags::NONE);
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.controller.state().muted);
    assert!(!f.gpio_mock.mute_pin_low(), "GPIO /MUTE released on play");
}

#[tokio::test]
async fn explicit_set_mute_true_overrides_recorded_unmute_intent() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, true).unwrap())
        .await
        .unwrap();

    // Unmute while idle (intent recorded) ...
    f.controller
        .handle_command(AmpCommandKind::SetMute(false))
        .await
        .unwrap();
    assert!(!f.controller.state().muted);
    // ... then an explicit mute wins (the intent flag is gone by design).
    f.controller
        .handle_command(AmpCommandKind::SetMute(true))
        .await
        .unwrap();
    assert!(f.controller.state().muted);
    assert!(f.state_file.load().unwrap().muted);

    // And the next stream start respects it: stays muted.
    f.amp_mock.set_live_errors(AmpErrorFlags::NONE);
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(f.amp_mock.mock_serial_muted());
}

#[tokio::test]
async fn start_stream_write_error_fails_safe_to_muted_not_awaiting_clock() {
    // This scenario needs a backend whose serial-mute WRITE can fail
    // while the device READS (PLL handshake, live errors) keep working —
    // exactly the transient-I2C shape that used to strand the machine in
    // `AwaitingClock` (audit round 4, Issue 2). `MockAmpBackend` provides
    // that injection point.
    let dir = tempdir().unwrap();
    let (backend, backend_ctrl) = MockAmpBackend::new_mocked();
    let (gpio, _gpio_mock) = AmpGpio::new_mocked();
    let state_file = StateFile::new(dir.path().join("amp-state.json"));
    let (device, amp_mock) = AmpDevice::new_mocked();
    let mut controller = AmpController::new(
        Arc::new(backend),
        Arc::new(device),
        gpio,
        state_file.clone(),
        AmpState::default(),
    );
    controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();

    // The PLL handshake succeeds (device reads fine), but the serial-mute
    // WRITE (the unmute step after the handshake) fails.
    backend_ctrl.set_serial_mute_fail(true);
    controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap_err();
    assert_eq!(
        controller.ctrl_state(),
        CtrlState::Muted,
        "error paths must reset AwaitingClock to Muted"
    );

    // The controller is fully functional afterwards: clearing the fault
    // and retrying the stream start succeeds normally.
    backend_ctrl.set_serial_mute_fail(false);
    amp_mock.set_live_errors(AmpErrorFlags::NONE);
    controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(controller.ctrl_state(), CtrlState::Playing);
}

#[tokio::test]
async fn stream_stopping_mutes_across_transition() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);

    f.controller
        .handle_command(AmpCommandKind::StreamStopping)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    // Hardware is muted (serial bit + GPIO /MUTE LOW, pop-free standby)...
    assert!(f.amp_mock.mock_serial_muted());
    assert!(f.gpio_mock.mute_pin_low());
    // ...but the USER's mute intent is untouched (no A5 regression): the
    // next stream must auto-resume playback.
    assert!(
        !f.controller.state().muted,
        "transient mute must not persist as user intent"
    );
    assert!(!f.state_file.load().unwrap().muted);

    // Restart: plays again at the user level without any SetMute call.
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.amp_mock.mock_serial_muted());
    assert!(!f.gpio_mock.mute_pin_low());
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 60);
}

#[tokio::test]
async fn thermal_warning_throttles_minus_six_db() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(100, false).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();

    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::HighTempWarning));
    f.controller.handle_error_edge().await.unwrap();

    // -6 dB throttle: raw 100 -> 94 (raw is dB attenuation); still Playing.
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 94);
    assert_eq!(f.controller.state().level_raw, 100, "user level preserved");
    assert!(!f.controller.state().muted);

    // Repeated edges while the warning is active must not re-apply.
    f.controller.handle_error_edge().await.unwrap();
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 94, "no ratchet");
    assert_eq!(f.controller.state().level_raw, 100);

    // Safety tick with the warning cleared restores the user level.
    f.amp_mock.set_live_errors(AmpErrorFlags::from_bits(0));
    f.controller.handle_safety_tick().await.unwrap();
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 100, "restored");
}

#[tokio::test]
async fn critical_fault_recovers_and_returns_to_muted() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(100, false).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.amp_mock.mock_serial_muted());

    // Model the /ENABLE register wipe: the reset pulse clears the serial
    // mute bit on the real device. The controller must re-assert it.
    f.gpio_mock.set_reset_hook(Some(Arc::new({
        let amp_mock = f.amp_mock.clone();
        move || amp_mock.set_mock_serial_muted(false)
    })));

    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::OvercurrentProtect));
    f.controller.handle_error_edge().await.unwrap();

    // Recovery re-initialized the registers and left the amp muted.
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(
        f.amp_mock.mock_serial_muted(),
        "serial mute must be re-asserted after the register wipe"
    );
    assert!(
        f.gpio_mock.mute_pin_low(),
        "GPIO /MUTE must be LOW after recovery"
    );
    // Transient mute: the user's unmuted intent survives recovery.
    assert!(!f.controller.state().muted);
    assert!(
        f.amp_mock.mock_vlp_enabled(),
        "VLP re-enabled during recovery"
    );
    assert!(
        f.amp_mock.mock_limiter_enabled(),
        "Limiter re-enabled during recovery"
    );
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 100, "volume restored");
    // A real /ENABLE reset pulse (HIGH->LOW) wiped the registers.
    assert_eq!(f.gpio_mock.reset_calls(), 1);
}

#[tokio::test]
async fn pll_handshake_bounded_by_timeout() {
    let f = fixture();
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::PllClockLoss));
    let started = std::time::Instant::now();
    let locked = f
        .controller
        .pll_lock_handshake(Duration::from_millis(50))
        .await
        .unwrap();
    assert!(!locked);
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_secs(2));

    f.amp_mock.set_live_errors(AmpErrorFlags::NONE);
    let locked = f
        .controller
        .pll_lock_handshake(Duration::from_millis(50))
        .await
        .unwrap();
    assert!(locked);
}

#[tokio::test]
async fn command_error_keeps_prior_state() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();

    // Inject a bus fault; SetLevel fails and prior state survives.
    f.amp_mock.set_fault(Some("simulated bus failure".into()));
    let err = f
        .controller
        .handle_command(AmpCommandKind::SetLevel(100))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("simulated bus failure"));
    assert_eq!(
        f.controller.state().level_raw,
        60,
        "state unchanged after fault"
    );

    // Clear the fault: the next command succeeds.
    f.amp_mock.set_fault(None);
    f.controller
        .handle_command(AmpCommandKind::SetLevel(100))
        .await
        .unwrap();
    assert_eq!(f.controller.state().level_raw, 144);
}

#[tokio::test]
async fn run_loop_processes_commands_until_close() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    let mirror = f.controller.mirror();

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
    let (_edge_tx, edge_rx) = tokio::sync::mpsc::channel(4);
    let (_shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);

    let controller = f.controller;
    let task = tokio::spawn(async move {
        controller
            .run(cmd_rx, Some(edge_rx), shutdown_rx, false)
            .await;
    });

    cmd_tx
        .send(AmpCommand::fire_and_forget(AmpCommandKind::SetLevel(100)))
        .await
        .unwrap();
    // Give the loop a moment to process, then close the channel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(cmd_tx);
    task.await.unwrap();

    let mirrored = *mirror.lock().await;
    assert_eq!(mirrored.level_raw, 144);
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 144);
}

#[tokio::test]
async fn idle_user_mute_survives_stream_start() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();

    // User mutes while idle.
    f.controller
        .handle_command(AmpCommandKind::SetMute(true))
        .await
        .unwrap();
    // Stream starts with a locked PLL: user mute is never overridden.
    f.amp_mock.set_live_errors(AmpErrorFlags::from_bits(0));
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(f.controller.state().muted);
    assert!(f.amp_mock.mock_serial_muted());
    assert!(f.gpio_mock.mute_pin_low());

    // Explicit unmute (intent recorded) + next stream start plays.
    f.controller
        .handle_command(AmpCommandKind::SetMute(false))
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.amp_mock.mock_serial_muted());
    assert!(
        !f.gpio_mock.mute_pin_low(),
        "electrical LOW = muted; must be released"
    );
}

#[tokio::test]
async fn set_mute_while_playing_mirrors_both_directions() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    f.amp_mock.set_live_errors(AmpErrorFlags::from_bits(0));
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.gpio_mock.mute_pin_low());

    f.controller
        .handle_command(AmpCommandKind::SetMute(true))
        .await
        .unwrap();
    assert!(f.gpio_mock.mute_pin_low());
    f.controller
        .handle_command(AmpCommandKind::SetMute(false))
        .await
        .unwrap();
    assert!(!f.gpio_mock.mute_pin_low(), "unmute must release /MUTE too");
}

#[tokio::test]
async fn closed_edge_channel_does_not_hot_spin() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    let mirror = f.controller.mirror();

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
    let (edge_tx, edge_rx) = tokio::sync::mpsc::channel::<()>(4);
    let (_shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);

    let controller = f.controller;
    let task = tokio::spawn(async move {
        controller
            .run(cmd_rx, Some(edge_rx), shutdown_rx, false)
            .await;
    });

    // Close the edge channel: the loop must keep processing commands.
    drop(edge_tx);
    tokio::time::sleep(Duration::from_millis(50)).await;
    cmd_tx
        .send(AmpCommand::fire_and_forget(AmpCommandKind::SetLevel(100)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(cmd_tx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("loop must not hang after edge-channel close")
        .unwrap();
    assert_eq!(mirror.lock().await.level_raw, 144);
}

#[tokio::test]
async fn stream_restart_after_pll_loss_resumes() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);

    // PLL clock loss during playback: transient mute, stream dead.
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::PllClockLoss));
    f.controller.handle_error_edge().await.unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(f.amp_mock.mock_serial_muted());
    assert!(
        !f.controller.state().muted,
        "PLL-loss mute must not persist as user intent"
    );

    // Next stream (PLL locked): auto-resumes at the user level.
    f.amp_mock.set_live_errors(AmpErrorFlags::NONE);
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.amp_mock.mock_serial_muted());
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 60);
}

#[tokio::test]
async fn stream_restart_after_fault_recovery_resumes() {
    let mut f = fixture();
    f.gpio_mock.set_reset_hook(Some(Arc::new({
        let amp_mock = f.amp_mock.clone();
        move || amp_mock.set_mock_serial_muted(false)
    })));
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();

    // Critical fault mid-playback → full recovery, ends muted (hardware).
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::UndervoltagePvdd));
    f.controller.handle_error_edge().await.unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(f.amp_mock.mock_serial_muted());

    // Next stream auto-resumes: recovery's mute was transient only.
    f.amp_mock.set_live_errors(AmpErrorFlags::NONE);
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.amp_mock.mock_serial_muted());
    assert!(!f.gpio_mock.mute_pin_low());
    assert_eq!(f.amp_mock.mock_master_volume_raw(), 60);
}

#[tokio::test]
async fn stop_then_reboot_keeps_unmuted_user_intent() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("amp-state.json");

    // Session 1: play, then a normal stream stop; the state file is
    // written after the stop.
    let (backend, _amp_mock) = I2cAmpBackend::new_mocked();
    let (gpio, _gpio_mock) = AmpGpio::new_mocked();
    let state_file = StateFile::new(state_path.clone());
    let device = backend.device().clone();
    let mut controller = AmpController::new(
        Arc::new(backend),
        device,
        gpio,
        state_file.clone(),
        AmpState::default(),
    );
    controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    controller
        .handle_command(AmpCommandKind::StreamStopping)
        .await
        .unwrap();
    assert!(
        !state_file.load().unwrap().muted,
        "persisted intent after a stop must be the user's unmuted request"
    );

    // Session 2 (reboot): boot from the same file; the user intent is
    // still unmuted, so the next stream plays without any SetMute call.
    let (backend, amp_mock) = I2cAmpBackend::new_mocked();
    let (gpio, gpio_mock) = AmpGpio::new_mocked();
    let state_file = StateFile::new(state_path);
    let device = backend.device().clone();
    let mut controller = AmpController::new(
        Arc::new(backend),
        device,
        gpio,
        state_file.clone(),
        state_file.load().unwrap(),
    );
    controller.boot_init(controller.state()).await.unwrap();
    controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(controller.ctrl_state(), CtrlState::Playing);
    assert!(!amp_mock.mock_serial_muted());
    assert!(!gpio_mock.mute_pin_low());
}

#[tokio::test]
async fn thermal_warning_at_stream_start_throttles_without_reset() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(100, false).unwrap())
        .await
        .unwrap();

    // Warm amp (HighTempWarning live) at stream start: throttle -6 dB,
    // NOT a full fault recovery reset.
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::HighTempWarning));
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();

    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert_eq!(
        f.amp_mock.mock_master_volume_raw(),
        94,
        "-6 dB throttle applied at stream start (raw 100 -> 94)"
    );
    assert_eq!(f.controller.state().level_raw, 100, "user level preserved");
    assert!(!f.controller.state().muted);
    assert!(
        !f.amp_mock.mock_serial_muted(),
        "no transient mute for a thermal warning"
    );
    assert_eq!(
        f.gpio_mock.reset_calls(),
        0,
        "thermal warning must never trigger an /ENABLE reset"
    );
}

#[tokio::test]
async fn boot_init_forces_serial_mute_while_preserving_user_unmute_intent() {
    let mut f = fixture();
    // User left the speaker UNMUTED before the reboot. Boot must still
    // force the serial mute (0x36 bit 7): the audio path stays off until
    // the first PLL handshake, regardless of user intent.
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();

    assert!(
        f.amp_mock.mock_serial_muted(),
        "serial mute must be forced ON at boot even when the user intent is unmuted"
    );
    assert!(
        f.gpio_mock.mute_pin_low(),
        "GPIO /MUTE stays LOW until the first handshake"
    );
    // ...while the USER mute intent is preserved verbatim for start_stream.
    assert!(
        !f.controller.state().muted,
        "persisted user intent must not be overwritten by the boot mute"
    );

    // The first stream start (PLL locked) releases both mutes normally.
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.amp_mock.mock_serial_muted());
    assert!(!f.gpio_mock.mute_pin_low());
}

#[tokio::test]
async fn pll_loss_while_playing_asserts_gpio_mute() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, false).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    assert_eq!(f.controller.ctrl_state(), CtrlState::Playing);
    assert!(!f.gpio_mock.mute_pin_low());

    // BCLK dies mid-stream: BOTH mute paths must engage — the serial bit
    // (I2C) AND the electrical /MUTE (GPIO), which works even if the
    // synchronous digital domain can no longer clock the register mute.
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::PllClockLoss));
    f.controller.handle_error_edge().await.unwrap();

    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);
    assert!(f.amp_mock.mock_serial_muted());
    assert!(
        f.gpio_mock.mute_pin_low(),
        "GPIO /MUTE must be asserted on PLL clock loss (parity with StreamStopping)"
    );
}

#[tokio::test]
async fn pll_loss_while_user_muted_clears_stream_live() {
    let mut f = fixture();
    f.controller
        .boot_init(AmpState::new(60, true).unwrap())
        .await
        .unwrap();
    f.controller
        .handle_command(AmpCommandKind::StreamStarting)
        .await
        .unwrap();
    // Stream live but user-muted: ready, staying muted.
    assert_eq!(f.controller.ctrl_state(), CtrlState::Muted);

    // Clock dies while the user is muted. `stream_live` must be cleared
    // even though ctrl is already Muted — otherwise a later SetMute(false)
    // would unmute into the dead clock without a handshake.
    f.amp_mock
        .set_live_errors(fault_flags(AmpFault::PllClockLoss));
    f.controller.handle_error_edge().await.unwrap();
    assert!(f.amp_mock.mock_serial_muted());
    assert!(f.gpio_mock.mute_pin_low());

    // The subsequent unmute must NOT restore audio (dead clock): the
    // intent is recorded while the audio path stays muted until the next
    // real StreamStarting handshake.
    f.controller
        .handle_command(AmpCommandKind::SetMute(false))
        .await
        .unwrap();
    assert!(
        f.amp_mock.mock_serial_muted(),
        "unmute into a dead clock must be deferred, not applied"
    );
    assert!(
        f.gpio_mock.mute_pin_low(),
        "GPIO /MUTE must stay asserted after the deferred unmute"
    );
}
